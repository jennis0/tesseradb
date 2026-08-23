//! **Arm 2 — the coarse-zoom filtered count**, which is where the render-column route is asked for
//! something a viewport-shaped route cannot bound.
//!
//! Arm 1's win comes from the viewport being small. At the coarsest zoom it is not: the request
//! spans the whole view and asks for a **count per tile**, so a row-space route walks every row in
//! the corpus. This arm prices that cell, which is the one that decides whether the entity-space
//! copy can be dropped outright or only skipped when the viewport is small.
//!
//! Same three routes as arm 1, answering "how many visible rows in each tile match?":
//!
//! - **E** — the shipped scan under `M_auth`, then `project` (the per-tile crossing never fires
//!   here: the viewport is the whole view, so the result can never exceed 3× its rows), then a
//!   range cardinality per tile.
//! - **R-dense** — scan the hot column whole, intersect the matches with the row-space mask once,
//!   then a range cardinality per tile.
//! - **R-masked** — walk only the masked rows, counting per tile as it goes.
//!
//! The three are checked to produce identical count vectors before any timing is reported.

use std::time::Instant;

use croaring::Bitmap;
use placement::{mask, median, permutation, project, splitmix, MaskShape};
use tessera_filter::{Codes, ValueColumn};
use tessera_types::AttrLocalId;

/// Tiles at a coarse level: 64×64 over the quadtree, which is the order a whole-view request
/// resolves to before the drawn-mark budget starts cutting depth.
const TILES: usize = 4_096;
const ROUNDS: usize = 3;

fn tile_ranges(n: usize) -> Vec<std::ops::Range<u32>> {
    let width = n.div_ceil(TILES) as u32;
    (0..TILES as u32)
        .map(|t| {
            let lo = t * width;
            (lo.min(n as u32))..((lo + width).min(n as u32))
        })
        .collect()
}

fn counts_from_rows(rows: &Bitmap, tiles: &[std::ops::Range<u32>]) -> Vec<u64> {
    tiles
        .iter()
        .map(|r| rows.range_cardinality(r.clone()))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scales: Vec<usize> = if args.len() > 1 {
        args[1..].iter().map(|s| s.parse().expect("scale")).collect()
    } else {
        vec![10_000_000, 100_000_000]
    };

    println!("n,mask,coverage,match_share,route,detail,ms,ns_per_corpus_row,result_entities,hits");

    for &n in &scales {
        eprintln!("n={n}: building fixture");
        let (entity_to_row, _row_to_entity) = permutation(n);
        let tiles = tile_ranges(n);

        for &domain in &[1_000u64, 4] {
            let values_by_entity: Vec<u16> =
                (0..n).map(|e| (splitmix(e as u64) % domain) as u16).collect();
            let mut values_by_row = vec![0u16; n];
            for (entity, &row) in entity_to_row.iter().enumerate() {
                values_by_row[row as usize] = values_by_entity[entity];
            }
            let column = ValueColumn::universal(Codes::U16(values_by_entity.clone().into()));
            let needle = 1u16;
            let match_share = 1.0 / domain as f64;

            for shape in [MaskShape::Blocked, MaskShape::Scattered] {
                for &coverage in &[0.01f64, 0.25] {
                    let auth = mask(n, shape, coverage);
                    let row_mask = project(&auth, &entity_to_row);

                    // ---- E ---------------------------------------------------------------
                    let mut scan_ms = Vec::new();
                    let mut rest_ms = Vec::new();
                    let mut counts_e = Vec::new();
                    let mut result_card = 0;
                    for _ in 0..ROUNDS {
                        let t = Instant::now();
                        let result = column.scan_eq(&auth, AttrLocalId::new(needle as u32));
                        scan_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                        result_card = result.cardinality();
                        let t = Instant::now();
                        let rows = project(&result, &entity_to_row);
                        counts_e = counts_from_rows(&rows, &tiles);
                        rest_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                    }

                    // ---- R-dense ---------------------------------------------------------
                    let mut dense_ms = Vec::new();
                    let mut counts_r = Vec::new();
                    for _ in 0..ROUNDS {
                        let t = Instant::now();
                        let mut matches = Bitmap::new();
                        let mut buf: Vec<u32> = Vec::with_capacity(4096);
                        for (row, &v) in values_by_row.iter().enumerate() {
                            if v == needle {
                                buf.push(row as u32);
                                if buf.len() == 4096 {
                                    matches.add_many(&buf);
                                    buf.clear();
                                }
                            }
                        }
                        matches.add_many(&buf);
                        matches.and_inplace(&row_mask);
                        counts_r = counts_from_rows(&matches, &tiles);
                        dense_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                    }

                    // ---- R-masked --------------------------------------------------------
                    let mut masked_ms = Vec::new();
                    let mut counts_m = Vec::new();
                    for _ in 0..ROUNDS {
                        let t = Instant::now();
                        let mut counts = vec![0u64; tiles.len()];
                        let mut cursor = row_mask.cursor();
                        for (t_idx, range) in tiles.iter().enumerate() {
                            cursor.reset_at_or_after(range.start);
                            let mut c = 0u64;
                            while let Some(row) = cursor.current() {
                                if row >= range.end {
                                    break;
                                }
                                cursor.move_next();
                                if values_by_row[row as usize] == needle {
                                    c += 1;
                                }
                            }
                            counts[t_idx] = c;
                        }
                        counts_m = counts;
                        masked_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                    }

                    assert_eq!(counts_e, counts_r, "E and R-dense disagree at n={n}");
                    assert_eq!(counts_r, counts_m, "R variants disagree at n={n}");
                    let hits: u64 = counts_r.iter().sum();

                    let emit = |route: &str, detail: &str, ms: f64| {
                        println!(
                            "{n},{},{coverage},{match_share:.5},{route},{detail},{ms:.2},{:.2},\
                             {result_card},{hits}",
                            shape.name(),
                            ms * 1e6 / n as f64
                        );
                    };
                    let scan = median(scan_ms);
                    let rest = median(rest_ms);
                    emit("E-scan", "entity-space column", scan);
                    emit("E-project+count", "project", rest);
                    emit("E-total", "project", scan + rest);
                    emit("R-dense", "render column", median(dense_ms));
                    emit("R-masked", "render column", median(masked_ms));
                }
            }
        }
    }
}
