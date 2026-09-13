//! The row-major columns: **a level's membership addressed by row instead of by artifact.**
//!
//! An artifact-major level answers a viewport by walking the tile index and probing each candidate's
//! bitmap. A row-major level answers it by scanning `viewport ∩ M_auth` once and reading off which
//! artifact each visible row belongs to — so candidacy costs **points rather than artifacts**, and
//! nothing in the request is a function of how many artifacts the layer holds
//! (`design/artifact-serving-at-scale.md` §5.1).
//!
//! Two forms, and which one applies is not a choice:
//!
//! - **[`ServingLayout::RowMajorLabel`]** — one label per row, for a level whose memberships
//!   partition the corpus. A single-valued attribute predicate is the motivating case: every point
//!   carries exactly one value, so the memberships are disjoint.
//! - **[`ServingLayout::RowMajorList`]** — a list per row, the same inversion at a larger constant,
//!   for a level whose memberships overlap.
//!
//! **A level that claims to partition and does not is composed artifact-major**, loudly
//! ([`RowColumn::compose`] and [`RowColumn::project`] both return `None` on a double claim). Keeping
//! the last writer would give each contested row to whichever artifact happened to be walked last,
//! which is a masked count short for one artifact and long for another with nothing reporting it.
//! **A level served from its column has no artifact-major form to be composed into**, so a label
//! column that stops partitioning under an amendment takes the list form instead
//! ([`RowColumn::recompose_as_list`]).
//!
//! # What this replaces, and what it does not
//!
//! It replaces the **candidacy walk** and the **counting route** for the levels it covers, and the
//! per-artifact declared size the proportional criterion divides by. It does **not** replace the
//! generating sets containment is tested against, or the visible-row set derived content is computed
//! from: both are per-artifact row-space questions with no row-addressed form, and both go on being
//! answered from [`crate::artifacts::MembershipRows`] exactly as they were.
//!
//! **The residency half of §5.1 is taken.** A level whose column the manifest names is served from
//! that one file: the column answers candidacy, the masked counts and the declared sizes, each
//! artifact's extent is folded out of its own bytes in the pass that already walks it
//! ([`RowColumn::extents`]), and the artifact-major bitmaps are never built
//! ([`crate::artifacts::MembershipRows::rows_held`]). At the rung 6 corpus — 1,646,192 artifacts
//! over ~3.4×10⁹ member entries a level — the form that replaces was a measured ~28 GB retained and
//! ~10 GB transient per level. Every write reaches the column and the extents beside it; nothing is
//! transposed back.
//!
//! **Where a layer derives a hull, the artifact-major form is transposed out of the column** rather
//! than
//! projected a second time from the level's memberships ([`RowColumn::transpose`], and
//! [`crate::artifacts::ArtifactRows::build_from_column`] is the caller): the column already holds
//! the membership, addressed by row, so reaching the other address is one sequential pass instead
//! of a decode and a permutation of every artifact's members. At rung 3's `mesh/descriptors` —
//! 30,217 artifacts over 1.66×10⁹ membership entries — that is **14.5 s at open against 24 s**,
//! the open as a whole 14.4 s against 23.9, and `/readyz` 24.8 s against 33.8
//! (`probes/2026-09-02-cold-start/`). That same transpose is what a column-only form takes back
//! when a flush, a merge or a publication reaches it, every amendment being expressed over the
//! artifact-major half.
//!
//! **Three things are not in the column and still project**: each content's **generating set**,
//! which containment is tested against and which is a different set from the membership; a level
//! whose column this prefix does not hold, which projects and then composes its column as before;
//! and an attribute predicate's column, whose labels come from the value column rather than from
//! any stored membership. The transposition also refuses a column with a live **tail** — the base
//! alone is what a projection produces, and a form stopping short of the flushed rows would be
//! narrow. Every one of those is a fallback to the route that existed before, so the worst case is
//! the old cost and never a wrong answer.
//!
//! # The declared sizes are derived, never stored
//!
//! §10's answer to the proportional criterion's denominator on a row-major level is the per-artifact
//! **unmasked** membership size, which is mask-independent and corpus-wide. It is folded up in the
//! same pass that validates the column — an artifact's size is how many rows carry its label — so it
//! is a function of the bytes beside it rather than a second thing that could disagree with them.
//! That is [`crate::tile_index`]'s rule for the node hierarchy, one structure along.

use std::sync::Arc;

use croaring::Bitmap;

use tessera_lifecycle::membership::ArtifactRecord;
use tessera_store::membership::{
    pack_label_column, LabelColumnPack, ListColumnPack, ROW_COLUMN_HOLE, TILE_INDEX_EMPTY,
    TILE_INDEX_HOLE,
};
use tessera_store::permutation::RowSpace;
use tessera_types::layer::ServingLayout;

use crate::artifacts::MembershipRows;

/// One walk of a level's live artifacts, handing each ordinal its **projected** rows — and the
/// bytes are produced by [`tessera_store::derived::project_row_column`], beside the format.
///
/// **A callback rather than an iterator**, because the caller has to be able to run it more than
/// once: a list column is an offset table sized by one pass and filled by a second, and the fold's
/// walk holds one membership at a time rather than the level's. An iterator would have to be
/// re-created, which is what this type is.
type LevelWalk<'a> = tessera_store::derived::LevelWalk<'a>;
use crate::compose::WholeMask;

/// One `(view, layer, level)`'s row-addressed membership — mapped where a fold wrote it, a buffer
/// where a publication built it.
///
/// `Clone` so that `Arc::make_mut` can amend a column in place: between requests the executor
/// thread is the column's only holder and no copy is made; where a request is still reading the
/// form, the copy is the amendment, the counts and a live tail's labels where there is one — never
/// the pack or the base counts, which stay behind their `Arc`s. That copy is what makes the
/// cost bound below conditional: a write under a reader pays the accumulated amendment once more,
/// which is small beside the form's own copy `bring_forward` logs as `cloned_ms`.
#[derive(Clone)]
pub struct RowColumn {
    /// **Shared, because a live tail is attached by deriving a second column over the same base.**
    /// A predicate level's base is a function of the prefix and the level's version; its tail moves
    /// at every flush. Copying four bytes a row per flush is what this `Arc` exists to avoid — at
    /// 10⁹ rows the base is the 4 GB the layout was chosen for.
    pack: Arc<Pack>,
    /// Per ordinal, how many **rows** carry this artifact's label — the unmasked membership size in
    /// this view's row space, base and tail together. Derived at open; see the module doc.
    declared: Vec<u32>,
    /// The base's own half of `declared`, kept so [`RowColumn::with_tail`] can add a tail's counts
    /// without re-walking four bytes a row.
    base_declared: Arc<Vec<u32>>,
    /// Per ordinal, the lowest and highest **base** row carrying this artifact's label — folded up
    /// in the same pass as `base_declared` and read by [`Self::extents`].
    ///
    /// **The extents are a function of the column's own bytes**, which is what makes a level served
    /// from the column alone need no second file: a fold-written extent column and this one could
    /// disagree, and a hole in the index would make an artifact's rows read as absent where the
    /// membership has them — a silently short answer. Derived here, the two cannot part company,
    /// which is [`crate::tile_index`]'s rule for the node hierarchy one structure along.
    base_extents: Arc<Vec<(u32, u32)>>,
    /// **The rows above the base a fold has not yet absorbed**, where this column has any.
    tail: Option<TailLabels>,
    /// **The labels a write added to a column that was already built**, where any were added.
    ///
    /// [`TailLabels`] one step further: that one answers for rows *above* the base, this one adds
    /// to rows anywhere. A growth joins entities to an artifact that already exists, and the rows
    /// those entities hold are ordinary base rows the pack already addresses — so the label cannot
    /// go in the tail and, for a list column, cannot go in the pack either without rewriting the
    /// offset table and every value above the insertion. Held beside the pack, the write costs the
    /// rows it touched and the pack's bytes are not read, copied or rewritten
    /// (`2026-09-03-post-flush-artifact-frames.md`).
    ///
    /// **Every label at an extent row is here**, whether a flush, a growth or the column's own
    /// composition put it there ([`Self::compose_over_base`]), and the pack labels base rows
    /// alone. That is the rule a merge's rebase rests on ([`Self::rebase`]): the rows a merge
    /// renumbers are extent rows, so their labels are all in the one half of the column that can
    /// be edited.
    ///
    /// ⊘ **Bounded by what has accumulated since the last fold**, on [`TailLabels`]' own note and
    /// with the same reset: the fold rewrites the level's column whole, and a deployment that
    /// never folds accumulates one entry per `(row, artifact)` every write adds whatever this
    /// structure does. What has accumulated bounds the memory and not the write: a write costs its
    /// own batch ([`RowColumn::amend`]) while the column is unshared, however much is already held.
    added: Option<Added>,
}

/// What one pass over a viewer's visible rows folded up, per ordinal — see
/// [`RowColumn::accumulate_over`].
///
/// **A count a row contributes to here is a count over rows the locator could place**, which is
/// every row of a well-formed generation and is the same set the positions came from. The masked
/// count served beside an artifact comes from [`RowColumn::histogram_over`] and counts every
/// visible row, placeable or not; the two are equal wherever the row space places its own rows, and
/// this one is never served as a count.
pub struct LevelAccumulation {
    pub counts: Vec<u32>,
    pub sums: Vec<[f64; 2]>,
    pub boxes: Vec<[u32; 4]>,
}

/// Labels added to rows a column already addresses — see [`RowColumn::added`].
#[derive(Debug, Default, Clone)]
struct Added {
    /// `(row, ordinal)`, ascending and deduplicated, and never a pair the column already carries.
    pairs: Vec<(u32, u32)>,
    /// The rows [`Self::pairs`] names — so a scan asks *is any of this in view* in
    /// O(containers touched) rather than walking the pairs.
    rows: Bitmap,
    /// One past the highest row named, which may be above the pack's own row count: a flush
    /// publishes rows the pack never covered, and this is what carries them.
    row_end: u32,
}

