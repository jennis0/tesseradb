//! Cell-occupancy analysis of a built bundle's `morton.u32` column.
//!
//! Offline measurement tool for candidate B1 of
//! `docs/evidence/memos/2026-07-30-viewport-hot-path-and-bundle-size-review.md` (§2/B1): within a
//! leaf Morton cell the identity column is sorted, so B1's search route replaces the per-row
//! scan only where cells hold enough rows — the memo puts the warm scan-vs-search crossover
//! near 25–75 rows per cell, and names the morton column's run-length distribution "the first
//! task of any B1 implementation plan". This tool extracts that distribution: how many rows
//! live in cells of what size, globally and (optionally) within one viewport's tiles.
//!
//! Usage:
//!
//! ```text
//! cell_histogram <bundle> [x0 y0 x1 y1 zoom]
//! ```
//!
//! `<bundle>` is a bundle root (the directory containing `CURRENT`), opened read-only through
//! the ordinary digest-verified read protocol. The optional `x0 y0 x1 y1` bbox (extent units)
//! plus `zoom` (tile depth, 0–16) restricts a second analysis to that bbox's tiles.
//!
//! One JSON object goes to stdout; human-oriented notes go to stderr. Keys:
//!
//! - `bundle`, `partition`, `slice`, `segment`, `rows` — what was analysed.
//! - `global` — an occupancy object over the whole column.
//! - `region` — `null`, or the bbox/zoom drilldown: `bbox`, `zoom`, `tiles` (tiles resolved),
//!   `tiles_occupied` (tiles with at least one row), `mean_occupied_cells_per_tile`
//!   (occupied cells ÷ tiles resolved), `mean_rows_per_occupied_cell_per_tile` (mean over
//!   occupied tiles of that tile's rows ÷ that tile's occupied cells), and `occupancy` — the
//!   same occupancy object restricted to the region's rows.
//!
//! An occupancy object ("cell" = one distinct 32-bit Morton code; "cell size" = rows carrying
//! that code):
//!
//! - `rows`, `distinct_codes`, `max_cell_size`.
//! - `mean_rows_per_cell` — `rows / distinct_codes`.
//! - `median_rows_per_cell` — lower median of cell size over **cells**: the smallest size `l`
//!   with at least `(distinct_codes + 1) / 2` cells of size ≤ `l`. `null` when empty.
//! - `rows_weighted_median_cell_size` — lower median of cell size over **rows**: the smallest
//!   `l` with at least `(rows + 1) / 2` rows in cells of size ≤ `l`. `null` when empty. This is
//!   the B1 headline: half the rows live in cells at least this big.
//! - `rows_fraction_in_cells_ge` — fraction of rows in cells of size ≥ 8, 32, 128, 1024.
//! - `histogram` — all 18 buckets `{1, 2, 3-4, 5-8, …, 32769-65536, >65536}` (powers of two),
//!   each `{bucket, cells, rows, rows_fraction}`.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use serde_json::json;
use tessera_spatial::{tiles_for_bbox, tiles_for_bbox_count, Extent};
use tessera_store::{open_bundle, tile_ranges_all, SegmentData};

/// Ceiling on tiles a region request may resolve. `tiles_for_bbox` at depth 16 over the full
/// extent would enumerate 65536² tiles (~69 GB of `Tile`s — see its doc); refusing above this
/// budget keeps the tool's memory O(tiles requested) with a bound the caller can reason about
/// (2²² tiles ≈ 100 MB of tiles + ranges) instead of an allocator abort.
const TILE_BUDGET: u64 = 1 << 22;

const USAGE: &str = "usage: cell_histogram <bundle> [x0 y0 x1 y1 zoom]";

/// Row-fraction thresholds reported as headline numbers. 8 and 32 bracket the memo's measured
/// warm crossover (≈25–75 rows); 128 and 1024 show how much of the corpus sits deep in
/// search-wins territory.
const GE_THRESHOLDS: [u32; 4] = [8, 32, 128, 1024];

const BUCKETS: usize = 18;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("cell_histogram: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    bundle: PathBuf,
    region: Option<RegionArgs>,
}

struct RegionArgs {
    bbox: [f64; 4],
    zoom: u8,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let region = match args.len() {
        1 => None,
        6 => {
            let mut bbox = [0f64; 4];
            for (slot, raw) in bbox.iter_mut().zip(&args[1..5]) {
                *slot = raw
                    .parse::<f64>()
                    .map_err(|_| format!("bbox coordinate '{raw}' is not a number"))?;
                if !slot.is_finite() {
                    return Err(format!("bbox coordinate '{raw}' is not finite"));
                }
            }
            let zoom: u8 = args[5]
                .parse()
                .map_err(|_| format!("zoom '{}' is not an integer", args[5]))?;
            if zoom > 16 {
                return Err(format!("zoom {zoom} exceeds the grid depth 16"));
            }
            Some(RegionArgs { bbox, zoom })
        }
        _ => return Err("expected 1 argument (bundle) or 6 (bundle + bbox + zoom)".to_string()),
    };
    Ok(Args {
        bundle: PathBuf::from(&args[0]),
        region,
    })
}

