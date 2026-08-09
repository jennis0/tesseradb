//! What a filter costs when it **matches a lot** — the axis every other arm holds low.
//!
//! Arms 1–6 all use predicates that match a small fraction of the candidate, because they were
//! measuring the traversal. But a filter surface issues unselective predicates routinely: a range
//! covering most of a domain, a tick-box set with everything ticked, a prefix a third of the corpus
//! shares. Those are the shapes where the *result* rather than the scan sets the cost.
//!
//! Reports peak RSS as well as time, because the failure mode here is memory rather than latency:
//! the result is accumulated before it becomes a bitmap, so a filter matching a quarter of a 10⁹
//! corpus is a multi-hundred-megabyte allocation on a request path — one the compute-admission gate
//! rations CPU for and knows nothing about.

use std::time::Instant;

use croaring::Bitmap;
use tessera_filter::{Codes, Endpoint, Scalar, ValueColumn};

/// Peak resident set size in bytes, from `/proc/self/status`'s `VmHWM` — the high-water mark, which
/// is what a transient allocation shows up in and `statm`'s current figure does not.
fn peak_rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("linux");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .expect("a number")
                .parse()
                .expect("kb");
            return kb * 1024;
        }
    }
    0
}

/// Reset the high-water mark so each arm reports its own peak rather than the run's.
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

#[inline]
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args
        .get(1)
        .map(|s| s.parse().expect("n"))
        .unwrap_or(1_000_000_000);

    let values: Vec<u8> = (0..n).map(|e| (splitmix(e) % 100) as u8).collect();
    let column = ValueColumn::universal(Codes::U8(values.into()));

    let mut broad = Bitmap::new();
    broad.add_range(0u32..(n / 4) as u32);
    broad.run_optimize();
    let mut all = Bitmap::new();
    all.add_range(0u32..n as u32);
    all.run_optimize();

    println!("n,candidate,predicate,selectivity_pct,candidate_entities,ms,hits,peak_rss_mb");

    // Selectivity is the *share of the candidate that matches* — the axis under test. `lt` bounds
    // pick it: values are uniform over 0..100, so `< 50` matches half.
    for (cname, cand) in [("broad-25pct", &broad), ("all", &all)] {
        for (label, hi) in [
            ("range-1pct", 1i128),
            ("range-25pct", 25),
            ("range-50pct", 50),
            ("range-100pct", 100),
        ] {
            let bound = Endpoint {
                value: Scalar::Int(hi),
                inclusive: false,
            };
            let run = || column.scan_range(cand, None, Some(bound));
            let _ = run();
            reset_peak();
            let base = peak_rss_bytes();
            let t = Instant::now();
            let hits = run();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let peak = peak_rss_bytes().saturating_sub(base);
            println!(
                "{n},{cname},{label},{hi},{},{ms:.1},{},{:.0}",
                cand.cardinality(),
                hits.cardinality(),
                peak as f64 / 1e6
            );
        }
    }
}
