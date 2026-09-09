//! The record every arm emits, and the JSONL writer that persists it.
//!
//! **`work` is a mandatory field, not an `Option`.** That is the single most important design
//! decision in this module, and it is a direct response to `probes/optimisations.md` §0: *"any
//! benchmark that varies cardinality while holding container structure constant will mislead;
//! the ratios that matter come from varying shape."* The cost model throughout Tessera is
//! O(containers touched), not O(cardinality), so a latency without a container count beside it
//! cannot distinguish a regression from a workload shift. Making the field non-optional means a
//! new arm cannot forget it — it will not compile.
//!
//! `env` exists for the same reason one layer up: every number here comes off WSL2 on a shared
//! 12-core box, and `probes/results.md` §1 is explicit that *"timings are indicative, not
//! certified — treat ratios as evidence and absolutes as a starting point."* A record that does
//! not say what it ran on cannot be compared with one from another day.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Bumped when a field's meaning changes, not when one is appended.
pub const SCHEMA_VERSION: u32 = 1;

/// One measured cell: one arm at one point in its parameter space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub schema: u32,
    pub run_id: String,
    /// Deterministic and human-readable, e.g.
    /// `viewport/2422486/categories-subclass/w10000/k30/z8/contig`. The resume ledger subtracts
    /// completed `cell_id`s from the planned set, so it must be stable across runs.
    pub cell_id: String,
    pub arm: String,
    pub scale: u64,
    pub label_set: String,
    /// Arm-specific axes. Free-form so arms need no schema changes here, but every key that
    /// appears must also appear in `cell_id` or the ledger cannot distinguish two cells.
    pub params: serde_json::Value,
    pub work: Work,
    pub timing: Timing,
    /// Per-stage breakdown when the arm went through `Engine::viewport` with `bench-timing` on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stages: Option<Stages>,
    pub normalised: Normalised,
    pub env: Env,
    pub status: String,
    /// Why a cell is `degenerate`, `generator_bound`, `suspect`, etc. Empty in the normal case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

/// What the cell actually did — the denominators every normalised cost divides by.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Work {
    /// Roaring containers spanned by the mask being exercised. **The primary work measure.**
    /// Union cost is measured linear in containers, with only ~2x per-container drift across
    /// 400x of scale (`probes/results.md`), so this is the number that makes a latency portable.
    pub containers: u64,
    /// Cardinality of the mask. Reported *beside* `containers`, never instead of it — two masks
    /// of equal cardinality can differ ~130x in cost through contiguity alone.
    pub mask_cardinality: u64,
    /// Fraction of the corpus this principal can see. Cells are compared at *equal coverage*;
    /// a 100%-coverage principal is degenerate (there is nothing to mask) and is flagged.
    pub coverage: f64,
    /// Morton run-length ratio against the 1/(1-p) random baseline. The flat-hash control must
    /// return exactly 1.00 — if it does not, the estimator is broken and no other ratio in the
    /// run is interpretable (`probes/results.md` §5).
    pub run_ratio: f64,
    pub tiles_resolved: u64,
    pub tiles_nonempty: u64,
    /// Sum of masked visible counts over tiles.
    pub sigma_visible: u64,
    /// Sum of `range.len()` over tiles — rows spanned, visible or not.
    /// `rows_in_ranges - sigma_visible` is C4's numerator.
    pub rows_in_ranges: u64,
    pub rows_materialised: u64,
    pub points_gathered: u64,
    /// Sub-cells §7.3's underlay evaluated — `tiles_nonempty x 4^offset`, not the number emitted.
    /// The gap between the two is work spent discovering emptiness. Zero when unrequested.
    #[serde(default)]
    pub underlay_cells_evaluated: u64,
    /// Distinct 4 KiB pages touched. Exact arithmetic, not an estimate: a row's byte offset in a
    /// fixed-width column is `index * width`.
    pub pages_touched: u64,
    pub bytes_touched: u64,
    /// True when the cell's mask covers the whole corpus. Such a cell measures the absence of
    /// masking, not masking, and must not be compared against a real principal.
    pub degenerate: bool,
}

