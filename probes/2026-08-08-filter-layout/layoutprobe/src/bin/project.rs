//! With the scan affordable, the binding term is getting an entity-space filter result into **row
//! space**. Two routes, and the corpus has never compared them.
//!
//! - **project** — iterate the result's set bits, gather `entity_to_row`, sort, build a row-space
//!   bitmap. O(set bits), and the route `filter-surface.md` §4 was built around. The corpus holds
//!   two measured points that disagree per-bit by 12×: 8.8 s for a 69.3×10⁶-entity mask
//!   (`probes/results.md` §6, i.e. 127 ns/set-bit) and 10.7 s at 10⁹ (§10.4, i.e. 10.7 ns/item).
//!   No curve exists between them, so this arm measures one.
//! - **per-tile test** — never project. For each tile the viewport actually asks for, walk its
//!   contiguous row range, map row → entity, and test membership in the result. O(rows in the
//!   viewport), independent of how large the result is.
//!
//! The two scale on different axes, so the question is not which is faster but **where they
//! cross**: a viewport asks for a few hundred tiles, and a filter result can be a tenth of the
//! corpus.
//!
//! `entity_to_row` is a genuine permutation rather than an affine map, because both routes are
//! dominated by random access and a structured map would flatter them equally and wrongly.

use std::time::Instant;

use croaring::Bitmap;

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A tessera_id-order permutation, built by Fisher–Yates so it has no exploitable structure.
/// Returns `(entity_to_row, row_to_entity)` — the design derives the second by inverting the
/// segment's `tessera_id` column rather than storing it (contracts §2.6), which costs the same
/// random access this measures.
fn permutation(n: u64) -> (Vec<u32>, Vec<u32>) {
    let mut entity_to_row: Vec<u32> = (0..n as u32).collect();
    for i in (1..n as usize).rev() {
        let j = (splitmix(i as u64) % (i as u64 + 1)) as usize;
        entity_to_row.swap(i, j);
    }
    let mut row_to_entity = vec![0u32; n as usize];
    for (e, &r) in entity_to_row.iter().enumerate() {
        row_to_entity[r as usize] = e as u32;
    }
    (entity_to_row, row_to_entity)
}

/// A filter result of the given cardinality. Contiguous, which is the *cheap* end for the project
/// route — a scattered result would only widen the gap this arm is looking for.
fn result_of(n: u64, share: f64) -> Bitmap {
    let mut b = Bitmap::new();
    let len = (n as f64 * share) as u64;
    let lo = (n - len) / 2;
    b.add_range(lo as u32..(lo + len) as u32);
    b.run_optimize();
    b
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(100_000_000);

    eprintln!("building permutation at n={n}…");
    let (entity_to_row, row_to_entity) = permutation(n);

    println!("n,result_share,result_cardinality,route,viewport_rows,ms,ns_per_unit");

    // A viewport resolves to a few hundred tiles, each a contiguous row range (§5.2). Sweeping the
    // tile width sweeps the viewport's total row count, which is the axis the per-tile route pays on.
    const TILES: u64 = 300;
    let widths: [u64; 3] = [1_000, 10_000, 100_000];

    for share in [0.0001, 0.001, 0.01, 0.1] {
        let result = result_of(n, share);
        let card = result.cardinality();

        // --- project: gather, sort, build.
        let project = |warm: bool| -> (f64, u64) {
            let t = Instant::now();
            let mut rows: Vec<u32> = Vec::with_capacity(card as usize);
            for e in result.iter() {
                rows.push(entity_to_row[e as usize]);
            }
            rows.sort_unstable();
            let mut out = Bitmap::new();
            out.add_many(&rows);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let c = out.cardinality();
            if warm {
                assert_eq!(c, card, "projection lost entities");
            }
            (ms, c)
        };
        let _ = project(true);
        let (project_ms, _) = project(false);
        println!(
            "{n},{share},{card},project,,{project_ms:.2},{:.1}",
            project_ms * 1e6 / card as f64
        );

        // --- per-tile test: never project; walk each tile's rows and test membership.
        for width in widths {
            let viewport_rows = TILES * width;
            if viewport_rows > n {
                continue;
            }
            let stride = n / TILES;
            let run = |warm: bool| -> (f64, u64) {
                let t = Instant::now();
                let mut hits = 0u64;
                for tile in 0..TILES {
                    let lo = tile * stride;
                    for r in lo..(lo + width).min(n) {
                        if result.contains(row_to_entity[r as usize]) {
                            hits += 1;
                        }
                    }
                }
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                if warm {
                    // The count is what a tile actually needs; assert only that it is plausible.
                    assert!(hits <= viewport_rows);
                }
                (ms, hits)
            };
            let _ = run(true);
            let (ms, hits) = run(false);
            println!(
                "{n},{share},{card},per_tile,{viewport_rows},{ms:.2},{:.1}",
                ms * 1e6 / viewport_rows as f64
            );
            let _ = hits;
        }
    }
}
