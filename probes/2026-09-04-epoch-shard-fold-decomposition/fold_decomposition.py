"""Decompose one compaction fold's total cost on the rung-3 all-in bundle (MedCPT, 36M points).

Serves a copy of the bundle, authorises three grants of different coverage, warms each with one
zoom-0 all-layers viewport, ingests one batch of rows carrying `mesh/descriptors` keys, deletes a
hundred of them, flushes, folds, and then repeats the viewports. Every step is timed with the
server's own figure where the API gives one (`x-tessera-server-us`, `/control/status`'s staircase)
and wall time otherwise. The server's resident set is sampled at every step and once a second
during the fold.

Usage: fold_decomposition.py <scratch dir> <output dir>

`TESSERA_BUNDLE` names the bundle to serve. It must be a copy: a fold writes a new prefix into
it and reclaims the old one.
"""

import base64
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, "/home/joe/code/tessera/test_corpora")
import pyarrow as pa  # noqa: E402
import pyarrow.parquet as pq  # noqa: E402
import requests  # noqa: E402
from pyarrow import ipc  # noqa: E402

from common import serve_battery  # noqa: E402
from common.deployment import Deployment  # noqa: E402
from common.ingest_cycle import Control, wait_for  # noqa: E402

R = Path("/home/joe/code/tessera")
RUNG = R / "data/ladder/medcpt"
BUNDLE = Path(
    os.environ.get("TESSERA_BUNDLE", R / "data/ladder/.measure/medcpt36-foldprobe/bundle")
)
BINARY = Path(os.environ.get("TESSERA_BIN", R / "target/release/tessera"))
PORTS = (8221, 8222, 8223)
LAYER = "mesh/descriptors"
BATCH_ROWS = 10_000
DELETE_ROWS = 100
MIN_AVAILABLE_GIB = 16.0
FOLD_TIMEOUT_S = 3600.0
REQUEST_TIMEOUT_S = 1200.0
# The rung's row count, from its build log. The measured figure is read back from `/control/status`.
TOTAL_ROWS = 35_920_666
LADDER_TARGETS = {"head": 1.0, "mid": 0.1, "tail": 0.001}

if "medcpt36/allin" in str(BUNDLE):
    sys.exit("refusing to serve the original all-in bundle: a fold writes into it")

scratch = Path(sys.argv[1])
out_dir = Path(sys.argv[2])
scratch.mkdir(parents=True, exist_ok=True)
out_dir.mkdir(parents=True, exist_ok=True)
out: dict = {"bundle": str(BUNDLE), "binary": str(BINARY), "ports": PORTS, "steps": {}}


def now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds")


