"""Where a request to `POST /v1/artifacts/viewport` spends its time, level by level and phase by
phase, on gbif-64p. Needs a binary built with the temporary `TPROF` counters (see README.md),
which print one line per request to the server's log.

    python3 probes/2026-10-06-species-tiles/tile_phases.py <tessera> <bench run.json> <out.json>

The bench run file supplies the principals' terms and the dense region each is asked at, as
`route_depths.py` on serving/stage5-bench uses it.
"""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

import requests

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))

from test_corpora.common.deployment import Deployment, read_env_file, tomllib  # noqa: E402
from test_corpora.common.serve_battery import frames  # noqa: E402

DEPLOYMENT = Path(
    os.environ.get("DEPLOYMENT", "/home/joe/code/tessera/data/ladder/gbif-64p/bench-stage5")
)
SCREEN = (1600, 900)
PER_TILE = int(os.environ.get("PER_TILE", "50"))
REPEATS = int(os.environ.get("REPEATS", "3"))
# (map zoom, tile depth, levels)
ASKS = [
    (9, 11, [1]),
    (9, 11, [2]),
    (9, 13, [1]),
    (9, 13, [2]),
    (12, 14, [2]),
    (12, 16, [2]),
]


def box_at(q: dict, centre: list[float], z: int) -> list[float]:
    w, h = SCREEN
    sy = (q["y_max"] - q["y_min"]) / 2**z
    sx = (q["x_max"] - q["x_min"]) / 2**z * (w / h)
    return [centre[0] - sx / 2, centre[1] - sy / 2, centre[0] + sx / 2, centre[1] + sy / 2]


def parse(line: str) -> dict:
    out: dict = {}
    for part in line.split("|")[1:]:
        fields = part.split()
        slot = fields[0]
        out[slot] = {k: int(v) for k, v in (f.split("=") for f in fields[1:])}
    return out


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
        DEPLOYMENT / "scratch-species-tiles",
        ports,
        Path(binary),
        cap_bytes=24 * 2**30,
        swap_bytes=2 * 2**30,
    )
    served.clear_scratch()
    served.start()
    print(f"server pid {served.pid}", flush=True)
    log = Path(served.log_path)
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
            meta = http.get(f"{served.viewer}/v1/meta", timeout=120).json()
            view, layer = meta["views"][0], meta["layers"][0]
            q = view["quantisation"]
            centre = p["regions"][0]["centre"]
            for z, depth, levels in ASKS:
                body = {
                    "view": view["id"],
                    "zoom": depth,
                    "bbox": box_at(q, centre, z),
                    "layers": [layer["name"]],
                    "levels": levels,
                    "per_tile": PER_TILE,
                }
                for rep in range(REPEATS + 1):
                    seen = log.read_text().count("TPROF")
                    t0 = time.perf_counter()
                    resp = http.post(f"{served.viewer}/v1/artifacts/viewport", json=body, timeout=600)
                    content = resp.content
                    ms = (time.perf_counter() - t0) * 1000.0
                    resp.raise_for_status()
                    lines = [ln for ln in log.read_text().splitlines() if "TPROF" in ln]
                    prof = parse(lines[seen]) if len(lines) > seen else {}
                    row = {
                        "principal": p["label"],
                        "zoom": z,
                        "depth": depth,
                        "levels": levels,
                        "rep": rep,
                        "ms": round(ms, 1),
                        "bytes": len(content),
                        "tiles": sum(1 for kind, _ in frames(content) if kind == 5),
                        "prof": prof,
                    }
                    rows.append(row)
                    s = prof.get(f"s{levels[0]}", {})
                    print(
                        f"{p['label']:>4} z{z} d{depth} L{levels} rep{rep} {ms:8.1f} ms "
                        f"tiles={row['tiles']} cand={s.get('n_cand', 0)} heap={s.get('n_heap', 0)} "
                        f"popped={s.get('n_popped', 0)} probes={s.get('n_probes', 0)} "
                        f"scans={s.get('n_scans', 0)} chosen={s.get('n_chosen', 0)}",
                        flush=True,
                    )
    finally:
        served.stop()
    Path(out).write_text(json.dumps({"binary": binary, "per_tile": PER_TILE, "rows": rows}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
