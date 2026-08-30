# Test audit R3 — `tessera-engine`, the write path

**Status:** Evidence — never normative. Base commit `2bde89a6` on `main`. This assesses **tests,
not the code under them**: nothing below is a claim that shipped behaviour is wrong, and no defect
in shipped code was found. Track R3 of a three-track test-quality campaign; Wave 0's
[reachability memo](2026-08-30-test-reachability.md) is the input for which tests run.
`conformance.md` §4.6 is untouched — a row moves only when a test moves with it, and no test moved.

## Results

**Rule S / Rule F is covered, and the coverage discriminates.** The canonical fail-open —
`delete → suppress → unsuppress` leaving the entity visible — has a test that goes red, and there
is a second, independent one for the other direction (a fold retiring a suppression). Both were
checked by reading and one was checked by **mutation**:

- `crates/tessera-lifecycle/src/overlay.rs:418`
  `retirement_takes_deletions_and_leaves_every_suppression_standing` puts an entity that is *both*
  deleted and suppressed into a retirement set and asserts the suppression survives it. It is the
  rule in its most direct form, at the only function that can withdraw an overlay entry, and there
  is no sibling of `Overlay::retire` for `suppressed`.
- `crates/tessera-engine/tests/fold.rs:1113`
  `a_suppression_survives_the_fold_and_an_unsuppress_afterwards_reveals_its_item` covers the fold
  end of it. **Mutation run:** `FoldPlan`'s `D₀` was changed from `overlay.deleted_entities()` to
  `overlay.denied()` — the union, which is exactly the conflation write-path §5.4 forbids — in a
  throwaway worktree. The test went **red**, at `fold.rs:1141`, for the right reason: the
  suppressed item lost its row at the fold, so the later unsuppress revealed nothing
  (`left: 9999, right: 10000`). Twenty-seven of the binary's twenty-eight tests still passed, so
  the discrimination is specific rather than incidental. Worth noting for a future reviewer: the
  `overlay_depth() == 1` assertion in the same test survives that mutation — the assertion that
  catches it is the visibility one, exactly as the test's own "mutations this kills" note says.
- The read-path end (`tests/deny_mask.rs:161`, `tests/compose.rs:295`/`:311`,
  `tests/artifact_containment.rs:327`) sits in other tracks' surfaces and is present; it is
  recorded here only so the owner can see the rule is covered from both sides.

**The surface as a whole is healthy.** 206 tests were assessed — 140 integration across the twenty
named files, 66 `#[test]` blocks in `src/{write,flush,merge,compact,coalesce,refresh,cancel}.rs`.
Nine are `#[ignore]`d (`scale.rs` ×5, `ingest_shape.rs` ×4), all measurement harnesses with reason
strings, and **no correctness claim exists only inside an ignored test**: the one that looked like a
candidate — `scale.rs`'s multi-segment fold — asserts a corpus-survives guard on a memory
measurement, and multi-segment folds are covered unignored at `tests/fold.rs:2410` and `:2501`.

The convention that makes this subsystem auditable is worth recording: most tests carry a
**"mutations this kills"** paragraph naming the specific defect the body catches, and at least one
(`tests/merge.rs:390`) documents what it does *not* cover so nobody reads it as covering that.
Reading a test against its design section was decisive almost everywhere; one mutation was run and
it confirmed the doc.

Three findings, none of them a disclosure surface with no test.

## Findings

### F1 — the deny-publication liveness floor has no test at all

**Claim:** write-path §5.6's "a drain that never closes must still publish" — a publication every
64 windows under sustained deny arrival — is enforced by one branch that nothing exercises.

**Evidence:** `crates/tessera-engine/src/write.rs:3381`
`const OVERLAY_PUBLICATION_MAX_WINDOWS: u64 = 64;` and `crates/tessera-engine/src/write.rs:7427`
`if self.windows_since_publication >= OVERLAY_PUBLICATION_MAX_WINDOWS { self.publish_overlay_state(); }`.
The constant appears **only** in `crates/tessera-engine/src/write.rs` — three sites, all definitions or its own doc.
No test in the workspace drives more than 64 consecutive deny windows without a drain close.

The branch is load-bearing rather than belt-and-braces: the executor's other publication site
(`crates/tessera-engine/src/write.rs:4745`) sits *after* `while self.run_deny_pass() {}`, so under the sustained arrival
the floor was written for, the drain loop does not exit and that site is not reached. The floor is
then the only publication route in that state.

