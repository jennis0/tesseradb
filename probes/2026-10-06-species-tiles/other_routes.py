"""The routes other than the tile walk that read the species level, timed on one deployment: browse
(two pages of a search and the children of a genus, with and without a filter), `/v1/aggregate` grouped by the level, the points viewport
tagged with the layer, `/v1/artifacts/{id}`, and the bulk read with `ids`.

    python3 probes/2026-10-06-species-tiles/other_routes.py <mosaica> <bench run.json> <out.json>

`DEPLOYMENT` picks the deployment (bench-stage5 by default). The server starts on a fresh cache
with the bundle's pages evicted, so each viewer's first ask of a route is the cold one, and
`REPEATS` more give the warm figures.
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

DEPLOYMENT = Path(
    os.environ.get("DEPLOYMENT", "/home/joe/code/mosaica/data/ladder/gbif-64p/bench-stage5")
)
SCREEN = (1600, 900)
REPEATS = int(os.environ.get("REPEATS", "5"))
#: Route names separated by `|` to time; absent is every route.
ROUTES = [r for r in os.environ.get("ROUTES", "").split("|") if r]
LEVEL = 2
FILTER = {"kingdom": {"eq": "Plantae"}}


def evict(bundle: Path) -> None:
    for root, _, files in os.walk(bundle):
        for name in files:
            fd = os.open(os.path.join(root, name), os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            os.close(fd)


def box_at(q: dict, centre: list[float], z: int) -> list[float]:
    w, h = SCREEN
    sy = (q["y_max"] - q["y_min"]) / 2**z
    sx = (q["x_max"] - q["x_min"]) / 2**z * (w / h)
    return [centre[0] - sx / 2, centre[1] - sy / 2, centre[0] + sx / 2, centre[1] + sy / 2]


def timed(call) -> tuple[float, requests.Response]:
    t0 = time.perf_counter()
    r = call()
    _ = r.content
    ms = (time.perf_counter() - t0) * 1000.0
    r.raise_for_status()
    return ms, r


def main() -> int:
    binary, run_json, out = sys.argv[1:4]
    run = json.loads(Path(run_json).read_text())
    settings = tomllib.loads((DEPLOYMENT / "mosaica.toml").read_text())
    serve = settings["serve"]
    ports = tuple(int(serve[k].rsplit(":", 1)[1]) for k in ("viewer", "session", "control"))
    bundle = (DEPLOYMENT / settings["bundle"]["path"]).resolve()
    cred = (dict(os.environ) | read_env_file(DEPLOYMENT / ".env"))[serve["operator_credential_env"]]
    served = Deployment(
        DEPLOYMENT,
        bundle,
        DEPLOYMENT / "scratch-other-routes",
        ports,
        Path(binary),
        cap_bytes=24 * 2**30,
        swap_bytes=2 * 2**30,
    )
    served.clear_scratch()
    evict(bundle)
    served.start()
    print(f"server pid {served.pid}", flush=True)
    rows = []
    try:
        for p in run["principals"]:
            r = requests.post(
                f"{served.session}/session/authorise",
                json={"terms": p["term_list"]},
                headers={"Authorization": f"Bearer {cred}"},
                timeout=120,
            )
            r.raise_for_status()
            http = requests.Session()
            http.headers["Authorization"] = f"Bearer {r.json()['token']}"
            v = served.viewer
            meta = http.get(f"{v}/v1/meta", timeout=120).json()
            view, layer = meta["views"][0]["id"], meta["layers"][0]["name"]
            q = meta["views"][0]["quantisation"]
            centre = p["regions"][0]["centre"]
            # A species has a served genus, so it is never a root: page the search form instead.
            browse = {"view": view, "layer": layer, "level": LEVEL, "limit": 100, "q": "a"}
            first = http.post(f"{v}/v1/artifacts/browse", json=browse, timeout=600)
            first.raise_for_status()
            page1 = first.json()
            ids = [row["tessera_id"] for row in page1["artifacts"]]
            genus = http.post(
                f"{v}/v1/artifacts/browse",
                json={"view": view, "layer": layer, "level": LEVEL - 1, "limit": 1, "q": "a"},
                timeout=600,
            ).json()["artifacts"][0]["tessera_id"]
            children = {"view": view, "layer": layer, "level": LEVEL, "limit": 100, "parent": genus}
            asks = {
                "browse page 1": lambda: http.post(
                    f"{v}/v1/artifacts/browse", json=browse, timeout=600
                ),
                "browse page 2": lambda: http.post(
                    f"{v}/v1/artifacts/browse",
                    json=browse | {"cursor": page1["next"]},
                    timeout=600,
                ),
                "browse filtered": lambda: http.post(
                    f"{v}/v1/artifacts/browse", json=browse | {"filters": FILTER}, timeout=600
                ),
                "browse children of the top genus": lambda: http.post(
                    f"{v}/v1/artifacts/browse", json=children, timeout=600
                ),
                "browse children, filtered": lambda: http.post(
                    f"{v}/v1/artifacts/browse", json=children | {"filters": FILTER}, timeout=600
                ),
                "aggregate by species": lambda: http.post(
                    f"{v}/v1/aggregate",
                    json={
                        "view": view,
                        "groupings": [{"by": {"layer": layer, "level": LEVEL, "top": 50}}],
                    },
                    timeout=600,
                ),
                "aggregate by species, filtered": lambda: http.post(
                    f"{v}/v1/aggregate",
                    json={
                        "view": view,
                        "filters": FILTER,
                        "groupings": [{"by": {"layer": layer, "level": LEVEL, "top": 50}}],
                    },
                    timeout=600,
                ),
                "viewport z11 tagged": lambda: http.post(
                    f"{v}/v1/viewport",
                    json={
                        "view": view,
                        "zoom": 11,
                        "bbox": box_at(q, centre, 9),
                        "layers": [layer],
                        "levels": [LEVEL],
                    },
                    timeout=600,
                ),
                "viewport z14 tagged": lambda: http.post(
                    f"{v}/v1/viewport",
                    json={
                        "view": view,
                        "zoom": 14,
                        "bbox": box_at(q, centre, 12),
                        "layers": [layer],
                        "levels": [LEVEL],
                    },
                    timeout=600,
                ),
                "artifact by id x20": lambda: [
                    http.post(f"{v}/v1/artifacts/{i}", json={"view": view}, timeout=600)
                    for i in ids[:20]
                ][-1],
                "tiles z9 depth 11": lambda: http.post(
                    f"{v}/v1/artifacts/viewport",
                    json={
                        "view": view,
                        "zoom": 11,
                        "bbox": box_at(q, centre, 9),
                        "layers": [layer],
                        "levels": [LEVEL],
                        "per_tile": 50,
                    },
                    timeout=600,
                ),
                "tiles z9 depth 13": lambda: http.post(
                    f"{v}/v1/artifacts/viewport",
                    json={
                        "view": view,
                        "zoom": 13,
                        "bbox": box_at(q, centre, 9),
                        "layers": [layer],
                        "levels": [LEVEL],
                        "per_tile": 50,
                    },
                    timeout=600,
                ),
                "bulk read 100 ids": lambda: http.post(
                    f"{v}/v1/artifacts",
                    json={
                        "view": view,
                        "layer": layer,
                        "ids": ids,
                        "fields": ["key", "level", "masked_count", "content", "centroid", "box"],
                    },
                    timeout=600,
                ),
            }
            for name, call in asks.items():
                if ROUTES and name not in ROUTES:
                    continue
                cold, resp = timed(call)
                warm = sorted(timed(call)[0] for _ in range(REPEATS))
                row = {
                    "principal": p["label"],
                    "route": name,
                    "cold_ms": round(cold, 1),
                    "warm_p50_ms": round(warm[math.ceil(len(warm) / 2) - 1], 1),
                    "warm_max_ms": round(warm[-1], 1),
                    "bytes": len(resp.content),
                }
                rows.append(row)
                print(json.dumps(row), flush=True)
    finally:
        served.stop()
    Path(out).write_text(json.dumps({"binary": binary, "bundle": str(bundle), "rows": rows}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
