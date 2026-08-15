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

## The kill modifier

A stage may carry a kill (§10.1): [`Killed`] wraps it, boots the faults build for exactly that
stage, parks the write executor at a named publication seam, SIGKILLs the server, discards what a
power cut would have taken, restarts the ordinary build, and hands the walk back to `run_plan`'s
ordinary record. The comparison then gains one disjunct in [`check`] — ``after ≡ before`` or
``after ≡ before + entitlement``, exactly — and nothing else changes. [`Killed`]'s own doc
carries the argument; `test_crash_atomicity.py` carries the two limits a crash test must not
overclaim past.
"""

from __future__ import annotations

import json
import re
import shutil
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
    kill_server,
    stop_server,
)

from .battery import Battery, Recorded, Viewport, build_battery, record
from .entitlement import Delta, Entity, Nothing, Rows, Unexplained, diff


class StageInvarianceViolation(AssertionError):
    """A stage changed something it was not entitled to change — or failed to change what it
    claimed. The message carries the stage, the claim and the observed delta."""


#: The faults build's own target directory (decision 0071). Never `target/release`, which the
#: oracle harness owns and builds with default features by rule — `ensure_cli_built`'s doc
#: records the incident that rule comes from, and this path is that doc's "its own path".
FAULTS_TARGET_DIR = REPO_ROOT / "target" / "faults"
FAULTS_CLI_BIN = FAULTS_TARGET_DIR / "release" / "tessera"


def ensure_faults_cli_built() -> None:
    """Build the faults binary — `fault-injection` declared by name, to its own target directory.

    Same shape as `ensure_cli_built` and deliberately never short-circuited on existence: cargo
    tracks the feature set, so a stale or wrong-featured binary at this path is rebuilt rather
    than trusted. The separate ``--target-dir`` is what keeps the two binaries from ever
    overwriting each other — a stage carrying a kill boots this one, every other stage boots
    `CLI_BIN`.
    """
    subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "-p",
            "tessera-cli",
            "--features",
            "fault-injection",
            "--target-dir",
            str(FAULTS_TARGET_DIR),
        ],
        cwd=REPO_ROOT,
        check=True,
    )


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


def _post_flush(server: Server) -> None:
    """`POST /control/flush`, waiting for nothing. `SuiteHarness.pull_tick` is the waiting form;
    this is what a `provoke` uses, because a stage about to park the executor cannot promise
    which counters still move before it does."""
    resp = requests.post(
        f"{server.control_base}/control/flush",
        headers={"Authorization": f"Bearer {OPERATOR_CREDENTIAL}"},
        timeout=10,
    )
    resp.raise_for_status()


# -- the per-seam discard rules (§12.3's table) --------------------------------------------------
#
# A SIGKILL alone is not a power cut: the page cache survives process death, so bytes nothing
# fsynced are still readable by the next process. Each function below takes exactly what §12.3
# says a power cut would have taken at its seam — no more, or the test is a false failure; no
# less, or it is a no-op that verifies replay logic while claiming durability.

#: `next_prefix_name`'s shape, as the startup sweep filters it: `v` + five-or-more digits.
_PREFIX_RE = re.compile(r"v\d{5,}")


def _live_prefix(bundle_root: Path) -> str:
    return json.loads((bundle_root / "CURRENT").read_text())["prefix"]


def _bundle_files(bundle_root: Path) -> frozenset[str]:
    """Every file under the bundle root, by root-relative path — the snapshot half of the
    appeared-files discard. The WAL and the cache live outside the bundle root by this suite's
    layout, so neither can be swept up: the WAL's durability boundary is its own (the sync
    sidecar rule), and the cache is not part of the bundle contract."""
    return frozenset(str(p.relative_to(bundle_root)) for p in bundle_root.rglob("*") if p.is_file())


def _reference_strings(node) -> set[str]:
    """Every dict key and string value in a parsed manifest, recursively. Manifest file
    references are prefix-relative path strings — `files` map keys and the named path fields —
    and collecting all strings rather than schema-chasing keeps this independent of which field
    a future manifest shape names a file in. Over-collection is safe: it can only make the
    discard *keep* a file, never take one."""
    out: set[str] = set()
    if isinstance(node, dict):
        for key, value in node.items():
            out.add(key)
            out |= _reference_strings(value)
    elif isinstance(node, list):
        for value in node:
            out |= _reference_strings(value)
    elif isinstance(node, str):
        out.add(node)
    return out


def _committed_references(bundle_root: Path) -> tuple[str, set[str]]:
    """The live prefix and every path a durable, reachable manifest names. Reachability roots at
    `CURRENT`: the prefix it names, that prefix's `MANIFEST.json`, and every complete
    `SEGMENTS-<n>.json` under it. A manifest inside an unflipped prefix is itself unreachable and
    deliberately contributes nothing."""
    prefix = _live_prefix(bundle_root)
    prefix_dir = bundle_root / prefix
    refs: set[str] = set()
    for manifest in [
        prefix_dir / "MANIFEST.json",
        *sorted(prefix_dir.glob("partitions/*/SEGMENTS-*.json")),
    ]:
        refs |= _reference_strings(json.loads(manifest.read_text()))
    return prefix, refs


def _looks_like_commitment(rel_path: str) -> bool:
    """Whether a path is itself commit-point metadata — `CURRENT`, a `MANIFEST.json`, a complete
    side-manifest. None may ever *appear* during a stage parked before its commit; one appearing
    means the kill did not land where the seam claimed, and the discard must refuse rather than
    delete committed state."""
    name = Path(rel_path).name
    if name.endswith(".tmp"):
        return False
    return name in ("CURRENT", "MANIFEST.json") or (
        name.startswith("SEGMENTS-") and name.endswith(".json")
    )


def _discard_appeared_files(bundle_root: Path, before: frozenset[str]) -> tuple[str, ...]:
    """§12.3's discard rule for the manifest-publish and merge-publish seams: every file the
    killed stage wrote that no durable manifest names — the side-manifest being written and the
    segment files only it would have named, or a merge's output segment, its inputs untouched.

    Implemented as *appeared since the snapshot, unreferenced by any committed manifest*, with
    both halves load-bearing: the snapshot keeps the discard from touching pre-existing
    unreferenced residue that a power cut would not have taken (it may be synced, and it is the
    reclamation's business), and the reference check turns a kill that missed its seam into a
    named refusal instead of a deleted publication.
    """
    appeared = sorted(_bundle_files(bundle_root) - before)
    prefix, refs = _committed_references(bundle_root)
    published = [
        rel
        for rel in appeared
        if _looks_like_commitment(rel)
        or (rel.startswith(f"{prefix}/") and rel[len(prefix) + 1 :] in refs)
    ]
    if published:
        raise RuntimeError(
            f"the parked executor was meant to have published nothing, but committed state "
            f"appeared during the killed stage: {published[:4]} — the kill did not land on the "
            f"seam it claimed, and discarding would corrupt a real publication"
        )
    for rel in appeared:
        (bundle_root / rel).unlink()
    # Directories the stage created are empty once their files are gone; prune deepest-first and
    # leave any that still hold pre-existing files.
    for rel_dir in sorted({str(Path(rel).parent) for rel in appeared}, key=len, reverse=True):
        try:
            (bundle_root / rel_dir).rmdir()
        except OSError:
            pass
    return tuple(appeared)


def _discard_unflipped_prefixes(bundle_root: Path) -> tuple[str, ...]:
    """§12.3's discard rule for the `CURRENT`-flip seam: the whole unflipped prefix, which is
    unreferenced by construction — the fold's commit point is a single rename, so everything it
    wrote is discardable until that happens. No snapshot and no bookkeeping, which is why this
    seam's test is the first built. `CURRENT.tmp` is included for completeness; parked before
    `write_current` none exists, but a power cut inside the flip itself would leave one."""
    live = _live_prefix(bundle_root)
    took: list[str] = []
    for child in sorted(bundle_root.iterdir()):
        if child.is_dir() and child.name != live and _PREFIX_RE.fullmatch(child.name):
            shutil.rmtree(child)
            took.append(child.name)
    tmp = bundle_root / "CURRENT.tmp"
    if tmp.exists():
        tmp.unlink()
        took.append("CURRENT.tmp")
    return tuple(took)


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

    def spawn(self, faults: bool = False) -> None:
        """Start `tessera serve` on this harness's bundle, cache and WAL, and wait for health.

        Same paths across two calls is exactly the restart-on-same-state case — which is what the
        Load stage is. ``faults=True`` boots the faults build from its own target directory
        (decision 0071) — only a [`Killed`] stage does this, and only for its own life; every
        other spawn is the default-features binary the oracle harness owns.
        """
        if faults:
            ensure_faults_cli_built()
            binary = FAULTS_CLI_BIN
        else:
            ensure_cli_built()
            binary = CLI_BIN
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
            [str(binary), "serve", "-c", str(config_path)],
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

    def kill(self) -> None:
        """SIGKILL — no graceful shutdown, no unwind, no drop guards. The crash half of the kill
        modifier; `oracle.harness.kill_server` is the same call the restart-replay test uses."""
        if self.proc is not None:
            kill_server(self.proc)
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

    # -- the faults build's arming surface (decision 0071) --------------------------------------

    def arm(self, site: str) -> None:
        """`POST /control/faults/arm` — faults build only; against the ordinary binary the route
        does not exist and this raises, which is the right failure for a plan that put a kill on
        a stage without booting the build that can take one."""
        resp = requests.post(
            f"{self.server.control_base}/control/faults/arm",
            headers={"Authorization": f"Bearer {OPERATOR_CREDENTIAL}"},
            json={"site": site},
            timeout=10,
        )
        resp.raise_for_status()

    def arrivals(self, site: str) -> int:
        """How many times the executor has reached `site` since it was armed. Non-zero under a
        stall arming means a thread is demonstrably parked — the kill precondition, observed
        rather than slept for."""
        resp = requests.get(
            f"{self.server.control_base}/control/faults/arrivals",
            headers={"Authorization": f"Bearer {OPERATOR_CREDENTIAL}"},
            params={"site": site},
            timeout=10,
        )
        resp.raise_for_status()
        return resp.json()["arrivals"]


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

    def provoke(self, h: SuiteHarness) -> None:
        """The non-waiting half of `apply`, for a stage carrying a kill: issue the request that
        makes the stage run, and wait for nothing. `apply`'s waits observe completion, and a
        stage about to park at a seam never completes — the [`Killed`] wrapper's observable is
        the parked arrival instead. Stages with no provocation defined cannot carry a kill."""
        raise RuntimeError(f"stage `{self.label}` cannot carry a kill: no provocation defined")


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

    def _ingest(self, h: SuiteHarness) -> None:
        resp = h.server.ingest(self._body, self._batch_id)
        if resp.status_code != 200:
            raise RuntimeError(f"ingest refused ({resp.status_code}): {resp.text}")
        accepted = resp.json().get("accepted")
        if accepted != len(self._fx_keys):
            raise RuntimeError(
                f"ingest accepted {accepted} rows where the batch carried {len(self._fx_keys)}"
            )

    def apply(self, h: SuiteHarness) -> None:
        self._ingest(h)
        executor = h.executor()
        self._snap = {
            "flushes": executor["flush"]["flushes"],
            "refreshes": executor["flush"]["refreshes"],
        }
        h.pull_tick()

    def provoke(self, h: SuiteHarness) -> None:
        """Ingest — acked against the WAL, so the batch is durable whatever happens next — then
        request the tick whose flush will run into the armed seam."""
        self._ingest(h)
        _post_flush(h.server)

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

    def provoke(self, h: SuiteHarness) -> None:
        """Request the tick that dispatches this stage, and wait for nothing (base doc)."""
        _post_flush(h.server)

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

    def __init__(self, label: str = "fold"):
        self.label = label
        self._snap: dict | None = None

    @staticmethod
    def _request(h: SuiteHarness) -> None:
        resp = requests.post(
            f"{h.server.control_base}/control/compact",
            headers={"Authorization": f"Bearer {OPERATOR_CREDENTIAL}"},
            timeout=10,
        )
        resp.raise_for_status()

    def apply(self, h: SuiteHarness) -> None:
        compaction = h.status()["compaction"]
        self._snap = {
            "folds": compaction["folds"],
            "fold_failures": compaction["fold_failures"],
            "refreshes": h.executor()["flush"]["refreshes"],
        }
        self._request(h)

    def provoke(self, h: SuiteHarness) -> None:
        """Request the fold, and wait for nothing (base doc)."""
        self._request(h)

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


# -- the kill modifier (§10.1, §12.3) ----------------------------------------------------------


#: The three publication seams a kill may land on, with the counters that must not have moved
#: while the executor is parked there — "parked means blocked" made checkable. The switchboard's
#: two ack-contract sites are deliberately absent: their crash story is the WAL's, held by the
#: truncating restart-replay tests, and §12.3's discard table has no row for them.
_SEAM_SITES = ("before_manifest_publish", "before_current_flip", "before_merge_publish")


class Killed(Stage):
    """A stage killed at a publication seam — §10.1's modifier, not a mechanism of its own.

    The whole crash apparatus is borrowed: the stage's own `provoke` starts it, the switchboard's
    arrival count proves a thread is parked at the seam, `run_plan`'s ordinary recordings supply
    both sides of the comparison, and [`check`] grows exactly one disjunct — ``after ≡ before``
    or ``after ≡ before + entitlement``, and never anything between. That is what makes a crash
    affordable at every stage rather than at the one everybody worries about: the plan gains one
    wrapper, and the entitlement algebra it already trusts does the work.

    The sequence: boot the faults build (decision 0071 — the seam pause sites exist in no other
    binary, and it lives at its own target path so `target/release/tessera` stays the harness's
    default-features build), arm the site, provoke the stage, wait for the executor to
    demonstrably arrive, assert nothing published, SIGKILL, optionally discard what was not
    synced, then boot the ordinary build for the after-recording — the restarts book-ended inside
    this stage are reopen-on-same-state, entitled to change nothing, so anything they perturbed
    fails the same disjunction.

    **`discard_unsynced` is what makes this a power-loss simulation rather than a replay test.**
    The page cache survives process death: every byte the killed process wrote is readable by the
    next one whether or not anything fsynced it, so a kill-and-restart alone cannot fail an
    engine that acks before it syncs — measured, not argued (§10.1). The discard applies §12.3's
    per-seam rule for what a power cut would have taken; each rule's own doc carries its
    reasoning. Run with ``discard_unsynced=False`` the same walk still asserts the disjunction,
    but it is then a statement about recovery logic only, and no plan may present it as more.
    """

    def __init__(self, stage: Stage, kill_at: str, discard_unsynced: bool = True):
        if kill_at not in _SEAM_SITES:
            raise ValueError(
                f"not a publication seam: {kill_at!r} — a kill lands on one of {_SEAM_SITES}"
            )
        self.stage = stage
        self.kill_at = kill_at
        self.discard_unsynced = discard_unsynced
        self.label = f"kill-{stage.label}"
        #: Root-relative paths (or prefix names, at the flip) the discard took — the test's
        #: evidence that the rule fired, and on what.
        self.discarded: tuple[str, ...] = ()
        self._snap: dict | None = None

    def _published_counters(self, h: SuiteHarness) -> dict:
        status = h.status()
        executor = status["write_executor"]
        return {
            "flushes": executor["flush"]["flushes"],
            "merges": executor["merges"],
            "segments_version": status["partitions"][0]["segments_version"],
            "folds": status["compaction"]["folds"],
            "fold_failures": status["compaction"]["fold_failures"],
        }

    def _assert_nothing_published(self, h: SuiteHarness) -> None:
        """Parked means *blocked*: the seam's own publication counter must not have moved, and
        no flush may have ridden the same tick at the other two seams (the isolation the tick
        stages already assert, kept under a kill)."""
        now = self._published_counters(h)
        watched = {
            "before_manifest_publish": ("flushes",),
            "before_merge_publish": ("merges", "segments_version", "flushes"),
            "before_current_flip": ("folds", "fold_failures", "flushes"),
        }[self.kill_at]
        for key in watched:
            if now[key] != self._snap[key]:
                raise RuntimeError(
                    f"{self.label}: `{key}` moved ({self._snap[key]} -> {now[key]}) while the "
                    f"executor was parked at {self.kill_at} — parked was supposed to mean "
                    f"nothing published"
                )

    def apply(self, h: SuiteHarness) -> None:
        h.stop()
        h.spawn(faults=True)
        _poll(
            lambda: all(p["readiness"] for p in h.status()["partitions"]),
            f"{self.label}: the faults build never became ready",
            timeout=30.0,
        )
        files_before = _bundle_files(h.bundle_root)
        self._snap = self._published_counters(h)
        h.arm(self.kill_at)
        self.stage.provoke(h)
        _poll(
            lambda: h.arrivals(self.kill_at) >= 1,
            f"{self.label}: the executor never reached {self.kill_at}",
            timeout=300.0,
        )
        self._assert_nothing_published(h)
        h.kill()
        if self.discard_unsynced:
            if self.kill_at == "before_current_flip":
                self.discarded = _discard_unflipped_prefixes(h.bundle_root)
            else:
                self.discarded = _discard_appeared_files(h.bundle_root, files_before)
            if not self.discarded:
                raise RuntimeError(
                    f"{self.label}: the discard found nothing to take — the seam's premise is "
                    f"that the publication's files are on disc while the executor is parked, and "
                    f"a discard with nothing to do is a crash test quietly reduced to a restart"
                )
        h.spawn()
        h.authorise()

    def barrier(self, h: SuiteHarness) -> None:
        _poll(
            lambda: all(p["readiness"] for p in h.status()["partitions"]),
            f"{self.label}: the restarted bundle never became ready",
            timeout=30.0,
        )

    def entitlement(self) -> Delta:
        return self.stage.entitlement()


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

    A [`Killed`] stage is checked against §10.1's disjunction instead — ``after ≡ before`` or
    ``after ≡ before + entitlement``, exactly, with no third outcome — which is the whole of what
    crash atomicity adds to the comparison.

    Raises [`StageInvarianceViolation`] with the claim and the observation side by side — and
    with the diff's own reasons when the change was not expressible as an entitlement at all.
    """
    if result.before is None:
        return  # the establishing stage: nothing to compare against (Build's doc)
    entitled = claimed if claimed is not None else result.stage.entitlement()
    if isinstance(result.stage, Killed):
        if result.delta == Nothing() or result.delta == entitled:
            return
        raise StageInvarianceViolation(
            f"stage `{result.label}` was killed mid-flight and must land on an endpoint — "
            f"`before` exactly, or `before + {entitled!r}` exactly — and the recordings show "
            f"{result.delta!r}, which is neither"
        )
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
    "FAULTS_CLI_BIN",
    "Fold",
    "Killed",
    "Load",
    "Merge",
    "Rotate",
    "Stage",
    "StageInvarianceViolation",
    "StageResult",
    "SuiteHarness",
    "Write",
    "check",
    "ensure_faults_cli_built",
    "run_plan",
]
