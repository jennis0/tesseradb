"""Crash atomicity at the publication seams — correctness-suite §10.1 over §12.3's driver.

**A stage killed part-way through must leave the system at one of its two endpoints and never
between them.** That is stage invariance with its right-hand side disjoined — ``after ≡ before``
or ``after ≡ before + entitlement``, exactly — so a crash here is a *modifier* on a stage, not a
mechanism of its own: the plans below are ordinary plans, the recordings are the battery's, the
comparison is `entitlement.diff`'s, and the only new moving part is [`Killed`], which parks the
write executor at a named seam in the faults build (decision 0071) and SIGKILLs the server there.
An arbitrary kill essentially never lands on a seam; an armed pause site always does, which is
the whole reason the sites exist.

Three seams are driven, each at the instant where bytes exist on disc and nothing durable names
them:

- **the fold's `CURRENT` flip** — built first because its discard needs no bookkeeping: the
  commit point is a single rename, so the whole unflipped prefix is discardable until it happens;
- **the side-manifest publish** — driven through a flush, though the site fires for every
  executor-side side-manifest commit (merge, coalesce and the deny-overlay write share the same
  crash story), so one seam test stands for the class;
- **the merge's execution-to-publication gap** — the merged segment exists, its inputs stand,
  and the executor has committed to nothing.

## The two limits, stated so nothing below reads as more than it is

**A SIGKILL is not a power cut.** The page cache survives process death, so every byte the killed
process wrote is readable by the next one whether or not anything fsynced it — an engine that
acked before it synced passes every kill-and-restart test, which is measured rather than argued
(§10.1). The `discard_unsynced` step is what makes these power-loss simulations: it applies
§12.3's per-seam rule for what a power cut would have taken — the whole unflipped prefix at the
flip; the appeared, manifest-unreferenced files at the other two seams — before the restart that
records the after-state.

**fsync ordering is not observable through the API**, so nothing in this module — or in any
black-box crash test — can falsify an engine that acks before it syncs. The discard deletes what
was *not published*; it cannot tell what was *not durable*, because the API offers no view of the
sync boundary. That property is held in Rust, by the `Published` token type and the pause site
inside the ack function, and issue #71 tracks whether an end-to-end check is worth its cost. What
this module establishes is strictly: a kill at a publication seam, with the unpublished residue
taken away as a power cut would take it, recovers to an endpoint of the stage and never to a
state between them.

## The negative control

A crash suite that has never rejected anything proves nothing, so the disjoined check's ability
to refuse a landing *between* the endpoints is asserted with real recordings: a killed flush
claiming a three-row batch of which the recordings show only two — a torn commit's exact shape —
must raise, two rows being strictly between ``before`` and ``before + three``.
"""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from oracle import catalogue as cat

from .driver import (
    Build,
    Fold,
    Killed,
    Merge,
    Stage,
    StageInvarianceViolation,
    StageResult,
    SuiteHarness,
    Write,
    _PREFIX_RE,
    poll,
    check,
    run_plan,
)
from .entitlement import Nothing, Rows, diff

# One corpus, one principal, one saturation rule — the stage-invariance module's constants and
# its ingest-body builder, imported rather than restated: a second statement of the wire schema
# or the grants would drift, and these plans assert the same invariance with one modifier added.
from .test_stage_invariance import BBOX, FILTER_DEPARTMENT, GRANTS, K, _write_stage


def _harness(private_catalogue_bundle, tmp_path_factory, label: str) -> SuiteHarness:
    return SuiteHarness(
        bundle_root=private_catalogue_bundle(label),
        run_dir=tmp_path_factory.mktemp(f"{label}-run"),
        grants=GRANTS,
        view_id=cat.VIEW_ID,
        bbox=BBOX,
        k=K,
        filters={"department": {"eq": FILTER_DEPARTMENT}},
    )