**Class:** missing. **Severity: S3.** No disclosure and nothing irreversible — the dispositions are
in force and WAL-durable whether or not a manifest carries them; what degrades is the restore path
and the log's ability to shed members.

**What a defect looks like:** an off-by-one or an inverted comparison here, or a refactor that
resets `windows_since_publication` in the wrong place, leaves a node under sustained revocation with
deny state that never reaches a side-manifest, for as long as the arrival lasts. Every other test in
the file publishes at drain close and so cannot see it.

**Confidence:** high that no test exists (grep over the whole workspace for the constant, and over
the test files for a >64-window deny drive). Mutation not used — absence needs no mutation.

**Disposition:**

### F2 — three `fold.rs` cases share one deadline across sequential waits, which the file's own helper doc forbids

**Claim:** `wait_for`'s doc states the rule and three inline loops in the same file break it, so a
red in those cases names the wrong step and can be caused by the *earlier* wait's elapsed time.

**Evidence:** `crates/tessera-engine/tests/fold.rs:187`

> Each call gets its own deadline — a single deadline shared across a test's several waits expires
> in whichever one happens to be last, which reports the wrong step.

against `fold.rs:1266` (one 60 s deadline read at `:1269`, `:1279` and `:1306`), `fold.rs:1410`
(read at `:1413` and `:1443`) and `fold.rs:1571` (read at `:1576` and `:1592`). All three are the
paused-fold cases, which are the file's longest.

**Class:** under-discriminating (as a failure report, not as a pass). **Severity: S4** — the tests
still catch the defects they name; what is unreliable is the message when they do not.

**What a defect looks like:** the fold reaches its hold slowly under load, and the assertion that
fires is "the fold never published" at `:1306` rather than "the fold never reached its hold" at
`:1269` — a reviewer triaging that message investigates publication and finds nothing wrong with it.

**Confidence:** high, by reading. Mutation not used.

**Disposition:**

### F3 — the merge/deny race is discriminating on only one of two orderings, by its own account

**Claim:** `a_suppression_racing_a_merge_is_in_force_once_both_have_landed` catches a lost deny
probabilistically rather than deterministically.

**Evidence:** `crates/tessera-engine/tests/merge.rs:487`

> A publication that re-derived from a *captured* overlay rather than the live one loses the deny on
> exactly one of the two orderings, so this fails intermittently rather than never.

**Class:** under-discriminating. **Severity: S3** — a lost deny is a disclosure, so the defect class
is S1-shaped; what holds the severity down is that the test does fail, on some runs, and the
limitation is stated at the test rather than hidden.

**What a defect looks like:** a publication captures the overlay before re-deriving; on the orderings
where the deny lands after the capture it is silently dropped, and the suite is green on the runs
that took the other ordering.

**Confidence:** high — this is the test's own statement, verified against the body. It is recorded
because the module doc says the deterministic form (a pause site between a merge's execution and its
publication) is out of reach, and that reasoning predates
`tests/merge.rs:1026`'s
`the_merge_publication_seam_parks_the_executor_between_execution_and_publication`, which is that
site. Whether the site can now carry this case is a question for the owner, not a defect.

**Disposition:**

## The `fold.rs` flake handed to this track

Wave 0 recorded `--test fold` failing once inside a package run (`error: 1 target failed`), then
passing 28/28 alone and passing a full re-run. **Reading does not explain it, and this is reported
as unexplained rather than resolved.**

What reading rules out: no fixed path, no port, no environment variable, no `/tmp` collision, no
process-global state and no `Once`/`static` in `tests/common`. Every case builds its own
`TempDir` fixture. There is no `bench-timing` gate in the file, so Wave 0's dev-edge change is not
implicated.

What reading leaves standing is a wall-clock dependency. Every wait in the file is an
`Instant::now() + Duration` deadline of 30 s or 60 s (twenty declarations, `fold.rs:169` through
`:2599`), and F2's three shared deadlines are the least headroom in the file. Measured against them:
the whole binary ran in **5.86 s** for 28 tests during this audit's mutation run on an otherwise
busy machine, so the per-wait margin is large — large enough that a plain timeout under load is
possible but not obviously the cause, and the reported form (a target failure with no named test)
is more consistent with a harness-level abort than with a deadline assertion, which would have
named the case.