def log(message: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {message}", flush=True)


def save() -> None:
    (out_dir / "result.json").write_text(json.dumps(out, indent=1))


def available_gib() -> float:
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemAvailable:"):
            return int(line.split()[1]) / (1 << 20)
    return float("nan")


def rss(pid: int) -> dict:
    fields = {}
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            key, _, value = line.partition(":")
            if key in ("VmRSS", "RssAnon", "RssFile", "VmHWM"):
                fields[key] = int(value.split()[0]) * 1024
    except OSError:
        pass
    return fields


def require_memory(before: str) -> None:
    gib = available_gib()
    out["steps"][f"available_gib_before_{before}"] = round(gib, 1)
    if gib < MIN_AVAILABLE_GIB:
        log(f"MemAvailable is {gib:.1f} GiB before the {before}; stopping")
        out["stopped"] = f"MemAvailable {gib:.1f} GiB before the {before}"
        save()
        raise SystemExit(2)


class NicedDeployment(Deployment):
    """The harness's deployment, with the server under `nice -n 10` and a long stream deadline.

    The deadline: `serve.stream_deadline_ms` defaults to 60 s, and a request that pays a rebuild
    longer than that is shed mid-body with no server time reported. The rebuild is what this
    probe measures, so the deadline is raised until every request finishes and reports.
    """

    def _write_toml(self) -> None:
        super()._write_toml()
        with open(self.toml, "a") as handle:
            handle.write(f"stream_deadline_ms = {int(REQUEST_TIMEOUT_S * 1000)}\n")

    def start(self, log_path: Path | None = None, timeout: float = 900.0) -> None:
        log_path = log_path or (self.scratch / "serve.log")
        command = [
            "systemd-run",
            "--user",
            "--scope",
            "--collect",
            "--quiet",
            "-p",
            "MemorySwapMax=0",
            "--",
            "nice",
            "-n",
            "10",
            str(self.binary),
            "serve",
            "--deployment",
            str(self.toml),
        ]
        self.log_path = log_path
        with open(log_path, "wb") as handle:
            self.proc = subprocess.Popen(
                command,
                stdout=handle,
                stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL,
                env=self.env,
                start_new_session=True,
            )
        self._wait_ready(timeout)
        self.pid = self._served_pid()
        self.cgroup = self._cgroup_of(self.pid)


def probe_batch(points: pa.Table, external_ids: list[str], layer: str, keys: list[str]) -> bytes:
    """An ingest batch of `points` rows, each carrying `keys` in `layer`'s membership column.

    The same wire form the growth-trigger probe's one-row batch used: the passthrough plugin's
    `access` descriptor list, the external id as bytes, the point attributes, and one list column
    named for the layer.
    """
    n = points.num_rows
    branches = points.column("branches").to_pylist()
    arrays = [
        points.column("x").cast(pa.float64()).combine_chunks(),
        points.column("y").cast(pa.float64()).combine_chunks(),
        pa.array([",".join(b) for b in branches], pa.string()),
        pa.array([e.encode() for e in external_ids], pa.binary()),
        points.column("published").combine_chunks(),
        pa.array(points.column("title").to_pylist(), pa.string()),
        pa.array(points.column("mesh_major").to_pylist(), pa.string()),
        pa.array(external_ids, pa.string()),
        pa.array([list(keys)] * n, pa.list_(pa.string())),
    ]
    names = ["x", "y", "access", "external_id", "published", "title", "mesh_major", "pmid", layer]
    batch = pa.RecordBatch.from_arrays(arrays, names=names)
    sink = pa.BufferOutputStream()
    with ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


class RssSampler:
    """Sample the server's resident set once a second on a thread, for the fold's flight."""

    def __init__(self, pid: int):
        self.pid = pid
        self.samples: list[dict] = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self) -> None:
        t0 = time.perf_counter()
        while not self._stop.is_set():
            sample = rss(self.pid)
            sample["t_s"] = round(time.perf_counter() - t0, 1)
            self.samples.append(sample)
            self._stop.wait(1.0)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._stop.set()
        self._thread.join(timeout=5)
        return False

    def peak(self) -> dict:
        if not self.samples:
            return {}
        return {
            "peak_rss_bytes": max(s.get("VmRSS", 0) for s in self.samples),
            "peak_anon_bytes": max(s.get("RssAnon", 0) for s in self.samples),
            "samples": len(self.samples),
        }


# ---------------------------------------------------------------------------------------------
# The run
# ---------------------------------------------------------------------------------------------

require_memory("serve")
d = NicedDeployment(RUNG, BUNDLE, scratch, PORTS, BINARY)
d.clear_scratch()
out["started_at"] = now()
t0 = time.perf_counter()
d.start()
out["steps"]["open"] = {"wall_s": round(time.perf_counter() - t0, 2), "at": now(), "rss": rss(d.pid)}
(scratch / "serve.pid").write_text(str(d.pid))
log(f"served pid={d.pid} open={out['steps']['open']['wall_s']} s")
save()

