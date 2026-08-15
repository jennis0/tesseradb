"""The stage driver — correctness-suite §12.3's protocol over the real API, build-order row 3.

The driver walks a stage plan. Per stage, in order: record the battery, apply the stage, barrier,
record again, check the recording delta against the stage's entitlement
(`suite.entitlement.diff`). Everything here speaks `/v1/*` and `/control/*` against a spawned
`tessera serve`, like any deployment — the in-process alternative cannot see the serialisation
layer, where half the compared surfaces only exist (§12 r4).

## Triggering, and why three stages are driven sideways

The control plane is five routes, so build, load, write, deny and the fold are requested directly
— and **merge, coalesce and rotation cannot be requested at all**. They dispatch on eligibility
at the executor's tick. The driver therefore sequences by eligibility (§12.3):

- ``flush_max_age_secs`` is pinned to a day in the suite's own server config, so no tick fires on
  its own and the driver owns the clock;
- ``POST /control/flush`` pulls a tick — consumed by the executor whether or not anything is
  buffered, which is what makes an empty-buffer tick a pure maintenance dispatch;
- **a pulled tick dispatches everything currently eligible**, so a stage is isolated by arranging
  that only it is eligible when its tick is pulled. That arrangement is the *plan's* job (the
  writes are counted so no write's own tick makes a merge or coalesce eligible), and each tick
  stage's barrier asserts the flush counter did not move — a plan that mis-counted fails loudly as
  a broken plan, not quietly as a mis-attributed delta.

Two of §12.3's named knobs did not survive contact with the running system. ``serve.tier_width``
and ``serve.segment_floor_bytes`` are parsed by the server's config and **never reach the
engine** — `tessera_engine::session::merge_policy` hard-codes 4 and 16 MiB — and the coalesce's
width is a `CoalescePolicy` default (8) with no config key at all. So the driver takes the ladder
as found: merge eligibility is four same-tier segments, coalesce eligibility is eight same-tier
delta-axis entries, and the one knob that *does* reach selection — ``max_merged_segment_bytes``,
set here below the base segment's size — is what keeps the base segment out of every merge window
(its own size bound is the exclusion; `tessera-store::merge`'s module doc).

## Barriers

Each stage's barrier is wire- or filesystem-observable, never a sleep:

| stage | barrier |
|---|---|
| build, load | the spawn's own health wait, then `/control/status` readiness |
| write (ingest + flush) | ``flush.flushes`` up, then ``flush.refreshes`` up — the second half is decision 0044 D1: an established session serves its old projection until the background refresh replaces it, so a recording after the version bump alone is short by exactly the batch |
| merge | ``merges`` up, ``segments_version`` bumped, ``refreshes`` up (a merge's publication runs the same refresh pass) |
| coalesce | ``coalesces`` up — the one stage that moves no row and bumps no version (write-path §7), so its barrier must be a counter; the version is asserted *unchanged* |
| deny | the 200 acknowledgement itself — a deny is fail-closed at acceptance (write-path §5.4), so there is nothing later to wait for |
| fold | ``compaction.folds`` up (``fold_failures`` watched so a refused fold is its own diagnosis), then ``refreshes`` up |
| rotation | the WAL's active member index rises on disc — no rotation counter reaches `/control/status`, and the WAL family lives at a path the harness owns (decision 0038: the log is a sequence, not a file) |

The refresh barrier leans on this suite having **one live session per server process**: the
counter counts projections produced, so with several resident sessions it can rise before *this*
session's entry is replaced. One battery principal is the driver's contract with itself.

## Why the server is spawned here and not by `oracle.harness`

`oracle.harness.spawn_server` writes a config without the ``[ingest]`` keys this protocol
requires (the pinned tick clock, the compaction window forced off so a CI run inside the default
00:00–04:00 UTC window cannot self-dispatch a fold mid-plan) or the merge cap. The spawn below
reuses the harness's binary discipline (`ensure_cli_built` — default features, always), its port
allocation, its `Server` handle and its health wait, and writes its own TOML.
"""

from __future__ import annotations

