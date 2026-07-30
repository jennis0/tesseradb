//! **The gather access-pattern probe** — discharging `probes/optimisations.md` §3.5's standing
//! debt: *"the gather was never measured — PROBE OWED. Phase 0 measured every bitmap primitive
//! and no column read. The whole selection path downstream of `range_cardinality` is therefore
//! modelled, not observed."*
//!
//! # Why this is priority-independent
//!
//! This arm **never calls `Engine::viewport` or `sample_tile`**. It opens a segment's columns
//! directly, builds its own mask, and supplies its own row selection. Whatever sampler ships —
//! the real `priority = splitmix64(entity) >> 48` one currently in flight, or anything after it —
//! cannot change these numbers, because the thing being measured is *the shape of the memory
//! access*, not which rows a policy chose.
//!
//! That matters because today's placeholder `sample_tile` takes the first `k` rows in storage
//! order: a contiguous forward run, the cheapest gather that exists. Benchmarking only it would
//! understate every realistic sampler. The real priority key is uncorrelated with row order by
//! construction (it is a keyed Feistel of the entity id), so a real sampler's access pattern is
//! `scattered` — and the gap between `contiguous` and `scattered` here is the cost the current
//! placeholder is hiding.
//!
//! # Axes
//!
//! * **pattern** — the axis this probe exists for (see [`Pattern`]).
//! * **k** — rows gathered. `full_scan` ignores it and takes everything visible, because that is
//!   the direct-evaluation priority read, the route `probes/results.md` §5 found handles 12–99%
//!   of occupied tiles.
//! * **range rows** — how much of the segment the tile spans, i.e. the "points in cell" axis.
//! * **coverage** — how much of that range the principal can see.
//! * **columns** — `xy` through `full`, testing design §10.4's "ten pages per column" claim.
//!
//! Held constant within a cell: the segment, the mask, the row range, and **the number of rows
//! gathered**. Only the access pattern varies. That is what makes the comparison mean anything.

use croaring::Bitmap;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;

use tessera_authz::PostingsReader;
use tessera_store::read::{open_bundle, ColumnsRef};

use crate::arms::{Context, Result};
use crate::corpus::{build_grant_to_coverage, GrantShape, TermStats};
use crate::report::Work;

/// How the `k` gathered rows are distributed through the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// First `k` visible rows. **Today's placeholder sampler**, and the lower-bound control: one
    /// forward run, maximal page reuse, maximal readahead benefit.
    Contiguous,
    /// Every `ceil(visible/k)`-th visible row. Models any spatially-uniform sampler.
    Strided,
    /// `k` visible rows drawn uniformly, then **sorted ascending**. Models the real priority
    /// sampler, whose key is uncorrelated with row order.
    Scattered,
    /// The same rows in draw order. Isolates what sorting buys — design §10.4 claims a sorted
    /// gather is "forward-sequential-with-gaps and cooperates with readahead", and this is the
    /// arm that tests it rather than repeating it.
    ScatteredUnsorted,
    /// Every visible row in the range. The direct-evaluation priority read — and, not
    /// incidentally, exactly what `EffectiveMask::iter_range` does today for every tile
    /// regardless of `k` (finding F1).
    FullScan,
}

impl Pattern {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "contiguous" | "contig" => Some(Pattern::Contiguous),
            "strided" => Some(Pattern::Strided),
            "scattered" => Some(Pattern::Scattered),
            "scattered-unsorted" => Some(Pattern::ScatteredUnsorted),
            "full-scan" | "fullscan" => Some(Pattern::FullScan),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Pattern::Contiguous => "contiguous",
            Pattern::Strided => "strided",
            Pattern::Scattered => "scattered",
            Pattern::ScatteredUnsorted => "scattered-unsorted",
            Pattern::FullScan => "full-scan",
        }
    }
}

