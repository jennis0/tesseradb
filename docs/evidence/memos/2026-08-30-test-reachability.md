# Test reachability — which tests CI executes, and which merely exist

**Status:** Evidence — never normative. Every listing below was taken at `4a4c01df` on `main`;
`main` advanced to `2bde89a6` while the measurements ran, a documentation-only commit promoting
`projections.md`, which compiles no test and moves no count. Nothing about test *quality* is
assessed here: the single question is whether a test that exists is a test that runs.
`conformance.md` §4.6 is untouched — a row moves only when a test moves with it, and no test
moved.

## Results

- **2,296 test entries** are listed by the workspace's harnesses (2,283 distinct names; thirteen
  names are carried by two crates each). **22 are `#[ignore]`d**, so the per-PR gate's
  `cargo test --workspace` executes **2,261**.
- **Two tests exist under `--workspace` and vanish under `cargo test -p tessera-engine`**, and the
  resolver-2 hypothesis explaining them is **confirmed by measurement**. Both are in
  `crates/tessera-engine/tests/viewport.rs`, gated `#[cfg(feature = "bench-timing")]`.
- **One of the two is the only test of an Appendix C row's stated property** —
  `rows_in_ranges_is_mask_independent` pins the mask-independence that C4's numerator rests on.
  It does run in CI today, but only as a side effect of an unrelated crate's default feature. That
  is a reachability finding, not a defect: nothing is currently untested.
- **Ten of the 22 `#[ignore]`s carry no reason string.** Every one turns out to be a measurement
  harness rather than a silenced assertion — checked individually, not assumed — but nothing in the
  attribute says so, and the repository cannot tell "slow" from "broken" mechanically.
- **No invariant row of `conformance.md` §4.6 rests on an unreachable test.** Every cited piece of
  evidence is either in the Python suite (which CI runs) or in a Rust test the gate executes.
- **Three CI defects fixed** (below): the missing `--no-fail-fast`, the two client gates that ran
  nowhere, and a timing figure the workflow header presented as a standing budget. The figure
  itself is kept with its date and reframed — it is cited at four other sites, so withdrawing it
  here would have orphaned all four.
- **No live defect in shipped code was found.** The audit was reachability only.

> **Correction, same day, found by track R6.** The per-member sweep behind the second bullet was
> run for **`tessera-engine` only**, not for every workspace member. Nothing below says otherwise
> and every count is accurate as scoped — but the section reads as a settled workspace-wide
> result, and it was not one. R6 then found the same resolver-2 mechanism in `tessera-types`, at a
> far worse ratio: `cargo test -p tessera-types` lists **17 of its 39 tests**, the other 22 being
> `#[cfg(feature = "serde")]` in `layer.rs` and unified on only through `tessera-lifecycle`'s
> dependency. So the mechanism Wave 0 found was **not** the only instance, and the impression that
> it had been swept for was this memo's to avoid giving. The full sweep has since been run; its
> result is recorded in `docs/test-audit-campaign.md` §8.

## The executed-versus-existing diff

| Selection | Distinct tests listed |
|---|---|
| `cargo test --workspace` (the CI line) | 2,283 |
| — of those, `#[ignore]`d and therefore skipped | 22 |
| — therefore executed | 2,261 |
| `cargo test -p tessera-engine` | 803 |
| `cargo test -p tessera-engine -p tessera-bench` | 833 |
| `cargo test --workspace --exclude tessera-bench` | 2,253 |

Every test the engine's own selection lists is also listed by the workspace selection — the
inclusion holds in that direction with no exceptions. The reverse direction is where the two
disagree, and it disagrees by exactly two names.

Counts come from `cargo test … -- --list`, which asks each compiled harness what it would run. A
grep over `#[test]` cannot answer this question: it sees source, and every mechanism that removes a
test here operates at feature-resolution or harness-selection time. The workspace has no doctests —
every `Doc-tests` section of the listing reports `0 tests`.

## The resolver-2 finding: confirmed

