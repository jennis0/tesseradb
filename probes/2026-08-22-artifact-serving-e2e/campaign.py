"""The Stage 7 campaign's driver: a bundle, a server, principals, and the artifact channel.

**Everything here goes through the real request path.** The scale investigation
(`probes/2026-08-20-artifact-serving-scale/`) measured the design probe-side, in a bench binary
that owned its own control flow; this drives `tessera serve` over HTTP, decodes the wire frames a
client decodes, and compares the served artifact counts against `tessera corpus artifact-census`.
The point of the exercise is that nothing between the socket and the store is stubbed.

Offline measurement tooling is where Python is allowed (CLAUDE.md): this is a consumer of the
server, never a component of it, and it reads the wire with `reference/oracle/wire.py` — the
independently written decoder the conformance suite already trusts — rather than a second copy.
"""

from __future__ import annotations

import base64
import json
import os
import socket
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

import requests

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "reference"))

from oracle.wire import decode_frames  # noqa: E402

CLI = REPO_ROOT / "target" / "release" / "tessera"
SESSION_CREDENTIAL = "campaign-session-credential"
OPERATOR_CREDENTIAL = "campaign-operator-credential"
IDENTITY_KEY = "000102030405060708090a0b0c0d0e0f"

#: The generator's quantisation extent — `GRID_EXTENT`, the cell grid's own coordinates. Every
#: viewport below is expressed in it, so a bbox here and a census tile there mean the same region.
GRID = 65536.0

