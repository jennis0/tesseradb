//! **Where does a viewport's artifact pass actually spend its time, at ten million artifacts?**
//!
//! `artifact-delivery.md` Stage 8 owes one figure — *"at 10⁹ with ~10⁷ artifacts, a viewport serves
//! inside its budget"* — and nothing has ever measured the serving side of it. The fold's artifact
//! pass is measured (32.8 s on eight threads), residency is measured (3.6 GB on the realistic arm),
//! and the cut is measured (0.6 ms per ten thousand visible). The pass between them is not.
//!
//! This is that measurement, and it is deliberately a **breakdown** rather than a total: the
//! serving loop in `viewport.rs` is four separable pieces with different growth laws, and a single
//! wall-clock number cannot say which one to attack.
//!
//! # The loop being measured
//!
//! ```text
//! for ordinal in 0..rows.len() {
//!     if !rows.intersects(ordinal, &tile_rows, mask) { continue }   // candidacy
//!     ... verdict:  masked_count(ordinal, mask)                     // the number, and the criterion
//!                   satisfied_rank(ordinal, mask, ...)              // containment
//! }
//! cut(&lineage, &passing, budget, prune)                            // the cut
//! ```
//!
//! Every one of those is called here through its shipped entry point on a real
//! [`ArtifactRows`] built by `ArtifactRows::build` from real `ArtifactRecord`s through a real
//! [`RowSpace`]. **Nothing is modelled**: the probe that preceded this one measured a category
//! column it had put in the render table, which is not where an indexed column lives, and the
//! correction moved its headline figure by 5×. So the rule for this campaign is that a number
//! comes from the structure the engine ships or it does not go in the table.
//!
//! # The three things the breakdown is for
//!
//! 1. **Candidacy is paid by every artifact and the rest only by survivors.** At a narrow viewport
//!    almost nothing survives, so the loop is candidacy and nothing else — and candidacy is
//!    `O(population)` however narrow the viewport is, which is the shape an index removes.
//! 2. **The mask-dependent half and the mask-independent half grow differently.** `masked_count`
//!    and `satisfied_rank` are functions of `M_auth` and the layer, *not* of the viewport, so a
//!    request is not where they have to be paid. Knowing their share tells you what a per-session
//!    materialisation would actually buy.
//! 3. **The loop is sequential and the machine has twelve cores.** Whatever else is true, that is a
//!    constant factor sitting on the table, and it should be priced before anything structural is.
//!
//! # Arms
//!
//! | arm | what it is |
//! |---|---|
//! | `runs` | a few contiguous row runs per artifact — a spatial cluster after the Morton sort |
//! | `scattered` | uniform over the row space — an attribute predicate's carriers, and the pessimistic end |
//!
//! The residency campaign measured these two as one model (~90 B per container) and a real
//! clustering sits between them, closer to `runs`. The same bracketing applies to time, and for the
//! same reason: both quantities track **containers touched**.
//!
//! # Usage
//!
//! ```text
//! cargo run --release --bin artifact_serving_scale -- --rows 100000000
//! cargo run --release --bin artifact_serving_scale -- --rows 1000000000 --artifacts 10000000
//! ```

use std::sync::Arc;
use std::time::Instant;

use croaring::Bitmap;
use rayon::prelude::*;

use tessera_engine::artifacts::ArtifactRows;
use tessera_engine::compose::MaskedSet;
use tessera_lifecycle::membership::{ArtifactRecord, ContentSet};
use tessera_spatial::morton::{tiles_for_bbox, Bounds};
use tessera_store::permutation::{Permutation, RowSpace};
use tessera_store::row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
use tessera_types::{EntityId, RowId};

/// A deterministic 64-bit stream — the same population on every run, so two runs compare.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// The membership shapes the model actually produces.
///
/// **`annotation-representation.md` §2.0 names three membership *sources* — enumerated, spatial
/// predicate, attribute predicate — but the axis that decides every cost here is a different one:
/// row-space locality.** Rows are Morton rank, so an artifact is cheap exactly when what it
/// describes is somewhere rather than everywhere. The sources fall on both sides of that line and
/// the taxonomy does not line up with it, which is why the arms are named for the shape.
///
/// | arm | what has this shape | locality |
/// |---|---|---|
/// | `runs` | an HDBSCAN clustering, a point-and-radius blob | one row block — clusters **partition** the map |
/// | `regions` | an administrative boundary, a spatial predicate, a coarse level of a hierarchy | contiguous but large: many blocks, one node higher up the tree |
/// | `scattered` | an attribute predicate, a per-analyst selection, a term treated as an artifact | **none** — its members are everywhere the term is |
///
/// The third is the one that bounds the design, and it is not a corner case: `annotations.md` §8.3
/// is about exactly it (*"the scattered set, and why a category is not enough"*), and §8.7's
/// terms-as-artifacts is the same shape again.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    Runs,
    Regions,
    Scattered,
    Partition,
    Nested,
    Dispersed,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Runs => "runs",
            Arm::Regions => "regions",
            Arm::Scattered => "scattered",
            Arm::Partition => "partition",
            Arm::Nested => "nested",
            Arm::Dispersed => "dispersed",
        }
    }

    fn parse(name: &str) -> Option<Arm> {
        match name {
            "runs" => Some(Arm::Runs),
            "regions" => Some(Arm::Regions),
            "scattered" => Some(Arm::Scattered),
            "partition" => Some(Arm::Partition),
            "nested" => Some(Arm::Nested),
            "dispersed" => Some(Arm::Dispersed),
            _ => None,
        }
    }
}

/// The mask a request composes to.
///
/// **Three sets, not one**, because that is what [`EffectiveMask`] is: base minus the overlay's
/// denials plus the buffer's additions, and `count_intersection` pays all three. A probe using a
/// bare bitmap would measure a third of the arithmetic the serving path does. `minus` and `plus`
/// are small here exactly as they are in a running system — the deny lane is not a second corpus.
///
/// [`EffectiveMask`]: tessera_engine::compose::EffectiveMask
struct ComposedMask {
    base: Bitmap,
    minus: Bitmap,
    plus: Bitmap,
}

impl MaskedSet for ComposedMask {
    fn count_intersection(&self, set: &Bitmap) -> u64 {
        let base = self.base.and_cardinality(set);
        let minus = self.minus.and_cardinality(set);
        let plus = self.plus.and_cardinality(set);
        base - minus + plus
    }

    fn intersects_set(&self, set: &Bitmap) -> bool {
        if self.plus.intersect(set) {
            return true;
        }
        if !self.base.intersect(set) {
            return false;
        }
        let mut visible = self.base.and(set);
        visible.andnot_inplace(&self.minus);
        !visible.is_empty()
    }

    fn visible_rows(&self, set: &Bitmap) -> Bitmap {
        let mut visible = self.base.and(set);
        visible.andnot_inplace(&self.minus);
        visible.or_inplace(&self.plus.and(set));
        visible
    }
}

/// A mask admitting some number of **signature groups** — which is what a principal's visible set
/// actually is.
///
/// `M_auth` is the union of the posting lists of the terms a principal holds, and a term's carriers
/// are whole signature groups. So the visible set is contiguous in entity space and **scattered in
/// row space**, which is the space `EffectiveMask` holds it in and the space every intersection
/// below happens in. A striped or run-based mask would understate the container count of every
/// operation that touches it.
///
/// Three sets rather than one, because that is what `EffectiveMask` is: base minus the overlay's
/// denials plus the buffer's additions, and `count_intersection` pays all three.
fn mask(rows: u32, row_order: &[u32], bases: &[u32], groups: u32, rng: &mut Rng) -> ComposedMask {
    let mut base = Bitmap::new();
    let ceiling = bases[groups as usize];
    for (row, &entity) in row_order.iter().enumerate() {
        if entity < ceiling {
            base.add(row as u32);
        }
    }
    base.run_optimize();

    // The deny lane: a few thousand suppressed entities, which is the order a real overlay carries.
    // **Drawn from the reserved stripe** — see [`reserved_for_deny`] for why, and for what that
    // means the figures below do not price.
    let mut draw = |want: usize, into: &mut Bitmap| {
        let mut taken = 0;
        let mut attempts = 0;
        while taken < want && attempts < want * 64 {
            attempts += 1;
            let row = rng.below(rows as u64) as u32;
            let entity = row_order[row as usize];
            if reserved_for_deny(entity, bases[signature_group(row) as usize]) {
                into.add(row);
                taken += 1;
            }
        }
    };
    let mut minus = Bitmap::new();
    draw(4096, &mut minus);
    minus.and_inplace(&base);

    let mut plus = Bitmap::new();
    draw(1024, &mut plus);
    plus.andnot_inplace(&base);

    ComposedMask { base, minus, plus }
}

/// A viewport, as the serving path sees one: a set of contiguous **row** ranges.
///
/// **Built by the shipped Morton decomposition**, not by a synthetic pattern. A viewport is a
/// rectangle on the map; `tiles_for_bbox` is what turns one into tiles; a tile is a contiguous
/// Morton code range; and rows are Morton rank, so a tile is a contiguous row range. Ranges that
/// abut merge, which is why a viewport covering whole quadrants is a handful of long runs and one
/// straddling a quadrant boundary is a fringe of short ones.
///
/// That distribution is the thing a synthetic viewport gets wrong, and it is not a detail: **it
/// decides how many index blocks a viewport covers *entirely***, which is what route E turns into
/// free answers. Two earlier revisions of this probe used evenly-sized ranges strided across the
/// space and then across a window; the first made candidacy rise as the viewport narrowed, and the
/// second made whole-block coverage impossible at every zoom but the widest. Neither is a viewport
/// anybody can pan to.
///
/// The rectangle is placed off-centre on purpose. A viewport aligned to a quadrant is the easy case
/// — one range, every block covered — and a probe that only measured that would be measuring its
/// own fixture.
fn viewport(rows: u32, fraction: f64, depth: u8) -> Bitmap {
    let extent = Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    };
    let mut tiles = Bitmap::new();
    if fraction >= 1.0 {
        tiles.add_range(0..rows);
        tiles.run_optimize();
        return tiles;
    }
    let side = fraction.sqrt();
    // Off a quadrant boundary by an irrational-ish offset, so the decomposition is a real one.
    let x0 = (0.5 - side / 2.0 + 0.031).clamp(0.0, 1.0 - side);
    let y0 = (0.5 - side / 2.0 + 0.017).clamp(0.0, 1.0 - side);
    for tile in tiles_for_bbox([x0, y0, x0 + side, y0 + side], depth, &extent) {
        let (lo, hi) = tile.code_range();
        // Rows are Morton rank and the fixture's points are uniform, so a code range is the
        // proportional row range. A real corpus is not uniform; what that changes is which rows a
        // given rectangle holds, not that a tile is contiguous in row space.
        let scale = |code: u64| ((code * rows as u64) >> 32) as u32;
        let (a, b) = (scale(lo), scale(hi).min(rows));
        if b > a {
            tiles.add_range(a..b);
        }
    }
    tiles.run_optimize();
    tiles
}

/// How wide a membership is spread, which is the only axis that decides its cost.
#[derive(Clone, Copy)]
struct Shape {
    members: u32,
    /// `runs` arm: how many pieces the membership is broken into inside its own stretch.
    runs: u32,
    /// `dispersed` arm: how many distinct row blocks it occupies.
    blocks: u32,
}

