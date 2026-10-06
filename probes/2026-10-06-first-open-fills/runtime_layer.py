"""A flat layer published at runtime on gbif-64p, and its first reads on the artifacts viewport.

    python3 probes/2026-10-06-first-open-fills/runtime_layer.py <tessera> <bench run.json> <out.json>

It starts the binary on a fresh cache over the bench deployment's bundle, registers `genus-flat`
(the taxonomy's genus level declared again as a flat layer), publishes its artifacts from
`members-taxonomy.parquet` by `gbifid`, and then, for the 100% and 1% viewers, asks the
artifacts viewport for the world at depth 2 and for the viewer's densest region at map zoom 9,
depth 11: the first request, then five more. The built-in genus level is asked the same way for
comparison. The server's log lines about the new layer are kept, with their times. The server
serves a copy of the bundle, since a publication writes into the bundle it serves.
"""

from __future__ import annotations

import json
import math
import os
import shutil
import sys
import time
from collections import defaultdict
from pathlib import Path

import pyarrow.parquet as pq
import requests

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))

from test_corpora.common.deployment import Deployment, read_env_file, tomllib  # noqa: E402
from test_corpora.common.serve_battery import frames  # noqa: E402

DEPLOYMENT = Path("/home/joe/code/tessera/data/ladder/gbif-64p/first-open-fills")
MEMBERS = DEPLOYMENT.parent / "members-taxonomy.parquet"
LAYER = "genus-flat"
SCREEN = (1600, 900)
REPEATS = 5
#: Kept under the 10,000 artifacts and 64 MiB of a publication request.
BATCH_ARTIFACTS = 10_000
BATCH_MEMBERS = 3_000_000


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


def genus_members() -> dict[str, list[int]]:
    table = pq.read_table(MEMBERS)
    entity = table.column("entity").to_pylist()
    keys = table.column("key").to_pylist()
    out: dict[str, list[int]] = defaultdict(list)
    for e, k in zip(entity, keys):
        if k and len(k) > 1 and k[1] is not None:
            out[k[1]].append(e)
    return out


def batches(groups: dict[str, list[int]]):
    batch, members = [], 0
    for key in sorted(groups):
        ids = groups[key]
        if batch and (len(batch) >= BATCH_ARTIFACTS or members + len(ids) > BATCH_MEMBERS):
            yield batch
            batch, members = [], 0
        batch.append({"key": key, "members": {"gbifid": ids}})
        members += len(ids)
    if batch:
        yield batch


def ask(http: requests.Session, viewer: str, body: dict) -> dict:
    t0 = time.perf_counter()
    r = http.post(f"{viewer}/v1/artifacts/viewport", json=body, timeout=600)
    content = r.content
    ms = (time.perf_counter() - t0) * 1000.0
    r.raise_for_status()
    return {
        "ms": round(ms, 1),
        "bytes": len(content),
        "frames": sum(1 for kind, _ in frames(content) if kind == 5),
        "at": time.time(),
    }


