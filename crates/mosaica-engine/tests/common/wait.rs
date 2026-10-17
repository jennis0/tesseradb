//! Waiting on the write executor: the poll loop the lifecycle cases share, and the executor
//! events they wait for.
//!
//! Each case names its own deadline (a `WAIT` const, or the argument to [`wait_until`]) because a
//! heavy corpus needs longer than a two-item one; the loop itself is the same everywhere.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use mosaica_engine::Engine;

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
///
/// Returns once the cycle its last request was answered with has published, so no request of its
/// own is left to publish rows the caller buffers next.
pub fn tick_until(engine: &Engine, what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
    let mut last = Instant::now() - Duration::from_secs(1);
    let mut requested = 0;
    wait_until(what, within, || {
        if last.elapsed() >= Duration::from_millis(50) {
            requested = engine.request_flush_publication();
            last = Instant::now();
        }
        cond()
    });
    wait_until("the last requested publication", within, || {
        engine.publication() >= requested
    });
}

/// Request a flush once and wait for the publication cycle that honours it: every row buffered
/// before the request is then served, and no request is left to publish rows buffered after this
/// returns.
pub fn publish_buffered(engine: &Engine) {
    let publication = engine.request_flush_publication();
    wait_until(
        "the publication honouring the flush",
        Duration::from_secs(60),
        || engine.publication() >= publication,
    );
}

/// [`compact`] of every shard with something to reclaim.
pub fn fold(engine: &Engine) {
    compact(engine, None);
}

/// Request a compaction of `shard`, or of every shard with something to reclaim, and wait until
/// the request is answered, failing the moment a compaction is discarded rather than published.
/// A request made while a compaction runs is held until that one lands, and is answered by the
/// compactions after it. Returns how many compactions published meanwhile, one already running
/// included.
pub fn compact(engine: &Engine, shard: Option<mosaica_types::ShardId>) -> u64 {
    let before = engine.write_executor_stats();
    engine.request_compaction(shard);
    let mut published = 0;
    wait_until(
        "the compaction's publication",
        Duration::from_secs(120),
        || {
            let now = engine.write_executor_stats();
            assert_eq!(
                now.fold_failures, before.fold_failures,
                "the compaction was discarded rather than published"
            );
            published = now.folds - before.folds;
            published > 0 && !now.fold_requested
        },
    );
    published
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

/// Two ticks: the first seals the log member holding the records, the second reclaims it.
pub fn reclaim_log(engine: &Engine) {
    tick(engine);
    tick(engine);
}
