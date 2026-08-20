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
//! Rows = 1 000 000. Mask = half the corpus visible. `N` is the layer's artifact count; each
//! artifact holds `rows / N` scattered members. **263 is the demo corpus's HDBSCAN layer**, which
//! is the number the owner named as the one to hold in mind.
//!
//! | N | viewport | A move | A request | B request | B / A request |
//! |---:|---|---:|---:|---:|---:|
//! | 64 | whole | 8.09 ms | 0.64 ms | 2 347 ms | 3 690× |
//! | 64 | tenth | 8.09 ms | 0.43 ms | 236 ms | 554× |
//! | **263** | **whole** | **15.10 ms** | **1.02 ms** | **8 202 ms** | **8 040×** |
//! | 263 | tenth | 15.10 ms | 0.62 ms | 844 ms | 1 355× |
//! | 1 024 | whole | 31.74 ms | 1.31 ms | 23 890 ms | 18 211× |
//! | 1 024 | tenth | 31.74 ms | 0.58 ms | 2 284 ms | 3 908× |
//!
//! **A, and not marginally.** The arithmetic set up above — `A move` against `B request` × requests
//! between generation moves — does not need doing: A's entire generation-move cost is repaid by
//! **one** request, at every size and both viewport widths. At the demo corpus's 263 artifacts over
//! the whole map, A costs 15 ms once per move and 1.0 ms per request where B costs 8.2 **seconds**
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
//! **`A request` is 3.9 µs per artifact at N = 263**, which is `annotation-representation.md` §2's
//! own microseconds-per-artifact figure arrived at independently — a sign the fixture is at a
//! realistic density rather than a flattering one.
//!
//! Run: `cargo run --release -p tessera-bench --bin predicate_membership_cost`

use std::sync::Arc;
use std::time::Instant;

use croaring::Bitmap;
use tessera_engine::artifacts::ArtifactRows;
use tessera_engine::compose::MaskedSet;
use tessera_lifecycle::membership::ArtifactRecord;
use tessera_store::permutation::{Permutation, RowSpace};
use tessera_store::row_entity::{write_row_entity, RowToEntity, ROW_ENTITY_FILE};
use tessera_types::{EntityId, RowId};

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
    const ROWS: u32 = 1_000_000;
    let space = row_space(&dir, ROWS);

    // Half the corpus visible: a mask dense enough that a count touches every container, which is
    // the expensive side of Roaring's O(containers touched) rather than the flattering one.
    let mut mask = Bitmap::new();
    for row in (0..ROWS).step_by(2) {
        mask.add(row);
    }
    mask.run_optimize();
    let mask = PlainMask(mask);

    println!("rows = {ROWS}, mask = half of them");
    println!(
        "{:>6} {:>10} {:>12} {:>12} {:>12} {:>12} {:>10}",
        "N", "viewport", "A move", "A request", "B request", "B/A request", "moves"
    );

    for n in [64usize, 263, 1024] {
        let sets = memberships(ROWS, n);
        let recs = records(&sets);

        // **A's whole extra cost, and it is paid at a generation move.** `ArtifactRows::build` is
        // one `project_base` per artifact, and `project_base` decodes the whole membership — the
        // same call an enumerated layer already pays at open and at a move.
        let started = Instant::now();
        let rows = ArtifactRows::build(
            recs.iter().enumerate().map(|(i, r)| (i as u32, r)),
            &space,
        );
        let a_move = started.elapsed();

        for (label, span) in [("whole", ROWS), ("tenth", ROWS / 10)] {
            let mut tile_rows = Bitmap::new();
            tile_rows.add_range(0..span);
            tile_rows.run_optimize();

            // **The loop both arms ride on**: candidacy against the viewport, then the masked
            // count that the criterion tests and the response carries.
            let started = Instant::now();
            let mut served = 0u64;
            for ordinal in 0..n as u32 {
                if rows.intersects(ordinal, &tile_rows, &mask) {
                    served += rows.masked_count(ordinal, &mask);
                }
            }
            let a_request = started.elapsed();
            std::hint::black_box(served);

            // **B's shape: one inversion, N probes per row.** The domain is the viewport's rows;
            // each is inverted once however many entity-space sets ride on it, and each set costs
            // one probe on top (`per_tile_crossing_multi`'s own cost note).
            let started = Instant::now();
            let mut hits = 0u64;
            for row in 0..span {
                let Some(entity) = space.entity_of(RowId::new(row)) else {
                    continue;
                };
                let raw = entity.raw() as u32;
                for set in &sets {
                    if set.contains(raw) {
                        hits += 1;
                    }
                }
            }
            let b_request = started.elapsed();
            std::hint::black_box(hits);

            let ratio = b_request.as_secs_f64() / a_request.as_secs_f64();
            // How many requests A's generation-move cost is worth, priced in B's per-request extra
            // over A's. Below one, B is cheaper however often the layer is served.
            let extra = b_request.as_secs_f64() - a_request.as_secs_f64();
            let moves = if extra > 0.0 {
                format!("{:.0}", a_move.as_secs_f64() / extra)
            } else {
                "-".to_string()
            };
            println!(
                "{:>6} {:>10} {:>10.2}ms {:>10.2}ms {:>10.2}ms {:>11.1}x {:>10}",
                n,
                label,
                a_move.as_secs_f64() * 1e3,
                a_request.as_secs_f64() * 1e3,
                b_request.as_secs_f64() * 1e3,
                ratio,
                moves,
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}
