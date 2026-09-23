"""Server profiles — correctness-suite §7 over §12.3's driver, build-order row 7.

**The regime a deployment at 10⁹ actually runs in — less memory than data — is the one nothing
else tests.** Every artefact is memory-mapped and advised; that this works when the pages cannot
all stay resident has been an assumption. The variable is the memory-to-data ratio, not the
absolute size, which is what makes the regime affordable here: capping the server's memory from
outside reproduces at fixture size the eviction pressure a billion-row corpus applies by weight,
for the cost of a cgroup flag instead of hours and a machine. One plan — the full §2 stage
sequence `test_stage_invariance` walks, imported rather than transcribed so the two modules
cannot drift apart on the eligibility arithmetic — is walked once per profile, and every stage is
checked against its entitlement with the recordings of its own walk.

**Each profile asserts that the walk completes and its answers are correct — never that it fits
a bound.** A timing or memory budget belongs to `measurement.md`; a threshold assertion in a test
that runs on developer machines is a flake generator. And an invariance comparison never crosses
a profile: byte-identical responses are a property of one pinned thread count, an implementation
detail rather than a guarantee (decision 0030), so `default`'s role as baseline is about
*outcomes* — the same plan passing there is what isolates the regime as the variable when
another profile fails. The module's structure enforces the rule rather than remembering it: the
walks share a fixture slot, so no two profiles' recordings ever exist at the same time.

## The constrained number, at fixture size

§7 fixes `constrained` at roughly half the bundle, a rule written for corpora that dwarf the
process. At fixture size it inverts: the catalogue bundle is ~16 MiB, half of it is ~8 MiB, and
the server binary alone is 33 MiB — the process cannot exist in a literal half-bundle, so the
limit here is a measured process floor plus half the bundle, keeping the rule's live half (the
bundle's pages cannot all stay charged alongside the plan's transients) at the size where the
floor still dominates. The floor is measured, not supposed: VmHWM 34,064 kB under the saturated
battery on the catalogue bundle (2026-08-15, this host), with headroom for the write, fold and
rotation phases the read-only measurement does not exercise. At deployment scale the floor is
rounding noise and the rule reads as written.

Two behaviours below the completion zone were measured while pinning the out-of-memory control's
limit, and the gap between them matters:

- at 8 MiB the kernel kills the server during boot, reliably — the binary's own text cannot
  fault in (`memory.events` records `oom_kill 1`);
- at 16 MiB the server neither boots nor dies: enough charge is reclaimable that the kernel
  thrashes text pages indefinitely, and the run surfaces as a health-wait timeout with
  `oom_kill 0` — a resource outcome the cgroup never names.

So the control below pins the discrimination at 8 MiB, where the kill is certain, and the
constrained walk's limit sits far from both bands. The thrash band is why the floor carries
headroom rather than hugging the measurement.

**At this corpus size the limit is real but does not yet bite — measured, and stated so nobody
reads more into a green walk than it holds.** The constrained walk completes with every
`memory.events` counter at zero and a peak charge of ~20 MiB against its ~55 MiB limit: the
battery's working set simply fits, because the floor that dominates the limit also dwarfs the
half-bundle that was §7's point. What this walk therefore establishes is the machinery — the
scope wrapper, the charge accounting, the outcome classification — and completion-with-correct-
answers under it; the eviction regime itself starts to exist where the corpus outgrows the
floor, which is §14's row 7 sizing the profile at 10⁷, not here.

## Why a memory kill must not look like a correctness failure

A server killed under the limit exits by signal, and every symptom the driver then sees — a
refused connection mid-battery, a barrier that never comes — is indistinguishable from the
symptoms of a wedged or corrupted engine. The cgroup is the only witness that knows the
difference: the kernel records the kill in the scope's `memory.events` before the death is
observable. The driver reads that and raises `OutOfMemory` — a measurement, deliberately not an
`AssertionError` — because a harness that scores the two alike reports a data-corruption bug
where the finding is "the engine does not fit", and sends someone to the wrong crate. The
control here contrives the kill and asserts the classification, so the profile's first real
out-of-memory arrives pre-triaged.
"""

from __future__ import annotations

import pytest

from oracle import catalogue as cat

