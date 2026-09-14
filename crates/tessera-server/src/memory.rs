//! Allocator retention on the serve path: the arena cap taken at startup, and the trim cadence
//! that gives freed pages back to the kernel.
//!
//! # What this is for
//!
//! A serving node's caches are bounded and accounted. The memory the allocator keeps after those
//! caches have released is neither. A rung 6 node (196 GiB bundle, `MemoryMax=24G`) reached
//! 14.96 GiB of `RssAnon` after a six-principal battery, of which 14.15 GiB was inside glibc —
//! 373 non-main arena heaps holding 8.87 GiB resident of 23.31 GiB of address space, plus a
//! 5.28 GiB main arena — while the server's own cache accounting totalled 127 MB. The growth was a
//! ratchet that saturated: zero over the last 21 minutes of the heaviest load, a seventh principal
//! cost 9.5 MB and nine further requests cost nothing.
//!
//! Retention on a node with memory to spare is free. On that node it was not: the cgroup sat 86 KB
//! under its cap with 1.2 × 10⁹ file refaults and 8 TB read in 104 minutes, so every byte the
//! allocator held was a byte of the bundle's page cache evicted, and the bundle is what the next
//! request reads.
//!
//! # The cadence, and the two mechanisms not chosen
//!
//! [`HeapWatch`] trims when `RssAnon` has grown by [`TRIM_GROWTH_BYTES`] since the last trim. Two
//! properties follow from keying on growth. It is self-extinguishing: a node whose ratchet has
//! saturated stops trimming, because the baseline is re-read after each trim and nothing grows past
//! it. And its cost is amortised against allocation rather than against traffic — one trim per
//! `TRIM_GROWTH_BYTES` of net anonymous growth, whatever the request rate.
//!
//! *A trim after any request whose CPU time passed a threshold* was the alternative, and it fails
//! the saturated case: a heavy principal panning a saturated node pays the walk on every request
//! and gets nothing back, for ever. *A trim on the refresh tick* ties the cadence to the write path
//! — a read-only deployment publishes nothing, so the tick that would carry it never fires.
//!
//! The growth test costs one `/proc/self/status` read, so a time gate sits in front of it: at most
//! one reading per [`CHECK_INTERVAL_MS`], claimed by one thread. The trim itself runs on a blocking
//! thread, never on the reactor and never on a compute worker, because `malloc_trim` walks every
//! arena's free lists and takes each arena's lock as it goes.
//!
//! # The arena cap
//!
//! glibc creates an arena per contending thread up to `8 × cores` — 96 on a 12-core box — and
//! destroys none. [`arena_max`] caps that at the width of the compute pool, which is the widest set
//! of threads that allocate in parallel inside one request. The trade is stated where the cap is
//! set: fewer arenas retain less and cost more lock contention.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use tessera_types::process;

use crate::state::AppState;

/// Anonymous growth since the last trim that asks for the next one.
///
/// The figure sets the amortised cost: one `malloc_trim` per this much net anonymous growth.
///
/// **What a trim costs, measured** on a warmed 64p node (25,846,007 rows, six principals, three
/// batteries in one process, 2026-09-14): the two trims this threshold produced took 7.4 ms
/// returning 67.7 MB and 11.1 ms returning 100.1 MB. The cost tracks what is returned rather than
/// how often the walk runs — the same battery at a 16 MiB threshold took 102 trims of about 2.1 ms
/// each, returning 7.6 to 18.4 MB apiece. So a smaller threshold does not cost more in total; it
/// returns memory sooner and holds `RssAnon` lower (742 MiB against 761 MiB after two batteries).
/// 256 MiB is the conservative end of that: it keeps the cadence rare on a node whose growth is
/// ordinary and still answers the rung 6 ratchet, where 14.15 GiB of retention is about 56 trims.
///
/// It is not a bound on retention: what a trim returns is what the allocator has free, and a node
/// holding 10 GB of live bitmaps keeps holding them. At the 64p scale the trims returned 7 to 11%
/// of `RssAnon`, because most of that node's anonymous memory is the masked-count cache doing its
/// job — which is exactly the reading `/control/status`' cache blocks beside `heap` are for.
pub const TRIM_GROWTH_BYTES: u64 = 256 * 1024 * 1024;