class FlushReplay(Stage):
    """Pull the tick that flushes whatever WAL replay reinstated after the crash.

    Not a [`Write`]: the batch was ingested — and acked, so the WAL holds it durably — by the
    killed stage. This stage only completes the publication the kill interrupted; the composite
    assertion (`diff` across the kill and this) is the test's, because which endpoint the restart
    lands on is the engine's choice and the entitlement of *this* stage depends on it.
    """

    label = "flush-replayed"

    def __init__(self):
        self._snap: dict | None = None

    def apply(self, h: SuiteHarness) -> None:
        executor = h.executor()
        self._snap = {
            "flushes": executor["flush"]["flushes"],
            "refreshes": executor["flush"]["refreshes"],
        }
        h.pull_tick()

    def barrier(self, h: SuiteHarness) -> None:
        poll(
            lambda: h.executor()["flush"]["flushes"] > self._snap["flushes"],
            "the replayed batch never flushed — was it lost with the crash?",
        )
        poll(
            lambda: h.executor()["flush"]["refreshes"] > self._snap["refreshes"],
            "the post-replay refresh never replaced the resident projection",
        )


# -- the fold's CURRENT flip --------------------------------------------------------------------


@pytest.fixture(scope="module")
def flip_seam(tmp_path_factory, private_catalogue_bundle):
    """One write flushed, then a fold killed while parked before the `CURRENT` rename — the
    folded prefix complete and synced on disc, no durable pointer naming it — the unflipped
    prefix discarded, and a recovery fold over the discarded state."""
    fx = cat.ingest_fx_keys(2)
    h = _harness(private_catalogue_bundle, tmp_path_factory, "crash-flip")
    kill = Killed(Fold(), kill_at="before_current_flip")
    plan = [Build(), _write_stage(0, fx), kill, Fold(label="fold-recovery")]
    try:
        yield {r.label: r for r in run_plan(h, plan)}
    finally:
        h.stop()


def test_a_fold_killed_at_the_flip_lands_on_before_exactly(flip_seam):
    """The disjunction, degenerate by design: a fold is entitled to nothing, so both endpoints
    are `before` and a killed fold must change not one byte of any canonical surface."""
    check(flip_seam["kill-fold"])
    assert flip_seam["kill-fold"].delta == Nothing()


def test_the_flip_discard_took_whole_prefixes_and_nothing_else(flip_seam):
    """§12.3's rule for this seam is prefix-granular — no per-file bookkeeping — so what the
    discard reports taking must be prefix trees (or the flip's own temporary), never a file
    inside the live prefix."""
    discarded = flip_seam["kill-fold"].stage.discarded
    assert discarded, "a fold parked at the flip has written a whole prefix; none was found"
    assert all(_PREFIX_RE.fullmatch(name) or name == "CURRENT.tmp" for name in discarded)


def test_the_discarded_state_still_folds(flip_seam):
    """Recovery: the same bundle, minus everything the power cut took, folds to completion and
    still changes nothing — the crash consumed the fold's work, not its preconditions."""
    check(flip_seam["fold-recovery"])


# -- the side-manifest publish ------------------------------------------------------------------


@pytest.fixture(scope="module")
def manifest_seam(tmp_path_factory, private_catalogue_bundle):
    """A flush killed while parked before its side-manifest commit — the publication's files on
    disc, nothing durable naming them — those files discarded, and the interrupted publication
    completed after the restart.

    Which endpoint the restart lands on is the engine's to choose (the batch is in the WAL
    either way), so the recovery tick is pulled only when the after-recording shows `before`;
    the composite claim the tests make — the acked batch arrives exactly once — holds on both
    branches.
    """
    fx = cat.ingest_fx_keys(2)
    batch = tuple(fx)
    h = _harness(private_catalogue_bundle, tmp_path_factory, "crash-manifest")
    kill = Killed(_write_stage(0, fx), kill_at="before_manifest_publish")
    try:
        results = {r.label: r for r in run_plan(h, [Build(), kill])}
        kill_result = results[kill.label]
        replay = None
        if kill_result.delta != Rows(batch):
            [replay] = run_plan(h, [FlushReplay()])
        yield SimpleNamespace(
            kill=kill_result,
            batch=batch,
            replay=replay,
            final=replay.after if replay is not None else kill_result.after,
        )
    finally:
        h.stop()


def test_a_flush_killed_at_the_manifest_seam_lands_on_an_endpoint(manifest_seam):
    """The disjunction with two distinct endpoints: `before` (the commit never happened) or
    `before + exactly the batch` — and nothing between."""
    check(manifest_seam.kill)


