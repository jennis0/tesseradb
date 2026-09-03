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
//!
//! # What this replaces, and what it does not
//!
//! It replaces the **candidacy walk** and the **counting route** for the levels it covers, and the
//! per-artifact declared size the proportional criterion divides by. It does **not** replace the
//! generating sets containment is tested against, or the visible-row set derived content is computed
//! from: both are per-artifact row-space questions with no row-addressed form, and both go on being
//! answered from [`crate::artifacts::MembershipRows`] exactly as they were.
//!
//! ⊘ **So the residency half of §5.1 is not taken here.** The artifact-major row form is still held
//! for a row-major level, which is what the layout exists to avoid at 10⁹ rows (4 GB against 78.5).
//! Not holding it needs containment's projection-loss test and the computed properties to reach the
//! membership another way, and neither is designed; what this stage delivers is the mechanism, the
//! record, the files and the route — with every answer asserted identical to the artifact-major
//! one's, which is what a later change removing the row form would be checked against.
//!
//! **What has moved is which of the two is derived from the other.** Where the prefix holds this
//! level's column, the artifact-major form is now **transposed out of it** rather than projected a
//! second time from the level's memberships ([`RowColumn::transpose`], and
//! [`crate::artifacts::ArtifactRows::build_from_column`] is the caller): the column already holds
//! the membership, addressed by row, so reaching the other address is one sequential pass instead
//! of a decode and a permutation of every artifact's members. At rung 3's `mesh/descriptors` —
//! 30,217 artifacts over 1.66×10⁹ membership entries — that is **14.5 s at open against 24 s**,
//! the open as a whole 14.4 s against 23.9, and `/readyz` 24.8 s against 33.8
//! (`probes/2026-09-02-cold-start/`).
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
    pack_label_column, LabelColumnPack, ListColumnPack, ROW_COLUMN_HOLE,
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
    /// ⊘ **Bounded by what has accumulated since the last fold**, on [`TailLabels`]' own note and
    /// with the same reset: the fold rewrites the level's column whole, and a deployment that
    /// never folds accumulates one entry per `(row, artifact)` every write adds whatever this
    /// structure does.
    added: Option<Arc<Added>>,
}

