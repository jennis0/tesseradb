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
DEFAULT_SLICE = "s0"


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


def ensure_fixture_bundle(
    bundle_root: Path,
    *,
    points: str = DEFAULT_POINTS,
    pairs: str = DEFAULT_PAIRS,
    limit: int | None = DEFAULT_LIMIT,
    extent: str = DEFAULT_EXTENT,
    slice_id: str = DEFAULT_SLICE,
) -> None:
    """Build a bundle at `bundle_root` via the CLI, if one doesn't already exist there.

    A **pre-r6 bundle at `bundle_root` is rebuilt rather than reused.** Contracts r6 makes
    MANIFEST's `identity` object required — an absent one is a typed reader error, not a
    default — so `tessera serve` correctly refuses a bundle built before r6. Testing for
    `CURRENT` alone would hand every test a bundle the server will not open, and the failure
    surfaces as an opaque fixture-setup error rather than "your fixture is stale".

    The build is given `--mint-id-key` explicitly. r6 requires a build to refuse unless one of
    `--carry-id-key-from` / `--id-key-file` / `--id-key` / `--mint-id-key` is named, precisely so
    a human decides the key's lineage rather than a tool inventing one silently. A test fixture is
    a genuinely new lineage each time it is built, so minting is the correct answer here — and
    stating it satisfies the rule rather than circumventing it. Note that this makes the fixture's
    `tessera_id`s differ between rebuilds, which is why nothing may persist them across runs.
    """
    if (bundle_root / "CURRENT").exists():
        if _bundle_has_identity(bundle_root):
            return
        print(f"fixture at {bundle_root} predates contracts r6 (no MANIFEST identity) — rebuilding")
        shutil.rmtree(bundle_root)
    ensure_cli_built()
    args = [
        str(CLI_BIN),
        "build",
        "--points",
        points,
        "--pairs",
        pairs,
        "--extent",
        extent,
        "--slice",
        slice_id,
        "--out",
        str(bundle_root),
    ]
    if limit is not None:
        args += ["--limit", str(limit)]
    args += ["--mint-id-key"]
    subprocess.run(args, cwd=REPO_ROOT, check=True)


def _bundle_has_identity(bundle_root: Path) -> bool:
    """True if `bundle_root`'s manifest carries the r6-required `identity` object.

    Deliberately tolerant of an unreadable or malformed bundle: anything we cannot confirm as
    r6-shaped is treated as needing a rebuild. Being wrong in that direction costs a rebuild;
    being wrong in the other hands every test a bundle the server refuses to open.
    """
    try:
        current = json.loads((bundle_root / "CURRENT").read_text())
        manifest_path = bundle_root / current["prefix"] / "MANIFEST.json"
        return "identity" in json.loads(manifest_path.read_text())
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

    def viewport(self, token: str, slice_id: str, zoom: int, bbox, k: int | None = None) -> bytes:
        """Returns the raw framed body (matches the pre-refactor `reference/tests/conftest.py`
        behaviour exactly — the differential suite depends on getting bytes back here)."""
        return self.viewport_response(token, slice_id, zoom, bbox, k=k).content

    def viewport_response(
        self, token: str, slice_id: str, zoom: int, bbox, k: int | None = None
    ) -> requests.Response:
        """Like `viewport`, but returns the full `requests.Response` — for callers that need
        headers (e.g. `x-tessera-pin`) alongside the body."""
        body = {"slice": slice_id, "zoom": zoom, "bbox": list(bbox)}
        if k is not None:
            body["k"] = k
        resp = requests.post(
            f"{self.viewer_base}/v1/viewport",
            headers={"Authorization": f"Bearer {token}"},
            json=body,
            timeout=30,
        )
        resp.raise_for_status()
        return resp

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


def write_config(
    tmp_dir: Path,
    bundle_root: Path,
    cache_dir: Path,
    wal_path: Path,
    viewer_port: int,
    session_port: int,
    control_port: int,
) -> Path:
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
"""
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
        tmp_dir, bundle_root, cache_dir, wal_path, viewer_port, session_port, control_port
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
        [str(CLI_BIN), "serve", "-c", str(config_path)],
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