def main() -> int:
    binary, run_json, out = sys.argv[1:4]
    run = json.loads(Path(run_json).read_text())
    settings = tomllib.loads((DEPLOYMENT / "tessera.toml").read_text())
    serve = settings["serve"]
    ports = tuple(int(serve[k].rsplit(":", 1)[1]) for k in ("viewer", "session", "control"))
    bundle = (DEPLOYMENT / settings["bundle"]["path"]).resolve()
    cred = (dict(os.environ) | read_env_file(DEPLOYMENT / ".env"))[serve["operator_credential_env"]]
    control_headers = {"Authorization": f"Bearer {cred}"}
    # A fresh scratch directory and a copy of the bundle, since a publication writes membership
    # segments into the bundle it serves and a layer's name is registered once.
    scratch = DEPLOYMENT / "scratch-runtime-layer"
    shutil.rmtree(scratch, ignore_errors=True)
    scratch.mkdir(parents=True)
    shutil.copytree(bundle, scratch / "bundle")
    bundle = scratch / "bundle"
    served = Deployment(
        DEPLOYMENT,
        bundle,
        DEPLOYMENT / "scratch-runtime-layer",
        ports,
        Path(binary),
        cap_bytes=24 * 2**30,
        swap_bytes=2 * 2**30,
    )
    served.clear_scratch()
    groups = genus_members()
    evict(bundle)
    served.start()
    result: dict = {"binary": binary, "bundle": str(bundle), "layer": LAYER, "reads": []}
    try:
        declaration = {
            "name": LAYER,
            "title": "Genus, published at runtime",
            "views": ["geo"],
            "membership": "enumerated",
            "value_set": "open",
            "hierarchy": {"kind": "flat"},
            "visibility": "public",
            "artifact_visibility": {"default": "inherited"},
            "require_member_visibility": {"count": 1},
            "content": {"computed": ["centroid", "box"]},
        }
        t0 = time.perf_counter()
        r = requests.put(
            f"{served.control}/control/layers", json=declaration, headers=control_headers, timeout=120
        )
        if r.status_code >= 300:
            raise SystemExit(f"registration refused: {r.status_code} {r.text[:500]}")
        result["register_ms"] = round((time.perf_counter() - t0) * 1000.0, 1)
        t0 = time.perf_counter()
        sent = members = 0
        all_batches = list(batches(groups))
        for i, batch in enumerate(all_batches):
            wait = "?wait=visible" if i == len(all_batches) - 1 else ""
            r = requests.put(
                f"{served.control}/control/layers/{LAYER}/artifacts{wait}",
                json={"artifacts": batch},
                headers=control_headers,
                timeout=600,
            )
            if r.status_code >= 300:
                raise SystemExit(f"publication refused: {r.status_code} {r.text[:500]}")
            refused = r.json().get("refused") or []
            if refused:
                result.setdefault("refused", 0)
                result["refused"] += len(refused)
            sent += len(batch)
            members += sum(len(a["members"]["gbifid"]) for a in batch)
        result["publish"] = {
            "artifacts": sent,
            "members": members,
            "requests": len(all_batches),
            "ms": round((time.perf_counter() - t0) * 1000.0, 1),
        }
        print(json.dumps(result["publish"]), flush=True)
        result["status_before_reads"] = requests.get(
            f"{served.control}/control/status", headers=control_headers, timeout=60
        ).json()["masked_count_cache"]
        for p in [p for p in run["principals"] if p["label"] in ("100%", "1%")]:
            r = requests.post(
                f"{served.session}/session/authorise",
                json={"terms": p["term_list"]},
                headers=control_headers,
                timeout=120,
            )
            r.raise_for_status()
            http = requests.Session()
            http.headers["Authorization"] = f"Bearer {r.json()['token']}"
            meta = http.get(f"{served.viewer}/v1/meta", timeout=120).json()
            view = meta["views"][0]
            q = view["quantisation"]
            world = [(q["x_min"] + q["x_max"]) / 2, (q["y_min"] + q["y_max"]) / 2]
            for layer, levels in ((LAYER, None), ("taxonomy/tree", [1])):
                for z, centre in ((0, world), (9, p["regions"][0]["centre"])):
                    body = {
                        "view": view["id"],
                        "zoom": z + 2,
                        "bbox": box_at(q, centre, z),
                        "layers": [layer],
                        "per_tile": 50,
                    }
                    if levels is not None:
                        body["levels"] = levels
                    first = ask(http, served.viewer, body)
                    warm = sorted(ask(http, served.viewer, body)["ms"] for _ in range(REPEATS))
                    row = {
                        "principal": p["label"],
                        "layer": layer,
                        "map_zoom": z,
                        "depth": z + 2,
                        "tiles": first["frames"],
                        "bytes": first["bytes"],
                        "first_ms": first["ms"],
                        "first_ended_at": first["at"],
                        "warm_p50_ms": warm[math.ceil(len(warm) / 2) - 1],
                        "warm_max_ms": warm[-1],
                    }
                    result["reads"].append(row)
                    print(json.dumps(row), flush=True)
        result["status_after_reads"] = requests.get(
            f"{served.control}/control/status", headers=control_headers, timeout=60
        ).json()["masked_count_cache"]
    finally:
        served.stop()
    result["log"] = [
        line
        for line in (served.scratch / "serve.log").read_text().splitlines()
        if LAYER in line
    ]
    Path(out).write_text(json.dumps(result, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
