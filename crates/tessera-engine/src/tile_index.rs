//! The hierarchical row-range index: **which artifacts a viewport could possibly reach**, answered
//! without looking at the population.
//!
//! Rows are Morton rank, so a tile at any zoom is a contiguous row range and the client's tile
//! hierarchy *is* a hierarchy of row ranges. This holds, per node, the artifacts whose **whole
//! membership** lies inside it — `own` for those whose finest containing node is exactly this one,
//! `subtree` for the union over everything beneath — plus the per-artifact extents the nodes are
//! derived from and the [`everywhere`](TileIndex::everywhere) set of artifacts too wide for any
//! node at all.
//!
//! # Why this is a candidate generator and not a decision
//!
//! [`crate::compose::MaskedSet::intersects_set`] records that an early draft served an artifact
//! *"wherever the box intersected the viewport"*, which discloses the unmasked extent by panning: a
//! viewer sees a shape's edge in a region holding nothing they may see. What is refused there is
//! using unmasked geometry as **the answer**. This uses it two ways and neither is that
//! (`design/artifact-serving-at-scale.md` §4.1):
//!
//! - **as a superset filter** — an artifact in no node the viewport touches has no member there, so
//!   it cannot have a *visible* one. Skipping it withholds nothing.
//! - **as the collapse that makes one probe exact for two questions** — where `membership ⊆
//!   viewport`, `membership ∩ (viewport ∩ M_auth)` and `membership ∩ M_auth` are the same set, so
//!   the request-shaped probe answers the mask-shaped question exactly.
//!
//! **Every candidate still pays a masked probe.** Containment in a covered node is a fact about
//! *cost*, never an answer — the review's finding 1 was an earlier revision claiming otherwise, and
//! the artifact it would have served is one all of whose members are outside `M_auth`. Nothing here
//! returns a verdict; [`crate::artifacts::ArtifactView::verdict`] is still the only thing that
//! does, and it runs for every candidate this hands back.
//!
//! # The two halves, and which one is durable
//!
//! §3 sizes them apart: **80 MB of extents at 10⁷ artifacts against 4.2 MB of index.** The extents
//! are what scales with the population, so they are what the fold writes and what a reader maps
//! ([`tessera_store::membership::TileIndexPack`], the layout memo's constraint 13). The node
//! hierarchy is a pure function of them — an artifact's node is the finest whose block holds both
//! ends of its span — so it is folded up here, in the same pass that validates the column, rather
//! than written as a second thing that could disagree with the bytes beside it.

use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashMap;

use tessera_store::membership::{
    pack_tile_index, TileIndexPack, TILE_INDEX_EMPTY, TILE_INDEX_HOLE,
};

use crate::artifacts::MembershipRows;
use crate::compose::MaskedSet;

/// The finest row range a node addresses: **ten bits, a thousand rows.**
///
/// ⊘ **The probe's constant, measured at no other value** (`design/artifact-serving-at-scale.md`
/// §4.4's closing note, carried into `2026-08-21-artifact-layout-selection.md` §9's constraint 6).
/// It is kept rather than re-chosen, and what it is doing is worth stating so a later measurement
/// knows what it is arguing with. The floor trades settling granularity against the index's own
/// size: nodes are bounded at `rows / 2¹⁰`, which is the 4.2 MB §3 measures at 10⁸ rows and the
/// 25.8 MB at 10⁹. Below it the node count starts to dominate what the index costs to hold, and a
/// viewport narrower than a thousand rows has so few candidates that the per-candidate probe is
/// already the cheap route — there is nothing left for a finer node to save.
const FINEST_SHIFT: u32 = 10;

/// Bits per level: **four, a sixteen-way fan-out.**
///
/// ⊘ The probe's other constant, and measured at no other value either. Morton over two dimensions
/// makes a quadtree level two bits, so four bits is *two* quad levels per index level — half the
/// depth, hence half the nodes a narrow viewport's descent visits, at the cost of a coarser
/// alignment for the settle test. The extent beside the tree is what makes that trade affordable:
/// it settles an artifact the walk handed back on an alignment boundary
/// ([`TileIndex::inside`]), so widening the fan-out costs candidates rather than answers.
const LEVEL_STEP: u32 = 4;

