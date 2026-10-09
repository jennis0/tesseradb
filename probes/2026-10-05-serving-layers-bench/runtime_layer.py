"""A flat layer published at runtime on gbif-64p, and its first reads on the artifacts viewport.

    python3 probes/2026-10-05-serving-layers-bench/runtime_layer.py --deployment <dir> \
        --binary <mosaica> --run <bench run.json> --out <out.json> [--cap 24G] [--swap 2G]

It copies the deployment's bundle into a scratch directory, since a publication writes into the
bundle it serves, and starts the binary on a fresh cache over the copy, with its pages evicted.
It registers `genus-flat` (the taxonomy's genus level declared again as a flat layer) and
publishes its artifacts from the corpus's taxonomy member file by `gbifid`. Then, for the 100%
and 1% viewers of the bench run, it asks the artifacts viewport for the world at depth 2 and for
the viewer's densest region at map zoom 9, depth 11: the first request, then five more. The
built-in genus level is asked the same way, after the copy's pages are evicted again. The
server's log lines about the new layer are kept, with their times, and the copy is removed.
"""

from __future__ import annotations

import argparse
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

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from test_corpora.common.deployment import Deployment, tomllib
from test_corpora.common.interactive_bench import box_at, evict, size_bytes
from test_corpora.common.serve_battery import frames

LAYER = "genus-flat"
REPEATS = 5
#: Kept under the 10,000 artifacts and 64 MiB of a publication request.
BATCH_ARTIFACTS = 10_000
BATCH_MEMBERS = 3_000_000


def members_file(deployment: Path) -> Path:
    """The taxonomy member file the deployment's corpus declaration names."""
    schema = (deployment / tomllib.loads((deployment / "mosaica.toml").read_text())["build"]["schema"]).resolve()
    return schema.parent / tomllib.loads(schema.read_text())["sources"]["taxonomy"]


def genus_members(path: Path) -> dict[str, list[int]]:
    table = pq.read_table(path)
    out: dict[str, list[int]] = defaultdict(list)
    for e, k in zip(table.column("entity").to_pylist(), table.column("key").to_pylist()):
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


def publish(served: Deployment, groups: dict[str, list[int]], result: dict) -> None:
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
    headers = served.control_headers()
    t0 = time.perf_counter()
    r = requests.put(f"{served.control}/control/layers", json=declaration, headers=headers, timeout=120)
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
            headers=headers,
            timeout=600,
        )
        if r.status_code >= 300:
            raise SystemExit(f"publication refused: {r.status_code} {r.text[:500]}")
        result["refused"] = result.get("refused", 0) + len(r.json().get("refused") or [])
        sent += len(batch)
        members += sum(len(a["members"]["gbifid"]) for a in batch)
    result["publish"] = {
        "artifacts": sent,
        "members": members,
        "requests": len(all_batches),
        "ms": round((time.perf_counter() - t0) * 1000.0, 1),
    }
    print(json.dumps(result["publish"]), flush=True)


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
    groups = genus_members(members_file(args.deployment))
    scratch = args.deployment / "bench-scratch-runtime-layer"
    shutil.rmtree(scratch, ignore_errors=True)
    scratch.mkdir(parents=True)
    settings = tomllib.loads((args.deployment / "mosaica.toml").read_text())
    original = (args.deployment / settings["bundle"]["path"]).resolve()
    copy = scratch / "bundle"
    served = Deployment.of(
        args.deployment,
        args.binary,
        scratch,
        cap_bytes=size_bytes(args.cap),
        swap_bytes=size_bytes(args.swap),
        bundle=copy,
    )
    result: dict = {"binary": str(args.binary), "bundle": str(original), "layer": LAYER, "reads": []}
    try:
        shutil.copytree(original, copy)
        # A page still dirty from the copy cannot be dropped until it is written.
        os.sync()
        evict(copy)
        served.start()
        publish(served, groups, result)
        result["status_before_reads"] = served.figures_status()
        for p in [p for p in run["principals"] if p["label"] in ("100%", "1%")]:
            http = served.viewer_session(p["term_list"])
            meta = http.get(f"{served.viewer}/v1/meta", timeout=120).json()
            view = meta["views"][0]
            q = view["quantisation"]
            world = [(q["x_min"] + q["x_max"]) / 2, (q["y_min"] + q["y_max"]) / 2]
            for layer, levels in ((LAYER, None), ("taxonomy/tree", [1])):
                if layer != LAYER:
                    evict(copy)
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
        result["status_after_reads"] = served.figures_status()
    finally:
        served.stop()
        log = scratch / "serve.log"
        if log.is_file():
            result["log"] = [line for line in log.read_text().splitlines() if LAYER in line]
        args.out.write_text(json.dumps(result, indent=1))
        shutil.rmtree(scratch, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
