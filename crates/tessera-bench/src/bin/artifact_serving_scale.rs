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
use tessera_store::permutation::{Permutation, RowSpace};
use tessera_store::row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
use tessera_spatial::morton::{tiles_for_bbox, Bounds};
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
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Runs => "runs",
            Arm::Regions => "regions",
            Arm::Scattered => "scattered",
            Arm::Partition => "partition",
        }
    }

    fn parse(name: &str) -> Option<Arm> {
        match name {
            "runs" => Some(Arm::Runs),
            "regions" => Some(Arm::Regions),
            "scattered" => Some(Arm::Scattered),
            "partition" => Some(Arm::Partition),
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
fn mask(rows: u32, row_order: &[u32], groups: u32, group_size: u32, rng: &mut Rng) -> ComposedMask {
    let mut base = Bitmap::new();
    let ceiling = groups.saturating_mul(group_size);
    for (row, &entity) in row_order.iter().enumerate() {
        if entity < ceiling {
            base.add(row as u32);
        }
    }
    base.run_optimize();

    // The deny lane: a few thousand suppressed entities, which is the order a real overlay carries.
    let mut minus = Bitmap::new();
    for _ in 0..4096 {
        minus.add(rng.below(rows as u64) as u32);
    }
    minus.and_inplace(&base);

    let mut plus = Bitmap::new();
    for _ in 0..1024 {
        plus.add(rng.below(rows as u64) as u32);
    }
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
fn membership_rows(
    arm: Arm,
    row_count: u32,
    artifact: usize,
    artifacts: usize,
    members: u32,
    runs: u32,
    rng: &mut Rng,
) -> Bitmap {
    let mut bitmap = Bitmap::new();
    match arm {
        Arm::Runs => {
            // The stretch this artifact owns, and a local window a few times wider so neighbours
            // interleave at their edges as real clusters do.
            let stride = (row_count as u64 / artifacts.max(1) as u64).max(1);
            let base = (artifact as u64 * stride) as u32;
            let window = (stride * 4).min(row_count as u64) as u32;
            let per_run = (members / runs.max(1)).max(1);
            for _ in 0..runs.max(1) {
                let offset = rng.below(window.max(1) as u64) as u32;
                let start = base.saturating_add(offset).min(row_count - per_run.min(row_count));
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

/// A row space whose permutation is the one the build actually produces.
///
/// Row *r*'s entity is `base[g] + rank of r within group g`, where *g* is *r*'s signature group.
/// Signature and map position are uncorrelated — what a document is *about* does not follow from
/// where it sits — so the group is drawn from a hash of the row rather than from its neighbourhood,
/// and the consequence is the one that matters for every measurement below: **an artifact that is
/// contiguous in row space is 32 runs in entity space, and a principal's visible set is contiguous
/// in entity space and scattered in row space.** The two directions are not symmetric and a fixture
/// that shuffled would have neither.
fn row_space(dir: &std::path::Path, rows: u32) -> (RowSpace, Vec<u32>) {
    // Pass one: how big is each group.
    let group_of = |row: u32| -> u32 {
        let mut z = (row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z ^= z >> 29;
        (z % SIGNATURE_GROUPS as u64) as u32
    };
    let mut sizes = vec![0u32; SIGNATURE_GROUPS as usize];
    for row in 0..rows {
        sizes[group_of(row) as usize] += 1;
    }
    let mut base = vec![0u32; SIGNATURE_GROUPS as usize];
    let mut running = 0u32;
    for (g, size) in sizes.iter().enumerate() {
        base[g] = running;
        running += size;
    }

    // Pass two: the assignment itself.
    let mut cursor = base.clone();
    let mut row_order: Vec<u32> = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let g = group_of(row) as usize;
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
    (space, row_order)
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
    let served = tessera_engine::cut::cut(lineage, candidates, Some(1_000), true);
    (start.elapsed().as_secs_f64() * 1e6, served.len())
}

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
        candidates.par_iter().map(|&o| rows.masked_count(o, mask)).sum()
    } else {
        candidates.iter().map(|&o| rows.masked_count(o, mask)).sum()
    };
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    let start = Instant::now();
    phases.passing = if parallel {
        candidates.par_iter().filter(|&&o| contained(rows, o, mask)).count()
    } else {
        candidates.iter().filter(|&&o| contained(rows, o, mask)).count()
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
        (0..rows.len() as u32).into_par_iter().filter(|&o| candidate(o)).collect()
    } else {
        (0..rows.len() as u32).filter(|&o| candidate(o)).collect()
    };
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = if parallel {
        candidates.par_iter().map(|&o| rows.masked_count(o, mask)).sum()
    } else {
        candidates.iter().map(|&o| rows.masked_count(o, mask)).sum()
    };
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    let start = Instant::now();
    phases.passing = if parallel {
        candidates.par_iter().filter(|&&o| contained(rows, o, mask)).count()
    } else {
        candidates.iter().filter(|&&o| contained(rows, o, mask)).count()
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
        shortlist.par_iter().filter(|&&o| rows.intersects(o, tiles, mask)).copied().collect()
    } else {
        shortlist.iter().filter(|&&o| rows.intersects(o, tiles, mask)).copied().collect()
    };
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = if parallel {
        candidates.par_iter().map(|&o| rows.masked_count(o, mask)).sum()
    } else {
        candidates.iter().map(|&o| rows.masked_count(o, mask)).sum()
    };
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    let start = Instant::now();
    phases.passing = if parallel {
        candidates.par_iter().filter(|&&o| contained(rows, o, mask)).count()
    } else {
        candidates.iter().filter(|&&o| contained(rows, o, mask)).count()
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
        shortlist.par_iter().filter(|&&o| rows.intersects(o, tiles, mask)).copied().collect()
    } else {
        shortlist.iter().filter(|&&o| rows.intersects(o, tiles, mask)).copied().collect()
    };
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    // The count is a lookup, not an intersection.
    let start = Instant::now();
    let total: u64 = candidates.iter().map(|&o| session.counts[o as usize] as u64).sum();
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
fn assert_same_answer(
    rows: &ArtifactRows,
    index: &TileIndex,
    tiles: &Bitmap,
    mask: &ComposedMask,
    row_count: u32,
) {
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
    let total: u64 = candidates.iter().map(|&o| session.counts[o as usize] as u64).sum();
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
    let total: u64 = candidates.iter().map(|&o| session.counts[o as usize] as u64).sum();
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
    /// Per distinct expression, the ordinals whose containment it decides.
    ///
    /// Held for the wide case, where taking the union once beats testing each candidate.
    groups: Vec<Bitmap>,
    /// Per ordinal, which expression decides it — so a **narrow** request never touches the
    /// population at all. Unioning thirty-two bitmaps over ten million ordinals is
    /// `O(containers)`, which is ~6 ms whatever the viewport; a byte lookup per candidate is
    /// `O(candidates)`, which at a 0.024% viewport is three hundred of them.
    ///
    /// One byte per artifact. The two forms are the same fact, and the route picks between them on
    /// the size of the candidate set exactly as §5's two layouts do.
    of_ordinal: Vec<u8>,
}

impl ContainmentGroups {
    /// Built from the population: each artifact's generating set lies inside one signature group by
    /// construction, so the expression it composes to is that group's own term.
    fn build(rows: &ArtifactRows, artifacts: usize) -> Self {
        let mut groups = vec![Bitmap::new(); SIGNATURE_GROUPS as usize];
        for ordinal in 0..artifacts as u32 {
            // The fixture draws each generating set from `ordinal % SIGNATURE_GROUPS`.
            groups[(ordinal % SIGNATURE_GROUPS) as usize].add(ordinal);
        }
        for g in &mut groups {
            g.run_optimize();
        }
        let of_ordinal = (0..artifacts as u32)
            .map(|ordinal| (ordinal % SIGNATURE_GROUPS) as u8)
            .collect();
        let _ = rows;
        ContainmentGroups { groups, of_ordinal }
    }

    /// The artifacts whose containment this principal satisfies, as a set — the wide route.
    fn satisfied(&self, holds: u32) -> Bitmap {
        let lists: Vec<&Bitmap> = self.groups.iter().take(holds as usize).collect();
        Bitmap::fast_or(&lists)
    }

    /// Whether this one artifact's containment holds — the narrow route.
    fn holds_for(&self, ordinal: u32, holds: u32) -> bool {
        (self.of_ordinal[ordinal as usize] as u32) < holds
    }

    fn resident_bytes(&self) -> u64 {
        self.groups
            .iter()
            .map(|g| g.get_serialized_size_in_bytes::<croaring::Portable>() as u64)
            .sum::<u64>()
            + self.of_ordinal.len() as u64
    }
}

/// **Route G — the settled route with containment resolved from the build-time partition.**
///
/// The per-token structure is gone: what stands in its place is one union of a handful of
/// build-time bitmaps, priced here as part of the request that uses it.
fn grouped(held: &Held, holds: u32, tiles: &Bitmap, mask: &ComposedMask) -> Phases {
    let (rows, index, contains, lineage, row_count) = (
        held.rows,
        held.index,
        held.contains,
        held.lineage,
        held.row_count,
    );
    let viewport_rows = tiles.cardinality();
    let mut phases = Phases::default();

    let start = Instant::now();
    let (settled, open) = index.settled_and_open(tiles, row_count);
    // **Which way round to ask depends on how much the viewport left.** Unioning the satisfied
    // groups is `O(containers)` and independent of the viewport; testing each candidate is
    // `O(candidates)`. The viewport walk has already said which is smaller.
    let wide = settled.cardinality() + open.cardinality() > (row_count as u64 / 64).max(4096);
    let mut passing = if wide {
        let passes = contains.satisfied(holds);
        let mut s = settled;
        s.and_inplace(&passes);
        let mut o = open;
        o.and_inplace(&passes);
        for x in o.iter() {
            if index.inside(x, tiles, viewport_rows) || rows.intersects(x, tiles, mask) {
                s.add(x);
            }
        }
        s
    } else {
        let mut s: Bitmap = settled.iter().filter(|&o| contains.holds_for(o, holds)).collect();
        for x in open.iter() {
            if contains.holds_for(x, holds)
                && (index.inside(x, tiles, viewport_rows) || rows.intersects(x, tiles, mask))
            {
                s.add(x);
            }
        }
        s
    };
    passing.run_optimize();
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
        .take(1000)
        .map(|&o| rows.masked_count(o, mask))
        .sum();
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);

    phases.containment = 0.0;
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
fn hoisted(held: &Held, holds: u32, tiles: &Bitmap, mask: &ComposedMask) -> Phases {
    let (rows, index, contains, lineage, row_count) = (
        held.rows,
        held.index,
        held.contains,
        held.lineage,
        held.row_count,
    );
    let viewport_rows = tiles.cardinality();
    let mut phases = Phases::default();
    let start = Instant::now();
    let (settled, open) = index.settled_and_open(tiles, row_count);
    // The one composition. Its cost is the viewport's containers, not the population's.
    let here = mask.visible_rows(tiles);
    let mut passing: Bitmap = settled.iter().filter(|&o| contains.holds_for(o, holds)).collect();
    for x in open.iter() {
        if !contains.holds_for(x, holds) {
            continue;
        }
        if index.inside(x, tiles, viewport_rows) {
            passing.add(x);
            continue;
        }
        if rows.get(x).is_some_and(|m| m.intersect(&here)) {
            passing.add(x);
        }
    }
    passing.run_optimize();
    let candidates: Vec<u32> = passing.iter().collect();
    phases.candidacy = start.elapsed().as_secs_f64() * 1e6;
    phases.candidates = candidates.len();

    let start = Instant::now();
    let total: u64 = candidates
        .iter()
        .take(1000)
        .map(|&o| rows.masked_count(o, mask))
        .sum();
    phases.count = start.elapsed().as_secs_f64() * 1e6;
    std::hint::black_box(total);
    phases.containment = 0.0;
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
            let (from, to) = (self.at[row as usize] as usize, self.at[row as usize + 1] as usize);
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
    passing.and_inplace(&contains.satisfied(holds));
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
    let rows_n: u32 = flag("--rows", 100_000_000) as u32;
    let artifacts: usize = flag("--artifacts", 1_000_000) as usize;
    let members: u32 = flag("--members", 100) as u32;
    let runs_per: u32 = flag("--runs", 4) as u32;
    let arm = args
        .iter()
        .position(|a| a == "--arm")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| Arm::parse(n))
        .unwrap_or(Arm::Runs);

    eprintln!(
        "# rows={rows_n} artifacts={artifacts} members={members} runs={runs_per} arm={}",
        arm.name()
    );

    let mut rng = Rng(0x5EED);
    let t = Instant::now();
    let (space, row_order) = row_space(&dir, rows_n);
    let group_size = rows_n / SIGNATURE_GROUPS;
    eprintln!("# row space: {:.1} s", t.elapsed().as_secs_f64());

    let t = Instant::now();
    let mut row_span_total = 0u64;
    let records: Vec<ArtifactRecord> = (0..artifacts)
        .map(|i| {
            let in_rows = membership_rows(arm, rows_n, i, artifacts, members, runs_per, &mut rng);
            row_span_total += in_rows.statistics().n_containers as u64;
            // One content, whose generating set is a handful of the artifact's own members: the
            // One content, whose generating set is a handful of the artifact's own members: the
            // prompt sample a summariser was shown, not the whole cluster.
            //
            // **Drawn from inside one signature group, which is `annotations.md` §7.8's own
            // mitigation and not a convenience.** Containment is `|G ∩ M| == |G|` and it "is not a
            // coverage fraction" — *what decides is **which** terms, never how many items*. A
            // sample scattered across thirty-two signature groups needs a principal holding all
            // thirty-two, so no narrower principal is ever served any label at all, and every arm
            // below 100% would be timing a layer that serves nothing. A per-term generating set is
            // what the design says to build instead, and with one the label reaches exactly the
            // principals holding that term — which is the behaviour worth measuring.
            let group = (i as u32) % SIGNATURE_GROUPS;
            let mut sample_rows: Bitmap = in_rows
                .iter()
                .filter(|&row| {
                    space
                        .entity_of(RowId::new(row))
                        .is_some_and(|e| (e.raw() as u32) / group_size.max(1) == group)
                })
                .take(8)
                .collect();
            // **Never empty.** An empty generating set is *corpus-independent* and therefore
            // vacuously contained — served to everyone who reaches the layer — so a fixture that
            // let one fall out would measure containment passing universally and call it a
            // measurement of containment. A small artifact may hold no member of its own signature
            // group, and then the sample is drawn from whatever it does hold; the expression that
            // decides it is that member's, which is exactly what the grouping below reads.
            if sample_rows.is_empty() {
                sample_rows = in_rows.iter().take(1).collect();
            }
            ArtifactRecord {
                entity: EntityId::new(i as u64),
                key: None,
                members: to_entities(&in_rows, &space),
                contents: vec![ContentSet {
                    values: Some(vec!["a label".to_string()]),
                    generated_from: to_entities(&sample_rows, &space),
                }],
                attached_to: None,
                parent: None,
            }
        })
        .collect();
    eprintln!(
        "# entity-space records: {:.1} s, {:.1} row blocks per artifact",
        t.elapsed().as_secs_f64(),
        row_span_total as f64 / artifacts as f64
    );

    let t = Instant::now();
    let row_forms = ArtifactRows::build(
        records.iter().enumerate().map(|(i, r)| (i as u32, r)),
        &space,
    );
    eprintln!(
        "# projection (the generation move): {:.1} s",
        t.elapsed().as_secs_f64()
    );
    drop(records);

    let t = Instant::now();
    let column = LabelColumn::build(&row_forms, rows_n);
    if let Some(c) = &column {
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
    let contains = ContainmentGroups::build(&row_forms, artifacts);
    eprintln!(
        "# containment groups: {} distinct, {:.1} s, {:.1} MB serialised",
        SIGNATURE_GROUPS,
        t.elapsed().as_secs_f64(),
        contains.resident_bytes() as f64 / 1e6
    );

    let t = Instant::now();
    let index = TileIndex::build(&row_forms, rows_n);
    eprintln!(
        "# tile index: {:.1} s to build, {:.1} MB serialised",
        t.elapsed().as_secs_f64(),
        index.resident_bytes() as f64 / 1e6
    );

    // **A three-way tree over the layer**, so the cut has a frontier to resolve rather than a flat
    // level it short-circuits. Held once, as a generation object is: `Lineages` caches it in the
    // engine now, and a request that rebuilt it would be timing a build rather than a cut.
    //
    // ⊘ **This tree is not a hierarchy, and every treed figure here is suspect because of it.** The
    // parent of ordinal *o* is `(o - 1) / 3`, so the tree's shape is the ordinal space's and has no
    // relation to the geometry — while each artifact's membership sits near its *own* ordinal.
    // A real nested layer is the opposite: a parent's membership **contains** its children's, so a
    // parent is in view whenever any child is, and the root is in view always.
    //
    // The consequence is measurable and was measured. At a three-quarter viewport the artifacts near
    // ordinal zero — which here are the whole top of the tree — fall out of view, so no node above
    // the cut passes, every lineage below is its own fallback, and the cut pays for a shape a real
    // hierarchy cannot have. It is why the campaign's ridge at a three-quarter viewport is
    // **unexplained rather than established**: part of it is this.
    //
    // Fixing it means assigning membership from the tree rather than the tree from the ordinals —
    // a parent's rows being the union of its children's — which also changes what a level costs to
    // store, since every level then covers the corpus.
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

    println!("arm,rows,artifacts,mask_pct,viewport_pct,depth,route,setup_ms,candidacy_us,count_us,containment_us,cut_us,total_us,candidates,served");

    // Whole signature groups, so the mask percentages are the ones a principal can actually have.
    // **Swept through the middle, not around it.** Three densities — everything, a tenth, a
    // thirtieth — miss the band where the cut is dearest: enough artifacts pass to make the sweep
    // expensive, not enough for the downward walk's guard to hold. That band is somewhere between
    // them, and a table sampling only the ends reports the wrong worst case.
    for groups in [SIGNATURE_GROUPS, 24, 16, 8, 3, 1] {
        let mask_pct = 100.0 * groups as f64 / SIGNATURE_GROUPS as f64;
        let m = mask(rows_n, &row_order, groups, group_size, &mut rng);

        let t = Instant::now();
        let session = SessionVerdicts::build(&row_forms, &m, 1);
        let session_ms = t.elapsed().as_secs_f64() * 1e3;
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
            assert_same_answer(&row_forms, &index, &tiles, &m, rows_n);

            let routes: Vec<Route> = vec![
                ("shipped", 0.0, Box::new(|| shipped(&row_forms, &tiles, &m, false))),
                ("shipped+par", 0.0, Box::new(|| shipped(&row_forms, &tiles, &m, true))),
                ("early", 0.0, Box::new(|| early_exit(&row_forms, &tiles, &m, false))),
                ("early+par", 0.0, Box::new(|| early_exit(&row_forms, &tiles, &m, true))),
                ("index", 0.0, Box::new(|| indexed(&row_forms, &index, &tiles, &m, rows_n, false))),
                ("index+par", 0.0, Box::new(|| indexed(&row_forms, &index, &tiles, &m, rows_n, true))),
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
                    Box::new(|| {
                        grouped(&held, groups, &tiles, &m)
                    }),
                ),

                (
                    "hoisted",
                    0.0,
                    Box::new(|| {
                        hoisted(&held, groups, &tiles, &m)
                    }),
                ),
            ];

            let mut routes = routes;
            if matches!(arm, Arm::Scattered | Arm::Partition) {
                routes.push((
                    "listed",
                    0.0,
                    Box::new(|| listed(&held, groups, &tiles, &m)),
                ));
            }
            if matches!(arm, Arm::Scattered | Arm::Partition) {
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

            // **`--only` keeps the legacy routes out of a run that cannot afford them.** The
            // shipped loop is `O(artifacts)` with a masked intersection apiece, which at ten million
            // scattered artifacts is ~24 s a call — hours across the sweep, to re-measure a figure
            // three smaller scales already establish. Which routes run does not change what any of
            // them answers: each builds its own result from the same held state.
            let only: Option<Vec<&str>> = args
                .iter()
                .position(|a| a == "--only")
                .and_then(|i| args.get(i + 1))
                .map(|v| v.split(',').collect());
            for (route, setup_ms, run) in routes {
                if only.as_ref().is_some_and(|keep| !keep.contains(&route)) {
                    continue;
                }
                let mut best = Phases::default();
                let mut best_total = f64::MAX;
                for _ in 0..3 {
                    let p = run();
                    if p.total() < best_total {
                        best_total = p.total();
                        best = p;
                    }
                }
                println!(
                    "{},{rows_n},{artifacts},{mask_pct},{viewport_pct},{depth},{route},{setup_ms:.0},{:.0},{:.0},{:.0},{:.0},{:.0},{},{}",
                    arm.name(),
                    best.candidacy,
                    best.count,
                    best.containment,
                    best.cut,
                    best_total,
                    best.candidates,
                    best.passing
                );
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}
