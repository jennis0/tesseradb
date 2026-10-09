//! **What bounds ingest throughput.**
//!
//! ## 1. The buffer clone (F3)
//!
//! `apply_window` deep-copies the whole ingest buffer per commit-window close. With `B` rows
//! buffered between flushes and a close every `W`:
//!
//! ```text
//! Σ copies = W · (0 + 1 + … + (B/W − 1)) = B²/2W     per flush interval
//! per-row cost ∝ B/W
//! ```
//!
//! A sweep across a 24x change in `B/W` is clean once the noise floor is below the clone's
//! ~2.3 µs signal —
//! **3.10 → 3.34 → 5.27 → 6.45 µs/row at 0.0/1.5/5.5/11.5 predicted copies per row**, a slope of
//! **291 ns per item copied**, against 200–220 ns measured independently in
//! `probes/2026-07-31-ingest-baseline`. F3 is confirmed, and at ~37% of ingest cost it was then the
//! **largest single stage**.
//!
//! It is not any more, and the law is untouched — only its constant. A `BufferedItem` behind an
//! `Arc` makes each copy a pointer rather than four heap allocations; the clone still costs
//! `B²/2W`, now at ~13 ns per unit of `B/W` and **0–9% of ingest**, and `apply_window`'s per-row
//! loop is the largest stage at 38–50%
//! (`docs/evidence/memos/2026-08-05-ingest-rate.md` §2).
//!
//! The lesson worth keeping: a flat sweep refutes nothing until you know the noise floor is below
//! the effect you are looking for.
//!
//! ## 2. Window size has an optimum, and it is not "as large as possible"
//!
//! Batching amortises per-call overhead and reduces clone count, but `assign_sorted` is `n log n`
//! in the window's rows — which `config.rs` warns about at `DEFAULT_COMMIT_WINDOW_MAX_ITEMS`
//! ("not a free dial in the compression direction"). The two fight, and the measured curve turns:
//! **6.94 → 4.43 → 4.92 → 7.90 µs/row at W = 10k / 40k / 120k / 240k.** One 240,000-row window is
//! *worse* than twenty-four 10,000-row ones, which is the shape that matters and which survives.
//!
//! **"~40,000" no longer holds, and nothing replaces it.** Re-measured after the `Arc` change
//! lowered everything `assign_sorted` competes against: 5.95 / 2.66 / 3.14 µs/row at 1k / 10k / 40k
//! (`docs/evidence/memos/2026-08-05-ingest-rate.md` §4). 1,000 is clearly the wrong side of the
//! knee; 10,000 and 40,000 are 18% apart, inside that campaign's run-to-run bar, so the optimum is
//! somewhere between them and this data cannot say where. The shipped `ingest_max_batch_rows` is
//! 10,000 and is not on the wrong side.
//!
//! ## The knobs, and what they do
//!
//! A window closes on **either** its row bound or the work queue observed empty (decision 0034 —
//! no linger). `accept_ingest` blocks on its receipt, so a serial caller never has two jobs queued
//! and closes a window per call: the number of closes equals the number of calls, whatever
//! `commit_window_max_items` says. So `ingest_max_batch_rows` is the driver for a bulk loader and
//! `commit_window_max_items` is a ceiling that must not sit below it. Under concurrent load the
//! window gathers and the roles swap. They default to 10,000 and are equal by construction.
//!
//! ## Still unattributed
//!
//! ~2 µs/row of the remaining ~6 sits outside `close_window` entirely — the submit → queue →
//! receipt round-trip that `accept_ingest` blocks on. `WriteStage` cannot see into it.
//!
//! Ignored, and release-only, for `scale.rs`' reasons. Run with `--features bench-timing` or the
//! stage columns are all zero.
//!
//! ```text
//! cargo test -p tessera-engine --release --features bench-timing --test ingest_shape -- --ignored --nocapture
//! ```

mod common;

use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig};
use tessera_lifecycle::UnallocatedRow;

const BASE: u64 = 1_000_000;
/// Rows per configuration. Enough to swamp the per-run fixed costs at every batch size tried, and
/// divisible by every batch size and every thread count swept below — a moving denominator would
/// make the rows/s columns incomparable, which is the only thing these tests are for.
const ROWS: usize = 240_000;

fn engine(tmp: &std::path::Path, root: &std::path::Path, window: usize) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        EngineConfig {
            flush_max_age_secs: 3600,
            // **The row trigger off.** This cell drives publication itself — it pins `B`
            // by flushing and waiting, so a trigger that published on its own would
            // measure a different buffer depth than the one the sweep set.
            flush_max_items: usize::MAX,
            max_merged_segment_bytes: None,
            // Compaction §9's trigger is off unless a deployment configures one.
            compaction: tessera_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(1024)
        .expect("the executor starts once");
    engine.set_commit_window_max_rows(window);
    engine
}

