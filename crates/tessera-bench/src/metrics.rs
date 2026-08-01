//! Measurement primitives: repetition policy, container counting, the run-ratio estimator, and
//! the `/proc` counters that decide whether a cell is trustworthy.

use std::path::Path;

use croaring::Bitmap;

/// Run `f` `reps` times and return every sample.
///
/// **Min-of-N is the reporting convention, and the repetition is not optional.**
/// `probes/results.md` §6 records the reason in one line: *"a single-shot first pass reported
/// figures 5-10x higher and was noise — repeat before believing."* The probes used N=3 generally
/// and N=5 at the largest scale. Every sample is returned rather than only the minimum so the spread
/// stays inspectable — a cell whose min and max differ by 10x is telling you something even when
/// its min looks fine.
pub fn repeat<T, F: FnMut() -> T>(reps: u32, mut f: F) -> Vec<u64> {
    assert!(reps > 0, "a cell needs at least one repetition");
    let mut samples = Vec::with_capacity(reps as usize);
    for _ in 0..reps {
        let start = std::time::Instant::now();
        let out = f();
        let elapsed = start.elapsed().as_nanos() as u64;
        // Keep the result alive across the timer so the optimiser cannot delete the work.
        std::hint::black_box(out);
        samples.push(elapsed);
    }
    samples
}

/// Roaring containers a bitmap spans: the count of distinct high 16 bits among its values.
///
/// **This is the cost model's unit.** The probes measured union cost as linear in containers spanned
/// with only ~2x per-container drift across 400x of scale, and found two regimes differing 13x in
/// per-container constant (sparse/array ~114 ns, dense/bitmap ~1.54 us). A latency reported
/// without this number cannot be told apart from a workload that simply moved.
///
/// Mirrors the probes' own definition (`containers(bm) = len(unique(asarray(bm) >> 16))`)
/// so figures are comparable with `probes/results.md`.
pub fn containers(bitmap: &Bitmap) -> u64 {
    let mut count = 0u64;
    let mut last_high: Option<u32> = None;
    // Roaring iteration is ascending, so a change in the high half is a new container and no set
    // is needed — this stays O(cardinality) in time and O(1) in space.
    for value in bitmap.iter() {
        let high = value >> 16;
        if last_high != Some(high) {
            count += 1;
            last_high = Some(high);
        }
    }
    count
}

/// Mean run length of set bits, over the baseline a random set of the same density would give.
///
/// The baseline for a uniformly random set of density `p` is `1/(1-p)`. A ratio of 1.00 means no
/// clustering beyond chance.
///
/// **Use this as a control, not only as a measurement.** `probes/results.md` §5 established that
/// the flat-hash corpus returns *exactly* 1.00 — so if a run's hash-flat cell returns anything
/// else, the estimator or the corpus is broken and every other ratio in that run is
/// uninterpretable. That is gate G0b, and it aborts before anything is reported.
pub fn run_ratio(bitmap: &Bitmap, universe: u64) -> f64 {
    let cardinality = bitmap.cardinality();
    if cardinality == 0 || universe == 0 {
        return 0.0;
    }
    let mut runs = 0u64;
    let mut prev: Option<u32> = None;
    for value in bitmap.iter() {
        if prev.map(|p| value != p + 1).unwrap_or(true) {
            runs += 1;
        }
        prev = Some(value);
    }
    if runs == 0 {
        return 0.0;
    }
    let mean_run = cardinality as f64 / runs as f64;
    let p = cardinality as f64 / universe as f64;
    if p >= 1.0 {
        // Every bit set: one run, and the baseline is infinite. Report 1.0 rather than a
        // divide-by-zero — the cell is degenerate anyway and is flagged as such.
        return 1.0;
    }
    let baseline = 1.0 / (1.0 - p);
    mean_run / baseline
}

/// Distinct 4 KiB pages a set of row indices touches in one fixed-width column.
///
/// Exact, not estimated: a row's byte offset is `index * width`, so the page is
/// `index * width / 4096`. `rows` must be ascending, which every caller's selection already is.
/// This is what makes the gather probe's access-pattern axis meaningful — contiguous, strided and
/// scattered selections of the *same size* touch wildly different page counts, and that
/// difference is the thing being measured.
pub fn pages_touched(rows: &[u32], width_bytes: u64) -> u64 {
    const PAGE: u64 = 4096;
    let mut pages = 0u64;
    let mut last: Option<u64> = None;
    for &row in rows {
        let first = row as u64 * width_bytes / PAGE;
        let last_page = ((row as u64 + 1) * width_bytes - 1) / PAGE;
        for page in first..=last_page {
            if last != Some(page) {
                pages += 1;
                last = Some(page);
            }
        }
    }
    pages
}