`crates/tessera-engine/tests/viewport.rs` gates two tests on `#[cfg(feature = "bench-timing")]`:

- `f1_selection_visits_exactly_the_visible_set` (`crates/tessera-engine/tests/viewport.rs:2144`)
- `rows_in_ranges_is_mask_independent` (`crates/tessera-engine/tests/viewport.rs:2222`)

`tessera-engine`'s self dev-dependency — the mechanism its manifest documents as the only way an
integration test can turn on a feature of the library it links — enables `fault-injection` and not
`bench-timing`. So nothing inside the crate's own selection switches these on. What switches them on
is `tessera-bench`, whose manifest declares `default = ["bench-timing"]` and whose `bench-timing`
forwards to `tessera-engine/bench-timing`; under cargo's feature resolver the feature unifies onto
the normal `tessera-engine` edge for the whole `--workspace` build.

Four selections, measured, which isolate the cause to `tessera-bench` alone:

| Selection | Both tests listed? |
|---|---|
| `--workspace` | yes |
| `-p tessera-engine` | **no** |
| `-p tessera-engine -p tessera-bench` | yes |
| `--workspace --exclude tessera-bench` | **no** |

Adding `tessera-bench` to a selection that lacked them restores them; removing it from a selection
that had them takes them away. The hypothesis is **confirmed**.

This is the property `tessera-engine/Cargo.toml` and `tessera-lifecycle/Cargo.toml` already record
for `fault-injection` — that unification across a `--workspace` build happens and is measured rather
than assumed. What was not recorded is the consequence for tests: a developer running
`cargo test -p tessera-engine` on the crate they are editing runs a strictly smaller suite than CI
does, and nothing tells them so.

**Two further tests lose an assertion arm rather than vanishing**, which is the same mechanism in a
quieter form. `viewport_response_body_is_byte_identical_at_compute_threads_1_and_8` and its
`_with_sparse_empty_tiles` sibling (`crates/tessera-server/tests/http.rs:1535` and `:1672`) each
carry a `#[cfg(feature = "bench-timing")]` block that forces the parallel tile fan-out by setting
the serial-fallback threshold to zero (`crates/tessera-server/tests/http.rs:1571`, `:1708`); the
engine's own `tests/viewport.rs:2577` and `:2665` do the same. Under `-p` the tests still run, and
still pass, over the serial fold only. They are not in the table above because their names are
present in both listings — only their coverage differs.

## Unreachable tests crossed against the invariant matrix and the leak register

| Test | Where | Why it may not run | Invariant / C row it carries | Reachable in CI today? |
|---|---|---|---|---|
| `rows_in_ranges_is_mask_independent` | `crates/tessera-engine/tests/viewport.rs:2222` | `bench-timing`, on only through `tessera-bench`'s default feature | **C4's numerator.** `rows_in_ranges − sigma_visible` is the register's "rows scanned that this principal cannot see"; the row's whole framing needs `rows_in_ranges` to be a function of the viewport and not of the mask | yes, incidentally |
| `f1_selection_visits_exactly_the_visible_set` | `crates/tessera-engine/tests/viewport.rs:2144` | same | the *implemented route* for **I7** — direct evaluation reads every visible row in a tile. Its own doc is explicit that it does **not** own I7, which the selection differential and the Python oracle cover at output level in default builds | yes, incidentally |
| `latency_sanity_at_2_4m_p99_under_50ms` | `crates/tessera-engine/tests/viewport.rs:1284` | bare `#[ignore]` | none — a latency budget | no |
| `measure_the_column_cost` | `crates/tessera-engine/tests/membership_column.rs:586` | bare `#[ignore]` | none — a cost measurement, differenced across four runs | no |
| seven measurement harnesses | `crates/tessera-engine/tests/hull_geometry.rs:27`, `:218`, `:355`, `:539`, `:704`, `:766`, `:921` | bare `#[ignore]` | none — the campaign behind `artifact-shapes.md` §6, and they read a corpus that is not in the repository | no |
| `what_a_triangulation_costs_against_the_dig` | `crates/tessera-engine/tests/hull_triangulation.rs:30` | bare `#[ignore]` | none — ruling C's measurement, `artifact-shapes.md` §8 | no |
| twelve reasoned `#[ignore]`s | `scale.rs` ×5, `ingest_shape.rs` ×4, `build_equivalence.rs`, `residency.rs`, `lifecycle/membership.rs` | `#[ignore = "…"]`, each naming minutes-to-hours or a real corpus | none load-bearing; `lifecycle/membership.rs:2139` names the eight-membership variant that covers its path | no |