/// Labels added to rows a column already addresses — see [`RowColumn::added`].
#[derive(Debug, Default)]
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
    ) -> Option<Self> {
        let ordinals = membership.len() as u32;
        let each = |visit: &mut dyn FnMut(u32, &Bitmap)| {
            for ordinal in 0..ordinals {
                if let Some(rows) = membership.get(ordinal) {
                    visit(ordinal, rows);
                }
            }
        };
        Self::assemble(ordinals, row_count, layout, &each)
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
    pub fn project<'a, I>(
        ordinals: u32,
        space: &RowSpace,
        layout: ServingLayout,
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
        Self::assemble(ordinals, space.base_rows(), layout, &each)
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
            tail: Some(tail),
            added: self.added.clone(),
        }
    }

    /// **This column with `pairs` added at the rows they name** — what a growth, a publication or a
    /// flush writes into a level whose column is already built.
    ///
    /// `pairs` is `(row, ordinal)` in any order. A pair the column already carries is dropped
    /// rather than counted twice. The cost is the pairs, and the pack is neither read whole,
    /// copied nor rewritten: it is shared, exactly as [`Self::with_tail`] shares it.
    ///
    /// **`None` where the form cannot express the result** — the label column, and a row that
    /// would come to carry two artifacts. That is the double claim the module doc forbids: the
    /// memberships have stopped partitioning, so the layout has stopped being true, and the
    /// caller's answer is the artifact-major route, which answers identically and says so. A list
    /// column has no such case.
    ///
    /// **Recomposing instead is what this replaces.** At rung 3's `mesh/descriptors` — 30,217
    /// artifacts over 1.66×10⁹ entries — composing the list column again cost ~100 s on the
    /// executor thread, where it blocks every ingest and every deny, for one entity joining three
    /// artifacts (`2026-09-03-post-flush-artifact-frames.md`).
    pub fn with_added(&self, pairs: &[(u32, u32)], row_count: u32) -> Option<Self> {
        let label_form = matches!(*self.pack, Pack::Label(_));
        let mut merged: Vec<(u32, u32)> = self
            .added
            .as_ref()
            .map(|added| added.pairs.clone())
            .unwrap_or_default();
        // The rows already spoken for, for the label form's refusal — O(containers) to ask,
        // against a scan of `merged` per pair.
        let mut claimed = self
            .added
            .as_ref()
            .map(|added| added.rows.clone())
            .unwrap_or_default();
        for (row, ordinal) in pairs {
            let mut carried = false;
            let mut occupied = false;
            self.for_each_label(*row, |held| {
                occupied = true;
                carried |= held == *ordinal;
            });
            if carried {
                continue;
            }
            if label_form && (occupied || claimed.contains(*row)) {
                return None;
            }
            claimed.add(*row);
            merged.push((*row, *ordinal));
        }
        merged.sort_unstable();
        merged.dedup();
        let mut rows = Bitmap::new();
        let mut row_end = row_count;
        for (row, _) in &merged {
            rows.add(*row);
            row_end = row_end.max(row.saturating_add(1));
        }
        rows.run_optimize();
        Some(self.over_added(Some(Arc::new(Added {
            pairs: merged,
            rows,
            row_end,
        }))))
    }

    /// This column over the same pack and tail, with `added` in place of whatever it held.
    ///
    /// **The one place [`Self::declared`] is re-folded**, because a per-artifact count is the
    /// base's plus the tail's plus the amendment's and no two of those are held together. The walk
    /// is over the tail and the amendment, never over the pack — that is what `base_declared`
    /// exists for.
    fn over_added(&self, added: Option<Arc<Added>>) -> Self {
        let mut declared = self.base_declared.as_ref().clone();
        // **A publication adds ordinals the pack never had**, and `declared` is what
        // [`Self::len`] answers from — so the column has to grow to cover them or every reader
        // sized by that length would index past its own count.
        if let Some(added) = &added {
            let highest = added
                .pairs
                .iter()
                .map(|(_, ordinal)| *ordinal as usize + 1)
                .max()
                .unwrap_or(0);
            if declared.len() < highest {
                declared.resize(highest, 0);
            }
        }
        if let Some(tail) = &self.tail {
            for label in &tail.labels {
                if *label != ROW_COLUMN_HOLE {
                    if let Some(count) = declared.get_mut(*label as usize) {
                        *count += 1;
                    }
                }
            }
        }
        if let Some(added) = &added {
            for (_, ordinal) in &added.pairs {
                if let Some(count) = declared.get_mut(*ordinal as usize) {
                    *count += 1;
                }
            }
        }
        RowColumn {
            pack: Arc::clone(&self.pack),
            declared,
            base_declared: Arc::clone(&self.base_declared),
            tail: self.tail.clone(),
            added,
        }
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
        match &pack {
            Pack::Label(pack) => {
                for row in 0..pack.rows() as usize {
                    let label = pack.label(row);
                    if label != ROW_COLUMN_HOLE {
                        declared[label as usize] += 1;
                    }
                }
            }
            Pack::List(pack) => {
                for row in 0..pack.rows() as usize {
                    for ordinal in pack.list(row) {
                        declared[ordinal as usize] += 1;
                    }
                }
            }
        }
        RowColumn {
            pack: Arc::new(pack),
            base_declared: Arc::new(declared.clone()),
            declared,
            tail: None,
            added: None,
        }
    }

    /// The two builders, sharing one walk protocol: `each` calls `visit` once per live artifact with
    /// its projected rows, and may be called more than once.
    fn assemble(
        ordinals: u32,
        row_count: u32,
        layout: ServingLayout,
        each: LevelWalk<'_>,
    ) -> Option<Self> {
        let bytes =
            tessera_store::derived::project_row_column(ordinals, row_count, layout, each)?;
        Some(Self::of_bytes(bytes, layout))
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
            RowColumn::compose(&membership, 10, ServingLayout::RowMajorLabel).expect("partitions");

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
            RowColumn::compose(&membership, 4, ServingLayout::RowMajorList).expect("always builds");
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
        assert!(RowColumn::compose(&overlapping, 4, ServingLayout::RowMajorLabel).is_none());
        assert!(RowColumn::compose(&overlapping, 4, ServingLayout::RowMajorList).is_some());
        // And artifact-major has no column at all, in either builder.
        assert!(RowColumn::compose(&overlapping, 4, ServingLayout::ArtifactMajor).is_none());
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
            let composed = RowColumn::compose(&membership, 8, layout).expect("builds");
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
            let column = RowColumn::compose(membership, 10, layout).expect("composes");
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
            let column = RowColumn::compose(&membership, ROWS, layout).expect("partitions");
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
        let base = RowColumn::compose(&membership, 4, ServingLayout::RowMajorLabel).expect("builds");
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
            let column = RowColumn::compose(&membership, row_count, layout).expect("partitions");
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
}
