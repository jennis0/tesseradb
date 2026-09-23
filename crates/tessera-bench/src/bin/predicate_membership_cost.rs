//! **Stage 6's opening question: cache a predicate layer's row form, or derive it per request?**
//!
//! A predicate layer has no stored membership. `membership = { attribute = … }` says *the members
//! are the points whose column holds this key*, so the set exists as a posting list in entity space
//! and has to reach row space before a viewport can answer anything about it. Two ways, and the
//! owner asked which is better before any of it is written:
//!
//! - **A — cache the row form.** Project each artifact's membership into row space once and hold
//!   it, exactly as an enumerated layer's is held (`ArtifactRows`, rebuilt at a generation move).
//!   The per-request cost is then an enumerated layer's, and the predicate is invisible to the
//!   serving path.
//! - **B — derive per request.** Take the posting list as it stands and cross it into row space
//!   inside the request, which is what the filter surface's row route already does for a leaf.
//!   No storage, no invalidation rule, freshness by construction.
//!
//! The third option is the per-request predicate bound doing all the work — evaluate everything and
//! refuse a layer too large to serve — and it is not an alternative to either: a bound is about how
//! much a client gets back, and both of these are about what the server spends getting it. The
//! fourth, filtering candidates on geometry *before* evaluation, is a disclosure decision taken on
//! a stamp and is refused on the same grounds as
//! [decision 0041](../../../../docs/decisions/0041-pins-become-a-staleness-stamp.md).
//!
//! # What is measured, and what each arm is
//!
//! Three timings, at three layer sizes and two viewport widths:
//!
//! - **`A move`** — `ArtifactRows::build` over the layer: one `project_base` per artifact, decoding
//!   the whole membership. This is A's *whole* extra cost and it is paid **per generation move**,
//!   not per request.
//! - **`A request`** — the serving loop as it stands: per artifact, one candidacy test against the
//!   viewport and one masked count. Both arms pay this; it is here as the denominator, because a
//!   route whose extra cost is a fraction of the loop it rides on is free in practice.
//! - **`B request`** — one row-space walk over the viewport's domain, `entity_of` per row, and one
//!   probe per artifact per row. This is `per_tile_crossing_multi`'s shape — one inversion however
//!   many entity-space sets ride on it (decision 0062's one-crossing rule) — and it is paid **per
//!   request**.
//!
//! The arithmetic that decides is therefore not which number is smaller. It is `A move` against
//! `B request × (requests between generation moves)`, with `A request` as the scale both sit
//! against.
//!
//! **The fixture is synthetic and says so.** The permutation is a shuffled identity, which is the
//! scattered case Phase 0 measured real masks to be (results §5); memberships are scattered too,
//! because an attribute predicate's posting list is a value's carriers and not a range. What is not
//! modelled is a *spatially clustered* predicate — a boundary whose members share a region — which
//! would make B's domain smaller and A's projection no cheaper. That case favours B and is not
//! measured here.
//!
//! # Measured 2026-08-20, WSL2 on this host, release
//!
//! Mask = half the corpus visible. `N` is the layer's artifact count and the corpus is partitioned
//! across it, so members per artifact is `rows / N` — which is what an attribute predicate is: a
//! column's distinct values partition its carriers. **263 is the demo corpus's HDBSCAN layer**, the
//! number the owner named as the one to hold in mind. A `~` marks a `B request` scaled from
//! `B_SET_CAP` sets rather than measured whole.
//!
//! **10⁶ rows** — one member per artifact at the top of the table, which is the degenerate end and
//! is here to isolate the per-artifact term:
//!
//! | N | members | viewport | A move | A request | B request | C request | A held |
//! |---:|---:|---|---:|---:|---:|---:|---:|
//! | 64 | 15 625 | whole | 7.6 ms | 0.5 ms | 2 199 ms | 7.9 ms | 2 MB |
//! | **263** | 3 802 | **whole** | **13.9 ms** | **0.7 ms** | ~6 547 ms | 9.2 ms | 2 MB |
//! | 1 024 | 976 | whole | 29.8 ms | 1.3 ms | ~15 640 ms | 9.2 ms | 2 MB |
//! | 10 000 | 100 | whole | 220.5 ms | 6.7 ms | ~81 336 ms | 8.7 ms | 3 MB |
//! | 100 000 | 10 | whole | 1 525 ms | 39.3 ms | ~445 522 ms | 7.8 ms | 11 MB |
//! | **1 000 000** | 1 | **whole** | **6 665 ms** | **68.5 ms** | ~2 373 701 ms | **8.6 ms** | 18 MB |
//! | 1 000 000 | 1 | tenth | 6 665 ms | 30.2 ms | ~238 062 ms | 3.1 ms | 18 MB |
//!
//! **10⁷ rows**, the same layer sizes — which separates *a million artifacts costs this* from *a
//! million members costs this*, and carries the third arm:
//!
//! | N | members | viewport | A move | A request | B request | **C request** | A held |
//! |---:|---:|---|---:|---:|---:|---:|---:|
//! | 64 | 156 250 | whole | 113.6 ms | 6.5 ms | 55 688 ms | 145.6 ms | 20 MB |
//! | **263** | 38 022 | **whole** | **194.0 ms** | **10.4 ms** | ~126 059 ms | **170.3 ms** | 20 MB |
//! | 1 024 | 9 765 | whole | 335.0 ms | 22.0 ms | ~322 175 ms | 163.7 ms | 21 MB |
//! | 10 000 | 1 000 | whole | 1 672 ms | 106.3 ms | ~1 579 225 ms | 155.8 ms | 32 MB |
//! | 100 000 | 100 | whole | 10 017 ms | 572.6 ms | ~10 354 640 ms | 155.5 ms | 101 MB |
//! | **1 000 000** | 10 | **whole** | **15 571 ms** | **461.6 ms** | ~39 387 944 ms | **175.2 ms** | 108 MB |
//! | 1 000 000 | 10 | tenth | 15 571 ms | 133.7 ms | ~3 953 351 ms | 52.5 ms | 108 MB |
//!
//! ## C: the counts come from the column, and the artifact count stops mattering
//!
//! **C is flat in the artifact count.** 146–175 ms over the whole map at 10⁷ rows and 8–9 ms at
//! 10⁶, at every layer size from 64 artifacts to a million, because it never mentions the artifact
//! count. A single-valued attribute predicate *partitions* the corpus — the column's distinct values
//! are its artifacts and every point carries one — so the column already says which artifact each
//! point belongs to, and one pass answers **every** artifact at once.
//!
//! Its answers are asserted against A's rather than assumed equal — a fast wrong answer is worth
//! nothing — at a sampled thirty-second of the ordinals, for both the count and the candidacy.
//!
//! **Where the column actually lives, because the first draft of this arm assumed wrong.** Not the
//! render table: `membership = { attribute = … }` names an **indexed** column, and `attrs/` holds
//! it as a `tessera_filter::ValueColumn` — a dense typed code array plus a presence bitmap,
//! addressed by **entity**. That is what `FilterColumns::category_membership` already walks to
//! answer *which values can this principal see*. Entity space is also where the counting pass wants
//! to be: a masked count is `|membership ∩ M_auth|` and `M_auth` is entity-space, so that pass
//! projects nothing and crosses nothing.
//!
//! **It is two passes with different domains, and that is a disclosure rule rather than an
//! optimisation.** The count is over the whole membership (`annotations.md` §4.2: what this
//! principal can see of the artifact, not what is on screen) so its pass walks the mask; candidacy
//! is against the viewport, so that pass walks `viewport ∩ mask` — **and it is the expensive half**.
//! At 10⁷ rows a tenth-of-map request is 52 ms against the whole map's 175 ms, so roughly 120 ms of
//! C is the inversion, one `entity_of` per viewport row. The counting pass alone is ~30 ms.
//!
//! **So a route choice with a crossover, exactly like the filter surface's own — and the crossover
//! moves with the corpus.** At 10⁶ rows it sits near 10 000 artifacts and C wins 8× at a million;
//! at 10⁷ rows it sits near 30 000 and C wins **2.6×** at a million (175 ms against 462 ms). C is
//! flat in artifacts and *worse than linear* in corpus size — 8.6 ms to 175 ms for ten times the
//! rows, because at 10⁷ neither the code array nor the row→entity table fits in cache — while A is
//! linear in artifacts and indifferent to the corpus. Neither dominates; the choice is a latency
//! one with no disclosure content, since both compute the same quantities from inside `M_auth`.
//!
//! **The width dispatch matters and the width itself does not.** Hoisting the `Codes` match out of
//! the loop, which is what `ValueColumn`'s own scan does, is worth 264 ms → 170 ms at 10⁷ rows.
//! The declared width is not visible in these numbers — `u8` at 263 artifacts and `u32` at 1 024
//! measure the same — because the pass is bound by random access and by the inversion rather than
//! by the buffer. A million artifacts needs `u32` and gets it: `[[vocabulary]].width` is already a
//! required key taking `u8 | u16 | u32` (`configuration.md` §1), and `Codes` carries all three.
//!
//! ⊘ **Three reductions are modelled, not measured.** The counting pass is a function of `M_auth`
//! and the layer and **not of the request**, so it can be held per session on the mask fragment's
//! own cadence, leaving only candidacy per request. The candidacy pass's inversion is exactly what
//! a **rendered** copy of the column would remove — the render table is not needed for counting and
//! is precisely what would make candidacy cheap, which is the one thing the first draft of this arm
//! got right for the wrong reason. And with counts coming from C, row forms are needed only for the
//! artifacts actually **served**, whose number the budget bounds, so `A move` becomes a handful of
//! lazy projections rather than a million eager ones.
//!
//! ⊘ **C is for a single-valued *category* predicate and says nothing about the others.** A
//! multi-valued column does not partition, so its histogram costs one increment per value per row.
//! A **keyword** column's ordinals are per *layer* — the base and every extent mint their own — so
//! a histogram over one would count per layer and need each layer's dictionary to merge, which is a
//! real complication at the scale where a million distinct values is plausible. A **spatial**
//! predicate has no column at all, but its membership is Morton ranges and its masked count is a
//! `range_cardinality`, cheap per artifact without any of this.
//!
//! ## At a million artifacts the caching arm needs a second route, and B is not a candidate
//!
//! **A layer of a million predicate artifacts costs ~0.5 s per request over the whole map** on a
//! ten-million-point corpus, and 0.12 s over a tenth of it. That is the serving loop testing every
//! artifact — `intersects` then `masked_count` — and **the cut cannot reduce it**, because the cut
//! runs after the verdicts and so serves fewer artifacts while evaluating exactly as many.
//! Comfortable is a few thousand: 22 ms at 1 024, 106 ms at 10 000, and past that it dominates
//! whatever else the viewport is doing — **which is what C is for**, above the crossover.
//!
//! **The generation-move cost is the harder constraint: 15.5 s**, paid whenever the generation
//! moves rather than once. It is mostly a per-artifact term — a million `project_base` calls — so a
//! bigger corpus makes it worse only slowly (6.8 s at 10⁶ rows against 15.5 s at 10⁷, for ten times
//! the members), and it is the number to attack first if a layer this size is ever wanted.
//!
//! **Memory is not the problem at this scale and it is worth saying so**: 108 MB of row forms for a
//! million artifacts over ten million points, which is the one resource that scales the way a
//! reader expects.
//!
//! **B is out at every scale, and the gap widens with N** — 3 700× at 64 artifacts, 83 000× at a
//! million, where it is eleven hours per request. Its probe term is `rows × artifacts` and a layer
//! is exactly a collection of sets, so growing the layer is growing the thing it is linear in.
//!
//! **What this says about the per-request bound** (delivery §2, ⊘ unspecified): it has to be a
//! **refusal on the layer's declared artifact count**, checked before any evaluation. Not on the
//! principal's visible count — that requires the evaluation the bound exists to avoid, and a
//! refusal that varies by principal is a disclosure channel of its own. A total artifact count is
//! corpus-wide and identical for everyone, so refusing on it discloses nothing.
//!
//! **A, and not marginally.** The arithmetic set up above — `A move` against `B request` × requests
//! between generation moves — does not need doing: A's entire generation-move cost is repaid by
//! **one** request, at every size and both viewport widths. At the demo corpus's 263 artifacts over
//! the whole map, A costs 15 ms once per move and 0.9 ms per request where B costs 6.9 **seconds**
//! per request.
//!
//! **The reason is that B's cost is `rows × artifacts` and A's is `containers × artifacts`.** One
//! crossing serves every set (0062's rule holds — the inversion is paid once), but each set still
//! costs a probe per row, and a *layer* is exactly a collection of sets. The crossing's own
//! crossover — the ~5% at which the row route beats the entity route for a filter leaf — never
//! arrives here, because it is a statement about one set and this is a statement about N of them:
//! the tenth-of-the-map column is B's best case and it still loses by three orders of magnitude.
//!
//! **There is a third shape and it is the honest competitor**, not the crossing: derive per request
//! by *projecting* each posting list rather than crossing it — which is A's work, done per request
//! instead of per move. Its per-request cost is therefore the `A move` column, 15 ms at N = 263, or
//! ~15× A's per-request cost. It loses to A whenever a generation serves more than one request,
//! which is always, and it is the option to reach for if the caching invalidation rule ever turns
//! out to be the hard part.
//!
//! **`A request` is 3.5 µs per artifact at N = 263**, which is `annotation-representation.md` §2's
//! own microseconds-per-artifact figure arrived at independently — a sign the fixture is at a
//! realistic density rather than a flattering one. It falls to 69 ns at a million artifacts of one
//! member each, which is the floor: one container touched, twice.
//!
//! Run at a second corpus size by passing one: `… --bin predicate_membership_cost 10000000`.
//!
//! Run: `cargo run --release -p tessera-bench --bin predicate_membership_cost`