import time
import os
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Sequence

import requests

from oracle import wire
from oracle.harness import (
    CLI_BIN,
    OPERATOR_CREDENTIAL,
    REPO_ROOT,
    SESSION_CREDENTIAL,
    Server,
    ensure_cli_built,
    free_port,
    stop_server,
)

from .battery import Battery, Recorded, Viewport, build_battery, record
from .entitlement import Delta, Entity, Nothing, Rows, Unexplained, diff


class StageInvarianceViolation(AssertionError):
    """A stage changed something it was not entitled to change — or failed to change what it
    claimed. The message carries the stage, the claim and the observed delta."""


def _suite_config(
    bundle_root: Path,
    cache_dir: Path,
    wal_path: Path,
    viewer_port: int,
    session_port: int,
    control_port: int,
) -> str:
    # θ saturated and both caps above any fixture total — §10's saturation precondition, without
    # which a point-set comparison is confounded by the selection refilling behind a removed row.
    # The merge cap sits below the catalogue base segment's ~3.2 MB so the base excludes itself
    # from selection (module doc); the config loader refuses the value if the base ever shrinks
    # under it, which is the loud failure this suite wants.
    return f"""
[bundle]
path = "{bundle_root}"
cache = "{cache_dir}"
wal = "{wal_path}"

[plugin]
module = "builtin:passthrough"

[disclosure]
min_visible_members = 10
token_max_lifetime = 3600

[serve]
viewer = "127.0.0.1:{viewer_port}"
session = "127.0.0.1:{session_port}"
control = "127.0.0.1:{control_port}"
session_credential_env = "TESSERA_REFERENCE_SESSION_CRED"
operator_credential_env = "TESSERA_REFERENCE_OPERATOR_CRED"
max_k = 1000000
k_min = 2
k_max_marks = 1000000
theta_target_marks = 1099511627776
max_merged_segment_bytes = 1048576

[ingest]
flush_max_age_secs = 86400
compaction_window_start = "off"
"""