**Not reproduced. Frequency unknown. Deliberately not hammered** — two other audit tracks were
running concurrently. The honest statement is that a shared-deadline case timing out under load is
the only mechanism the file offers, F2 makes such a timeout report the wrong step, and neither fact
amounts to an explanation.

## What was checked and found sound

Kept so the attacks are not re-run.

- **Rule S has no second retirement route in code reachable from the write path.**
  `Overlay::retire` (`crates/tessera-lifecycle/src/overlay.rs:194`) touches `deleted` only; its
  three engine call sites (`crates/tessera-engine/src/write.rs:5953`, `:11288`, and `retire_artifacts` at `:1560`, which
  is the layer registry and not the overlay) were read. No sibling exists.
- **The mutation that would conflate the two rules is caught** — see Results. `D₀ = deleted ∪
  suppressed` turns `fold.rs:1113` red.
- **Rule F's retirement is derived from what was demonstrably removed, not from the plan**, and
  that is tested at both levels: `src/compact.rs:1674`–`:1779` (five unit cases on `executed`,
  including `executed ⊆ D₀`) and `tests/fold.rs:1381`–`:1601` end to end, including a mid-flight
  delete that is not in `D₀` and must not retire.
- **I9 across a fold** is covered without relying on the weak assertion that names it.
  `tests/fold.rs:517`'s `assert_ne!(reborn, entity, "(I9)")` is on its own only a difference check,
  but the durable claim is asserted properly next door: `tests/fold.rs:1703` pins that
  `SEGMENTS-<n>.json` carries the **live** `entity_id_high_water` and not the fold snapshot's, and
  `tests/flush_visibility.rs:232` pins that the allocator's floor comes from that field. A fold
  cannot regress the allocator without one of those two going red.
- **The asymmetric durability fold** (write-path §5.5) is covered per op, not in aggregate:
  `tests/write.rs:642` (a deny applies anyway), `:748` (hidden now, visible after restart), `:791`
  (an ingest applies nothing), `:882` (an unsuppress applies nothing), `:943` (a torn WAL stays
  poisoned and still applies denies). The `ReceiptLost` / `ExecutorDead` split — 500 rather than
  503 for a durable, in-force change — is pinned at its producer (`tests/write.rs:1117`) after the
  file's own doc records that reverting it once compiled and passed the workspace.
- **The publication seams pause and are shown to pause**, with a settle-then-assert-not-yet step so
  a `Stall` that failed to block fails reliably rather than passing racily
  (`tests/seam_pause.rs:118`, `:171`; the merge seam at `tests/merge.rs:1026`).
- **The manifest deny-state reader** carries a negative control (`tests/manifest_deny_state.rs:112`)
  and the seed-versus-replay ordering case (`:155`), which is the one that is invisible to every
  other test in the file because those carry no WAL.
- **The `deny` and `tombstones` fields are written separately, never from the union**
  (`crates/tessera-lifecycle/src/overlay.rs:104`'s doc states the rule; `tests/overlay_publication.rs:173`
  asserts it, in another track's surface).
- **Compaction §9's four gauges and its window** have twenty-three unit cases in `src/compact.rs`
  plus five end-to-end dispatch cases in `tests/fold.rs:2171`–`:2599`, including the shut-window
  negative.
- **Nothing weak was found in the `is_err()` sweep.** Five bare `is_err()`/`is_ok()` sites exist
  across the twenty files; each sits beside a kind-checked assertion for the same boundary or is
  followed by an assertion on the effect (`tests/write.rs:168`, `:958`, `:1190`;
  `tests/coalesce_text.rs:467`, whose sibling at `:481` matches on the error text). None is a case
  where the error *kind* carries the meaning and is unchecked.
- **No test in this surface was found that cannot fail.** The zero-assert bodies are all helper
  delegations (`fold`, `fold_discarded`, `wait_for`, `wait_until`, `visible`), and each helper
  asserts.

## Appendix R — review trail

- **r1 (2026-08-30)** — first pass. Base `2bde89a6`. Method: read each test against
  `write-path.md` §§2, 4, 5.4–5.6, 7, 8, `compaction.md` §§4, 5, 9, and
  `concurrency-lifecycle.md`'s recovery half; one mutation, in a throwaway git worktree, removed on
  completion, nothing committed. Nothing was escalated mid-audit: no route by which a suppression
  could retire other than on unsuppress was found, and no design section in scope read as ambiguous.