/// Build `n` rows starting at `from` — term 1 of the three, timed by the caller.
fn build_rows(engine: &Engine, from: usize, n: usize) -> Vec<UnallocatedRow> {
    let descriptors = vec![b"0".to_vec()];
    (from..from + n)
        .map(|i| UnallocatedRow {
            view: "s0".to_string(),
            join: None,
            x: ((i * 7) % 1000) as f64,
            y: ((i * 13) % 1000) as f64,
            scalars: Vec::new(),
            terms: engine.resolve_terms(&descriptors),
            descriptors: descriptors.clone(),
            scoped: Vec::new(),
        })
        .collect()
}

fn fixture(tmp: &std::path::Path) -> std::path::PathBuf {
    let root = tmp.join("bundle");
    build_fixture_n(
        &root,
        &tmp.join("points.parquet"),
        &tmp.join("pairs.parquet"),
        BASE,
    );
    root
}

/// **Term 1 against term 2**: how much of `scale.rs`' "ack" is the harness building rows?
#[test]
#[ignore = "minutes, release only"]
fn row_construction_against_accept_ingest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = fixture(tmp.path());
    let engine = engine(tmp.path(), &root, 10_000);

    let (mut build_total, mut accept_total) = (Duration::ZERO, Duration::ZERO);
    let mut done = 0usize;
    let mut batch = 0usize;
    while done < ROWS {
        let n = 10_000.min(ROWS - done);
        let t = Instant::now();
        let rows = build_rows(&engine, done, n);
        build_total += t.elapsed();

        let t = Instant::now();
        engine
            .ingest_rows(rows, format!("a-{batch}"), [batch as u8; 32])
            .expect("ingest is accepted");
        accept_total += t.elapsed();
        done += n;
        batch += 1;
    }

    let stats = engine.write_executor_stats();
    eprintln!(
        "CONSTRUCT/ACCEPT rows={ROWS} build={build_total:.2?} ({:.2} us/row) \
         accept={accept_total:.2?} ({:.2} us/row) fsyncs={} appends={}",
        build_total.as_secs_f64() * 1e6 / ROWS as f64,
        accept_total.as_secs_f64() * 1e6 / ROWS as f64,
        stats.wal_fsyncs,
        stats.wal_appends,
    );
}

/// Ingest `total` rows in sub-batches of `window`, flushing every `between_flushes` rows.
///
/// Returns the seconds spent inside `accept_ingest` alone — rows are built up front, so the
/// harness's own allocation is excluded and what is left is the write path.
fn ingest_cost(engine: &Engine, total: usize, window: usize, between_flushes: usize) -> f64 {
    let batches: Vec<Vec<UnallocatedRow>> = (0..total / window)
        .map(|b| build_rows(engine, b * window, window))
        .collect();

    let mut spent = Duration::ZERO;
    let mut since_flush = 0usize;
    for (b, rows) in batches.into_iter().enumerate() {
        let t = Instant::now();
        engine
            .ingest_rows(rows, format!("s-{b}"), [b as u8; 32])
            .expect("ingest is accepted");
        spent += t.elapsed();
        since_flush += window;
        if since_flush >= between_flushes {
            let flushes = engine.write_executor_stats().flushes;
            engine.request_flush();
            let deadline = Instant::now() + Duration::from_secs(300);
            while engine.write_executor_stats().flushes <= flushes {
                assert!(Instant::now() < deadline, "flush timed out");
                std::thread::sleep(Duration::from_millis(2));
            }
            since_flush = 0;
        }
    }
    spent.as_secs_f64()
}

/// **Fix the window, raise how much is buffered between flushes.** Per-row cost should rise
/// linearly in `B` — that is the `B/W` law, and a flat column refutes it.
#[test]
#[ignore = "minutes, release only"]
fn cost_against_rows_buffered_between_flushes() {
    for between in [10_000usize, 40_000, 120_000, 240_000] {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = fixture(tmp.path());
        let engine = engine(tmp.path(), &root, 10_000);
        let secs = ingest_cost(&engine, ROWS, 10_000, between);
        eprintln!(
            "BUFFERED B={between:>7} W=10000 (B/W={:>4.1}) {:>9.0} rows/s {:>7.2} us/row",
            between as f64 / 10_000.0,
            ROWS as f64 / secs,
            secs * 1e6 / ROWS as f64,
        );
    }
}

/// **Fix what is buffered, raise the window.** Per-row cost should fall as `1/W`, reaching the
/// fsync floor at `W = B`, where a round is one close over an empty buffer.
#[test]
#[ignore = "minutes, release only"]
fn cost_against_commit_window_size() {
    for window in [10_000usize, 40_000, 120_000, 240_000] {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = fixture(tmp.path());
        let engine = engine(tmp.path(), &root, window);
        let secs = ingest_cost(&engine, ROWS, window, ROWS);
        let stats = engine.write_executor_stats();
        eprintln!(
            "WINDOW  B={ROWS} W={window:>7} (B/W={:>4.1}) {:>9.0} rows/s {:>7.2} us/row fsyncs={}",
            ROWS as f64 / window as f64,
            ROWS as f64 / secs,
            secs * 1e6 / ROWS as f64,
            stats.wal_fsyncs,
        );
    }
}