/// **What is corpus-relative here and what is not.** §4.4's first bullet — *a hierarchy, not a
/// granularity* — is about a flat index at a fixed block size, which settles nothing at mid-zoom
/// because the viewport is then made of tiles smaller than the block. The *set* of levels answers
/// that and is derived from the corpus: shifts run from the coarsest one the row count actually
/// reaches down to [`FINEST_SHIFT`], so there is a level matching every zoom a viewer can be at.
/// What stays fixed is the floor and the step above, and neither is measured at another value.
///
/// Coarse to fine. The first entry is the largest shift with `row_count >> shift > 0`, so there
/// are at most `2^LEVEL_STEP` roots however large the corpus is.
fn shifts_for(row_count: u32) -> Vec<u32> {
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

/// One ordinal's row-space extent. **Three states and not two**, which is the layout memo's
/// constraint 3 and the sentinel rule the durable column encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extent {
    /// No artifact at this ordinal. A fold executed a deletion and the slot is held open because
    /// an ordinal is identity (write cycle §3.4, Rule F's artifact arm). **Never a candidate, on
    /// any route.**
    Hole,
    /// A live artifact whose membership projects to nothing in this view — every member is awaiting
    /// a fold. Absent from every viewport, and **still an artifact**: the identifier route resolves
    /// it exactly as before, and where the layer declares no criterion it serves with a zero count.
    Empty,
    /// `[min_row, max_row]`, inclusive.
    Span { lo: u32, hi: u32 },
}

/// One `(view, layer, level)`'s index. See the module doc.
#[derive(Clone)]
pub struct TileIndex {
    /// The durable half — mapped where a fold wrote it, a buffer where a publication built it.
    /// `Arc` because a level's index is shared by every request that reaches it.
    pack: Arc<TileIndexPack>,
    /// Coarse to fine. Each entry is that level's row-range shift.
    shifts: Vec<u32>,
    /// `own[level][block]` — artifacts whose finest containing node is this one.
    ///
    /// **Sparse, and that is not just a saving.** A dense vector at 10⁹ rows is ~10⁶ slots at the
    /// finest level alone, nearly all of them empty; holding only the occupied blocks is what lets
    /// the walk below descend into a child *because something is there* rather than descending
    /// into sixteen and asking each one.
    own: Vec<FxHashMap<u32, Bitmap>>,
    /// `subtree[level][block]` — every artifact contained anywhere beneath, this node included. A
    /// missing key means the whole subtree is empty, which is what makes the descent cheap.
    subtree: Vec<FxHashMap<u32, Bitmap>>,
    /// Artifacts too wide for any node — they straddle the root's children at every level.
    ///
    /// **First-class, not an implementation detail of the walk** (`2026-08-21-artifact-layout-selection.md`
    /// §9's constraint 7). Every artifact of a *scattered* layer lands here at every size measured
    /// (§5), so this set is what makes such a layer cost what it costs at whole-map zoom; folding
    /// it into the root's subtree would lose the distinction the cost model rests on. It is
    /// returned on every request whatever the viewport, and each of its members takes the full
    /// masked probe.
    everywhere: Bitmap,
}

impl std::fmt::Debug for TileIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileIndex")
            .field("ordinals", &self.pack.ordinals())
            .field("row_count", &self.pack.row_count())
            .field("levels", &self.shifts.len())
            .field("everywhere", &self.everywhere.cardinality())
            .finish()
    }
}

impl Default for TileIndex {
    fn default() -> Self {
        Self::from_pack(
            TileIndexPack::from_bytes(pack_tile_index(0, &[]))
                .expect("an empty column frames by construction"),
        )
    }
}

