//! Allocator retention on the serve path: the arena cap set at startup, and the trim that gives
//! freed pages back to the kernel.
//!
//! The caches are bounded, but what the allocator keeps after they release memory is not, and on
//! a node near its memory cap every retained byte evicts bundle pages the next request reads.
//! [`HeapWatch`] trims when `RssAnon` has grown by [`TRIM_GROWTH_BYTES`] since the last trim, so a
//! node whose retention has stopped growing stops trimming, and the cost follows allocation
//! rather than request rate. Free memory that builds up without `RssAnon` growing is never
//! returned.
//!
//! glibc creates up to `8 × cores` arenas and destroys none; [`arena_max`] caps them at the
//! compute pool's width.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use tessera_types::process;

use crate::state::AppState;

/// Anonymous growth since the last trim that triggers the next. A trim's cost tracks what it
/// returns rather than how often it runs, so this sets how soon memory comes back. It does not
/// bound retention: a trim returns only what the allocator has free.
pub const TRIM_GROWTH_BYTES: u64 = 256 * 1024 * 1024;

/// The shortest interval between two reads of `/proc/self/status`, which keeps the read off the
/// per-request path.
const CHECK_INTERVAL_MS: u64 = 250;

/// What the allocator has done since start: `/control/status`' `heap` block.
#[derive(Debug, Clone, Copy)]
pub struct HeapStats {
    /// `RssAnon`: the caches, the allocator's retention and every other anonymous page.
    pub anon_bytes: u64,
    /// `RssFile`: the resident bundle mappings, the page cache that retention competes with.
    pub file_bytes: u64,
    /// `VmRSS` now.
    pub resident_bytes: u64,
    /// Trims completed since start.
    pub trims: u64,
    /// `RssAnon` before the last trim minus after it; zero if it returned nothing or another thread
    /// allocated meanwhile.
    pub last_trim_returned_bytes: u64,
    /// What the last trim took, in microseconds; it rises with the arena count.
    pub last_trim_micros: u64,
    /// `RssAnon` when the last trim finished, which the growth test compares against. Zero before
    /// the first trim.
    pub trim_baseline_bytes: u64,
}

/// The trim cadence's state, one per process.
pub struct HeapWatch {
    /// Process start, so the time gate is an integer of milliseconds rather than a lock.
    started: Instant,
    /// Milliseconds since `started` at the last reading, claimed by compare-exchange so concurrent
    /// requests take one reading between them.
    last_check_ms: AtomicU64,
    /// `RssAnon` read after the last trim, so what a trim failed to return is not counted as growth
    /// again.
    baseline_anon: AtomicU64,
    /// Whether a trim is running; a request that sees growth meanwhile does nothing.
    in_flight: AtomicBool,
    trims: AtomicU64,
    last_returned: AtomicU64,
    last_micros: AtomicU64,
}

impl Default for HeapWatch {
    fn default() -> Self {
        HeapWatch {
            started: Instant::now(),
            last_check_ms: AtomicU64::new(0),
            // The first trim comes after serving growth, not after the bundle open.
            baseline_anon: AtomicU64::new(process::resident_bytes().anon),
            in_flight: AtomicBool::new(false),
            trims: AtomicU64::new(0),
            last_returned: AtomicU64::new(0),
            last_micros: AtomicU64::new(0),
        }
    }
}

impl HeapWatch {
    /// Whether a trim is due, reading `/proc/self/status` at most once per [`CHECK_INTERVAL_MS`].
    /// A `true` claims the trim and the caller must run [`Self::trim`]; only one caller is told so.
    pub fn trim_due(&self) -> bool {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let last = self.last_check_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < CHECK_INTERVAL_MS {
            return false;
        }
        if self
            .last_check_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        let anon = process::resident_bytes().anon;
        if anon.saturating_sub(self.baseline_anon.load(Ordering::Relaxed)) < TRIM_GROWTH_BYTES {
            return false;
        }
        !self.in_flight.swap(true, Ordering::AcqRel)
    }

