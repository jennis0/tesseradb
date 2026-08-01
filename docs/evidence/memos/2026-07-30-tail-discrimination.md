# Tail discrimination: cold page faults, or not? (2026-07-30)

## Method

Task 2 of the external-ID identity plan. The 10⁹ viewport benchmark (`docs/archive/plans/bench-baselines/2026-07-29-1e9-k-sweep.json`) shows a near-constant ~42–47 ms gap between server-side p99 and p50 across k=30..1000. That gap has been *hypothesised* — never measured — to be cold major page faults against a 51.1 GB bundle on a 47 GiB box. This memo reports the direct measurement, on the **current** bundle (`/tmp/tessera-1e9`), before any rebuild or format change.

Four arms, same bundle, same principal (w = 10,000-descriptor grant, seed 0), same 2,000 seeded viewports per k (k = 50, 500, 1000; mixed zooms 4–12, ~300-tile spans — identical geometry, generated once and reused verbatim across every arm):

| arm | binary | pre-fault | what it isolates |
|---|---|---|---|
| A | normal | no | baseline, whatever residency the OS happens to have |
| B | normal | yes | A with every hot mapping (`columns.arrow`, `morton.u32`, `permutation.bin`) read end-to-end before measuring |
| C | `--features tessera-engine/skip-id-index` | no | the external-ID index (18.9 GB of extents) never mapped by the engine's own loader — simulates the *post-rebuild* residency the identity/sidecar change is meant to achieve |
| D | `skip-id-index` | yes | C + the pre-fault pass — the most-resident condition achievable here |

Implementation: `crates/tessera-store/src/sidecar.rs` (named `external_ids.rs` when this was written) gained an `IndexState::Disabled` variant (`ExternalIdIndex::disabled()`) whose `resolve` returns `Err(StoreError::IdIndexDisabled)` — **never** `Ok(None)`, which would read as "unknown external id" and let a WAL-resident suppression silently fail to apply. `crates/tessera-engine/src/session.rs` gates the load behind the `skip-id-index` feature (declared on `tessera-engine`, re-exported through `tessera-server` and `tessera-cli` so `cargo build --release --features tessera-engine/skip-id-index` reaches the binary), and `Engine::open` refuses to start under the feature if the WAL contains any `Change` (deny) record — the measurement workload is viewport-only and never issues one, so this is a misuse guard, not a live constraint. `scripts/discriminate_tail.py` drives all four arms, reusing `scripts/bench_k_sweep.py`'s `gen_viewports`/`percentile`/`read_dictionary_descriptors` by import and `reference/oracle/harness.py` for process/port handling.

Direct evidence is `majflt` (field 12 of `/proc/<pid>/stat`), read before and after each arm's full serving phase (boot → warm-up → all three k sweeps). Also recorded per arm: `minflt`, peak RSS (`VmHWM`), and the `SwapTotal`/`SwapFree` delta from `/proc/meminfo`.