impl TileIndex {
    /// Derive one level's extents from its row form and fold the hierarchy over them.
    ///
    /// **Called inside the same closure that builds the row form**, at one store borrow and one
    /// level version — the one-snapshot rule (`2026-08-21-artifact-layout-selection.md` §9's first
    /// constraint). A growth landing between the two reads would leave a **stale-narrow** extent
    /// beside a membership that has since reached past it, and a narrow extent settles an artifact
    /// whose members are outside the viewport: the collapse the settled half rests on would no
    /// longer be true, and the probe would answer a different question from the one recorded.
    pub fn build(membership: &MembershipRows, row_count: u32) -> Self {
        let mut spans = Vec::with_capacity(membership.len());
        for ordinal in 0..membership.len() as u32 {
            spans.push(match membership.get(ordinal) {
                // A hole: the row form's `None`, which is a slot no record occupies.
                None => TILE_INDEX_HOLE,
                // A live artifact whose projection is empty. `minimum`/`maximum` are `None`
                // together or not at all, so one test settles it.
                Some(rows) => match (rows.minimum(), rows.maximum()) {
                    (Some(lo), Some(hi)) => (lo, hi),
                    _ => TILE_INDEX_EMPTY,
                },
            });
        }
        // Framed and read back through the same checks a mapped file takes, for the reason
        // `ContainmentPartition::packed` gives: the two routes are one reader, so a framing rule
        // can never hold for a file and not for the form a publication built.
        Self::from_pack(
            TileIndexPack::from_bytes(pack_tile_index(row_count, &spans))
                .expect("a column this crate just packed frames by construction"),
        )
    }

    /// The same column, projected straight from a level's records without building the row form
    /// first — what the fold writes.
    ///
    /// **Equal to [`Self::build`] over the row form of the same level**, by construction rather
    /// than by an argument: an extent is `minimum` and `maximum` over `RowSpace::project_base`'s
    /// output, which is precisely what [`MembershipRows`] stores. What is *not* the same is what is
    /// held while it runs — one membership at a time rather than the whole level's, which at ten
    /// million artifacts is the difference between eight bytes an artifact and the gigabytes §7.3
    /// prices a row form at. The fold runs this before it has flipped, so that difference is
    /// residency it would otherwise be holding twice.
    ///
    /// A skipped ordinal is a **hole**, exactly as the row form's `resize_with(|| None)` makes it.
    pub fn project<'a>(
        artifacts: impl Iterator<Item = (u32, &'a tessera_lifecycle::membership::ArtifactRecord)>,
        space: &tessera_store::permutation::RowSpace,
    ) -> Self {
        let mut spans: Vec<(u32, u32)> = Vec::new();
        for (ordinal, record) in artifacts {
            let idx = ordinal as usize;
            if spans.len() <= idx {
                spans.resize(idx + 1, TILE_INDEX_HOLE);
            }
            let rows = space.project_base(&record.members);
            spans[idx] = match (rows.minimum(), rows.maximum()) {
                (Some(lo), Some(hi)) => (lo, hi),
                _ => TILE_INDEX_EMPTY,
            };
        }
        Self::from_pack(
            TileIndexPack::from_bytes(pack_tile_index(space.base_rows(), &spans))
                .expect("a column this crate just packed frames by construction"),
        )
    }

    /// Open a fold-written extent column, mapped in place, and fold the hierarchy over it. A torn
    /// or foreign file **refuses** — see [`TileIndexPack`].
    pub fn open(path: &std::path::Path) -> tessera_store::Result<Self> {
        Ok(Self::from_pack(TileIndexPack::open(path)?))
    }

    /// The one pass over the column: place each artifact at the finest node whose block holds both
    /// ends of its span, then fold the levels together fine to coarse.
    fn from_pack(pack: TileIndexPack) -> Self {
        let shifts = shifts_for(pack.row_count());
        let mut own: Vec<FxHashMap<u32, Bitmap>> =
            (0..shifts.len()).map(|_| FxHashMap::default()).collect();
        let mut everywhere = Bitmap::new();

        for ordinal in 0..pack.ordinals() {
            let span = pack.span(ordinal as usize);
            if span == TILE_INDEX_HOLE || span == TILE_INDEX_EMPTY {
                continue;
            }
            let (lo, hi) = span;
            // Fine to coarse, taking the first level that fits — so an artifact sits as deep as its
            // alignment allows and is settled by the smallest viewport that can settle it.
            let placed = shifts
                .iter()
                .enumerate()
                .rev()
                .find(|(_, &s)| lo >> s == hi >> s);
            match placed {
                Some((level, &s)) => {
                    own[level].entry(lo >> s).or_default().add(ordinal);
                }
                // Too wide for the coarsest node there is: `everywhere`, and returned on every
                // request whatever the viewport.
                None => everywhere.add(ordinal),
            }
        }

        // A node's set is its own plus its children's, one pass because the children are contiguous
        // in the level below and the level below is already complete when this reaches it.
        let mut subtree = own.clone();
        for level in (1..shifts.len()).rev() {
            let step = shifts[level - 1] - shifts[level];
            let (upper, lower) = subtree.split_at_mut(level);
            for (block, child) in lower[0].iter() {
                upper[level - 1]
                    .entry(block >> step)
                    .or_default()
                    .or_inplace(child);
            }
        }
        for level in own.iter_mut().chain(subtree.iter_mut()) {
            for node in level.values_mut() {
                node.run_optimize();
            }
        }
        everywhere.run_optimize();

        TileIndex {
            pack: Arc::new(pack),
            shifts,
            own,
            subtree,
            everywhere,
        }
    }

