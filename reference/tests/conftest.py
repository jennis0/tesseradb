"""Spawns a real `tessera serve` process against the /tmp/tessera-250k fixture bundle and gives
tests an HTTP-only handle to it.

This is the "server spawned via a fixture" the task brief calls for: the harness runs the actual
`tessera` binary (built via `cargo build --release -p tessera-cli`), not an in-process shortcut,
and talks to it only over HTTP/the filesystem — never by calling into `tessera-engine` directly
(Python is a consumer, never a component).
"""

from __future__ import annotations

import os
import socket
import subprocess
import time
from pathlib import Path

import pytest
import requests

REPO_ROOT = Path(__file__).resolve().parents[2]
BUNDLE_ROOT = Path("/tmp/tessera-250k")
CLI_BIN = REPO_ROOT / "target" / "release" / "tessera"

SESSION_CREDENTIAL = "reference-oracle-session-secret"
OPERATOR_CREDENTIAL = "reference-oracle-operator-secret"


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _ensure_cli_built() -> None:
    if CLI_BIN.exists():
        return
    subprocess.run(
        ["cargo", "build", "--release", "-p", "tessera-cli"],
        cwd=REPO_ROOT,
        check=True,
    )


def _ensure_fixture_bundle() -> None:
    if (BUNDLE_ROOT / "CURRENT").exists():
        return
    _ensure_cli_built()
    subprocess.run(
        [
            str(CLI_BIN),
            "build",
            "--points",
            "data/scaled/geometry.parquet",
            "--pairs",
            "data/scaled/pairs/categories-subclass.pairs.parquet",
            "--limit",
            "250000",
            "--extent",
            "0,65536,0,65536",
            "--slice",
            "s0",
            "--out",
            str(BUNDLE_ROOT),
        ],
        cwd=REPO_ROOT,
        check=True,
    )


class Server:
    def __init__(self, viewer_port: int, session_port: int, control_port: int):
        self.viewer_base = f"http://127.0.0.1:{viewer_port}"
        self.session_base = f"http://127.0.0.1:{session_port}"
        self.control_base = f"http://127.0.0.1:{control_port}"
        self.session_credential = SESSION_CREDENTIAL
        self.operator_credential = OPERATOR_CREDENTIAL

    def authorise(self, terms: list[str]) -> dict:
        import base64
        import json

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
        return resp.content

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


@pytest.fixture(scope="session")
def bundle_root() -> Path:
    _ensure_fixture_bundle()
    return BUNDLE_ROOT


@pytest.fixture(scope="session")
def server(tmp_path_factory, bundle_root):
    _ensure_cli_built()

    tmp_dir = tmp_path_factory.mktemp("tessera-serve")
    cache_dir = tmp_dir / "cache"
    wal_path = tmp_dir / "wal.log"

    viewer_port = _free_port()
    session_port = _free_port()
    control_port = _free_port()

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

    env = os.environ.copy()
    env["TESSERA_REFERENCE_SESSION_CRED"] = SESSION_CREDENTIAL
    env["TESSERA_REFERENCE_OPERATOR_CRED"] = OPERATOR_CREDENTIAL

    proc = subprocess.Popen(
        [str(CLI_BIN), "serve", "-c", str(config_path)],
        cwd=REPO_ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    srv = Server(viewer_port, session_port, control_port)

    deadline = time.monotonic() + 20.0
    up = False
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            out = proc.stdout.read().decode(errors="replace") if proc.stdout else ""
            raise RuntimeError(f"tessera serve exited early ({proc.returncode}):\n{out}")
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

    yield srv

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)
