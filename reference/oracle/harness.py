"""Shared server-spawning harness (Task 15).

Extracted from `reference/tests/conftest.py` so `conformance/` can reuse it without copy-paste —
CLAUDE.md's "Python drives, never implements" applies to test drivers too: one process-spawning
implementation, reused by both suites, not two divergent copies.

Everything here does the same thing `reference/tests/conftest.py` did before this refactor: build
the release `tessera` binary via cargo if it is missing, build a fixture bundle via the CLI if one
is missing at the requested root, spawn `tessera serve` as a real subprocess against a
`tessera.toml` this module writes, and give the caller an HTTP-only `Server` handle. Nothing here
calls into `tessera-engine` directly — only HTTP and the filesystem (Phase 1's fixture bundle
build is via the CLI subprocess, same as before).
"""

from __future__ import annotations

import base64
import json
import os
import signal
import shutil
import socket
import subprocess
import time
from pathlib import Path

import requests

REPO_ROOT = Path(__file__).resolve().parents[2]
CLI_BIN = REPO_ROOT / "target" / "release" / "tessera"

SESSION_CREDENTIAL = "reference-oracle-session-secret"
OPERATOR_CREDENTIAL = "reference-oracle-operator-secret"

DEFAULT_POINTS = "data/scaled/geometry.parquet"
DEFAULT_PAIRS = "data/scaled/pairs/categories-subclass.pairs.parquet"
DEFAULT_LIMIT = 250_000
DEFAULT_EXTENT = "0,65536,0,65536"
DEFAULT_VIEW = "s0"


# ---------------------------------------------------------------------------------------------
# Fixture reuse: a stamped recipe, not a predicate over the artefact
# ---------------------------------------------------------------------------------------------
#
# A fixture bundle is built once per machine at a fixed path and reused across sessions. Deciding
# *whether* it may be reused by inspecting the bundle is the construction that failed twice in this
# file's own history — once on the r6 `identity` object, once on `--mint-external-ids` — and each
# time the fix was to extend the predicate by one more clause. That is an allowlist, and the input
# that is not on it is precisely the one that goes wrong silently.
#
# The receipt inverts it: the builder writes down **the whole input set** it built from, and the
# reuse test is equality against the input set the caller wants now. Adding a build input can then
# only fail in the safe direction — an unrecorded input is a rebuild that was not needed, never a
# reuse that should not have happened. It is also the answer to how the suite sat red for a month:
# green was a function of `(checkout, /tmp state)` rather than of the checkout, because the state
# in `/tmp` carried no record of what produced it.


def recipe_path(bundle_root: Path) -> Path:
    """The receipt's path — *beside* the bundle, not inside it.

    Outside, because the bundle is a `tessera build` output and the byte-scanner and the shape
    checks treat everything under it as the build's own; a fixture-management file living there
    would be the suite planting something in the artefact it is supposed to be auditing.
    """
    return bundle_root.parent / f"{bundle_root.name}.FIXTURE.json"


def read_recipe(bundle_root: Path) -> dict | None:
    """The stamped recipe, or `None` if there isn't a readable one."""
    try:
        return json.loads(recipe_path(bundle_root).read_text())
    except (OSError, ValueError):
        return None


def write_recipe(bundle_root: Path, wanted: dict | None) -> None:
    """Stamp the recipe, or remove the stamp when `wanted` is `None`.

    Removing first and stamping last is what makes the receipt mean "this bundle was built from
    this, completely": a build that dies part way through leaves no receipt at all.
    """
    path = recipe_path(bundle_root)
    if wanted is None:
        path.unlink(missing_ok=True)
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(wanted, indent=2, sort_keys=True))


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def ensure_cli_built() -> None:
    """Build `target/release/tessera` with **default features**, always.

    This deliberately does NOT short-circuit on `CLI_BIN.exists()`. Cargo already
    no-ops in about a second when nothing has changed, so the saving was negligible —
    and the cost was severe: an existence check cannot tell a default-feature binary
    from one built with a *measurement* feature enabled.

    That is not hypothetical. On 2026-07-30 the tail-discrimination probe built a
    `--features tessera-engine/skip-id-index` binary into this same path. Every
    subsequent run of this suite silently reused it, so the external-ID index was
    disabled and every `/control/changes` request panicked the server — surfacing as
    two `RemoteDisconnected` failures that looked like a code regression and survived
    a `git stash` (stashing sources does not rebuild a binary), which made them look
    pre-existing on master. They were an artefact.

    Letting cargo decide is the fix: it tracks the feature set, so a binary left
    behind with the wrong features is rebuilt rather than trusted. Anything needing a
    non-default binary must build it to its own path and never to `CLI_BIN`.
    """
    subprocess.run(
        ["cargo", "build", "--release", "-p", "tessera-cli"],
        cwd=REPO_ROOT,
        check=True,
    )