    /// The durable bytes — what the fold writes into the prefix.
    pub fn as_bytes(&self) -> &[u8] {
        self.pack.as_bytes()
    }

    /// How many ordinals this index covers, holes included.
    pub fn len(&self) -> usize {
        self.pack.ordinals() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The row space this index was folded over.
    pub fn row_count(&self) -> u32 {
        self.pack.row_count()
    }

    /// How many artifacts are too wide for any node. **Reported in the build's tracing**, because
    /// it is the number that says a layer is scattered and will pay the full probe at every zoom.
    pub fn everywhere(&self) -> u64 {
        self.everywhere.cardinality()
    }

    /// One ordinal's extent, with the hole and the empty projection told apart.
    pub fn extent(&self, ordinal: u32) -> Extent {
        if ordinal as usize >= self.len() {
            return Extent::Hole;
        }
        match self.pack.span(ordinal as usize) {
            TILE_INDEX_HOLE => Extent::Hole,
            TILE_INDEX_EMPTY => Extent::Empty,
            (lo, hi) => Extent::Span { lo, hi },
        }
    }

    /// Whether this artifact's whole membership lies inside the viewport — the alignment-free
    /// settle test, and the one that rescues what the node walk handed back on a boundary.
    ///
    /// Node boundaries are powers of two, so an artifact of ten thousand rows at an arbitrary
    /// offset usually straddles one and is stored a level up, whose node the viewport must cover
    /// sixteen times as much of. `viewport ⊇ [min, max] ⟹ viewport ⊇ membership` at one
    /// `contains_range`. It is a **sufficient** condition and not a necessary one: an artifact with
    /// a hole the viewport misses is settled by it, correctly.
    ///
    /// `viewport_rows` is `|viewport|`, computed once per request. **An artifact spanning more rows
    /// than the viewport holds cannot be inside it**, and that subtraction is what keeps the test
    /// cheap for exactly the scattered artifacts it can never settle — their span is the whole map,
    /// so the walk would otherwise be the whole map to reach the answer *no*.
    ///
    /// Both sentinels answer `false`: a hole and an empty projection are in no viewport.
    pub fn inside(&self, ordinal: u32, viewport: &Bitmap, viewport_rows: u64) -> bool {
        let Extent::Span { lo, hi } = self.extent(ordinal) else {
            return false;
        };
        if (hi as u64 - lo as u64 + 1) > viewport_rows {
            return false;
        }
        viewport.contains_range(lo..=hi)
    }

    /// Walk the hierarchy top-down for one viewport: take a whole subtree where the viewport covers
    /// a node, descend only where it cuts one.
    ///
    /// **Allocation-free per node** (`2026-08-21-artifact-layout-selection.md` §9's third
    /// precondition): `range_cardinality` answers both questions from one call — zero is disjoint,
    /// full is covered, anything else is the viewport's edge — where building a `Bitmap` per node
    /// merely to ask put a `malloc` on every node of every walk and cost 5× at 10⁹ rows.
    ///
    /// Cost is the viewport's **perimeter** among the occupied nodes, not the population: a child
    /// with nothing anywhere beneath it has no `subtree` entry and is never visited at all.
    pub fn candidates(&self, viewport: &Bitmap) -> Candidates {
        // Ordinals too wide for any node are candidates unconditionally, whatever the viewport.
        let mut open: Vec<&Bitmap> = vec![&self.everywhere];
        let mut settled: Vec<&Bitmap> = Vec::new();
        let mut nodes_visited = 0u64;
        let row_count = self.pack.row_count() as u64;
        if row_count == 0 {
            return Candidates::of(settled, open, nodes_visited);
        }

        let top = self.shifts[0];
        let mut stack: Vec<(usize, u32)> = (0..=((row_count - 1) >> top) as u32)
            .filter(|block| self.subtree[0].contains_key(block))
            .map(|block| (0, block))
            .collect();
        while let Some((level, block)) = stack.pop() {
            nodes_visited += 1;
            let shift = self.shifts[level];
            let lo = (block as u64) << shift;
            if lo >= row_count {
                continue;
            }
            let hi = (lo + (1u64 << shift) - 1).min(row_count - 1);
            let width = hi - lo + 1;
            let met = viewport.range_cardinality(lo as u32..=hi as u32);
            if met == 0 {
                continue;
            }
            if met == width {
                if let Some(node) = self.subtree[level].get(&block) {
                    settled.push(node);
                }
                continue;
            }
            // Cut by the viewport's edge. Whatever is stored *at* this node straddles the children,
            // so it keeps the full masked test; the occupied children are walked.
            if let Some(node) = self.own[level].get(&block) {
                open.push(node);
            }
            let Some(&finer) = self.shifts.get(level + 1) else {
                // The finest level has nothing beneath it, and `subtree` there is `own`, which was
                // just taken.
                continue;
            };
            let step = shift - finer;
            for child in (block << step)..((block + 1) << step) {
                if self.subtree[level + 1].contains_key(&child) {
                    stack.push((level + 1, child));
                }
            }
        }
        Candidates::of(settled, open, nodes_visited)
    }
}

/// One request's viewport, with `viewport ∩ M_auth` composed **once** beside it.
///
/// **Composed here and nowhere else**, which is what makes the hoisting safe rather than merely
/// fast: the only route to `here` is [`MaskedSet::visible_rows`], so a candidacy question cannot
/// come to be answered against the pre-overlay projection by a caller that assembled the pair
/// itself. Its cost is the viewport's containers rather than the population's, and §7.1 measures
/// the difference as load-bearing — 3.3 s against 553 ms at 10⁷ artifacts, composing per artifact
/// instead of per request.
pub struct Viewport<'a> {
    rows: &'a Bitmap,
    cardinality: u64,
    here: Bitmap,
}