/// Minor and major page faults for this process, from `/proc/self/stat` fields 10 and 12.
///
/// Major faults are the honest ones: a "warm" cell that took major faults was reading from disk,
/// and its latency is a page-cache artefact rather than a property of the code. Cells are flagged
/// suspect on that basis and excluded from gating — see `Env::bundle_fits_in_ram`.
pub fn faults() -> (u64, u64) {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0);
    };
    // The comm field may contain spaces inside parentheses; everything after `) ` is positional.
    let Some(rest) = stat.rsplit_once(") ") else {
        return (0, 0);
    };
    let fields: Vec<&str> = rest.1.split_whitespace().collect();
    // After `) `, index 0 is state (field 3). minflt is field 10 -> index 7; majflt is 12 -> 9.
    let minor = fields.get(7).and_then(|v| v.parse().ok()).unwrap_or(0);
    let major = fields.get(9).and_then(|v| v.parse().ok()).unwrap_or(0);
    (minor, major)
}

/// Peak resident set in KiB, from `/proc/self/status`'s `VmHWM`.
pub fn rss_peak_kib() -> u64 {
    read_status_kib("VmHWM:")
}

/// Current resident set in KiB, from `VmRSS`.
///
/// The concurrency arm's Arm A samples this after each authorised session to build the
/// RSS-vs-N curve, which is that arm's actual result.
#[allow(dead_code)]
pub fn rss_current_kib() -> u64 {
    read_status_kib("VmRSS:")
}

fn read_status_kib(key: &str) -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Total and available memory in GiB, from `/proc/meminfo`.
pub fn memory_gib() -> (u64, u64) {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let field = |key: &str| -> u64 {
        info.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            / (1024 * 1024)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

pub fn load_avg() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0)
}

/// Total bytes under `path`, following no symlinks.
pub fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_bytes(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// Cost of one `Instant::now()` pair, measured rather than assumed.
///
/// The viewport probe takes ~4 clock reads per tile, so at ~300 tiles the instrumentation itself
/// costs ~1200 reads. That is real perturbation and the reports subtract it explicitly instead of
/// pretending it is free.
pub fn clock_lap_ns() -> u64 {
    const ROUNDS: u32 = 100_000;
    let start = std::time::Instant::now();
    for _ in 0..ROUNDS {
        std::hint::black_box(std::time::Instant::now());
    }
    start.elapsed().as_nanos() as u64 / ROUNDS as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_counts_distinct_high_halves() {
        let mut b = Bitmap::new();
        b.add(1);
        b.add(2);
        b.add(65_536); // second container
        b.add(65_537);
        b.add(200_000); // third
        assert_eq!(containers(&b), 3);
    }

    #[test]
    fn containers_of_empty_is_zero() {
        assert_eq!(containers(&Bitmap::new()), 0);
    }

    #[test]
    fn run_ratio_of_a_fully_contiguous_block_exceeds_one() {
        // 1000 consecutive values in a universe of 1e6: density 0.001, baseline ~1.001,
        // mean run 1000 -> ratio ~999.
        let b = Bitmap::from_range(0..1000);
        let r = run_ratio(&b, 1_000_000);
        assert!(
            r > 900.0,
            "contiguous block should be highly clustered: {r}"
        );
    }

    #[test]
    fn run_ratio_of_a_maximally_scattered_set_is_about_one() {
        // Every other value: runs of length 1, density 0.5, baseline 2.0 -> ratio 0.5.
        // Not 1.0, because alternating is *less* clustered than random. The control that must
        // land on exactly 1.00 is the hash-flat corpus, not this synthetic.
        let mut b = Bitmap::new();
        for v in (0..1000).step_by(2) {
            b.add(v);
        }
        let r = run_ratio(&b, 1000);
        assert!((r - 0.5).abs() < 0.01, "expected ~0.5, got {r}");
    }

    #[test]
    fn pages_touched_contiguous_vs_scattered_differ_as_the_probe_needs() {
        // 8-byte column, 4096-byte pages -> 512 rows per page.
        let contiguous: Vec<u32> = (0..512).collect();
        assert_eq!(pages_touched(&contiguous, 8), 1);

        // Same row count, one per page: 512 pages.
        let scattered: Vec<u32> = (0..512).map(|i| i * 512).collect();
        assert_eq!(pages_touched(&scattered, 8), 512);
    }

    #[test]
    fn repeat_returns_one_sample_per_repetition() {
        let samples = repeat(3, || 1 + 1);
        assert_eq!(samples.len(), 3);
    }

    #[test]
    fn proc_counters_are_readable_on_this_platform() {
        // Not asserting values — only that parsing does not silently yield zeros everywhere,
        // which would make every `env` block a lie.
        assert!(rss_current_kib() > 0, "VmRSS should be readable");
        let (total, _) = memory_gib();
        assert!(total > 0, "MemTotal should be readable");
    }
}
