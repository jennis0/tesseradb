//! **Crossing entity space and row space for a *filtered viewport*, three ways.**
//!
//! A filter answers in **entity space** — the value column is indexed by entity id, which is
//! assigned at build in permission-signature order and permanent under I9. A viewport asks in
//! **row space** — Morton order, so a tile is a contiguous row range. The two orders are
//! uncorrelated by construction (one is optimised for authorisation contiguity, the other for
//! spatial contiguity), so *something* has to cross between them. This arm prices the candidates.
//!
//! Every route answers the **same question**: which rows in the viewport pass the filter? The
//! routes are checked against each other before any timing is reported — a faster route that
//! answers differently is not a faster route.
//!
//! - **A — project.** The built route. Gather `entity_to_row` over every set bit of the result,
//!   sort, build a row-space bitmap, intersect with the viewport. O(result).
//! - **B — per-tile test.** Walk each tile's row range; recover the entity and test membership.
//!   O(viewport rows). Two variants, because the difference between them is the whole question:
//!   - **B-ideal** indexes a materialised `row_to_entity` array. This is what
//!     `probes/2026-08-08-filter-layout/` arm 3 measured, and **the system has no such array**.
//!   - **B-real** does what the engine would actually have to do: read the row's `tessera_id`
//!     (a `u64` column read, modelled as an indexed gather) and `IdentityKey::invert` it. The
//!     real Feistel, from `tessera-types`, not a stand-in.
//! - **C — coarse Morton pre-filter.** A second bitmap per coarse Morton cell, holding the
//!   *entities* whose rows fall in it. Union the cells the viewport touches, intersect with the
//!   result, and project only the survivors. Stays in entity space until the last step; the
//!   projection is then bounded by the viewport rather than by the result. Its storage is
//!   measured too, because that is the objection to it: a cell's entity set is *scattered* in
//!   entity space — the same property that makes authorisation postings compress works against
//!   this structure.
//!
//! The permutation is a genuine Fisher–Yates shuffle, for arm 3's reason: every route here is
//! dominated by random access, and a structured map would flatter them all equally and wrongly.

use std::time::Instant;

use croaring::{Bitmap, Portable};
use tessera_types::{EntityId, IdentityKey};

/// Tiles a viewport resolves to (`filter-surface.md` §4 and arm 3 both use ~300).
const TILES: usize = 300;
/// Route C's cells are sized **to the tile width being swept**, which is the structure's best
/// case: a cell coarser than a tile over-selects (a first run with a fixed 4,096 cells made the
/// pre-filter 24× too coarse at 1,000-row tiles and buried it), and a cell finer than a tile is
/// the inverse permutation with extra steps. Matching them is the fairest reading of the idea.
const ROUNDS: usize = 3;

fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `(entity_to_row, row_to_entity)` — a genuine shuffle, so neither direction is contiguous.
fn permutation(n: usize) -> (Vec<u32>, Vec<u32>) {
    let mut entity_to_row: Vec<u32> = (0..n as u32).collect();
    let mut state = 0x5DEE_CE66_D1CE_u64;
    for i in (1..n).rev() {
        state = splitmix(state);
        let j = (state % (i as u64 + 1)) as usize;
        entity_to_row.swap(i, j);
    }
    let mut row_to_entity = vec![0u32; n];
    for (entity, &row) in entity_to_row.iter().enumerate() {
        row_to_entity[row as usize] = entity as u32;
    }
    (entity_to_row, row_to_entity)
}

/// An entity-space filter result holding `share` of the corpus, scattered — which is what a
/// masked scan over an ingest-ordered column produces for an attribute uncorrelated with
/// position.
fn result_of(n: usize, share: f64) -> Bitmap {
    let mut out = Bitmap::new();
    let step = (1.0 / share) as u64;
    let mut e = 0u64;
    let mut state = 0x1234_5678u64;
    while e < n as u64 {
        out.add(e as u32);
        state = splitmix(state);
        e += 1 + state % (2 * step - 1);
    }
    out
}