/// Latencies. `min` is the headline — the probes' own convention, for the reason
/// `metrics::repeat` gives — and the full sample is kept so a distribution can be re-derived
/// without re-running.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Timing {
    pub n_repeats: u32,
    /// **The headline.** `probes/results.md` §6: *"min of 5 (a single-shot first pass reported
    /// figures 5-10x higher and was noise — repeat before believing)."* Min absorbs scheduler
    /// noise and page-cache warmth without needing a separate warm-up phase.
    pub min_ns: u64,
    pub median_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    /// The retained sample set, so the spread can be inspected without a re-run.
    ///
    /// Capped at [`MAX_RETAINED_SAMPLES`] — see [`Timing::from_samples`]. `n_repeats` always
    /// reports the true count, so a reader can tell that thinning happened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_ns: Vec<u64>,
}

/// Cap on retained per-sample data.
///
/// The load arm issues hundreds of thousands of requests per cell (28k rps x 8 s), and retaining
/// every one produced a 28 MB JSONL for a single sweep. Percentiles are computed from the
/// *complete* set before thinning, so nothing in the summary changes; only the retained detail is
/// bounded.
pub const MAX_RETAINED_SAMPLES: usize = 10_000;

impl Timing {
    /// Build from raw samples. Percentiles are nearest-rank, matching `scripts/bench_k_sweep.py`'s
    /// `percentile()` so numbers from the Rust and Python harnesses are directly comparable.
    ///
    /// Above [`MAX_RETAINED_SAMPLES`] the retained set is thinned by taking every n-th value from
    /// the *sorted* samples. That preserves the shape of the distribution — including both tails,
    /// which a head-truncation would discard and which are the only interesting part of a latency
    /// sample — while bounding the record size.
    pub fn from_samples(mut samples: Vec<u64>) -> Self {
        assert!(!samples.is_empty(), "a cell must have at least one sample");
        let n = samples.len();
        samples.sort_unstable();
        let pick = |p: f64| {
            let idx = ((n as f64 * p) as usize).min(n - 1);
            samples[idx]
        };
        let (min_ns, median_ns, p99_ns, max_ns) =
            (samples[0], pick(0.5), pick(0.99), samples[n - 1]);

        let retained = if n > MAX_RETAINED_SAMPLES {
            let step = n.div_ceil(MAX_RETAINED_SAMPLES);
            let mut thinned: Vec<u64> = samples.iter().copied().step_by(step).collect();
            // Stepping can drop the final element; the maximum is the one sample never to lose.
            if thinned.last() != Some(&max_ns) {
                thinned.push(max_ns);
            }
            thinned
        } else {
            samples
        };

        Timing {
            n_repeats: n as u32,
            min_ns,
            median_ns,
            p99_ns,
            max_ns,
            all_ns: retained,
        }
    }
}

/// Per-stage nanoseconds, mirroring `tessera_engine::StageTimings` plus the server-owned
/// serialise step. Shares its field order with the `x-tessera-stage-ns` header.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stages {
    pub generation_resolve_ns: u64,
    pub stamp_compare_ns: u64,
    pub view_lookup_ns: u64,
    pub row_projection_ns: u64,
    pub compose_ns: u64,
    /// §7.2's θ anchor. `#[serde(default)]` so runs recorded before this stage existed still parse.
    #[serde(default)]
    pub theta_anchor_ns: u64,
    /// §7.2's `N_occ(d)` walk — θ's second anchor, zero on a memo hit and for a filtered request.
    #[serde(default)]
    pub theta_occupancy_ns: u64,
    pub tiles_for_bbox_ns: u64,
    pub tile_ranges_ns: u64,
    pub count_ns: u64,
    pub select_ns: u64,
    pub gather_ns: u64,
    /// §7.3's density underlay. Zero unless the request asked for it.
    #[serde(default)]
    pub underlay_ns: u64,
    #[serde(default)]
    pub arrow_serialise_ns: u64,
    pub total_ns: u64,
    pub unattributed_ns: u64,
    /// Clock reads taken, times the per-lap cost from `calibrate` — the perturbation this
    /// instrumentation introduced, reported so a reader can subtract it rather than assume zero.
    pub clock_laps: u64,
    pub clock_overhead_ns: u64,
    pub row_projection_built: bool,
}