use std::sync::Arc;
use std::time::Instant;

use arrow::buffer::ScalarBuffer;
use croaring::{Bitmap, Portable};
use tessera_filter::{Codes, ValueColumn};
use tessera_engine::artifacts::ArtifactRows;
use tessera_engine::compose::MaskedSet;
use tessera_lifecycle::membership::ArtifactRecord;
use tessera_store::permutation::{Permutation, RowSpace};
use tessera_store::row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
use tessera_types::{EntityId, RowId};

/// Best-of, for the per-request arms. The move arm is single-shot: it is seconds at the top of the
/// table and a repeat would say the same thing more slowly.
const REPEATS: usize = 3;

/// How many of a layer's sets the B arm actually probes. Its probe term is linear in the set count,
/// so beyond this the figure is scaled rather than measured — see the call site.
const B_SET_CAP: usize = 64;

/// A composed mask stripped to what both arms ask of one.
///
/// Neither arm is about composition — an `EffectiveMask` layers a geometry projection, an overlay
/// and a deny mask over the same Roaring arithmetic — so a plain bitmap keeps the measurement on
/// the operation being compared rather than on the composition both would pay identically.
struct PlainMask(Bitmap);

impl MaskedSet for PlainMask {
    fn count_intersection(&self, set: &Bitmap) -> u64 {
        self.0.and_cardinality(set)
    }

