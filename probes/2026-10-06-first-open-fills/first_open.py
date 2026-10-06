"""What a viewer's first reads of the taxonomy cost on full GBIF, step by step.

    python3 probes/2026-10-06-first-open-fills/first_open.py <tessera> <bench run.json> <out.json> [--reopen]

It starts the binary on a fresh cache over `data/ladder/gbif`, under the bench's cap, with the
bundle's pages evicted first. For each of the bench's viewers in turn it sends what the store's
first open sends for the layer (the world at depth 2, level 0), then the read by identifier of the
artifacts that answered, as the store reads its points' tags, then the same read again, then genus
and species at zoom 9 over the viewer's densest region. With `--reopen` it then restarts the server
over the kept cache and sends the same steps again.

Beside each step's time it keeps the server's figures counters, the bytes the bundle's disk read
while the step ran (from `/proc/diskstats`, so another session's reads count too), the scope's
major faults, the one-minute load, and any `PROBE` lines a binary with timers wrote.
"""

from __future__ import annotations

import io
import json
import os
import shutil
import sys
import time
from pathlib import Path

import requests
from pyarrow import ipc

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))

from test_corpora.common.deployment import Deployment, read_env_file, tomllib  # noqa: E402
from test_corpora.common.serve_battery import frames  # noqa: E402

DEPLOYMENT = Path("/home/joe/code/tessera/data/ladder/gbif")
SCREEN = (1600, 900)
LAYER = "taxonomy/tree"