impl Added {
    /// Every ordinal added at `row`, ascending. Empty for a row this adds nothing at.
    fn at(&self, row: u32) -> &[(u32, u32)] {
        let lo = self.pairs.partition_point(|(r, _)| *r < row);
        let hi = self.pairs.partition_point(|(r, _)| *r <= row);
        &self.pairs[lo..hi]
    }

    /// Remove every pair at a row in `lo..hi`, returning how many each ordinal lost — the half of
    /// a merge's rebase that gives the renumbered span up ([`RowColumn::rebase`]). One
    /// `partition_point` at each end and one move of what lies above, never a pass over the pairs
    /// below.
    fn drain_span(&mut self, lo: u32, hi: u32) -> Vec<(u32, u32)> {
        let from = self.pairs.partition_point(|(r, _)| *r < lo);
        let to = self.pairs.partition_point(|(r, _)| *r < hi);
        self.rows.remove_range(lo..hi);
        self.pairs.drain(from..to).collect()
    }

    /// Merge `batch` into [`Self::pairs`], keeping it ascending. `batch` is ascending,
    /// deduplicated and disjoint from what is held, which [`RowColumn::amend`] arranges.
    ///
    /// An append where the batch lies wholly above what is held, which is where a flush's rows and
    /// most growths lie; otherwise one pass from the back, in place. The held list is never
    /// re-sorted, so a write costs its batch plus, on the merge path, one move of what is held.
    fn merge(&mut self, batch: Vec<(u32, u32)>) {
        let Some(&first) = batch.first() else {
            return;
        };
        if self.pairs.last().is_none_or(|last| *last < first) {
            self.pairs.extend(batch);
            return;
        }
        let held = self.pairs.len();
        self.pairs.resize(held + batch.len(), (0, 0));
        let (mut i, mut j, mut k) = (held, batch.len(), self.pairs.len());
        while j > 0 {
            k -= 1;
            if i > 0 && self.pairs[i - 1] > batch[j - 1] {
                self.pairs[k] = self.pairs[i - 1];
                i -= 1;
            } else {
                self.pairs[k] = batch[j - 1];
                j -= 1;
            }
        }
    }
}

/// The labels of the rows **above** a column's base — the flushed tail.
///
/// **A second dense array rather than a wider base**, and the cadence is the whole reason. A base
/// column is a function of the prefix and the level's version, so it survives every flush; the tail
/// is a function of the geometry and is rebuilt whenever `segments_version` moves. Widening the
/// base instead would rebuild four bytes a row at every flush — the cost `RowSpace::project_base`
/// exists to avoid, arriving through the other door.
///
/// ⊘ **Bounded by the flushed tail**, which the merge ladder bounds and the fold resets. Nothing
/// here bounds it independently: a deployment that never folds accumulates rows above its base
/// whatever this structure does, and the tail is one `u32` per such row.
#[derive(Clone)]
pub struct TailLabels {
    /// The first row this covers — the base's row count.
    row_base: u32,
    /// One label per row of `[row_base, row_base + labels.len())`, [`ROW_COLUMN_HOLE`] where no
    /// artifact claims the row.
    labels: Vec<u32>,
}

impl TailLabels {
    /// A tail over `[row_base, row_base + labels.len())`.
    pub fn new(row_base: u32, labels: Vec<u32>) -> Self {
        TailLabels { row_base, labels }
    }

    /// The label at an absolute row, or [`ROW_COLUMN_HOLE`] where the row is outside this tail.
    fn label(&self, row: u32) -> u32 {
        row.checked_sub(self.row_base)
            .and_then(|at| self.labels.get(at as usize).copied())
            .unwrap_or(ROW_COLUMN_HOLE)
    }

    /// One past the last row this covers.
    fn row_end(&self) -> u32 {
        self.row_base.saturating_add(self.labels.len() as u32)
    }
}

enum Pack {
    Label(LabelColumnPack),
    List(ListColumnPack),
}

impl std::fmt::Debug for RowColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowColumn")
            .field("layout", &self.layout())
            .field("ordinals", &self.len())
            .field("rows", &self.row_count())
            .finish()
    }
}

impl RowColumn {
    /// Compose one from a level's resident row form — what a publication or a growth builds, before
    /// any fold has consolidated it.
    ///
    /// `None` where `layout` is [`ServingLayout::RowMajorLabel`] and the memberships do not
    /// partition: the caller composes the level artifact-major instead and says so.
    ///
    /// **Framed and read back through the same checks a mapped file takes**, for the reason
    /// [`crate::containment::ContainmentPartition`] gives: the two routes are one reader, so a
    /// framing rule can never hold for a file and not for the form a publication built.
    pub fn compose(
        membership: &MembershipRows,
        row_count: u32,
        layout: ServingLayout,
        scratch: &std::path::Path,
    ) -> Option<Self> {
        let ordinals = membership.len() as u32;
        let each = |visit: &mut dyn FnMut(u32, &Bitmap)| {
            for ordinal in 0..ordinals {
                if let Some(rows) = membership.get(ordinal) {
                    visit(ordinal, rows);
                }
            }
        };
        Self::assemble(ordinals, row_count, layout, scratch, &each)
    }

    /// [`Self::compose`] with **the pack over the base rows alone and every row above them in the
    /// amendment** — what a form built while the row space already carries extents composes.
    ///
    /// The split is what a merge's rebase rests on ([`Self::rebase`]): a merge renumbers extent
    /// rows, and the amendment is the one half of a column that can give a span of rows up and
    /// take it again, while the pack is packed bytes that are shared and never rewritten. A pack
    /// that reached above the base would hold labels at rows a merge has since renumbered, with no
    /// edit cheaper than composing the column again. So the pack covers `[0, base_rows)` whatever
    /// the row space held when the column was composed, and the rows in `[base_rows, row_count)`
    /// enter through [`Self::amend`], which a flush and a growth also use. A membership entirely
    /// below the base costs nothing extra; one that reaches above it is copied once, at the
    /// composition that already walks it whole.
    ///
    /// `None` on [`Self::compose`]'s terms: a label column whose memberships do not partition.
    pub fn compose_over_base(
        membership: &MembershipRows,
        base_rows: u32,
        row_count: u32,
        layout: ServingLayout,
        scratch: &std::path::Path,
    ) -> Option<Self> {
        let ordinals = membership.len() as u32;
        let reaches_above = |rows: &Bitmap| rows.maximum().is_some_and(|max| max >= base_rows);
        let each = |visit: &mut dyn FnMut(u32, &Bitmap)| {
            for ordinal in 0..ordinals {
                if let Some(rows) = membership.get(ordinal) {
                    if reaches_above(rows) {
                        let mut below = rows.clone();
                        below.remove_range(base_rows..);
                        visit(ordinal, &below);
                    } else {
                        visit(ordinal, rows);
                    }
                }
            }
        };
        let mut column = Self::assemble(ordinals, base_rows, layout, scratch, &each)?;
        if row_count <= base_rows {
            return Some(column);
        }
        let mut above: Vec<(u32, u32)> = Vec::new();
        for ordinal in 0..ordinals {
            if let Some(rows) = membership.get(ordinal).filter(|rows| reaches_above(rows)) {
                let mut iter = rows.iter();
                iter.reset_at_or_after(base_rows);
                above.extend(iter.map(|row| (row, ordinal)));
            }
        }
        // A label column composed over the base already partitions, and the rows above it are the
        // same memberships' rows, so the amendment cannot be refused here.
        if !column.amend(&above, row_count) {
            return None;
        }
        Some(column)
    }

    /// **Every `(row, ordinal)` this column carries**, in row order over the pack and then the two
    /// live halves — one sequential pass, nothing held.
    ///
    /// `below` bounds it to the base rows, which is what a recomposition's pack takes
    /// ([`Self::compose_over_base`]'s split).
    fn for_each_pair(&self, below: Option<u32>, visit: &mut dyn FnMut(u32, u32)) {
        let ceiling = below.unwrap_or(u32::MAX);
        match &*self.pack {
            Pack::Label(pack) => {
                let end = (pack.rows() as usize).min(ceiling as usize);
                for row in 0..end {
                    let label = pack.label(row);
                    if label != ROW_COLUMN_HOLE {
                        visit(row as u32, label);
                    }
                }
            }
            Pack::List(pack) => {
                let end = (pack.rows() as usize).min(ceiling as usize);
                for row in 0..end {
                    for ordinal in pack.list(row) {
                        visit(row as u32, ordinal);
                    }
                }
            }
        }
        if let Some(tail) = &self.tail {
            for row in tail.row_base..tail.row_end().min(ceiling) {
                let label = tail.label(row);
                if label != ROW_COLUMN_HOLE {
                    visit(row, label);
                }
            }
        }
        if let Some(added) = &self.added {
            for row in added.rows.iter() {
                if row >= ceiling {
                    continue;
                }
                for (_, ordinal) in added.at(row) {
                    visit(row, *ordinal);
                }
            }
        }
    }

    /// **Take the list form, from this column's own bytes plus the pairs that would not fit the
    /// label form** — what a level served from its column does when an amendment makes its
    /// memberships overlap.
    ///
    /// A label column refuses a row that would come to carry two artifacts, and a level served from
    /// its column has no other membership to fall back to. The bounded answer is the one the fold
    /// already takes for such a level (decision 0094): the list form, composed through the
    /// disk-backed partition route ([`tessera_store::derived::project_row_column_pairs`]) from the
    /// pairs this column already holds and the pairs the amendment added. **Nothing row-sized is
    /// held while it runs** — one partition bucket, exactly as the fold's composition and the
    /// build's — so the level never materialises the artifact-major form the layout exists to
    /// avoid.
    ///
    /// The pack covers `[0, base_rows)` and everything above enters as the amendment, which is
    /// [`Self::compose_over_base`]'s split and is what a later merge's rebase rests on.
    ///
    /// `None` where the composition could not be written or read back — an I/O failure rather than
    /// a shape this cannot express: a list column takes any membership, so there is no second
    /// refusal below this one.
    pub fn recompose_as_list(
        &self,
        extra: &[(u32, u32)],
        base_rows: u32,
        row_count: u32,
        scratch: &std::path::Path,
    ) -> Option<Self> {
        let ordinals = self
            .len()
            .max(extra.iter().map(|(_, o)| *o as usize + 1).max().unwrap_or(0))
            as u32;
        let pairs = |visit: &mut dyn FnMut(u32, u32)| {
            self.for_each_pair(Some(base_rows), visit);
            for (row, ordinal) in extra {
                if *row < base_rows {
                    visit(*row, *ordinal);
                }
            }
        };
        let mut column = Self::assemble_pairs(
            ordinals,
            base_rows,
            ServingLayout::RowMajorList,
            scratch,
            &pairs,
        )?;
        if row_count <= base_rows {
            return Some(column);
        }
        let mut above: Vec<(u32, u32)> = Vec::new();
        self.for_each_pair(None, &mut |row, ordinal| {
            if row >= base_rows {
                above.push((row, ordinal));
            }
        });
        above.extend(extra.iter().copied().filter(|(row, _)| *row >= base_rows));
        // A list column refuses nothing, so this cannot fail for a reason the form can express.
        if !column.amend(&above, row_count) {
            return None;
        }
        Some(column)
    }