/// The shortest interval between two readings of `/proc/self/status`.
///
/// The read is 20–40 µs, which is immaterial per request and is not immaterial at the rate a
/// health probe and a tile fan-out generate together. At 250 ms the sampling costs at most
/// 160 µs/s whatever the traffic, and a ratchet that climbs [`TRIM_GROWTH_BYTES`] in under that is
/// a node trimming four times a second at most.
const CHECK_INTERVAL_MS: u64 = 250;

/// What the process's allocator has done since it started — `/control/status`' `heap` block.
#[derive(Debug, Clone, Copy)]
pub struct HeapStats {
    /// `RssAnon` now: the caches, the allocator's retention and every other anonymous page.
    pub anon_bytes: u64,
    /// `RssFile` now: the resident part of the bundle's mappings, which is the page cache the
    /// allocator's retention competes with.
    pub file_bytes: u64,
    /// `VmRSS` now.
    pub resident_bytes: u64,
    /// Trims completed since start.
    pub trims: u64,
    /// What the last trim gave back: `RssAnon` before minus after, in bytes, or zero where it gave
    /// nothing back or another thread allocated across it. A trim that returns nothing while
    /// `anon_bytes` stays high is the honest reading that the memory is live, not retained.
    pub last_trim_returned_bytes: u64,
    /// What the last trim cost, in microseconds. It rises with the arena count, which is what
    /// [`arena_max`] bounds.
    pub last_trim_micros: u64,
    /// `RssAnon` at the last trim's completion — the baseline the next trim's growth test is
    /// against. Zero before the first trim.
    pub trim_baseline_bytes: u64,
}

/// The growth cadence's state: one per process, held by `AppState`.
pub struct HeapWatch {
    /// Process start, so the time gate can be an integer of milliseconds rather than a lock around
    /// an `Instant`.
    started: Instant,
    /// Milliseconds since `started` at the last `/proc/self/status` reading. Claimed by
    /// compare-exchange, so concurrent requests take one reading between them rather than one
    /// each.
    last_check_ms: AtomicU64,
    /// `RssAnon` at the last completed trim. The growth test is against this, and it is re-read
    /// after the trim rather than before it, so what a trim failed to return is not counted as
    /// growth again.
    baseline_anon: AtomicU64,
    /// Whether a trim is on a blocking thread now. A second request that sees growth while one
    /// runs does nothing: the running trim is already returning what both of them saw.
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
            // The process's own anonymous set at startup, so the first trim comes after
            // `TRIM_GROWTH_BYTES` of serving growth rather than after the bundle open.
            baseline_anon: AtomicU64::new(process::resident_bytes().anon),
            in_flight: AtomicBool::new(false),
            trims: AtomicU64::new(0),
            last_returned: AtomicU64::new(0),
            last_micros: AtomicU64::new(0),
        }
    }
}

impl HeapWatch {
    /// Whether a trim is due now, under both gates. Cheap on the path it is called from: an
    /// integer compare, and a `/proc/self/status` read at most once per [`CHECK_INTERVAL_MS`].
    ///
    /// Claiming is part of the answer — a `true` marks the trim in flight, and the caller must run
    /// [`Self::trim`]. Two callers cannot both be told `true` for one growth step.
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

    /// Trim, record what it cost and returned, and re-baseline. Runs on a blocking thread: the
    /// walk takes every arena's lock in turn.
    ///
    /// Releases the claim [`Self::trim_due`] took, including on the path where the allocator
    /// returned nothing.
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