#: The layers the generator's declaration carries, by the shape each one exercises. `treed` is the
#: `nested` lineage; `boundary` is added by `shape_layer_toml` because the generator's roster
#: carries prefixes and no geometry.
LAYER_FLAT = "generator/flat"
LAYER_PARTITION_ENUM = "generator/partition-enumerated"
LAYER_PARTITION_ATTR = "generator/partition-attribute"
LAYER_TREED = "generator/treed"
LAYER_BOUNDARY = "campaign/boundary"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def run(argv: list[str], cwd: Path | None = None, env: dict | None = None) -> str:
    """A checked subprocess whose failure carries its own output — a silent non-zero here would
    otherwise surface as a confusing decode error two steps later."""
    full = os.environ.copy()
    full["TESSERA_IDENTITY_KEY"] = IDENTITY_KEY
    if env:
        full.update(env)
    proc = subprocess.run(argv, cwd=cwd, env=full, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(
            f"{' '.join(str(a) for a in argv)} exited {proc.returncode}\n"
            f"--- stdout ---\n{proc.stdout}\n--- stderr ---\n{proc.stderr}"
        )
    return proc.stdout


def free_bytes(path: Path) -> int:
    st = os.statvfs(path)
    return st.f_bavail * st.f_frsize


#: The campaign's disk floor. Every step that writes at scale checks it first and stops rather
#: than filling the disk — a fixture that dies half-written costs the tier twice.
DISK_FLOOR_BYTES = 10 * 1024**3


def require_disk(path: Path, want: int = DISK_FLOOR_BYTES) -> None:
    have = free_bytes(path)
    if have < want:
        raise RuntimeError(
            f"{path}: {have / 1024**3:.1f} GB free, below the {want / 1024**3:.1f} GB floor — "
            "stopping rather than filling the disk"
        )


# ---------------------------------------------------------------------------- fixtures


def materialise(work: Path, seed: int, n: int, terms_per_level: int) -> dict:
    """`tessera corpus materialise` at one tier, with the wall clock and peak RSS recorded."""
    fixture = work / "fixture"
    require_disk(work)
    started = time.monotonic()
    out = run(
        [
            "/usr/bin/time",
            "-f",
            "%e %M",
            str(CLI),
            "corpus",
            "materialise",
            "--seed",
            str(seed),
            "--n",
            str(n),
            "--out",
            str(fixture),
            "--terms-per-level",
            str(terms_per_level),
        ]
    )
    return {"seconds": time.monotonic() - started, "stdout": out, "dir": str(fixture)}


DEPLOYMENT_TOML = """
[bundle]
path  = "{bundle}"
cache = "{cache}"
wal   = "{wal}"

[build]
schema = "{schema}"

[identity]
env = "TESSERA_IDENTITY_KEY"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:{viewer}"
session = "127.0.0.1:{session}"
control = "127.0.0.1:{control}"
session_credential_env  = "TESSERA_CAMPAIGN_SESSION_CRED"
operator_credential_env = "TESSERA_CAMPAIGN_OPERATOR_CRED"
max_k = 1000000
k_max_marks = 1000000
theta_target_marks = 1099511627776

[ingest]
flush_max_age_secs = 86400
compaction_window_start = "off"
"""


def write_deployment(work: Path, ports: tuple[int, int, int]) -> Path:
    viewer, session, control = ports
    path = work / "tessera.toml"
    # The WAL's directory has to exist before `serve` opens it — a missing parent is an io error
    # at boot, not a created path.
    (work / ".tessera" / "cache").mkdir(parents=True, exist_ok=True)
    path.write_text(
        DEPLOYMENT_TOML.format(
            bundle=work / "bundle",
            cache=work / ".tessera" / "cache",
            wal=work / ".tessera" / "wal.log",
            schema=work / "fixture" / "campaign-config.toml",
            viewer=viewer,
            session=session,
            control=control,
        )
    )
    return path


def build(work: Path) -> dict:
    """`tessera build` over the campaign declaration, wall clock and peak RSS recorded."""
    require_disk(work)
    started = time.monotonic()
    out = run(
        ["/usr/bin/time", "-f", "BUILD_TIME %e %M", str(CLI), "build", "--deployment", str(work / "tessera.toml")],
        cwd=work,
    )
    return {"seconds": time.monotonic() - started, "stdout": out}


# ---------------------------------------------------------------------------- the server


@dataclass
class Server:
    work: Path
    viewer: int
    session: int
    control: int
    proc: subprocess.Popen | None = None
    log: Path | None = None

    @property
    def viewer_base(self) -> str:
        return f"http://127.0.0.1:{self.viewer}"

    @property
    def session_base(self) -> str:
        return f"http://127.0.0.1:{self.session}"

    @property
    def control_base(self) -> str:
        return f"http://127.0.0.1:{self.control}"

    def spawn(self, timeout: float = 600.0) -> None:
        env = os.environ.copy()
        env["TESSERA_IDENTITY_KEY"] = IDENTITY_KEY
        env["TESSERA_CAMPAIGN_SESSION_CRED"] = SESSION_CREDENTIAL
        env["TESSERA_CAMPAIGN_OPERATOR_CRED"] = OPERATOR_CREDENTIAL
        self.log = self.work / "server.log"
        handle = self.log.open("ab")
        self.proc = subprocess.Popen(
            [str(CLI), "serve", "--deployment", str(self.work / "tessera.toml")],
            cwd=self.work,
            env=env,
            stdout=handle,
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                tail = self.log.read_bytes()[-8192:].decode(errors="replace")
                raise RuntimeError(f"tessera serve exited {self.proc.returncode}:\n{tail}")
            try:
                if requests.get(f"{self.viewer_base}/healthz", timeout=2).status_code == 200:
                    return
            except requests.exceptions.ConnectionError:
                pass
            time.sleep(0.2)
        self.proc.terminate()
        raise RuntimeError("tessera serve did not become healthy in time")

    def stop(self) -> None:
        if self.proc is not None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=30)
            self.proc = None

    def rss_bytes(self) -> int:
        """The server's resident set, read from `/proc` — the campaign's memory figure, taken from
        outside the process so it costs the measurement nothing."""
        if self.proc is None:
            return 0
        try:
            with open(f"/proc/{self.proc.pid}/statm") as f:
                return int(f.read().split()[1]) * os.sysconf("SC_PAGE_SIZE")
        except (FileNotFoundError, ProcessLookupError, IndexError):
            return 0

    def status(self) -> dict:
        r = requests.get(
            f"{self.control_base}/control/status",
            headers={"Authorization": f"Bearer {OPERATOR_CREDENTIAL}"},
            timeout=30,
        )
        r.raise_for_status()
        return r.json()

    def authorise(self, terms: list[str], timeout: float = 900.0) -> tuple[str, float]:
        """One principal's session. Returns `(token, seconds)` — the establishment cost is a
        campaign figure in its own right, because at a million terms a broad grant's `M_auth` is
        a union of a hundred thousand posting lists and the request path never pays for it."""
        payload = base64.b64encode(json.dumps({"terms": terms}).encode()).decode()
        started = time.monotonic()
        r = requests.post(
            f"{self.session_base}/session/authorise",
            headers={"Authorization": f"Bearer {SESSION_CREDENTIAL}"},
            json={"auth_data": payload},
            timeout=timeout,
        )
        elapsed = time.monotonic() - started
        r.raise_for_status()
        return r.json()["token"], elapsed

    def meta(self, token: str) -> dict:
        r = requests.get(
            f"{self.viewer_base}/v1/meta",
            headers={"Authorization": f"Bearer {token}"},
            timeout=120,
        )
        r.raise_for_status()
        return r.json()


@dataclass
class Viewport:
    """One request's viewport, named by the fraction of the grid it covers."""

    name: str
    bbox: list[float]
    zoom: int
    fraction: float = field(default=0.0)


def viewports() -> list[Viewport]:
    """The seven zooms of the design's §7 grid, as fractions of the whole map.

    Each is a centred square of side `sqrt(fraction) * GRID`, at the zoom whose tiles are a few
    hundred across the viewport — the same ladder the probe swept, expressed as a bbox because
    that is what a client sends.
    """
    ladder = [
        ("100%", 1.0, 0),
        ("75%", 0.75, 1),
        ("50%", 0.5, 1),
        ("25%", 0.25, 2),
        ("6.25%", 0.0625, 3),
        ("0.39%", 0.0039062, 5),
        ("0.024%", 0.00024414, 7),
    ]
    out = []
    for name, fraction, zoom in ladder:
        side = GRID * (fraction**0.5)
        lo = (GRID - side) / 2.0
        hi = lo + side
        out.append(Viewport(name, [lo, lo, hi, hi], zoom, fraction))
    return out


def viewport_request(
    server: Server,
    token: str,
    vp: Viewport,
    layers: list[str] | None,
    k: int = 0,
    artifact_budget: int | None = None,
    timeout: float = 900.0,
) -> tuple[float, int, list, dict]:
    """One `/v1/viewport`. Returns `(seconds, body_bytes, artifacts, trailer)`.

    `k = 0` asks for no points: the campaign measures the **artifact** channel, and a wide viewport
    that also gathered a million points would report the gather's cost as the artifact route's.
    Where the point channel is wanted it is asked for explicitly.
    """
    body = {"view": "s0", "zoom": vp.zoom, "bbox": vp.bbox, "k": k}
    if layers is not None:
        body["layers"] = layers
    if artifact_budget is not None:
        body["artifact_budget"] = artifact_budget
    started = time.monotonic()
    r = requests.post(
        f"{server.viewer_base}/v1/viewport",
        headers={"Authorization": f"Bearer {token}"},
        json=body,
        timeout=timeout,
    )
    raw = r.content
    elapsed = time.monotonic() - started
    if r.status_code != 200:
        raise RuntimeError(f"/v1/viewport {r.status_code}: {raw[:500]!r}")
    _tiles, _points, _sub, artifacts, trailer = decode_frames(raw)
    return elapsed, len(raw), artifacts or [], trailer


# ---------------------------------------------------------------------------- the oracle


CENSUS_BIN = REPO_ROOT / "target" / "release" / "artifact_campaign_census"


def artifact_census(
    seed: int, n: int, arm: str, grant: str, terms_per_level: int, scratch: Path
) -> dict[int, int]:
    """The generator's closed-form census as a `{artifact ordinal: masked count}` map.

    **Through `artifact_campaign_census`, not `tessera corpus artifact-census`**, and the reason is
    the process boundary rather than the census: the verb takes its principal as one argv string,
    and Linux caps a single argument at 128 KB, so every grant above about 13 000 terms is
    `Argument list too long`. The campaign's broad principals hold 131 072. The bench arm reads the
    grant from a file and calls the same `tessera-corpus` methods the verb calls.
    """
    import pyarrow.ipc as ipc

    scratch.mkdir(parents=True, exist_ok=True)
    grant_file = scratch / "grant.txt"
    grant_file.write_text(grant)
    out_path = scratch / f"census-{arm}.arrow"
    proc = subprocess.run(
        [
            str(CENSUS_BIN),
            "--seed", str(seed),
            "--n", str(n),
            "--terms-per-level", str(terms_per_level),
            "--arm", arm,
            "--grant-file", str(grant_file),
            "--out", str(out_path),
        ],
        capture_output=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"census exited {proc.returncode}: {proc.stderr.decode()[:800]}")
    out: dict[int, int] = {}
    with open(out_path, "rb") as handle:
        reader = ipc.open_stream(handle)
        for batch in reader:
            for artifact, count in zip(batch.column(0).to_pylist(), batch.column(1).to_pylist()):
                out[artifact] = count
    out_path.unlink(missing_ok=True)
    return out
