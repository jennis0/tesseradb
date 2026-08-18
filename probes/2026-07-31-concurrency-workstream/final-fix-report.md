# Final-review fix wave — report

Three small findings from the final review, fixed on branch `concurrency/viewpath` at
7e5b553 → this commit.

## Finding 1 — doc drift: serial-fallback caveat missing from bench/README.md §8 and viewer.rs's stage-header doc

**What changed.** Added one clarifying paragraph in each place, stating that the "cross-worker
sum, not a wall-clock partition" claim for per-tile stage fields only holds **above**
`SERIAL_FALLBACK_MAX_ROWS` (200,000 rows-in-ranges). Below that line `Engine::viewport` folds the
tile sweep serially regardless of `compute_threads` — confirmed by reading `should_fold_serially`
and its call site in `crates/tessera-engine/src/viewport.rs:655-724`, which predicate on
`total_rows_in_ranges` alone, never on `compute_threads` — so for those requests the per-tile
fields still partition the request's own wall clock, at any thread count.

- `bench/README.md` §8, after the existing cross-worker-sum paragraph.
- `crates/tessera-server/src/viewer.rs:407-417` (the `stage_header` doc comment), appended after
  the D-D/D-E paragraph it echoes.

No code changed for this finding, doc-only.

## Finding 2 — `/v1/items` double generation load

**What changed.** Moved the epoch validation from the handler (`viewer.rs`'s `item` function,
which called `state.engine.meta()` — its own `generation.load_full()` plus a clone of every
declared scalar and view name, just to read one field) into `Engine::item` itself
(`crates/tessera-engine/src/viewport.rs`), checked against the one `generation.load_full()` that
method already performs for the entity lookup that follows. This was the "preferred" option in
the brief and I took it rather than arguing for a different shape, since the response bytes/status
can be preserved exactly (verified — see tests below).

- Added `EngineError::StaleIdentityEpoch` (`crates/tessera-engine/src/session.rs`), a unit variant
  with `Display` `"stale identity epoch"`.
- `Engine::item`'s signature grew a third parameter, `epoch: Option<u32>`, and its return type
  changed from `Result<Option<ItemOut>, StoreError>` to the crate's `Result<Option<ItemOut>>`
  (`= Result<_, EngineError>`) — the epoch check runs first, before entity inversion, so it stays
  entity-independent and pre-inversion exactly as before (C4 timing-channel argument unchanged).
  The pre-existing `?` on `external_id_of` became `.map_err(EngineError::Store)?` since that call
  still returns `StoreError`.
- `map_engine_error` (`crates/tessera-server/src/error.rs`) gained an explicit arm:
  `EngineError::StaleIdentityEpoch => ApiError::Conflict("stale identity epoch; re-resolve by
  external_id")` — the exact string the handler used to construct directly, so the 409 body is
  byte-identical.
- `run_item`/`item` in `viewer.rs`: dropped the `state.engine.meta()` pre-check block entirely,
  pass `req.epoch` through to `state.engine.item(...)`, and map its `Err` via `map_engine_error`
  instead of `map_store_error` (still routes `Store`/`Io` through `map_store_error` internally, so
  that 500 body is unchanged too). `map_store_error` became unused in `viewer.rs` and was dropped
  from the import.
- Updated 7 call sites in `crates/tessera-engine/tests/viewport.rs` for the new 3-arg signature,
  and fixed `a_sidecar_error_on_drill_down_is_an_error_not_a_missing_external_id`'s assertion from
  `matches!(err, StoreError::InvalidSidecar { .. })` to
  `matches!(err, EngineError::Store(StoreError::InvalidSidecar { .. }))`.
- Added a new engine-level test, `item_epoch_check_is_entity_independent_and_decided_before_
  inversion`, mirroring the server's `item_with_a_stale_epoch_is_409_...` test at the layer this
  fix moved the check into: a matching epoch is a no-op, a mismatched epoch is
  `Err(StaleIdentityEpoch)` identically for a real visible id and one naming nothing.