def _poll(predicate: Callable[[], bool], what: str, timeout: float = 60.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise TimeoutError(f"barrier not reached within {timeout:.0f}s: {what}")


def _wal_member_index(wal_path: Path) -> int:
    """The active WAL member's sequence number — the rotation observable (decision 0038: members
    are ``<stem>-<n:06><ext>``, and the active one is the highest-numbered)."""
    members = sorted(wal_path.parent.glob(f"{wal_path.stem}-[0-9]*{wal_path.suffix}"))
    if not members:
        return -1
    return int(members[-1].stem.rsplit("-", 1)[1])


@dataclass
class SuiteHarness:
    """One suite run's server, session and battery — the mutable context a plan walks.

    The battery principal's grants, the slice and the (full-extent) bbox are the caller's; the
    battery itself is established once, immediately after the establishing stage, and never
    changes for the life of the plan — §12.2's rule that there is one definition of what we ask.
    """

    bundle_root: Path
    run_dir: Path
    grants: tuple[str, ...]
    slice_id: str
    bbox: tuple[float, float, float, float]
    k: int
    filters: dict | None = None
    zooms: tuple[int, ...] = (0, 3)
    underlay_offset: int = 2

    server: Server | None = None
    proc: subprocess.Popen | None = None
    token: str | None = None
    battery: Battery | None = None
    #: tessera_id -> fx_key for every point served at establishment — how a deny stage maps a
    #: battery item back to the item the fixture planted.
    fx_by_tessera: dict[int, int] = field(default_factory=dict)
    item_ids: tuple[int, ...] = ()

    @property
    def cache_dir(self) -> Path:
        return self.run_dir / "cache"

    @property
    def wal_path(self) -> Path:
        return self.run_dir / "wal.log"

    def spawn(self) -> None:
        """Start `tessera serve` on this harness's bundle, cache and WAL, and wait for health.

        Same paths across two calls is exactly the restart-on-same-state case — which is what the
        Load stage is.
        """
        ensure_cli_built()
        viewer_port, session_port, control_port = free_port(), free_port(), free_port()
        config_path = self.run_dir / "tessera.toml"
        config_path.write_text(
            _suite_config(
                self.bundle_root,
                self.cache_dir,
                self.wal_path,
                viewer_port,
                session_port,
                control_port,
            )
        )
        env = os.environ.copy()
        env["TESSERA_REFERENCE_SESSION_CRED"] = SESSION_CREDENTIAL
        env["TESSERA_REFERENCE_OPERATOR_CRED"] = OPERATOR_CREDENTIAL
        self.proc = subprocess.Popen(
            [str(CLI_BIN), "serve", "-c", str(config_path)],
            cwd=REPO_ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        self.server = Server(viewer_port, session_port, control_port)
        deadline = time.monotonic() + 30.0
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                output = self.proc.stdout.read().decode(errors="replace") if self.proc.stdout else ""
                raise RuntimeError(
                    f"tessera serve exited early ({self.proc.returncode}):\n{output}"
                )
            try:
                if requests.get(f"{self.server.viewer_base}/healthz", timeout=1).status_code == 200:
                    break
            except requests.exceptions.ConnectionError:
                pass
            time.sleep(0.1)
        else:
            self.proc.terminate()
            raise RuntimeError("tessera serve did not become healthy within 30s")

    def stop(self) -> None:
        if self.proc is not None:
            stop_server(self.proc)
            self.proc = None
            self.server = None

    def authorise(self) -> None:
        self.token = self.server.authorise(list(self.grants))["token"]

    def establish_battery(self) -> None:
        """Build the battery from the deployment's own answers, once.

        Item ids come from served responses — a `tessera_id` is a keyed permutation minted per
        build, so nothing may persist one across builds — and the same served points supply the
        tessera_id -> fx_key join every deny stage needs. A tiles-form viewport is appended
        beside `build_battery`'s bbox forms so both of contracts §3.2's request forms ride every
        stage.
        """
        meta = self.server.meta(self.token)
        raw = self.server.viewport(
            self.token, self.slice_id, 3, self.bbox, k=self.k, underlay_offset=2
        )
        points = wire.decode_viewport_points(raw)
        self.fx_by_tessera = dict(
            zip(
                points.column("tessera_id").to_pylist(),
                points.column("fx_key").to_pylist(),
            )
        )
        self.item_ids = tuple(sorted(self.fx_by_tessera)[:3])
        battery = build_battery(
            meta,
            slice_id=self.slice_id,
            item_ids=list(self.item_ids),
            bbox=self.bbox,
            zooms=self.zooms,
            k=self.k,
            underlay_offset=self.underlay_offset,
            filters=self.filters,
        )
        tiles_form = Viewport(
            self.slice_id, 1, tiles=(0, 1, 2, 3), k=self.k, underlay_offset=self.underlay_offset
        )
        self.battery = battery + (tiles_form,)

    # -- wire helpers ---------------------------------------------------------------------------

    def status(self) -> dict:
        return self.server.status()

    def executor(self) -> dict:
        return self.status()["write_executor"]

    def pull_tick(self) -> None:
        """`POST /control/flush` and wait for the executor to consume the tick.

        The 202 means accepted, not done; the tick counter is the executor's own record that the
        request was executed, and it is what a pulled maintenance dispatch rides on.
        """
        before = self.executor()["flush"]["ticks"]
        resp = requests.post(
            f"{self.server.control_base}/control/flush",
            headers={"Authorization": f"Bearer {OPERATOR_CREDENTIAL}"},
            timeout=10,
        )
        resp.raise_for_status()
        _poll(
            lambda: self.executor()["flush"]["ticks"] > before,
            "the pulled tick was never consumed",
            timeout=30.0,
        )


# -- stages ------------------------------------------------------------------------------------


class Stage:
    """One §2 stage: `apply` performs it, `barrier` observes that it finished, `entitlement` is
    what it may change. Stages are single-use — apply may capture state the barrier and the
    entitlement read back."""

    label: str

    def apply(self, h: SuiteHarness) -> None:
        raise NotImplementedError

    def barrier(self, h: SuiteHarness) -> None:  # noqa: B027 — acceptance-barriered stages pass
        pass

    def entitlement(self) -> Delta:
        return Nothing()


class Build(Stage):
    """`tessera build`'s bundle, opened by a fresh server — the establishing stage.

    The battery cannot run before a server exists, so this is the one stage with no before-state:
    its recording *is* the baseline every later stage is judged against, and the driver skips its
    entitlement check rather than pretending a comparison happened. (Invariance across a rebuild
    is not a property the system claims — `tessera_id` is minted per build.)
    """

    label = "build"

    def apply(self, h: SuiteHarness) -> None:
        h.spawn()
        h.authorise()

    def barrier(self, h: SuiteHarness) -> None:
        _poll(
            lambda: all(p["readiness"] for p in h.status()["partitions"]),
            "the built bundle never became ready",
            timeout=30.0,
        )


class Load(Stage):
    """Stop the server and reopen the same bundle, cache and WAL — §2's load/reopen/restart row.

    The session does not survive the process, so the harness re-authorises with the same grants;
    that the *answers* survive re-authorisation and replay is exactly this stage's assertion.
    """

    def __init__(self, label: str = "reload"):
        self.label = label

    def apply(self, h: SuiteHarness) -> None:
        h.stop()
        h.spawn()
        h.authorise()

    def barrier(self, h: SuiteHarness) -> None:
        _poll(
            lambda: all(p["readiness"] for p in h.status()["partitions"]),
            "the reopened bundle never became ready",
            timeout=30.0,
        )


class Write(Stage):
    """Ingest one batch over `/control/ingest`, then pull the tick that flushes it and wait for
    the background refresh — §2's write row end to end.

    The entitlement is the ingested rows, and nothing else (§10: flush is the one stage entitled
    to change answers, because the refresh exists to change them — decision 0044 D1). The caller
    supplies the wire-shaped Arrow body and the batch's `fx_key`s; the driver neither builds nor
    understands the corpus's schema.
    """

    def __init__(self, label: str, body: bytes, batch_id: str, fx_keys: Sequence[int]):
        self.label = label
        self._body = body
        self._batch_id = batch_id
        self._fx_keys = tuple(fx_keys)
        self._snap: dict | None = None

    def apply(self, h: SuiteHarness) -> None:
        resp = h.server.ingest(self._body, self._batch_id)
        if resp.status_code != 200:
            raise RuntimeError(f"ingest refused ({resp.status_code}): {resp.text}")
        accepted = resp.json().get("accepted")
        if accepted != len(self._fx_keys):
            raise RuntimeError(
                f"ingest accepted {accepted} rows where the batch carried {len(self._fx_keys)}"
            )
        executor = h.executor()
        self._snap = {
            "flushes": executor["flush"]["flushes"],
            "refreshes": executor["flush"]["refreshes"],
        }
        h.pull_tick()

    def barrier(self, h: SuiteHarness) -> None:
        _poll(
            lambda: h.executor()["flush"]["flushes"] > self._snap["flushes"],
            f"{self.label}: the flush never published",
        )
        _poll(
            lambda: h.executor()["flush"]["refreshes"] > self._snap["refreshes"],
            f"{self.label}: the background refresh never replaced the resident projection — "
            f"a recording now would be short by exactly this batch (decision 0044 D1)",
        )

    def entitlement(self) -> Delta:
        return Rows(self._fx_keys)


class _TickStage(Stage):
    """A stage with no control route, driven by pulling a tick while only it is eligible.

    The barrier asserts isolation as well as completion: the tick must not have published a flush
    (the plan schedules these against an empty commit window), so a mis-arranged plan fails as a
    plan defect rather than shipping a composite delta under one stage's name.
    """

    def __init__(self, label: str):
        self.label = label
        self._snap: dict | None = None

    def _snapshot(self, h: SuiteHarness) -> dict:
        executor = h.executor()
        return {
            "flushes": executor["flush"]["flushes"],
            "refreshes": executor["flush"]["refreshes"],
            "merges": executor["merges"],
            "coalesces": executor["coalesces"],
            "segments_version": h.status()["partitions"][0]["segments_version"],
        }

    def apply(self, h: SuiteHarness) -> None:
        self._snap = self._snapshot(h)
        h.pull_tick()

    def _assert_no_flush(self, h: SuiteHarness) -> None:
        flushes = h.executor()["flush"]["flushes"]
        if flushes != self._snap["flushes"]:
            raise RuntimeError(
                f"{self.label}: the pulled tick also published a flush "
                f"({self._snap['flushes']} -> {flushes}) — the plan did not isolate this stage"
            )


class Merge(_TickStage):
    """The row-space merge: eligible once four same-tier segments exist, dispatched at the pulled
    tick, published with its own `segments_version` bump — and entitled to change nothing."""

    def barrier(self, h: SuiteHarness) -> None:
        _poll(
            lambda: h.executor()["merges"] > self._snap["merges"],
            f"{self.label}: no merge published — either the ladder was not eligible "
            f"(four same-tier segments) or the tick never dispatched it",
        )
        version = h.status()["partitions"][0]["segments_version"]
        if version <= self._snap["segments_version"]:
            raise RuntimeError(
                f"{self.label}: merges counted but segments_version did not bump "
                f"({self._snap['segments_version']} -> {version})"
            )
        # A merge's publication runs the refresh pass; without this wait the after-recording
        # would compare the *old* projection with itself and the stage would test nothing.
        _poll(
            lambda: h.executor()["flush"]["refreshes"] > self._snap["refreshes"],
            f"{self.label}: the post-merge refresh never replaced the resident projection",
        )
        self._assert_no_flush(h)


class Coalesce(_TickStage):
    """The entity-space coalesce: moves no row and bumps no version by design (write-path §7),
    which is why its barrier must be the counter — and why the version is asserted still."""

    def barrier(self, h: SuiteHarness) -> None:
        _poll(
            lambda: h.executor()["coalesces"] > self._snap["coalesces"],
            f"{self.label}: no coalesce published — either no axis reached its width "
            f"(eight same-tier entries) or the tick never dispatched it",
        )
        version = h.status()["partitions"][0]["segments_version"]
        if version != self._snap["segments_version"]:
            raise RuntimeError(
                f"{self.label}: a coalesce moved segments_version "
                f"({self._snap['segments_version']} -> {version}), which write-path §7 says it "
                f"never does"
            )
        self._assert_no_flush(h)


class Deny(Stage):
    """One deny-lane operation — `suppress`, `unsuppress` or `delete` — entitled to exactly the
    entity named, at acceptance.

    The 200 acknowledgement is the barrier: a deny is fail-closed the moment it is accepted
    (write-path §5.4), applying to every subsequent request with nothing later to wait for. The
    target is resolved through a callable so a plan can name a battery item before the battery
    exists — it runs at apply time against the harness's established tessera->fx join.
    """

    def __init__(
        self, op: str, pick: Callable[[SuiteHarness], tuple[str, int]], label: str | None = None
    ):
        if op not in ("suppress", "unsuppress", "delete"):
            raise ValueError(f"not a deny op: {op}")
        self.op = op
        self.label = label or op
        self._pick = pick
        self._fx: int | None = None

    def apply(self, h: SuiteHarness) -> None:
        external_id_b64, self._fx = self._pick(h)
        resp = h.server.change(external_id_b64, self.op)
        if resp.status_code != 200:
            raise RuntimeError(f"{self.op} refused ({resp.status_code}): {resp.text}")

    def entitlement(self) -> Delta:
        if self._fx is None:
            raise RuntimeError(f"{self.label}: entitlement read before apply")
        return Entity(self._fx, restored=self.op == "unsuppress")


class Fold(Stage):
    """The compaction fold, requested over `/control/compact` — five passes, executed deletions,
    a global permutation rewrite, a prefix flip — and entitled to change nothing, because every
    deletion it executes left the served surface at acceptance (§10)."""

    label = "fold"

    def __init__(self):
        self._snap: dict | None = None

    def apply(self, h: SuiteHarness) -> None:
        compaction = h.status()["compaction"]
        self._snap = {
            "folds": compaction["folds"],
            "fold_failures": compaction["fold_failures"],
            "refreshes": h.executor()["flush"]["refreshes"],
        }
        resp = requests.post(
            f"{h.server.control_base}/control/compact",
            headers={"Authorization": f"Bearer {OPERATOR_CREDENTIAL}"},
            timeout=10,
        )
        resp.raise_for_status()

    def barrier(self, h: SuiteHarness) -> None:
        def landed() -> bool:
            compaction = h.status()["compaction"]
            if compaction["fold_failures"] > self._snap["fold_failures"]:
                raise RuntimeError(f"the fold failed rather than landed: {compaction}")
            return compaction["folds"] > self._snap["folds"]

        _poll(landed, "no compaction fold landed", timeout=300.0)
        _poll(
            lambda: h.executor()["flush"]["refreshes"] > self._snap["refreshes"],
            "the post-fold refresh never carried the resident session across the flip",
        )


class Rotate(Stage):
    """WAL rotation — no control route and no status counter, so it is driven as §12.3 says
    (growth since the last rotation, then a pulled tick with nothing to flush) and observed on
    disc: the active member's sequence number rises.

    This is the WAL third of §2's rotation row. The prefix and identity rotations ride the fold
    and are exercised — and barriered — there.
    """

    label = "rotate"

    def __init__(self):
        self._snap: dict | None = None

    def apply(self, h: SuiteHarness) -> None:
        self._snap = {
            "member": _wal_member_index(h.wal_path),
            "flushes": h.executor()["flush"]["flushes"],
        }
        h.pull_tick()

    def barrier(self, h: SuiteHarness) -> None:
        _poll(
            lambda: _wal_member_index(h.wal_path) > self._snap["member"],
            "the WAL never rotated — was there growth since the last rotation for the tick "
            "to see?",
            timeout=30.0,
        )
        flushes = h.executor()["flush"]["flushes"]
        if flushes != self._snap["flushes"]:
            raise RuntimeError(
                f"rotate: the pulled tick also published a flush — the plan did not isolate "
                f"this stage ({self._snap['flushes']} -> {flushes})"
            )


# -- the walk ----------------------------------------------------------------------------------


@dataclass
class StageResult:
    label: str
    stage: Stage
    before: Recorded | None
    after: Recorded
    delta: Delta | Unexplained | None


def run_plan(h: SuiteHarness, plan: Sequence[Stage]) -> list[StageResult]:
    """§12.3's loop: record, apply, barrier, record, diff — for every stage in order.

    The establishing stage (no server yet) has no before-recording and a `None` delta; its
    after-recording is the baseline. The battery is built immediately after it and never again.
    """
    results: list[StageResult] = []
    for stage in plan:
        before = record(h.server, h.token, h.battery) if h.server is not None else None
        stage.apply(h)
        stage.barrier(h)
        if h.battery is None:
            h.establish_battery()
        after = record(h.server, h.token, h.battery)
        delta = diff(before, after) if before is not None else None
        results.append(StageResult(stage.label, stage, before, after, delta))
    return results


def check(result: StageResult, claimed: Delta | None = None) -> None:
    """Assert the recorded delta equals the stage's entitlement (or an explicit `claimed`).

    Raises [`StageInvarianceViolation`] with the claim and the observation side by side — and
    with the diff's own reasons when the change was not expressible as an entitlement at all.
    """
    if result.before is None:
        return  # the establishing stage: nothing to compare against (Build's doc)
    entitled = claimed if claimed is not None else result.stage.entitlement()
    if result.delta == entitled:
        return
    raise StageInvarianceViolation(
        f"stage `{result.label}` was entitled to {entitled!r} and the recordings show "
        f"{result.delta!r}"
    )


__all__ = [
    "Build",
    "Coalesce",
    "Deny",
    "Fold",
    "Load",
    "Merge",
    "Rotate",
    "Stage",
    "StageInvarianceViolation",
    "StageResult",
    "SuiteHarness",
    "Write",
    "check",
    "run_plan",
]
