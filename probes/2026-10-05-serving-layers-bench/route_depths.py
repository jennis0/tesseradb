"""The artifacts viewport asked directly, at a tile depth two and four below the map zoom, for the
bench's principals: what the route costs when a client asks for about a screen's worth of
256-pixel tiles, beside what the TypeScript store asks for in the bench.

    python3 probes/2026-10-05-serving-layers-bench/route_depths.py <tessera> <bench run.json> <out.json>

It starts the binary on a fresh cache over the bench deployment, with the bundle's pages evicted
first, under the same memory cap as the bench.
"""

from __future__ import annotations

import json
import math
import os
import sys
import time
from pathlib import Path

import requests

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))

from test_corpora.common.deployment import Deployment, read_env_file, tomllib  # noqa: E402
from test_corpora.common.serve_battery import frames  # noqa: E402

DEPLOYMENT = Path("/home/joe/code/tessera/data/ladder/gbif-64p/bench-stage5")
SCREEN = (1600, 900)
ZOOMS = (0, 3, 6, 9, 12)
OFFSETS = (2, 4)
PER_TILE = 50
REPEATS = 5


def evict(bundle: Path) -> None:
    for root, _, files in os.walk(bundle):
        for name in files:
            fd = os.open(os.path.join(root, name), os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            os.close(fd)


def box_at(q: dict, centre: list[float], z: int) -> list[float]:
    """As the bench's `boxAt`: the data-space box the screen shows at map zoom `z`."""
    w, h = SCREEN
    sy = (q["y_max"] - q["y_min"]) / 2**z
    sx = (q["x_max"] - q["x_min"]) / 2**z * (w / h)
    return [centre[0] - sx / 2, centre[1] - sy / 2, centre[0] + sx / 2, centre[1] + sy / 2]


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
    binary, run_json, out = sys.argv[1:4]
    run = json.loads(Path(run_json).read_text())
    settings = tomllib.loads((DEPLOYMENT / "tessera.toml").read_text())
    serve = settings["serve"]
    ports = tuple(int(serve[k].rsplit(":", 1)[1]) for k in ("viewer", "session", "control"))
    bundle = (DEPLOYMENT / settings["bundle"]["path"]).resolve()
    cred = (dict(os.environ) | read_env_file(DEPLOYMENT / ".env"))[serve["operator_credential_env"]]
    served = Deployment(
        DEPLOYMENT,
        bundle,
        DEPLOYMENT / "scratch-route",
        ports,
        Path(binary),
        cap_bytes=24 * 2**30,
        swap_bytes=2 * 2**30,
    )
    served.clear_scratch()
    evict(bundle)
    served.start()
    result: dict = {"binary": binary, "bundle": str(bundle), "per_tile": PER_TILE, "rows": []}
    try:
        for p in run["principals"]:
            r = requests.post(
                f"{served.session}/session/authorise",
                json={"terms": p["term_list"]},
                headers={"Authorization": f"Bearer {cred}"},
                timeout=120,
            )
            r.raise_for_status()
            token = r.json()["token"]
            http = requests.Session()
            http.headers["Authorization"] = f"Bearer {token}"
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
        result["status"] = requests.get(
            f"{served.control}/control/status",
            headers={"Authorization": f"Bearer {cred}"},
            timeout=60,
        ).json()["masked_count_cache"]
    finally:
        served.stop()
    Path(out).write_text(json.dumps(result, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
