"""What a viewer's first reads of the taxonomy cost on full GBIF, step by step.

    python3 probes/2026-10-06-first-open-fills/first_open.py --deployment data/ladder/gbif \
        --binary <tessera> --run <bench run.json> --out <out.json> [--reopen] [--cap 24G] [--swap 2G]

It starts the binary on a fresh cache over the deployment's bundle, under the cap, with the
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

import argparse
import io
import json
import os
import shutil
import sys
import time
from pathlib import Path

import requests
from pyarrow import ipc

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from test_corpora.common.deployment import Deployment
from test_corpora.common.interactive_bench import box_at, evict, size_bytes
from test_corpora.common.serve_battery import frames

LAYER = "taxonomy/tree"


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


def run_steps(served: Deployment, run: dict, device: str, phase: str, out: list) -> None:
    log = served.log_path


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
            "figures": served.figures_status(),
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
        http = served.viewer_session(p["term_list"])
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
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--deployment", type=Path, required=True)
    ap.add_argument("--binary", type=Path, required=True)
    ap.add_argument("--run", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--reopen", action="store_true")
    ap.add_argument("--cap", default="24G")
    ap.add_argument("--swap", default="2G")
    args = ap.parse_args()
    run = json.loads(args.run.read_text())
    scratch = args.deployment / "bench-scratch-first-open"
    shutil.rmtree(scratch, ignore_errors=True)
    served = Deployment.of(
        args.deployment, args.binary, scratch, cap_bytes=size_bytes(args.cap), swap_bytes=size_bytes(args.swap)
    )
    bundle = served.bundle
    device = device_of(bundle)
    result: dict = {"binary": str(args.binary), "steps": [], "opens": []}
    phases = ["fresh"] + (["reopen"] if args.reopen else [])
    try:
        for phase in phases:
            evict(bundle)
            t0 = time.time()
            served.start(log=scratch / f"serve-{phase}.log")
            result["opens"].append({"phase": phase, "open_s": round(time.time() - t0, 1), "load1": os.getloadavg()[0]})
            print(json.dumps(result["opens"][-1]), flush=True)
            run_steps(served, run, device, phase, result["steps"])
            served.stop()
    finally:
        served.stop()
        result["cache_bytes"] = sum(
            f.stat().st_size for f in (scratch / "cache").rglob("*") if f.is_file()
        )
        args.out.write_text(json.dumps(result, indent=1))
        shutil.rmtree(scratch, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