    fn intersects_set(&self, set: &Bitmap) -> bool {
        self.0.intersect(set)
    }

    fn visible_rows(&self, set: &Bitmap) -> Bitmap {
        self.0.and(set)
    }
}

/// A deterministic scatter — a linear congruential step over `bound`, which visits every value once
/// when the multiplier is coprime with it. No `rand` dependency, and reproducible between runs.
fn scatter(bound: u32, seed: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(bound as usize);
    let mut x = seed % bound;
    let step = 2_654_435_761u64 % bound as u64;
    for _ in 0..bound {
        out.push(x);
        x = ((x as u64 + step) % bound as u64) as u32;
    }
    out
}

/// A row space over `rows` entities, permuted rather than the identity: an entity's row is
/// unrelated to its id, which is what makes `entity_of` a real lookup rather than a cast.
fn row_space(dir: &std::path::Path, rows: u32) -> RowSpace {
    let row_order = scatter(rows, 7);
    let perm_path = dir.join("permutation.bin");
    let entities: Vec<EntityId> = row_order.iter().map(|&e| EntityId::new(e as u64)).collect();
    tessera_store::write::write_permutation(&perm_path, &entities, rows as u64)
        .expect("permutation writes");
    let table_path = dir.join(ROW_ENTITY_FILE);
    write_row_entity(&table_path, &row_order).expect("row-entity table writes");
    RowSpace::new(
        Arc::new(Permutation::load(&perm_path).expect("permutation loads")),
        rows,
    )
    .with_row_entity(Arc::new(
        RowToEntity::load(&table_path).expect("row-entity table loads"),
    ))
}

