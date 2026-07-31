# `StageTimings.rows_in_ranges` mask-independence fix

Defect: `.superpowers/sdd/i-d-like-you-to-jiggly-cupcake/calibration-report.md` §14.2. Fixed on
`concurrency/viewpath` at 395bc11.

## Choice and argument

**(a) — moved the counting into the serial prefix, deleted the field from `TileStats`.**

`Engine::viewport` already materialises `ranges: Vec<Range<u32>>` (via `tile_ranges_all`) before
the tile sweep, and already summed it once for the serial/parallel predictor
(`total_rows_in_ranges = Σ range.len() + underlay_cells_demanded`). `rows_in_ranges` is exactly
that same sum minus the underlay term, so computing it there is not new work bolted on — it reuses
a sum the function already builds, one line earlier than the predictor that consumes it:

```rust
let rows_in_ranges: u64 = ranges.iter().map(|r| r.len() as u64).sum();
probe.count(|t| &mut t.rows_in_ranges, rows_in_ranges);
let total_rows_in_ranges: u64 = rows_in_ranges + underlay_cells_demanded;
```

This is mask-free by construction: `ranges` is built from `tile_ranges_all(segment, &tiles)`
alone, before `mask` is read anywhere in the request. There is no `Ok(None)` branch this counting
can be discarded by, because it no longer lives inside `tile_result` at all — I removed the field
from `TileStats` outright rather than leaving it there unused, so a future per-tile addition to
`TileStats` cannot silently reintroduce the same class of bug by copying the pattern.