def evict(bundle: Path) -> None:
    for root, _, files in os.walk(bundle):
        for name in files:
            fd = os.open(os.path.join(root, name), os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            os.close(fd)


def device_of(path: Path) -> str:
    dev = os.stat(path).st_dev
    return f"{os.major(dev)}:{os.minor(dev)}"


def sectors_read(device: str) -> int:
    major, minor = device.split(":")
    for line in Path("/proc/diskstats").read_text().splitlines():
        f = line.split()
        if f[0] == major and f[1] == minor:
            return int(f[5])
    return 0


def majfaults(cgroup: Path | None) -> int:
    if cgroup is None:
        return 0
    for line in (cgroup / "memory.stat").read_text().splitlines():
        if line.startswith("pgmajfault "):
            return int(line.split()[1])
    return 0


def box_at(q: dict, centre: list[float], z: int) -> list[float]:
    w, h = SCREEN
    sy = (q["y_max"] - q["y_min"]) / 2**z
    sx = (q["x_max"] - q["x_min"]) / 2**z * (w / h)
    return [centre[0] - sx / 2, centre[1] - sy / 2, centre[0] + sx / 2, centre[1] + sy / 2]


def run_steps(served: Deployment, run: dict, control: dict, device: str, phase: str, out: list) -> None:
    log = served.log_path

    def status() -> dict:
        s = requests.get(f"{served.control}/control/status", headers=control, timeout=60).json()
        return s["masked_count_cache"]

    def timed(http: requests.Session, url: str, body: dict, viewer: str, step: str, **extra) -> bytes:
        offset = log.stat().st_size
        sectors, faults = sectors_read(device), majfaults(served.cgroup)
        t0 = time.perf_counter()
        r = http.post(url, json=body, timeout=900)
        content = r.content
        ms = (time.perf_counter() - t0) * 1000.0
        r.raise_for_status()
        with open(log, "rb") as handle:
            handle.seek(offset)
            probes = [
                line.decode(errors="replace").strip()
                for line in handle.read().splitlines()
                if b"PROBE" in line
            ]
        row = {
            "phase": phase,
            "viewer": viewer,
            "step": step,
            "ms": round(ms, 1),
            "bytes": len(content),
            "disk_mb": round((sectors_read(device) - sectors) * 512 / 2**20, 1),
            "majfaults": majfaults(served.cgroup) - faults,
            "load1": os.getloadavg()[0],
            "figures": status(),
            "probes": probes,
            **extra,
        }
        out.append(row)
        print(json.dumps({k: v for k, v in row.items() if k not in ("figures", "probes")}), flush=True)
        for line in probes:
            print("   ", line, flush=True)
        return content

    wanted = os.environ.get("FIRST_OPEN_VIEWERS")
    for p in run["principals"]:
        if wanted and p["label"] not in wanted.split(","):
            continue
        r = requests.post(
            f"{served.session}/session/authorise",
            json={"terms": p["term_list"]},
            headers=control,
            timeout=120,
        )
        r.raise_for_status()
        http = requests.Session()
        http.headers["Authorization"] = f"Bearer {r.json()['token']}"
        meta = http.get(f"{served.viewer}/v1/meta", timeout=120).json()
        view = meta["views"][0]
        q = view["quantisation"]
        world = [(q["x_min"] + q["x_max"]) / 2, (q["y_min"] + q["y_max"]) / 2]
        tiles = f"{served.viewer}/v1/artifacts/viewport"
        content = timed(
            http,
            tiles,
            {
                "view": view["id"],
                "zoom": 2,
                "bbox": box_at(q, world, 0),
                "layers": [LAYER],
                "levels": [0],
                "per_tile": 50,
            },
            p["label"],
            "world, level 0, depth 2",
        )
        ids = sorted(
            {
                str(v)
                for kind, payload in frames(content)
                if kind == 5
                for v in ipc.open_stream(io.BytesIO(payload)).read_all().column("tessera_id").to_pylist()
            }
        )
        body = {"view": view["id"], "layer": LAYER, "ids": ids, "fields": ["level", "parents", "centroid"]}
        for step in ("tags by identifier, first", "tags by identifier, again"):
            timed(http, f"{served.viewer}/v1/artifacts", body, p["label"], step, ids=len(ids))
        timed(
            http,
            tiles,
            {
                "view": view["id"],
                "zoom": 11,
                "bbox": box_at(q, p["regions"][0]["centre"], 9),
                "layers": [LAYER],
                "levels": [1, 2],
                "per_tile": 50,
            },
            p["label"],
            "zoom 9, genus and species, depth 11",
        )


def main() -> int:
    binary, run_json, out = sys.argv[1:4]
    reopen = "--reopen" in sys.argv[4:]
    run = json.loads(Path(run_json).read_text())
    settings = tomllib.loads((DEPLOYMENT / "tessera.toml").read_text())
    serve = settings["serve"]
    ports = tuple(int(serve[k].rsplit(":", 1)[1]) for k in ("viewer", "session", "control"))
    bundle = (DEPLOYMENT / settings["bundle"]["path"]).resolve()
    device = device_of(bundle)
    cred = (dict(os.environ) | read_env_file(DEPLOYMENT / ".env"))[serve["operator_credential_env"]]
    control = {"Authorization": f"Bearer {cred}"}
    scratch = DEPLOYMENT / "bench-scratch-first-open"
    shutil.rmtree(scratch, ignore_errors=True)
    served = Deployment(
        DEPLOYMENT, bundle, scratch, ports, Path(binary), cap_bytes=24 * 2**30, swap_bytes=2 * 2**30
    )
    result: dict = {"binary": binary, "steps": [], "opens": []}
    phases = ["fresh"] + (["reopen"] if reopen else [])
    try:
        for phase in phases:
            evict(bundle)
            t0 = time.time()
            served.start(log=scratch / f"serve-{phase}.log")
            result["opens"].append({"phase": phase, "open_s": round(time.time() - t0, 1), "load1": os.getloadavg()[0]})
            print(json.dumps(result["opens"][-1]), flush=True)
            run_steps(served, run, control, device, phase, result["steps"])
            served.stop()
    finally:
        served.stop()
        result["cache_bytes"] = sum(
            f.stat().st_size for f in (scratch / "cache").rglob("*") if f.is_file()
        )
        Path(out).write_text(json.dumps(result, indent=1))
        shutil.rmtree(scratch, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
