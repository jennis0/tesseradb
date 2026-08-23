//! **The derived artifact structures, written by whoever produces a prefix** — the tile-index
//! extent column, the row-major column, the containment partition, and the pick that says which of
//! them a level gets.
//!
//! # Why this is here and not in the engine
//!
//! It was in the engine, and only the fold could reach it. `tessera build` writes a prefix too, and
//! a build that cannot write these files leaves every one of them to be derived on the first
//! request that names the level — which is the 111-second truncated response
//! `probes/2026-08-22-artifact-serving-e2e/README.md` §8 reproduces, and the 10–11× the same
//! campaign measures between a fresh bundle and its first fold.
//!
//! The rule this module follows is `tessera-build`'s own manifest comment about the filter
//! artefact: **the format's owner owns both halves, so the writer and the reader cannot drift.**
//! [`crate::membership`] owns the bytes of all three structures — [`pack_tile_index`],
//! [`pack_label_column`]/[`pack_list_column`] and [`pack_containment`] — and this owns the one pass
//! that produces their inputs and the one naming rule that files them. The engine's `TileIndex`,
//! `RowColumn` and `ContainmentPartition` are the *reading* halves and stay where they are, each
//! now composed by calling in here rather than by a second walk of its own.
//!
//! **Beside the formats rather than inside them.** This is the one part of the prefix's artifact
//! machinery that reads bitmaps, and it lives in its own file so [`crate::membership`] can keep
//! saying what is true of it — that the extent format knows nothing about the payload. The
//! boundary this module keeps is the other one: it never sees an `ArtifactRecord`, because every
//! entry point takes a walk the caller closes over its own store with.
//!
//! # What a caller supplies, and why it is a callback
//!
//! A level's memberships live in `tessera-lifecycle`, which this crate does not depend on and must
//! not: the Roaring form is that crate's and the addressing is [`crate::membership`]'s. So every
//! entry point here takes a [`LevelWalk`] — *call me with each ordinal and its projected rows* —
//! which the caller closes over its own store with. Nothing in this file knows what an
//! `ArtifactRecord` is.
//!
//! **A walk is called more than once** and has to be re-runnable: a list column is an offset table
//! sized by one pass and filled by a second, and the shape observation walks separately from the
//! projection. That is why it is a `Fn` and not an iterator.
//!
//! # Two costs this deliberately does not fold together
//!
//! Observing a level's shape and projecting its extents are the same `project_base` per artifact,
//! and a caller that wants both pays it twice. They are separate because they are wanted at
//! different times — the shape decides the layout *before* anything is written, and only the
//! layout says which of the two files is owed — and because the observation holds one membership at
//! a time where a fused pass would hold the level's.

use std::collections::HashMap;
use std::path::Path;

use croaring::Bitmap;

use tessera_types::layer::{LayerDeclaration, MembershipSource, ServingLayout};

use crate::manifest::{ContainmentExtent, RowColumnExtent, TileIndexExtent};
use crate::membership::{
    pack_containment, pack_label_column, pack_list_column, pack_tile_index, ROW_COLUMN_HOLE,
    TILE_INDEX_EMPTY, TILE_INDEX_HOLE,
};

/// One level's artifacts, visited in ascending ordinal with their **base** row projections.
///
/// A hole yields no visit at all, exactly as a level's own iteration does; an artifact whose
/// membership projects to nothing yields an empty bitmap, which is a different fact and is told
/// apart everywhere below.
pub type LevelWalk<'a> = &'a dyn Fn(&mut dyn FnMut(u32, &Bitmap));