Rejected (b) — folding `Ok(None)`'s stats too: `TileProbe::new()` for an empty tile still measures
nothing else (no `count_ns`/`select_ns`/etc. — the tile returns before any of those laps), so
threading a stats payload through the `Ok(None)` cases only to carry one already-computable field
is a wider, less obviously-correct change for the same result. It would also leave `rows_in_ranges`
looking like a genuinely per-tile, cross-worker-summed quantity in the doc (D-E's list), which it
structurally is not — it never depended on which worker ran which tile, only on geometry.

`tiles_resolved` — checked per the brief. It does **not** share the defect: it is counted once from
`tiles.len()` in the serial prefix (`probe.count(|t| &mut t.tiles_resolved, tiles.len() as u64)`,
before the match on `underlay_offset` even), never touched inside `tile_result` or `TileStats`. No
change needed there; documented in the new test and in `timing.rs`'s struct doc as the existing
mask-independent precedent this fix now matches `rows_in_ranges` to.

## Files changed

- `crates/tessera-engine/src/viewport.rs`
  - `Engine::viewport`: computes `rows_in_ranges` once from `ranges`, counts it into `probe.t`,
    reuses it for `total_rows_in_ranges` (behaviourally identical value, same underlay term added).
  - `tile_result`: removed `stats.count(|t| &mut t.rows_in_ranges, range.len() as u64)` — the sole
    per-tile counting site, and the sole `TileStats` field discarded by an `Ok(None)` return.
- `crates/tessera-engine/src/timing.rs`
  - `TileStats`: dropped the `rows_in_ranges` field; `TileStats::fold_into` no longer sums it.
  - `StageTimings` module doc and the `rows_in_ranges` field doc: clarified it is a serial-prefix,
    mask-independent quantity (like `tiles_resolved`), not a per-tile cross-worker sum — removed it
    from the D-E "per-tile counter" list and added a paragraph naming the fold-discards-`Ok(None)`
    bug this fix closes, so a future reader does not reintroduce it by analogy.
  - `TileStats` struct doc: added a paragraph explaining why the field was removed, as a guard for
    anyone tempted to add a similar "we already know this before the sweep" field back into the
    per-tile struct.
- `crates/tessera-engine/tests/viewport.rs`
  - New test `rows_in_ranges_is_mask_independent` (`#[cfg(feature = "bench-timing")]`).

No change to `crates/tessera-bench` or `crates/tessera-server`, which only *read*
`StageTimings.rows_in_ranges` — their code is unaffected by the field's value becoming correct.
`crates/tessera-engine/examples/calibration_sweep.rs` and `min_len_sweep.rs` already carry their
own doc-level workaround for this exact bug (computing `true_rows_in_ranges` independently, because
they predate this fix and could not wait for it) — left untouched as out of scope; their workaround
is still correct, just now redundant with the underlying field.

## TDD evidence

Failing test first, against the pre-fix code:

```
$ cargo test -p tessera-engine --features bench-timing --test viewport rows_in_ranges_is_mask_independent
thread 'rows_in_ranges_is_mask_independent' panicked:
assertion `left == right` failed: rows_in_ranges is documented mask-independent (Sigma range.len()
over resolved tiles) -- it must not depend on which tiles the grant leaves empty (serial fold):
980 (full) vs 0 (zero)
```

This reproduces the calibration report's 26x-gap finding at unit-test scale: a zero-coverage grant
(`zero_credential`, valid-but-sees-nothing per R5) leaves every touched tile empty, so under the
bug its `rows_in_ranges` collapsed to 0 while a full-coverage grant over the byte-identical viewport
(`zoom = 4`, bbox covering a quadrant — same shape as the existing non-degenerate brute-force test)
reported the true 980-row span.

After the fix, same command: `1 passed; 0 failed`.

The test also forces the parallel fan-out via the existing test-only
`Engine::set_serial_fallback_max_rows_for_test(0)` override and re-asserts both grants agree with
each other AND with the serial-fold figures above — covering "counters are always-on" (checked:
`Probe`/`TileProbe` compile to no-ops without `bench-timing`, confirmed by
`disabled_probe_reports_not_enabled_and_stays_zero` — so counters are feature-gated, not always-on;
the new test is itself `#[cfg(feature = "bench-timing")]` for that reason, matching the file's
existing `f1_selection_visits_exactly_the_visible_set` pattern) and "serial and parallel paths
agree" in one test.

Full verification, all green:

- `cargo test -p tessera-engine --features bench-timing` — 41 passed, 1 ignored (pre-existing
  `#[ignore]`d latency-sanity test), across `viewport.rs`, `selection.rs`, `send_sync.rs`, plus the
  lib's own 37 unit tests including `timing::tests::*`.
- `cargo test -p tessera-engine` (no feature) — 39 passed, 1 ignored (2 fewer than above: both
  feature-gated tests, `f1_selection_visits_exactly_the_visible_set` and the new
  `rows_in_ranges_is_mask_independent`, are absent, as expected).
- `cargo test --workspace` — all crates green, no failures.
- `cargo clippy --workspace --all-targets` — clean.
- `cargo clippy -p tessera-engine --all-targets --features bench-timing` — clean (confirmed a real
  recompile under the feature via a touch on `timing.rs` first).
- `bash scripts/check-layers.sh` — clean, exit 0.
- `cargo fmt --all -- --check` — pre-existing diffs in unrelated files
  (`tessera-authz/src/postings.rs`, `single_flight.rs`, `tessera-build/src/input.rs`, likely a
  rustfmt version skew in this environment); none in the three files this fix touched.

Preserved, verified explicitly:
- D-E duration semantics for per-tile timing fields (`count_ns`/`select_ns`/`gather_ns`/
  `underlay_ns`) — untouched; only a counter field moved.
- Byte-identical `ViewportOut` responses — the existing
  `viewport_output_is_byte_identical_at_compute_threads_1_and_8(_with_sparse_empty_tiles)` tests
  still pass unmodified (they don't compare `timings`, by design, but do exercise the exact
  `Ok(None)`-skip and multi-tile code paths this fix touches).
- Serial/parallel agreement on `rows_in_ranges` itself — new, asserted directly by the new test
  (not covered by the byte-equality tests, since `PartialEq` on `ViewportOut` ignores `timings`).

## Self-review

- Checked every other consumer of `TileStats`/`StageTimings::rows_in_ranges` in the workspace
  (`tessera-bench`'s `report.rs`, `arms/viewport.rs`, `arms/changes.rs`, `arms/ingest.rs`,
  `arms/tiles.rs`, `arms/gather.rs`; `tessera-server/src/viewer.rs`) — all read the field, none
  write or duplicate its counting logic, so none needed a code change.
- Confirmed `range` (the `Range<u32>` parameter to `tile_result`) is still used after removing its
  `rows_in_ranges` counting — yes, by `mask.count_range(range.clone())` two lines below and later
  by `Selection::of`, so no dead parameter was introduced.
- Confirmed the `total_rows_in_ranges` value read by `should_fold_serially` (the calibration
  predictor governing `SERIAL_FALLBACK_MAX_ROWS`) is numerically unchanged: it is still
  `Σ range.len() + underlay_cells_demanded`, only refactored to share the sum with the new
  `rows_in_ranges` counter rather than recomputing it — so the serial/parallel calibration from
  §14 is untouched by this fix.
- Grepped the whole workspace for `rows_in_ranges` post-fix to confirm no stale reference to the
  removed `TileStats` field remained (compiler would have caught it regardless, but confirmed the
  grep is clean of any doc comment now describing a moved behaviour incorrectly).