/// `n` memberships partitioning entity space, each scattered across it — an attribute predicate's
/// posting lists, which are a value's carriers and never a range.
fn memberships(rows: u32, n: usize) -> Vec<Bitmap> {
    let mut sets = vec![Bitmap::new(); n];
    for (i, entity) in scatter(rows, 13).into_iter().enumerate() {
        sets[i % n].add(entity);
    }
    for set in &mut sets {
        set.run_optimize();
    }
    sets
}

/// The predicate's own column, **as the engine already stores it**: a `ValueColumn` over the
/// artifact each entity belongs to.
///
/// **Not a new structure, and not the render table.** A single-valued attribute predicate
/// partitions the corpus — a column's distinct values are its artifacts and every point carries
/// one — and `membership = { attribute = … }` names a column the build has indexed, so `attrs/`
/// already holds exactly this: a dense typed code array plus a presence bitmap, addressed by entity
/// (`tessera_filter::ValueColumn`). It is what `FilterColumns::category_membership` already walks
/// to answer *which values can this principal see*; the histogram below accumulates counts over the
/// same walk.
///
/// **Entity space, which is where the counting pass wants to be anyway.** A masked count is
/// `|membership ∩ M_auth|` and `M_auth` is entity-space, so nothing is projected and nothing is
/// crossed. Universal presence, because a partitioning predicate is exactly the case where every
/// point carries a value — which is also the case `slot_of` answers with a bare index rather than a
/// rank.
///
/// The width is the vocabulary's declared one (`configuration.md` §1: `u8 | u16 | u32`), so it is a
/// real variable rather than a caveat: a layer of a million artifacts needs `u32` and one of a few
/// hundred needs `u8`.
fn value_column(rows: u32, n: usize, wide: bool) -> ValueColumn {
    let mut of_entity = vec![0u32; rows as usize];
    for (i, entity) in scatter(rows, 13).into_iter().enumerate() {
        of_entity[entity as usize] = (i % n) as u32;
    }
    let codes = if wide {
        Codes::U32(ScalarBuffer::from(of_entity))
    } else {
        Codes::U8(ScalarBuffer::from(
            of_entity.iter().map(|&c| c as u8).collect::<Vec<u8>>(),
        ))
    };
    ValueColumn::universal(codes)
}

