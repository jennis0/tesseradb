# Task 7 report — Parallel `Permutation::project` (cold-session serial block)

## Summary

Parallelised `Permutation::project` (`crates/tessera-store/src/permutation.rs`) using ambient
rayon (`par_chunks` over entity→row lookups → concat → `par_sort_unstable`), keeping
`tessera-store` executor-agnostic (no `rayon::ThreadPool` owned by the crate). The engine now
wraps its one call site (`RowProjection::new` inside the Task 1 slot-state builder closure,
`crates/tessera-engine/src/viewport.rs`) in `self.pool.install(..)`, so the cold-session build
runs on the engine's shared D-D pool rather than rayon's global default.

**Measured gate: PASSED (~2.2x at 8 threads, gate is ≥2x). Parallel implementation kept.**

## Implementation

### `crates/tessera-store/src/permutation.rs`

`project` now:
1. `mask.to_vec()` — materialise the entity-space set bits ascending (`entities: Vec<u32>`).
2. Compute a chunk length from the *ambient* pool's size (`rayon::current_num_threads()`), not a
   fixed constant — the store crate cannot know or assume how large the caller's pool is. Target
   is 8 chunks per worker (same over-subscription reasoning as `tessera-engine`'s
   `TILE_PAR_MIN_LEN`), so an unlucky worker landing a mostly-sentinel chunk doesn't stall the
   others.
3. `entities.par_chunks(chunk_len)` → map each chunk through `slots` (read-only `&[u32]`,
   never copied — the mmap-backed view is only borrowed) → per-chunk `Vec<u32>` of hit rows,
   skipping `ROW_ABSENT` sentinels and out-of-bound entities exactly as the serial code did
   (`slots.get(..)` returns `None` for out-of-bound, filtered by `filter_map`).
4. `.concat()` the per-chunk vectors into one `rows: Vec<u32>`.
5. `rows.par_sort_unstable()`.
6. `croaring::Bitmap::of(&rows)` — unchanged from before (uniqueness by construction: a
   permutation is a bijection, so no two entities can produce the same row, regardless of which
   chunk found them).

Doc comment states: executor-agnostic contract, that chunk boundaries don't affect the result
(order-independence → output bytes cannot change with thread count), and the transient memory
note (mask-derived `entities` vec + `rows` vec both live at once — roughly double
`mask.cardinality()` `u32`s on top of the never-copied mmap `slots` array).

### `crates/tessera-store/Cargo.toml`

Added `rayon = { workspace = true }` (workspace already pins `rayon = "1"`). No pool constructed
anywhere in this crate — `check-layers.sh`'s `deny tessera-store tokio` is unaffected and rayon in
store is explicitly fine per the brief.

### `crates/tessera-engine/src/viewport.rs`

The `get_or_build` closure's body:
```rust
probe.mark_projection_built();
self.pool
    .install(|| RowProjection::new(&session.fragment, &view_data.permutation))
```
Only the `RowProjection::new` call is wrapped, not the whole `get_or_build` — keeps the
single-flight map lock's O(1) hold time (D-G) unaffected by the pool boundary; `pool.install` is
called after the lock has already been released inside `get_or_build`.

## THE GATE NUMBERS

Command: `cargo test -p tessera-store --release --test permutation_project_parallel -- --ignored
--nocapture project_parallel_speedup_at_8_threads`

Fixture: `n = 16,000,000` entities, a genuine permutation (every entity has a unique shuffled
row, no sentinels), mask = every entity (cardinality 16M) — the worst-case (largest) row-lookup
workload for this size. Release mode. Page cache warmed with one untimed run before both timed
runs; best-of-3 per thread count.

Run 1: **1 thread = 464.902271ms, 8 threads = 208.290204ms, speedup = 2.23x**
Run 2 (rerun to check stability): **1 thread = 518.263428ms, 8 threads = 233.269243ms, speedup = 2.22x**