/// The viewport's tiles as row ranges, spread evenly across row space.
/// A **contiguous** result of the same cardinality — arm 3's shape, kept as a control. The two
/// arms disagree by ~5x on the per-tile constant, and the hypothesis is that this is why: a
/// membership test against a contiguous bitmap hits a handful of containers, where one against a
/// scattered bitmap is a cache miss per test.
fn result_contiguous(n: usize, share: f64) -> Bitmap {
    let card = (n as f64 * share) as u32;
    let mut out = Bitmap::new();
    out.add_range(0..card);
    out.run_optimize();
    out
}

fn viewport(n: usize, tile_width: usize) -> Vec<(u32, u32)> {
    let stride = n / TILES;
    (0..TILES)
        .map(|t| {
            let lo = (t * stride) as u32;
            let hi = (lo as usize + tile_width).min(n) as u32;
            (lo, hi)
        })
        .collect()
}

fn viewport_rows(tiles: &[(u32, u32)]) -> Bitmap {
    let mut b = Bitmap::new();
    for &(lo, hi) in tiles {
        b.add_range(lo..hi);
    }
    b
}

fn route_project(result: &Bitmap, entity_to_row: &[u32], view: &Bitmap) -> Bitmap {
    let mut rows: Vec<u32> = result
        .iter()
        .map(|e| entity_to_row[e as usize])
        .collect();
    rows.sort_unstable();
    let mut projected = Bitmap::of(&rows);
    projected.and_inplace(view);
    projected
}

fn route_per_tile_ideal(result: &Bitmap, row_to_entity: &[u32], tiles: &[(u32, u32)]) -> Bitmap {
    let mut out = Bitmap::new();
    for &(lo, hi) in tiles {
        for row in lo..hi {
            if result.contains(row_to_entity[row as usize]) {
                out.add(row);
            }
        }
    }
    out
}

fn route_per_tile_real(
    result: &Bitmap,
    tessera_id_by_row: &[u64],
    key: &IdentityKey,
    tiles: &[(u32, u32)],
) -> Bitmap {
    let mut out = Bitmap::new();
    for &(lo, hi) in tiles {
        for row in lo..hi {
            let id = tessera_id_by_row[row as usize];
            let (_shard, entity) = key.invert(tessera_types::TesseraId::new(id));
            if result.contains(entity.raw() as u32) {
                out.add(row);
            }
        }
    }
    out
}