def test_the_acked_batch_arrives_exactly_once(manifest_seam):
    """Across the crash and its recovery together, the acked batch appears exactly once: not
    lost with the discarded files (the WAL held it), not doubled by replay."""
    assert diff(manifest_seam.kill.before, manifest_seam.final) == Rows(manifest_seam.batch)


def test_the_manifest_discard_found_the_unpublished_files(manifest_seam):
    """While the executor is parked the flush's files are on disc and no manifest names them —
    the seam's own premise — so the discard must have taken a non-empty set, all of it inside
    the bundle."""
    assert manifest_seam.kill.stage.discarded


# -- the merge's execution-to-publication gap ---------------------------------------------------


@pytest.fixture(scope="module")
def merge_seam(tmp_path_factory, private_catalogue_bundle):
    """Four writes make a merge eligible (§12.3's ladder arithmetic, as `test_stage_invariance`
    counts it); the dispatching tick's merge executes on the pool and is killed while parked at
    the top of its publication — output segment on disc, inputs untouched, nothing committed.
    The orphan output is discarded and a recovery tick merges the untouched inputs."""
    fx = cat.ingest_fx_keys(8)
    h = _harness(private_catalogue_bundle, tmp_path_factory, "crash-merge")
    kill = Killed(Merge("merge"), kill_at="before_merge_publish")
    plan = [
        Build(),
        _write_stage(0, fx[0:2]),
        _write_stage(1, fx[2:4]),
        _write_stage(2, fx[4:6]),
        _write_stage(3, fx[6:8]),
        kill,
        Merge("merge-recovery"),
    ]
    try:
        yield {r.label: r for r in run_plan(h, plan)}
    finally:
        h.stop()


def test_a_merge_killed_before_publication_lands_on_before_exactly(merge_seam):
    """A merge is entitled to nothing, so as at the flip the disjunction is degenerate: the
    killed merge must leave every canonical surface bytes-identical."""
    check(merge_seam["kill-merge"])
    assert merge_seam["kill-merge"].delta == Nothing()


def test_the_merge_discard_took_its_orphan_output(merge_seam):
    """The rule for this seam: the merge's output segment, its inputs being untouched. The
    inputs' survival is asserted behaviourally by the recovery below; here, that the discard
    took the orphan it names."""
    assert merge_seam["kill-merge"].stage.discarded


def test_the_untouched_inputs_still_merge(merge_seam):
    """Recovery: with the orphan output gone, the same four segments are still eligible and the
    re-run merge publishes — counter up, version bumped, refresh landed (Merge's own barrier) —
    while changing nothing served."""
    check(merge_seam["merge-recovery"])


# -- negative controls --------------------------------------------------------------------------


def test_negative_control_a_landing_between_the_endpoints_is_refused(manifest_seam):
    """The disjoined check must refuse a torn commit: real recordings in which exactly two rows
    of a claimed three-row batch landed sit strictly between `before` and `before + entitlement`,
    and equal neither. A crash check that has never rejected anything proves nothing."""
    never_ingested = cat.ingest_fx_keys(3)[2]
    assert never_ingested not in manifest_seam.batch
    torn_claim = Killed(
        Write(
            label="forged-write",
            body=b"",
            batch_id="forged",
            fx_keys=(*manifest_seam.batch, never_ingested),
        ),
        kill_at="before_manifest_publish",
    )
    delta = diff(manifest_seam.kill.before, manifest_seam.final)
    forged = StageResult("forged-write", torn_claim, manifest_seam.kill.before, manifest_seam.final, delta)
    with pytest.raises(StageInvarianceViolation, match="neither"):
        check(forged)


def test_negative_control_a_kill_lands_only_on_a_publication_seam():
    """The two ack-contract sites take no kill from this driver: their crash story is the WAL's
    (truncate to the sync sidecar), and §12.3's discard table has no row for them — a kill armed
    there would assert atomicity with a discard rule this suite never defined."""
    with pytest.raises(ValueError, match="publication seam"):
        Killed(Fold(), kill_at="after_fsync")
