//! **Arm 1 — a filtered viewport, answered from the entity-space column and from the render
//! column.**
//!
//! A filterable column is stored twice today when it is also rendered: a fixed-width **row-space**
//! column in `columns.arrow` for the gather, and a flat **entity-space** value column under
//! `attrs/` for the filter. The question this arm prices is whether the second copy earns its
//! bytes, by measuring the one request that needs both — a viewport with a filter on it.
//!
//! Both routes answer exactly the same question: *which rows in this viewport pass the filter and
//! are visible to this principal?* They are checked against each other before any timing is
//! reported.
//!
//! - **E — the built route.** Scan the entity-space value column under `M_auth`
//!   (`ValueColumn::scan_eq`, the shipped code), then cross the entity-space result into row space
//!   by whichever of `filter-surface.md` §4's two routes the shipped rule picks: project the whole
//!   result, or test the viewport's own rows through `row-entity.u32`. Cost is O(|`M_auth`|) for the
//!   scan — the corpus-scale term — plus the crossing.
//! - **R-dense — the render column, scanned in row space.** Walk the viewport's row ranges over the
//!   hot column, keep the rows whose value matches, and intersect with the session's row-space mask
//!   projection. No entity-space artefact, no crossing, and no `row-entity.u32` lookup. Cost is
//!   O(viewport rows).
//! - **R-masked — the same, mask first.** Iterate the masked rows inside each range instead of every
//!   row, so a sparse principal walks less. Measured separately because the row-space mask is the
//!   *projection* of an entity-space one and is therefore scattered however contiguous `M_auth` was,
//!   which is exactly the case where "mask first" stops being free.
//!
//! **The row-space mask projection is not charged to either route.** It is the session's cached
//! fragment projection (`filter-surface.md` §4), built once per session and reused by every request
//! including unfiltered ones.
//!
//! Single-threaded, medians of three. Both routes parallelise over their own axis — E over the
//! result, R over the viewport — so the ratio is what to read, not the milliseconds.

use std::time::Instant;

use croaring::Bitmap;
use placement::{
    mask, median, permutation, project, range_bitmap, rows_in, viewport, MaskShape,
};
use tessera_filter::{Codes, ValueColumn};
use tessera_types::AttrLocalId;

/// A viewport resolves to ~300 tiles (`filter-surface.md` §7.2, and the two prior campaigns).
const TILES: usize = 300;
const ROUNDS: usize = 3;
/// `PER_TILE_CROSSING_RATIO` — the shipped crossover, transcribed so route E takes the route the
/// engine would take rather than the one that flatters it.
const PER_TILE_RATIO: u64 = 3;

/// Route E's crossing, both halves, with the shipped rule choosing between them.
fn cross(
    result: &Bitmap,
    entity_to_row: &[u32],
    row_to_entity: &[u32],
    ranges: &[std::ops::Range<u32>],
    view: &Bitmap,
    viewport_rows: u64,
) -> (Bitmap, &'static str) {
    if result.cardinality() > viewport_rows.saturating_mul(PER_TILE_RATIO) {
        let mut out = Bitmap::new();
        let mut buf: Vec<u32> = Vec::with_capacity(1024);
        for range in ranges {
            for row in range.clone() {
                if result.contains(row_to_entity[row as usize]) {
                    buf.push(row);
                    if buf.len() == 1024 {
                        out.add_many(&buf);
                        buf.clear();
                    }
                }
            }
        }
        out.add_many(&buf);
        (out, "per-tile")
    } else {
        let mut projected = project(result, entity_to_row);
        projected.and_inplace(view);
        (projected, "project")
    }
}

/// Route R, dense: scan every row of the viewport's ranges, then intersect with the mask.
///
/// The value read is sequential within a range — the same read the gather already performs for a
/// rendered column — so the loop is a comparison over a contiguous slice. Intersecting the matches
/// with the row-space mask afterwards costs O(containers touched) and, crucially, makes the work
/// independent of *which* rows the principal may see: the scan reads the same bytes either way.
fn route_r_dense(
    values_by_row: &[u16],
    ranges: &[std::ops::Range<u32>],
    needle: u16,
    row_mask: &Bitmap,
) -> Bitmap {
    let mut out = Bitmap::new();
    let mut buf: Vec<u32> = Vec::with_capacity(1024);
    for range in ranges {
        let (lo, hi) = (range.start as usize, range.end as usize);
        for (i, &v) in values_by_row[lo..hi].iter().enumerate() {
            if v == needle {
                buf.push(lo as u32 + i as u32);
                if buf.len() == 1024 {
                    out.add_many(&buf);
                    buf.clear();
                }
            }
        }
    }
    out.add_many(&buf);
    out.and_inplace(row_mask);
    out
}

