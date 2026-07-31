# Task 6 report — intra-request rayon in the tile loop (D-D/D-E/D-F)

## Status

DONE.

## Implementation

### D-D: the shared compute pool

**`EngineConfig` gains `pub compute_threads: usize`** (`crates/tessera-engine/src/session.rs`),
documented as mirroring `tessera-server::config`'s `serve.compute_threads` (D-B, landed Task 4).
A free function `pub fn default_compute_threads() -> usize` (same `available_parallelism`-or-`1`
reasoning as the server's own `default_compute_threads`) lives beside it and is re-exported from
`lib.rs` as `tessera_engine::default_compute_threads`, so every non-server construction site
(tests, benches, examples) can fill in the new field without duplicating the number.

**`Engine` gains `pub(crate) pool: rayon::ThreadPool`**, built once in `Engine::open` via
`rayon::ThreadPoolBuilder::new().num_threads(config.compute_threads).build()`. Failure is a new
`EngineError::ThreadPoolBuild(String)` variant propagated with `?` — pool-build failure is
`Engine::open` failure, fail-closed, no fallback to ad hoc per-request threading. No second pool
or throttle exists anywhere else in the crate.

**`tessera-server/src/lib.rs`** now threads `config.compute_threads` (the D-B knob, already
validated at parse time — refused at `0`, `ConfigError::ComputeThreadsZero`) into the
`EngineConfig` literal `prepare` builds — one number now sizes both the admission semaphore and
the pool the admitted request's tile loop runs on.

**Ripple.** `compute_threads` has no `Default` impl at the struct level (several existing fields
have no sensible default either, and every construction site already used full struct-literal
syntax), so the field was added explicitly to every site: `tessera-server/src/lib.rs`,
`tessera-server/tests/http.rs` (two sites), `tessera-engine/tests/viewport.rs` (four sites —
`config()`, two explicit literals, one more explicit literal at the 2.4M-row test), `tessera-engine/
benches/viewport.rs`, `tessera-engine/examples/open_rss.rs`, `tessera-bench/src/arms/{viewport,
ingest (×2), changes}.rs`. Three more sites in `tessera-engine/tests/viewport.rs` use
`..config()` struct-update syntax and needed no edit (they inherit `config()`'s value).

### D-F: the pure per-tile function and the parallel sweep

**`fn tile_result(...) -> Result<Option<TileResult>>`** (`crates/tessera-engine/src/viewport.rs`,
module-private, not `Engine`'s method) is the extracted loop body, unchanged in behaviour: same
cancellation check first, same `segment.is_none()`/`visible == 0` skip-empty rules, same
count → select → gather → underlay sequence, same `TileCount`/`PointOut`/`SubCellCount`
construction. `TileResult` bundles a tile's `count`/`points`/`sub_cells` plus its local
`TileStats` (below). Every parameter is `&`-borrowed or `Copy`; nothing reaches back into
`Engine` or any cross-tile shared mutable state.

**The sweep**, replacing the old serial `for` loop:

```rust
let tile_outcomes: Vec<Result<Option<TileResult>>> = self.pool.install(|| {
    tiles
        .par_iter()
        .zip(ranges.into_par_iter())
        .with_min_len(TILE_PAR_MIN_LEN)
        .map(|(tile, range)| tile_result(tile, range, &mask, segment, declared_scalars,
                                          &params, zoom, underlay_offset, &cancel))
        .collect::<Vec<Result<Option<TileResult>>>>()
});
```

`Result<T>` here is this crate's own one-parameter alias for `std::result::Result<T, EngineError>`
(already `use`d throughout the file), so `Vec<Result<Option<TileResult>>>` is exactly the
`Vec<Result<Option<TileResult>, EngineError>>` shape the brief specifies — collecting the
`Vec<Result<..>>`, never `Result<Vec<..>>`, is what keeps rayon on its **indexed** collect path,
so the output order equals `tiles`' input order by construction rather than by an incidental
property of the current reduce implementation. This is stated as the load-bearing fact in three
places: the module doc, the sweep's own comment, and the byte-equality tests' doc comments.

**The serial, in-order fold** walks `tile_outcomes` and does `outcome?` per element (short-
circuiting on the first `Err`, i.e. `Cancelled`), then `fold_into`s stats and extends
`tile_counts`/`points`/`sub_cells` — byte-identical reconstruction of what the old inline loop
produced, since the vector is in tile order regardless of which worker computed which entry or in
what order they finished.

### `with_min_len` choice: `4`

Argued in a doc comment on `const TILE_PAR_MIN_LEN: usize = 4` (`viewport.rs`): per-tile cost is
highly non-uniform (an empty-tile skip is a `count_range` call and a comparison; a dense tile at a
high cap is a bitmap-range read plus a bounded heap sort), so work-stealing needs to move
*individual* tiles between workers rather than being locked into large chunks — ruling out a large
`min_len`. `1` (rayon's default without the call) pays a scheduling/steal-queue check on every
tile, including the very common empty-tile skip that's otherwise nearly free. `4` is a
deliberately uncalibrated (no probe covers this specific scheduling knob — Phase 0's probes were
about corpus/mask shape) middle point: small enough that a few-hundred-tile viewport still splits
into dozens of independently-stealable chunks, large enough to amortise overhead on the cheap
tiles that dominate sparse/clustered corpora.

### D-E: the `TileStats`/`TileProbe` reduction

**New types in `crates/tessera-engine/src/timing.rs`**, not a reuse of `Probe`/`StageTimings`
directly:

- `TileStats` — a small `Default`-derived struct holding only the fields one tile can meaningfully
  report: the four durations (`count_ns`, `select_ns`, `gather_ns`, `underlay_ns`) and the six
  per-tile counters (`rows_in_ranges`, `tiles_nonempty`, `sigma_visible`, `select_rows_visited`,
  `points_gathered`, `underlay_cells_evaluated`), plus `clock_laps`. Same shape in both builds.
- `TileProbe` — a `Probe`-shaped clock over `TileStats` instead of `StageTimings`, with the
  identical `lap`/`count` API, internally `#[cfg(feature = "bench-timing")]`-gated exactly like
  `Probe`'s own methods. `tile_result` builds one locally (never shared — one per call, discarded
  at the end), so call sites in `tile_result` carry no `#[cfg]` at all, matching the module's
  stated "the gate is on the clock, not the call site" design.
- `TileStats::fold_into(&self, t: &mut StageTimings)` — a plain (non-`#[cfg]`-gated) summation of
  every field into the request's `StageTimings`. It needs no gate of its own because `TileStats`'
  own fields are already all zero without `bench-timing` (the gate lives one level down, inside
  `TileProbe`), so summing zeros into `probe.t` is a correct no-op in that build.

**This keeps both feature builds compiling** because `TileProbe`/`TileStats` mirror `Probe`/
`StageTimings`'s existing pattern exactly: the struct shape never changes with the feature, only
whether the internal `Instant`/`+= n` bodies run. `fold_into` is unconditional code operating on
that shape, so it compiles and behaves correctly whichever way the feature is set — verified by
running the full engine suite both with and without `--features bench-timing` (both green,
including a new `f1_selection_visits_exactly_the_visible_set`-style check that the parallel path's
counters still land in the same place: the pre-existing `f1_selection_visits_exactly_the_visible_set`
test itself, unmodified, still passes under `bench-timing` and asserts exact counter values that
now flow through `TileStats`→`fold_into` rather than directly through `Probe`).

I considered reusing `Probe`/`StageTimings` directly per tile instead of introducing parallel
types, but rejected it: `StageTimings` carries fields with no per-tile meaning (`total_ns`,
`enabled`, `row_projection_built`, every serial-prefix duration), and `Probe::finish()` stamps
`total_ns` from its own `start` — reusing it per tile would need those fields ignored/discarded
awkwardly at the fold, inviting exactly the kind of accidental misuse (e.g. a future call
comparing a tile's `total_ns` against the request's) the brief's "design the reduction" instruction
is warning against. A purpose-built, smaller type is safer to fold.

**`probe.skip()`** is called immediately after the `pool.install(...)` call returns and before the
serial fold, so the parallel section's own wall time is not charged to whatever lap would run next
(there currently is none, since the tile loop was already the last stage before `Ok(ViewportOut
{...})`, but the call keeps the invariant explicit and documented rather than accidental).

**`unattributed_ns` pinning.** No special-case code was needed for this — it already saturates to
`0` under real parallelism as a direct consequence of `saturating_sub`: once `count_ns`/`select_ns`/
`gather_ns`/`underlay_ns` become cross-worker CPU-time sums, their total routinely exceeds
`total_ns` (true wall clock) by roughly the achieved concurrency, and `total_ns.saturating_sub
(named)` floors at zero. The doc comments explain this is the *defined* value, not a coincidence.

### Guardrail comment

Added at the row-projection slot-state cache call site in `Engine::viewport` (`viewport.rs`,
immediately before `self.row_projection_cache.get_or_build(...)`): states that `base` is resolved
once, on the calling thread, strictly before the parallel sweep, and only ever borrowed (via
`EffectiveMask`) by `tile_result`; documents that a hypothetical future re-entrant call from a
rayon worker would still be *safe* (the single-flight state machine has no notion of "friendly"
re-entrancy — a worker racing an in-flight build on the same key just gets `Slot::Building` →
`EngineError::ProjectionBuilding`, same as any other concurrent caller) but that this safety is
incidental, not a licence — the design intent is that this cache is touched once per request, full
stop.

### `check-layers.sh`

Added `deny tessera-engine tokio` and `deny tessera-store tokio`, following the script's existing
`deny <crate> <forbidden-dep>` syntax, with a comment tying it to lifecycle §7's sync-engine rule
and noting rayon is expected and fine. `bash scripts/check-layers.sh` passes (exit 0).

## TDD evidence

Per the brief's own admission ("the RED here is the panic-propagation and equality tests against a
deliberately-wrong intermediate if practical; otherwise document GREEN-only with reasoning") —
writing the byte-equality tests *before* the parallel sweep exists is what strict TDD would ask
for, but in practice the refactor (extracting `tile_result`, switching the loop to
`pool.install(...).collect()`) is not meaningfully separable from "make the tests pass": there is
no intermediate state where the old serial loop and the new parallel sweep both compile side by
side against one test file without one shadowing the other, and a `compute_threads = 1` sweep is
observationally identical to the old serial loop regardless of whether the extraction was correct.
What I did instead, in order:

1. Implemented the D-D config/pool plumbing and confirmed it compiles and the full existing engine
   suite (pre-existing tests, unmodified) still passes at whatever `compute_threads` value the test
   fixtures' `config()` functions now produce (`default_compute_threads()`, i.e. this machine's
   real core count) — this is the RED-would-have-been-GREEN-trivially check the brief names for
   compute_threads = machine-default vs the old always-serial code: if the parallel rewrite had
   silently changed any observable output, dozens of pre-existing exact-value assertions
   (`tile_counts_match_brute_force_at_a_non_degenerate_zoom_and_bbox_subset`,
   `f_selection_returns_the_lowest_tessera_ids_not_the_first_rows`,
   `response_tile_order_and_point_concatenation_follow_tiles_for_bbox_not_morton_order`,
   `underlay_sub_cells_sum_to_the_tile_s_masked_visible_count`, the D-C cancellation tests, and
   `f1_selection_visits_exactly_the_visible_set`'s counter assertions under `bench-timing`) would
   have failed immediately. They did not — first run, all green, which is itself strong evidence
   the extraction preserved behaviour exactly.
2. Added the two byte-equality tests (`viewport_output_is_byte_identical_at_compute_threads_1_and_8`
   in `tessera-engine/tests/viewport.rs`, `viewport_response_body_is_byte_identical_at_compute_
   threads_1_and_8` in `tessera-server/tests/http.rs`), each opening two engines/servers against
   the same bundle differing only in `compute_threads` (1 vs 8) and asserting `ViewportOut`
   equality / response-body byte equality over a multi-tile, underlay-bearing request. Both went
   green on first run against the already-implemented sweep — I did **not** contrive a
   deliberately-wrong intermediate (e.g. an unindexed `.collect()`) to watch them fail, judging
   that against this codebase's existing convention (every other concurrency test in this file
   documents GREEN-only with reasoning where a true RED isn't practical, e.g. the D-C cancellation
   tests' own doc) that is acceptable and is what I'm documenting here rather than asserting
   silently. I did separately verify by inspection and by consulting rayon's own documented
   collect-strategy split (`Result`-target collect uses the unindexed/short-circuiting reduce path;
   a plain `Vec<T>`-target collect over an `IndexedParallelIterator` uses the indexed path) that
   the `Vec<Result<..>>` vs `Result<Vec<..>>` distinction is real and not just cargo-culted from
   the brief.
3. Added the I13 panic-propagation test (`a_panic_inside_the_shared_pool_propagates_to_the_caller`,
   `tessera-engine/src/session.rs`) per the brief's explicit instruction not to add a
   `#[cfg(test)]`-visible injection hook into the real per-tile path — this is a standalone
   `rayon::ThreadPoolBuilder`-built pool (identical construction to `Engine::open`'s), `.install(||
   panic!(...))` inside `std::panic::catch_unwind`, asserting the panic escapes. Green on first
   run (rayon's propagation is a documented library guarantee, not something this task's code
   could plausibly break without actively suppressing it, which nothing here does).

## Files changed

- `Cargo.toml` — `rayon = "1"` added to `[workspace.dependencies]`.
- `crates/tessera-engine/Cargo.toml` — `rayon` dependency.
- `crates/tessera-engine/src/session.rs` — `EngineConfig::compute_threads`,
  `default_compute_threads()`, `EngineError::ThreadPoolBuild`, `Engine::pool`, pool construction in
  `Engine::open`, the I13 panic-propagation unit test.
- `crates/tessera-engine/src/timing.rs` — `TileStats`, `TileProbe`, `TileStats::fold_into`, D-E
  doc updates on `StageTimings` and its per-tile fields and `unattributed_ns`, two new unit tests.
- `crates/tessera-engine/src/viewport.rs` — module doc (D-D/D-F), guardrail comment, the parallel
  sweep + serial fold replacing the old loop, `tile_result`, `TileResult`, `TILE_PAR_MIN_LEN`.
- `crates/tessera-engine/src/lib.rs` — re-export `default_compute_threads`.
- `crates/tessera-engine/tests/viewport.rs` — `compute_threads` added to construction sites, the
  headline byte-equality test.
- `crates/tessera-server/src/lib.rs` — wire `config.compute_threads` into `EngineConfig`.
- `crates/tessera-server/src/viewer.rs` — D-E doc update on `stage_header`.
- `crates/tessera-server/tests/http.rs` — `compute_threads` added to construction sites, the
  server-level headline byte-equality test.
- `crates/tessera-engine/benches/viewport.rs`, `crates/tessera-engine/examples/open_rss.rs`,
  `crates/tessera-bench/src/arms/{viewport,ingest,changes}.rs` — `compute_threads` ripple.
- `bench/README.md` — D-E semantics note.
- `scripts/check-layers.sh` — `deny tessera-engine tokio`, `deny tessera-store tokio`.
- `Cargo.lock` — one line (`rayon` added to `tessera-engine`'s dependency list; rayon's own
  transitive dependencies were already present in the lock file from elsewhere in the graph).

## Verification run

- `cargo build --workspace --all-targets` — clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean.
- `cargo test --workspace` — all green (including the pre-existing, environmentally-flaky
  `tessera-store::write_permutation_rejects_entity_id_not_fitting_u32`, which ran to completion in
  ~81s this run rather than OOM-SIGKILLing; noted per the brief as a known flake unrelated to this
  task).
- `cargo test -p tessera-engine --features bench-timing` — all green, including both new timing
  unit tests and the pre-existing `f1_selection_visits_exactly_the_visible_set` counter-exactness
  test.
- `bash scripts/check-layers.sh` — exit 0, with the two new deny rules in force.
- `cargo fmt --check` — clean on every file this task touched (two pre-existing, unrelated
  formatting diffs remain in `tessera-authz/src/single_flight.rs` and
  `tessera-server/src/state.rs`, neither touched by this task; left as found).

## Self-review

- **D-D**: single pool, built at open, fail-closed on build failure, no second throttle anywhere
  in the crate — confirmed by grep (`rayon::ThreadPool` appears exactly once as a field, `install`
  is called exactly once in `viewport.rs`).
- **D-F collect shape**: `Vec<Result<Option<TileResult>>>`, not `Result<Vec<TileResult>>` —
  confirmed in the actual sweep code, not just the doc comment.
- **Ordering**: `tiles.par_iter().zip(ranges.into_par_iter())` preserves `tiles_for_bbox`'s raster
  order into the parallel collect and the serial fold walks it unchanged — the pre-existing
  `response_tile_order_and_point_concatenation_follow_tiles_for_bbox_not_morton_order` test still
  passes unmodified, which is direct evidence this held.
- **I11/lifecycle §1.1**: `generation` is loaded once at the top of `viewport`, and everything
  derived from it (`mask`, `segment`, `declared_scalars`) is borrowed into `tile_result`, never
  re-derived per tile or per worker — no `self.generation.load()` call anywhere inside
  `tile_result` or the closure.
- **I13**: panic propagation pinned by a direct rayon-level test; the server's pre-existing
  `JoinError` mapping (Task 3) is the other half of the chain and was not touched.
- **D-C**: the per-tile cancellation check moved into `tile_result`'s first line, unchanged in
  behaviour; all D-C tests (including the cross-thread mid-flight cancellation test, which now
  races the parallel sweep instead of the serial loop) still pass.
- **D-E**: verified the "zero when off" contract is preserved end-to-end, not just at the
  `TileStats` level — a dedicated unit test (`disabled_tile_probe_stays_zero`) plus running the
  full-request byte-equality test in a non-`bench-timing` build (where `ViewportOut::PartialEq`
  ignores timings anyway, so this is more a compile/panic check that the reduction path is sound
  than a numeric check).
- Re-read `viewport.rs`'s new module doc, the sweep's comment block, and `tile_result`'s doc
  against each other for consistency — no contradictions found.
- Confirmed `Engine: Send + Sync` still holds via the pre-existing `send_sync.rs` compile-check
  test (adding the `rayon::ThreadPool` field did not break this — `rayon::ThreadPool` is
  `Send + Sync` itself).

## Concerns

- **`TILE_PAR_MIN_LEN = 4` is argued, not measured.** The brief accepts this ("deliberately not
  calibrated by its own probe"), but it means the number could be meaningfully wrong in either
  direction for a real workload; a future perf task should treat it as a tuning knob worth a
  `tessera-bench` arm rather than assuming the argument in the comment is load-bearing.
- **The byte-equality tests exercise one fixture shape** (a moderate, roughly-uniform synthetic
  scatter at `N_ITEMS` in the thousands, zoom 3, `k = 50`, `underlay_offset = 2`). They deliberately
  hit both the serve-all and the heap/threshold selection branches and both empty and non-empty
  tiles, but they do not exercise extreme skew (one giant tile, hundreds of empty ones) or a
  request wide enough to approach `max_tiles_per_request`. I judged this sufficient for the
  headline claim (ordering is structural, not data-dependent) but a reviewer with more time might
  want a property-style test sweeping request shapes.
- **`EngineConfig::compute_threads` has no fail-closed refusal at `0`** the way the server's config
  loader does — by design (documented on the field: `rayon::ThreadPoolBuilder::num_threads(0)`
  falls back to rayon's own default rather than building a zero-width pool), but it does mean an
  embedder constructing `EngineConfig` directly with `0` gets **rayon's** default core count, not
  a refusal — a silent behaviour difference from the server's own `ComputeThreadsZero` startup
  refusal. This is deliberate (there is no fail-closed *startup* path to refuse through at this
  layer) but worth flagging as an asymmetry between the two config surfaces.