**Trade-off, argued rather than hidden.** The epoch check used to run on the reactor thread,
before the compute-admission gate (`admit()`), so a stale-epoch request never consumed a gate
permit or a `spawn_blocking` slot. It now runs inside `Engine::item`, which executes after
`admit()` inside `spawn_blocking` — a stale-epoch request now briefly holds a gate permit for the
length of that call. No existing test asserts on this ordering (I checked — none of
`crates/tessera-server/tests/http.rs`'s epoch/gate tests couple the two), and the brief's own
"preferred" fix explicitly asks for the check to move inside `Engine::item`, so I took the trade
lifecycle §1.1's one-generation-per-request invariant asks for rather than working around it with,
e.g., a second cheap `Engine::identity_epoch()` accessor that would still cost a second
`load_full()` and not actually close the gap. Documented at both the doc-comment and the
call-site comment in `viewer.rs`.

## Finding 3 — WAL error `Display` in 500 bodies

**What changed.** Added `map_wal_error<E: std::fmt::Display>(e: E) -> ApiError` to
`crates/tessera-server/src/error.rs`, mirroring `map_store_error`'s shape exactly: logs the full
error at `error!` (`detail = %e`), returns a fixed body string ("a durability write failed; the
request was refused rather than answered partially"). Used at both call sites named in the
finding:

- `crates/tessera-server/src/control.rs:367` (`run_ingest`'s WAL failure arm) — the existing
  `tracing::error!("wal append/fsync failed for an ingest batch")` line is kept for batch context
  (it never referenced `e`), and `map_wal_error(e)` now also logs `e`'s `Display` — which, before
  this fix, wasn't logged anywhere at all, only leaked into the body.
- `crates/tessera-server/src/control.rs:530` (`run_changes`'s WAL failure arm) — same pattern; the
  deny-op ALARM log and the non-deny log are both kept for their op-specific context, with
  `map_wal_error(e)` supplying the detail log neither previously had.

Added the mirror unit test in `error.rs`, `map_wal_error_does_not_forward_the_detail_to_the_
caller`, same shape as `map_store_error_does_not_forward_the_detail_to_the_caller`: feeds a
leaky string containing a filesystem path, asserts the 500 body doesn't contain `/`.

No test asserted the exact previous body text (`"wal append/fsync failed: {e}"`) at either site,
so this changes no test-visible behaviour beyond the body's fixed string.

## Self-review

- Re-read every changed doc comment against the design docs it cites (I2/I7/I10/I13, C4, C5,
  N-3, D-A/D-B/D-G, lifecycle §1.1) — none of the invariant arguments changed meaning, only the
  call-stack location of the epoch check.
- Confirmed `map_engine_error`'s `Store`/`Io` arm still routes through `map_store_error`
  internally, so Finding 2's error-body text for a corrupt sidecar is unchanged (this path is
  unreachable in Phase 1 per the pre-existing doc comment, and has no HTTP-level test either way).
- Confirmed no other crate calls `Engine::item` with the old 2-arg signature
  (`grep -rn "\.item("` across `crates/`), so the signature change is exhaustively updated.
- Ran `cargo fmt --check` after the fix and cleaned the two formatting diffs it introduced in
  touched files (`crates/tessera-engine/tests/viewport.rs`, `crates/tessera-server/src/error.rs`);
  left the one pre-existing, unrelated diff in `crates/tessera-server/src/state.rs` alone (not a
  file this wave touched).

## Verify

- `cargo test -p tessera-server --lib error::` — 10/10 pass, including both new tests
  (`map_engine_error_takes_stale_identity_epoch_to_409_conflict`,
  `map_wal_error_does_not_forward_the_detail_to_the_caller`).
- `cargo test -p tessera-server --test http item` — 2/2 pass
  (`item_with_a_stale_epoch_is_409_and_a_matching_epoch_changes_nothing`,
  `i_item_404s_identically_for_unknown_and_invisible`).
- `cargo test -p tessera-server --test http ingest` and `... changes` — 10/10 and 4/4 pass
  (control-plane WAL paths).
- `cargo test -p tessera-engine --test viewport item` (plus the two item tests whose names don't
  match `item`, run explicitly) — all pass, including the new
  `item_epoch_check_is_entity_independent_and_decided_before_inversion` and the retargeted
  `a_sidecar_error_on_drill_down_is_an_error_not_a_missing_external_id`.
- `cargo test --workspace` — full pass, twice (run once mid-fix, once after the fmt cleanup);
  `write_permutation_rejects_entity_id_not_fitting_u32` (the known environmental OOM-SIGKILL
  flake) did not trip either run.
- `bash scripts/check-layers.sh` — exit 0, no output.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean, no warnings.
