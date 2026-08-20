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
//! | N | members | viewport | A move | A request | B request | A held |
//! |---:|---:|---|---:|---:|---:|---:|
//! | 64 | 15 625 | whole | 7.4 ms | 0.6 ms | 2 261 ms | 2 MB |
//! | **263** | 3 802 | **whole** | **15.5 ms** | **0.9 ms** | ~6 933 ms | 2 MB |
//! | 1 024 | 976 | whole | 28.6 ms | 1.4 ms | ~15 996 ms | 2 MB |
//! | 10 000 | 100 | whole | 221.8 ms | 6.7 ms | ~79 540 ms | 3 MB |
//! | 100 000 | 10 | whole | 1 566 ms | 39.7 ms | ~435 520 ms | 11 MB |
//! | **1 000 000** | 1 | **whole** | **6 804 ms** | **69.4 ms** | ~2 247 387 ms | 18 MB |
//! | 1 000 000 | 1 | tenth | 6 804 ms | 30.8 ms | ~228 609 ms | 18 MB |
//!
//! **10⁷ rows**, the same layer sizes — which separates *a million artifacts costs this* from *a
//! million members costs this*:
//!
//! | N | members | viewport | A move | A request | B request | A held |
//! |---:|---:|---|---:|---:|---:|---:|
//! | 64 | 156 250 | whole | 113.5 ms | 6.6 ms | 56 570 ms | 20 MB |
//! | **263** | 38 022 | **whole** | **200.6 ms** | **10.3 ms** | ~128 078 ms | 20 MB |
//! | 1 024 | 9 765 | whole | 334.7 ms | 24.8 ms | ~361 935 ms | 21 MB |
//! | 10 000 | 1 000 | whole | 1 704 ms | 109.1 ms | ~1 614 102 ms | 32 MB |
//! | 100 000 | 100 | whole | 10 337 ms | 615.2 ms | ~10 189 715 ms | 101 MB |
//! | **1 000 000** | 10 | **whole** | **15 524 ms** | **475.9 ms** | ~39 574 147 ms | 108 MB |
//! | 1 000 000 | 10 | tenth | 15 524 ms | 131.5 ms | ~3 939 833 ms | 108 MB |
//!
//! ## At a million artifacts the caching arm is what needs bounding, and B is not a candidate
//!
//! **A layer of a million predicate artifacts costs ~0.5 s per request over the whole map** on a
//! ten-million-point corpus, and 0.13 s over a tenth of it. That is the serving loop testing every
//! artifact — `intersects` then `masked_count` — and **the cut cannot reduce it**, because the cut
//! runs after the verdicts and so serves fewer artifacts while evaluating exactly as many.
//! Comfortable is a few thousand: 25 ms at 1 024, 109 ms at 10 000, and past that it dominates
//! whatever else the viewport is doing.
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

use croaring::{Bitmap, Portable};
use tessera_engine::artifacts::ArtifactRows;
use tessera_engine::compose::MaskedSet;
use tessera_lifecycle::membership::ArtifactRecord;
use tessera_store::permutation::{Permutation, RowSpace};
use tessera_store::row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
use tessera_types::{EntityId, RowId};

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

fn records(sets: &[Bitmap]) -> Vec<ArtifactRecord> {
    sets.iter()
        .enumerate()
        .map(|(i, members)| ArtifactRecord {
            entity: EntityId::new(i as u64),
            key: None,
            members: members.clone(),
            contents: Vec::new(),
            attached_to: None,
            parent: None,
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
    let mask = PlainMask(mask);

    println!("rows = {rows}, mask = half of them");
    println!(
        "{:>9} {:>8} {:>10} {:>12} {:>12} {:>14} {:>12} {:>9}",
        "N", "members", "viewport", "A move", "A request", "B request", "B/A request", "A held"
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

        for (label, span) in [("whole", rows), ("tenth", rows / 10)] {
            let mut tile_rows = Bitmap::new();
            tile_rows.add_range(0..span);
            tile_rows.run_optimize();

            // **The loop both arms ride on**: candidacy against the viewport, then the masked
            // count that the criterion tests and the response carries.
            let started = Instant::now();
            let mut served = 0u64;
            for ordinal in 0..n as u32 {
                if row_forms.intersects(ordinal, &tile_rows, &mask) {
                    served += row_forms.masked_count(ordinal, &mask);
                }
            }
            let a_request = started.elapsed();
            std::hint::black_box(served);

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

            let ratio = b_request / a_request.as_secs_f64();
            println!(
                "{:>9} {:>8} {:>10} {:>10.1}ms {:>10.1}ms {}{:>11.1}ms {:>11.0}x {:>7.0}MB",
                n,
                rows as usize / n,
                label,
                a_move.as_secs_f64() * 1e3,
                a_request.as_secs_f64() * 1e3,
                modelled,
                b_request * 1e3,
                ratio,
                held as f64 / 1e6,
            );
        }
        drop(row_forms);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