impl Stages {
    pub fn from_engine(t: &tessera_engine::StageTimings, clock_lap_ns: u64) -> Self {
        Stages {
            generation_resolve_ns: t.generation_resolve_ns,
            stamp_compare_ns: t.stamp_compare_ns,
            view_lookup_ns: t.view_lookup_ns,
            row_projection_ns: t.row_projection_ns,
            compose_ns: t.compose_ns,
            theta_anchor_ns: t.theta_anchor_ns,
            theta_occupancy_ns: t.theta_occupancy_ns,
            tiles_for_bbox_ns: t.tiles_for_bbox_ns,
            tile_ranges_ns: t.tile_ranges_ns,
            count_ns: t.count_ns,
            select_ns: t.select_ns,
            gather_ns: t.gather_ns,
            underlay_ns: t.underlay_ns,
            arrow_serialise_ns: 0,
            total_ns: t.total_ns,
            unattributed_ns: t.unattributed_ns(),
            clock_laps: t.clock_laps,
            clock_overhead_ns: t.clock_laps * clock_lap_ns,
            row_projection_built: t.row_projection_built,
        }
    }
}

/// **The gated quantities.** The tail-attribution memo's central recommendation: these stay
/// near-flat where raw p99 swings wildly, so a move here is the system getting slower, whereas a
/// move in p99 is usually the input mix changing. Gate on these, report p99 for the dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Normalised {
    pub ns_per_row_visible: f64,
    pub ns_per_point_gathered: f64,
    pub ns_per_tile: f64,
    pub ns_per_container: f64,
    pub ns_per_row_materialised: f64,
    /// Cost per sub-cell *evaluated* (not emitted) by §7.3's underlay. Zero when unrequested.
    ///
    /// Gated as its own quantity because the underlay's cost scales with a **request parameter**
    /// (`4^offset`) rather than with the corpus or the viewer's coverage, so it moves independently
    /// of every other normalised figure here and would otherwise contaminate them.
    #[serde(default)]
    pub ns_per_underlay_cell: f64,
}

impl Normalised {
    pub fn derive(min_ns: u64, work: &Work) -> Self {
        let per = |d: u64| {
            if d == 0 {
                0.0
            } else {
                min_ns as f64 / d as f64
            }
        };
        Normalised {
            ns_per_row_visible: per(work.sigma_visible),
            ns_per_point_gathered: per(work.points_gathered),
            ns_per_tile: per(work.tiles_nonempty),
            ns_per_container: per(work.containers),
            ns_per_row_materialised: per(work.rows_materialised),
            ns_per_underlay_cell: per(work.underlay_cells_evaluated),
        }
    }
}

/// What the cell ran on. Captured once per process and cloned into every record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Env {
    pub git_sha: String,
    pub dirty: bool,
    pub profile: String,
    pub features: Vec<String>,
    pub cores: usize,
    pub mem_gib: u64,
    pub free_ram_gib_before: u64,
    /// False once the bundle exceeds what the page cache can hold alongside the process. When
    /// false, absolute latencies are a page-cache lottery and only ratios survive.
    pub bundle_fits_in_ram: bool,
    pub major_faults_delta: u64,
    pub minor_faults_delta: u64,
    pub rss_peak_kib: u64,
    pub load_avg: f64,
}

/// Append-only JSONL sink. One line per cell, flushed per write so a killed run keeps everything
/// it had already measured — the resume ledger depends on that being true.
pub struct Writer {
    out: BufWriter<File>,
}

impl Writer {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Writer {
            out: BufWriter::new(file),
        })
    }

    pub fn write(&mut self, record: &Record) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.out, record).map_err(std::io::Error::other)?;
        self.out.write_all(b"\n")?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank_matching_the_python_harness() {
        // bench_k_sweep.py: sorted(v)[min(int(len*p), len-1)]
        let t = Timing::from_samples(vec![10, 20, 30, 40, 50]);
        assert_eq!(t.min_ns, 10);
        assert_eq!(t.max_ns, 50);
        assert_eq!(t.median_ns, 30); // int(5*0.5) = 2 -> sorted[2]
        assert_eq!(t.p99_ns, 50); // int(5*0.99) = 4 -> sorted[4]
    }

    #[test]
    fn normalised_costs_do_not_divide_by_zero() {
        let n = Normalised::derive(1_000, &Work::default());
        assert_eq!(n.ns_per_row_visible, 0.0);
        assert_eq!(n.ns_per_container, 0.0);
    }

    #[test]
    fn normalised_costs_divide_by_the_right_denominator() {
        let work = Work {
            sigma_visible: 100,
            tiles_nonempty: 10,
            containers: 4,
            ..Default::default()
        };
        let n = Normalised::derive(1_000, &work);
        assert_eq!(n.ns_per_row_visible, 10.0);
        assert_eq!(n.ns_per_tile, 100.0);
        assert_eq!(n.ns_per_container, 250.0);
    }
}
