//! Waiting on the write executor: the poll loop the lifecycle cases share, and the three
//! executor events they wait for.
//!
//! Each case names its own deadline (a `WAIT` const, or the argument to [`wait_until`]) because a
//! heavy corpus needs longer than a two-item one; the loop itself is the same everywhere.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use tessera_engine::Engine;

/// Poll `cond` every 5 ms until it holds, failing after `within` with what was being waited for.
pub fn wait_until(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Drive ticks until `cond` holds. A pass is dispatched on a tick only while none of its kind is
/// outstanding, so waiting on one tick can miss the pass a test is waiting for.
pub fn tick_until(engine: &Engine, what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
    let mut last = Instant::now() - Duration::from_secs(1);
    wait_until(what, within, || {
        if last.elapsed() >= Duration::from_millis(50) {
            engine.request_flush();
            last = Instant::now();
        }
        cond()
    });
}

/// Force a flush and wait for it to publish.
pub fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    wait_until("the flush never published", Duration::from_secs(60), || {
        engine.write_executor_stats().flushes > before
    });
}

/// Force a fold and wait for it to publish, failing the moment one is discarded rather than
/// published — a discarded fold leaves the counters where a timeout would.
pub fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    wait_until("the fold never published", Duration::from_secs(120), || {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        now.folds > before.folds
    });
}

/// **Force a tick and wait for it** — the moment a level's row forms are published from the
/// deltas accumulated since the last one (`ingest.md` §1.3, §10 ruling 6).
///
/// A write is durable at its acknowledgement and visible at the next publication, so a test that
/// writes and then reads what a viewer sees puts this between the two. The tick is requested
/// rather than waited for so that a test does not sit out `flush_max_age_secs`.
pub fn tick(engine: &Engine) {
    let before = engine.write_executor_stats().ticks;
    engine.request_flush();
    wait_until(
        "the tick that publishes the row forms never ran",
        Duration::from_secs(30),
        || engine.write_executor_stats().ticks != before,
    );
}