Both runs clear the ≥2x gate with a consistent ~10% margin. Box: 12 logical cores, WSL2,
memory-constrained (kept well under the box's OOM history: ~128MB transient for `entities` +
`rows` at this `n`, plus a ~64MB mmap-backed `permutation.bin`).

**Gate verdict: PASS. Action: parallel implementation kept, no revert.**

## TDD evidence

1. Wrote `crates/tessera-store/tests/permutation_project_parallel.rs` first, against the
   *still-serial* `project`. All 6 non-ignored tests passed immediately (expected — wrapping a
   serial call in `pool.install` at various thread counts is a no-op until `project` itself uses
   rayon), establishing the baseline the parallel implementation is verified against.
2. Implemented the parallel `project`.
3. Reran the same test file — all 6 tests still pass, this time genuinely exercising multi-worker
   chunking (thread counts 1/2/4/8 depending on the test).
4. Ran the ignored gate test in release mode (see above).

Correctness tests, independent of the byte-equality regression tests already in
`bundle_read.rs`:
- `empty_mask_projects_to_empty` — thread counts 1, 4.
- `all_entities_mask_projects_every_row` — a genuine bijection (no sentinels), thread counts 1,
  2, 4, 8.
- `sentinel_and_out_of_bound_entities_are_skipped_not_erred` — every third entity assigned, mask
  covers all in-bound entities plus `n+500` and `u32::MAX` (both far out of bound) — thread
  counts 1, 4, 8.
- `mask_touching_first_and_last_slot` — entities `0` and `n-1` only, so the first and last chunk
  boundary is exercised directly — thread counts 1, 4.
- `random_masks_match_the_serial_reference` — 10 random 30%-density masks over a permutation with
  a 10% sentinel gap, thread counts 1, 8.
