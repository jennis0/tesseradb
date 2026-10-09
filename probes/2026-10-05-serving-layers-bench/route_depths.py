"""The artifacts viewport asked directly, at a tile depth two and four below the map zoom, for the
bench's principals: what the route costs when a client asks for about a screen's worth of
256-pixel tiles, beside what the TypeScript store asks for in the bench.

    python3 probes/2026-10-05-serving-layers-bench/route_depths.py --deployment <dir> \
        --binary <mosaica> --run <bench run.json> --out <out.json> [--cap 24G] [--swap 2G]

It starts the binary on a fresh cache over the deployment's bundle, with the bundle's pages
evicted first, under the cap. The bench run gives the principals and their densest regions.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
import time
from pathlib import Path

import requests

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from test_corpora.common.deployment import Deployment
from test_corpora.common.interactive_bench import box_at, evict, size_bytes
from test_corpora.common.serve_battery import frames

ZOOMS = (0, 3, 6, 9, 12)
OFFSETS = (2, 4)
PER_TILE = 50
REPEATS = 5


def levels_at(layer: dict, z: int) -> list[int]:
    return [
        lv["level"]
        for lv in layer["levels"]
        if lv.get("zoom") is None or lv["zoom"][0] <= z <= lv["zoom"][1]
    ]


def ask(http: requests.Session, viewer: str, body: dict) -> dict:
    t0 = time.perf_counter()
    r = http.post(f"{viewer}/v1/artifacts/viewport", json=body, timeout=600)
    content = r.content
    ms = (time.perf_counter() - t0) * 1000.0
    r.raise_for_status()
    tiles = sum(1 for kind, _ in frames(content) if kind == 5)
    return {"ms": round(ms, 1), "bytes": len(content), "frames": tiles}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--deployment", type=Path, required=True)
    ap.add_argument("--binary", type=Path, required=True)
    ap.add_argument("--run", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--cap", default="24G")
    ap.add_argument("--swap", default="2G")
    args = ap.parse_args()
    run = json.loads(args.run.read_text())
    served = Deployment.of(
        args.deployment,
        args.binary,
        args.deployment / "bench-scratch-route",
        cap_bytes=size_bytes(args.cap),
        swap_bytes=size_bytes(args.swap),
    )
    served.clear_scratch()
    result: dict = {"binary": str(args.binary), "bundle": str(served.bundle), "per_tile": PER_TILE, "rows": []}
    try:
        evict(served.bundle)
        served.start()
        for p in run["principals"]:
            http = served.viewer_session(p["term_list"])
            meta = http.get(f"{served.viewer}/v1/meta", timeout=120).json()
            view = meta["views"][0]
            layer = meta["layers"][0]
            q = view["quantisation"]
            world = [(q["x_min"] + q["x_max"]) / 2, (q["y_min"] + q["y_max"]) / 2]
            dense = p["regions"][0]["centre"]
            for z in ZOOMS:
                centre = world if z == 0 else dense
                for offset in OFFSETS:
                    depth = min(16, z + offset)
                    body = {
                        "view": view["id"],
                        "zoom": depth,
                        "bbox": box_at(q, centre, z),
                        "layers": [layer["name"]],
                        "levels": levels_at(layer, z),
                        "per_tile": PER_TILE,
                    }
                    first = ask(http, served.viewer, body)
                    warm = [ask(http, served.viewer, body) for _ in range(REPEATS)]
                    ms = sorted(w["ms"] for w in warm)
                    row = {
                        "principal": p["label"],
                        "zoom": z,
                        "depth": depth,
                        "levels": body["levels"],
                        "tiles": first["frames"],
                        "bytes": first["bytes"],
                        "first_ms": first["ms"],
                        "warm_p50_ms": ms[math.ceil(len(ms) / 2) - 1],
                        "warm_max_ms": ms[-1],
                    }
                    result["rows"].append(row)
                    print(json.dumps(row), flush=True)
        result["status"] = served.figures_status()
    finally:
        served.stop()
    args.out.write_text(json.dumps(result, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