/// One artifact's membership, **in row space** — and generating it there is the correction that
/// makes this probe measure the system it claims to.
///
/// A first revision generated contiguous runs in *entity* space and let `project_base` permute them
/// into row space, which scattered every one of them: an artifact of a hundred members landed in
/// sixty-odd separate 65 536-row blocks, and both arms collapsed onto the pessimistic one. That is
/// backwards. Entity ids are allocated in **(signature, Morton) order** and rows are Morton rank,
/// so a spatially coherent cluster is contiguous *in row space* and it is the entity form that is
/// scattered — which is exactly what the residency campaign measured (11.8× the runs, hence ~12×
/// the memory) and why it generated its populations in row space too.
///
/// So: choose rows, then map back through the permutation to get the entity set the record carries.
///
/// | arm | placement |
/// |---|---|
/// | `runs` | artifact *i* sits at its own stretch of the space, in `runs` pieces — a clustering **partitions** the map, so the artifacts tile it rather than landing on top of one another |
/// | `scattered` | uniform over the whole space — an attribute predicate's carriers, which have no locality at all |
/// | `dispersed` | `blocks` separate row blocks — the bracket between the two, swept so the layout heuristic's threshold can be found rather than assumed |
fn membership_rows(
    arm: Arm,
    row_count: u32,
    artifact: usize,
    artifacts: usize,
    shape: Shape,
    rng: &mut Rng,
) -> Bitmap {
    let Shape {
        members,
        runs,
        blocks,
    } = shape;
    let mut bitmap = Bitmap::new();
    match arm {
        Arm::Dispersed => {
            // **The bracket between `runs` (1.0 blocks per artifact) and `scattered` (96.8).**
            // Nothing was ever measured between 1.6 and 10 blocks per artifact, which is exactly
            // where a layout heuristic's threshold has to sit — so this arm places a membership in
            // a controlled number of 65 536-row blocks rather than letting the shape decide.
            //
            // Blocks step one container at a time from the artifact's own stretch, so each piece
            // lands in a container of its own while the **extent stays local** — which is what
            // separates this arm from `scattered`. An earlier revision strided the pieces across the
            // whole row space, and every artifact then straddled the root at every block count: the
            // arm collapsed onto `scattered` and measured nothing but its own dispersal.
            let stride = (row_count as u64 / artifacts.max(1) as u64).max(1);
            let blocks = blocks.max(1);
            let per_block = (members / blocks).max(1);
            let span = (blocks as u64 * 65_536).min(row_count as u64) as u32;
            let base = ((artifact as u64 * stride) as u32).min(row_count.saturating_sub(span));
            for b in 0..blocks {
                // One container is 65 536 rows; step whole containers so each piece is its own.
                let start = base
                    .saturating_add(b * 65_536)
                    .min(row_count.saturating_sub(per_block.min(row_count)));
                bitmap.add_range(start..start.saturating_add(per_block).min(row_count));
            }
            bitmap.run_optimize();
        }
        Arm::Runs => {
            // The stretch this artifact owns, and a local window a few times wider so neighbours
            // interleave at their edges as real clusters do.
            let stride = (row_count as u64 / artifacts.max(1) as u64).max(1);
            let base = (artifact as u64 * stride) as u32;
            let window = (stride * 4).min(row_count as u64) as u32;
            let per_run = (members / runs.max(1)).max(1);
            for _ in 0..runs.max(1) {
                let offset = rng.below(window.max(1) as u64) as u32;
                let start = base
                    .saturating_add(offset)
                    .min(row_count - per_run.min(row_count));
                bitmap.add_range(start..start.saturating_add(per_run).min(row_count));
            }
            bitmap.run_optimize();
        }
        Arm::Regions => {
            // One contiguous stretch wide enough to span many index nodes — a boundary polygon's
            // rows, or a coarse level of a hierarchy.
            //
            // **Four times its own stride, so neighbours overlap a little rather than a lot.** A
            // first revision made the span `members × stride`, which at a hundred members is a
            // hundredfold redundancy — every region covering ninety-nine of its neighbours — and no
            // boundary set is like that. Overlap is the parameter this arm is really about: it
            // decides how many artifacts a viewport edge cuts, and therefore how many survive the
            // settle test to pay for a masked intersection.
            let stride = (row_count as u64 / artifacts.max(1) as u64).max(1);
            let span = (stride * 4).min(row_count as u64) as u32;
            let base = ((artifact as u64 * stride) as u32).min(row_count.saturating_sub(span));
            bitmap.add_range(base..base.saturating_add(span).min(row_count));
            bitmap.run_optimize();
        }
        Arm::Nested => {
            // Filled by the caller from `nested_ranges`; this arm's membership is not a function of
            // the artifact alone.
        }
        Arm::Scattered => {
            // Uniform over the whole space. **No amount of spatial indexing helps this shape** —
            // it touches every node of the tree, so it is never inside one and never outside one.
            for _ in 0..members {
                bitmap.add(rng.below(row_count as u64) as u32);
            }
        }
        Arm::Partition => {
            // Scattered like the arm above, and **disjoint**, which is the property that changes
            // what can be stored. A single-valued attribute predicate partitions the corpus: every
            // point carries exactly one value, so the layer's memberships cover the row space
            // without overlapping and one label per row says everything the bitmaps do.
            //
            // Every `artifacts`-th row, offset by this artifact's index — a deterministic
            // partition with no locality at all, which is the pessimistic placement for the
            // artifact-major route and makes no difference to the row-major one.
            let mut row = artifact as u32;
            while (row as u64) < row_count as u64 {
                bitmap.add(row);
                row = row.saturating_add(artifacts as u32);
                if artifacts == 0 {
                    break;
                }
            }
        }
    }
    bitmap
}

/// **A real nested hierarchy: the membership comes from the tree, not the tree from the ordinals.**
///
/// Every other arm gives an artifact a membership near its own ordinal and then lays a tree over the
/// ordinal space, which makes the two unrelated — so a viewport can drop the whole top of the tree
/// while leaving its leaves in view, and every lineage below becomes its own fallback. No hierarchy
/// can do that. A parent cluster **contains** its children, so a parent is in view whenever any
/// child is, and the root is in view always.
///
/// Here the root owns the whole row space and each node splits its range among its children, so a
/// node's membership is its subtree's extent — contiguous in row space, which is what makes a
/// hierarchy cheap to store despite every level covering the corpus: a node is one run whatever its
/// size, so the whole layer is `depth × (rows / 65 536)` containers rather than one per member.
///
/// The `members` and `runs` knobs do not apply to this arm: what a node holds is decided by where it
/// sits in the tree.
fn nested_ranges(artifacts: usize, row_count: u32) -> Vec<(u32, u32)> {
    let mut ranges = vec![(0u32, 0u32); artifacts];
    if artifacts == 0 {
        return ranges;
    }
    ranges[0] = (0, row_count);
    for node in 0..artifacts {
        let (lo, hi) = ranges[node];
        if hi <= lo {
            continue;
        }
        let kids: Vec<usize> = (1..=3)
            .map(|k| 3 * node + k)
            .filter(|&c| c < artifacts)
            .collect();
        if kids.is_empty() {
            continue;
        }
        // Split the parent's extent among its children, so the union of the children is the parent
        // exactly — which is the property the whole arm exists to have.
        let width = (hi - lo) as u64;
        for (i, &child) in kids.iter().enumerate() {
            let from = lo + (width * i as u64 / kids.len() as u64) as u32;
            let to = lo + (width * (i as u64 + 1) / kids.len() as u64) as u32;
            ranges[child] = (from, to);
        }
    }
    ranges
}

/// The entity set behind a row set — what the durable record actually carries.
///
/// `ArtifactRows::build` projects the record's entity-space membership forward through
/// `project_base`, so a probe that wants a particular *row* form has to hand it the entity form
/// that produces one. This is that inverse, and it is also why the entity form is the expensive
/// copy: contiguous rows are scattered entities.
fn to_entities(rows: &Bitmap, space: &RowSpace) -> Bitmap {
    let mut entities = Bitmap::new();
    for row in rows.iter() {
        if let Some(entity) = space.entity_of(RowId::new(row)) {
            entities.add(entity.raw() as u32);
        }
    }
    entities
}

/// A keyed permutation of the signature groups. Any odd multiplier is a bijection modulo a power of
/// two; this one exists so that an artifact's **generating** group is not its ordinal's group.
fn permuted_group(g: u32) -> u32 {
    (g.wrapping_mul(17).wrapping_add(5)) % SIGNATURE_GROUPS
}

