"""What a viewer's first reads of the taxonomy cost on full GBIF: each level's fill, and the first
tag read by identifier.

    python3 probes/2026-10-05-serving-layers-bench/gbif_first_reads.py <tessera> <bench run.json> <out.json>

It starts the binary on a fresh cache over `data/ladder/gbif`, under the bench's cap, with the
bundle's pages evicted first. For each of the bench's viewers in turn it sends what the store's
first open sends for the layer (the world at depth 2, level 0), then a read by identifier of the
artifacts that answered, as the store reads its points' tags, then the same read again, then
genus and species at zoom 9 over the viewer's densest region. After each step it keeps the
server's memory and `masked_count_cache`. The server's `PROBE` log lines are kept: a binary with
timers around each fill and each level read writes them.
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
from test_corpora.common.interactive_bench import process_memory  # noqa: E402
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


def box_at(q: dict, centre: list[float], z: int) -> list[float]:
    w, h = SCREEN
    sy = (q["y_max"] - q["y_min"]) / 2**z
    sx = (q["x_max"] - q["x_min"]) / 2**z * (w / h)
    return [centre[0] - sx / 2, centre[1] - sy / 2, centre[0] + sx / 2, centre[1] + sy / 2]


def timed(http: requests.Session, url: str, body: dict) -> tuple[float, bytes]:
    t0 = time.perf_counter()
    r = http.post(url, json=body, timeout=900)
    content = r.content
    ms = (time.perf_counter() - t0) * 1000.0
    r.raise_for_status()
    return round(ms, 1), content


def main() -> int:
    binary, run_json, out = sys.argv[1:4]
    run = json.loads(Path(run_json).read_text())
    settings = tomllib.loads((DEPLOYMENT / "tessera.toml").read_text())
    serve = settings["serve"]
    ports = tuple(int(serve[k].rsplit(":", 1)[1]) for k in ("viewer", "session", "control"))
    bundle = (DEPLOYMENT / settings["bundle"]["path"]).resolve()
    cred = (dict(os.environ) | read_env_file(DEPLOYMENT / ".env"))[serve["operator_credential_env"]]
    control = {"Authorization": f"Bearer {cred}"}
    scratch = DEPLOYMENT / "bench-scratch-first-reads"
    shutil.rmtree(scratch, ignore_errors=True)
    served = Deployment(
        DEPLOYMENT, bundle, scratch, ports, Path(binary), cap_bytes=24 * 2**30, swap_bytes=2 * 2**30
    )
    evict(bundle)
    t0 = time.time()
    served.start()
    result: dict = {"binary": binary, "open_s": round(time.time() - t0, 1), "steps": []}

    def status() -> dict:
        s = requests.get(f"{served.control}/control/status", headers=control, timeout=60).json()
        return s["masked_count_cache"]

    def note(viewer: str, step: str, ms: float, content: bytes, **extra) -> None:
        row = {
            "viewer": viewer,
            "step": step,
            "ms": ms,
            "bytes": len(content),
            "at": time.time(),
            "memory": process_memory(served.pid),
            "figures": status(),
            **extra,
        }
        result["steps"].append(row)
        print(json.dumps({k: v for k, v in row.items() if k not in ("figures",)}), flush=True)

    try:
        result["memory_at_ready"] = process_memory(served.pid)
        for p in run["principals"]:
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
            ms, content = timed(
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
            )
            ids = sorted(
                {
                    str(v)
                    for kind, payload in frames(content)
                    if kind == 5
                    for v in ipc.open_stream(io.BytesIO(payload))
                    .read_all()
                    .column("tessera_id")
                    .to_pylist()
                }
            )
            note(p["label"], "world, level 0, depth 2", ms, content)
            body = {
                "view": view["id"],
                "layer": LAYER,
                "ids": ids,
                "fields": ["level", "parents", "centroid"],
            }
            for step in ("tags by identifier, first", "tags by identifier, again"):
                ms, content = timed(http, f"{served.viewer}/v1/artifacts", body)
                note(p["label"], step, ms, content, ids=len(ids))
            ms, content = timed(
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
            )
            note(p["label"], "zoom 9, genus and species, depth 11", ms, content)
    finally:
        served.stop()
    result["log"] = [
        line for line in (scratch / "serve.log").read_text().splitlines() if "PROBE" in line
    ]
    result["cache_bytes"] = sum(f.stat().st_size for f in (scratch / "cache").rglob("*") if f.is_file())
    Path(out).write_text(json.dumps(result, indent=1))
    shutil.rmtree(scratch, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