**Cache-drop limitation, stated plainly.** `sync; echo 3 | sudo tee /proc/sys/vm/drop_caches` requires an interactive password under this WSL2 environment and could not be run non-interactively (confirmed: `sudo -n` refuses). `cache_dropped: false` is recorded in `probes/tail-discrimination.json`. Arms A and C are therefore only "warm-ish" — whatever the OS page cache already held from prior activity on this box (including each arm's own `verify_files` boot pass, which reads every manifest-listed file's bytes via buffered `File::read`, unconditionally, regardless of `skip-id-index` — see the script's docstring) stayed resident going into the measurement. **This does not weaken the refutation below**, because the refutation rests on the *pre-faulted* arms (B, D), where every hot mapping is deliberately, exhaustively read end-to-end immediately before measuring — the missing `drop_caches` cannot hide a real residency effect from arms whose whole design is "touch everything first."

## Results

Boot times here (48.6–110.2 s) are well under the prior cold-boot baseline (176.8 s) precisely because of the cache-drop limitation above — this box's page cache already held a meaningful fraction of the bundle from earlier work in this session.

| arm | k | p50 (ms) | p99 (ms) | p99−p50 (ms) | max (ms) |
|---|---|---|---|---|---|
| A | 50 | 5.72 | 45.03 | 39.32 | 228.03 |
| A | 500 | 23.07 | 53.17 | 30.09 | 816.03 |
| A | 1000 | 42.62 | 85.21 | **42.59** | 7130.70 |
| B | 50 | 4.15 | 40.16 | 36.02 | 279.05 |
| B | 500 | 22.78 | 52.72 | 29.94 | 770.93 |
| B | 1000 | 43.78 | 88.23 | **44.44** | 1752.00 |
| C | 50 | 4.52 | 45.72 | 41.20 | 250.19 |
| C | 500 | 24.31 | 58.48 | 34.17 | 1000.51 |
| C | 1000 | 41.76 | 79.72 | **37.96** | 1848.17 |
| D | 50 | 4.31 | 38.67 | 34.36 | 356.59 |
| D | 500 | 23.42 | 57.86 | 34.45 | 911.06 |
| D | 1000 | 42.70 | 86.56 | **43.86** | 1843.39 |

| arm | majflt_delta | minflt_delta | peak RSS | boot (s) | prefault (s) | swap used delta |
|---|---|---|---|---|---|---|
| A | **2,048** | 2,140,988 | 32,906,916 kB (31.38 GiB) | 110.2 | — | 161,712 kB (158.0 MB) |
| B | **259** | 2,118,483 | 32,412,084 kB (30.91 GiB) | 61.6 | 12.71 | 80,524 kB (78.6 MB) |
| C | **690** | 2,189,493 | 20,926,720 kB (19.96 GiB) | 61.6 | — | 217,864 kB (212.8 MB) |
| D | **0** | 2,006,398 | 21,159,348 kB (20.19 GiB) | 48.6 | 13.26 | 18,696 kB (18.3 MB) |

Full per-k data, both server-side and end-to-end timings: `probes/tail-discrimination.json`.

## Verdict: REFUTED

**Arm D settles it on its own.** Zero major page faults (`majflt_delta = 0`), and the p99−p50 gap at k=1000 is still 43.86 ms — statistically indistinguishable from arm A's 42.59 ms, the arm the hypothesis says should show the *largest* gap. A tail this size cannot be attributed to page faults when there were none to attribute it to.

**Arm A reinforces it quantitatively, not just directionally.** 2,048 major faults across 6,000 requests (2,000 per k × 3 k values) is ~0.34 faults per request. Even at a generous 0.5 ms per major fault (large for an SSD-backed page-in, let alone whatever WSL2's virtualised block layer costs), that is under 0.2 ms of expected per-request cost — two orders of magnitude short of the observed ~42 ms gap. The fault count does fall monotonically-ish across the arms in the direction the hypothesis predicts (A 2048 → B 259 → C 690 → D 0; C's non-monotonic uptick relative to B is noise at this sample size, not a reversal worth chasing), but the *magnitude* was never in the right range to explain the effect, at any arm.

**Do not read this as "partially confirmed."** The brief is explicit that a surviving tail at arm D is a refutation, full stop, and that is what happened. The residency story is not what is costing the tail.

## The second finding, which matters more for the owner's decision than the first

Arms C and D ran the `skip-id-index` binary at ~20 GB peak RSS against A/B's ~31 GB — that is, **C and D simulate the post-rebuild residency the entire external-ID identity change is meant to achieve** (the change's whole performance argument is "de-resident the 18.9 GB of external-ID extents"). The tail persists at 37.96 ms (C) and 43.86 ms (D) under that exact simulated condition.

**So the identity/sidecar change would not have fixed p99 even if the page-fault hypothesis had been right.** State this plainly: the change keeps its architectural justification in full — external IDs off the hot path, `node_id` deleted, one identity space, the boundary identity a pure function of the internal one — and it loses its performance justification entirely. The 90-minute rebuild and the bundle's irreversible deletion should be weighed against the architecture argument alone from here on; they buy nothing measurable against this specific tail.

## What the tail did correlate with, and what remains untested

Not page faults. Candidate explanations, named here as **untested hypotheses** — naming the next measurement is in scope for this memo; running it is not:

- **Per-request allocation in `compose`** (mask/fragment composition on the hot path) — an allocator-bound cost would show up as `p99` scaling with k (which it does, loosely) independent of residency.
- **`croaring` container materialisation on first touch of a range** — Roaring bitmap containers are lazily expanded/converted on access patterns that differ per request; a request touching a not-yet-materialised container pays a real, non-fault cost that would not appear in `majflt`.
- **mmap read-ahead behaviour** — even with pages resident, a fresh mapping's first touch can trigger kernel read-ahead work that isn't a "fault" in the `majflt` sense but still costs wall time.
- **Serialisation or HTTP/tokio scheduling jitter** — the end-to-end numbers (`e2e_p50`/`e2e_p99` in the JSON) run consistently ~2–5 ms above server-side numbers across every arm; that gap is fairly stable, so it is probably not the main story, but tokio scheduler jitter under load has not been ruled out as a contributor to the server-side tail itself.

**Outlier maxima, noted but set aside.** Arm A's single-request maximum at k=1000 is 7.13 **seconds**; arm B's is 1.75 s; C and D sit around 1.84–1.85 s. These are outlier-shaped (one or a handful of requests, not a distribution shift) rather than tail-shaped (which is a p99 phenomenon affecting ~1% of requests steadily) — almost certainly a different mechanism from the steady ~40 ms gap this memo is about (page cache eviction racing a specific request, a GC-like pause, or a scheduler stall), and is out of scope for this measurement's verdict. It is recorded here so a future investigation doesn't have to rediscover it.