/// One ranked content's **generating set**, as entity ids — and the decorrelation that makes the
/// masked candidacy test observable at all.
///
/// The earlier fixture drew every generating set from the artifact's own members, inside the group
/// `ordinal % 32`, while a principal's mask was the first `holds` groups and `ContainmentGroups`
/// returned `ordinal % 32` — one 32-way partition doing four jobs. Two things follow from that, and
/// both are fatal to what this probe is for:
///
/// - *contains G* implied *has a visible member*, because `G ⊆ members` and containment means every
///   member of `G` is visible. So a route that admitted a settled artifact on containment alone was
///   **indistinguishable from one that also probed the mask**, and the assertion that compares them
///   could not fail however the routes were written;
/// - the build-time containment partition was read off the ordinal rather than off the population,
///   so it could not disagree with `satisfied_rank` either.
///
/// Here the group is `permuted_group(hash(ordinal) + rank)` — independent of the ordinal's own
/// group and of where the membership sits — and the set is **not drawn from the membership**.
/// `annotations.md` §7.8's review trail already says a generating set need not be one (*"a
/// contrastive labeller's generating set includes the contrast material"*), and §8.2's boundary
/// artifacts have empty ones over memberships that may be entirely invisible. So an artifact can
/// now be *contained but invisible* and *visible but uncontained*, which is what gives
/// [`assert_same_answer`] something to catch.
///
/// **Entity space, not row space, and that is what §7.8 describes.** A per-term generating set is a
/// sample sharing a signature, and a signature group is contiguous in entity space — so drawing
/// entities directly from `[bases[g], bases[g + 1])` is the shape the design names, and its row
/// form is scattered exactly as a real one's would be.
fn generating_entities(ordinal: usize, rank: u32, spread: u32, size: u32, bases: &[u32]) -> Bitmap {
    let mut h = (ordinal as u64)
        .wrapping_mul(0xD6E8_FEB8_6659_FD93)
        .rotate_left(29)
        .wrapping_add(rank as u64);
    h ^= h >> 31;
    let mut out = Bitmap::new();
    let spread = spread.clamp(1, SIGNATURE_GROUPS);
    for j in 0..spread {
        let g =
            permuted_group(((h as u32).wrapping_add(rank).wrapping_add(j * 7)) % SIGNATURE_GROUPS);
        let (lo, hi) = (bases[g as usize], bases[g as usize + 1]);
        let span = (hi - lo).max(1);
        // The set is split across the groups it spans, so `size` is the whole content's sample.
        let take = (size / spread).max(1);
        for i in 0..take {
            let mut z = h
                .wrapping_add((j as u64) << 40)
                .wrapping_add((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            z ^= z >> 27;
            z = z.wrapping_mul(0x94D0_49BB_1331_11EB);
            let mut e = lo + (z % span as u64) as u32;
            // Never the deny lane's stripe — see [`reserved_for_deny`].
            if reserved_for_deny(e, lo) {
                e = if e + 1 < hi { e + 1 } else { e - 1 };
            }
            out.add(e);
        }
    }
    out
}

/// How many signature groups the fixture's entity space is divided into.
///
/// **Entity ids are allocated in signature-sorted order with the Morton code as the within-signature
/// tiebreak** ([decision 0073](../../../docs/decisions/0073-morton-tiebreak.md)), so entity space is
/// not a shuffle of row space — it is *G* contiguous blocks, each of which is itself in Morton
/// order. A random permutation would be the pessimal case and no corpus has one: it turns a
/// one-container row form into a hundred-container entity form, where the storage campaign measures
/// the real ratio at 11.8×.
///
/// Thirty-two is chosen to sit in that measured range: a row-contiguous artifact becomes at most 32
/// runs in entity space, and the ratio comes out near 12× for the hundred-member artifacts here.
const SIGNATURE_GROUPS: u32 = 32;

/// Which signature group a row's document belongs to.
///
/// Signature and map position are uncorrelated — what a document is *about* does not follow from
/// where it sits — so this is a hash of the row rather than a function of its neighbourhood.
fn signature_group(row: u32) -> u32 {
    let mut z = (row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z ^= z >> 29;
    (z % SIGNATURE_GROUPS as u64) as u32
}

/// One entity in every four is **never drawn into a generating set**, and the deny lane draws only
/// from those.
///
/// The overlay and the build-time containment expression interact: a suppression removes a member
/// of `G` from `M_auth` whatever the principal's terms say, so an expression composed at build
/// needs a correction the design carries as an explicit ⊘ (§4.2) and does not propose measuring
/// here. A fixture whose deny lane landed on generating sets would put that unmodelled correction
/// inside every containment figure and inside the assertion that checks it — so the two routes
/// would differ for a reason neither of them is about.
///
/// Reserving a stripe keeps the expression **exact**, which is what makes
/// [`assert_containment_partition`] a real check rather than a tolerance. It is a property of the
/// fixture and not of the design, and it is the reason no figure here prices the overlay
/// correction.
fn reserved_for_deny(entity: u32, base: u32) -> bool {
    (entity - base) % 4 == 3
}

/// A row space whose permutation is the one the build actually produces.
///
/// Row *r*'s entity is `base[g] + rank of r within group g`, where *g* is *r*'s signature group.
/// The consequence is the one that matters for every measurement below: **an artifact that is
/// contiguous in row space is 32 runs in entity space, and a principal's visible set is contiguous
/// in entity space and scattered in row space.** The two directions are not symmetric and a fixture
/// that shuffled would have neither.
///
/// Returns the group bases as well, one per group plus a terminator, because a principal's mask is
/// the union of whole groups and **the boundary has to be the group's own**. An earlier revision
/// took the ceiling as `holds × (rows / 32)`, which is a few hundred entities away from
/// `base[holds]` wherever the hash divided unevenly — so a handful of entities were visible whose
/// group the principal did not hold, and the build-time containment expression could not be exact
/// for them.
fn row_space(dir: &std::path::Path, rows: u32) -> (RowSpace, Vec<u32>, Vec<u32>) {
    // Pass one: how big is each group.
    let mut sizes = vec![0u32; SIGNATURE_GROUPS as usize];
    for row in 0..rows {
        sizes[signature_group(row) as usize] += 1;
    }
    let mut base = vec![0u32; SIGNATURE_GROUPS as usize + 1];
    let mut running = 0u32;
    for (g, size) in sizes.iter().enumerate() {
        base[g] = running;
        running += size;
    }
    base[SIGNATURE_GROUPS as usize] = running;

    // Pass two: the assignment itself.
    let mut cursor = base.clone();
    let mut row_order: Vec<u32> = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let g = signature_group(row) as usize;
        row_order.push(cursor[g]);
        cursor[g] += 1;
    }

    let perm_path = dir.join("permutation.bin");
    let entities: Vec<EntityId> = row_order.iter().map(|&e| EntityId::new(e as u64)).collect();
    tessera_store::write::write_permutation(&perm_path, &entities, rows as u64)
        .expect("permutation writes");
    drop(entities);
    let table_path = dir.join(ROW_ENTITY_FILE);
    write_row_entity(&table_path, &row_order).expect("row-entity table writes");
    let space = RowSpace::new(
        Arc::new(Permutation::load(&perm_path).expect("permutation loads")),
        rows,
    )
    .with_row_entity(Arc::new(
        RowToEntity::load(&table_path).expect("row-entity table loads"),
    ));
    (space, row_order, base)
}

/// One route's name, its once-per-token setup cost, and the closure that runs one request of it.
type Route<'a> = (&'a str, f64, Box<dyn Fn() -> Phases + 'a>);

/// The phases of one request, in microseconds.
#[derive(Default, Clone, Copy)]
struct Phases {
    candidacy: f64,
    count: f64,
    containment: f64,
    /// The frontier, resolved over the candidates this request produced.
    ///
    /// **Measured here rather than stitched in from `artifact_cut_cost`**, which is the same
    /// arithmetic over a synthetic passing set. A request's cut is a function of what its own
    /// verdict admitted, so the two stages belong on one fixture or the totals are an assembly
    /// rather than a measurement.
    cut: f64,
    candidates: usize,
    passing: usize,
}

impl Phases {
    fn total(&self) -> f64 {
        self.candidacy + self.count + self.containment + self.cut
    }
}

/// **Everything a request reads that a request did not produce** — the build-time structures and
/// the per-generation ones, gathered so a route's signature says what it *does* rather than what it
/// happens to need.
struct Held<'a> {
    rows: &'a ArtifactRows,
    index: &'a TileIndex,
    contains: &'a ContainmentGroups,
    lists: &'a ListColumn,
    lineage: &'a tessera_engine::cut::Lineage,
    row_count: u32,
    artifacts: usize,
}

/// The frontier over one request's candidates, at the budget a client can draw.
///
/// A thousand is the order `annotation-representation.md` §2.0.0 puts a request's artifact ceiling
/// at, and the order the cut probe uses.
fn frontier(lineage: &tessera_engine::cut::Lineage, candidates: &[u32]) -> (f64, usize) {
    let start = Instant::now();
    let served = tessera_engine::cut::cut(lineage, candidates, Some(BUDGET as u32), true);
    (start.elapsed().as_secs_f64() * 1e6, served.len())
}

/// A request's artifact ceiling — the order `annotation-representation.md` §2.0.0 puts it at, and
/// the order a client can draw.
const BUDGET: usize = 1_000;

/// The containment test's verdict, called through the shipped entry point.
fn contained(rows: &ArtifactRows, ordinal: u32, mask: &ComposedMask) -> bool {
    rows.satisfied_rank(ordinal, mask, true) != tessera_engine::artifacts::Containment::Unsatisfied
}

/// **Route A — the shipped loop.** Every ordinal tested for candidacy, then the survivors counted
/// and contained. `parallel` splits the same three passes across the rayon pool the engine already
/// uses elsewhere, which prices the constant factor before anything structural is proposed.
fn shipped(rows: &ArtifactRows, tiles: &Bitmap, mask: &ComposedMask, parallel: bool) -> Phases {
    let mut phases = Phases::default();

    let start = Instant::now();
    let candidates: Vec<u32> = if parallel {
        (0..rows.len() as u32)
            .into_par_iter()
            .filter(|&o| rows.intersects(o, tiles, mask))
            .collect()
    } else {
        (0..rows.len() as u32)
            .filter(|&o| rows.intersects(o, tiles, mask))
            .collect()
    };
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = if parallel {
        candidates
            .par_iter()
            .map(|&o| rows.masked_count(o, mask))
            .sum()
    } else {
        candidates.iter().map(|&o| rows.masked_count(o, mask)).sum()
    };
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    let start = Instant::now();
    phases.passing = if parallel {
        candidates
            .par_iter()
            .filter(|&&o| contained(rows, o, mask))
            .count()
    } else {
        candidates
            .iter()
            .filter(|&&o| contained(rows, o, mask))
            .count()
    };
    phases.containment = start.elapsed().as_secs_f64() * 1e6;

    phases
}

/// **Route B — the same loop, with the allocation moved behind a boolean.**
///
/// `ArtifactRows::intersects` materialises `membership ∩ viewport` and then asks the mask about it.
/// The materialisation is a heap allocation per artifact per request, paid whether or not the two
/// sets meet — and at a narrow viewport almost none of them do. `Bitmap::intersect` answers the
/// same question without allocating, so asking it first turns the common case into a container
/// walk and no malloc. **The answer is identical**: this is an early exit on a term of a
/// conjunction, not a different test, and no artifact's verdict moves.
fn early_exit(rows: &ArtifactRows, tiles: &Bitmap, mask: &ComposedMask, parallel: bool) -> Phases {
    let candidate = |o: u32| -> bool {
        let Some(m) = rows.get(o) else { return false };
        if !m.intersect(tiles) {
            return false;
        }
        let in_tiles = m.and(tiles);
        mask.intersects_set(&in_tiles)
    };

    let mut phases = Phases::default();
    let start = Instant::now();
    let candidates: Vec<u32> = if parallel {
        (0..rows.len() as u32)
            .into_par_iter()
            .filter(|&o| candidate(o))
            .collect()
    } else {
        (0..rows.len() as u32).filter(|&o| candidate(o)).collect()
    };
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = if parallel {
        candidates
            .par_iter()
            .map(|&o| rows.masked_count(o, mask))
            .sum()
    } else {
        candidates.iter().map(|&o| rows.masked_count(o, mask)).sum()
    };
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    let start = Instant::now();
    phases.passing = if parallel {
        candidates
            .par_iter()
            .filter(|&&o| contained(rows, o, mask))
            .count()
    } else {
        candidates
            .iter()
            .filter(|&&o| contained(rows, o, mask))
            .count()
    };
    phases.containment = start.elapsed().as_secs_f64() * 1e6;

    phases
}

/// **The build-time spatial index over artifacts: a quadtree of row ranges, addressed exactly as
/// the client's tiles are.**
///
/// Rows are Morton rank, so a tile at depth *d* is a contiguous row range and the tile hierarchy is
/// a hierarchy of row ranges. This holds, per node, the artifacts whose **whole membership** lies
/// inside it — `own` for those whose finest containing node is exactly this one, `subtree` for the
/// union over everything beneath.
///
/// **One granularity is not enough, and that is a measured claim rather than a design preference.**
/// A flat index at 65 536 rows answers a whole-map request in 1.6 ms and a 6.25% request in
/// 2.2 ms — no better than having no index — because a viewport at that zoom is made of tiles
/// smaller than the block, so no block is ever *fully* covered and nothing is ever settled. The
/// relationship between tile size and block size moves with the corpus, so a fixed block is right
/// at one scale and wrong at every other. A hierarchy has a level that matches every zoom.
///
/// # Why this is a candidate generator and not a decision
///
/// `MaskedSet::intersects_set` records that an early draft served an artifact *"wherever the box
/// intersected the viewport"*, which discloses the unmasked extent by panning: a viewer sees a
/// shape's edge in a region holding nothing they may see. What is refused there is using unmasked
/// geometry as **the answer**. This uses it two ways, and neither is that:
///
/// - **as a superset filter** — an artifact outside the viewport's blocks has no member in the
///   viewport at all, so it cannot have a *visible* one either. Skipping it withholds nothing.
/// - **as a containment fact** — an artifact wholly inside a fully covered node has
///   `membership ⊆ viewport`, so *"has a visible member here"* and *"has a visible member"* are the
///   same question, and the second is answered from inside `M_auth` alone.
///
/// In both directions the geometry decides only *which question to ask*. Every artifact served
/// still cleared the masked test, and [`assert_same_answer`] asserts the served sets are identical
/// ordinal for ordinal.
struct TileIndex {
    /// Coarse to fine. Each entry is that level's row-range shift.
    shifts: Vec<u32>,
    /// `own[level][block]` — artifacts whose finest containing node is this one.
    own: Vec<Vec<Bitmap>>,
    /// `subtree[level][block]` — every artifact contained anywhere beneath, this node included.
    subtree: Vec<Vec<Bitmap>>,
    /// Artifacts too wide for any node — they straddle the root's children at every level.
    everywhere: Bitmap,
    /// Per ordinal, the first and last row of its membership.
    ///
    /// **The node hierarchy alone settles too little, and this is why.** A node boundary is a power
    /// of two in row space, so an artifact of ten thousand rows sitting at an arbitrary offset
    /// usually straddles one — and is then stored at the level *above*, whose node the viewport must
    /// cover sixteen times as much of before anything is settled. That promotion is the quadtree's
    /// standard boundary problem and it costs the `regions` arm almost the whole benefit: measured,
    /// the node walk alone settles nothing at all at a 6.25% viewport.
    ///
    /// An extent makes the test alignment-free: `viewport ⊇ [min, max] ⟹ viewport ⊇ membership`, at
    /// one `contains_range` against a run-encoded bitmap and eight bytes an artifact. It is a
    /// **sufficient** condition and not a necessary one — an artifact with a hole the viewport
    /// misses is still settled by it, correctly — and it subsumes the node test rather than
    /// replacing it: the walk is what avoids looking at the population at all, and this is what
    /// rescues the ones the walk hands back.
    extent: Vec<(u32, u32)>,
}

/// The finest row range a node addresses. Ten bits is a thousand rows: finer than any viewport
/// worth distinguishing, and it bounds the node count at `rows / 1024`.
const FINEST_SHIFT: u32 = 10;
/// Four bits per level, i.e. a quadtree over a two-dimensional Morton space.
const LEVEL_STEP: u32 = 4;

impl TileIndex {
    fn build(rows: &ArtifactRows, row_count: u32) -> Self {
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
        let mut own: Vec<Vec<Bitmap>> = shifts
            .iter()
            .map(|&s| vec![Bitmap::new(); ((row_count as u64 >> s) + 1) as usize])
            .collect();
        let mut everywhere = Bitmap::new();
        let mut extent = vec![(u32::MAX, 0u32); rows.len()];

        for ordinal in 0..rows.len() as u32 {
            let Some(m) = rows.get(ordinal) else { continue };
            let (Some(lo), Some(hi)) = (m.minimum(), m.maximum()) else {
                continue;
            };
            extent[ordinal as usize] = (lo, hi);
            // The finest level whose block holds both ends — walking fine to coarse and taking the
            // first that fits.
            let mut placed = false;
            for (level, &s) in shifts.iter().enumerate().rev() {
                if lo >> s == hi >> s {
                    own[level][(lo >> s) as usize].add(ordinal);
                    placed = true;
                    break;
                }
            }
            if !placed {
                everywhere.add(ordinal);
            }
        }

        // `subtree` folds the levels together, fine to coarse: a node's set is its own plus its
        // children's, which is one pass because the children are contiguous in the level below.
        let mut subtree = own.clone();
        for level in (1..shifts.len()).rev() {
            let (coarse_shift, fine_shift) = (shifts[level - 1], shifts[level]);
            let step = coarse_shift - fine_shift;
            let (upper, lower) = subtree.split_at_mut(level);
            for (block, child) in lower[0].iter().enumerate() {
                if child.is_empty() {
                    continue;
                }
                upper[level - 1][block >> step].or_inplace(child);
            }
        }
        for level in own.iter_mut().chain(subtree.iter_mut()) {
            for b in level.iter_mut() {
                b.run_optimize();
            }
        }
        everywhere.run_optimize();
        TileIndex {
            shifts,
            own,
            subtree,
            everywhere,
            extent,
        }
    }

    /// The two halves a viewport splits its candidates into: the ones whose answer is already
    /// settled by containment, and the ones that still need the masked test.
    ///
    /// A top-down walk, taking whole subtrees where the viewport covers a node and descending only
    /// where it cuts one — so the cost is the viewport's **perimeter** in the hierarchy, not the
    /// population.
    fn settled_and_open(&self, tiles: &Bitmap, row_count: u32) -> (Bitmap, Bitmap) {
        let mut settled: Vec<&Bitmap> = Vec::new();
        let mut open: Vec<&Bitmap> = vec![&self.everywhere];
        let mut stack: Vec<(usize, u32)> = vec![(0, 0)];
        // The coarsest level's blocks, all of them — the walk's roots.
        let top = self.shifts[0];
        stack.clear();
        for block in 0..=((row_count as u64) >> top) as u32 {
            stack.push((0, block));
        }
        while let Some((level, block)) = stack.pop() {
            let shift = self.shifts[level];
            let lo = (block as u64) << shift;
            if lo > row_count as u64 {
                continue;
            }
            let hi = (lo + (1u64 << shift) - 1).min(row_count as u64 - 1);
            let (lo, hi) = (lo as u32, hi as u32);
            // **One range query, and no allocation.** A first revision built a `Bitmap` per node
            // just to ask whether the viewport met it, which put a `malloc` on every node of every
            // walk — invisible at 10⁸ rows and worth 5× at 10⁹, where the hierarchy is two levels
            // deeper and a narrow viewport descends all of it. `range_cardinality` answers both
            // questions from the same call: zero is disjoint, full is covered, anything else is the
            // viewport's edge.
            let width = hi as u64 - lo as u64 + 1;
            let inside = tiles.range_cardinality(lo..=hi);
            if inside == 0 {
                continue;
            }
            if inside == width {
                if let Some(b) = self.subtree[level].get(block as usize) {
                    settled.push(b);
                }
                continue;
            }
            // Cut by the viewport's edge. Whatever is stored *at* this node straddles the children,
            // so it keeps the masked test; the children are walked.
            if let Some(b) = self.own[level].get(block as usize) {
                open.push(b);
            }
            if level + 1 == self.shifts.len() {
                if let Some(b) = self.subtree[level].get(block as usize) {
                    open.push(b);
                }
                continue;
            }
            let step = shift - self.shifts[level + 1];
            for child in (block << step)..((block + 1) << step) {
                stack.push((level + 1, child));
            }
        }
        let settled = Bitmap::fast_or(&settled);
        let mut open = Bitmap::fast_or(&open);
        open.andnot_inplace(&settled);
        (settled, open)
    }

    /// The candidate ordinals for a viewport — both halves, for the route that re-tests everything.
    fn candidates(&self, tiles: &Bitmap, row_count: u32) -> Bitmap {
        let (settled, open) = self.settled_and_open(tiles, row_count);
        settled | open
    }

    /// Whether this artifact's whole membership lies inside the viewport — the alignment-free
    /// settle test. See [`TileIndex::extent`].
    ///
    /// `viewport_rows` is how many rows the viewport holds, computed once per request. **An
    /// artifact spanning more rows than the viewport contains cannot be inside it**, and that is one
    /// subtraction against a `contains_range` that would otherwise walk every container between the
    /// two ends. It is the difference between a test that is cheap for compact artifacts and one
    /// that is dear for exactly the scattered artifacts it can never settle: their extent is the
    /// whole map, so the walk is the whole map, so the answer is *no* the expensive way.
    fn inside(&self, ordinal: u32, tiles: &Bitmap, viewport_rows: u64) -> bool {
        let (lo, hi) = self.extent[ordinal as usize];
        if lo > hi || (hi as u64 - lo as u64 + 1) > viewport_rows {
            return false;
        }
        tiles.contains_range(lo..=hi)
    }

    fn resident_bytes(&self) -> u64 {
        self.own
            .iter()
            .chain(self.subtree.iter())
            .flat_map(|level| level.iter())
            .map(|p| p.get_serialized_size_in_bytes::<croaring::Portable>() as u64)
            .sum()
    }
}

/// **Route C — candidates from the index, then the shipped verdict on each.**
fn indexed(
    rows: &ArtifactRows,
    index: &TileIndex,
    tiles: &Bitmap,
    mask: &ComposedMask,
    row_count: u32,
    parallel: bool,
) -> Phases {
    let mut phases = Phases::default();

    let start = Instant::now();
    let shortlist = index.candidates(tiles, row_count);
    let shortlist: Vec<u32> = shortlist.iter().collect();
    let candidates: Vec<u32> = if parallel {
        shortlist
            .par_iter()
            .filter(|&&o| rows.intersects(o, tiles, mask))
            .copied()
            .collect()
    } else {
        shortlist
            .iter()
            .filter(|&&o| rows.intersects(o, tiles, mask))
            .copied()
            .collect()
    };
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = if parallel {
        candidates
            .par_iter()
            .map(|&o| rows.masked_count(o, mask))
            .sum()
    } else {
        candidates.iter().map(|&o| rows.masked_count(o, mask)).sum()
    };
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    let start = Instant::now();
    phases.passing = if parallel {
        candidates
            .par_iter()
            .filter(|&&o| contained(rows, o, mask))
            .count()
    } else {
        candidates
            .iter()
            .filter(|&&o| contained(rows, o, mask))
            .count()
    };
    phases.containment = start.elapsed().as_secs_f64() * 1e6;

    phases
}

/// **The per-session materialisation: everything that is a function of `M_auth` and not of the
/// request, computed once when the session's mask is composed.**
///
/// Two quantities, and neither of them has a viewport in it:
///
/// - **the containment verdict**, which is `|G ∩ M| == |G|` — the mask and the generating set;
/// - **the masked count**, which is `|membership ∩ M|` — the mask and the membership.
///
/// So a request need not compute either. What it must still do per request is candidacy, because
/// that *is* the viewport question, and the overlay check, because a suppression takes effect at
/// the ack and this structure is older than the ack. **The overlay stays live on every route** —
/// that is what `ArtifactView::verdict` step 1 is for, and nothing here caches above it.
struct SessionVerdicts {
    /// Ordinals whose mask-dependent conjuncts pass, as a set — so a request intersects rather
    /// than iterates.
    passes: Bitmap,
    /// Per ordinal, `|membership ∩ M_auth|` — the number served, and the criterion's input.
    counts: Vec<u32>,
}

impl SessionVerdicts {
    fn build(rows: &ArtifactRows, mask: &ComposedMask, criterion: u64) -> Self {
        let n = rows.len();
        let verdicts: Vec<(u32, u32)> = (0..n as u32)
            .into_par_iter()
            .filter_map(|ordinal| {
                let count = rows.masked_count(ordinal, mask);
                if count < criterion {
                    return None;
                }
                if !contained(rows, ordinal, mask) {
                    return None;
                }
                Some((ordinal, count as u32))
            })
            .collect();
        let mut passes = Bitmap::new();
        let mut counts = vec![0u32; n];
        for (ordinal, count) in verdicts {
            passes.add(ordinal);
            counts[ordinal as usize] = count;
        }
        passes.run_optimize();
        SessionVerdicts { passes, counts }
    }
}

/// **Route D — the index for the viewport, the session for the mask.**
///
/// The only per-request work left is the candidacy test, which is the one question that genuinely
/// has the request in it.
fn sessioned(
    rows: &ArtifactRows,
    index: &TileIndex,
    session: &SessionVerdicts,
    tiles: &Bitmap,
    mask: &ComposedMask,
    row_count: u32,
    parallel: bool,
) -> Phases {
    let mut phases = Phases::default();

    let start = Instant::now();
    let mut shortlist = index.candidates(tiles, row_count);
    shortlist.and_inplace(&session.passes);
    let shortlist: Vec<u32> = shortlist.iter().collect();
    let candidates: Vec<u32> = if parallel {
        shortlist
            .par_iter()
            .filter(|&&o| rows.intersects(o, tiles, mask))
            .copied()
            .collect()
    } else {
        shortlist
            .iter()
            .filter(|&&o| rows.intersects(o, tiles, mask))
            .copied()
            .collect()
    };
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    // The count is a lookup, not an intersection.
    let start = Instant::now();
    let total: u64 = candidates
        .iter()
        .map(|&o| session.counts[o as usize] as u64)
        .sum();
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    phases.containment = 0.0;
    phases.passing = candidates.len();
    phases
}

/// **The claim the index route rests on, asserted rather than argued.**
///
/// The index is built over *unmasked* membership, so the question a reader will ask is whether it
/// can change what a principal is served. It cannot, and this is the check: for every mask and
/// every viewport in the sweep, the ordinals the indexed route admits are the ordinals the shipped
/// route admits — the same list, in the same order.
///
/// It holds because the index is a **conservative superset**. An artifact whose masked membership
/// meets the viewport has *some* member there, so its full membership meets the viewport, so it is
/// in the block the viewport touches, so it is in the shortlist. And every shortlisted artifact is
/// then put through the identical masked `intersects` before it is served. Pruning cannot admit
/// what the mask rejects, and it cannot reject what the mask admits.
/// **The build-time containment partition decides exactly what `satisfied_rank` decides** — every
/// artifact, every mask, the rank included and not merely the verdict.
///
/// This is [decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)'s
/// whole claim, and until the fixture was decorrelated it could not be checked: the expression was
/// read off the ordinal and the generating set was drawn from the same partition, so the two sides
/// agreed by construction. Now the expression is composed from the population and the two can
/// disagree — which is what makes agreeing worth asserting.
///
/// Under `--legacy-fixture` a disagreement is **reported rather than fatal**, because demonstrating
/// it is what that flag is for.
fn assert_containment_partition(
    rows: &ArtifactRows,
    contains: &ContainmentGroups,
    mask: &ComposedMask,
    holds: u32,
    strict: bool,
) {
    let sat = contains.satisfied_exprs(holds);
    let wrong = (0..rows.len() as u32)
        .into_par_iter()
        .filter(|&o| {
            let theirs = contains.rank_for(o, &sat);
            let ours = match rows.satisfied_rank(o, mask, true) {
                tessera_engine::artifacts::Containment::Satisfied(i) => Some(i),
                _ => None,
            };
            theirs != ours
        })
        .count();
    if wrong == 0 {
        return;
    }
    let msg = format!(
        "the build-time containment partition disagrees with satisfied_rank at {wrong} of {} \
         ordinals, mask holds {holds} groups",
        rows.len()
    );
    assert!(!strict, "{msg}");
    eprintln!("# legacy fixture: {msg}");
}

fn assert_same_answer(held: &Held, tiles: &Bitmap, mask: &ComposedMask, holds: u32, strict: bool) {
    let (rows, index, contains, row_count) = (held.rows, held.index, held.contains, held.row_count);
    let viewport_rows = tiles.cardinality();
    let full: Vec<u32> = (0..rows.len() as u32)
        .filter(|&o| rows.intersects(o, tiles, mask))
        .collect();
    let shortlisted: Vec<u32> = index
        .candidates(tiles, row_count)
        .iter()
        .filter(|&o| rows.intersects(o, tiles, mask))
        .collect();
    assert_eq!(
        full, shortlisted,
        "the tile index changed which artifacts are candidates — it is a superset filter and must not"
    );

    // Route E skips the masked test for the settled half, so its equality is the sharper claim:
    // it must reproduce the shipped answer without having asked the question.
    let (settled_set, open_set) = index.settled_and_open(tiles, row_count);
    let mut route_e: Vec<u32> = settled_set
        .iter()
        .filter(|&o| rows.masked_count(o, mask) > 0)
        .collect();
    route_e.extend(open_set.iter().filter(|&o| {
        if index.inside(o, tiles, viewport_rows) {
            rows.masked_count(o, mask) > 0
        } else {
            rows.intersects(o, tiles, mask)
        }
    }));
    route_e.sort_unstable();
    assert_eq!(
        full, route_e,
        "the settled half is not settled — an artifact wholly inside a covered block had its \
         viewport answer differ from its mask answer"
    );

    // **Routes G and H, whose answer is the campaign's headline and was never checked.** They are
    // the only routes that resolve containment from the build-time partition rather than from
    // `satisfied_rank`, and — until this revision — the only ones that admitted a candidate without
    // asking the mask anything. Both halves of that are checked here, against the shipped loop's
    // own verdict: candidacy through `ArtifactRows::intersects`, containment through
    // `ArtifactRows::satisfied_rank`, on the structures the engine ships.
    let shipped_verdict: Vec<u32> = (0..rows.len() as u32)
        .into_par_iter()
        .filter(|&o| {
            rows.intersects(o, tiles, mask)
                && rows.satisfied_rank(o, mask, true)
                    != tessera_engine::artifacts::Containment::Unsatisfied
        })
        .collect();
    let oracle: Bitmap = shipped_verdict.iter().copied().collect();
    let sat = contains.satisfied_exprs(holds);
    for (route, produced) in [
        ("grouped", grouped_candidates(held, &sat, tiles, mask)),
        ("hoisted", hoisted_candidates(held, &sat, tiles, mask)),
    ] {
        // **The artifacts served with nothing visible in them**, named separately from the set
        // difference because it is the specific failure the masked candidacy test exists to
        // prevent: a viewer shown a label over a region holding nothing they may see.
        let blind = produced
            .iter()
            .filter(|&o| rows.masked_count(o, mask) == 0)
            .count();
        let extra = produced.andnot(&oracle).cardinality();
        let missing = oracle.andnot(&produced).cardinality();
        if extra == 0 && missing == 0 {
            continue;
        }
        let msg = format!(
            "route {route} served {} where the shipped loop served {} — {extra} it should not \
             have ({blind} of them with no visible member at all), {missing} it should have",
            produced.cardinality(),
            oracle.cardinality()
        );
        assert!(!strict, "{msg}");
        eprintln!("# legacy fixture: {msg}");
    }

    // **The rank beside each served artifact**, which is what decides the text on the wire — and
    // the count, which is the number beside it. Both over the artifacts a request would actually
    // return, which is what the budget bounds.
    if strict {
        for &o in shipped_verdict.iter().take(BUDGET) {
            let ours = match rows.satisfied_rank(o, mask, true) {
                tessera_engine::artifacts::Containment::Satisfied(i) => Some(i),
                _ => None,
            };
            assert_eq!(
                contains.rank_for(o, &sat),
                ours,
                "the containment partition serves ordinal {o} at the wrong rank"
            );
            assert!(
                rows.masked_count(o, mask) > 0,
                "ordinal {o} is served with no visible member"
            );
        }
    }
}

/// **Route E — the settled half is free, and it is almost the whole population.**
///
/// An artifact wholly inside a block the viewport covers entirely has `membership ⊆ viewport`, so
/// its candidacy question collapses into the session's: *does it have a visible member at all* —
/// which `SessionVerdicts` answered when the mask was composed, because the criterion it applied is
/// a masked count of at least one. Nothing per-artifact is left. The request is
///
/// ```text
/// settled ∩ passes                                 // bitmap arithmetic, O(containers)
///   ∪  { o ∈ open ∩ passes : intersects(o, tiles) } // the viewport's edge, and only its edge
/// ```
///
/// and the second line is bounded by how many artifacts a viewport boundary cuts, not by how many
/// exist. That is the difference between a cost in the population and a cost in the picture.
///
/// **The verdicts it produces are identical to the shipped loop's** — asserted, not argued. Nothing
/// here is a bound or an approximation: the settled half is settled by a containment fact about
/// row ranges, and the open half runs the shipped test unchanged.
fn settled(
    rows: &ArtifactRows,
    index: &TileIndex,
    session: &SessionVerdicts,
    tiles: &Bitmap,
    mask: &ComposedMask,
    row_count: u32,
) -> Phases {
    let viewport_rows = tiles.cardinality();
    let mut phases = Phases::default();

    let start = Instant::now();
    let (mut settled, mut open) = index.settled_and_open(tiles, row_count);
    settled.and_inplace(&session.passes);
    open.and_inplace(&session.passes);
    let mut candidates: Vec<u32> = settled.iter().collect();
    // Two tests on the open half, cheapest first: the extent settles an artifact the node walk had
    // to give up on, and only what neither settles pays for the masked intersection.
    let edge: Vec<u32> = open
        .iter()
        .filter(|&o| index.inside(o, tiles, viewport_rows) || rows.intersects(o, tiles, mask))
        .collect();
    candidates.extend_from_slice(&edge);
    candidates.sort_unstable();
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = candidates
        .iter()
        .map(|&o| session.counts[o as usize] as u64)
        .sum();
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    phases.containment = 0.0;
    phases.passing = candidates.len();
    phases
}

/// **The row-major layout: one label per row, in place of one bitmap per artifact.**
///
/// A single-valued attribute predicate **partitions** the corpus — the column's distinct values are
/// its artifacts and every point carries one — so the whole layer is `label[row]`, and both
/// questions the request path asks become sequential scans whose cost is in **points rather than
/// artifacts**:
///
/// - **candidacy** — walk `viewport ∩ M_auth` and mark the labels seen. The result is exactly *which
///   artifacts have a visible member in view*, for every artifact at once, in one pass.
/// - **the count** — walk `M_auth` and increment per label. No viewport in it, so it belongs in the
///   per-token structure beside the containment verdicts.
///
/// **Row-addressed, and that is the point.** `attrs/` already holds this column, but addressed by
/// **entity** — which is right for a filter and wrong for a viewport, because a viewport is a set of
/// contiguous *row* ranges and reaching the entity form needs an `entity_of` per row. The Stage 6
/// measurement is where that shows: ~120 ms of its 175 ms was the inversion, not the counting. A
/// row-addressed copy is what removes it, and it costs one array.
///
/// **This is not a new mechanism.** `artifacts-from-points` already *reads* this layout — an integer
/// key column, or a list column of the artifacts a point belongs to — and converts it into
/// artifact-major bitmaps on the way in. The option is to keep what the build was handed.
///
/// ⊘ **Single-valued only.** A multi-valued or overlapping layer needs a list per row rather than a
/// label, which is the same inversion at a larger constant and is not measured here.
struct LabelColumn {
    /// `label[row]` — the artifact that row belongs to, or `HOLE`.
    label: Vec<u32>,
}

const HOLE: u32 = u32::MAX;

impl LabelColumn {
    /// Build one, or decline where the layer does not partition — a row claimed twice is not a
    /// partition, and silently keeping the last writer would measure a layer the fixture did not
    /// build.
    fn build(rows: &ArtifactRows, row_count: u32) -> Option<Self> {
        let mut label = vec![HOLE; row_count as usize];
        for ordinal in 0..rows.len() as u32 {
            let Some(m) = rows.get(ordinal) else { continue };
            for row in m.iter() {
                if label[row as usize] != HOLE {
                    return None;
                }
                label[row as usize] = ordinal;
            }
        }
        Some(LabelColumn { label })
    }

    /// The per-token half: `|membership ∩ M_auth|` for every artifact, in one walk of the mask.
    fn counts(&self, mask: &ComposedMask, artifacts: usize) -> Vec<u32> {
        let mut counts = vec![0u32; artifacts];
        let visible = {
            let mut v = mask.base.clone();
            v.andnot_inplace(&mask.minus);
            v.or_inplace(&mask.plus);
            v
        };
        for row in visible.iter() {
            let at = self.label[row as usize];
            if at != HOLE {
                counts[at as usize] += 1;
            }
        }
        counts
    }

    /// The per-request half: which artifacts have a visible member inside the viewport.
    fn present(&self, tiles: &Bitmap, mask: &ComposedMask, artifacts: usize) -> Bitmap {
        let here = mask.visible_rows(tiles);
        let mut seen = vec![false; artifacts];
        for row in here.iter() {
            let at = self.label[row as usize];
            if at != HOLE {
                seen[at as usize] = true;
            }
        }
        let mut out = Bitmap::new();
        for (ordinal, &hit) in seen.iter().enumerate() {
            if hit {
                out.add(ordinal as u32);
            }
        }
        out.run_optimize();
        out
    }

    fn resident_bytes(&self) -> u64 {
        (self.label.len() * std::mem::size_of::<u32>()) as u64
    }
}

/// **Route F — the row-major layout, with containment still per token.**
///
/// Candidacy is one scan and the count is a lookup, so nothing in this request is a function of how
/// many artifacts the layer has.
fn columnar(
    column: &LabelColumn,
    session: &SessionVerdicts,
    tiles: &Bitmap,
    mask: &ComposedMask,
    artifacts: usize,
) -> Phases {
    let mut phases = Phases::default();

    let start = Instant::now();
    let mut present = column.present(tiles, mask, artifacts);
    present.and_inplace(&session.passes);
    let candidates: Vec<u32> = present.iter().collect();
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = candidates
        .iter()
        .map(|&o| session.counts[o as usize] as u64)
        .sum();
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    phases.containment = 0.0;
    phases.passing = candidates.len();
    phases
}

/// **Containment as a build-time partition, evaluated per request rather than per token.**
///
/// `G ⊆ M_auth` holds exactly when every member of the generating set is visible, and an entity is
/// visible exactly when its own visibility expression holds for the principal's terms. So
///
/// ```text
/// G ⊆ M_auth   ⟺   ( ⋀ vis(e) for e in G )( T )
/// ```
///
/// — **a boolean expression over terms, and nothing about the mask appears in it.** It can be
/// composed once at build time, canonicalised, and interned; artifacts sharing an expression share
/// an answer for every principal alive. `annotations.md` §4 says as much already: *"what decides is
/// which terms, never how many items — which is why terms are what make the test tractable"*.
///
/// **The number of distinct expressions is what decides whether this is worth anything**, and §7.8's
/// per-term generating set is what keeps it small: a sample drawn from inside one signature group
/// has the expression *holds that group's term*, so the count is the vocabulary's, not the layer's.
/// That is the same construction the fixture already builds for its generating sets, so the grouping
/// here is read off the population rather than imposed on it.
///
/// Per request the whole containment pass becomes: evaluate the distinct expressions against the
/// principal's term set, union the groups that pass. **O(distinct expressions + containers), with no
/// per-token structure and nothing proportional to the artifact count.**
///
/// ⊘ **Two things this arm does not model.** A suppression removes a member of `G` from `M_auth`
/// whatever the terms say, so the answer must be intersected with *no member suppressed* — an
/// inverted index from entity to the artifacts whose generating set holds it, refreshed when the
/// overlay changes rather than per token. And a generating set that lost members in projection can
/// never be contained, which is per view and mask-independent, so it folds into the group.
struct ContainmentGroups {
    /// The distinct expressions, canonicalised and interned. Each is the **sorted, deduplicated**
    /// list of signature groups whose terms a principal must hold — a conjunction, and the
    /// canonical form is what lets two artifacts with the same requirement share an id.
    exprs: Vec<Vec<u16>>,
    /// Per distinct expression, the ordinals it decides at some rank.
    ///
    /// Held for the wide case, where taking the union once beats testing each candidate.
    holders: Vec<Bitmap>,
    /// Per `(ordinal, rank)`, which expression decides it — so a **narrow** request never touches
    /// the population at all. Unioning the satisfied expressions over ten million ordinals is
    /// `O(containers)`; a lookup per candidate is `O(candidates)`, which at a 0.024% viewport is
    /// three hundred of them.
    ///
    /// **`u16`, not `u8`.** A byte holds 256 expressions, which is fewer than a real vocabulary has
    /// and fewer than a two-group conjunction can produce from 32 groups; the earlier width was
    /// sized to the fixture's 32 rather than to the quantity, so it would have understated the
    /// state by the exact factor that matters to
    /// [decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)'s
    /// storage claim. Two bytes per artifact per rank.
    of_ordinal: Vec<u16>,
    ranks: usize,
}

impl ContainmentGroups {
    /// Built from the population — **from each artifact's actual generating set**, not from its
    /// ordinal.
    ///
    /// `G ⊆ M_auth` holds exactly when every member of `G` is visible, and a member is visible
    /// exactly when the principal holds its signature's term. So the expression is the set of
    /// signature groups the generating set spans, and reading it off the record is the only way the
    /// structure can be wrong in the way a real one could be. The earlier revision returned
    /// `ordinal % 32` and so agreed with `satisfied_rank` by construction rather than by
    /// computation.
    fn build(records: &[ArtifactRecord], bases: &[u32], ranks: usize, legacy: bool) -> Self {
        let mut intern: rustc_hash::FxHashMap<Vec<u16>, u16> = Default::default();
        let mut exprs: Vec<Vec<u16>> = Vec::new();
        let mut holders: Vec<Bitmap> = Vec::new();
        let mut of_ordinal = vec![0u16; records.len() * ranks];
        let group_of_entity = |e: u32| -> u16 {
            // `bases` is ascending with a terminator, so this is the group whose extent holds `e`.
            (bases.partition_point(|&b| b <= e) - 1) as u16
        };
        for (ordinal, record) in records.iter().enumerate() {
            for rank in 0..ranks {
                let mut canonical: Vec<u16> = if legacy {
                    // The alignment the earlier fixture had: the expression is read off the
                    // ordinal. Kept so the before/after is demonstrable rather than described.
                    vec![(ordinal as u32 % SIGNATURE_GROUPS) as u16]
                } else {
                    record
                        .contents
                        .get(rank)
                        .map(|c| c.generated_from.iter().map(group_of_entity).collect())
                        .unwrap_or_default()
                };
                canonical.sort_unstable();
                canonical.dedup();
                let id = match intern.get(&canonical) {
                    Some(&id) => id,
                    None => {
                        let id = u16::try_from(exprs.len())
                            .expect("more distinct expressions than a u16 id can address");
                        intern.insert(canonical.clone(), id);
                        exprs.push(canonical);
                        holders.push(Bitmap::new());
                        id
                    }
                };
                of_ordinal[ordinal * ranks + rank] = id;
                holders[id as usize].add(ordinal as u32);
            }
        }
        for h in &mut holders {
            h.run_optimize();
        }
        ContainmentGroups {
            exprs,
            holders,
            of_ordinal,
            ranks,
        }
    }

    fn distinct(&self) -> usize {
        self.exprs.len()
    }

    /// Which expressions this principal satisfies — `O(distinct expressions)`, evaluated once per
    /// request against the token's terms and against nothing else. **An empty expression is
    /// vacuously satisfied**, which is `annotations.md` §8.2's corpus-independent content and is
    /// what `satisfied_rank` does with an empty generating set.
    fn satisfied_exprs(&self, holds: u32) -> Vec<bool> {
        self.exprs
            .iter()
            .map(|e| e.iter().all(|&g| (g as u32) < holds))
            .collect()
    }

    /// The artifacts whose containment this principal satisfies at some rank, as a set — the wide
    /// route.
    fn satisfied(&self, sat: &[bool]) -> Bitmap {
        let lists: Vec<&Bitmap> = self
            .holders
            .iter()
            .zip(sat)
            .filter_map(|(h, &ok)| ok.then_some(h))
            .collect();
        Bitmap::fast_or(&lists)
    }

    /// The first rank this principal is served, or `None` — the narrow route, and the quantity
    /// `ArtifactRows::satisfied_rank` returns.
    fn rank_for(&self, ordinal: u32, sat: &[bool]) -> Option<u32> {
        let at = ordinal as usize * self.ranks;
        (0..self.ranks)
            .find(|&r| sat[self.of_ordinal[at + r] as usize])
            .map(|r| r as u32)
    }

    /// Whether this one artifact's containment holds at any rank.
    fn holds_for(&self, ordinal: u32, sat: &[bool]) -> bool {
        let at = ordinal as usize * self.ranks;
        (0..self.ranks).any(|r| sat[self.of_ordinal[at + r] as usize])
    }

    fn resident_bytes(&self) -> u64 {
        self.holders
            .iter()
            .map(|g| g.get_serialized_size_in_bytes::<croaring::Portable>() as u64)
            .sum::<u64>()
            + (self.of_ordinal.len() * std::mem::size_of::<u16>()) as u64
            + self
                .exprs
                .iter()
                .map(|e| (e.len() * std::mem::size_of::<u16>()) as u64)
                .sum::<u64>()
    }
}

/// **The masked candidacy test, restored to routes G and H — and it is not optional.**
///
/// Both routes admitted the settled half on the containment partition alone and short-circuited the
/// open half through `TileIndex::inside`, so **no candidate anywhere on either route was asked
/// whether the principal can see a member of it**. That is a disclosure and a mismeasurement at the
/// same time: the artifacts it serves are ones the shipped loop withholds, and the work it omits is
/// the work those routes exist to price.
///
/// The settle test does not remove the question, it *changes* it: `membership ⊆ viewport` makes
/// *"has a visible member here"* the same question as *"has a visible member"*, and the second still
/// has to be asked of somebody. `SessionVerdicts` is where routes D and E ask it — which
/// [decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
/// rules out, leaving these routes to ask it per request. So every candidate pays one early-exiting
/// intersection, settled or open.
///
/// The two routes differ in *where the mask meets the viewport*, which is what they are named for
/// and the one thing the comparison is about: [`grouped`] composes per artifact through the shipped
/// `ArtifactRows::intersects`, [`hoisted`] composes `viewport ∩ M_auth` once. Both answer
/// `membership ∩ viewport ∩ M_auth ≠ ∅`, and identically.
fn grouped_candidates(held: &Held, sat: &[bool], tiles: &Bitmap, mask: &ComposedMask) -> Bitmap {
    let (rows, index, contains, row_count) = (held.rows, held.index, held.contains, held.row_count);
    let (settled, open) = index.settled_and_open(tiles, row_count);
    // **Which way round to ask depends on how much the viewport left.** Unioning the satisfied
    // expressions is `O(containers)` and independent of the viewport; testing each candidate is
    // `O(candidates)`. The viewport walk has already said which is smaller.
    let wide = settled.cardinality() + open.cardinality() > (row_count as u64 / 64).max(4096);
    let mut passing: Bitmap = if wide {
        let passes = contains.satisfied(sat);
        let mut s = settled;
        s.and_inplace(&passes);
        let mut o = open;
        o.and_inplace(&passes);
        s.or_inplace(&o);
        s.iter()
            .filter(|&x| rows.intersects(x, tiles, mask))
            .collect()
    } else {
        let mut s = settled;
        s.or_inplace(&open);
        s.iter()
            .filter(|&x| contains.holds_for(x, sat) && rows.intersects(x, tiles, mask))
            .collect()
    };
    passing.run_optimize();
    passing
}

/// **Route G — the settled route with containment resolved from the build-time partition.**
///
/// The per-token structure is gone: what stands in its place is one union of a handful of
/// build-time bitmaps, priced here as part of the request that uses it.
fn grouped(held: &Held, holds: u32, tiles: &Bitmap, mask: &ComposedMask) -> Phases {
    let (rows, contains, lineage) = (held.rows, held.contains, held.lineage);
    let mut phases = Phases::default();

    let start = Instant::now();
    // Evaluated against the token's terms and against nothing else — `O(distinct expressions)`.
    let sat = contains.satisfied_exprs(holds);
    let passing = grouped_candidates(held, &sat, tiles, mask);
    // **The result stays a set until something needs a list**, which is the difference between
    // 141 ms and 35 ms at ten million: `Bitmap::iter` is ascending by construction, so the sort the
    // earlier revision ran over ten million ordinals — 110 ms of that 141 — was sorting two runs
    // that were already in order. What is left is the materialisation itself, which only exists
    // because `cut` takes a slice.
    let candidates: Vec<u32> = passing.iter().collect();
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    // **The count, for the artifacts actually served.** Where the layer declares no existence
    // criterion the masked count is only the number beside a served artifact, so it is bounded by
    // the budget rather than by the population — which is what removes the other half of the
    // per-token structure. A thousand, the order a client can draw.
    let start = Instant::now();
    let total: u64 = candidates
        .iter()
        .take(BUDGET)
        .map(|&o| rows.masked_count(o, mask))
        .sum();
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    // **Which rank is served, for the artifacts actually served.** Ranked contents mean containment
    // has an answer and not just a verdict, and the answer decides which text goes on the wire.
    let start = Instant::now();
    let ranks: u32 = candidates
        .iter()
        .take(BUDGET)
        .filter_map(|&o| contains.rank_for(o, &sat))
        .sum();
    phases.containment = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(ranks);

    let (cut_us, served) = frontier(lineage, &candidates);
    phases.cut = cut_us;
    phases.passing = served;
    phases
}

/// **Route H — the mask is composed with the viewport once, not once per artifact.**
///
/// `ArtifactRows::intersects` asks *does this artifact have a visible member in view* by
/// materialising `membership ∩ viewport` and then putting that through the composed mask — three
/// set operations and a heap allocation, **per artifact**. But `viewport ∩ M_auth` has no artifact
/// in it. Composing it once per request leaves each artifact a single `Bitmap::intersect`, which is
/// a boolean with an early exit: it stops at the first container that meets.
///
/// **The early exit is why this matters most for the shape it was worst for.** A scattered artifact
/// has members everywhere, so it almost always *does* meet the viewport — 99.8% of them at a
/// hundred members and a 6.25% viewport — and the test that was costing 2.4 µs was confirming a
/// foregone conclusion the long way round. An early-exiting probe answers it at the first container.
///
/// Exact, and trivially so: `rows ∩ (viewport ∩ M_auth) ≠ ∅` and `rows ∩ viewport ∩ M_auth ≠ ∅` are
/// the same statement. What is bought is where the composition happens, not what it computes.
fn hoisted_candidates(held: &Held, sat: &[bool], tiles: &Bitmap, mask: &ComposedMask) -> Bitmap {
    let (rows, index, contains, row_count) = (held.rows, held.index, held.contains, held.row_count);
    let (settled, open) = index.settled_and_open(tiles, row_count);
    // The one composition. Its cost is the viewport's containers, not the population's — and every
    // candidate below is then a single early-exiting `Bitmap::intersect` against it. See
    // [`grouped_candidates`] for why the settled half pays it too.
    let here = mask.visible_rows(tiles);
    let mut all = settled;
    all.or_inplace(&open);
    let mut passing: Bitmap = all
        .iter()
        .filter(|&x| contains.holds_for(x, sat) && rows.get(x).is_some_and(|m| m.intersect(&here)))
        .collect();
    passing.run_optimize();
    passing
}

fn hoisted(held: &Held, holds: u32, tiles: &Bitmap, mask: &ComposedMask) -> Phases {
    let (rows, contains, lineage) = (held.rows, held.contains, held.lineage);
    let mut phases = Phases::default();
    let start = Instant::now();
    let sat = contains.satisfied_exprs(holds);
    let passing = hoisted_candidates(held, &sat, tiles, mask);
    let candidates: Vec<u32> = passing.iter().collect();
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = candidates
        .iter()
        .take(BUDGET)
        .map(|&o| rows.masked_count(o, mask))
        .sum();
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    let start = Instant::now();
    let ranks: u32 = candidates
        .iter()
        .take(BUDGET)
        .filter_map(|&o| contains.rank_for(o, &sat))
        .sum();
    phases.containment = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(ranks);

    let (cut_us, served) = frontier(lineage, &candidates);
    phases.cut = cut_us;
    phases.passing = served;
    phases
}

/// **The row-major layout for a layer that does *not* partition: a list per row.**
///
/// §5.1's label column needs each point to carry exactly one value. A per-analyst selection, a
/// terms-as-artifacts layer or a multi-valued attribute predicate carries several or none, so the
/// row-major form is `row → list of artifacts` rather than `row → artifact`. Everything else about
/// it is the same: candidacy is one scan of `viewport ∩ M_auth` marking what it finds, so it costs
/// **memberships in the viewport** rather than artifacts in the layer, and is flat in the artifact
/// count.
///
/// **Not a new shape either.** `artifacts-from-points` already reads a list column — the artifacts a
/// point belongs to — and converts it into artifact-major bitmaps on the way in. This is keeping it.
///
/// The storage question is different from the label column's and worth stating plainly: this is
/// `Σ|membership|` entries rather than one per row, so a layer whose points each belong to fifty
/// artifacts costs fifty times a label column. What it is being compared against is artifact-major
/// Roaring at ~78.5 B **per container** on scattered membership, which is ~20× more for the same
/// facts — so the inversion wins on space at every `k`, and loses to the label column only where a
/// label column is possible at all.
struct ListColumn {
    /// `at[row]..at[row + 1]` indexes `of_row`.
    at: Vec<u32>,
    of_row: Vec<u32>,
}

impl ListColumn {
    fn build(rows: &ArtifactRows, row_count: u32, artifacts: usize) -> Self {
        let mut counts = vec![0u32; row_count as usize + 1];
        for ordinal in 0..artifacts as u32 {
            let Some(m) = rows.get(ordinal) else { continue };
            for row in m.iter() {
                counts[row as usize] += 1;
            }
        }
        let mut at = vec![0u32; row_count as usize + 1];
        let mut running = 0u32;
        for (row, n) in counts.iter().take(row_count as usize).enumerate() {
            at[row] = running;
            running += n;
        }
        at[row_count as usize] = running;
        let mut cursor = at.clone();
        let mut of_row = vec![0u32; running as usize];
        for ordinal in 0..artifacts as u32 {
            let Some(m) = rows.get(ordinal) else { continue };
            for row in m.iter() {
                of_row[cursor[row as usize] as usize] = ordinal;
                cursor[row as usize] += 1;
            }
        }
        ListColumn { at, of_row }
    }

    /// Which artifacts have a visible member inside the viewport — every one of them, in one scan.
    fn present(&self, tiles: &Bitmap, mask: &ComposedMask, artifacts: usize) -> Bitmap {
        let here = mask.visible_rows(tiles);
        let mut seen = vec![false; artifacts];
        for row in here.iter() {
            let (from, to) = (
                self.at[row as usize] as usize,
                self.at[row as usize + 1] as usize,
            );
            for &ordinal in &self.of_row[from..to] {
                seen[ordinal as usize] = true;
            }
        }
        let mut out = Bitmap::new();
        for (ordinal, &hit) in seen.iter().enumerate() {
            if hit {
                out.add(ordinal as u32);
            }
        }
        out.run_optimize();
        out
    }

    fn resident_bytes(&self) -> u64 {
        ((self.at.len() + self.of_row.len()) * std::mem::size_of::<u32>()) as u64
    }
}

/// **Route I — the list column, for a layer that overlaps.**
fn listed(held: &Held, holds: u32, tiles: &Bitmap, mask: &ComposedMask) -> Phases {
    let (column, contains, lineage, artifacts) =
        (held.lists, held.contains, held.lineage, held.artifacts);
    let mut phases = Phases::default();
    let start = Instant::now();
    let mut passing = column.present(tiles, mask, artifacts);
    passing.and_inplace(&contains.satisfied(&contains.satisfied_exprs(holds)));
    let candidates: Vec<u32> = passing.iter().collect();
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();
    phases.count = 0.0;
    phases.containment = 0.0;
    let (cut_us, served) = frontier(lineage, &candidates);
    phases.cut = cut_us;
    phases.passing = served;
    phases
}

/// **How many distinct canonicalised expressions a generating-set population produces.**
///
/// [Decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)
/// replaces a per-token structure with a build-time one whose size is *the number of distinct
/// expressions*, and that number was asserted rather than counted. It is not the artifact count and
/// it is not the vocabulary: it is the number of distinct **sets of signatures** the layer's
/// generating sets span, which depends on how large a sample each content carries and on how
/// concentrated the corpus's signatures are.
///
/// Two populations, because they bracket the answer:
///
/// - the **fixture's own** construction, at several sample sizes and spreads — what the grid above
///   is measured on;
/// - a **real signature distribution**, read from `--signatures <file>`: one carrier count per
///   line (the last whitespace-separated integer on the line; `#` and blanks skipped). Each member
///   of a sample lands in a signature with probability proportional to its carriers, and the
///   expression is the distinct signatures the sample spans — which is the count the 2.4M-artifact
///   corpus run needs and cannot be guessed at.
///
/// 240 is in the size sweep because `annotations.md` §7.8's worked example uses a 240-document
/// prompt sample spanning eleven terms, which is the case the design predicts label creep for.
fn expression_census(args: &[String], sets: usize) {
    println!("population,sets,size,spread,distinct_exprs,mean_expr_len,ids_bytes,exprs_bytes");

    let report =
        |population: &str, size: u32, spread: &str, seen: &rustc_hash::FxHashSet<Vec<u32>>| {
            let distinct = seen.len();
            let total_len: usize = seen.iter().map(Vec::len).sum();
            let id_width = if distinct > u16::MAX as usize { 4 } else { 2 };
            println!(
                "{population},{sets},{size},{spread},{distinct},{:.2},{},{}",
                total_len as f64 / distinct.max(1) as f64,
                sets * id_width,
                total_len * 2,
            );
        };

    // The fixture's own population. Only the group bases matter, so they are spaced evenly.
    let span = 1u32 << 30;
    let bases: Vec<u32> = (0..=SIGNATURE_GROUPS)
        .map(|g| (g as u64 * span as u64 / SIGNATURE_GROUPS as u64) as u32)
        .collect();
    let group_of = |e: u32| -> u32 { (bases.partition_point(|&b| b <= e) - 1) as u32 };
    for size in [1u32, 2, 4, 8, 16, 64, 240] {
        for spread in [1u32, 2, 4] {
            let mut seen: rustc_hash::FxHashSet<Vec<u32>> = Default::default();
            for i in 0..sets {
                let g = generating_entities(i, 0, spread, size, &bases);
                let mut expr: Vec<u32> = g.iter().map(group_of).collect();
                expr.sort_unstable();
                expr.dedup();
                seen.insert(expr);
            }
            report("fixture", size, &spread.to_string(), &seen);
        }
    }

    // **The fixture's own construction pins the count at 32**, because it draws each set from a
    // fixed number of groups chosen by one hash — which is `annotations.md` §8.1's per-term
    // mitigation working exactly as its author intends, and is therefore the *best* case rather than
    // the expected one. The population below is what happens without it: each member of the sample
    // lands wherever its own document's signature is, and the expression is the distinct signatures
    // the sample turns out to span. That is the number a layer whose contents were summarised from
    // arbitrary samples would carry.
    let mut rng = Rng(0xB0BB1E);
    for size in [1u32, 2, 4, 8, 16, 64, 240] {
        let mut seen: rustc_hash::FxHashSet<Vec<u32>> = Default::default();
        for _ in 0..sets {
            let mut expr: Vec<u32> = (0..size)
                .map(|_| rng.below(SIGNATURE_GROUPS as u64) as u32)
                .collect();
            expr.sort_unstable();
            expr.dedup();
            seen.insert(expr);
        }
        report("uniform", size, "drawn", &seen);
    }

    let Some(path) = args
        .iter()
        .position(|a| a == "--signatures")
        .and_then(|i| args.get(i + 1))
    else {
        eprintln!("# no --signatures file given, so only the fixture's own population is counted");
        return;
    };
    let text = std::fs::read_to_string(path).expect("the signature distribution reads");
    let weights: Vec<u64> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_whitespace().last().and_then(|w| w.parse().ok()))
        .collect();
    assert!(
        !weights.is_empty(),
        "the signature distribution names no signatures"
    );
    let mut cumulative = Vec::with_capacity(weights.len());
    let mut running = 0u64;
    for w in &weights {
        running += w;
        cumulative.push(running);
    }
    eprintln!(
        "# signature distribution: {} signatures, {running} carriers",
        weights.len()
    );
    for size in [1u32, 2, 4, 8, 16, 64, 240] {
        let mut seen: rustc_hash::FxHashSet<Vec<u32>> = Default::default();
        for _ in 0..sets {
            let mut expr: Vec<u32> = (0..size)
                .map(|_| {
                    let draw = rng.below(running);
                    cumulative.partition_point(|&c| c <= draw) as u32
                })
                .collect();
            expr.sort_unstable();
            expr.dedup();
            seen.insert(expr);
        }
        report("corpus", size, "drawn", &seen);
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("tessera-serving-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");

    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str, default: u64| -> u64 {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(|v| v.parse().expect("a number"))
            .unwrap_or(default)
    };
    let text = |name: &str| -> Option<&String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
    };
    let present = |name: &str| args.iter().any(|a| a == name);

    let rows_n: u32 = flag("--rows", 100_000_000) as u32;
    let artifacts: usize = flag("--artifacts", 1_000_000) as usize;
    // **The members rule is `rows / artifacts`.** A layer is a partition of the corpus, so its
    // memberships have to cover it; a run whose members are a tenth of that measures a tenth of a
    // layer and every candidacy figure in it is proportionally cheap. The default follows the rule
    // rather than a constant, and the coverage line below says what any override actually did.
    let members: u32 = flag(
        "--members",
        (rows_n as u64 / artifacts.max(1) as u64).max(1),
    ) as u32;
    let runs_per: u32 = flag("--runs", 4) as u32;
    let blocks: u32 = flag("--blocks", 4) as u32;
    let ranks: usize = flag("--ranks", 2).max(1) as usize;
    let gset: u32 = flag("--gset", 8).max(1) as u32;
    let spread: u32 = flag("--spread", 1).max(1) as u32;
    let iters: usize = flag("--iters", 3).max(1) as usize;
    let shape = Shape {
        members,
        runs: runs_per,
        blocks,
    };
    let legacy = present("--legacy-fixture");
    let coverage = present("--coverage");
    let arm = text("--arm")
        .and_then(|n| Arm::parse(n))
        .unwrap_or(Arm::Runs);

    if present("--expressions") {
        expression_census(&args, artifacts);
        return;
    }

    // Under `--legacy-fixture` the ranks collapse to one and the generating sets come back from the
    // membership, which is the alignment the corrections removed. Assertions then **report** rather
    // than fail, because demonstrating the disagreement is the whole point of the flag.
    let ranks = if legacy { 1 } else { ranks };
    let strict = !legacy;

    eprintln!(
        "# rows={rows_n} artifacts={artifacts} members={members} runs={runs_per} blocks={blocks} \
         ranks={ranks} gset={gset} spread={spread} iters={iters} legacy={legacy} arm={}",
        arm.name()
    );

    // Every build phase, recorded rather than printed and forgotten — `--costs-out` writes them
    // beside the grid. The index build and the projection rebuild are quoted in the design and had
    // no data file behind them.
    let mut costs: Vec<(String, f64, u64)> = Vec::new();

    let mut rng = Rng(0x5EED);
    let t = Instant::now();
    let (space, row_order, bases) = row_space(&dir, rows_n);
    let group_size = rows_n / SIGNATURE_GROUPS;
    costs.push(("row_space".into(), t.elapsed().as_secs_f64(), 0));
    eprintln!("# row space: {:.1} s", t.elapsed().as_secs_f64());

    // The tree's extents, where the arm takes its membership from the tree.
    let nested = if arm == Arm::Nested {
        nested_ranges(artifacts, rows_n)
    } else {
        Vec::new()
    };

    let t = Instant::now();
    let mut row_span_total = 0u64;
    let mut member_total = 0u64;
    // One bit per row, only where asked: the exact covered fraction costs a pass over every
    // membership, which at the target is a second or two and not worth paying on every run.
    let mut covered = if coverage {
        vec![0u64; (rows_n as usize).div_ceil(64)]
    } else {
        Vec::new()
    };
    let records: Vec<ArtifactRecord> = (0..artifacts)
        .map(|i| {
            let in_rows = if arm == Arm::Nested {
                let (lo, hi) = nested[i];
                let mut b = Bitmap::new();
                if hi > lo {
                    b.add_range(lo..hi);
                }
                b.run_optimize();
                b
            } else {
                membership_rows(arm, rows_n, i, artifacts, shape, &mut rng)
            };
            row_span_total += in_rows.statistics().n_containers as u64;
            member_total += in_rows.cardinality();
            if coverage {
                for row in in_rows.iter() {
                    covered[row as usize / 64] |= 1 << (row % 64);
                }
            }
            // **Ranked contents, with disjoint generating sets** — which is what an artifact
            // actually carries: `annotations.md` §6 has the caller supply a ladder of descriptions
            // and the viewer served the first whose set they contain. A fixture with one content
            // measures a verdict where the model has an answer, and the rank is the thing that
            // decides which text goes on the wire.
            let contents: Vec<ContentSet> = (0..ranks)
                .map(|rank| {
                    let generated_from = if legacy {
                        // The old alignment: a sample of the artifact's own members inside the
                        // group its ordinal names. See [`generating_entities`] for what that made
                        // impossible to observe.
                        let group = (i as u32) % SIGNATURE_GROUPS;
                        let mut sample: Bitmap = in_rows
                            .iter()
                            .filter(|&row| {
                                space
                                    .entity_of(RowId::new(row))
                                    .is_some_and(|e| (e.raw() as u32) / group_size.max(1) == group)
                            })
                            .take(gset as usize)
                            .collect();
                        if sample.is_empty() {
                            sample = in_rows.iter().take(1).collect();
                        }
                        to_entities(&sample, &space)
                    } else {
                        generating_entities(i, rank as u32, spread, gset, &bases)
                    };
                    let values = vec![format!("a label, rank {rank}")];
                    ContentSet {
                        digest: tessera_lifecycle::membership::content_digest(&values),
                        values: Some(values),
                        cardinality: generated_from.cardinality(),
                        generated_from,
                    }
                })
                .collect();
            ArtifactRecord {
                entity: EntityId::new(i as u64),
                key: None,
                view: None,
                incarnation: 0,
                members: to_entities(&in_rows, &space).into(),
                contents,
                attached_to: None,
                parents: Vec::new(),
                access: Vec::new(),
            }
        })
        .collect();
    costs.push(("records".into(), t.elapsed().as_secs_f64(), 0));
    eprintln!(
        "# entity-space records: {:.1} s, {:.1} row blocks per artifact",
        t.elapsed().as_secs_f64(),
        row_span_total as f64 / artifacts as f64
    );
    // **Coverage, so a mis-sized run says so rather than reading as a cheap one.** A layer that
    // covers a tenth of the corpus has a tenth of the candidacy work in it, and the ratio is the
    // first thing to check against a headline.
    eprint!(
        "# coverage: {:.3} members per row ({member_total} members over {rows_n} rows), \
         {:.1} row blocks per artifact",
        member_total as f64 / rows_n as f64,
        row_span_total as f64 / artifacts as f64,
    );
    if coverage {
        let bits: u64 = covered.iter().map(|w| w.count_ones() as u64).sum();
        eprint!(
            ", {:.1}% of the row space covered",
            100.0 * bits as f64 / rows_n as f64
        );
    }
    eprintln!();
    drop(covered);

    // Built from the records, before they are dropped — the expression is the population's, and
    // reading it off anything else is what made the earlier revision unfalsifiable.
    let t = Instant::now();
    let contains = ContainmentGroups::build(&records, &bases, ranks, legacy);
    costs.push((
        "containment_groups".into(),
        t.elapsed().as_secs_f64(),
        contains.resident_bytes(),
    ));
    eprintln!(
        "# containment groups: {} distinct expressions over {} (artifact, rank) pairs, {:.1} s, \
         {:.1} MB serialised",
        contains.distinct(),
        artifacts * ranks,
        t.elapsed().as_secs_f64(),
        contains.resident_bytes() as f64 / 1e6
    );

    let t = Instant::now();
    let row_forms = ArtifactRows::build(
        records.iter().enumerate().map(|(i, r)| (i as u32, r)),
        &space,
    );
    costs.push(("projection".into(), t.elapsed().as_secs_f64(), 0));
    eprintln!(
        "# projection (the generation move): {:.1} s",
        t.elapsed().as_secs_f64()
    );
    drop(records);

    let t = Instant::now();
    let column = LabelColumn::build(&row_forms, rows_n);
    if let Some(c) = &column {
        costs.push((
            "label_column".into(),
            t.elapsed().as_secs_f64(),
            c.resident_bytes(),
        ));
        eprintln!(
            "# label column: {:.1} s to build, {:.0} MB resident",
            t.elapsed().as_secs_f64(),
            c.resident_bytes() as f64 / 1e6
        );
    } else {
        eprintln!("# label column: the layer does not partition, so there is none");
    }

    // **Built only for the shapes that would be served from it.** It is `Σ|membership|` entries, so
    // at a corpus-covering layer over 10⁹ rows it is gigabytes — and a clustered layer is served
    // artifact-major at every zoom, so building one for the `runs` arm measures nothing and costs
    // the run its headroom.
    let t = Instant::now();
    let lists = if matches!(arm, Arm::Scattered | Arm::Partition) {
        let built = ListColumn::build(&row_forms, rows_n, artifacts);
        costs.push((
            "list_column".into(),
            t.elapsed().as_secs_f64(),
            built.resident_bytes(),
        ));
        eprintln!(
            "# list column: {:.1} s to build, {:.0} MB resident",
            t.elapsed().as_secs_f64(),
            built.resident_bytes() as f64 / 1e6
        );
        built
    } else {
        eprintln!("# list column: not built — this arm is served artifact-major at every zoom");
        ListColumn {
            at: vec![0; 1],
            of_row: Vec::new(),
        }
    };

    let t = Instant::now();
    let index = TileIndex::build(&row_forms, rows_n);
    costs.push((
        "tile_index".into(),
        t.elapsed().as_secs_f64(),
        index.resident_bytes(),
    ));
    eprintln!(
        "# tile index: {:.1} s to build, {:.1} MB serialised",
        t.elapsed().as_secs_f64(),
        index.resident_bytes() as f64 / 1e6
    );
    // **The fourth population, named.** An artifact too wide for any node of the hierarchy is
    // neither settled nor prunable: it is handed back on every request at every zoom, and it pays a
    // masked intersection each time. On the clustered arms it is empty; on the scattered arm it is
    // the **whole layer**, which is the shape behind that arm's flat cost and behind the 137 s
    // datum. A run that does not report it reads as though the index applied.
    let everywhere = index.everywhere.cardinality();
    eprintln!(
        "# everywhere (too wide for any node): {everywhere} of {artifacts} artifacts, {:.1}%{}",
        100.0 * everywhere as f64 / artifacts.max(1) as f64,
        if everywhere * 2 > artifacts as u64 {
            " — the index settles nothing for the majority of this layer"
        } else {
            ""
        }
    );

    // **A three-way tree over the layer**, so the cut has a frontier to resolve rather than a flat
    // level it short-circuits. Held once, as a generation object is: `Lineages` caches it in the
    // engine now, and a request that rebuilt it would be timing a build rather than a cut.
    //
    // ⊘ **This tree is not a hierarchy on any arm but `nested`, and every treed figure on the other
    // arms is suspect because of it.** The parent of ordinal *o* is `(o - 1) / 3`, so the tree's
    // shape is the ordinal space's and has no relation to the geometry — while each artifact's
    // membership sits near its *own* ordinal. A real nested layer is the opposite: a parent's
    // membership **contains** its children's, so a parent is in view whenever any child is, and the
    // root is in view always. The `nested` arm is that correction.
    let lineage = tessera_engine::cut::Lineage::new(
        (0..artifacts as u32).map(|o| (o, (o > 0).then(|| (o - 1) / 3))),
    );

    let held = Held {
        rows: &row_forms,
        index: &index,
        contains: &contains,
        lists: &lists,
        lineage: &lineage,
        row_count: rows_n,
        artifacts,
    };

    // **Every iteration, not the best of them.** The design quoted the minimum of three whole runs
    // over a grid that already reported the minimum of three iterations, which is a minimum of nine
    // and is not a number any request will see. The median is what a cell is worth; the spread is
    // what says whether the median means anything.
    println!("arm,rows,artifacts,mask_pct,viewport_pct,depth,route,setup_ms,candidacy_us,count_us,containment_us,cut_us,total_us,min_total_us,max_total_us,candidates,served");

    // **`--only` keeps the legacy routes out of a run that cannot afford them.** The shipped loop is
    // `O(artifacts)` with a masked intersection apiece, which at ten million scattered artifacts is
    // ~24 s a call — hours across the sweep, to re-measure a figure three smaller scales already
    // establish. Which routes run does not change what any of them answers: each builds its own
    // result from the same held state.
    //
    // `--parity` is §4.2's comparison and nothing else: the shipped loop, the per-token route it was
    // measured against, and the two build-time-partition routes that replaced it. It had no data
    // file behind it.
    let parity_routes = "shipped,index+session,settled,grouped,hoisted";
    let only: Option<Vec<&str>> = text("--only")
        .map(|v| v.split(',').collect())
        .or_else(|| present("--parity").then(|| parity_routes.split(',').collect()));

    // Whole signature groups, so the mask percentages are the ones a principal can actually have.
    // **Swept through the middle, not around it.** Three densities — everything, a tenth, a
    // thirtieth — miss the band where the cut is dearest: enough artifacts pass to make the sweep
    // expensive, not enough for the downward walk's guard to hold. That band is somewhere between
    // them, and a table sampling only the ends reports the wrong worst case.
    for groups in [SIGNATURE_GROUPS, 24, 16, 8, 3, 1] {
        let mask_pct = 100.0 * groups as f64 / SIGNATURE_GROUPS as f64;
        let m = mask(rows_n, &row_order, &bases, groups, &mut rng);

        // Decision 0093's claim, checked before anything is timed against it.
        assert_containment_partition(&row_forms, &contains, &m, groups, strict);

        let t = Instant::now();
        let session = SessionVerdicts::build(&row_forms, &m, 1);
        let session_ms = t.elapsed().as_secs_f64() * 1e3;
        costs.push((
            format!("session_verdicts_mask{groups}"),
            t.elapsed().as_secs_f64(),
            0,
        ));
        eprintln!(
            "# session verdicts at mask={mask_pct}%: {session_ms:.0} ms, {} of {artifacts} pass",
            session.passes.cardinality()
        );

        // Depth is what a client picks: enough tiles to fill a screen, so roughly sixteen a side.
        // **Through the wide end too.** Jumping from a sixteenth of the map to all of it steps
        // over the zooms a viewer spends most of their time at, and the wide end is where a
        // viewport stops narrowing the candidate set — so it is where the cut's cost is decided.
        for (viewport_pct, depth) in [
            (100.0f64, 0u8),
            (75.0, 4),
            (50.0, 4),
            (25.0, 5),
            (6.25, 6),
            (0.39, 8),
            (0.024, 10),
        ] {
            let tiles = viewport(rows_n, viewport_pct / 100.0, depth);
            assert_same_answer(&held, &tiles, &m, groups, strict);

            let routes: Vec<Route> = vec![
                (
                    "shipped",
                    0.0,
                    Box::new(|| shipped(&row_forms, &tiles, &m, false)),
                ),
                (
                    "shipped+par",
                    0.0,
                    Box::new(|| shipped(&row_forms, &tiles, &m, true)),
                ),
                (
                    "early",
                    0.0,
                    Box::new(|| early_exit(&row_forms, &tiles, &m, false)),
                ),
                (
                    "early+par",
                    0.0,
                    Box::new(|| early_exit(&row_forms, &tiles, &m, true)),
                ),
                (
                    "index",
                    0.0,
                    Box::new(|| indexed(&row_forms, &index, &tiles, &m, rows_n, false)),
                ),
                (
                    "index+par",
                    0.0,
                    Box::new(|| indexed(&row_forms, &index, &tiles, &m, rows_n, true)),
                ),
                (
                    "index+session",
                    session_ms,
                    Box::new(|| sessioned(&row_forms, &index, &session, &tiles, &m, rows_n, false)),
                ),
                (
                    "settled",
                    session_ms,
                    Box::new(|| settled(&row_forms, &index, &session, &tiles, &m, rows_n)),
                ),
                (
                    "grouped",
                    0.0,
                    Box::new(|| grouped(&held, groups, &tiles, &m)),
                ),
                (
                    "hoisted",
                    0.0,
                    Box::new(|| hoisted(&held, groups, &tiles, &m)),
                ),
            ];

            let mut routes = routes;
            if matches!(arm, Arm::Scattered | Arm::Partition) {
                routes.push((
                    "listed",
                    0.0,
                    Box::new(|| listed(&held, groups, &tiles, &m)),
                ));
                // **The row-major list answers candidacy by a different mechanism**, so the claim
                // that it answers it identically is the one worth checking rather than describing.
                let mut theirs: Vec<u32> = lists.present(&tiles, &m, artifacts).iter().collect();
                let mut ours: Vec<u32> = (0..row_forms.len() as u32)
                    .filter(|&o| row_forms.intersects(o, &tiles, &m))
                    .collect();
                theirs.sort_unstable();
                ours.sort_unstable();
                assert_eq!(
                    ours, theirs,
                    "the list column changed which artifacts are candidates"
                );
            }
            if let Some(c) = &column {
                // **Asserted before it is timed.** The row-major route answers candidacy by a
                // different mechanism entirely, so the claim that it answers it *identically* is
                // the one worth checking rather than describing.
                let mut theirs: Vec<u32> = c.present(&tiles, &m, artifacts).iter().collect();
                let mut ours: Vec<u32> = (0..row_forms.len() as u32)
                    .filter(|&o| row_forms.intersects(o, &tiles, &m))
                    .collect();
                theirs.sort_unstable();
                ours.sort_unstable();
                assert_eq!(
                    ours, theirs,
                    "the row-major layout changed which artifacts are candidates"
                );
                let counts = c.counts(&m, artifacts);
                for ordinal in 0..row_forms.len() as u32 {
                    assert_eq!(
                        counts[ordinal as usize] as u64,
                        row_forms.masked_count(ordinal, &m),
                        "the row-major masked count disagrees at ordinal {ordinal}"
                    );
                }
                routes.push((
                    "column",
                    session_ms,
                    Box::new(|| columnar(c, &session, &tiles, &m, artifacts)),
                ));
            }

            for (route, setup_ms, run) in routes {
                if only.as_ref().is_some_and(|keep| !keep.contains(&route)) {
                    continue;
                }
                let mut taken: Vec<Phases> = (0..iters).map(|_| run()).collect();
                taken.sort_by(|a, b| a.total().total_cmp(&b.total()));
                let median = taken[taken.len() / 2];
                let (lo, hi) = (taken[0].total(), taken[taken.len() - 1].total());
                println!(
                    "{},{rows_n},{artifacts},{mask_pct},{viewport_pct},{depth},{route},{setup_ms:.0},{:.0},{:.0},{:.0},{:.0},{:.0},{lo:.0},{hi:.0},{},{}",
                    arm.name(),
                    median.candidacy,
                    median.count,
                    median.containment,
                    median.cut,
                    median.total(),
                    median.candidates,
                    median.passing
                );
            }
        }
    }

    if let Some(path) = text("--costs-out") {
        let mut out = String::from("phase,seconds,resident_bytes\n");
        for (name, secs, bytes) in &costs {
            out.push_str(&format!("{name},{secs:.3},{bytes}\n"));
        }
        std::fs::write(path, out).expect("the build-cost record writes");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