/// Which columns the gather reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Columns {
    /// `x`, `y` — 8 B/row.
    Xy,
    /// `x`, `y`, `tessera_id` — 16 B/row. What the real viewport path reads.
    XyId,
    /// Adds `priority` — 18 B/row, the r21 residency figure.
    XyIdPriority,
}

impl Columns {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "xy" => Some(Columns::Xy),
            "xy+id" => Some(Columns::XyId),
            "xy+id+priority" | "full" => Some(Columns::XyIdPriority),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Columns::Xy => "xy",
            Columns::XyId => "xy+id",
            Columns::XyIdPriority => "xy+id+priority",
        }
    }
    /// Total bytes per row across the columns read — the denominator for bandwidth, and the
    /// width `pages_touched` uses per column.
    fn widths(&self) -> &'static [u64] {
        match self {
            Columns::Xy => &[4, 4],
            Columns::XyId => &[4, 4, 8],
            Columns::XyIdPriority => &[4, 4, 8, 2],
        }
    }
}

/// Pick the rows this cell will gather, given the visible rows in the range.
///
/// `visible` is ascending. The returned selection has exactly `min(k, visible.len())` entries for
/// every pattern except `FullScan`, so the *amount* of gathering is held constant and only its
/// distribution varies.
fn select(pattern: Pattern, visible: &[u32], k: usize, seed: u64) -> Vec<u32> {
    let n = visible.len();
    if n == 0 {
        return Vec::new();
    }
    match pattern {
        Pattern::FullScan => visible.to_vec(),
        Pattern::Contiguous => visible.iter().copied().take(k).collect(),
        Pattern::Strided => {
            let step = n.div_ceil(k.max(1)).max(1);
            visible.iter().copied().step_by(step).take(k).collect()
        }
        Pattern::Scattered | Pattern::ScatteredUnsorted => {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut pool: Vec<u32> = visible.to_vec();
            pool.shuffle(&mut rng);
            pool.truncate(k.min(n));
            if pattern == Pattern::Scattered {
                pool.sort_unstable();
            }
            pool
        }
    }
}