fn route_coarse(
    result: &Bitmap,
    cells: &[Bitmap],
    cell_width: usize,
    entity_to_row: &[u32],
    tiles: &[(u32, u32)],
    view: &Bitmap,
) -> Bitmap {
    // Which coarse cells does the viewport touch? A tile is contiguous, so this is a range of
    // cell indices per tile — no search, just arithmetic.
    let mut touched: Vec<usize> = Vec::new();
    for &(lo, hi) in tiles {
        let first = lo as usize / cell_width;
        let last = (hi as usize - 1) / cell_width;
        for c in first..=last {
            touched.push(c);
        }
    }
    touched.sort_unstable();
    touched.dedup();

    let mut plausible = Bitmap::new();
    for &c in &touched {
        plausible.or_inplace(&cells[c]);
    }
    plausible.and_inplace(result);

    let mut rows: Vec<u32> = plausible
        .iter()
        .map(|e| entity_to_row[e as usize])
        .collect();
    rows.sort_unstable();
    let mut projected = Bitmap::of(&rows);
    projected.and_inplace(view);
    projected
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args
        .get(1)
        .map(|s| s.parse().unwrap())
        .unwrap_or(100_000_000);

    eprintln!("building the permutation at n={n}…");
    let (entity_to_row, row_to_entity) = permutation(n);

    // `columns.arrow`'s `tessera_id`, row-indexed — what route B actually has to read.
    eprintln!("building the tessera_id column…");
    let key = IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").expect("key");
    let tessera_id_by_row: Vec<u64> = row_to_entity
        .iter()
        .map(|&e| key.forward(0, EntityId::new(e as u64)).expect("forward").raw())
        .collect();

    let build_cells = |cell_width: usize| -> Vec<Bitmap> {
        let count = n.div_ceil(cell_width);
        let mut cells: Vec<Bitmap> = (0..count).map(|_| Bitmap::new()).collect();
        for (entity, &row) in entity_to_row.iter().enumerate() {
            cells[row as usize / cell_width].add(entity as u32);
        }
        for c in cells.iter_mut() {
            c.run_optimize();
        }
        cells
    };

    println!("n,shape,share,result_cardinality,tile_width,viewport_rows,route,ms,ns_per_unit,answer_rows");

    for (shape, share) in [
        ("scattered", 0.0001f64),
        ("scattered", 0.001),
        ("scattered", 0.01),
        ("scattered", 0.1),
        ("contiguous", 0.0001),
        ("contiguous", 0.001),
        ("contiguous", 0.01),
        ("contiguous", 0.1),
    ] {
        let result = if shape == "scattered" {
            result_of(n, share)
        } else {
            result_contiguous(n, share)
        };
        let card = result.cardinality();
        for tile_width in [1_000usize, 10_000] {
            let cell_width = tile_width;
            let cells = build_cells(cell_width);
            let cell_bytes: usize = cells
                .iter()
                .map(|c| c.get_serialized_size_in_bytes::<Portable>())
                .sum();
            eprintln!(
                "route C at cell_width={cell_width}: {} cells, {:.1} MB ({:.2} B/entity)",
                cells.len(),
                cell_bytes as f64 / (1u64 << 20) as f64,
                cell_bytes as f64 / n as f64
            );
            let tiles = viewport(n, tile_width);
            let view = viewport_rows(&tiles);
            let rows_in_view = view.cardinality();

            // **Agreement before timing.** Four routes, one answer.
            let a = route_project(&result, &entity_to_row, &view);
            let bi = route_per_tile_ideal(&result, &row_to_entity, &tiles);
            let br = route_per_tile_real(&result, &tessera_id_by_row, &key, &tiles);
            let c = route_coarse(&result, &cells, cell_width, &entity_to_row, &tiles, &view);
            assert_eq!(a, bi, "project and per-tile-ideal disagree");
            assert_eq!(a, br, "project and per-tile-real disagree");
            assert_eq!(a, c, "project and coarse disagree");
            let answer = a.cardinality();

            let mut timings: Vec<(&str, Vec<f64>, f64)> = vec![
                ("project", Vec::new(), card as f64),
                ("per_tile_ideal", Vec::new(), rows_in_view as f64),
                ("per_tile_real", Vec::new(), rows_in_view as f64),
                ("coarse_prefilter", Vec::new(), rows_in_view as f64),
            ];
            for _ in 0..ROUNDS {
                let t = Instant::now();
                std::hint::black_box(route_project(&result, &entity_to_row, &view));
                timings[0].1.push(t.elapsed().as_secs_f64() * 1e3);

                let t = Instant::now();
                std::hint::black_box(route_per_tile_ideal(&result, &row_to_entity, &tiles));
                timings[1].1.push(t.elapsed().as_secs_f64() * 1e3);

                let t = Instant::now();
                std::hint::black_box(route_per_tile_real(
                    &result,
                    &tessera_id_by_row,
                    &key,
                    &tiles,
                ));
                timings[2].1.push(t.elapsed().as_secs_f64() * 1e3);

                let t = Instant::now();
                std::hint::black_box(route_coarse(
                    &result,
                    &cells,
                    cell_width,
                    &entity_to_row,
                    &tiles,
                    &view,
                ));
                timings[3].1.push(t.elapsed().as_secs_f64() * 1e3);
            }

            for (route, samples, unit) in timings {
                let ms = median(samples);
                println!(
                    "{n},{shape},{share},{card},{tile_width},{rows_in_view},{route},{ms:.3},{:.2},{answer}",
                    ms * 1e6 / unit
                );
            }
        }
    }
}