def write_deployment(path: Path, *, bundle: Path, schema: Path) -> Path:
    """Write the `tessera.toml` a build is invoked against, and hand back its path.

    **Every harness here names it with `--deployment` rather than letting the search find one.**
    `tessera build` walks up from the working directory looking for this file (`configuration.md`
    §3); a test run's working directory is the repository, which has none, and whatever the search
    found above it would not be the fixture's. Naming it is the deterministic half.

    The `[serve]` section is here because the file describes a whole deployment; nothing in a build
    reads it, and the serving *secrets* are read at startup rather than at parse, so a build needs
    none of them exported.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"""
[bundle]
path  = "{bundle}"
cache = "{bundle}.cache"
wal   = "{bundle}.wal"

[build]
schema = "{schema}"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
"""
    )
    return path


#: The environment variable `tessera.toml` names by default, and the one every fixture build here
#: passes its identity key through. A key on a command line reaches shell history, process
#: listings and CI logs, so there is no flag that takes one.
IDENTITY_ENV = "TESSERA_IDENTITY_KEY"


def build_env(key_hex: str | None = None) -> dict:
    """The environment a `tessera build` subprocess runs in: this process's, plus the identity key
    where the caller has one to state. `--mint-id-key` builds pass `None`."""
    env = dict(os.environ)
    env.pop(IDENTITY_ENV, None)
    if key_hex is not None:
        env[IDENTITY_ENV] = key_hex
    return env


def run_build(args: list[str], *, key_hex: str | None = None) -> subprocess.CompletedProcess:
    """Run `tessera build` and hand back the completed process, refusal or not.

    The fixture builders above and in `catalogue.py` run the CLI with `check=True`, because for
    them a failed build is a broken harness. The schema-refusal catalogue is the opposite test:
    the refusal *is* the subject (records §2; decision 0013's naming discipline), so the caller
    asserts on the exit status and the message rather than having them converted into a
    `CalledProcessError`. Output is captured — stderr is where the CLI reports a refusal — and
    text-decoded so a test can grep it for the reason the message must name.
    """
    ensure_cli_built()
    return subprocess.run(
        [str(CLI_BIN), "build", *args],
        cwd=REPO_ROOT,
        env=build_env(key_hex),
        capture_output=True,
        text=True,
        check=False,
    )


def open_bundle_with_source(
    bundle_root: Path, points: Path | str, limit: int | None = None
) -> "object":
    """Open `bundle_root` as an oracle [`Bundle`] with its **source geometry attached**.

    This is the escape `conformance.md` §1's layering rule requires, and the only one: the oracle's
    geometry now comes from the points file the build consumed, and a definitional module may not
    go looking for it. So a driver — this function — resolves the path and hands it in, exactly as
    the fixture already hands in the identity key.

    Every driver that opens a bundle for a geometry comparison must come through here rather than
    calling `Bundle(root)` directly, because a `Bundle` with no source attached refuses to derive
    a code at all. That refusal is deliberate: the alternative, falling back to the stored column,
    is the tautology the third input exists to remove, and it would fire silently exactly when a
    harness forgot to wire the source up.

    `limit` must match the `--limit` the bundle was built with: the corpus is 10⁹ rows and the
    fixture is a prefix of it, so reading the whole file to check a prefix would exhaust the
    machine (`read_source_geometry`).

    **Nothing binds `points` to `bundle_root`.** No digest, no manifest entry — see
    `bundle.SourceGeometry`'s note. Handing in the wrong file is a whole-suite geometry failure
    that reads like an engine bug.
    """
    from .bundle import Bundle, read_source_geometry  # noqa: PLC0415 — avoids an import cycle

    bundle = Bundle(bundle_root)
    bundle.attach_source_geometry(read_source_geometry(points, bundle.extent, limit))
    return bundle


def ensure_fixture_bundle(
    bundle_root: Path,
    *,
    points: str = DEFAULT_POINTS,
    pairs: str = DEFAULT_PAIRS,
    limit: int | None = DEFAULT_LIMIT,
    extent: str = DEFAULT_EXTENT,
    view_id: str = DEFAULT_VIEW,
) -> None:
    """Build a bundle at `bundle_root` via the CLI, if one doesn't already exist there.

    A **pre-r6 bundle at `bundle_root` is rebuilt rather than reused.** Contracts r6 makes
    MANIFEST's `identity` object required — an absent one is a typed reader error, not a
    default — so `tessera serve` correctly refuses a bundle built before r6. Testing for
    `CURRENT` alone would hand every test a bundle the server will not open, and the failure
    surfaces as an opaque fixture-setup error rather than "your fixture is stale".

    The build is given `--mint-id-key` explicitly. r6 requires a build to refuse unless one of
    `--carry-id-key-from` / `--identity-file` / the environment / `--mint-id-key` is named,
    precisely so
    a human decides the key's lineage rather than a tool inventing one silently. A test fixture is
    a genuinely new lineage each time it is built, so minting is the correct answer here — and
    stating it satisfies the rule rather than circumventing it. Note that this makes the fixture's
    `tessera_id`s differ between rebuilds, which is why nothing may persist them across runs.

    `--mint-external-ids` is passed for the same class of reason and was **missing**, which broke
    six tests on any checkout that had to build the fixture fresh (three in `reference/tests`,
    three in `conformance/`, all of them a `KeyError` out of `Bundle.external_id_of`). The flag
    became opt-in on 2026-07-30 (memo §3.2 D1: contracts §2.4 forbids manufacturing an external ID
    for an item whose caller supplied none, and the Phase 0 corpus supplies none) and this builder
    was not updated with it; the suite went on passing only against a `/tmp` fixture built before
    the flip, and began failing when `/tmp` was wiped. Every test that addresses an item over
    `/control/changes` needs an external ID to address it *by*, so this fixture must carry them:
    opt-in in the product, mandatory here.

    Both of those are the same defect twice, and it is the receipt above — not this docstring —
    that closes the class: reuse is decided by comparing the full argument set against the stamp,
    so the next flag added here cannot be forgotten by the reuse test.
    """
    args = _fixture_build_argv(
        bundle_root, points=points, pairs=pairs, limit=limit, extent=extent, view_id=view_id
    )
    wanted = fixture_recipe(args, declaration=_fixture_config_text(view_id, extent))
    if _fixture_bundle_is_usable(bundle_root, wanted):
        return
    if bundle_root.exists():
        print(
            f"fixture at {bundle_root} was not built from this harness's current inputs "
            f"(stamp={read_recipe(bundle_root)}, wanted={wanted}) — rebuilding"
        )
    ensure_cli_built()
    write_recipe(bundle_root, None)
    if bundle_root.exists():
        shutil.rmtree(bundle_root)
    _write_fixture_config(bundle_root, view_id, extent)
    write_deployment(
        _fixture_deployment_path(bundle_root),
        bundle=bundle_root,
        schema=_fixture_config_path(bundle_root),
    )
    subprocess.run(args, cwd=REPO_ROOT, env=build_env(), check=True)
    write_recipe(bundle_root, wanted)


def _fixture_config_path(bundle_root: Path) -> Path:
    """Where this fixture's declaration is written — beside the bundle, since the corpus it names
    lives at absolute paths only `--file` may carry (configuration.md §3, §8)."""
    return bundle_root.parent / f"{bundle_root.name}.config.toml"


def _fixture_deployment_path(bundle_root: Path) -> Path:
    return bundle_root.parent / f"{bundle_root.name}.tessera.toml"


def _extent_toml(extent: str) -> str:
    """`x_min,x_max,y_min,y_max` — the shape every caller here already had — as the view's own
    `extent` key. The frame belongs to the view now, not to the invocation (configuration.md §1)."""
    x_min, x_max, y_min, y_max = (part.strip() for part in extent.split(","))
    return f"extent = {{ x = [{x_min}, {x_max}], y = [{y_min}, {y_max}] }}"


def _fixture_config_text(view_id: str, extent: str) -> str:
    """One view, its frame, its geometry, and the relation its points' labels are in. No
    attributes: this fixture's corpus is the scaled geometry file, which carries none.

    The two sources are named relatively **and overridden on the command line**: the files live
    under `data/scaled/`, which is a path this document may not carry (§3), and an override is
    what a deployment staging a source elsewhere reaches for.

    A pure function of the two things it varies with, because the **declaration is part of the
    recipe**: the extent moved out of the invocation and into this document, so a receipt that
    stamped only the argv would reuse a bundle quantised against a different frame — every stored
    cell wrong, and the bundle well-formed.
    """
    return (
        '[sources]\npoints = "points.parquet"\npairs = "pairs.parquet"\n\n'
        f'[[view]]\nname = "{view_id}"\n{_extent_toml(extent)}\n'
        'source = "points"\n'
        'point_visibility = { source = "pairs", default = "public" }\n'
    )


def _write_fixture_config(bundle_root: Path, view_id: str, extent: str) -> Path:
    path = _fixture_config_path(bundle_root)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(_fixture_config_text(view_id, extent))
    return path


def _fixture_build_argv(
    bundle_root: Path,
    *,
    points: str,
    pairs: str,
    limit: int | None,
    extent: str,
    view_id: str,
) -> list[str]:
    args = [
        str(CLI_BIN),
        "build",
        "--deployment",
        str(_fixture_deployment_path(bundle_root)),
        "--file",
        f"points={points}",
        "--file",
        f"pairs={pairs}",
        "--out",
        str(bundle_root),
    ]
    if limit is not None:
        args += ["--limit", str(limit)]
    args += ["--mint-id-key", "--mint-external-ids"]
    return args


def _input_stamp(argv: list[str]) -> dict:
    """`(size, mtime_ns)` of every input file the build reads, keyed by its `--file` binding.

    **The argv alone is not the recipe.** A binding names a path, and a probe script
    that regenerates a corpus *in place* leaves both paths identical while changing every byte
    behind them — which is not hypothetical: `data/scaled/geometry.parquet` was regenerated on
    2026-08-02 to carry sub-cell residuals and to correct an axis transposition, and an argv-only
    receipt would have handed every test a bundle built from the file that no longer exists,
    reporting a reuse rather than a staleness.

    Size and mtime rather than a digest, deliberately: the points file is 10 GB and this runs at
    the top of every test session, so hashing it would cost more than the rebuild it is protecting
    against. The failure it cannot see — a same-size edit that preserves mtime — is not a shape any
    generator here produces, and the structural gate in `_fixture_bundle_is_usable` is the second
    line against damage a receipt cannot observe.
    """
    stamp = {}
    for i, arg in enumerate(argv):
        if arg != "--file" or i + 1 >= len(argv):
            continue
        key, _, raw = argv[i + 1].partition("=")
        path = Path(raw)
        try:
            st = path.stat()
            stamp[key] = [st.st_size, st.st_mtime_ns]
        except OSError:
            # An input that cannot be stat'd is recorded as absent rather than raised on: the
            # build itself is about to fail on it and will say so far better than this would.
            stamp[key] = None
    return stamp


def fixture_recipe(argv: list[str], *, declaration: str = "") -> dict:
    """The stamped input set for [`ensure_fixture_bundle`]: the whole `tessera build` invocation,
    plus a stamp of the input *files* it names ([`_input_stamp`]).

    Everything this fixture is a function of is an argument to that command, a file that command
    reads, or a line of the **declaration** it reads them through — there is no synthesised corpus
    here, unlike the mask catalogue's. The declaration is stamped whole because the extent lives
    in it now rather than in the invocation, and a bundle quantised against a different frame has
    every stored cell wrong while remaining perfectly well-formed. The binary's path, the `--out`
    path and the `--deployment` path are dropped: none is a property of the fixture, and including
    them would force a rebuild per worktree.

    Note what the recipe cannot pin, and why that is correct: `--mint-id-key` mints a fresh
    identity key per build, so two bundles from an identical recipe have different `tessera_id`s.
    The recipe records the *lineage decision*, not the key. Nothing may persist a `tessera_id` from
    this fixture across runs — the docstring above says so for the same reason.
    """
    argv = argv[1:]
    for flag in ("--out", "--deployment"):
        at = argv.index(flag)
        argv = argv[:at] + argv[at + 2 :]
    return {
        "recipe_version": 3,
        "build_argv": argv,
        "declaration": declaration,
        "inputs": _input_stamp(argv),
    }


def _fixture_bundle_is_usable(bundle_root: Path, wanted: dict) -> bool:
    """The receipt matches, there is a readable bundle under it, and the bundle is still the
    build's own output — not one a previous run's write-path tests have advanced.

    The structural half is a second gate against damage *after* the receipt was written (a wiped
    `/tmp`, a half-deleted tree) — something the receipt cannot see. Deliberately tolerant of an
    unreadable or malformed bundle: anything that cannot be confirmed is treated as needing a
    rebuild. Being wrong in that direction costs a rebuild; being wrong in the other hands every
    test a bundle that is not the one it asked for.

    **The pristine check is the third gate, and it closes a poisoning the receipt structurally
    cannot see.** The bundle root is the deployment root: a suite run's `/control/changes` tests
    ack into a WAL in the run's own tmp dir, but the flushes those acks trigger publish
    `SEGMENTS-N.json` sets — and the deny state they carry — INTO this shared bundle,
    durably. The receipt still matches (same argv, same inputs), so the next run's differential
    is handed a corpus whose visible counts disagree with the oracle's parquet-derived ones by
    exactly the previous run's suppressions and deletes. Measured: a fresh fixture passes the
    differential, the very next invocation fails it, deterministically. A fresh build writes
    exactly one segment set per partition (`SEGMENTS-0.json`); any partition carrying more has
    been served against with writes and is rebuilt.
    """
    if read_recipe(bundle_root) != wanted:
        return False
    try:
        current = json.loads((bundle_root / "CURRENT").read_text())
        prefix_dir = bundle_root / current["prefix"]
        if json.loads((prefix_dir / "MANIFEST.json").read_text()) is None:
            return False
        for partition in (prefix_dir / "partitions").iterdir():
            segment_sets = list(partition.glob("SEGMENTS-*.json"))
            if len(segment_sets) > 1:
                return False
        return True
    except (OSError, KeyError, ValueError):
        return False


class Server:
    """An HTTP-only handle to a running `tessera serve` process."""

    def __init__(
        self,
        viewer_port: int,
        session_port: int,
        control_port: int,
        session_credential: str = SESSION_CREDENTIAL,
        operator_credential: str = OPERATOR_CREDENTIAL,
    ):
        self.viewer_base = f"http://127.0.0.1:{viewer_port}"
        self.session_base = f"http://127.0.0.1:{session_port}"
        self.control_base = f"http://127.0.0.1:{control_port}"
        self.session_credential = session_credential
        self.operator_credential = operator_credential

    def authorise(self, terms: list[str]) -> dict:
        auth_data = base64.b64encode(json.dumps({"terms": terms}).encode()).decode()
        resp = requests.post(
            f"{self.session_base}/session/authorise",
            headers={"Authorization": f"Bearer {self.session_credential}"},
            json={"auth_data": auth_data},
            timeout=10,
        )
        resp.raise_for_status()
        return resp.json()

    def viewport(
        self,
        token: str,
        view_id: str,
        zoom: int,
        bbox,
        k: int | None = None,
        underlay_offset: int | None = None,
        filters: dict | None = None,
    ) -> bytes:
        """Returns the raw framed body (matches the pre-refactor `reference/tests/conftest.py`
        behaviour exactly — the differential suite depends on getting bytes back here)."""
        return self.viewport_response(
            token, view_id, zoom, bbox, k=k, underlay_offset=underlay_offset, filters=filters
        ).content

    def meta(self, token: str) -> dict:
        """`GET /v1/meta`. Carries the §7.2 selection constants (`selection.k_min`,
        `selection.k_max_marks`, `selection.theta_target_marks`) the oracle needs to reproduce the
        served set — it cannot know deployment config any other way."""
        resp = requests.get(
            f"{self.viewer_base}/v1/meta",
            headers={"Authorization": f"Bearer {token}"},
            timeout=10,
        )
        resp.raise_for_status()
        return resp.json()

    def viewport_response(
        self,
        token: str,
        view_id: str,
        zoom: int,
        bbox,
        k: int | None = None,
        underlay_offset: int | None = None,
        filters: dict | None = None,
    ) -> requests.Response:
        """Like `viewport`, but returns the full `requests.Response` — for callers that need
        headers (e.g. `x-tessera-pin`) alongside the body."""
        resp = self.viewport_request(
            token, view_id, zoom, bbox, k=k, underlay_offset=underlay_offset, filters=filters
        )
        resp.raise_for_status()
        return resp

    def viewport_request(
        self,
        token: str,
        view_id: str,
        zoom: int,
        bbox,
        k: int | None = None,
        underlay_offset: int | None = None,
        filters: dict | None = None,
    ) -> requests.Response:
        """[`viewport_response`] without the raise — for tests whose subject *is* the refusal
        (contracts §3.2: an unknown filter column is a `422`, `none_of` is a `422`), where
        `raise_for_status` would convert the assertion target into a harness exception."""
        body = {"view": view_id, "zoom": zoom, "bbox": list(bbox)}
        # `layers` omitted means no layers since contracts r38 (D9); the oracle wants every layer
        # the principal reaches, which is what the I3 containment tests compare against.
        body["layers"] = "all"
        if k is not None:
            body["k"] = k
        if underlay_offset is not None:
            body["underlay_offset"] = underlay_offset
        if filters is not None:
            body["filters"] = filters
        return requests.post(
            f"{self.viewer_base}/v1/viewport",
            headers={"Authorization": f"Bearer {token}"},
            json=body,
            timeout=30,
        )

    def item(self, token: str, handle: int, pin: str | None = None) -> requests.Response:
        body: dict = {}
        if pin is not None:
            body["pin"] = pin
        return requests.post(
            f"{self.viewer_base}/v1/items/{handle}",
            headers={"Authorization": f"Bearer {token}"},
            json=body,
            timeout=10,
        )

    def change(self, external_id_b64: str, op: str, access: str | None = None) -> requests.Response:
        item = {"external_id": external_id_b64, "op": op}
        if access is not None:
            item["access"] = access
        return requests.post(
            f"{self.control_base}/control/changes",
            headers={"Authorization": f"Bearer {self.operator_credential}"},
            json=[item],
            timeout=10,
        )

    def changes(self, items: list[dict]) -> requests.Response:
        return requests.post(
            f"{self.control_base}/control/changes",
            headers={"Authorization": f"Bearer {self.operator_credential}"},
            json=items,
            timeout=10,
        )

    def ingest(self, body: bytes, batch_id: str) -> requests.Response:
        return requests.post(
            f"{self.control_base}/control/ingest",
            headers={
                "Authorization": f"Bearer {self.operator_credential}",
                "x-tessera-batch-id": batch_id,
                "Content-Type": "application/vnd.apache.arrow.stream",
            },
            data=body,
            timeout=10,
        )

    def status(self) -> dict:
        resp = requests.get(
            f"{self.control_base}/control/status",
            headers={"Authorization": f"Bearer {self.operator_credential}"},
            timeout=10,
        )
        resp.raise_for_status()
        return resp.json()

    def flush(self, timeout: float = 60.0) -> None:
        """Ask for a flush and **wait for one to complete** (contracts §3.4).

        The route answers `202` — accepted, not done — so a caller that returned there would be
        asserting against whichever generation happened to be live. The barrier is
        `/control/status`'s own `flushes` counter rising past the value read before the request,
        which is the executor's record of a publication rather than an inference from one.
        """
        before = self.status()["write_executor"]["flush"]["flushes"]
        resp = requests.post(
            f"{self.control_base}/control/flush",
            headers={"Authorization": f"Bearer {self.operator_credential}"},
            timeout=10,
        )
        resp.raise_for_status()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.status()["write_executor"]["flush"]["flushes"] > before:
                return
            time.sleep(0.1)
        raise TimeoutError(f"no flush completed within {timeout}s")

    def compact(self, timeout: float = 300.0) -> None:
        """Ask for a compaction fold and **wait for one to land** (contracts §3.4).

        Same shape as [`flush`] and the same reason for the barrier, over a longer wait: a fold
        re-reads and rewrites the whole corpus. `fold_failures` is watched beside `folds` so a
        refused or failed fold surfaces as its own error rather than as a timeout — the two are
        different diagnoses, and the gates a fold can refuse on (memory, free disc, a poisoned WAL)
        are properties of the machine rather than of the code under test.
        """
        before = self.status()["compaction"]
        resp = requests.post(
            f"{self.control_base}/control/compact",
            headers={"Authorization": f"Bearer {self.operator_credential}"},
            timeout=10,
        )
        resp.raise_for_status()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            now = self.status()["compaction"]
            if now["fold_failures"] > before["fold_failures"]:
                raise RuntimeError(f"the compaction fold failed: {now}")
            if now["folds"] > before["folds"]:
                return
            time.sleep(0.2)
        raise TimeoutError(f"no compaction fold landed within {timeout}s")


def write_config(
    tmp_dir: Path,
    bundle_root: Path,
    cache_dir: Path,
    wal_path: Path,
    viewer_port: int,
    session_port: int,
    control_port: int,
    *,
    theta_target_marks: int | None = None,
    k_min: int = 2,
    k_max_marks: int | None = None,
    max_k: int | None = None,
    max_underlay_cells: int | None = None,
) -> Path:
    """Write a `tessera.toml`.

    `theta_target_marks` defaults to a value large enough to **saturate** theta, which turns §7.2's
    selection into "serve every visible row up to the cap". Suites that assert masking or wire shape
    want that: with theta live, every point-set assertion would also depend on the density rule's
    threshold clause, so a masking bug and a theta bug would be indistinguishable. Pass a real value
    to exercise density deliberately.
    """
    if theta_target_marks is None:
        theta_target_marks = 1 << 40
    # Both caps default well above any harness fixture, for the same reason as theta: a suite
    # asserting masking or wire shape should not have its point sets silently truncated by a cap
    # it did not choose. Suites that mean to exercise a cap pass one.
    if k_max_marks is None:
        k_max_marks = 1_000_000
    if max_k is None:
        max_k = 1_000_000
    # Left at the server's own default unless a caller asks. The §3.3 underlay multiplies the tile
    # count by 4^offset, and the deployment default (8192 cells) refuses a wide bbox at a deep zoom
    # with a 422 rather than truncating — correct for a deployment, and a suite that means to sweep
    # the underlay across a zoom range has to raise it deliberately rather than discover it as an
    # unexplained 422.
    config_text = f"""
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
max_k = {max_k}
k_min = {k_min}
k_max_marks = {k_max_marks}
theta_target_marks = {theta_target_marks}
"""
    if max_underlay_cells is not None:
        config_text += f"max_underlay_cells = {max_underlay_cells}\n"
    config_path = tmp_dir / "tessera.toml"
    config_path.write_text(config_text)
    return config_path


def spawn_server(
    bundle_root: Path,
    tmp_dir: Path,
    *,
    cache_dir: Path | None = None,
    wal_path: Path | None = None,
    log_path: Path | None = None,
    env_extra: dict[str, str] | None = None,
    max_k: int | None = None,
    k_max_marks: int | None = None,
    theta_target_marks: int | None = None,
    max_underlay_cells: int | None = None,
) -> tuple[Server, subprocess.Popen]:
    """Start `tessera serve` against `bundle_root`, using `cache_dir`/`wal_path` (defaulting to
    `tmp_dir/cache`, `tmp_dir/wal.log`) for its durable state. Passing the SAME `cache_dir`/
    `wal_path` across two calls is exactly the restart-on-same-state case
    (`test_restart_replay.py`).

    If `log_path` is given, the child's stdout+stderr are redirected to that file (so a caller
    can scan the full log after the process is killed, without risking a filled pipe buffer
    stalling the child mid-test — `test_byte_scan.py`'s log-scan needs this). Otherwise stdout is
    piped in-process (matching the original `reference/tests/conftest.py` behaviour, which only
    ever reads that pipe on an early-exit failure).
    """
    ensure_cli_built()

    cache_dir = cache_dir if cache_dir is not None else (tmp_dir / "cache")
    wal_path = wal_path if wal_path is not None else (tmp_dir / "wal.log")

    viewer_port = free_port()
    session_port = free_port()
    control_port = free_port()

    config_path = write_config(
        tmp_dir,
        bundle_root,
        cache_dir,
        wal_path,
        viewer_port,
        session_port,
        control_port,
        max_k=max_k,
        k_max_marks=k_max_marks,
        theta_target_marks=theta_target_marks,
        max_underlay_cells=max_underlay_cells,
    )

    env = os.environ.copy()
    env["TESSERA_REFERENCE_SESSION_CRED"] = SESSION_CREDENTIAL
    env["TESSERA_REFERENCE_OPERATOR_CRED"] = OPERATOR_CREDENTIAL
    if env_extra:
        env.update(env_extra)

    if log_path is not None:
        log_file = open(log_path, "ab")
        stdout_target = log_file
        stderr_target = subprocess.STDOUT
    else:
        stdout_target = subprocess.PIPE
        stderr_target = subprocess.STDOUT

    proc = subprocess.Popen(
        # `--deployment`, not `-c`: the configuration rework made `tessera.toml` the one document
        # both entry points read, and `serve` takes the same flag `build` does
        # (`configuration.md` §3). The old spelling was refused at argument parsing, so every
        # spawn here failed at startup rather than in a test's own assertion.
        [str(CLI_BIN), "serve", "--deployment", str(config_path)],
        cwd=REPO_ROOT,
        env=env,
        stdout=stdout_target,
        stderr=stderr_target,
    )
    if log_path is not None:
        log_file.close()  # the child inherited the fd; our handle can close now

    srv = Server(viewer_port, session_port, control_port)

    deadline = time.monotonic() + 20.0
    up = False
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            extra = ""
            if proc.stdout is not None:
                extra = proc.stdout.read().decode(errors="replace")
            elif log_path is not None:
                extra = log_path.read_text(errors="replace")
            raise RuntimeError(f"tessera serve exited early ({proc.returncode}):\n{extra}")
        try:
            resp = requests.get(f"{srv.viewer_base}/healthz", timeout=1)
            if resp.status_code == 200:
                up = True
                break
        except requests.exceptions.ConnectionError:
            pass
        time.sleep(0.1)

    if not up:
        proc.terminate()
        raise RuntimeError("tessera serve did not become healthy within 20s")

    return srv, proc


def stop_server(proc: subprocess.Popen) -> None:
    """Graceful stop (SIGTERM, then SIGKILL if it doesn't exit) — used for normal teardown."""
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def kill_server(proc: subprocess.Popen) -> None:
    """SIGKILL — no graceful shutdown, no chance to flush anything not already fsynced. This is
    the restart-replay test's crash simulation: whatever the WAL doesn't already have durably is
    allowed to be lost, but nothing it does have may reappear as anything other than what it was
    acked as."""
    os.kill(proc.pid, signal.SIGKILL)
    proc.wait(timeout=10)