    /// The same column, projected straight from a level's records without building the row form
    /// first — what the fold writes.
    ///
    /// **Equal to [`Self::compose`] over the row form of the same level**, by construction rather
    /// than by an argument: both read `RowSpace::project_base`'s output, which is precisely what
    /// [`MembershipRows`] stores. What differs is what is held while it runs — one membership at a
    /// time rather than the whole level's, which is the same asymmetry
    /// [`crate::tile_index::TileIndex::project`] takes and for the same reason.
    ///
    /// **`level` is called more than once**, and it has to be: a list column is an offset table and
    /// a value array, and sizing the first needs a pass the second then fills. A label column takes
    /// one pass and is handed the same closure.
    ///
    /// ⊘ **No production caller.** The fold writes its columns through
    /// `tessera_store::derived::project_row_column` and the engine composes through the same
    /// function, so what reaches this is `crates/tessera-bench/src/bin/epoch_shard_tile_index.rs`
    /// and this crate's tests. Kept because it is the projection the two routes are asserted equal
    /// against.
    pub fn project<'a, I>(
        ordinals: u32,
        space: &RowSpace,
        layout: ServingLayout,
        scratch: &std::path::Path,
        level: impl Fn() -> I,
    ) -> Option<Self>
    where
        I: Iterator<Item = (u32, &'a ArtifactRecord)>,
    {
        let each = |visit: &mut dyn FnMut(u32, &Bitmap)| {
            for (ordinal, record) in level() {
                visit(ordinal, &space.project_base(&record.members));
            }
        };
        Self::assemble(ordinals, space.base_rows(), layout, scratch, &each)
    }

    /// Open a fold-written column, mapped in place, and check it is the form the manifest claims.
    ///
    /// **The tag is checked against the file, not trusted over it** (selection memo §5). Each format
    /// carries its own magic, so a manifest that names a list where a label column sits refuses at
    /// the first bytes rather than reading an offset table as labels — and the caller's answer to a
    /// refusal is to recompose the level, which is what every request did before this structure
    /// existed.
    pub fn open(path: &std::path::Path, expected: ServingLayout) -> tessera_store::Result<Self> {
        let pack = match expected {
            ServingLayout::RowMajorLabel => Pack::Label(LabelColumnPack::open(path)?),
            ServingLayout::RowMajorList => Pack::List(ListColumnPack::open(path)?),
            ServingLayout::ArtifactMajor => {
                return Err(tessera_store::StoreError::MalformedBundle {
                    detail: format!(
                        "row-major column {}: the manifest tags it {}, which has no column — the \
                         entry names a file no writer produces",
                        path.display(),
                        expected.pin_word()
                    ),
                })
            }
        };
        Ok(Self::over(pack))
    }

    /// **A label column over `labels`, addressed by row** — what a *predicate* level's membership
    /// is, built from the value column the layer names rather than from any stored membership.
    ///
    /// **The label form only**, because a single-valued column partitions by construction: every
    /// row carries one value, so the double claim [`Self::compose`] has to guard against cannot
    /// arise here. A row no artifact claims carries [`ROW_COLUMN_HOLE`], which is what a point with
    /// no value for the column has.
    pub fn from_labels(ordinals: u32, labels: &[u32]) -> Self {
        let bytes = pack_label_column(ordinals, labels);
        Self::over(Pack::Label(
            LabelColumnPack::from_bytes(bytes)
                .expect("a column this crate just packed frames by construction"),
        ))
    }

    /// This column with `tail` attached — the rows above its base that a fold has not yet absorbed.
    ///
    /// **The base is shared, not copied.** A predicate level's base is valid for a whole prefix and
    /// its tail moves at every flush, so the flush pays one walk of the tail rather than a second
    /// copy of four bytes a row.
    ///
    /// ⊘ **The label form only.** A list column's tail would be an offset table continuing the
    /// base's, and no predicate produces one — the two row-major forms a *stored* membership takes
    /// are written whole by the fold and have no live half.
    pub fn with_tail(&self, tail: TailLabels) -> Self {
        let mut declared = self.base_declared.as_ref().clone();
        for label in &tail.labels {
            if *label != ROW_COLUMN_HOLE {
                if let Some(count) = declared.get_mut(*label as usize) {
                    *count += 1;
                }
            }
        }
        RowColumn {
            pack: Arc::clone(&self.pack),
            declared,
            base_declared: Arc::clone(&self.base_declared),
            base_extents: Arc::clone(&self.base_extents),
            tail: Some(tail),
            // A predicate base is never amended — `bring_forward` and `extend_flushed` take a
            // stored level only — so there is no amendment to carry, and `declared` above counts
            // none. Carrying one here would understate the proportional criterion's denominator.
            added: None,
        }
    }

    /// **Add `pairs` at the rows they name** — what a growth, a publication or a flush writes into
    /// a level whose column is already built.
    ///
    /// `pairs` is `(row, ordinal)` in any order, repeats allowed. A pair the column already carries
    /// is dropped rather than counted twice. The cost is the batch and not what has accumulated:
    /// the batch is sorted on its own, merged into the amendment already held
    /// ([`Added::merge`]), and moves [`Self::declared`] by one per pair added. The pack is neither
    /// read whole, copied nor rewritten: it is shared, exactly as [`Self::with_tail`] shares it.
    ///
    /// **`false` where the form cannot express the result**, and the column is then as it was —
    /// the label column, and a row that would come to carry two artifacts, whether the first claim
    /// is in the pack, in an earlier amendment or elsewhere in this batch. That is the double claim
    /// the module doc forbids: the memberships have stopped partitioning, so the layout has
    /// stopped being true, and the caller's answer is the artifact-major route, which answers
    /// identically and says so. A list column has no such case.
    ///
    /// **Recomposing instead is what this replaces.** At rung 3's `mesh/descriptors` — 30,217
    /// artifacts over 1.66×10⁹ entries — composing the list column again cost ~100 s on the
    /// executor thread, where it blocks every ingest and every deny, for one entity joining three
    /// artifacts (`2026-09-03-post-flush-artifact-frames.md`).
    pub fn amend(&mut self, pairs: &[(u32, u32)], row_count: u32) -> bool {
        let label_form = matches!(*self.pack, Pack::Label(_));
        let mut batch: Vec<(u32, u32)> = pairs.to_vec();
        batch.sort_unstable();
        batch.dedup();
        // What the column does not yet carry, in batch order. Nothing is written until the whole
        // batch has been read, so a refusal leaves the column as it was.
        let mut kept: Vec<(u32, u32)> = Vec::with_capacity(batch.len());
        for (row, ordinal) in batch {
            let mut carried = false;
            let mut occupied = false;
            self.for_each_label(row, |held| {
                occupied = true;
                carried |= held == ordinal;
            });
            if carried {
                continue;
            }
            // `kept` is ascending by row, so a second claim inside the batch is the pair before.
            if label_form && (occupied || kept.last().is_some_and(|(r, _)| *r == row)) {
                return false;
            }
            kept.push((row, ordinal));
        }
        let added = self.added.get_or_insert_with(Added::default);
        added.row_end = added.row_end.max(row_count);
        if let Some((last, _)) = kept.last() {
            added.row_end = added.row_end.max(last.saturating_add(1));
        }
        // **A publication adds ordinals the pack never had**, and `declared` is what [`Self::len`]
        // answers from — so the column grows to cover them or every reader sized by that length
        // would index past its own count.
        if let Some(highest) = kept.iter().map(|(_, ordinal)| *ordinal as usize + 1).max() {
            if self.declared.len() < highest {
                self.declared.resize(highest, 0);
            }
        }
        for (row, ordinal) in &kept {
            self.declared[*ordinal as usize] += 1;
            added.rows.add(*row);
        }
        added.rows.run_optimize();
        added.merge(kept);
        true
    }

    /// **Give up every label in `lo..hi` and take `pairs` in their place** — what a row-space merge
    /// does to a level's column: the rows inside the merged span name other entities afterwards,
    /// so the labels there are dropped and the same memberships are labelled again at the rows
    /// they now hold. `pairs` is the renumbered span's `(row, ordinal)` in any order, as
    /// [`Self::amend`] takes them.
    ///
    /// **Only the amendment can hold a label inside the span**, and that is what makes this an
    /// edit rather than a composition: the pack covers the base rows alone
    /// ([`Self::compose_over_base`]), a merge never consumes the base segment, and a live tail
    /// belongs to an attribute predicate whose column is composed again at every geometry move
    /// and is never rebased. So the span's labels are drained from the amendment in one move, the
    /// counts go down by what was drained, and the new pairs enter through `amend`. A merge
    /// preserves the row count, so `row_count` is what it was.
    ///
    /// `false` on [`Self::amend`]'s terms — the label form and a row that would carry two
    /// artifacts. The span's old labels are already gone by then, so the caller does what it does
    /// for a refused amendment: the level is served artifact-major from here on, and every answer
    /// is unchanged.
    pub fn rebase(&mut self, lo: u32, hi: u32, pairs: &[(u32, u32)], row_count: u32) -> bool {
        debug_assert!(
            lo >= self.base_rows(),
            "a merge's span begins inside the base rows, which a merge never consumes"
        );
        if let Some(added) = &mut self.added {
            for (_, ordinal) in added.drain_span(lo, hi) {
                if let Some(count) = self.declared.get_mut(ordinal as usize) {
                    *count = count.saturating_sub(1);
                }
            }
        }
        self.amend(pairs, row_count)
    }

    /// Which form this is.
    pub fn layout(&self) -> ServingLayout {
        match *self.pack {
            Pack::Label(_) => ServingLayout::RowMajorLabel,
            Pack::List(_) => ServingLayout::RowMajorList,
        }
    }

    /// How many ordinals this column covers, holes included.
    pub fn len(&self) -> usize {
        self.declared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }

    /// The row space this column was addressed in — the base, plus the tail where it has one.
    pub fn row_count(&self) -> u32 {
        let base = match &*self.pack {
            Pack::Label(pack) => pack.rows(),
            Pack::List(pack) => pack.rows(),
        };
        let with_tail = match &self.tail {
            Some(tail) => tail.row_end().max(base),
            None => base,
        };
        // **And the amendment**, which may name rows above both: a flush publishes rows the pack
        // never covered and there is no tail on a list column to hold them.
        match &self.added {
            Some(added) => added.row_end.max(with_tail),
            None => with_tail,
        }
    }

    /// The base's row count alone — where the durable half ends and the live one begins.
    pub fn base_rows(&self) -> u32 {
        match &*self.pack {
            Pack::Label(pack) => pack.rows(),
            Pack::List(pack) => pack.rows(),
        }
    }

    /// The artifact's **unmasked** membership size in this view's row space — the proportional
    /// criterion's denominator on a row-major level.
    ///
    /// Zero for a hole and for a live artifact whose membership projects to nothing, exactly as the
    /// row form's cardinality is: the two are told apart by the records, never here.
    pub fn declared_size(&self, ordinal: u32) -> u64 {
        self.declared
            .get(ordinal as usize)
            .copied()
            .map(u64::from)
            .unwrap_or(0)
    }

    /// **Candidacy: one scan of `viewport ∩ M_auth`, marking labels.**
    ///
    /// Every ordinal returned has a member the viewer can see inside the viewport — which is
    /// *exactly* the question the artifact-major route reaches through the tile index's walk and a
    /// masked probe per candidate. There is no separate probe here because the scan already asked
    /// it: a row in `here` is visible by construction, so the artifact it labels has a visible
    /// member in view.
    ///
    /// `here` must come from the composed mask and from nothing else — see
    /// [`MaskedSet::visible_rows`], which is the only way to obtain one.
    ///
    /// **Ascending**, which is what the cut downstream is entitled to.
    ///
    /// A `Vec<bool>` over the level's ordinals rather than adding into the bitmap as the scan goes:
    /// a scattered layer's rows hit the same handful of ordinals over and over, and a set insert per
    /// row is the cost the marking array exists to remove. It is one byte per artifact for the
    /// length of the call.
    pub fn candidates(&self, here: &Bitmap) -> Bitmap {
        let mut seen = vec![false; self.len()];
        let base_rows = self.base_rows();
        match &*self.pack {
            Pack::Label(pack) => {
                for row in here.iter() {
                    // **The tail answers for the rows above the base**, which is what makes a
                    // point ingested since the last fold a candidate on the next request: its row
                    // is above the base, so the packed column does not label it and the live half
                    // does.
                    let label = if row < base_rows {
                        pack.label(row as usize)
                    } else {
                        self.tail.as_ref().map_or(ROW_COLUMN_HOLE, |t| t.label(row))
                    };
                    if label != ROW_COLUMN_HOLE {
                        seen[label as usize] = true;
                    }
                }
            }
            Pack::List(pack) => {
                for row in here.iter() {
                    if row >= base_rows {
                        continue;
                    }
                    for ordinal in pack.list(row as usize) {
                        seen[ordinal as usize] = true;
                    }
                }
            }
        }
        // **The amendment, as a second pass over the rows it names that are in view** — never per
        // row of `here`, which is the scan this layout exists to keep at one pass. `and` is
        // O(containers touched), so a column nothing has amended pays one empty intersection.
        if let Some(added) = &self.added {
            for row in added.rows.and(here).iter() {
                for (_, ordinal) in added.at(row) {
                    if let Some(hit) = seen.get_mut(*ordinal as usize) {
                        *hit = true;
                    }
                }
            }
        }
        let mut out = Bitmap::new();
        for (ordinal, hit) in seen.iter().enumerate() {
            if *hit {
                out.add(ordinal as u32);
            }
        }
        out.run_optimize();
        out
    }

    /// Every artifact of this level that labels `row`: one for the label form, any number for the
    /// list form, none at a hole and — for a list column, which has no live tail — none above the
    /// base. The per-point membership column reads a served point's leaf here
    /// (`client-components.md` §5.10); `row` is one the caller already gathered, so this discloses
    /// nothing the walk up to a served ancestor does not then bound.
    pub fn for_each_label(&self, row: u32, mut visit: impl FnMut(u32)) {
        let base_rows = self.base_rows();
        match &*self.pack {
            Pack::Label(pack) => {
                let label = if row < base_rows {
                    pack.label(row as usize)
                } else {
                    self.tail.as_ref().map_or(ROW_COLUMN_HOLE, |t| t.label(row))
                };
                if label != ROW_COLUMN_HOLE {
                    visit(label);
                }
            }
            Pack::List(pack) => {
                if row < base_rows {
                    for ordinal in pack.list(row as usize) {
                        visit(ordinal);
                    }
                }
            }
        }
        // **And whatever a write added at this row** ([`Self::added`]). Asked of a bitmap first, so
        // a column nothing has amended pays one `contains` and a column that has pays a binary
        // search only at the rows it names — this is on the histogram's per-row walk.
        if let Some(added) = &self.added {
            if added.rows.contains(row) {
                for (_, ordinal) in added.at(row) {
                    visit(*ordinal);
                }
            }
        }
    }


    /// **The artifact-major bitmaps, derived by transposing this column** — the level's membership
    /// in row space, read off the row form instead of projected a second time.
    ///
    /// A level recorded row-major has both forms at open: the column the fold wrote, and the
    /// row form every artifact-major answer is still computed from (see the module doc). Building
    /// the second by projecting every membership through the permutation costs a decode and a
    /// projection of the whole level, whose entries are already sitting in this column.
    /// Transposing them is the same information at one sequential read, and the two are asserted
    /// equal artifact for artifact.
    ///
    /// **Measured at rung 3's `mesh/descriptors`** — 30,217 artifacts, 36M rows, 1.66×10⁹ entries,
    /// warm cache, single-threaded (`probes/2026-09-02-cold-start/`): the projection takes
    /// **23.6–25.1 s** and this takes **13.8–15.4 s**, of which 0.9 s counts, 4.9 s places, 6.8 s
    /// encodes and 1.3 s deserialises the finished bitmaps.
    ///
    /// **The base alone**, which is exactly what [`RowSpace::project_base`] produces and therefore
    /// what a projected form holds: `None` where a tail is attached, so a caller can never be
    /// handed a form that stops short of the rows the live half labels.
    ///
    /// One [`Bitmap`] per ordinal this column covers, holes included — a hole transposes to an
    /// empty bitmap, and telling an empty artifact from a hole is the caller's job, from the
    /// records, exactly as it is on the projecting route.
    ///
    /// # Why it is blocked and counting-sorted rather than added row by row
    ///
    /// Appending each row to its ordinals' bitmaps as the walk reaches it touches a different
    /// container on every value — 30,217 of them interleaved at rung 3 — and that measured
    /// **68 s**, three times the projection it was meant to replace. The walk is instead cut into
    /// blocks of **2¹⁶ rows, which is exactly one Roaring container**, each block counting-sorted
    /// by ordinal so that every ordinal's rows arrive contiguous and ascending. Each run is then
    /// the container's members, and it is handed to that ordinal's [`tessera_roaring::Sink`]
    /// **finished** — the array form below croaring's threshold, stamped words above it — rather
    /// than inserted value by value.
    ///
    /// The intermediate routes are all measured, because each looked like the answer:
    /// `Bitmap::add_many` per run is **19 s** (the inserts alone 11.6 s); `Sink::push_block` for
    /// every container is **24 s**, because a sparse container's payload is scanned out of 8 KB of
    /// words whatever it holds; `Sink::push_members` staged is **17.3 s**, the staging flush
    /// cloning every container a second time; and unstaged, which is this, **14.5 s**. The block is at least 2¹⁶ rows — a Roaring block, so a short
    /// ordinal's run lands inside one container — and grows with the ordinal count so that the
    /// per-block sweep over the offset table stays bounded by the row count rather than
    /// multiplying by it.
    pub fn transpose(&self) -> Option<Vec<Bitmap>> {
        // **And an amended column is refused for the tail's reason**: what a transposition must
        // produce is the base alone, and an amendment names rows the base does not carry.
        if self.tail.is_some() || self.added.is_some() {
            return None;
        }
        let ordinals = self.len();
        if ordinals == 0 {
            return Some(Vec::new());
        }
        let base_rows = self.base_rows();
        // **One sink per ordinal, each holding its containers until the walk is done.** A sink
        // that flushed as it filled would hand croaring a stream every 128 containers and union
        // it in, and a union clones every container it takes — at a few hundred members a
        // container that second copy is most of what the encoder costs. Unstaged, each artifact's
        // membership is deserialised once and becomes the bitmap without a merge; what it costs
        // instead is the serialized bytes of the whole level held until [`Sink::finish`], which
        // is the row form this is about to produce anyway.
        //
        // Empty for a hole and for an artifact this column labels no row with, which is the same
        // answer an empty projection gives.
        let mut sinks: Vec<tessera_roaring::Sink> =
            (0..ordinals)
                .map(|_| tessera_roaring::Sink::unstaged())
                .collect();
        // The block's counting sort: how many rows each ordinal takes, where its run starts, how
        // far it has been filled, and which ordinals the block touched at all. All four are
        // allocated once for the whole walk; only `touched` is swept per block, so a level with far
        // more ordinals than a block has rows costs its own size once rather than once per block.
        let mut counts = vec![0usize; ordinals];
        let mut starts = vec![0usize; ordinals];
        let mut cursor = vec![0usize; ordinals];
        let mut touched: Vec<u32> = Vec::new();
        let mut values: Vec<u32> = Vec::new();
        let mut words = [0u64; tessera_roaring::WORDS];
        let block = tessera_roaring::BLOCK as u32;
        let mut lo = 0u32;
        while lo < base_rows {
            let hi = lo.saturating_add(block).min(base_rows);
            touched.clear();
            {
                let (counts, touched) = (&mut counts, &mut touched);
                let mut count = |ordinal: u32| {
                    let at = ordinal as usize;
                    if counts[at] == 0 {
                        touched.push(ordinal);
                    }
                    counts[at] += 1;
                };
                match &*self.pack {
                    // **The values alone**, read straight through: counting needs the ordinal and
                    // not the row, so the offset table is touched twice for the whole block.
                    Pack::List(pack) => pack.for_each_value(lo as usize, hi as usize, count),
                    Pack::Label(_) => {
                        for row in lo..hi {
                            self.for_each_label(row, &mut count);
                        }
                    }
                }
            }
            let mut total = 0usize;
            for ordinal in &touched {
                let at = *ordinal as usize;
                starts[at] = total;
                cursor[at] = total;
                total += counts[at];
            }
            values.clear();
            values.resize(total, 0);
            {
                let (cursor, values) = (&mut cursor, &mut values);
                let mut place = |row: u32, ordinal: u32| {
                    let at = ordinal as usize;
                    values[cursor[at]] = row;
                    cursor[at] += 1;
                };
                match &*self.pack {
                    Pack::List(pack) => pack.for_each_row_value(lo as usize, hi as usize, place),
                    Pack::Label(_) => {
                        for row in lo..hi {
                            self.for_each_label(row, |ordinal| place(row, ordinal));
                        }
                    }
                }
            }
            // **The container, handed over finished.** A block is 2¹⁶ rows and a Roaring block is
            // 2¹⁶ values, so every row of this block lands in one container of one key — the words
            // below *are* that container, and the sink writes it into the portable stream rather
            // than croaring inserting it value by value.
            let key = u16::try_from(lo >> 16).expect("a row below 2³² has a block key below 2¹⁶");
            for ordinal in &touched {
                let at = *ordinal as usize;
                let (from, to) = (starts[at], cursor[at]);
                // **The run is already the container's members, ascending**, which is the array
                // form's whole case: below croaring's threshold the payload is those members as
                // `u16`s, so stamping them into 8 KB of words for the encoder to scan back out
                // would be 1,024 word reads for a few hundred members. Above it the payload *is*
                // the words, and stamping them is what the block form takes.
                if to - from <= tessera_roaring::ARRAY_MAX as usize {
                    sinks[at].push_members(key, &values[from..to]);
                } else {
                    // Counted as it is stamped rather than taken from the run's length, because
                    // `push_block` requires the popcount and a row a column listed twice would
                    // otherwise inflate it — the descriptor and the payload must agree.
                    let mut card = 0u32;
                    for row in &values[from..to] {
                        let offset = (row - lo) as usize;
                        let bit = 1u64 << (offset & 63);
                        let word = &mut words[offset >> 6];
                        if *word & bit == 0 {
                            *word |= bit;
                            card += 1;
                        }
                    }
                    sinks[at].push_block(key, card, &words);
                    // Cleared by the rows that set it — O(members) rather than the 8 KB the block
                    // is wide.
                    for row in &values[from..to] {
                        words[((row - lo) as usize) >> 6] = 0;
                    }
                }
                counts[at] = 0;
            }
            lo = hi;
        }
        Some(sinks.into_iter().map(tessera_roaring::Sink::finish).collect())
    }

    /// **The masked count for every artifact of this level, in one walk of the mask** — decision
    /// 0093's one named exception, and the only route a row-major level has to the quantity the
    /// disclosure rule requires.
    ///
    /// **Over the whole mask, not over the viewport.** A viewer is told how many of an artifact's
    /// documents they can see, which does not change as they pan; a per-viewport count would move
    /// with the box and let a viewer difference two boxes for the members in between.
    ///
    /// **Filter-blind**, exactly as [`MaskedSet::count_intersection`] is: a filtered count here
    /// would make an artifact's existence criterion a function of the filter, so an artifact would
    /// appear and disappear as a viewer typed — a filter moving the frontier down, which **I12**
    /// forbids. [`MaskedSet::visible_all`] is the composed mask and carries no filter, which is what
    /// makes that structural rather than remembered.
    ///
    /// `u32` per artifact, which is ~4 B each — 4 MB at 10⁶ artifacts and 40 MB at 10⁷ — and is why
    /// the cache holding these is byte-budgeted (`crate::histogram`). A count cannot exceed the row
    /// space, which is `u32`-addressed.
    pub fn histogram(&self, mask: &impl WholeMask) -> Vec<u32> {
        self.histogram_over(&mask.visible_all())
    }

    /// The same walk over a row set the caller already holds — [`Self::histogram`]'s body, and its
    /// only other caller is browse's **filtered** count (`highlight-and-hierarchy.md` §4), which
    /// asks for `M_auth ∩ filter` rather than for `M_auth`.
    ///
    /// **Taking a bitmap rather than a mask is what makes that expressible without weakening the
    /// mask-only rule above.** [`Self::histogram`] is filter-blind because the count beside an
    /// artifact is what the principal may see; this is the *second* number §4 defines, and it is
    /// obtained by narrowing an already-composed set. A caller handing it anything not derived
    /// from the composed mask would be counting rows outside `M_auth`, which is why the one
    /// production caller narrows [`WholeMask::visible_all`] and nothing else.
    /// # Why it is split, and what the split does not change
    ///
    /// The walk is over **every visible row**, and a list column reads a row's whole label list —
    /// rung 3's `mesh/descriptors` is ~46 labels over 3.6 × 10⁷ rows, so one pass is
    /// 1.7 × 10⁹ increments and was **measured at 2.7 s** single-threaded, paid once per session
    /// by whichever request first needs the level's counts (a browse page, or a `member_of`
    /// leaf's gate).
    ///
    /// So the row space is cut into chunks and each is walked on its own thread into its own count
    /// vector, summed at the end — the same shape the row route's own scan takes, on the pool the
    /// caller installs. **The answer is identical**: addition is associative, every row lands in
    /// exactly one chunk, and no chunk sees a row outside `visible`.
    pub fn histogram_over(&self, visible: &croaring::Bitmap) -> Vec<u32> {
        use rayon::prelude::*;

        let ordinals = self.len();
        let Some(last) = visible.maximum() else {
            return vec![0u32; ordinals];
        };
        // One chunk per worker, floored so a small view is not split into slivers whose per-chunk
        // count vector costs more than the walk it saves.
        const MIN_CHUNK: u64 = 1 << 21;
        let span = last as u64 + 1;
        let workers = rayon::current_num_threads().max(1) as u64;
        let chunk = (span.div_ceil(workers)).max(MIN_CHUNK);
        let chunks = span.div_ceil(chunk);
        (0..chunks)
            .into_par_iter()
            .map(|c| {
                let lo = u32::try_from(c * chunk).unwrap_or(u32::MAX);
                let end = (c + 1) * chunk;
                let mut counts = vec![0u32; ordinals];
                let mut rows = visible.iter();
                rows.reset_at_or_after(lo);
                for row in rows {
                    if (row as u64) >= end {
                        break;
                    }
                    self.for_each_label(row, |ordinal| counts[ordinal as usize] += 1);
                }
                counts
            })
            .reduce(
                || vec![0u32; ordinals],
                |mut acc, part| {
                    for (a, b) in acc.iter_mut().zip(part) {
                        *a += b;
                    }
                    acc
                },
            )
    }

    /// **One pass over the visible rows accumulating every artifact's count, position sum and
    /// bounding box at once** — the masked count and the two derived properties a level served
    /// from its column alone has no per-artifact membership to compute one at a time.
    ///
    /// [`Self::histogram_over`]'s walk with two more accumulators on it, chunked and reduced the
    /// same way and for the same reason. The row is read once and its position once, whatever the
    /// layer declares: reading it again per property would multiply the only expensive term.
    ///
    /// **Why an accumulation and not a per-artifact walk.** The alternative is to take one
    /// artifact's rows out of the column and compute over them, which costs the visible rows inside
    /// that artifact's extent — and a *scattered* artifact's extent is the whole row space, so the
    /// walk is `|M_auth|` per artifact and a viewport serving a hundred of them pays it a hundred
    /// times. This pays it once for the level, per session, under the same key and the same byte
    /// budget the counts are under (`crate::histogram`).
    ///
    /// `position` answers a row's grid position, or `None` for a row no segment places — dropped
    /// rather than defaulted, exactly as [`crate::derived::RowLocator::position`]'s caller drops it:
    /// `(0, 0)` is a real position and a row the space cannot place would pull the mean to the
    /// origin.
    ///
    /// ⊘ **The transient is one accumulator set per worker** — a `u32`, two `f64` and four `u32` an
    /// ordinal, 36 B, so 58 MB a level at 1.6×10⁶ artifacts times the pool's width while the pass
    /// runs. That is the shape [`Self::histogram_over`] already has at 4 B an ordinal.
    pub fn accumulate_over(
        &self,
        visible: &croaring::Bitmap,
        position: &(dyn Fn(u32) -> Option<(u32, u32)> + Sync),
    ) -> LevelAccumulation {
        use rayon::prelude::*;

        let ordinals = self.len();
        let empty = || LevelAccumulation {
            counts: vec![0u32; ordinals],
            sums: vec![[0.0f64; 2]; ordinals],
            boxes: vec![[u32::MAX, u32::MAX, 0, 0]; ordinals],
        };
        let Some(last) = visible.maximum() else {
            return empty();
        };
        const MIN_CHUNK: u64 = 1 << 21;
        let span = last as u64 + 1;
        let workers = rayon::current_num_threads().max(1) as u64;
        let chunk = (span.div_ceil(workers)).max(MIN_CHUNK);
        let chunks = span.div_ceil(chunk);
        (0..chunks)
            .into_par_iter()
            .map(|c| {
                let lo = u32::try_from(c * chunk).unwrap_or(u32::MAX);
                let end = (c + 1) * chunk;
                let mut acc = empty();
                let mut rows = visible.iter();
                rows.reset_at_or_after(lo);
                for row in rows {
                    if (row as u64) >= end {
                        break;
                    }
                    let Some((x, y)) = position(row) else {
                        continue;
                    };
                    self.for_each_label(row, |ordinal| {
                        let i = ordinal as usize;
                        acc.counts[i] += 1;
                        acc.sums[i][0] += x as f64;
                        acc.sums[i][1] += y as f64;
                        let b = &mut acc.boxes[i];
                        b[0] = b[0].min(x);
                        b[1] = b[1].min(y);
                        b[2] = b[2].max(x);
                        b[3] = b[3].max(y);
                    });
                }
                acc
            })
            .reduce(empty, |mut a, b| {
                for i in 0..a.counts.len() {
                    a.counts[i] += b.counts[i];
                    a.sums[i][0] += b.sums[i][0];
                    a.sums[i][1] += b.sums[i][1];
                    a.boxes[i][0] = a.boxes[i][0].min(b.boxes[i][0]);
                    a.boxes[i][1] = a.boxes[i][1].min(b.boxes[i][1]);
                    a.boxes[i][2] = a.boxes[i][2].max(b.boxes[i][2]);
                    a.boxes[i][3] = a.boxes[i][3].max(b.boxes[i][3]);
                }
                a
            })
    }

    /// The durable bytes — what the fold writes into the prefix.
    ///
    /// **The base alone**, and that is the same rule the layout rests on: a fold renumbers the base
    /// row space and publishes a new prefix, so the tail it would have written is exactly the part
    /// that fold has just absorbed.
    pub fn as_bytes(&self) -> &[u8] {
        match &*self.pack {
            Pack::Label(pack) => pack.as_bytes(),
            Pack::List(pack) => pack.as_bytes(),
        }
    }

    /// One pass over the packed column, folding up the per-artifact declared sizes.
    fn over(pack: Pack) -> Self {
        let ordinals = match &pack {
            Pack::Label(pack) => pack.ordinals(),
            Pack::List(pack) => pack.ordinals(),
        };
        let mut declared = vec![0u32; ordinals as usize];
        // **The extents come off the same walk**, so a level served from the column alone pays no
        // second pass for them: `u32::MAX, 0` is the empty accumulator and reads back as the
        // *empty* sentinel, which is what an ordinal no row labels is.
        let mut extents = vec![(u32::MAX, 0u32); ordinals as usize];
        let mut widen = |ordinal: u32, row: usize| {
            let e = &mut extents[ordinal as usize];
            let row = row as u32;
            e.0 = e.0.min(row);
            e.1 = e.1.max(row);
        };
        match &pack {
            Pack::Label(pack) => {
                for row in 0..pack.rows() as usize {
                    let label = pack.label(row);
                    if label != ROW_COLUMN_HOLE {
                        declared[label as usize] += 1;
                        widen(label, row);
                    }
                }
            }
            Pack::List(pack) => {
                for row in 0..pack.rows() as usize {
                    for ordinal in pack.list(row) {
                        declared[ordinal as usize] += 1;
                        widen(ordinal, row);
                    }
                }
            }
        }
        RowColumn {
            pack: Arc::new(pack),
            base_declared: Arc::new(declared.clone()),
            base_extents: Arc::new(extents),
            declared,
            tail: None,
            added: None,
        }
    }

    /// **Each artifact's lowest and highest row, from the column's own bytes** — the extents a
    /// level served from the column alone is placed in the tile index by.
    ///
    /// `live` marks the ordinals the level has a record for, parallel to the level's ordinals: the
    /// column cannot tell a **hole** from an artifact whose membership projects to nothing, both
    /// labelling no row, and the two are different things (`crate::tile_index::Extent`). An ordinal
    /// `live` does not mark is a hole; one it marks that no row labels is empty.
    ///
    /// The base half is folded at construction ([`Self::base_extents`]); the tail and the labels a
    /// write added are walked here, and both are bounded by what has arrived since the last fold
    /// rather than by the row space.
    pub fn extents(&self, live: &[bool]) -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = self
            .base_extents
            .iter()
            .enumerate()
            .map(|(ordinal, &(lo, hi))| {
                match live.get(ordinal) {
                    Some(true) | None if lo <= hi => (lo, hi),
                    Some(true) | None => TILE_INDEX_EMPTY,
                    Some(false) => TILE_INDEX_HOLE,
                }
            })
            .collect();
        let mut widen = |ordinal: u32, row: u32| {
            let Some(e) = out.get_mut(ordinal as usize) else {
                return;
            };
            if *e == TILE_INDEX_HOLE {
                return;
            }
            if *e == TILE_INDEX_EMPTY {
                *e = (row, row);
            } else {
                e.0 = e.0.min(row);
                e.1 = e.1.max(row);
            }
        };
        if let Some(tail) = &self.tail {
            for row in tail.row_base..tail.row_end() {
                let label = tail.label(row);
                if label != ROW_COLUMN_HOLE {
                    widen(label, row);
                }
            }
        }
        if let Some(added) = &self.added {
            for row in added.rows.iter() {
                for (_, ordinal) in added.at(row) {
                    widen(*ordinal, row);
                }
            }
        }
        out
    }

    /// [`Self::assemble`] fed by `(row, ordinal)` pairs — see
    /// [`tessera_store::derived::project_row_column_pairs`]. The two share the composition; what
    /// differs is only how the caller has the membership to hand.
    fn assemble_pairs(
        ordinals: u32,
        row_count: u32,
        layout: ServingLayout,
        scratch: &std::path::Path,
        each: tessera_store::derived::PairWalk<'_>,
    ) -> Option<Self> {
        let path = match tessera_store::derived::project_row_column_pairs(
            ordinals, row_count, layout, scratch, each,
        ) {
            Ok(Some(path)) => path,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "a row-major column would not be composed from the column it replaces"
                );
                return None;
            }
        };
        let pack = match layout {
            ServingLayout::RowMajorLabel => LabelColumnPack::open(&path).map(Pack::Label),
            _ => ListColumnPack::open(&path).map(Pack::List),
        };
        let _ = std::fs::remove_file(&path);
        match pack {
            Ok(pack) => Some(Self::over(pack)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "a row-major column this process just composed would not be read back"
                );
                None
            }
        }
    }

    /// The two builders, sharing one walk protocol: `each` calls `visit` once per live artifact with
    /// its projected rows, and may be called more than once.
    fn assemble(
        ordinals: u32,
        row_count: u32,
        layout: ServingLayout,
        scratch: &std::path::Path,
        each: LevelWalk<'_>,
    ) -> Option<Self> {
        // **The composition writes the column into a file and this reads it back**, which is what
        // it costs to have one implementation of the column on both sides (owner ruling,
        // 2026-09-12; `docs/evidence/memos/2026-09-12-bounded-assembly-design.md` §4.6). What the
        // file buys is the row-sized lane it replaces: the pass held four bytes a row while it
        // composed, and now holds one partition bucket. The bytes it reads back are the column the
        // form was going to hold anyway.
        let path = match tessera_store::derived::project_row_column(
            ordinals, row_count, layout, scratch, each,
        ) {
            Ok(Some(path)) => path,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "a row-major column would not be composed; the level is served artifact-major"
                );
                return None;
            }
        };
        // **Mapped, not read into a heap vector.** The column is 4 B a row for the label form and
        // more for the list one — 14 GB a level at rung 6 — and reading it back would put the
        // row-sized array the composition just stopped holding straight back on the heap, on the
        // serving side. The pack frames a mapping and an owned buffer through the same checks, so
        // what is served is the same structure either way.
        //
        // **Unlinked as soon as it is mapped**: the mapping holds the bytes after the directory
        // entry goes, and the entry is this process's scratch that nothing else reads.
        let pack = match layout {
            ServingLayout::RowMajorLabel => LabelColumnPack::open(&path).map(Pack::Label),
            _ => ListColumnPack::open(&path).map(Pack::List),
        };
        let _ = std::fs::remove_file(&path);
        match pack {
            Ok(pack) => Some(Self::over(pack)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "a row-major column this process just composed would not be read back; the \
                     level is served artifact-major"
                );
                None
            }
        }
    }

    /// Frame a column this process just produced and read it back through the same checks a mapped
    /// file takes — the reason [`crate::containment::ContainmentPartition`] gives: the two routes
    /// are one reader, so a framing rule can never hold for a file and not for the form a
    /// publication built.
    pub fn of_bytes(bytes: Vec<u8>, layout: ServingLayout) -> Self {
        let pack = match layout {
            ServingLayout::RowMajorLabel => Pack::Label(
                LabelColumnPack::from_bytes(bytes)
                    .expect("a column this crate just packed frames by construction"),
            ),
            _ => Pack::List(
                ListColumnPack::from_bytes(bytes)
                    .expect("a column this crate just packed frames by construction"),
            ),
        };
        Self::over(pack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::MaskedSet;

    /// The scratch a composition partitions through. One directory for the whole test binary: a
    /// composition names its own files and removes them, so they cannot collide.
    fn scratch() -> &'static std::path::Path {
        static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        DIR.get_or_init(|| tempfile::tempdir().expect("a scratch directory"))
            .path()
    }

    fn composed(
        membership: &MembershipRows,
        row_count: u32,
        layout: ServingLayout,
    ) -> Option<RowColumn> {
        RowColumn::compose(membership, row_count, layout, scratch())
    }

    fn composed_over_base(
        membership: &MembershipRows,
        base_rows: u32,
        row_count: u32,
        layout: ServingLayout,
    ) -> Option<RowColumn> {
        RowColumn::compose_over_base(membership, base_rows, row_count, layout, scratch())
    }

    fn rows_of(sets: &[Option<&[u32]>]) -> MembershipRows {
        MembershipRows::of_rows(
            sets.iter()
                .map(|set| set.map(|s| s.iter().copied().collect::<Bitmap>()))
                .collect(),
        )
    }

    fn bitmap(values: &[u32]) -> Bitmap {
        values.iter().copied().collect()
    }

    /// **The label form answers the three questions the route asks of it**, and the hole is a real
    /// state: a row no artifact claims contributes to nobody's count.
    #[test]
    fn a_label_column_answers_candidacy_counts_and_sizes() {
        // Ordinal 0 holds rows 0..3, ordinal 1 holds 5 and 6, ordinal 2 is a hole, ordinal 3 is
        // live with an empty projection. Rows 4, 7, 8, 9 belong to nobody.
        let membership = rows_of(&[Some(&[0, 1, 2]), Some(&[5, 6]), None, Some(&[])]);
        let column =
            composed(&membership, 10, ServingLayout::RowMajorLabel).expect("partitions");

        assert_eq!(column.layout(), ServingLayout::RowMajorLabel);
        assert_eq!(column.len(), 4);
        assert_eq!(column.row_count(), 10);
        assert_eq!(column.declared_size(0), 3);
        assert_eq!(column.declared_size(1), 2);
        assert_eq!(column.declared_size(2), 0, "a hole has no rows");
        assert_eq!(column.declared_size(3), 0, "and neither has an empty one");
        assert_eq!(column.declared_size(99), 0, "past the level is not a panic");

        // Candidacy over a viewport∩mask holding row 1 and row 6.
        assert_eq!(
            column
                .candidates(&bitmap(&[1, 6]))
                .iter()
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        // A viewport holding only unclaimed rows returns nothing.
        assert!(column.candidates(&bitmap(&[4, 8])).is_empty());

        // The histogram is over the whole mask, not the viewport.
        let mask = bitmap(&[0, 2, 5, 9]);
        assert_eq!(column.histogram(&mask), vec![2, 1, 0, 0]);
    }

    /// The list form is the same three answers where a row belongs to several artifacts — which is
    /// exactly the state the label form refuses to represent.
    #[test]
    fn a_list_column_carries_a_row_that_several_artifacts_claim() {
        let membership = rows_of(&[Some(&[0, 1]), Some(&[1, 2]), Some(&[])]);
        let column =
            composed(&membership, 4, ServingLayout::RowMajorList).expect("always builds");
        assert_eq!(column.layout(), ServingLayout::RowMajorList);
        assert_eq!(column.declared_size(0), 2);
        assert_eq!(column.declared_size(1), 2);
        assert_eq!(column.declared_size(2), 0);

        // Row 1 is claimed by both, so a viewport holding only it makes both candidates.
        assert_eq!(
            column.candidates(&bitmap(&[1])).iter().collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(column.histogram(&bitmap(&[0, 1, 2, 3])), vec![2, 2, 0]);
    }

    /// **A level that does not partition is refused a label column**, rather than served one whose
    /// contested rows went to whichever artifact was walked last. The list form takes the same
    /// memberships.
    #[test]
    fn a_double_claim_declines_the_label_form_and_not_the_list_form() {
        let overlapping = rows_of(&[Some(&[0, 1]), Some(&[1, 2])]);
        assert!(composed(&overlapping, 4, ServingLayout::RowMajorLabel).is_none());
        assert!(composed(&overlapping, 4, ServingLayout::RowMajorList).is_some());
        // And artifact-major has no column at all, in either builder.
        assert!(composed(&overlapping, 4, ServingLayout::ArtifactMajor).is_none());
    }

    /// **A composed column and a mapped one are the same structure**, so the fold's consolidation is
    /// a change of backing rather than a second encoder — and a file offered under the wrong tag is
    /// a refusal rather than a misread.
    #[test]
    fn a_column_answers_the_same_mapped_as_composed() {
        let tmp = tempfile::tempdir().unwrap();
        for (layout, name) in [
            (ServingLayout::RowMajorLabel, "c.tslb"),
            (ServingLayout::RowMajorList, "c.tsll"),
        ] {
            let membership = rows_of(&[Some(&[0, 1, 2]), None, Some(&[5, 6]), Some(&[])]);
            let composed = composed(&membership, 8, layout).expect("builds");
            let path = tmp.path().join(name);
            std::fs::write(&path, composed.as_bytes()).unwrap();
            let mapped = RowColumn::open(&path, layout).unwrap();

            assert_eq!(mapped.len(), composed.len());
            assert_eq!(mapped.row_count(), composed.row_count());
            for ordinal in 0..4u32 {
                assert_eq!(
                    mapped.declared_size(ordinal),
                    composed.declared_size(ordinal)
                );
            }
            let here = bitmap(&[1, 6]);
            assert_eq!(
                mapped.candidates(&here).iter().collect::<Vec<_>>(),
                composed.candidates(&here).iter().collect::<Vec<_>>()
            );
            let mask = bitmap(&[0, 1, 5]);
            assert_eq!(mapped.histogram(&mask), composed.histogram(&mask));

            // The other tag over the same bytes: the distinct magics are what make this a refusal.
            let other = match layout {
                ServingLayout::RowMajorLabel => ServingLayout::RowMajorList,
                _ => ServingLayout::RowMajorLabel,
            };
            assert!(RowColumn::open(&path, other).is_err());
            assert!(RowColumn::open(&path, ServingLayout::ArtifactMajor).is_err());
        }
    }

    /// **The transposed form is the membership the column was composed from**, ordinal for
    /// ordinal — the property `ArtifactRows::build_from_column` rests on, and the reason a level
    /// recorded row-major need not project its memberships a second time at open. Both forms, and
    /// the fixtures carry the two states a transposition could lose: a hole and an artifact whose
    /// projection is empty, which transpose to the same empty bitmap and are told apart by the
    /// records rather than here.
    #[test]
    fn a_transposed_column_is_the_membership_it_was_composed_from() {
        // Ordinal 2 is a hole and ordinal 3 is live with an empty projection; rows 4 and 9 belong
        // to nobody. Disjoint, so both forms compose.
        let partitioned = rows_of(&[Some(&[0, 1, 2]), Some(&[5, 6]), None, Some(&[])]);
        // The same three questions where rows are claimed several times over, which is the state
        // the label form refuses and the list form is for.
        let overlapping = rows_of(&[Some(&[0, 1, 7]), Some(&[1, 2, 7]), None, Some(&[7])]);
        for (membership, layout) in [
            (&partitioned, ServingLayout::RowMajorLabel),
            (&partitioned, ServingLayout::RowMajorList),
            (&overlapping, ServingLayout::RowMajorList),
        ] {
            let column = composed(membership, 10, layout).expect("composes");
            let transposed = column.transpose().expect("a column with no tail transposes");
            assert_eq!(transposed.len(), column.len());
            for ordinal in 0..transposed.len() as u32 {
                let projected = membership.get(ordinal).cloned().unwrap_or_default();
                let rows = &transposed[ordinal as usize];
                assert_eq!(
                    rows.to_vec(),
                    projected.to_vec(),
                    "transposed rows disagreed at ordinal {ordinal} under {layout:?}"
                );
                assert_eq!(
                    column.declared_size(ordinal),
                    rows.cardinality(),
                    "the declared size disagreed with the transposition at ordinal {ordinal}"
                );
            }
        }
    }

    /// **Over a row space wide enough to cross the transposition\'s blocks**, which is where a
    /// per-block offset table that did not reset, or a block boundary that dropped a row, would
    /// show — 2¹⁶ rows is one block, so the fixture spans several and gives each artifact rows in
    /// every one of them.
    #[test]
    fn a_transposition_crosses_its_own_block_boundary() {
        const ROWS: u32 = 5 << 16;
        const ORDINALS: u32 = 7;
        let sets: Vec<Vec<u32>> = (0..ORDINALS)
            .map(|ordinal| (0..ROWS).filter(|row| row % ORDINALS == ordinal).collect())
            .collect();
        let refs: Vec<Option<&[u32]>> = sets.iter().map(|s| Some(s.as_slice())).collect();
        let membership = rows_of(&refs);
        for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
            let column = composed(&membership, ROWS, layout).expect("partitions");
            let transposed = column.transpose().expect("no tail");
            for ordinal in 0..ORDINALS {
                assert_eq!(
                    transposed[ordinal as usize].to_vec(),
                    membership.get(ordinal).unwrap().to_vec(),
                    "block-crossing transposition disagreed at ordinal {ordinal} under {layout:?}"
                );
            }
        }
    }

    /// **A column with a live tail refuses to transpose**, rather than handing back a form that
    /// stops at the base: the rows a flush appended are labelled by the tail and by nothing in the
    /// packed bytes, so a form built from the base alone would be narrow — the direction a row
    /// form must never be wrong in.
    #[test]
    fn a_tailed_column_does_not_transpose() {
        let membership = rows_of(&[Some(&[0, 1]), Some(&[2])]);
        let base = composed(&membership, 4, ServingLayout::RowMajorLabel).expect("builds");
        assert!(base.transpose().is_some());
        let tailed = base.with_tail(TailLabels::new(4, vec![1, ROW_COLUMN_HOLE]));
        assert!(tailed.transpose().is_none());
    }

    /// The column and the row form agree about every artifact's size and every artifact's masked
    /// count — which is the property the whole route rests on, asserted here at the structure and
    /// again end to end in `tests/artifact_row_major.rs`.
    #[test]
    fn the_column_and_the_row_form_agree_artifact_for_artifact() {
        let sets: Vec<Vec<u32>> = (0..64u32)
            .map(|i| ((i * 7)..(i * 7 + 7)).collect())
            .collect();
        let refs: Vec<Option<&[u32]>> = sets.iter().map(|s| Some(s.as_slice())).collect();
        let membership = rows_of(&refs);
        let row_count = 64 * 7;
        for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
            let column = composed(&membership, row_count, layout).expect("partitions");
            let mask: Bitmap = (0..row_count).filter(|r| r % 3 == 0).collect();
            let histogram = column.histogram(&mask);
            for ordinal in 0..64u32 {
                let rows = membership.get(ordinal).unwrap();
                assert_eq!(
                    u64::from(histogram[ordinal as usize]),
                    mask.count_intersection(rows),
                    "masked count disagreed at ordinal {ordinal} under {layout:?}"
                );
                assert_eq!(
                    column.declared_size(ordinal),
                    rows.cardinality(),
                    "declared size disagreed at ordinal {ordinal} under {layout:?}"
                );
            }
        }
    }

    /// Every `(row, ordinal)` the column labels, ascending — the observable an amendment changes.
    fn every_pair(column: &RowColumn) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for row in 0..column.row_count() {
            column.for_each_label(row, |ordinal| out.push((row, ordinal)));
        }
        out.sort_unstable();
        out
    }

    /// What an amended column holds beside its pack: pairs, rows, `row_end` and the counts.
    #[derive(Debug, PartialEq)]
    struct Amendment {
        pairs: Vec<(u32, u32)>,
        rows: Vec<u32>,
        row_end: u32,
        declared: Vec<u32>,
    }

    fn amendment(column: &RowColumn) -> Amendment {
        let added = column.added.as_ref().expect("the column was amended");
        Amendment {
            pairs: added.pairs.clone(),
            rows: added.rows.iter().collect(),
            row_end: added.row_end,
            declared: column.declared.clone(),
        }
    }

    /// **Many small amendments equal one**, on both row-major forms. The writes append above the
    /// highest row amended so far, merge into rows below it, repeat a pair the pack carries and one
    /// an earlier write added, name an ordinal the pack never had and, on the list form, put a
    /// second label at rows an earlier write labelled. What is compared is the representation —
    /// pairs, rows, `row_end` and the declared counts — and the counts are then checked against a
    /// walk of the labels.
    #[test]
    fn many_small_amendments_equal_one_batch_on_both_forms() {
        for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
            let list = layout == ServingLayout::RowMajorList;
            // Ordinal 0 holds rows 0..10, 1 holds 20..30, 2 is a hole, 3 holds 40..50; on the list
            // form ordinal 1 also holds 45..50. Rows 10..20, 30..40 and 50..64 belong to nobody.
            let zero: Vec<u32> = (0..10).collect();
            let one: Vec<u32> = (20..30).chain(if list { 45..50 } else { 0..0 }).collect();
            let three: Vec<u32> = (40..50).collect();
            let membership = rows_of(&[
                Some(zero.as_slice()),
                Some(one.as_slice()),
                None,
                Some(three.as_slice()),
            ]);
            let base = composed(&membership, 64, layout).expect("composes");

            let mut writes: Vec<(Vec<(u32, u32)>, u32)> = vec![
                // A flush's rows, above everything held: an append.
                ((64..72).map(|row| (row, 1)).collect(), 72),
                // A growth into unclaimed base rows below the highest amended row: a merge.
                ((12..16).map(|row| (row, 3)).collect(), 72),
                // An append carrying a pair the pack holds and a pair the first write added.
                (
                    [(5, 0), (64, 1)]
                        .into_iter()
                        .chain((80..84).map(|row| (row, 0)))
                        .collect(),
                    90,
                ),
                // A merge, out of order and with a repeat, to an ordinal the pack never had.
                (vec![(35, 5), (33, 5), (34, 5), (33, 5)], 90),
            ];
            if list {
                // A second label at rows an earlier write labelled, and at rows the pack labels.
                writes.push((vec![(64, 2), (66, 2), (12, 0), (25, 3)], 90));
            }

            let mut stepwise = base.clone();
            let mut every: Vec<(u32, u32)> = Vec::new();
            for (batch, row_count) in &writes {
                assert!(
                    stepwise.amend(batch, *row_count),
                    "{layout:?}: no write here claims a row twice"
                );
                every.extend(batch);
            }
            every.reverse();
            let mut at_once = base.clone();
            assert!(at_once.amend(&every, 90));

            assert_eq!(amendment(&stepwise), amendment(&at_once), "{layout:?}");
            assert_eq!(every_pair(&stepwise), every_pair(&at_once), "{layout:?}");
            assert_eq!(stepwise.row_count(), 90, "{layout:?}");
            assert_eq!(stepwise.len(), 6, "{layout:?}: ordinal 5 widened the column");

            let mut counted = vec![0u32; stepwise.len()];
            for (_, ordinal) in every_pair(&stepwise) {
                counted[ordinal as usize] += 1;
            }
            assert_eq!(stepwise.declared, counted, "{layout:?}: the counts are the labels");

            let pack_only = every_pair(&base);
            let beyond_the_pack: Vec<(u32, u32)> = every_pair(&stepwise)
                .into_iter()
                .filter(|pair| !pack_only.contains(pair))
                .collect();
            assert_eq!(
                stepwise.added.as_ref().unwrap().pairs,
                beyond_the_pack,
                "{layout:?}: the amendment holds exactly what the pack does not"
            );
            assert_eq!(
                stepwise.as_bytes(),
                base.as_bytes(),
                "{layout:?}: the pack is untouched"
            );
        }
    }

    /// **A column composed over the base answers as one composed whole**, on both forms: the same
    /// pairs, counts and row count — and its pack stops at the base, every row above it being in
    /// the amendment. Ordinal 1 straddles the base so one membership is split between the two.
    #[test]
    fn a_column_composed_over_the_base_answers_as_one_composed_whole() {
        let membership = rows_of(&[
            Some(&[0, 1, 2]),
            Some(&[6, 7, 8, 9]),
            None,
            Some(&[12, 13, 15]),
        ]);
        for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
            let whole = composed(&membership, 16, layout).expect("partitions");
            let split =
                composed_over_base(&membership, 8, 16, layout).expect("partitions");
            assert_eq!(every_pair(&split), every_pair(&whole), "{layout:?}");
            assert_eq!(split.declared, whole.declared, "{layout:?}");
            assert_eq!(split.row_count(), 16, "{layout:?}");
            assert_eq!(
                split.base_rows(),
                8,
                "{layout:?}: the pack stops at the base"
            );
            assert_eq!(
                amendment(&split).pairs,
                vec![(8, 1), (9, 1), (12, 3), (13, 3), (15, 3)],
                "{layout:?}: every row above the base is in the amendment"
            );
            let mask: Bitmap = (0..16).filter(|r| r % 2 == 1).collect();
            assert_eq!(split.histogram(&mask), whole.histogram(&mask), "{layout:?}");
        }
        // Nothing above the base: no amendment at all, so the column can still transpose.
        let base_only =
            composed_over_base(&membership, 16, 16, ServingLayout::RowMajorList)
                .expect("builds");
        assert!(base_only.added.is_none());
        assert!(base_only.transpose().is_some());
    }

    /// **A rebase gives up exactly the span's labels and takes the new ones**, equal to a column
    /// composed over the rebased memberships: the rows in `8..16` are permuted as a merge permutes
    /// them, and rows below and above the span stay as they were, on both forms.
    #[test]
    fn a_rebase_relabels_the_span_and_nothing_else() {
        let before = rows_of(&[
            Some(&[0, 1, 8, 9, 20]),
            Some(&[4, 12, 13, 21]),
            None,
            Some(&[14, 15, 22]),
        ]);
        // The merge's permutation of rows 8..16: 8→13, 9→12, 12→8, 13→9, 14→15, 15→14.
        let after = rows_of(&[
            Some(&[0, 1, 13, 12, 20]),
            Some(&[4, 8, 9, 21]),
            None,
            Some(&[15, 14, 22]),
        ]);
        for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
            let mut column =
                composed_over_base(&before, 8, 24, layout).expect("partitions");
            let span: Vec<(u32, u32)> = vec![(13, 0), (12, 0), (8, 1), (9, 1), (15, 3), (14, 3)];
            assert!(column.rebase(8, 16, &span, 24), "{layout:?}");
            let expected = composed_over_base(&after, 8, 24, layout).expect("partitions");
            assert_eq!(every_pair(&column), every_pair(&expected), "{layout:?}");
            assert_eq!(column.declared, expected.declared, "{layout:?}");
            assert_eq!(amendment(&column), amendment(&expected), "{layout:?}");
            assert_eq!(
                column.row_count(),
                24,
                "{layout:?}: a merge preserves the row count"
            );
        }
    }

    /// **A growth into a row an earlier growth labelled is refused on the label form.** The first
    /// claim is in the amendment rather than in the pack and the rule is the same; the refusal
    /// leaves the column as it was. A pair already carried is dropped rather than refused, and the
    /// list form takes the second label.
    #[test]
    fn a_growth_into_a_row_already_amended_is_refused_on_the_label_form() {
        let membership = rows_of(&[Some(&[0, 1]), Some(&[4, 5])]);
        let mut label = composed(&membership, 8, ServingLayout::RowMajorLabel)
            .expect("partitions");
        assert!(label.amend(&[(2, 0), (9, 1)], 10));
        let before = amendment(&label);
        assert!(!label.amend(&[(9, 0)], 10), "row 9 already carries ordinal 1");
        assert!(
            !label.amend(&[(7, 0), (2, 1)], 10),
            "one double claim refuses the whole batch"
        );
        assert!(
            !label.amend(&[(7, 0), (7, 1)], 10),
            "and so does a double claim inside the batch"
        );
        assert_eq!(amendment(&label), before, "a refused amendment changes nothing");
        assert!(
            label.amend(&[(9, 1), (2, 0), (0, 0)], 10),
            "pairs already carried are dropped, not refused"
        );
        assert_eq!(amendment(&label), before);

        let mut list = composed(&membership, 8, ServingLayout::RowMajorList)
            .expect("always builds");
        assert!(list.amend(&[(2, 0), (9, 1)], 10));
        assert!(
            list.amend(&[(9, 0), (2, 1)], 10),
            "the list form carries a row several artifacts claim"
        );
        assert_eq!(list.declared, vec![4, 4]);
        assert_eq!(
            every_pair(&list)[2..],
            [(2, 0), (2, 1), (4, 1), (5, 1), (9, 0), (9, 1)]
        );
    }
}
