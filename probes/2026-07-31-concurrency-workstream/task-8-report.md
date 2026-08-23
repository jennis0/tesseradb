# Task 8 report — Hot-path hygiene: drop the viewport request's second generation load

## Summary

`Engine::viewport` (`crates/tessera-engine/src/viewport.rs`) already resolves
`declared_scalars` from the one generation it loads at request start (lifecycle §1.1: "a request
thread loads the generation pointer exactly once, at request start"). The viewer handler
(`crates/tessera-server/src/viewer.rs::run_viewport`) was calling `state.engine.meta()`
*afterwards*, purely to read the declared-scalar names for response assembly — a second
`generation.load_full()` plus wholesale clones of `views` and `declared_scalars`, both of which
were then thrown away except for the names.

Fixed by having `Engine::viewport` return the declared-scalar names in `ViewportOut`, sourced from
the same `declared_scalars` view `row_to_point` already reads for every point in the response. The
handler now reads `out.scalar_names` and makes no second engine call. No `declared_scalar_names()`
accessor was added — the brief is explicit that such an accessor would just move the second load
behind a different name rather than remove it.

## Implementation

### `crates/tessera-engine/src/viewport.rs`

- Added `pub scalar_names: Vec<String>` to `ViewportOut`, with a doc comment stating why it's
  there (avoids the redundant `meta()` load) and that it comes from the same generation as
  `points`.
- At the `Ok(ViewportOut { .. })` construction site (end of `Engine::viewport`), populated it from
  the `declared_scalars` local already bound earlier in the function
  (`&generation.bundle.manifest.declared_scalars`, the same view `tile_result`/`row_to_point`
  read for every gathered point): `declared_scalars.iter().map(|d| d.name.clone()).collect()`.
- No second `generation.load_full()` anywhere in this path — `declared_scalars` is derived from
  the single generation snapshot taken at the top of `Engine::viewport`, unchanged from before this
  task.

### `crates/tessera-server/src/viewer.rs`

- Removed the `let meta = state.engine.meta(); let scalar_names = meta.declared_scalars...`
  block inside `run_viewport`. `build_scalar_columns(&out.points, &out.scalar_names)` now reads the
  names straight off the engine's own return value.
- Left the module's two other `state.engine.meta()` call sites untouched, both legitimate:
  - `meta()` handler (`GET /v1/meta`, line ~115) — this **is** the meta endpoint; it has no
    generation to reuse from anywhere else.
  - `item()` handler's epoch check (line ~625) — an O(1) in-memory comparison already running on
    the reactor (not inside `spawn_blocking`), unrelated to the viewport hot path this task
    targets, and out of scope per the brief.

## `PartialEq` decision

`scalar_names` **joins** the comparison (added to the hand-written `impl PartialEq for
ViewportOut`, alongside `pin`/`tiles`/`points`/`sub_cells`; only `timings` stays excluded).

Why: `scalar_names` is drawn from the same manifest as the values inside `points`, in the same
generation. Two `ViewportOut`s that already agree on `points` necessarily agree on
`scalar_names` too (same fixture ⇒ same declared-scalar schema), so including it in the equality
check is harmless in every existing test, including
`viewport_output_is_byte_identical_at_compute_threads_1_and_8`
(`crates/tessera-engine/tests/viewport.rs`), which asserts a whole-struct `assert_eq!`. Leaving it
out would have been an unmotivated silent exemption — `timings` is excluded for a stated reason
(wall-clock is not part of response identity); `scalar_names` has no analogous reason to be
exempt, so it stays in. Updated the type's doc comment to say so explicitly rather than leaving the
reader to infer it from the diff.

## Files changed

- `crates/tessera-engine/src/viewport.rs` — `ViewportOut.scalar_names` field, `PartialEq` impl,
  construction site.
- `crates/tessera-server/src/viewer.rs` — `run_viewport` reads `out.scalar_names` instead of
  calling `state.engine.meta()`.

Confirmed via `grep -rn "ViewportOut {"` that `Engine::viewport`'s own construction is the only
literal `ViewportOut` build site in the workspace (tests only read fields via `engine.viewport(..)`
calls, never construct the struct directly), so no other call site needed updating.

## Test evidence

- `cargo build --workspace` — clean, no warnings.
- `cargo test --workspace` — all green. Full log tail confirms every crate's `test result: ok`,
  `0 failed` throughout. `tessera-store`'s
  `write_permutation_rejects_entity_id_not_fitting_u32` ran (60s, the known slow/OOM-prone
  allocation test) and passed this run — no SIGKILL observed, noted as pre-existing flake per the
  task instructions, not something this change touches.
- `cargo test -p tessera-engine --features bench-timing` — all green, including the 37-test
  `tests/viewport.rs` suite (36 passed, 1 ignored perf-sanity test unrelated to this change) and
  specifically `viewport_output_is_byte_identical_at_compute_threads_1_and_8`, which does a
  whole-struct `assert_eq!(out_1, out_8)` and now covers `scalar_names` too.
- `cargo test -p tessera-server --test http` — all 34 tests green, including
  `viewport_response_body_is_byte_identical_at_compute_threads_1_and_8` (the wire-byte assertion
  the brief calls out) and `viewer_meta_reports_the_identity_epoch_and_never_the_key` (confirms
  `/v1/meta` is unaffected).
- `bash scripts/check-layers.sh` — exit 0, no output (clean).
- `cargo check --workspace --all-targets` — no warnings.

### TDD note

No new RED test was written. A test asserting the response's scalar column set/order already
exists as the byte-identity assertions in both `crates/tessera-engine/tests/viewport.rs`
(`viewport_output_is_byte_identical_at_compute_threads_1_and_8`) and
`crates/tessera-server/tests/http.rs` (`viewport_response_body_is_byte_identical_at_compute_threads_1_and_8`).
Writing a fresh RED for "names come from one generation load instead of two" would need to assert
an absence (no second `load_full()`), which isn't observable from the response bytes — the whole
point of the fix is that it's byte-invisible. Behaviour-preservation is evidenced by the untouched
byte-assertion suite staying green unmodified, per the brief's own acknowledgement that "a RED here
may be impractical."

## Self-review

- Checked the diff is minimal: 28 insertions / 11 deletions across the two files named in the
  brief, nothing else touched.
- Confirmed `declared_scalars` (the local already bound in `Engine::viewport`, line ~527 pre-diff)
  is exactly the same view `row_to_point` reads per-point — so `scalar_names`' order and content
  are identical to what `meta()` would have produced (manifest order, both routes), satisfying the
  brief's "names come from the same manifest in the same order either way" constraint.
- Confirmed no other `ViewportOut` construction site exists (grep), so the `bench-timing`-gated
  construction the task brief warned about is actually the same single site — `bench-timing` only
  gates `StageTimings` population inside `probe`/`stats`, not a separate `ViewportOut` literal.
- Confirmed the two remaining `meta()` call sites in `viewer.rs` are both outside the viewport hot
  path (one IS the meta endpoint; the other is a cheap reactor-side check on `/v1/items`), matching
  the brief's "only remove the VIEWPORT hot-path call" instruction.
- British spelling maintained in new comments (e.g. "authorisation" pattern followed elsewhere in
  the file was not needed here, but no American spellings were introduced).
- Re-ran the full workspace test suite, the bench-timing engine suite, and the server http suite
  after the change — all green, matching pre-change behaviour exactly (byte-identity assertions
  are the strongest evidence available here).

## Concerns

None. This was a small, contained change with strong existing test coverage (both an engine-level
and a server-level byte-identity assertion already existed and both stayed green unmodified). The
only judgement call was the `PartialEq` inclusion decision, argued above.