fn run(args: &Args) -> Result<(), String> {
    let opened_at = Instant::now();
    let bundle = open_bundle(&args.bundle).map_err(|e| format!("open_bundle: {e}"))?;
    eprintln!(
        "opened and verified {} in {:.1}s",
        args.bundle.display(),
        opened_at.elapsed().as_secs_f64()
    );

    // A build writes exactly one segment per (partition, slice) and this tool's single-pass
    // run-length scan is only meaningful over one sorted column, so anything else is refused
    // rather than silently merged (concatenating segments would fabricate runs at the seams).
    let mut segments: Vec<(&str, &str, &SegmentData)> = Vec::new();
    for (phash, partition) in &bundle.partitions {
        for (slice_id, slice) in &partition.slices {
            for seg in &slice.segments {
                segments.push((phash, slice_id, seg));
            }
        }
    }
    let &(phash, slice_id, seg) = match segments.as_slice() {
        [only] => only,
        other => {
            return Err(format!(
                "expected the single Phase 1 segment, found {}",
                other.len()
            ))
        }
    };
    let codes = seg.morton.u32();
    eprintln!(
        "segment '{}' (partition '{phash}', slice '{slice_id}'): {} rows",
        seg.seg_id,
        codes.len()
    );

    let scan_at = Instant::now();
    let mut global = Occupancy::default();
    global.scan(codes);
    eprintln!(
        "global pass: {} distinct codes in {:.1}s",
        global.distinct_codes(),
        scan_at.elapsed().as_secs_f64()
    );

    let region = match &args.region {
        Some(region_args) => analyse_region(&bundle.manifest.quantisation, seg, region_args)?,
        None => serde_json::Value::Null,
    };

    let report = json!({
        "bundle": args.bundle.display().to_string(),
        "partition": phash,
        "slice": slice_id,
        "segment": seg.seg_id,
        "rows": codes.len() as u64,
        "global": global.to_json(),
        "region": region,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("report serialises")
    );
    Ok(())
}

fn analyse_region(
    quantisation: &tessera_store::manifest::Quantisation,
    seg: &SegmentData,
    region: &RegionArgs,
) -> Result<serde_json::Value, String> {
    let extent = Extent {
        x_min: quantisation.x_min,
        x_max: quantisation.x_max,
        y_min: quantisation.y_min,
        y_max: quantisation.y_max,
    };
    extent
        .validate()
        .map_err(|e| format!("bundle quantisation extent: {e}"))?;

    let tile_count = tiles_for_bbox_count(region.bbox, region.zoom, &extent);
    if tile_count > TILE_BUDGET {
        return Err(format!(
            "bbox at zoom {} resolves to {tile_count} tiles, over the {TILE_BUDGET} budget — \
             use a shallower zoom or a smaller bbox",
            region.zoom
        ));
    }
    let tiles = tiles_for_bbox(region.bbox, region.zoom, &extent);
    let ranges: Vec<Range<u32>> = tile_ranges_all(seg, &tiles);

    // Distinct tiles at one depth cover disjoint code ranges and a cell is one code, so no run
    // spans two tiles and per-tile scanning counts every region row exactly once. (A degenerate
    // bbox cannot repeat a tile: `tiles_for_bbox` enumerates a coordinate grid.)
    let codes = seg.morton.u32();
    let mut occupancy = Occupancy::default();
    let mut tiles_occupied = 0u64;
    let mut occupied_cells = 0u64;
    let mut sum_rows_per_cell = 0f64;
    for range in &ranges {
        let window = &codes[range.start as usize..range.end as usize];
        if window.is_empty() {
            continue;
        }
        let cells = occupancy.scan(window);
        tiles_occupied += 1;
        occupied_cells += cells;
        sum_rows_per_cell += window.len() as f64 / cells as f64;
    }
    eprintln!(
        "region pass: {} tiles, {tiles_occupied} occupied, {} rows",
        tiles.len(),
        occupancy.rows
    );

    Ok(json!({
        "bbox": region.bbox,
        "zoom": region.zoom,
        "tiles": tiles.len() as u64,
        "tiles_occupied": tiles_occupied,
        "mean_occupied_cells_per_tile": ratio(occupied_cells, tiles.len() as u64),
        "mean_rows_per_occupied_cell_per_tile": if tiles_occupied == 0 {
            0.0
        } else {
            sum_rows_per_cell / tiles_occupied as f64
        },
        "occupancy": occupancy.to_json(),
    }))
}

/// The cell-occupancy accumulator: exact `(cells, rows)` per distinct run length.
///
/// Memory is provably sub-linear without any bucketing loss: `D` distinct run lengths over `N`
/// rows satisfy `D(D+1)/2 ≤ N` (each distinct length occurs at least once), so `D ≤ √(2N)` —
/// under 45k map entries at 10⁹ rows. Exact per-length counts are what make the medians and
/// threshold fractions exact rather than bucket-resolution.
#[derive(Default)]
struct Occupancy {
    rows: u64,
    /// run length → (cells of that size, rows in them), ascending by length.
    by_len: BTreeMap<u32, (u64, u64)>,
}

impl Occupancy {
    /// Accumulate the runs of `codes` (a sorted slice — runs of equal codes are contiguous, so
    /// one forward pass finds every cell; no hashing, no per-cell allocation). Returns the
    /// number of runs (occupied cells) seen in this call.
    ///
    /// A caller splitting one column across several calls must split only at cell boundaries;
    /// `analyse_region`'s tile ranges satisfy that by construction (a cell is one code and a
    /// code belongs to exactly one tile at a given depth).
    fn scan(&mut self, codes: &[u32]) -> u64 {
        let mut cells = 0u64;
        let mut i = 0usize;
        while i < codes.len() {
            let code = codes[i];
            let mut j = i + 1;
            while j < codes.len() && codes[j] == code {
                j += 1;
            }
            let len = (j - i) as u32;
            let entry = self.by_len.entry(len).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += u64::from(len);
            self.rows += u64::from(len);
            cells += 1;
            i = j;
        }
        cells
    }

    fn distinct_codes(&self) -> u64 {
        self.by_len.values().map(|&(cells, _)| cells).sum()
    }

    /// The smallest run length whose cumulative weight (ascending by length) reaches
    /// `(total + 1) / 2` — the lower median under the given weighting.
    fn lower_median(&self, weight_of: impl Fn(&(u64, u64)) -> u64) -> Option<u32> {
        let total: u64 = self.by_len.values().map(&weight_of).sum();
        if total == 0 {
            return None;
        }
        let half = total.div_ceil(2);
        let mut cumulative = 0u64;
        for (&len, counts) in &self.by_len {
            cumulative += weight_of(counts);
            if cumulative >= half {
                return Some(len);
            }
        }
        unreachable!("cumulative weight reaches its own total");
    }

    fn to_json(&self) -> serde_json::Value {
        let cells_total = self.distinct_codes();
        let max_cell = self.by_len.keys().next_back().copied().unwrap_or(0);

        let mut histogram = [(0u64, 0u64); BUCKETS];
        for (&len, &(cells, rows)) in &self.by_len {
            let bucket = &mut histogram[bucket_index(len)];
            bucket.0 += cells;
            bucket.1 += rows;
        }
        let histogram_json: Vec<serde_json::Value> = histogram
            .iter()
            .enumerate()
            .map(|(idx, &(cells, rows))| {
                json!({
                    "bucket": bucket_label(idx),
                    "cells": cells,
                    "rows": rows,
                    "rows_fraction": ratio(rows, self.rows),
                })
            })
            .collect();

        let fractions: serde_json::Map<String, serde_json::Value> = GE_THRESHOLDS
            .iter()
            .map(|&threshold| {
                let rows_ge: u64 = self
                    .by_len
                    .range(threshold..)
                    .map(|(_, &(_, rows))| rows)
                    .sum();
                (threshold.to_string(), json!(ratio(rows_ge, self.rows)))
            })
            .collect();

        json!({
            "rows": self.rows,
            "distinct_codes": cells_total,
            "mean_rows_per_cell": ratio(self.rows, cells_total),
            "median_rows_per_cell": self.lower_median(|&(cells, _)| cells),
            "rows_weighted_median_cell_size": self.lower_median(|&(_, rows)| rows),
            "max_cell_size": max_cell,
            "rows_fraction_in_cells_ge": fractions,
            "histogram": histogram_json,
        })
    }
}

/// Bucket for a run length: 0 → {1}, 1 → {2}, then `(2^(k-1), 2^k]` per bucket `k`, saturating
/// at 17 (> 65536).
fn bucket_index(len: u32) -> usize {
    debug_assert!(len >= 1, "a run has at least one row");
    let bits = (32 - (len - 1).leading_zeros()) as usize;
    bits.min(BUCKETS - 1)
}

fn bucket_label(idx: usize) -> String {
    match idx {
        0 => "1".to_string(),
        1 => "2".to_string(),
        17 => ">65536".to_string(),
        _ => format!("{}-{}", (1u64 << (idx - 1)) + 1, 1u64 << idx),
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}