    /// Trims, records the cost and what came back, re-baselines and releases the claim. Run it on a
    /// blocking thread: the walk takes every arena's lock in turn.
    pub fn trim(&self) {
        let before = process::resident_bytes().anon;
        let started = Instant::now();
        process::trim_heap();
        let micros = started.elapsed().as_micros() as u64;
        let after = process::resident_bytes().anon;
        self.baseline_anon.store(after, Ordering::Relaxed);
        self.last_returned
            .store(before.saturating_sub(after), Ordering::Relaxed);
        self.last_micros.store(micros, Ordering::Relaxed);
        self.trims.fetch_add(1, Ordering::Relaxed);
        self.in_flight.store(false, Ordering::Release);
        tracing::debug!(
            returned_bytes = before.saturating_sub(after),
            micros,
            anon_bytes = after,
            "the allocator returned its free pages to the kernel"
        );
    }

    /// The `/control/status` block, with the resident figures read now.
    pub fn stats(&self) -> HeapStats {
        let resident = process::resident_bytes();
        HeapStats {
            anon_bytes: resident.anon,
            file_bytes: resident.file,
            resident_bytes: resident.total,
            trims: self.trims.load(Ordering::Relaxed),
            last_trim_returned_bytes: self.last_returned.load(Ordering::Relaxed),
            last_trim_micros: self.last_micros.load(Ordering::Relaxed),
            trim_baseline_bytes: self.baseline_anon.load(Ordering::Relaxed),
        }
    }
}

/// After each response, dispatches a due trim onto a blocking thread. Every plane mounts it,
/// since each one grows the heap. A streamed body may still be emitting when this runs.
pub async fn trim_after_response(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    if state.heap.trim_due() {
        tokio::task::spawn_blocking(move || state.heap.trim());
    }
    response
}

/// How many arenas glibc may create: the compute pool's width, the threads that allocate in
/// parallel within one request, with a floor of 4. Lock contention on shared arenas under many
/// concurrent requests has not been measured.
pub fn arena_max(compute_threads: usize) -> usize {
    compute_threads.max(4)
}

/// Applies [`arena_max`] to this process. Call it before any thread allocates: it bounds arena
/// creation, not arenas already made.
pub fn cap_arenas(compute_threads: usize) {
    let arenas = arena_max(compute_threads);
    if process::set_arena_max(arenas) {
        tracing::info!(arenas, "the allocator's arena count is capped");
    } else {
        // A non-glibc allocator has no arenas to cap; the node serves either way.
        tracing::debug!(
            arenas,
            "this allocator does not take an arena cap; retention is the allocator's own"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first call after construction is inside the interval and takes no reading.
    #[test]
    fn the_time_gate_admits_one_check_per_interval() {
        let watch = HeapWatch::default();
        // Refused by the clock, not by the growth test.
        assert!(!watch.trim_due());
        assert_eq!(watch.stats().trims, 0);
    }

    /// A trim is counted and re-baselines. Whether memory came back depends on the allocator, so it
    /// is not asserted.
    #[test]
    fn a_trim_rebaselines_and_is_counted() {
        let watch = HeapWatch::default();
        watch.trim();
        let stats = watch.stats();
        assert_eq!(stats.trims, 1);
        assert!(
            stats.trim_baseline_bytes > 0,
            "the baseline is re-read after the trim"
        );
        // Within the noise of two `/proc` reads around one call.
        assert!(stats.anon_bytes.abs_diff(stats.trim_baseline_bytes) < 64 * 1024 * 1024);
    }

    /// The claim is exclusive: whatever the growth, two threads cannot both be sent to trim.
    #[test]
    fn one_claim_at_a_time() {
        let watch = HeapWatch::default();
        assert!(!watch.in_flight.swap(true, Ordering::AcqRel));
        // With the claim held, no second claim is handed out even when the clock and the baseline
        // say a trim is due.
        watch.last_check_ms.store(0, Ordering::Relaxed);
        watch.baseline_anon.store(0, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(CHECK_INTERVAL_MS + 10));
        assert!(!watch.trim_due());
        watch.trim();
        assert_eq!(watch.stats().trims, 1);
    }

    /// The cap is the pool's width, and the floor holds under it.
    #[test]
    fn the_arena_cap_follows_the_pool_and_has_a_floor() {
        assert_eq!(arena_max(12), 12);
        assert_eq!(arena_max(25), 25);
        assert_eq!(arena_max(1), 4);
        assert_eq!(arena_max(0), 4);
    }
}