/// Route R, mask first: visit only the rows the principal may see.
///
/// Fewer values read when the principal is sparse, but the reads are scattered — a projected mask
/// has no run structure in row space — and the work now depends on the mask, which is the property
/// the dense variant keeps.
fn route_r_masked(
    values_by_row: &[u16],
    ranges: &[std::ops::Range<u32>],
    needle: u16,
    row_mask: &Bitmap,
) -> Bitmap {
    let mut out = Bitmap::new();
    let mut buf: Vec<u32> = Vec::with_capacity(1024);
    // A cursor seeks to the range and walks the set bits inside it. Intersecting a copy of the mask
    // with each range instead would cost O(mask containers) per range — 300 times per request, which
    // measures the probe rather than the route.
    let mut cursor = row_mask.cursor();
    for range in ranges {
        cursor.reset_at_or_after(range.start);
        while let Some(row) = cursor.current() {
            if row >= range.end {
                break;
            }
            cursor.move_next();
            if values_by_row[row as usize] == needle {
                buf.push(row);
                if buf.len() == 1024 {
                    out.add_many(&buf);
                    buf.clear();
                }
            }
        }
    }
    out.add_many(&buf);
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scales: Vec<usize> = if args.len() > 1 {
        args[1..].iter().map(|s| s.parse().expect("scale")).collect()
    } else {
        vec![10_000_000, 100_000_000]
    };

    println!(
        "n,mask,coverage,viewport_rows,match_share,route,detail,ms,ns_per_viewport_row,\
         result_entities,hits"
    );

    for &n in &scales {
        eprintln!("n={n}: building fixture");
        let (entity_to_row, row_to_entity) = permutation(n);

        // A category column: `domain` distinct codes, uncorrelated with position, so a value's
        // members are scattered in row space — the realistic shape for an ingest-ordered attribute.
        for &domain in &[1_000u64, 4] {
            let values_by_entity: Vec<u16> =
                (0..n).map(|e| (splitmix64(e as u64) % domain) as u16).collect();
            let mut values_by_row = vec![0u16; n];
            for (entity, &row) in entity_to_row.iter().enumerate() {
                values_by_row[row as usize] = values_by_entity[entity];
            }
            let column = ValueColumn::universal(Codes::U16(values_by_entity.clone().into()));
            let needle = 1u16;
            let match_share = 1.0 / domain as f64;

            for shape in [MaskShape::Contiguous, MaskShape::Blocked, MaskShape::Scattered] {
                for &coverage in &[0.01f64, 0.25] {
                    let auth = mask(n, shape, coverage);
                    // The session's cached fragment projection — built once per session, charged to
                    // neither route.
                    let row_mask = project(&auth, &entity_to_row);

                    for &width in &[1_000u32, 100] {
                        let ranges = viewport(n, TILES, width);
                        let view = range_bitmap(&ranges);
                        let vp_rows = rows_in(&ranges);

                        // ---- E: the built route ------------------------------------------
                        let mut scan_ms = Vec::new();
                        let mut cross_ms = Vec::new();
                        let mut detail = "";
                        let mut answer_e = Bitmap::new();
                        let mut result_card = 0u64;
                        for _ in 0..ROUNDS {
                            let t = Instant::now();
                            let result = column.scan_eq(&auth, AttrLocalId::new(needle as u32));
                            scan_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                            result_card = result.cardinality();
                            let t = Instant::now();
                            let (rows, which) = cross(
                                &result,
                                &entity_to_row,
                                &row_to_entity,
                                &ranges,
                                &view,
                                vp_rows,
                            );
                            cross_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                            detail = which;
                            answer_e = rows;
                        }

                        // ---- R: the render column ----------------------------------------
                        let mut dense_ms = Vec::new();
                        let mut masked_ms = Vec::new();
                        let mut answer_r = Bitmap::new();
                        let mut answer_m = Bitmap::new();
                        for _ in 0..ROUNDS {
                            let t = Instant::now();
                            answer_r = route_r_dense(&values_by_row, &ranges, needle, &row_mask);
                            dense_ms.push(t.elapsed().as_secs_f64() * 1000.0);

                            let t = Instant::now();
                            answer_m = route_r_masked(&values_by_row, &ranges, needle, &row_mask);
                            masked_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                        }

                        assert_eq!(
                            answer_e, answer_r,
                            "routes disagree: n={n} shape={} coverage={coverage} width={width}",
                            shape.name()
                        );
                        assert_eq!(answer_r, answer_m, "R variants disagree");

                        let hits = answer_r.cardinality();
                        let emit = |route: &str, detail: &str, ms: f64| {
                            println!(
                                "{n},{},{coverage},{vp_rows},{match_share:.5},{route},{detail},\
                                 {ms:.3},{:.2},{result_card},{hits}",
                                shape.name(),
                                ms * 1e6 / vp_rows as f64
                            );
                        };
                        let scan = median(scan_ms);
                        let crossing = median(cross_ms);
                        emit("E-scan", "entity-space column", scan);
                        emit("E-cross", detail, crossing);
                        emit("E-total", detail, scan + crossing);
                        emit("R-dense", "render column", median(dense_ms));
                        emit("R-masked", "render column", median(masked_ms));
                    }
                }
            }
        }
    }
}

fn splitmix64(x: u64) -> u64 {
    placement::splitmix(x)
}