try:
    control = Control(d.control, d.credential("operator"))
    cred = d.credential("session")
    status0 = control.status()
    out["live_rows_at_open"] = status0["compaction"]["live_rows"]

    ranks = json.loads((RUNG / "branch-ranks.json").read_text())
    ladder = {
        name: rung
        for name, rung in zip(
            LADDER_TARGETS,
            serve_battery.compose_ladder(ranks, TOTAL_ROWS, list(LADDER_TARGETS.values())),
        )
    }
    out["grants"] = {name: {"target": r["target"], "terms": r["terms"]} for name, r in ladder.items()}

    # ---- 2. authorise -------------------------------------------------------------------------
    tokens: dict[str, str] = {}
    out["steps"]["authorise"] = {}
    for name, rung in ladder.items():
        token, dt = serve_battery.authorise(d.session, cred, rung["terms"], timeout=600)
        tokens[name] = token
        out["steps"]["authorise"][name] = {"wall_s": round(dt, 3), "at": now()}
        log(f"authorise {name} ({len(rung['terms'])} terms): {dt:.3f} s")
    save()

    m = serve_battery.meta(d.viewer, tokens["head"])
    view = m["views"][0]["id"]
    q = m["views"][0]["quantisation"]
    full = [q["x_min"], q["y_min"], q["x_max"], q["y_max"]]
    # A depth-8 viewport over one 256th of the extent in each axis, at the centre.
    cx, cy = (q["x_min"] + q["x_max"]) / 2, (q["y_min"] + q["y_max"]) / 2
    wx, wy = (q["x_max"] - q["x_min"]) / 256, (q["y_max"] - q["y_min"]) / 256
    small = [cx - wx / 2, cy - wy / 2, cx + wx / 2, cy + wy / 2]
    out["view"] = view
    out["viewports"] = {"zoom0": full, "zoom8": small}

    def viewport(step: str, name: str, zoom: int, bbox: list[float]) -> dict:
        """One `layers: "all"` viewport under the grant's existing token, retried on 429."""
        token = tokens[name]
        retries = 0
        t0 = time.perf_counter()
        while True:
            try:
                s = serve_battery.viewport(
                    d.viewer, token, view, zoom, bbox, k=1, layers="all", timeout=REQUEST_TIMEOUT_S
                )
                r = {
                    "served": True,
                    "wall_s": round(time.perf_counter() - t0, 2),
                    "server_ms": s["server_ms"],
                    "visible": (s["counts"] or {}).get("visible"),
                    "bytes": s["bytes"],
                }
                break
            except requests.HTTPError as e:
                if e.response is not None and e.response.status_code == 429 and retries < 600:
                    retries += 1
                    time.sleep(1.0)
                    continue
                r = {
                    "served": False,
                    "wall_s": round(time.perf_counter() - t0, 2),
                    "status": e.response.status_code if e.response is not None else None,
                    "error": str(e)[:300],
                }
                break
            except Exception as e:  # noqa: BLE001 (a shed is recorded as a result)
                r = {
                    "served": False,
                    "wall_s": round(time.perf_counter() - t0, 2),
                    "error": f"{type(e).__name__}: {e}"[:300],
                }
                break
        r["retries_429"] = retries
        r["at"] = now()
        r["rss"] = rss(d.pid)
        out["steps"].setdefault(step, {})[name] = r
        log(f"{step} {name}: {r}")
        save()
        return r

    # ---- 3. warm: zoom 0, whole extent, all layers, twice per grant --------------------------
    for name in ladder:
        viewport("before_1", name, 0, full)
        viewport("before_2", name, 0, full)
    out["status_after_warm"] = control.status()
    save()

    # ---- 4. ingest, delete, flush -----------------------------------------------------------
    keys = (
        pq.read_table(RUNG / "mesh-descriptors.parquet", columns=["key"])
        .column("key")
        .to_pylist()[:3]
    )
    reader = pq.ParquetFile(RUNG / "points.parquet")
    first = next(
        reader.iter_batches(
            batch_size=BATCH_ROWS,
            columns=["x", "y", "branches", "published", "title", "mesh_major"],
        )
    )
    points = pa.Table.from_batches([first])
    run_id = uuid.uuid4().hex[:12]
    external_ids = [f"foldprobe-{run_id}-{i:06d}" for i in range(points.num_rows)]
    body = probe_batch(points, external_ids, LAYER, keys)
    t0 = time.perf_counter()
    r, ingest_s = control.ingest(body, f"foldprobe-{run_id}", requests.Session(), timeout=900)
    ingest = {"status": r.status_code, "wall_s": round(ingest_s, 3), "rows": points.num_rows, "at": now()}
    try:
        body_json = r.json()
        ingest.update({k: body_json.get(k) for k in ("accepted", "minted", "over_bound", "clipped")})
    except ValueError:
        ingest["body"] = r.text[:300]
    ingest["rss"] = rss(d.pid)
    out["steps"]["ingest"] = ingest
    log(f"ingest: {ingest}")

    items = [
        {"external_id": base64.b64encode(e.encode()).decode(), "op": "delete"}
        for e in external_ids[:DELETE_ROWS]
    ]
    r, delete_s = control.changes(items, timeout=900)
    out["steps"]["delete"] = {"status": r.status_code, "wall_s": round(delete_s, 3), "rows": len(items), "at": now()}
    log(f"delete: {out['steps']['delete']}")

    st = control.status()
    flushes_before = st["write_executor"]["flush"]["flushes"]
    refreshes_before = st["write_executor"]["flush"]["refreshes"]
    t0 = time.perf_counter()
    code = control.flush().status_code
    request_s = time.perf_counter() - t0
    published, publish_s = wait_for(
        lambda: control.status()["write_executor"]["flush"]["flushes"] > flushes_before,
        timeout=1800,
        interval=0.25,
    )
    st = control.status()
    out["steps"]["flush"] = {
        "status": code,
        "request_s": round(request_s, 3),
        "published": published,
        "publish_wall_s": round(publish_s, 3),
        "flushes": st["write_executor"]["flush"]["flushes"],
        "refreshes_before": refreshes_before,
        "refreshes_after": st["write_executor"]["flush"]["refreshes"],
        "live_rows": st["compaction"]["live_rows"],
        "at": now(),
        "rss": rss(d.pid),
    }
    log(f"flush: {out['steps']['flush']}")
    save()

    # One viewport per grant after the flush, so the post-fold requests carry the fold's effects
    # and not the flush's.
    for name in ladder:
        viewport("after_flush", name, 0, full)
    out["status_after_flush"] = control.status()
    save()

    # ---- 5. fold ------------------------------------------------------------------------------
    require_memory("fold")
    st = control.status()
    folds_before = st["compaction"]["folds"]
    failures_before = st["compaction"]["fold_failures"]
    refreshes_before = st["write_executor"]["flush"]["refreshes"]
    rss_before = rss(d.pid)
    with RssSampler(d.pid) as sampler:
        t0 = time.perf_counter()
        code = control.compact().status_code
        out["steps"]["compact_request"] = {"status": code, "at": now()}
        log(f"compact requested: {code}")
        done, fold_wall = wait_for(
            lambda: (
                (s := control.status()["compaction"])["folds"] > folds_before
                or s["fold_failures"] > failures_before
            ),
            timeout=FOLD_TIMEOUT_S,
            interval=1.0,
        )
    st = control.status()
    compaction = st["compaction"]
    out["steps"]["fold"] = {
        "completed": done and compaction["folds"] > folds_before,
        "folds": compaction["folds"],
        "fold_failures": compaction["fold_failures"],
        "wall_s": round(fold_wall, 2),
        "at": now(),
        "last_secs": compaction["last_secs"],
        "last_rss_bytes": compaction["last_rss_bytes"],
        "last_attr_bytes_read": compaction.get("last_attr_bytes_read"),
        "last_attr_bytes_written": compaction.get("last_attr_bytes_written"),
        "passes": compaction["passes"],
        "live_rows": compaction["live_rows"],
        "refreshes_before": refreshes_before,
        "refreshes_after": st["write_executor"]["flush"]["refreshes"],
        "rss_before": rss_before,
        "rss_after": rss(d.pid),
        "sampled": sampler.peak(),
    }
    out["fold_rss_samples"] = sampler.samples
    log(f"fold: completed={out['steps']['fold']['completed']} wall={fold_wall:.1f} s passes={compaction['passes']}")
    save()

    # ---- 6. after publication: the same viewports, twice per grant ---------------------------
    for name in ladder:
        viewport("after_fold_1", name, 0, full)
        viewport("after_fold_2", name, 0, full)
    out["status_after_fold_requests"] = control.status()
    save()

    # ---- 7. depth 8 over a small region, twice per grant ------------------------------------
    for name in ladder:
        viewport("zoom8_1", name, 8, small)
        viewport("zoom8_2", name, 8, small)
    out["status_final"] = control.status()
    out["rss_final"] = rss(d.pid)
    save()
finally:
    out["stopped_at"] = now()
    d.stop()
    try:
        os.kill(d.pid, signal.SIGTERM)
    except (ProcessLookupError, TypeError):
        pass
    save()
    # The lines this probe attributes from, in the order the server wrote them.
    patterns = re.compile(
        "|".join(
            [
                r"row form and tile index are built",
                r"held row form took",
                r"adopted the prefix",
                r"artifact pass rebuilt",
                r"compaction fold",
                r"identity rotated",
                r"superseded prefix",
                r"SHED",
                r"serving layout",
                r"containment partition",
                r"dispatching",
                r"a flush",
                r"flush published",
                r"refresh",
                r"ready to serve",
                r"opened",
                r"shapes",
                r"suggestion",
                r"WAL",
                r"wal",
                r"list column names parent edges",
            ]
        )
    )
    excerpt = [line for line in (scratch / "serve.log").read_text(errors="replace").splitlines() if patterns.search(line)]
    (out_dir / "serve-log-excerpt.txt").write_text("\n".join(excerpt) + "\n")
log("done")