**No row of `conformance.md` §4.6 is left without evidence by any of this.** The rows recorded as
covered cite the Python conformance suite (I1, I2, I3, I7, I10, I12's mask half), `trybuild`
compile-fail fixtures (I4), or Rust property tests (I9, I13a) — all of which the gate executes. The
rows recorded as not covered (I5, I6, I8, I11, I13b) are uncovered for the reasons that document
gives, and none of them turns out to have a test hiding behind a feature gate.

`rows_in_ranges_is_mask_independent` is the one place a leak-register row's stated property has
exactly one test and that test sits behind a feature gate the crate under test does not itself
enable. The property is real and the bug it pins was real — the test's own doc records a 26× gap on
a 10⁹ corpus under two grants at the same shape — so what is worth the owner's attention is not that
it is failing to run, but that whether it runs is currently decided by a benchmark crate's default
feature. Removing `default = ["bench-timing"]` from `tessera-bench`, for any reason unrelated to
this test, would delete C4's only assertion from CI silently.

## Bare `#[ignore]`s

Ten of the 22, all in `tessera-engine`'s integration tests: `viewport.rs:1284`,
`membership_column.rs:586`, `hull_triangulation.rs:30`, and `hull_geometry.rs` at `:27`, `:218`,
`:355`, `:539`, `:704`, `:766` and `:921`.

Each was read rather than assumed, and each is a measurement harness: `hull_geometry.rs`'s module
doc says it is ignored because the corpus it reads is not in the repository and gives the command
to run it; `hull_triangulation.rs` is ruling C's timing comparison; `measure_the_column_cost`'s own
doc says "run with `--ignored --nocapture`". **None is a silenced assertion.** But that is a fact
established by reading ten module docs, and the attribute — the thing a reviewer sees on the line
above the failing test they are triaging — says nothing. The twelve reasoned ones show the shape
that works: `"minutes to hours, and wants a release build — see the module doc"` needs no
investigation at all.

## What changed in CI

Three edits to `.github/workflows/ci.yml`, no test touched.

1. **`cargo test --workspace` becomes `cargo test --workspace --no-fail-fast`.** Without it cargo
   stops at the first failing test binary and never runs the rest, so the summary pairs the
   failures it reached with a passing total that is quietly *smaller* than the previous run's —
   which reads as success at a glance. `CLAUDE.md` records that this has already been mistaken for
   a green gate here, and the workflow was the one place still missing the flag.

2. **A `clients` job runs `scripts/check-clients.sh` and `clients/py/check.sh`.** Both are in
   `CLAUDE.md`'s gate and neither ran automatically, so the TypeScript client's suites and the
   Python package's tests were gated only by whoever remembered to run them locally. The job sets up
   Node 20 (both scripts need npm — the Python one because its wheel's hatchling hook shells out to
   it) and Python 3.12, then runs `npm --prefix clients/ts ci` before the checks, because
   `check-clients.sh` deliberately refuses an absent `node_modules` rather than installing one.
   `check-clients.sh` exists because one rename shipped three defects into `clients/` that a
   Rust-and-Python gate could not see; running it nowhere is that gate again.

3. **The header's timing paragraph is reframed, and the figures are kept.** It cited
   `cargo test --workspace` at 94 s and `pytest conformance/tests` at 24 s, measured on a developer
   machine on 2026-08-01, and used that as the standing reason the ignored tests are excluded.

   Withdrawing the numbers was the first correction written here and it was the wrong one: the
   94 s is cited at **four other sites** — `conformance.md` §6, `correctness-suite.md` §16 twice,
   and `crates/tessera-engine/tests/scale.rs`'s module doc — and two of those cite it as the budget
   an ignored test is excluded against. Deleting it in one place would have left four live
   citations pointing at a figure the workflow no longer carried, which is a worse defect than the
   one being fixed.

   A dated measurement does not go stale; treating it as current headroom is what was wrong. So the
   figures stand with their date, and the header now states plainly that they are **measurements
   with a date, not a current budget**, that the workspace has grown to 2,283 distinct tests since,
   that neither has been re-measured and neither may be quoted as headroom, and that the exclusion
   of the ignored set rests on what those 22 tests *do* — real corpora, minutes to hours — which is
   durable and independent of any figure. No new number is invented.

   The four citing sites are left alone: they remain correct against a figure that is still in the
   workflow and still carries its date. Re-measuring both, and then revising all five together, is
   the follow-up this memo recommends and does not perform.

## One observation outside this memo's scope

`crates/tessera-engine/tests/fold.rs` failed once during the verification runs above and did not
reproduce: the package run reported `error: 1 target failed: --test fold`, `--test fold` alone then
passed 28 of 28, and a full re-run of the package passed with exit 0. `fold.rs` contains no
`bench-timing` gate, so it is not a consequence of the dev-edge change made alongside this work;
the likeliest reading is contention on a loaded machine.

It is recorded because a test that is red under load and green alone cannot be trusted to mean
anything **when it is red**, which is a test-quality question rather than a reachability one. It
belongs to whichever pass examines the write path, and it is written down here rather than left in
a transcript so that pass does not have to rediscover it. Not investigated; not reproduced
deliberately; frequency unknown.

## `scripts/check-test-reachability.sh` — adopted for the gate, in its `--quick` form

The measurement above is re-runnable as `scripts/check-test-reachability.sh`. It prints the
existing-versus-executed counts, lists every bare `#[ignore]` and exits non-zero on them, and
sweeps every workspace member for tests reachable only under `--workspace`. `--quick` keeps the
first two steps and skips the sweep; naming crates as arguments sweeps only those, which is how
the two-name result above reproduces in one pass (`bash scripts/check-test-reachability.sh
tessera-engine`).

**`--quick` is in the gate**, by owner ruling on 2026-08-30 — in `CLAUDE.md`'s gate block, in
`docs/agents/parallel-work.md`'s, and in the `rust` job of `.github/workflows/ci.yml`, where it
sits after `cargo test` so it reuses that build rather than compiling the workspace again.

A mechanical refusal for a bare `#[ignore]` is a decision about what the repository blocks a build
on, which is why it was the owner's to make rather than this memo's. The disclosure surface is not
involved. What distinguishes it from `CLAUDE.md`'s "report loudly, let the operator decide, and do
not block a build" is who is being refused: that rule governs operator-facing behaviour over real
data, where a refusal moves cost onto a caller who often cannot act on it, whereas this is a
developer-facing lint over the repository's own source, satisfiable in the same edit that trips
it — the same class as `clippy -D warnings`, which this repository already blocks on.

It went green with ten reason strings, one per bare `#[ignore]`, each taken from the reason the
test's own doc comment already gave. All ten are measurement harnesses rather than silenced
assertions:

| Test | Reason recorded |
|---|---|
| `hull_geometry.rs` × 7, `hull_triangulation.rs` × 1 | needs a built bundle in `TESSERA_HULL_BUNDLE`, which is not in the repository |
| `viewport.rs` `latency_sanity_at_2_4m_p99_under_50ms` | builds `/tmp/tessera-2m4` from the real corpus and times it — release only |
| `membership_column.rs` `measure_the_column_cost` | the column's per-response cost by differencing — run with `--ignored --nocapture` |

**The full sweep stays out of the gate.** It re-resolves features once per member, so it
recompiles the world several times over — a cost the per-PR gate should not carry. Run it when a
feature gate or a dev-dependency changes.