/// One term's postings, handed to a visitor — the shape [`SignatureIndex::build`] walks the
/// postings through. See [`PostingSlice`] for why it is a callback and not a return.
pub type PostingWalk<'a> = &'a dyn Fn(u32, &mut dyn FnMut(PostingSlice<'_>)) -> std::io::Result<()>;

/// One level's generating sets, visited in ascending ordinal — rank order within each artifact.
///
/// Separate from [`LevelWalk`] because it is a different projection of the same records: a
/// membership is what a viewport asks about, and a generating set is what containment does.
pub type ContentWalk<'a> = &'a dyn Fn(&mut dyn FnMut(u32, &[&Bitmap]));

// ---------------------------------------------------------------------------------------------
// The node ladder
//
// Moved here from the engine's `tile_index` because the *fraction of a level that no node holds*
// is now what the layout pick reads, and the pick has to be computable at a build that never
// constructs the hierarchy. One definition, two readers.
// ---------------------------------------------------------------------------------------------

/// The finest row range a node addresses: **ten bits, a thousand rows.**
///
/// ⊘ **The probe's constant, measured at no other value** (`design/artifact-serving-at-scale.md`
/// §4.4's closing note, carried into `2026-08-21-artifact-layout-selection.md` §9's constraint 6).
/// The floor trades settling granularity against the index's own size: nodes are bounded at
/// `rows / 2¹⁰`, which is the 4.2 MB §3 measures at 10⁸ rows and the 25.8 MB at 10⁹.
pub const FINEST_SHIFT: u32 = 10;

/// Bits per level: **four, a sixteen-way fan-out.**
///
/// ⊘ The probe's other constant, and measured at no other value either. Morton over two dimensions
/// makes a quadtree level two bits, so four bits is *two* quad levels per index level — half the
/// depth at the cost of a coarser alignment for the settle test.
pub const LEVEL_STEP: u32 = 4;

/// The index's levels for a row space, **coarse to fine**.
///
/// The first entry is the largest shift with `row_count >> shift > 0`, so there are at most
/// `2^LEVEL_STEP` roots however large the corpus is; the last is [`FINEST_SHIFT`].
pub fn tile_index_shifts(row_count: u32) -> Vec<u32> {
    let mut shifts = Vec::new();
    let mut shift = 32;
    while shift > FINEST_SHIFT {
        shift -= LEVEL_STEP;
        if (row_count as u64) >> shift > 0 || shift <= FINEST_SHIFT {
            shifts.push(shift.max(FINEST_SHIFT));
        }
    }
    if shifts.is_empty() {
        shifts.push(FINEST_SHIFT);
    }
    shifts
}

/// **Whether an extent is too wide for every node there is** — the `everywhere` test, answered
/// from one shift.
///
/// The hierarchy places an artifact at the *finest* level whose block holds both ends of its span,
/// so an artifact is in `everywhere` exactly when the **coarsest** level fails that test: a finer
/// level has smaller blocks and cannot succeed where a coarser one did not. That collapses the
/// whole placement to one comparison per artifact, which is what makes the fraction computable in
/// a pass that never builds the tree.
pub fn is_everywhere(lo: u32, hi: u32, coarsest_shift: u32) -> bool {
    (lo >> coarsest_shift) != (hi >> coarsest_shift)
}

/// The shift the `everywhere` test is taken at, for a row space of `row_count` rows.
pub fn coarsest_shift(row_count: u32) -> u32 {
    tile_index_shifts(row_count)[0]
}

// ---------------------------------------------------------------------------------------------
// The shape a level is observed to have, and the pick that reads it
// ---------------------------------------------------------------------------------------------

/// ⊘ **Provisional: the fraction of a level's artifacts too wide for any index node at or above
/// which the level is served row-major.**
///
/// **This axis replaced blocks per artifact on 2026-08-23**, on the campaign's own evidence
/// (`docs/evidence/memos/2026-08-22-artifact-scale-campaign.md`, "The bracket re-run"). Four
/// controlled points with every other fixture statistic held exactly — 0.960 members per row, 32
/// distinct containment expressions, a 2.0 MB tile index — moved blocks per artifact through 6, 8,
/// 10 and 12 and the whole-map cell **fell**, 104.1 → 42.8 → 44.3 → 36.2 ms, with the worst cell
/// falling monotonically 122.3 → 81.7 → 67.4 → 68.1. The one quantity that tracked the cost was the
/// `everywhere` set, rising **1.6% → 2.3% → 2.9% → 3.6%** as it fell. A threshold in blocks per
/// artifact therefore flips *away from a layout that is improving*, which is what the old constant
/// of 10.0 did; blocks per artifact conflates container count with spread, and eight containers
/// clumped settle where eight corner-to-corner do not.
///
/// So this is **0.25**, and every part of that number is an argument rather than a measurement:
///
/// - It is seven times the top of the measured-improving band, so all four bracket points — and
///   the whole region between them — stay artifact-major. A threshold inside 1.6–3.6% would flip
///   in territory where the layout being flipped away from is measurably getting better.
/// - It is far below where the layers that *win* by flipping sit. Every artifact of a scattered
///   layer lands in `everywhere` at every size the campaign measured (§5, and the engine's
///   `tile_index` module doc says the same), so the two 10⁷ enumerated layers whose fold-time flip
///   bought **11× and 10.5×** are at or near 1.0 on this axis and flip on any threshold below it.
/// - Everything between 3.6% and that band is unmeasured, and the direction of the mistake decides
///   where in it to sit. Picking artifact-major where row-major would have been cheaper costs
///   latency on a route measured, built and correct at every size reached; picking row-major where
///   the level does not suit it costs a whole-map scan of `viewport ∩ M_auth` on exactly the
///   request an artifact-major level answers without scanning anything. A quarter is a *quarter of
///   the level* unplaceable — a level that scattered is not one the node walk is doing work for.
///
/// ⊘ **Provisional pending a sweep along this axis.** The campaign's bracket varied blocks per
/// artifact and read the fraction off it; nothing has yet varied the fraction through 0.1–0.9 and
/// measured the crossover, which is what would replace this argument with a number.
pub const ROW_MAJOR_EVERYWHERE_FRACTION: f64 = 0.25;

/// ⊘ **Provisional: the artifact count below which the pick stays artifact-major whatever the
/// spread.**
///
/// A thousand. Below it the whole-map cell is milliseconds on either route — the measured cells run
/// in single-digit milliseconds at 10³ artifacts — so a flip buys nothing measurable and costs a
/// column, a manifest entry and a per-session histogram. Above it the row-major scan's `O(visible
/// rows)` starts to be paid against a per-candidate cost that is climbing with the population.
///
/// It is a **tiebreak and not a bound**: a level under it that is *pinned* row-major is served
/// row-major, because a pin is an operator saying they know something the observations do not.
pub const ROW_MAJOR_MIN_ARTIFACTS: u64 = 1_000;

/// What one level looks like, as a build or a fold observes it — **after** any retirements the
/// caller has already executed, which is exactly the case the fold's re-evaluation exists for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelShape {
    /// How many live artifacts the level holds. Holes are not counted: a retired slot is not an
    /// artifact, and counting it would keep a level that has been emptied looking populous.
    pub artifacts: u64,
    /// **Reported, and no longer the trigger** (decision 0092's (c)). The mean number of Roaring
    /// containers a membership touches — the measured cost model is that bitmap operations cost
    /// O(containers touched) rather than O(cardinality), so this says how much *work* a membership
    /// is. What it does not say is how far that work is spread, which is what the walk pays for;
    /// see [`Self::everywhere_fraction`] and [`ROW_MAJOR_EVERYWHERE_FRACTION`].
    pub blocks_per_artifact: f64,
    /// **The trigger.** The fraction of live artifacts whose extent is too wide for every node of
    /// the tile index — the set the walk cannot place and returns on every request whatever the
    /// viewport. Zero for a level with no artifacts.
    pub everywhere_fraction: f64,
    /// Whether the memberships are disjoint — which decides the **label/list** split and is not a
    /// choice. Observed rather than declared: single-valuedness is a property of the data.
    pub partitions: bool,
}

impl LevelShape {
    /// The shape of a level with nothing in it — what a registration sees, since a level with no
    /// artifacts has no spread to observe.
    pub fn empty() -> Self {
        LevelShape {
            artifacts: 0,
            blocks_per_artifact: 0.0,
            everywhere_fraction: 0.0,
            partitions: true,
        }
    }
}

/// Observe one level's shape in one pass, holding **one membership at a time**.
///
/// Every figure comes from the same walk: the container count is the cost model's own number, the
/// extent is `minimum`/`maximum` over the projection, and the running union is what says whether
/// the memberships partition — dropped the moment an overlap is found, so a level that plainly
/// overlaps pays one intersection rather than a second copy of itself.
pub fn observe_shape(row_count: u32, each: LevelWalk<'_>) -> LevelShape {
    let shift = coarsest_shift(row_count);
    let mut artifacts = 0u64;
    let mut blocks = 0u64;
    let mut everywhere = 0u64;
    let mut partitions = true;
    let mut claimed = Bitmap::new();
    each(&mut |_, rows| {
        if rows.is_empty() {
            return;
        }
        artifacts += 1;
        blocks += rows.statistics().n_containers as u64;
        if let (Some(lo), Some(hi)) = (rows.minimum(), rows.maximum()) {
            if is_everywhere(lo, hi, shift) {
                everywhere += 1;
            }
        }
        if partitions {
            if claimed.intersect(rows) {
                partitions = false;
                claimed = Bitmap::new();
            } else {
                claimed.or_inplace(rows);
            }
        }
    });
    if artifacts == 0 {
        return LevelShape::empty();
    }
    LevelShape {
        artifacts,
        blocks_per_artifact: blocks as f64 / artifacts as f64,
        everywhere_fraction: everywhere as f64 / artifacts as f64,
        partitions,
    }
}

/// **The one rule.** `pin` is the layer's declared override, which is read and never re-derived.
///
/// `source` decides representability before anything else: a shape has no per-row source, and
/// inverting its ranges into a column would materialise the membership the ranges exist to avoid.
/// A row-major pin on such a layer is refused at parse
/// (`tessera_types::layer::DeclarationError::LayoutWithoutRowSource`), so reaching here with one is
/// a declaration that never validated — answered artifact-major rather than trusted.
///
/// **A pin never flips**, at the build or at any fold after it. That is the whole point of pinning:
/// a layer whose measured shape says one thing and whose operator knows another — a level about to
/// be grown, a benchmark, a bug being cornered. A nightly fold that silently reverted it would make
/// the key a suggestion.
///
/// **A pinned `column` on a level that does not partition is not corrected here.** Whether the
/// memberships are disjoint is checked where the column is built, and a double claim declines the
/// column and leaves the level artifact-major with a loud trace — so the fallback is one decision at
/// one place rather than a rule this function and the builder would each have to hold.
pub fn choose(declaration: &LayerDeclaration, shape: LevelShape) -> ServingLayout {
    // **A predicate's form follows from its membership and is never re-derived.** A shape's
    // members are row ranges recomputed per request; a single-valued attribute's members *are* the
    // column, one label per row. Neither has a second form to be chosen between, which is why
    // `LayerDeclaration::validate` refuses a pin on either and why the fold's re-evaluation reaches
    // here and leaves both alone.
    //
    // ⊘ A spatial layer that declares no `shape` has no ranges to serve and holds no artifacts, so
    // it falls through to the ordinary pick and lands artifact-major over an empty level.
    match declaration.membership {
        MembershipSource::Spatial if declaration.shape.is_some() => {
            return ServingLayout::SpatialRanges
        }
        MembershipSource::Attribute(_) => return ServingLayout::RowMajorLabel,
        MembershipSource::Spatial | MembershipSource::Enumerated => {}
    }
    if let Some(pinned) = declaration.layout {
        return pinned;
    }
    let row_major = shape.artifacts >= ROW_MAJOR_MIN_ARTIFACTS
        && shape.everywhere_fraction >= ROW_MAJOR_EVERYWHERE_FRACTION;
    if !row_major {
        return ServingLayout::ArtifactMajor;
    }
    // **The label/list split follows from the membership, never from the numbers.** A level whose
    // memberships are disjoint has exactly one label per row; one whose memberships overlap needs a
    // list, and pays the larger constant for it.
    if shape.partitions {
        ServingLayout::RowMajorLabel
    } else {
        ServingLayout::RowMajorList
    }
}

// ---------------------------------------------------------------------------------------------
// Byte production
// ---------------------------------------------------------------------------------------------

/// One level's extent column, framed — [`crate::membership::pack_tile_index`]'s input produced in
/// one walk.
///
/// A skipped ordinal is a **hole**, exactly as the row form's `resize_with(|| None)` makes it; a
/// live artifact whose membership projects to nothing is [`TILE_INDEX_EMPTY`], which is a different
/// fact and is told apart by every reader.
///
/// **`ordinals` is the level's own length, holes included, and it is not the walk's business.** A
/// level ending in a retired slot yields no visit for it, so a column sized by the last *visit*
/// would be short — and a reader compares an offered column's length against the level's before
/// adopting it, so a short one is silently dropped and derived again. The count is what the caller
/// knows and the walk does not.
pub fn project_tile_index(ordinals: u32, row_count: u32, each: LevelWalk<'_>) -> Vec<u8> {
    let mut spans: Vec<(u32, u32)> = vec![TILE_INDEX_HOLE; ordinals as usize];
    each(&mut |ordinal, rows| {
        let idx = ordinal as usize;
        if spans.len() <= idx {
            spans.resize(idx + 1, TILE_INDEX_HOLE);
        }
        spans[idx] = match (rows.minimum(), rows.maximum()) {
            (Some(lo), Some(hi)) => (lo, hi),
            _ => TILE_INDEX_EMPTY,
        };
    });
    pack_tile_index(row_count, &spans)
}

/// One level's row-major column, framed — or `None` where the level cannot take the form asked
/// for.
///
/// `None` on [`ServingLayout::RowMajorLabel`] means the memberships do **not** partition: a row was
/// claimed twice, which is the refusal a declaration could not make because single-valuedness is a
/// property of the data. The caller serves the level artifact-major and says so.
///
/// `None` on an artifact-major or spatial layout is the caller asking for a file no writer
/// produces.
pub fn project_row_column(
    ordinals: u32,
    row_count: u32,
    layout: ServingLayout,
    each: LevelWalk<'_>,
) -> Option<Vec<u8>> {
    match layout {
        ServingLayout::ArtifactMajor | ServingLayout::SpatialRanges => None,
        ServingLayout::RowMajorLabel => {
            let mut labels = vec![ROW_COLUMN_HOLE; row_count as usize];
            let mut overlapped = false;
            each(&mut |ordinal, rows| {
                for row in rows.iter() {
                    let at = row as usize;
                    // A row past the column is a member the projection placed above this view's
                    // base row space, which `project_base` does not produce. Guarded rather than
                    // trusted: the alternative is a panic on a shape nothing here controls.
                    if at >= labels.len() {
                        continue;
                    }
                    // **A double claim is not a partition**, and this is where the pin's second
                    // refusal fires — the one that could not be checked at parse.
                    if labels[at] != ROW_COLUMN_HOLE {
                        overlapped = true;
                        return;
                    }
                    labels[at] = ordinal;
                }
            });
            if overlapped {
                return None;
            }
            Some(pack_label_column(ordinals, &labels))
        }
        ServingLayout::RowMajorList => {
            // Pass one sizes each row's list; pass two fills it. Two passes rather than a
            // vector per row, which at 10⁹ rows is the allocator's whole address space in
            // headers alone.
            let mut at = vec![0u32; row_count as usize + 1];
            each(&mut |_, rows| {
                for row in rows.iter() {
                    if (row as usize) < row_count as usize {
                        at[row as usize + 1] += 1;
                    }
                }
            });
            for i in 1..at.len() {
                at[i] += at[i - 1];
            }
            let mut values = vec![0u32; *at.last().unwrap_or(&0) as usize];
            let mut cursor = at.clone();
            each(&mut |ordinal, rows| {
                for row in rows.iter() {
                    let at = row as usize;
                    if at >= row_count as usize {
                        continue;
                    }
                    values[cursor[at] as usize] = ordinal;
                    cursor[at] += 1;
                }
            });
            Some(pack_list_column(ordinals, &at, &values))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The containment partition
// ---------------------------------------------------------------------------------------------

/// One term's postings, as this module needs to read them.
///
/// **An adapter, not a second format.** `tessera-authz` owns `postings.arrow` and this crate does
/// not depend on it, so a caller hands each posting over in whichever of the two shapes it is
/// stored in and the walk below is written once. The decode that produces the shape stays where the
/// format lives.
pub enum PostingSlice<'a> {
    /// Ascending little-endian `u32` entity ids.
    Array(&'a [u8]),
    /// A Roaring bitmap of entity ids.
    Roaring(&'a Bitmap),
}

/// Every entity's term signature, for the entities one level's generating sets name.
///
/// **Built by one pass over the postings, not one probe per entity.** The inverse direction —
/// entity to terms — is not stored anywhere, so the only route is to walk each term's posting and
/// intersect it with the entities wanted. Done per entity that would be `O(terms)` each; done once
/// for the whole level it is `O(terms)` in total, which is why this is a level-scale object and
/// not a lookup.
pub struct SignatureIndex {
    /// `entity → its term ids, ascending`. Absent means *no term reaches this entity*, which is a
    /// real and fail-closed answer rather than a missing one.
    by_entity: HashMap<u32, Vec<u32>>,
}

impl SignatureIndex {
    /// Walk `term_count` postings, keeping only the entities in `wanted`.
    ///
    /// `posting` is called once per term and hands its posting to the visitor, or does not call the
    /// visitor at all where the file carries no record for that term — which is an ordinary answer.
    pub fn build(
        wanted: &Bitmap,
        term_count: u32,
        posting: PostingWalk<'_>,
    ) -> std::io::Result<Self> {
        let mut by_entity: HashMap<u32, Vec<u32>> = HashMap::default();
        if wanted.is_empty() {
            return Ok(SignatureIndex { by_entity });
        }
        for term in 0..term_count {
            posting(term, &mut |slice| match slice {
                // Ascending `u32` little-endian, so the walk is the decode. Tested against
                // `wanted` one at a time because the array form is the *small* terms, and
                // materialising a bitmap to intersect would cost more than the probe.
                PostingSlice::Array(bytes) => {
                    for chunk in bytes.chunks_exact(4) {
                        let entity = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                        if wanted.contains(entity) {
                            by_entity.entry(entity).or_default().push(term);
                        }
                    }
                }
                // The intersection first: a large term's posting may be the whole corpus, and
                // `and` is O(containers touched) against a generating-set union that is not.
                PostingSlice::Roaring(view) => {
                    for entity in view.and(wanted).iter() {
                        by_entity.entry(entity).or_default().push(term);
                    }
                }
            })?;
        }
        // Terms are walked in ascending ordinal, so every signature is already ascending and
        // duplicate-free — a term's posting names an entity at most once. Asserted rather than
        // sorted: re-sorting would hide a postings file that had stopped being a set.
        debug_assert!(by_entity
            .values()
            .all(|sig| sig.windows(2).all(|w| w[0] < w[1])));
        Ok(SignatureIndex { by_entity })
    }

    fn signature(&self, entity: u32) -> &[u32] {
        self.by_entity
            .get(&entity)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// The interning state, alive only while a level is being composed.
///
/// **It builds exactly the arrays the durable form holds**, so composing and opening a file are
/// the same structure reached two ways rather than two encodings that have to be kept in step.
#[derive(Default)]
struct Interner {
    /// Every expression's canonical encoding, concatenated: `nclauses, (len, terms…)*`.
    words: Vec<u32>,
    /// `at[e]..at[e + 1]` is expression `e`'s words, with the trailing sentinel — so the last
    /// expression needs no special case, the case a reader gets wrong.
    at: Vec<u32>,
    seen: HashMap<Vec<u32>, u32>,
}

impl Interner {
    fn intern(&mut self, canonical: Vec<u32>) -> u32 {
        if let Some(id) = self.seen.get(&canonical) {
            return *id;
        }
        if self.at.is_empty() {
            self.at.push(0);
        }
        let id = (self.at.len() - 1) as u32;
        self.words.extend_from_slice(&canonical);
        self.at.push(self.words.len() as u32);
        self.seen.insert(canonical, id);
        id
    }

    fn finish(self) -> (Vec<u32>, Vec<u32>) {
        let Interner { words, mut at, .. } = self;
        if at.is_empty() {
            at.push(0);
        }
        (words, at)
    }
}

/// One expression identifier per `(artifact, rank)`, with the width the durable form will use.
///
/// **`u16` with a checked promotion to `u32`, never a byte and never a truncation**
/// (`2026-08-21-artifact-layout-selection.md` §9, constraint 4). A truncated identifier does not
/// fail — it names a *different* expression, which is a containment verdict for another artifact's
/// generating set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct IdColumn {
    ids: Vec<u32>,
    wide: bool,
}

impl IdColumn {
    fn push(&mut self, id: u32) {
        self.wide |= u16::try_from(id).is_err();
        self.ids.push(id);
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    /// Bytes per identifier, for the durable form's header and for a residency line.
    fn width(&self) -> u8 {
        if self.wide {
            4
        } else {
            2
        }
    }
}

/// The partition under construction: the interned table and the per-`(artifact, rank)` column.
///
/// **Public because it is the only constructor**, and there must be exactly one: the interning key
/// and the stored expression are the same bytes, so a second assembly route is a way for them to
/// drift. `compose_containment` below drives it from a level's generating sets; the engine's own
/// clause-named test builder drives it from clauses directly, and both reach the same encoder.
#[derive(Default)]
pub struct ContainmentBuilder {
    interner: Interner,
    at: Vec<u32>,
    ids: IdColumn,
}

impl ContainmentBuilder {
    pub fn new() -> Self {
        ContainmentBuilder {
            at: vec![0],
            ..Default::default()
        }
    }

    /// One artifact's ranks, in order, each already canonically encoded by
    /// [`encode_expression`].
    ///
    /// A level is dense over its ordinals and the walk is in ordinal order, but a hole between two
    /// artifacts yields no record at all — so the offsets are carried forward to the ordinal being
    /// written rather than pushed once per record.
    pub fn push(&mut self, ordinal: u32, ranks: impl IntoIterator<Item = Vec<u32>>) {
        let idx = ordinal as usize;
        while self.at.len() <= idx {
            self.at.push(self.ids.len() as u32);
        }
        for expression in ranks {
            let id = self.interner.intern(expression);
            self.ids.push(id);
        }
        self.at.push(self.ids.len() as u32);
    }

    /// Frame what has been pushed. The caller reads it back through
    /// [`crate::membership::ContainmentPack`], which is what makes composing and opening one reader.
    pub fn finish(self) -> Vec<u8> {
        let ContainmentBuilder { interner, at, ids } = self;
        let (words, expr_at) = interner.finish();
        pack_containment(ids.width(), &at, &ids.ids, &expr_at, &words)
    }
}

/// Compose one `(layer, level)`'s containment partition, framed.
///
/// **Two walks of the level under one borrow of the caller's store**, which is the one-snapshot
/// rule (`2026-08-21-artifact-layout-selection.md` §9, constraint 1): the first collects the
/// entities the generating sets name so the signature pass knows what to look for, the second
/// composes. A growth landing between them would leave the expression describing a set the
/// membership no longer has, so both come from the same borrow — which is the caller's to hold, and
/// is why `contents` is a re-runnable walk.
pub fn compose_containment(contents: ContentWalk<'_>, signatures: &SignatureIndex) -> Vec<u8> {
    let mut builder = ContainmentBuilder::new();
    contents(&mut |ordinal, generating| {
        let ranks: Vec<Vec<u32>> = generating
            .iter()
            .map(|set| canonicalise(set, signatures))
            .collect();
        builder.push(ordinal, ranks);
    });
    builder.finish()
}

/// The entities one level's generating sets name, as [`SignatureIndex::build`] wants them.
pub fn generating_entities(contents: ContentWalk<'_>) -> Bitmap {
    let mut wanted = Bitmap::new();
    contents(&mut |_, generating| {
        for set in generating {
            wanted.or_inplace(set);
        }
    });
    wanted
}

/// One generating set's containment expression, canonically encoded.
///
/// **The conservative label join, written down**: a token satisfies the expression when, for every
/// member of the generating set, it holds at least one of that member's terms. So the encoding is
/// one clause per member — its whole signature, ascending — and the clauses are sorted and
/// deduplicated so two generating sets with the same signature multiset intern to the same id.
///
/// A member no term reaches contributes an **empty** clause, which nothing satisfies: an artifact
/// whose generating set includes an entity outside every term is contained by nobody, which is the
/// fail-closed direction on the one test I3 exists to make conservative.
fn canonicalise(generated_from: &Bitmap, signatures: &SignatureIndex) -> Vec<u32> {
    encode_expression(
        generated_from
            .iter()
            .map(|entity| signatures.signature(entity))
            .collect(),
    )
}

/// The canonical encoding of a clause list — sorted and deduplicated, then framed as
/// `nclauses, (len, terms…)*`.
///
/// One function so the interning key and the stored expression cannot be produced by two different
/// rules.
pub fn encode_expression(mut clauses: Vec<&[u32]>) -> Vec<u32> {
    clauses.sort_unstable();
    clauses.dedup();
    let mut words = Vec::with_capacity(1 + clauses.len() * 2);
    words.push(clauses.len() as u32);
    for clause in clauses {
        words.push(clause.len() as u32);
        words.extend_from_slice(clause);
    }
    words
}

// ---------------------------------------------------------------------------------------------
// Filing what was produced
//
// One naming rule, one durability sequence, one manifest-entry shape — shared so that a build and
// a fold cannot file the same structure two ways. Every failure is a dropped entry rather than a
// refusal: these files are derived, and a level without one composes it on first use, which is
// what every request did before they existed.
// ---------------------------------------------------------------------------------------------

/// One derived file waiting to be filed: its coordinates and its bytes.
pub struct Filed {
    pub view: String,
    pub layer: String,
    pub level: u32,
    pub level_version: u64,
    /// The form the bytes are in, where the kind has more than one. Read by
    /// [`file_row_columns`] for the extension and for the manifest entry's tag; ignored by the
    /// kinds that have a single form.
    pub layout: ServingLayout,
    pub bytes: Vec<u8>,
}

/// Create `prefix_dir/partitions/<partition>/<kind>`, or say why not.
fn derived_dir(prefix_dir: &Path, partition: &str, kind: &str) -> Option<std::path::PathBuf> {
    let dir = prefix_dir.join("partitions").join(partition).join(kind);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(source) => {
            tracing::warn!(path = %dir.display(), %source, "a derived-structure directory would not be created");
            None
        }
    }
}

/// The naming rule every derived file follows: a layer name and a view id are caller-shaped and
/// never reach a filename; the publication that introduced the file does.
fn derived_name(kind: &str, n: u64, index: usize, extension: &str) -> String {
    format!("{kind}-{n:06}-{index:03}.{extension}")
}

/// Write one kind's files and return the manifest entries that name them.
///
/// The directory entry itself has to be durable, or a crash leaves a manifest naming a file whose
/// name was never written — the rule every other publication follows. A directory that will not
/// fsync drops the whole kind.
fn file_all<T>(
    prefix_dir: &Path,
    partition: &str,
    kind: &str,
    n: u64,
    items: Vec<Filed>,
    extension_of: &dyn Fn(&Filed) -> &'static str,
    entry_of: &dyn Fn(&Filed, String) -> T,
) -> Vec<T> {
    if items.is_empty() {
        return Vec::new();
    }
    let Some(dir) = derived_dir(prefix_dir, partition, kind) else {
        return Vec::new();
    };
    let mut entries = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let name = derived_name(kind, n, index, extension_of(item));
        if let Err(error) = crate::write_and_fsync(&dir.join(&name), &item.bytes) {
            tracing::warn!(
                layer = %item.layer,
                level = item.level,
                view = %item.view,
                kind,
                %error,
                "a derived artifact structure would not be written; that level derives it on first use"
            );
            continue;
        }
        entries.push(entry_of(
            item,
            format!("partitions/{partition}/{kind}/{name}"),
        ));
    }
    if let Err(error) = crate::fsync_dir(&dir) {
        tracing::warn!(%error, kind, "a derived-structure directory would not be fsynced; its files are dropped");
        return Vec::new();
    }
    entries
}

/// File this prefix's tile-index extent columns, one per `(view, layer, level)`.
pub fn file_tile_indexes(
    prefix_dir: &Path,
    partition: &str,
    n: u64,
    items: Vec<Filed>,
) -> Vec<TileIndexExtent> {
    file_all(
        prefix_dir,
        partition,
        "tile-index",
        n,
        items,
        &|_| "tsti",
        &|item, path| TileIndexExtent {
            path,
            view: item.view.clone(),
            layer: item.layer.clone(),
            level: item.level,
            level_version: item.level_version,
        },
    )
}

/// File this prefix's row-major columns, one per `(view, layer, level)` whose layout has one.
///
/// The extension names the form, so a directory listing says which is which, and the manifest
/// entry's tag is the same [`Filed::layout`] the bytes were packed in — checked against the file's
/// own magic when a reader adopts it.
pub fn file_row_columns(
    prefix_dir: &Path,
    partition: &str,
    n: u64,
    items: Vec<Filed>,
) -> Vec<RowColumnExtent> {
    file_all(
        prefix_dir,
        partition,
        "row-column",
        n,
        items,
        &|item| match item.layout {
            ServingLayout::RowMajorList => "tsll",
            _ => "tslb",
        },
        &|item, path| RowColumnExtent {
            path,
            view: item.view.clone(),
            layer: item.layer.clone(),
            level: item.level,
            level_version: item.level_version,
            layout: item.layout,
        },
    )
}

/// File this prefix's containment partitions, one per `(layer, level)`.
///
/// A partition is not per view — it is a function of the level's records and the prefix's postings
/// — so `Filed::view` is ignored here and the entry carries none.
pub fn file_containment(
    prefix_dir: &Path,
    partition: &str,
    n: u64,
    items: Vec<Filed>,
) -> Vec<ContainmentExtent> {
    file_all(
        prefix_dir,
        partition,
        "containment",
        n,
        items,
        &|_| "tscp",
        &|item, path| ContainmentExtent {
            path,
            layer: item.layer.clone(),
            level: item.level,
            level_version: item.level_version,
        },
    )
}

#[cfg(test)]
mod derived_tests {
    use super::*;

    #[allow(clippy::type_complexity)]
    fn walk_of(sets: &[Vec<u32>]) -> impl Fn(&mut dyn FnMut(u32, &Bitmap)) + '_ {
        move |visit: &mut dyn FnMut(u32, &Bitmap)| {
            for (ordinal, set) in sets.iter().enumerate() {
                let bitmap: Bitmap = set.iter().copied().collect();
                visit(ordinal as u32, &bitmap);
            }
        }
    }

    /// **The level set is corpus-relative and the floor is not**, and the coarsest shift is the one
    /// the `everywhere` test is taken at.
    #[test]
    fn the_hierarchy_has_a_level_for_every_zoom_and_sixteen_roots_at_most() {
        assert_eq!(tile_index_shifts(0), vec![10]);
        assert_eq!(tile_index_shifts(500), vec![10]);
        assert_eq!(tile_index_shifts(100_000_000), vec![24, 20, 16, 12, 10]);
        assert_eq!(
            tile_index_shifts(1_000_000_000),
            vec![28, 24, 20, 16, 12, 10]
        );
        for rows in [1_000u32, 1_000_000, 100_000_000, u32::MAX] {
            let top = coarsest_shift(rows);
            assert!(
                (rows as u64) >> top < (1 << LEVEL_STEP),
                "{rows} rows would give more than a fan-out of roots"
            );
        }
    }

    /// The trigger's own arithmetic: a level whose artifacts are clumped has no `everywhere` set at
    /// all, and one whose artifacts span the map is entirely `everywhere`.
    #[test]
    fn the_everywhere_fraction_separates_a_clumped_level_from_a_spread_one() {
        let clumped: Vec<Vec<u32>> = (0..100u32).map(|i| vec![i * 10, i * 10 + 5]).collect();
        let shape = observe_shape(1_000_000, &walk_of(&clumped));
        assert_eq!(shape.artifacts, 100);
        assert_eq!(shape.everywhere_fraction, 0.0);

        let spread: Vec<Vec<u32>> = (0..100u32).map(|i| vec![i, 999_999 - i]).collect();
        let shape = observe_shape(1_000_000, &walk_of(&spread));
        assert_eq!(shape.artifacts, 100);
        assert_eq!(shape.everywhere_fraction, 1.0);
    }

    /// Holes and empty projections are not artifacts, and neither counts towards any figure.
    #[test]
    fn an_empty_projection_is_not_an_artifact() {
        let sets = vec![vec![1u32, 2], vec![], vec![7]];
        let shape = observe_shape(4_096, &walk_of(&sets));
        assert_eq!(shape.artifacts, 2);
        assert!(shape.partitions);
        assert_eq!(observe_shape(4_096, &walk_of(&[])), LevelShape::empty());
    }

    /// Disjointness is observed, not declared.
    #[test]
    fn overlapping_memberships_do_not_partition() {
        assert!(!observe_shape(4_096, &walk_of(&[vec![1, 2], vec![2, 3]])).partitions);
        assert!(observe_shape(4_096, &walk_of(&[vec![1, 2], vec![3, 4]])).partitions);
    }
}