    /// The block `/control/status` publishes. Reads `/proc/self/status` on the call, so an
    /// operator polling it sees the resident figures now rather than at the last cadence check.
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

/// The cadence's hook: after a response, ask [`HeapWatch::trim_due`] and dispatch the trim it
/// claims onto a blocking thread.
///
/// **Every plane mounts it**, so the cadence is a property of the router rather than of
/// `tessera serve`: a viewport builds the projections, `/session/authorise` builds the mask
/// fragment, and `/control/ingest` allocates a commit window. A plane left out would be a plane
/// whose growth nothing answers.
///
/// **After the response, and on a blocking thread.** The decision is two integer loads and, at
/// most four times a second, one `/proc` read; the walk that follows it is milliseconds and takes
/// every arena's lock, which is not something to do on the reactor. A streamed viewport's body
/// outlives this layer, so the reading it triggers is taken while that body is still emitting —
/// the cadence is a growth measure, not a completion barrier.
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

/// How many arenas glibc may create, given the compute pool's width.
///
/// **The pool's width, because that is what allocates in parallel.** One request's tile sweep fans
/// out across the whole rayon pool, and those threads allocate and free Roaring containers
/// throughout it; the reactor's workers serialise responses beside them. An arena per pool thread
/// is enough for the fan-out to proceed without two workers sharing a lock most of the time, and it
/// is eight times fewer than the `8 × cores` glibc would otherwise reach.
///
/// **What the cap costs, and what it buys, measured.** With more simultaneously allocating threads
/// than arenas, threads share an arena and serialise on its lock. One battery each on a 64p node
/// (25,846,007 rows, six principals, 12 cores, 2026-09-14), capped at 12 against the same binary
/// allowed 4,096:
///
/// | | uncapped | capped at 12 |
/// |---|---|---|
/// | arena-shaped anonymous regions | 44 | 15 |
/// | anonymous address space | 3,013 MiB | 1,157 MiB |
/// | hot p50, zoom 6, 100% principal | 2.73 / 3.04 ms | 2.58 / 2.76 ms |
/// | hot p50, zoom 12, 100% principal | 12.80 / 29.58 ms | 13.78 / 29.49 ms |
///
/// Two cells per zoom, both reported. The latency differences are inside the scatter between
/// repeats of one binary — three batteries in one capped process gave 14.53, 14.69 and 15.46 ms at
/// the first zoom 12 cell — so at this concurrency the cap costs nothing measurable and takes 1.9
/// GiB off the address space. It is not a claim about a box with far more cores than this one.
///
/// The floor of 4 is for a single-core box, where `8 × cores` would still be 8: a cap that made an
/// unusual deployment allocate through one arena would be a contention change nobody measured.
pub fn arena_max(compute_threads: usize) -> usize {
    compute_threads.max(4)
}

/// Apply [`arena_max`] to this process, logging whether the allocator took it.
///
/// Called from [`crate::prepare`] before the engine opens, which is before the compute pool, the
/// reactor and the write executor exist. It bounds arena *creation*, so a call after the threads
/// have contended would leave whatever they already made.
pub fn cap_arenas(compute_threads: usize) {
    let arenas = arena_max(compute_threads);
    if process::set_arena_max(arenas) {
        tracing::info!(arenas, "the allocator's arena count is capped");
    } else {
        // Not a refusal: a non-glibc allocator has no arenas to cap and its retention is its own
        // business. The node serves either way.
        tracing::debug!(
            arenas,
            "this allocator does not take an arena cap; retention is the allocator's own"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The time gate is what keeps the `/proc` read off the per-request path, so the first call
    /// after construction must not take one: a burst of requests in the first
    /// [`CHECK_INTERVAL_MS`] reads the file no more than once.
    #[test]
    fn the_time_gate_admits_one_check_per_interval() {
        let watch = HeapWatch::default();
        // `last_check_ms` starts at zero and `started` is now, so the first call is inside the
        // interval and is refused by the clock rather than by the growth test.
        assert!(!watch.trim_due());
        assert_eq!(watch.stats().trims, 0);
    }

    /// A trim records its cost and re-baselines, so the growth test after it is against what the
    /// process holds now. Nothing here asserts that memory was returned: a test process has
    /// whatever the allocator happens to have free.
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
        // Within the sampling noise of two `/proc` reads around one call: the baseline is the
        // anonymous set as the trim left it.
        assert!(stats.anon_bytes.abs_diff(stats.trim_baseline_bytes) < 64 * 1024 * 1024);
    }

    /// The claim is exclusive: whatever the growth, two threads cannot both be sent to trim.
    #[test]
    fn one_claim_at_a_time() {
        let watch = HeapWatch::default();
        assert!(!watch.in_flight.swap(true, Ordering::AcqRel));
        // With the claim held, the gates cannot hand out a second one even with the clock and the
        // baseline forced to say a trim is due.
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