impl<'a> Viewport<'a> {
    pub fn compose(rows: &'a Bitmap, mask: &impl MaskedSet) -> Self {
        Viewport {
            cardinality: rows.cardinality(),
            here: mask.visible_rows(rows),
            rows,
        }
    }

    /// The viewport as a row-space set.
    pub fn rows(&self) -> &Bitmap {
        self.rows
    }

    /// `viewport ∩ M_auth`.
    pub fn here(&self) -> &Bitmap {
        &self.here
    }

    /// `|viewport|`, taken once — the extent test's cheap refusal.
    pub fn cardinality(&self) -> u64 {
        self.cardinality
    }
}

/// What one viewport's walk returned: every ordinal that could have a member in view, and which of
/// them the geometry already knows sit wholly inside it.
///
/// **Neither half is a verdict.** `settled` says one probe against `viewport ∩ M_auth` is exact for
/// the mask-shaped question too; the rest say a probe is still owed. Every ordinal here pays one.
pub struct Candidates {
    settled: Bitmap,
    /// `settled ∪ open`, which is what a caller iterates — ascending, which is the order the cut
    /// downstream of it is entitled to.
    all: Bitmap,
    nodes_visited: u64,
}

impl Candidates {
    fn of(settled: Vec<&Bitmap>, open: Vec<&Bitmap>, nodes_visited: u64) -> Self {
        let settled = Bitmap::fast_or(&settled);
        let mut all = Bitmap::fast_or(&open);
        all.or_inplace(&settled);
        Candidates {
            settled,
            all,
            nodes_visited,
        }
    }