**Swap.** All four arms show a modest swap delta (18–213 MB, `SwapFree` falling), consistent with the box (47 GiB RAM, 12 GiB swap) coming under some memory pressure during measurement, particularly for the larger-RSS arms (A: 158 MB, C: 213 MB) — not large enough to be a primary tail explanation on its own, but worth carrying forward if a later investigation touches memory pressure directly.

## Step 3a: settling the `terms/` line

One `du` on the existing (pre-rebuild) bundle, per the plan's request:

```
$ du -sh /tmp/tessera-1e9
48G     /tmp/tessera-1e9
$ du -sb /tmp/tessera-1e9
51142099202
$ du -sb /tmp/tessera-1e9/*/partitions/*/terms/
266622154
$ du -sb /tmp/tessera-1e9/*/partitions/*/terms/pairs.parquet
121442840
$ du -sb /tmp/tessera-1e9/*/partitions/*/terms/postings.arrow
145175218
$ du -sb /tmp/tessera-1e9/*/partitions/*/entities/
20250013716
$ du -sbc /tmp/tessera-1e9/*/partitions/*/slices/*/segments/*/columns.arrow
22625001234  total
$ du -sb /tmp/tessera-1e9/v00000/partitions/default/slices/s0/segments/seg-0/morton.u32
4000000000
$ du -sb /tmp/tessera-1e9/v00000/partitions/default/slices/s0/permutation.bin
4000000016
```

**`terms/` is 266,622,154 bytes ≈ 0.25 GiB — neither the plan's assumed 3.7 GiB residual nor contracts §2.4's ~5.6 GiB `pairs.parquet` estimate.** Both branches the plan's disk-gate arithmetic was built to choose between are wrong; the true figure is roughly an order of magnitude below the *smaller* of the two.

**Root cause of the 3.7 GiB residual theory: a GB/GiB unit conflation, not a real cost.** The "51.1 GiB" the plan's before-table was built to sum to is `frozen_bundle_bytes / 1e9` (decimal **GB**) as reported by `scripts/bench_p99.py` — `51,142,099,202 / 10⁹ = 51.14`. The same byte count in binary **GiB** is `51,142,099,202 / 2³⁰ = 47.63`. The ~3.5 GiB gap between those two units of the same number is what got attributed to an unmeasured `terms/` line, because a table whose rows were genuinely in GiB was being forced to sum to a total mislabelled as GiB when it was actually GB. Once every row is measured in GiB: `columns.arrow` 21.07 + `morton.u32`+`permutation.bin` 7.45 + `terms/` 0.25 + `entities/` (external-ID extents) 18.86 = **47.63 GiB**, which matches the measured `du -sb` total (`51,142,099,202` bytes = 47.630 GiB) to three decimal places (the ~36 KiB residual is `MANIFEST.json`/`CURRENT`/the dictionary). **The previous draft's 0.25 GiB figure for `terms/` — which the plan's current text calls out and dismisses as "exactly why its table failed to sum" — was correct all along.**

**Contracts §2.4's ~5.6 GiB `pairs.parquet` estimate is independently refuted, and this one is a real corpus finding, not a units bug.** `pairs.parquet` is 121,442,840 bytes ≈ 0.113 GiB, roughly 48× under the estimate. This says something true about the synthetic corpus: it carries far fewer term pairs per item than §2.4's estimate assumes. §2.4 should be annotated the next time it is touched; this memo does not amend it.

**Consequence for the disk gate: the after-rebuild total is smaller than either branch the plan was choosing between, not larger.** Using the measured `terms/` = 0.25 GiB in the plan's "after" column: `17.3 (columns) + 7.45 (morton+permutation) + 0.25 (terms) + 14.9 (extents, u32 entity) + 3.7 (locator) = 43.6 GiB`, against the plan's previous 47.0/48.9 GiB branches. Free space is ~15 GiB; the old bundle is 47.63 GiB measured (51.1 GB decimal). **The old and new bundles still cannot coexist** — that conclusion is unchanged — but the margin after a rebuild is more comfortable than previously projected. `docs/archive/plans/2026-07-30-external-id-identity.md`'s "Arithmetic" section has been corrected in the same commit as this memo, including the propagated figure at Task 14 Step 2.

## Summary for the owner

1. **REFUTED.** The ~42–47 ms viewport tail is not explained by cold major page faults. Arm D (post-rebuild residency simulated, fully pre-faulted) shows zero major faults and the tail intact.
2. **The rebuild's performance justification is gone; its architecture justification is not.** Simulating the rebuild's target residency (arms C/D, ~20 GB peak RSS) does not remove the tail either. The 90-minute rebuild and irreversible deletion of `/tmp/tessera-1e9` should be evaluated against the identity/architecture argument alone.
3. **`terms/` is settled at 0.25 GiB**, correcting the plan's arithmetic in both directions the plan considered — the after-rebuild bundle is smaller than projected, not larger.
4. **The real cause of the tail is still open.** Candidates named above (allocation, Roaring container materialisation, mmap read-ahead, scheduler jitter) are untested; this memo does not pick one.