from .driver import (
    Build,
    Coalesce,
    Deny,
    Fold,
    Load,
    Merge,
    OutOfMemory,
    Profile,
    Rotate,
    StageInvarianceViolation,
    SuiteHarness,
    _advise_out_of_page_cache,
    check,
    run_plan,
    scope_runner_unavailable,
)
from .test_stage_invariance import (
    BBOX,
    CHECKED_LABELS,
    COALESCE_WIDTH,
    FILTER_DEPARTMENT,
    GRANTS,
    K,
    MERGE_TIER_WIDTH,
    _battery_item,
    _write_stage,
)

#: The measured process floor for the constrained limit (module doc: VmHWM 34,064 kB under the
#: saturated battery, plus headroom for the plan's write-path transients and clear of the
#: measured 16 MiB thrash band).
PROCESS_FLOOR_BYTES = 48 * 1024 * 1024

#: The out-of-memory control's limit — inside the certain-kill band the module doc records.
#: Contrived on purpose: the control exists to observe the discrimination, not the regime.
OOM_CONTROL_LIMIT_BYTES = 8 * 1024 * 1024

PROFILE_NAMES = ("default", "constrained", "cold", "single-thread")


def _profile(name: str, bundle_root) -> Profile:
    if name == "default":
        return Profile()
    if name == "cold":
        return Profile("cold", cold=True)
    if name == "single-thread":
        return Profile("single-thread", compute_threads=1)
    if name == "constrained":
        bundle_bytes = sum(p.stat().st_size for p in bundle_root.rglob("*") if p.is_file())
        return Profile("constrained", memory_max=PROCESS_FLOOR_BYTES + bundle_bytes // 2)
    raise ValueError(f"not a profile: {name}")


def _rotation_growth(h: SuiteHarness) -> None:
    """Suppress and immediately unsuppress battery item 0 — two accepted WAL appends whose net
    served-surface effect is nothing, so the rotate stage's `Nothing` entitlement still holds.

    The rotate stage needs WAL growth from the boot that takes its tick (`Rotate`'s doc), and
    under the cold profile every stage begins in a fresh boot, so the growth the shared plan's
    denies provided is discarded at the boundary. Carried in *every* profile's plan rather than
    the cold one's only, so the four walks stay the same sequence — the module's whole claim —
    and because a pair that failed to restore byte-identically would fail the rotate stage's
    entitlement in the warm profiles too, which is coverage, not cost.
    """
    external_id_b64, _fx = _battery_item(0)(h)
    for op in ("suppress", "unsuppress"):
        resp = h.server.change(external_id_b64, op)
        if resp.status_code != 200:
            raise RuntimeError(
                f"rotation growth: {op} refused ({resp.status_code}): {resp.text}"
            )


def _full_plan() -> list:
    """The stage-invariance plan, stage for stage — §7 applies a profile to a full stage
    sequence, and this module's claim is that the *same* sequence holds under every regime."""
    fx = cat.ingest_fx_keys(12)
    return [
        Build(),
        _write_stage(0, fx[0:2]),
        _write_stage(1, fx[2:4]),
        _write_stage(2, fx[4:6]),
        Merge("merge-1"),
        _write_stage(3, fx[6:8]),
        Coalesce("coalesce"),
        _write_stage(4, fx[8:10]),
        Merge("merge-2"),
        _write_stage(5, fx[10:12]),
        Deny("suppress", _battery_item(0)),
        Deny("unsuppress", _battery_item(0)),
        Deny("delete", _battery_item(1)),
        Rotate(grow=_rotation_growth),
        Load("reload-live"),
        Fold(),
        Load("reload-folded"),
    ]


@pytest.fixture(scope="module", params=PROFILE_NAMES)
def profile_walk(request, tmp_path_factory, private_catalogue_bundle):
    """One full plan walk under one profile — a private bundle copy and its own harness, so no
    recording, deny or fold from one profile's walk can reach another's."""
    name = request.param
    if name == "constrained":
        reason = scope_runner_unavailable()
        if reason is not None:
            pytest.skip(f"the constrained profile needs a user cgroup scope: {reason}")
    bundle = private_catalogue_bundle(f"profiles-{name}")
    if name == "constrained":
        # A cgroup charges a file page to the first cgroup that faults it, and this private copy
        # was just written — its pages sit in the page cache charged to *this* process, so
        # whatever the scoped server reads of them is free against its limit. Advised away once,
        # before the walk, so the server's own faults carry the charge; once and not per spawn,
        # because per-spawn eviction is the cold profile's regime, not this one's. At fixture
        # size the correction is small (the walk's charged working set measured ~20 MiB with or
        # without it — the battery simply never faults most of the bundle), but the sharing it
        # removes scales with everything a busier host happens to hold resident, and a limit
        # accounting someone else's cache is not the limit the profile declared.
        _advise_out_of_page_cache(bundle)
    h = SuiteHarness(
        bundle_root=bundle,
        run_dir=tmp_path_factory.mktemp(f"profiles-{name}-run"),
        grants=GRANTS,
        view_id=cat.VIEW_ID,
        bbox=BBOX,
        k=K,
        filters={"department": {"eq": FILTER_DEPARTMENT}},
        profile=_profile(name, bundle),
        merge_tier_width=MERGE_TIER_WIDTH,
        coalesce_width=COALESCE_WIDTH,
    )
    try:
        results = run_plan(h, _full_plan())
        yield h, {r.label: r for r in results}
    finally:
        h.stop()


def test_the_walk_ran_every_stage_the_plan_promised(profile_walk):
    _, results = profile_walk
    assert set(results) == {"build", *CHECKED_LABELS}


@pytest.mark.parametrize("label", CHECKED_LABELS)
def test_every_stage_answers_correctly_within_its_profile(profile_walk, label):
    """The profile's whole claim, per stage: the walk completed and
    `diff(before, after) == entitlement`, judged entirely inside one profile's recordings."""
    _, results = profile_walk
    check(results[label])


def test_a_completed_constrained_walk_is_accounted_for_by_its_cgroup(profile_walk):
    """The constrained walk's bookkeeping, not a bound: the scope's memory record is readable
    while the walk's server still runs, and a walk that completed was by definition not killed —
    `oom_kill` at zero is outcome coherence, the same ledger the discrimination control reads
    from the other side."""
    h, _ = profile_walk
    if h.profile.memory_max is None:
        pytest.skip("only the constrained profile runs under a scope")
    report = h.scope_memory()
    assert report is not None, "the scope's cgroup was not readable while the server ran"
    assert report["events"].get("oom_kill", 0) == 0, (
        "a walk that completed every stage cannot also have been killed under its limit"
    )


# -- §12.5's second rule, observed rather than trusted -------------------------------------------


def test_a_memory_killed_run_reports_a_resource_outcome_not_a_correctness_failure(
    tmp_path_factory, private_catalogue_bundle
):
    """Contrive the kill — a limit inside the measured certain-kill band — and observe the
    discrimination: the walk raises `OutOfMemory` carrying the cgroup's own `oom_kill` count,
    and that type is not the correctness failure's type, so the first real out-of-memory a
    constrained run meets is reported as a measurement rather than triaged as data corruption."""
    reason = scope_runner_unavailable()
    if reason is not None:
        pytest.skip(f"the constrained profile needs a user cgroup scope: {reason}")
    h = SuiteHarness(
        bundle_root=private_catalogue_bundle("profiles-oom-control"),
        run_dir=tmp_path_factory.mktemp("profiles-oom-control-run"),
        grants=GRANTS,
        view_id=cat.VIEW_ID,
        bbox=BBOX,
        k=K,
        profile=Profile("constrained", memory_max=OOM_CONTROL_LIMIT_BYTES),
    )
    try:
        with pytest.raises(OutOfMemory) as caught:
            run_plan(h, [Build()])
    finally:
        h.stop()
    outcome = caught.value
    assert outcome.events.get("oom_kill", 0) >= 1, (
        "the outcome must carry the cgroup's own record of the kill, not an inference from "
        "the symptoms"
    )
    assert outcome.stage_label == "build"
    assert outcome.__cause__ is not None, (
        "the classification wraps the failure the driver actually saw; discarding it would "
        "hide what the death looked like from the inside"
    )
    assert not isinstance(outcome, AssertionError)


def test_a_resource_outcome_can_never_be_scored_as_a_correctness_failure():
    """The type relationship the discrimination hangs on, pinned structurally: anything that
    catches or counts `StageInvarianceViolation` — or assertion failures generally — can never
    sweep up an `OutOfMemory`. Someone folding the one into the other's hierarchy "for tidiness"
    fails here, with the reason in front of them."""
    assert not issubclass(OutOfMemory, AssertionError)
    assert not issubclass(OutOfMemory, StageInvarianceViolation)
    assert not issubclass(StageInvarianceViolation, OutOfMemory)