    /// Every candidate ordinal, **ascending**.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.all.iter()
    }

    /// Whether the walk settled this ordinal — its membership lies inside a node the viewport
    /// covers entirely, so `membership ∩ viewport = membership`.
    pub fn is_settled(&self, ordinal: u32) -> bool {
        self.settled.contains(ordinal)
    }

    pub fn len(&self) -> u64 {
        self.all.cardinality()
    }

    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    pub fn settled_len(&self) -> u64 {
        self.settled.cardinality()
    }

    /// How many occupied nodes the walk popped. **Instrumentation, not a served fact** — it names
    /// no artifact and no principal, and it is what `tests/artifact_tile_index.rs` asserts is far
    /// below the population at a narrow viewport.
    pub fn nodes_visited(&self) -> u64 {
        self.nodes_visited
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row form built by hand — these cases are about the index, and projecting through a
    /// permutation would test the projection instead.
    fn rows_of(sets: &[Option<&[u32]>]) -> MembershipRows {
        MembershipRows::of_rows(
            sets.iter()
                .map(|set| set.map(|s| s.iter().copied().collect::<Bitmap>()))
                .collect(),
        )
    }

    fn viewport(range: std::ops::RangeInclusive<u32>) -> Bitmap {
        let mut b = Bitmap::new();
        b.add_range(*range.start()..=*range.end());
        b
    }

    /// The three extents are three different answers, and the two that produce no candidate are
    /// still not the same fact. Collapsing them is what makes a hole readable as an artifact.
    #[test]
    fn a_hole_and_an_empty_projection_are_told_apart() {
        let index = TileIndex::build(&rows_of(&[Some(&[5, 9]), None, Some(&[])]), 4_096);
        assert_eq!(index.extent(0), Extent::Span { lo: 5, hi: 9 });
        assert_eq!(index.extent(1), Extent::Hole);
        assert_eq!(index.extent(2), Extent::Empty);
        // Past the level, which a request can reach through a stale ordinal.
        assert_eq!(index.extent(99), Extent::Hole);

        // Neither is ever a candidate, at the widest viewport there is.
        let candidates = index.candidates(&viewport(0..=4_095));
        assert_eq!(candidates.iter().collect::<Vec<_>>(), vec![0]);
    }

    /// **The walk returns a superset of what the viewport can reach**, which is the whole of its
    /// licence: an ordinal it omits is one with no member in view at all. Checked exhaustively
    /// against the membership itself over a grid of viewports.
    #[test]
    fn nothing_with_a_member_in_view_is_ever_omitted() {
        let sets: Vec<Vec<u32>> = (0..400u32)
            .map(|i| {
                // A spread of shapes: compact runs, boundary-straddling runs, and a few that reach
                // across the whole space and so land in `everywhere`.
                match i % 4 {
                    0 => vec![i * 13, i * 13 + 1],
                    1 => vec![1_023 + i, 1_024 + i],
                    2 => vec![i, 8_000 + i],
                    _ => vec![i * 7],
                }
            })
            .collect();
        let refs: Vec<Option<&[u32]>> = sets.iter().map(|s| Some(s.as_slice())).collect();
        let membership = rows_of(&refs);
        let index = TileIndex::build(&membership, 16_384);
        assert!(
            index.everywhere() > 0,
            "the fixture must exercise the wide set"
        );

        for (lo, hi) in [
            (0u32, 16_383u32),
            (0, 1_023),
            (1_000, 1_100),
            (4_096, 8_191),
            (8_000, 8_010),
            (12_345, 12_345),
        ] {
            let view = viewport(lo..=hi);
            let candidates = index.candidates(&view);
            for ordinal in 0..membership.len() as u32 {
                let reachable = membership
                    .get(ordinal)
                    .is_some_and(|m| m.and_cardinality(&view) > 0);
                if reachable {
                    assert!(
                        candidates.iter().any(|o| o == ordinal),
                        "ordinal {ordinal} has a member in [{lo}, {hi}] and the walk dropped it"
                    );
                }
                // And a settled artifact really is inside the viewport — the collapse the probe
                // against `viewport ∩ M_auth` rests on.
                if candidates.is_settled(ordinal) {
                    let m = membership
                        .get(ordinal)
                        .expect("settled implies a membership");
                    assert_eq!(
                        m.and_cardinality(&view),
                        m.cardinality(),
                        "ordinal {ordinal} was settled by [{lo}, {hi}] without being inside it"
                    );
                }
            }
        }
    }

    /// The extent test settles what the node walk cannot: an artifact straddling a node boundary is
    /// promoted a level, and without this it is only settled by a viewport sixteen times larger.
    #[test]
    fn the_extent_settles_across_an_alignment_boundary() {
        // 1 023..=1 024 straddles the 1 024-row boundary, so the finest node cannot hold it.
        let index = TileIndex::build(&rows_of(&[Some(&[1_023, 1_024])]), 16_384);
        let view = viewport(1_000..=1_100);
        let candidates = index.candidates(&view);
        assert!(candidates.iter().any(|o| o == 0));
        assert!(
            !candidates.is_settled(0),
            "the node walk cannot settle an artifact that straddles a boundary"
        );
        assert!(
            index.inside(0, &view, view.cardinality()),
            "and the extent is what settles it"
        );
    }

    /// A span wider than the viewport is refused before `contains_range` walks it — the guard that
    /// keeps a scattered layer's *no* from costing the whole map.
    #[test]
    fn a_span_wider_than_the_viewport_is_refused_without_walking_it() {
        let index = TileIndex::build(&rows_of(&[Some(&[0, 16_000])]), 16_384);
        let view = viewport(0..=100);
        assert!(!index.inside(0, &view, view.cardinality()));
        // And the whole map does settle it, which is what says the guard is about size and not
        // about the artifact.
        let whole = viewport(0..=16_383);
        assert!(index.inside(0, &whole, whole.cardinality()));
    }

    /// **The level set is corpus-relative and the floor is not.** A small corpus builds one level;
    /// a large one builds a level for every zoom down to a thousand rows, with at most sixteen
    /// roots however large it is.
    #[test]
    fn the_hierarchy_has_a_level_for_every_zoom_and_sixteen_roots_at_most() {
        assert_eq!(shifts_for(0), vec![10]);
        assert_eq!(shifts_for(500), vec![10]);
        assert_eq!(shifts_for(100_000_000), vec![24, 20, 16, 12, 10]);
        assert_eq!(shifts_for(1_000_000_000), vec![28, 24, 20, 16, 12, 10]);
        for rows in [1_000u32, 1_000_000, 100_000_000, u32::MAX] {
            let top = shifts_for(rows)[0];
            assert!(
                (rows as u64) >> top < (1 << LEVEL_STEP),
                "{rows} rows would give more than a fan-out of roots"
            );
        }
    }

    /// The walk touches the viewport's perimeter among the **occupied** nodes, not the population.
    /// An order-of-magnitude assertion rather than a benchmark.
    #[test]
    fn a_narrow_viewport_visits_far_fewer_nodes_than_there_are_artifacts() {
        let sets: Vec<Vec<u32>> = (0..20_000u32).map(|i| vec![i * 50, i * 50 + 3]).collect();
        let refs: Vec<Option<&[u32]>> = sets.iter().map(|s| Some(s.as_slice())).collect();
        let index = TileIndex::build(&rows_of(&refs), 1_000_000);
        let narrow = index.candidates(&viewport(500_000..=500_999));
        assert!(
            narrow.nodes_visited() < 200,
            "a thousand-row viewport visited {} nodes over 20 000 artifacts",
            narrow.nodes_visited()
        );
        assert!(
            narrow.len() < 100,
            "and it returned {} candidates",
            narrow.len()
        );
    }

    /// **A composed index and a mapped one are the same structure**, so the fold's consolidation is
    /// a change of backing rather than a second encoder.
    #[test]
    fn an_index_answers_the_same_mapped_as_composed() {
        let composed = TileIndex::build(
            &rows_of(&[Some(&[5, 9]), None, Some(&[]), Some(&[1_023, 4_000])]),
            8_192,
        );
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.tsti");
        std::fs::write(&path, composed.as_bytes()).unwrap();
        let mapped = TileIndex::open(&path).unwrap();

        assert_eq!(mapped.len(), composed.len());
        assert_eq!(mapped.row_count(), composed.row_count());
        assert_eq!(mapped.everywhere(), composed.everywhere());
        for ordinal in 0..4u32 {
            assert_eq!(mapped.extent(ordinal), composed.extent(ordinal));
        }
        for (lo, hi) in [(0u32, 8_191u32), (0, 1_023), (4_000, 4_100)] {
            let view = viewport(lo..=hi);
            let (a, b) = (mapped.candidates(&view), composed.candidates(&view));
            assert_eq!(a.iter().collect::<Vec<_>>(), b.iter().collect::<Vec<_>>());
            assert_eq!(a.settled_len(), b.settled_len());
        }
    }
}