fn records(sets: &[Bitmap]) -> Vec<ArtifactRecord> {
    sets.iter()
        .enumerate()
        .map(|(i, members)| ArtifactRecord {
            entity: EntityId::new(i as u64),
            key: None,
            view: None,
            members: members.clone().into(),
            contents: Vec::new(),
            attached_to: None,
            parents: Vec::new(),
            access: Vec::new(),
        })
        .collect()
}

fn main() {
    // The house idiom for these binaries: a process-named directory under the system temp, removed
    // on the way out rather than left for the next run to reuse by accident.
    let dir = std::env::temp_dir().join(format!("tessera-predicate-cost-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");
    // The corpus size, so the same layer sizes can be run against two of them — which is what
    // separates "a million artifacts costs this" from "a million members costs this".
    let rows: u32 = std::env::args()
        .nth(1)
        .map(|a| a.parse().expect("a row count"))
        .unwrap_or(1_000_000);
    let space = row_space(&dir, rows);

    // Half the corpus visible: a mask dense enough that a count touches every container, which is
    // the expensive side of Roaring's O(containers touched) rather than the flattering one.
    let mut mask = Bitmap::new();
    for row in (0..rows).step_by(2) {
        mask.add(row);
    }
    mask.run_optimize();
    // The same set in entity space, which is where `M_auth` and the value column both live. In the
    // engine this is the composed mask itself and the row-space one is its projection; here the two
    // are derived from each other so the arms are comparing the same principal.
    let mut mask_entities = Bitmap::new();
    for row in mask.iter() {
        if let Some(e) = space.entity_of(RowId::new(row)) {
            mask_entities.add(e.raw() as u32);
        }
    }
    mask_entities.run_optimize();
    let mask = PlainMask(mask);

    println!("rows = {rows}, mask = half of them");
    println!(
        "{:>9} {:>8} {:>10} {:>12} {:>12} {:>14} {:>12} {:>12} {:>9}",
        "N", "members", "viewport", "A move", "A request", "B request", "B/A request",
        "C request", "A held"
    );

    for n in [64usize, 263, 1024, 10_000, 100_000, 1_000_000] {
        let sets = memberships(rows, n);
        let recs = records(&sets);

        // **A's whole extra cost, and it is paid at a generation move.** `ArtifactRows::build` is
        // one `project_base` per artifact, and `project_base` decodes the whole membership — the
        // same call an enumerated layer already pays at open and at a move.
        let started = Instant::now();
        let row_forms = ArtifactRows::build(
            recs.iter().enumerate().map(|(i, r)| (i as u32, r)),
            &space,
        );
        let a_move = started.elapsed();

        // What A holds between moves, which is the other half of its price and becomes the live
        // question long before the timings do. Serialised size stands for resident size: it is the
        // container payload without the allocator's own overhead, so it is the floor rather than
        // the figure.
        let held: u64 = (0..n as u32)
            .filter_map(|o| row_forms.get(o))
            .map(|b| b.get_serialized_size_in_bytes::<Portable>() as u64)
            .sum();

        // The column at its real declared width: `u8` while the artifact count fits it, `u32`
        // beyond — which a layer of a million artifacts needs, so at the bottom of this table the
        // wide arm is the honest one rather than a pessimism.
        let column = value_column(rows, n, n > u8::MAX as usize + 1);

        for (label, span) in [("whole", rows), ("tenth", rows / 10)] {
            let mut tile_rows = Bitmap::new();
            tile_rows.add_range(0..span);
            tile_rows.run_optimize();

            // **The loop both arms ride on**: candidacy against the viewport, then the masked
            // count that the criterion tests and the response carries.
            let mut a_request = std::time::Duration::MAX;
            for _ in 0..REPEATS {
                let started = Instant::now();
                let mut served = 0u64;
                for ordinal in 0..n as u32 {
                    if row_forms.intersects(ordinal, &tile_rows, &mask) {
                        served += row_forms.masked_count(ordinal, &mask);
                    }
                }
                a_request = a_request.min(started.elapsed());
                std::hint::black_box(served);
            }

            // **B's shape: one inversion, N probes per row.** The domain is the viewport's rows;
            // each is inverted once however many entity-space sets ride on it, and each set costs
            // one probe on top (`per_tile_crossing_multi`'s own cost note).
            //
            // **Capped, and the cap is why the large-N figures are modelled.** At a million
            // artifacts this arm is 10¹² probes, which is hours per cell. The probe term is exactly
            // linear in the set count — that is the whole of what the cost note claims — so it is
            // measured over `B_SET_CAP` sets and scaled, with the inversion (which is paid once
            // whatever the set count) held out of the scaling. A row marked `~` is that.
            let probed = sets.len().min(B_SET_CAP);
            let started = Instant::now();
            let mut hits = 0u64;
            let mut inversions = 0u64;
            for row in 0..span {
                let Some(entity) = space.entity_of(RowId::new(row)) else {
                    continue;
                };
                inversions += 1;
                let raw = entity.raw() as u32;
                for set in &sets[..probed] {
                    if set.contains(raw) {
                        hits += 1;
                    }
                }
            }
            let b_capped = started.elapsed();
            std::hint::black_box(hits);
            std::hint::black_box(inversions);

            // The inversion, measured on its own so the probe term can be scaled without carrying
            // it along — it is paid once per row however many sets ride on the walk.
            let started = Instant::now();
            let mut only_inversions = 0u64;
            for row in 0..span {
                if space.entity_of(RowId::new(row)).is_some() {
                    only_inversions += 1;
                }
            }
            let b_walk = started.elapsed();
            std::hint::black_box(only_inversions);

            let probe_term = (b_capped.as_secs_f64() - b_walk.as_secs_f64()).max(0.0);
            let b_request =
                b_walk.as_secs_f64() + probe_term * (n as f64 / probed as f64);
            let modelled = if probed < n { "~" } else { " " };

            // **C — the same answers from one pass over the column, whatever the artifact count.**
            //
            // Two walks, because the two questions have different domains and that is a disclosure
            // rule rather than an optimisation: a masked count is `|membership ∩ M_auth|` over the
            // **whole** membership (`annotations.md` §4.2 — what this principal can see of the
            // artifact, not what is on screen), while candidacy is against the viewport.
            //
            // **The counting pass is entity-space and touches no row at all** — `M_auth` is entity
            // space and so is `attrs/`'s value column, so there is nothing to project and nothing
            // to cross. The candidacy pass is the one that needs row space, and it needs exactly
            // one inversion per viewport row — decision 0062's one crossing, used to read *one*
            // code per row rather than to probe `n` sets.
            //
            // **The width dispatch is hoisted out of the loop**, which is not a liberty — it is
            // what `ValueColumn`'s own scan does (`ScanWork`), and it is the difference between
            // reading a typed slice and matching an enum per element. Left inside, this arm
            // measures the `match` rather than the read: 264 ms against 44 ms at 10⁷ rows,
            // measured, which is most of the arm.
            let visible_here = tile_rows.and(&mask.0);
            let mut counts = vec![0u32; n];
            let mut here = vec![false; n];
            let mut c_request = std::time::Duration::MAX;
            for _ in 0..REPEATS {
                counts.iter_mut().for_each(|c| *c = 0);
                here.iter_mut().for_each(|h| *h = false);
                let started = Instant::now();
                macro_rules! histogram {
                    ($codes:expr) => {{
                        let codes = $codes;
                        for entity in mask_entities.iter() {
                            counts[codes[entity as usize] as usize] += 1;
                        }
                        for row in visible_here.iter() {
                            if let Some(e) = space.entity_of(RowId::new(row)) {
                                here[codes[e.raw() as usize] as usize] = true;
                            }
                        }
                    }};
                }
                match column.codes() {
                    Codes::U8(v) => histogram!(v.as_ref()),
                    Codes::U32(v) => histogram!(v.as_ref()),
                    _ => unreachable!("this fixture writes only the two declared widths"),
                }
                c_request = c_request.min(started.elapsed());
            }
            std::hint::black_box(&counts);
            std::hint::black_box(&here);

            // A fast wrong answer is worth nothing, so the two arms are compared rather than
            // assumed equal: every artifact's histogram count must be its masked count, and its
            // histogram flag its candidacy.
            for ordinal in (0..n as u32).step_by((n / 32).max(1)) {
                assert_eq!(
                    counts[ordinal as usize] as u64,
                    row_forms.masked_count(ordinal, &mask),
                    "the histogram and the per-artifact count disagree at ordinal {ordinal}"
                );
                assert_eq!(
                    here[ordinal as usize],
                    row_forms.intersects(ordinal, &tile_rows, &mask),
                    "the histogram and the per-artifact candidacy disagree at ordinal {ordinal}"
                );
            }

            let ratio = b_request / a_request.as_secs_f64();
            println!(
                "{:>9} {:>8} {:>10} {:>10.1}ms {:>10.1}ms {}{:>11.1}ms {:>11.0}x {:>10.1}ms {:>7.0}MB",
                n,
                rows as usize / n,
                label,
                a_move.as_secs_f64() * 1e3,
                a_request.as_secs_f64() * 1e3,
                modelled,
                b_request * 1e3,
                ratio,
                c_request.as_secs_f64() * 1e3,
                held as f64 / 1e6,
            );
        }
        drop(row_forms);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
