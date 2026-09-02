# F4: the row-projection cache builds under its own lock (2026-07-30)

Handoff note. Self-contained — assumes no shared context.

**Same file as F1** (`crates/tessera-engine/src/viewport.rs`), a few lines apart, so whoever
takes both should read both. They are independent fixes: F1 is in the per-tile selection loop, F4
is in the request preamble.

## The defect

`Engine::viewport` gets-or-builds the session's row projection through a mutex, and the *build*
happens while the lock is held:

```rust
let base: Arc<RowProjection> = {
    let mut cache = self.row_projection_cache.lock().unwrap();
    match cache.get(&cache_key) {
        Some(existing) => Arc::clone(existing),
        None => {
            // Crosses entity space into row space over the *whole* fragment
            // (`Permutation::project`'s cost note: seconds at 10⁹ rows)
            let projected = Arc::new(RowProjection::new(
                &session.fragment,
                &slice_data.permutation,
            ));
            cache.insert(cache_key, Arc::clone(&projected));
            projected
        }
    }
};
```

`RowProjection::new` is the I4 entity-space→row-space crossing — the single most expensive
non-amortised operation in the read path. Phase 0 measured `Permutation::project` at **8.8 s for a
69M-item mask** at 10⁹, and the 10⁹ k-sweep recorded a **9.46 s** first-viewport warm-up. The
cache key is `(token_id, slice, segments_version)`, so it is one build per session — but the lock
is **global**, not per-key.

Consequence: every distinct session's first viewport serialises its projection build behind one
mutex. N new sessions cost N sequential projection builds, no matter how many cores are free.

## Evidence

`scripts/bench_concurrency.py`, 2.42M `categories-subclass`, w=10, k=30, zoom 8, 8 s per cell.
Two arms differing only in the token set: **Arm B** reuses ≤4 grant sets across N workers (shared
fragments, shared projections); **Arm A** gives every worker its own grant set (N distinct
fragments, N distinct projections).

| arm | conc | rps | p50 | p99 | server p99 | server CPU |
|---|---:|---:|---:|---:|---:|---:|
| B shared | 100 | 48,588 | 1.82 ms | 5.41 ms | 1.26 ms | 818% |
| B shared | 1000 | 49,475 | 17.63 ms | 60.20 ms | 1.61 ms | 865% |
| A distinct | 100 | 39,782 | 2.14 ms | 6.53 ms | 1.45 ms | 712% |
| A distinct | 1000 | **18,599** | 20.53 ms | **1042.31 ms** | **10.49 ms** | **426%** |

**The tell is the CPU column.** Going from 100 to 1000 distinct principals, throughput halves,
end-to-end p99 reaches a full second, server-side p99 goes up 7× — and server CPU *drops* from
712% to 426%. A CPU-bound system does not get slower while using less CPU. Those threads are
blocked, and the only global lock on that path is this one.

Arm B is the control: same request, same geometry, same k, 1000 concurrent — but the projections
are shared, so after the first few requests nobody takes the slow branch. Its server-side p99
stays at 1.61 ms while Arm A's reaches 10.49 ms.

## Why this is more urgent than it looks

**It bites on session churn, not session count.** The arm above reached it by ramping to 1000
concurrent principals, which reads like a scale problem. It is not. The slow branch is taken once
per `(token, slice, segments_version)`, so it fires on:

- **token rotation** — `token_max_lifetime` defaults to 1 hour, so every live session takes this
  branch again every hour, and they will tend to cluster;
- **any bundle swap** — `segments_version` is part of the key, so a rebuild invalidates *every*
  session's projection at once and the next request from each takes the slow branch;
- **cold start with a warm client population** — every reconnecting session, at once.

A deployment with 100 steady users and hourly rotation meets this on every rotation. The 1000-user
ramp is how the benchmark *found* it, not the only shape in which it occurs.

## The fix

The lock protects a `HashMap`; it should not also protect the expensive computation. Two shapes,
in increasing order of effort:

1. **Build outside the lock, insert under it.** Lock → miss → *drop the lock* → build → re-acquire
   → `entry().or_insert_with(...)`. Concurrent builders for the same key duplicate work (two
   sessions racing on the same token build twice, one result is dropped), but no builder blocks an
   *unrelated* key. Given the key includes `token_id`, same-key races are rare — this is likely
   sufficient and is a small change.

2. **Per-key in-progress markers.** Store `Arc<OnceLock<Arc<RowProjection>>>` (or a `Shared`
   future) in the map so concurrent builders for the same key wait on *that* key rather than on
   the map. Removes the duplicate work in case 1 at the cost of a more involved value type.

Do not simply narrow the critical section around `cache.get` while leaving the build inside a
second acquisition of the same lock — that is the same serialisation with extra steps.

## While you are in there

`row_projection_cache` is an **unbounded `FxHashMap` with no eviction of any kind**. Entries are
keyed by `token_id`, so every session that ever drew a viewport retains its projection for the
process lifetime, even after the token expires. Measured marginal cost is **~248 KiB per session**
at 2.42M (243 MiB of RSS delta across 1000 sessions); the mask scales with the corpus, so that is
~2.5 MB/session at 25M and ~100 MB/session at 10⁹ — where 1000 live sessions is ~100 GB, which is
design §13.1's own "a thousand live auth inputs is 125 GB" arriving from the other direction.

This is a separate defect from the lock and does not have to be fixed in the same change, but it
is the reason the lock fix alone will not make session churn free: rotating tokens hourly with no
eviction grows the map without bound.

## How to measure the fix

```
cargo build --release -p tessera-bench           # bench-timing is on by default here
reference/.venv/bin/python scripts/bench_concurrency.py \
    --bundle /tmp/tessera-bench/fixtures/2422486/categories-subclass \
    --scale 2422486 --concurrency 5,10,100,1000 --arms B,A --duration 8 --w 10
```

Fixtures are prebuilt under `/tmp/tessera-bench/fixtures/<scale>/<label-set>/` for 250,000 /
2,422,486 / 25,000,000; rebuild with `scripts/bench_build_fixtures.sh` (idempotent).

**Expected after the fix:** Arm A at c=1000 stops collapsing — throughput should track Arm B's
shape rather than halving, server CPU should *rise* toward Arm B's ~865% instead of falling to
426%, and server-side p99 should come back toward Arm B's ~1.6 ms. The `row_projection_built` flag
in `StageTimings` marks exactly which requests took the slow branch, and
`Engine::row_projection_cache_len()` exposes the map size for the eviction question.

**Two caveats on the numbers above, both recorded in `crates/tessera-bench/src/arms/load.rs`.**
Every Arm B cell is flagged `generator_bound`: at 2.42M a viewport is cheap enough (server p99
1.6 ms) that throughput lands within 3× of the load generator's own `/healthz` ceiling, so Arm B's
absolute rps is a floor rather than a measurement. Arm A at c=1000 is *not* so flagged — it is
slow enough to be unambiguous, which is precisely why the finding survives the caveat. And the
generator itself saturates at c=1000 (its `/healthz` ceiling *falls* from 123.6k to 110.5k rps
with a 22 ms p99), so both c=1000 rows understate what the server could do.

## Related

- docs/evidence/memos/2026-07-30-f1-selection-overdraw.md — same file, per-tile selection path.
- **F2**, in `crates/tessera-engine/src/compose.rs`: `compose` iterates the entire overlay and
  buffer on every viewport. Measured at ~10 ns per buffer entry (rejected) and ~18.5 ns per
  overlay entry (resolved), zoom-independent because `compose` runs once per request before the
  tile loop. Details in `crates/tessera-bench/src/arms/{ingest,changes}.rs`.