/// The gather itself: read the selected rows out of the mmapped columns.
///
/// Deliberately mirrors `tessera_engine::viewport::row_to_point`'s access shape — index into each
/// column slice per row — without constructing a `PointOut`, so what is measured is the column
/// reads and not the `Vec<PointOut>` allocation that sits on top of them in the real path. That
/// allocation is real and is attributed separately by the stage timer's `gather_ns`.
#[inline]
fn gather(columns: &ColumnsRef, rows: &[u32], which: Columns) -> u64 {
    let ids = columns.tessera_id();
    let xs = columns.x();
    let ys = columns.y();
    let mut acc = 0u64;
    match which {
        Columns::Xy => {
            for &r in rows {
                let i = r as usize;
                acc = acc.wrapping_add(xs[i].to_bits() as u64 ^ ys[i].to_bits() as u64);
            }
        }
        Columns::XyId => {
            for &r in rows {
                let i = r as usize;
                acc = acc
                    .wrapping_add(xs[i].to_bits() as u64 ^ ys[i].to_bits() as u64)
                    .wrapping_add(ids[i]);
            }
        }
        Columns::XyIdPriority => {
            let ps = columns.priority();
            for &r in rows {
                let i = r as usize;
                acc = acc
                    .wrapping_add(xs[i].to_bits() as u64 ^ ys[i].to_bits() as u64)
                    .wrapping_add(ids[i])
                    .wrapping_add(ps[i] as u64);
            }
        }
    }
    acc
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    ctx: &Context,
    patterns: &[String],
    ks: &[usize],
    coverages: &[f64],
    range_rows: &[u32],
    columns: &[String],
    seed: u64,
) -> Result<()> {
    let mut run = ctx.open("gather")?;

    let patterns: Vec<Pattern> = patterns
        .iter()
        .map(|p| Pattern::parse(p).ok_or_else(|| format!("unknown pattern {p:?}")))
        .collect::<std::result::Result<_, _>>()?;
    let columns: Vec<Columns> = columns
        .iter()
        .map(|c| Columns::parse(c).ok_or_else(|| format!("unknown column set {c:?}")))
        .collect::<std::result::Result<_, _>>()?;

    for fixture in &ctx.fixtures {
        let postings = PostingsReader::open(&fixture.postings_path(), true)?;
        let stats = TermStats::compute(&postings)?;
        let bundle = open_bundle(&fixture.root)?;
        let Some(partition) = bundle.partitions.values().next() else {
            continue;
        };
        let Some(slice) = partition.slices.values().next() else {
            continue;
        };
        let Some(segment) = slice.segments.first() else {
            continue;
        };
        let total_rows = segment.columns.row_count();

        for &coverage_target in coverages {
            let (grant, coverage) = build_grant_to_coverage(
                &stats,
                &postings,
                GrantShape::Random,
                coverage_target,
                fixture.scale,
                seed,
            )?;
            if grant.terms.is_empty() {
                continue;
            }
            // Entity space -> row space, once, outside every timed loop (I4's only bridge).
            let entity_mask = crate::postings::union(&postings, &grant.terms)?;
            let row_mask: Bitmap = slice.permutation.project(&entity_mask);
            let containers = crate::metrics::containers(&row_mask);
            let run_ratio = crate::metrics::run_ratio(&row_mask, total_rows as u64);

            for &rows_in_range in range_rows {
                if rows_in_range > total_rows {
                    continue;
                }
                // A range anchored at the segment's midpoint: away from the edges, where row
                // density is least representative.
                let lo = (total_rows / 2).saturating_sub(rows_in_range / 2);
                let hi = (lo + rows_in_range).min(total_rows);
                let visible: Vec<u32> = row_mask.and(&Bitmap::from_range(lo..hi)).to_vec();
                if visible.is_empty() {
                    continue;
                }

                for &which in &columns {
                    for &pattern in &patterns {
                        for &k in ks {
                            let selection = select(pattern, &visible, k, seed);
                            if selection.is_empty() {
                                continue;
                            }

                            let cell_id = format!(
                                "gather/{}/{}/cov{:.4}/rows{}/{}/{}/k{}",
                                fixture.scale,
                                fixture.label_set,
                                coverage_target,
                                rows_in_range,
                                which.name(),
                                pattern.name(),
                                k
                            );
                            if run.ledger.is_done(&cell_id) {
                                run.skipped += 1;
                                continue;
                            }

                            let (minor_before, major_before) = crate::metrics::faults();
                            let samples = crate::metrics::repeat(ctx.repeat, || {
                                gather(&segment.columns, &selection, which)
                            });
                            let (minor_after, major_after) = crate::metrics::faults();

                            // Exact, not estimated: a row's byte offset in a fixed-width column
                            // is `index * width`. Summed across the columns actually read.
                            // Requires ascending rows, so the unsorted arm is sorted first — the
                            // page *set* is the same either way, only the order differs, and it
                            // is the order this arm is measuring.
                            let mut ascending = selection.clone();
                            ascending.sort_unstable();
                            let pages: u64 = which
                                .widths()
                                .iter()
                                .map(|w| crate::metrics::pages_touched(&ascending, *w))
                                .sum();
                            let bytes: u64 =
                                which.widths().iter().sum::<u64>() * selection.len() as u64;

                            let mut env = run.env.clone();
                            env.minor_faults_delta = minor_after.saturating_sub(minor_before);
                            env.major_faults_delta = major_after.saturating_sub(major_before);

                            let mut flags = Vec::new();
                            // A cell that took major faults read from disk; its latency is a
                            // page-cache artefact, not a property of the access pattern.
                            if env.major_faults_delta > 0 {
                                flags.push("major_faults".to_string());
                            }
                            if selection.len() < k && pattern != Pattern::FullScan {
                                flags.push(format!("k_clamped={}", selection.len()));
                            }

                            let work = Work {
                                containers,
                                mask_cardinality: entity_mask.cardinality(),
                                coverage,
                                run_ratio,
                                tiles_resolved: 1,
                                tiles_nonempty: 1,
                                sigma_visible: visible.len() as u64,
                                rows_in_ranges: rows_in_range as u64,
                                rows_materialised: visible.len() as u64,
                                points_gathered: selection.len() as u64,
                                pages_touched: pages,
                                bytes_touched: bytes,
                                degenerate: coverage >= 0.999,
                            };

                            run.emit(
                                cell_id,
                                fixture,
                                serde_json::json!({
                                    "pattern": pattern.name(),
                                    "k": k,
                                    "k_actual": selection.len(),
                                    "columns": which.name(),
                                    "range_rows": rows_in_range,
                                    "visible_in_range": visible.len(),
                                    "coverage_target": coverage_target,
                                    "coverage_actual": coverage,
                                    "bytes_per_row": which.widths().iter().sum::<u64>(),
                                    "seed": seed,
                                }),
                                work,
                                samples,
                                None,
                                flags,
                            )?;
                        }
                    }
                }
            }
        }
    }

    run.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible() -> Vec<u32> {
        (0..1000).map(|i| i * 3).collect()
    }

    #[test]
    fn every_pattern_except_full_scan_gathers_exactly_k_rows() {
        // The invariant that makes the comparison meaningful: the *amount* of gathering is held
        // constant across patterns, so a difference in time is a difference in access shape.
        let v = visible();
        for pattern in [
            Pattern::Contiguous,
            Pattern::Strided,
            Pattern::Scattered,
            Pattern::ScatteredUnsorted,
        ] {
            let s = select(pattern, &v, 50, 7);
            assert_eq!(s.len(), 50, "{:?} should gather exactly k", pattern);
        }
        assert_eq!(select(Pattern::FullScan, &v, 50, 7).len(), v.len());
    }

    #[test]
    fn contiguous_is_a_prefix_and_strided_spans_the_range() {
        let v = visible();
        let c = select(Pattern::Contiguous, &v, 10, 0);
        assert_eq!(
            c,
            v[..10].to_vec(),
            "contiguous is the storage-order prefix"
        );

        let s = select(Pattern::Strided, &v, 10, 0);
        assert!(
            *s.last().unwrap() > *c.last().unwrap(),
            "strided must reach further into the range than a prefix does"
        );
    }

    #[test]
    fn scattered_is_sorted_and_unsorted_is_not_but_they_touch_the_same_rows() {
        let v = visible();
        let sorted = select(Pattern::Scattered, &v, 100, 42);
        let unsorted = select(Pattern::ScatteredUnsorted, &v, 100, 42);
        assert!(sorted.windows(2).all(|w| w[0] <= w[1]), "scattered ascends");

        let mut a = sorted.clone();
        let mut b = unsorted.clone();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "same seed, same rows — only the order differs");
        assert_ne!(sorted, unsorted, "and the order really does differ");
    }

    #[test]
    fn selections_are_deterministic_under_a_seed() {
        let v = visible();
        assert_eq!(
            select(Pattern::Scattered, &v, 50, 3),
            select(Pattern::Scattered, &v, 50, 3)
        );
        assert_ne!(
            select(Pattern::Scattered, &v, 50, 3),
            select(Pattern::Scattered, &v, 50, 4)
        );
    }

    #[test]
    fn every_selected_row_is_actually_visible() {
        // A gather that reads an unmasked row would be an I2 violation in the shape of a
        // benchmark. Cheap to assert, so assert it.
        let v = visible();
        let set: std::collections::HashSet<u32> = v.iter().copied().collect();
        for pattern in [
            Pattern::Contiguous,
            Pattern::Strided,
            Pattern::Scattered,
            Pattern::ScatteredUnsorted,
            Pattern::FullScan,
        ] {
            for row in select(pattern, &v, 50, 1) {
                assert!(set.contains(&row), "{pattern:?} selected an invisible row");
            }
        }
    }
}