- `project_on_the_global_pool_without_an_explicit_install_still_matches` — no `pool.install` at
  all (rayon's global pool), confirming the executor-agnostic fallback path.

Oracle used throughout: `serial_project`, a from-scratch reimplementation walking `row_of` one
entity at a time — deliberately *not* a second call to `Permutation::project`, since after this
change that method's internals are the thing under test.

## Files changed

- `crates/tessera-store/src/permutation.rs` — parallel `project`.
- `crates/tessera-store/Cargo.toml` — `+rayon`.
- `crates/tessera-engine/src/viewport.rs` — `self.pool.install(..)` around the `RowProjection::new`
  call site.
- `crates/tessera-store/tests/permutation_project_parallel.rs` — new: correctness tests + the
  `--ignored` gate test.
- `Cargo.lock` — updated for the new `tessera-store` → `rayon` edge (rayon was already present in
  the lockfile via other crates; this adds the direct edge).

## Verification run

- `cargo test -p tessera-store` (debug, all non-ignored): pass, including the pre-existing
  `permutation_project_matches_per_entity_row_of_loop` byte-equality-style test in
  `bundle_read.rs`, unchanged and still passing against the now-parallel `project`.
- `cargo test --workspace`: all crates pass (0 failures across the whole run — greped for
  `FAILED|error\[|error:` in the full log, none found besides the ok summaries).
- `bash scripts/check-layers.sh`: exit 0.
- Known environmental flake (`write_permutation_rejects_entity_id_not_fitting_u32`, OOM-SIGKILL
  risk per the task brief): ran twice as part of the `segment_roundtrip.rs` suite (54s and 56s),
  passed both times, no OOM observed this session.

## Self-review

- **Executor-agnostic held**: grepped the diff — no `rayon::ThreadPool` construction anywhere in
  `tessera-store`; `rayon::current_num_threads()` only *reads* the ambient pool's size, it does
  not build one. `check-layers.sh` still passes (rayon in store is explicitly permitted by the
  brief; tokio-in-store deny is untouched and irrelevant here).
- **Correctness under chunking**: verified the chunk-boundary case explicitly
  (`mask_touching_first_and_last_slot`), and thread counts up to 8 against masks sized in the
  thousands to tens of thousands so `chunk_len` (`entities.len() / (threads*8)`) is small enough
  that several real chunk boundaries exist even at low cardinality — not just a degenerate
  single-chunk case.
- **Sort/uniqueness**: unchanged reasoning from the pre-existing code (bijection ⇒ no duplicate
  rows), `par_sort_unstable` on `u32` is a total-order sort so output bytes are deterministic
  regardless of the chunking/scheduling that produced `rows` beforehand.
- **Cache lock scope**: double-checked that `self.pool.install(..)` in `viewport.rs` wraps only
  the `RowProjection::new` call, called from inside the `build` closure `SingleFlightCache::
  get_or_build` invokes *after* releasing its map lock (confirmed by reading
  `crates/tessera-engine/src/single_flight.rs`) — so D-G's "lock held only for the O(1)
  transition" guarantee is untouched.
- **Gate honesty**: ran the timing test twice, not once, given the ~10-11% margin over the 2x
  threshold and the shared/WSL2 box — both runs agreed (2.23x, 2.22x), so this isn't a
  one-off favourable scheduling fluke.

## Concerns

- **Disk space**: this box's root filesystem was at 100% (121 MB free) partway through this task,
  which failed `cargo test --workspace` mid-build (`No space left on device`) on an unrelated
  crate (`tessera-server`'s `http` test binary). Freed space with `cargo clean -p tessera-bench`
  and `cargo clean -p tessera-cli` (3.4 GB reclaimed, both are downstream/leaf crates unrelated to
  this task's changes) rather than any destructive raw deletion, then reran successfully. This is
  an environmental condition of the shared box/target dir, not something introduced by this
  task's changes — worth flagging since it will recur for later tasks in this workstream unless
  addressed (a periodic `cargo clean -p` of leaf/bench/cli crates, or a bigger disk).
- The ~2.2x speedup margin over the 2x gate (~10-11%) is real and reproduced twice, but it is not
  a large margin — a busier box or a different `n` could plausibly land just under 2x on a bad
  run. I did not treat this as gate-failure grounds (the brief's gate is about the representative
  measurement, which passed twice), but it's worth knowing this isn't a 5x-headroom result.
- `permutation_project_parallel.rs`'s gate test asserts cardinality equality (`out.cardinality()
  == n`) rather than hard-failing on `speedup < 2.0`, per the brief's own instruction that a gate
  failure is a *code* decision (revert), not a test assertion — so this test will never itself go
  red on a slow box; the printed `speedup=` line is what a human (or a future task) must read.

---

## Fix round 1 (review: Important + fold-in Minor)

### Issue

Review found `crates/tessera-store/src/permutation.rs:204,214,222` peaked at ~3x
`mask.cardinality()` transient `u32`s, not the documented ~2x: `entities` (from `mask.to_vec()`)
and `per_chunk` (`Vec<Vec<u32>>` from the chunked map) were both still live when `per_chunk.concat()`
built `rows`, so `entities` + `per_chunk` + `rows` (+ the output bitmap) coexisted momentarily.
The doc comment (permutation.rs:193-196, pre-fix) and the gate test's sizing note
(permutation_project_parallel.rs:~211-213, pre-fix) both stated "~128 MB at n=16M" / "roughly
double" — both undercounted the true ~192 MB / 3x peak. At the 10⁹/69M-entity design target that
gap is ~830 MB transient vs the claimed ~276 MB, on a box with an OOM-kill history — a real risk,
not just a doc inaccuracy.

Folded in (same fix family): the per-chunk `collect()` had no capacity hint, so each chunk's local
`Vec` regrew from zero via amortised doubling — extra realloc churn stacked on top of the 3x peak.

### Fix

`crates/tessera-store/src/permutation.rs`, `Permutation::project`:
1. The per-chunk map now pre-sizes each chunk's local `Vec` with `Vec::with_capacity(chunk.len())`
   (an exact upper bound — filtering can only shrink a chunk's hit count, never grow it) and fills
   it via `.extend(..)` instead of `.collect()` — no reallocation as it fills.
2. `drop(entities)` immediately after the `per_chunk` collect — `entities` is provably dead from
   that point (nothing downstream reads it), so its allocation is freed *before* `rows` is even
   reserved, rather than living until the function returns.
3. `rows` is no longer built via `per_chunk.concat()` (which requires the *whole* of `per_chunk`
   to stay alive alongside the *whole* of the newly-built `rows`). Instead: compute
   `total_rows = per_chunk.iter().map(Vec::len).sum()`, reserve `rows` at that exact capacity
   once (`Vec::with_capacity(total_rows)`, no realloc during fill either), then
   `rows.extend(per_chunk.into_iter().flatten())` — this consumes `per_chunk` by value, so each
   inner chunk `Vec` is dropped as soon as `flatten`/`extend` has drained it, rather than the
   entire structure staying resident until `rows` is complete.

Net effect: peak transient is now genuinely bounded by two `mask.cardinality()`-sized buffers
overlapping at once (either "`entities` + the just-finished `per_chunk`" during phase 1, or "the
just-finished `per_chunk` + `rows`'s freshly-reserved capacity" at the phase-1/phase-2 boundary),
never three. Both doc comments (permutation.rs's `project` doc, and the gate test's sizing note)
were rewritten to state this precisely rather than just asserting a number.

### Covering tests re-run

Correctness suite (debug, all thread-count variants — empty/all-entities/sentinel+out-of-bound/
first-last-slot/random masks at 1/2/4/8 threads, plus the no-`install` global-pool path):
```
cargo test -p tessera-store
```
Result: all 6 non-ignored tests in `permutation_project_parallel.rs` pass, plus every other
pre-existing test in the crate (`bundle_read.rs`'s byte-equality-style projection test included).

Ignored gate test, re-run twice in release mode to confirm the fix didn't cost the speedup:
```
cargo test -p tessera-store --release --test permutation_project_parallel -- --ignored --nocapture project_parallel_speedup_at_8_threads
```
Run 1: `n=16000000 1-thread=484.962465ms 8-thread=189.57482ms speedup=2.56x`
Run 2: `n=16000000 1-thread=491.034096ms 8-thread=187.244498ms speedup=2.62x`

Speedup *improved* (was 2.22-2.23x pre-fix) — removing the realloc churn and the third live buffer
helped both the 1-thread and 8-thread arms, netting a better margin over the >=2x gate, not a
worse one. Gate verdict unchanged: PASS, parallel implementation kept.

Full re-verification before commit:
- `cargo test --workspace` — all green (checked via `grep -E "FAILED|error\[|error:" ` over the
  full log; none found).
- `bash scripts/check-layers.sh` — exit 0.

### Minor-3 (record only, no code change)

The 2.56-2.62x gate figure (and the original 2.22-2.23x) is the new parallel code at 1 thread
against itself at 8 threads — the brief's stated baseline. It is **not** a comparison against the
pre-branch serial `for`-loop-over-`row_of` implementation, which had no `to_vec`/chunking/concat
overhead at all. Against that original serial loop, the real cold-session wall-clock gain at 8
threads is likely somewhat under the measured ~2.5x, plausibly nearer the 2x gate threshold itself
— the 1-thread arm of the *parallel* code carries the `to_vec()` + chunk-management overhead that
the original serial loop didn't pay, but the 8-thread arm reclaims that overhead via
parallelism, which is what the ratio's improvement after this fix confirms (less overhead helped
both arms, but the ratio itself isn't a like-for-like "old serial loop vs new parallel code"
comparison). No action taken — this is the honest caveat on what the gate number does and doesn't
prove, for the record.
