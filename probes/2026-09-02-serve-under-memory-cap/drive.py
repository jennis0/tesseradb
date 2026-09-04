#!/usr/bin/env python3
"""Drive a running tessera serve (medcpt bundle) with a fixed request sequence, and report
p50/p99 per request kind. Used to compare 4G / 12G / no-cap runs and to check byte-identical
correctness of counts across caps.

Usage: python3 drive.py --viewer http://127.0.0.1:8121 --session http://127.0.0.1:8122 \
    --session-cred $TESSERA_MEDCPT_SESSION_CRED --out results-4g.json [--save-bodies]
"""
import argparse
import base64
import json
import statistics
import sys
import time

import requests
from pyarrow import ipc
import io

EXTENT = 65536.0


def tile_counts(content: bytes):
    """Decode the response's first frame (u8 kind, u32 LE length, payload -- always a tiles
    frame first, `core/frame.ts`'s convention) and return (sum visible, sum matched, n rows).
    `matched` is absent when no filter was asked; treated as None then. Point sampling for `k`
    marks is not claimed deterministic across runs, so this -- not a raw byte digest -- is the
    correctness check: the masked counts, not which points a sample happened to pick."""
    if len(content) < 5:
        return None
    kind = content[0]
    if kind != 1:
        return None
    length = int.from_bytes(content[1:5], "little")
    # The tiles frame is Arrow IPC *stream* format (0xFFFFFFFF continuation marker, no file
    # footer) -- `tableFromIPC` in JS auto-detects; pyarrow needs the stream reader explicitly.
    table = ipc.open_stream(io.BytesIO(content[5 : 5 + length])).read_all()
    visible = sum(int(v) for v in table.column("visible").to_pylist()) if "visible" in table.column_names else None
    matched = sum(int(v) for v in table.column("matched").to_pylist()) if "matched" in table.column_names else None
    return {"visible": visible, "matched": matched, "n_tiles": table.num_rows}

BROAD_TERMS = ["B", "E", "C", "G", "D", "N", "A", "M", "unindexed", "F", "Z", "H", "I", "L", "J", "K", "V"]
NARROW_TERMS = ["V"]  # 4,910 pairs -- rarest MeSH branch
MEDIUM_TERMS = ["K", "J"]  # ~4M pairs combined -- a mid band

PRINCIPALS = {
    "narrow": NARROW_TERMS,
    "medium": MEDIUM_TERMS,
    "broad": BROAD_TERMS,
}


def authorise(session_base, cred, terms):
    auth_data = base64.b64encode(json.dumps({"terms": terms}).encode()).decode()
    r = requests.post(
        f"{session_base}/session/authorise",
        headers={"Authorization": f"Bearer {cred}"},
        json={"auth_data": auth_data},
        timeout=30,
    )
    r.raise_for_status()
    return r.json()["token"]


def meta(viewer_base, token):
    r = requests.get(f"{viewer_base}/v1/meta", headers={"Authorization": f"Bearer {token}"}, timeout=10)
    r.raise_for_status()
    return r.json()


def viewport(viewer_base, token, view_id, zoom, bbox, k=30, filters=None, extra=None):
    body = {"view": view_id, "zoom": zoom, "bbox": list(bbox), "k": k, "layers": "all"}
    if filters is not None:
        body["filters"] = filters
    if extra:
        body.update(extra)
    t0 = time.perf_counter()
    r = requests.post(
        f"{viewer_base}/v1/viewport",
        headers={"Authorization": f"Bearer {token}"},
        json=body,
        timeout=30,
    )
    dt = time.perf_counter() - t0
    r.raise_for_status()
    server_us = int(r.headers.get("x-tessera-server-us", "0"))
    counts = tile_counts(r.content)
    return {
        "wall_ms": dt * 1000,
        "server_ms": server_us / 1000,
        "bytes": len(r.content),
        "sha256": __import__("hashlib").sha256(r.content).hexdigest(),
        "counts": counts,
    }


def browse(viewer_base, token, **body):
    t0 = time.perf_counter()
    r = requests.post(
        f"{viewer_base}/v1/artifacts/browse",
        headers={"Authorization": f"Bearer {token}"},
        json={k: v for k, v in body.items() if v is not None},
        timeout=30,
    )
    dt = time.perf_counter() - t0
    server_us = int(r.headers.get("x-tessera-server-us", "0")) if r.ok else 0
    return {
        "wall_ms": dt * 1000,
        "server_ms": server_us / 1000,
        "status": r.status_code,
        "bytes": len(r.content) if r.ok else 0,
        "sha256": __import__("hashlib").sha256(r.content).hexdigest() if r.ok else None,
        "body": r.text[:300] if not r.ok else None,
    }


def pan_sequence(q):
    """A handful of zoom levels with a small pan sequence at each -- ~300-tile scale requests."""
    x0, y0, x1, y1 = q["x_min"], q["y_min"], q["x_max"], q["y_max"]
    w = x1 - x0
    h = y1 - y0
    seqs = []
    for zoom in (0, 3, 6, 9, 12):
        span_w = w / (2 ** min(zoom, 6))
        span_h = h / (2 ** min(zoom, 6))
        for i, (fx, fy) in enumerate([(0.5, 0.5), (0.3, 0.3), (0.7, 0.3), (0.3, 0.7), (0.7, 0.7)]):
            cx = x0 + fx * w
            cy = y0 + fy * h
            bbox = [cx - span_w / 2, cy - span_h / 2, cx + span_w / 2, cy + span_h / 2]
            seqs.append((zoom, bbox))
    return seqs


def pct(vals, p):
    if not vals:
        return None
    s = sorted(vals)
    idx = min(int(len(s) * p), len(s) - 1)
    return s[idx]


def summarise(samples):
    server = [s["server_ms"] for s in samples]
    wall = [s["wall_ms"] for s in samples]
    return {
        "n": len(samples),
        "server_p50_ms": pct(server, 0.5),
        "server_p99_ms": pct(server, 0.99),
        "server_max_ms": max(server) if server else None,
        "wall_p50_ms": pct(wall, 0.5),
        "wall_p99_ms": pct(wall, 0.99),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--viewer", required=True)
    ap.add_argument("--session", required=True)
    ap.add_argument("--session-cred", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    results = {"kinds": {}, "correctness": {}}

    results["died"] = None

    def alive():
        try:
            requests.get(f"{args.viewer}/readyz", timeout=3)
            return True
        except requests.exceptions.RequestException:
            return False

    def record_kind(kind, fn):
        """Run fn() -> (samples, digests); catch a server death mid-sequence and keep partials."""
        samples, digests = [], []
        try:
            for s, d in fn():
                samples.append(s)
                digests.append(d)
        except requests.exceptions.RequestException as e:
            results["died"] = {"during": kind, "error": str(e), "server_alive_after": alive()}
        results["kinds"][kind] = summarise(samples) if samples else None
        results["correctness"][kind] = digests
        return results["died"] is None

    tokens = {}
    for label, terms in PRINCIPALS.items():
        tokens[label] = authorise(args.session, args.session_cred, terms)
    results["principals"] = {label: terms for label, terms in PRINCIPALS.items()}

    m = meta(args.viewer, tokens["narrow"])
    view_id = m["views"][0]["id"]
    q = m["views"][0]["quantisation"]
    results["view"] = view_id

    mesh_view = None
    for v in m["views"]:
        if "mesh" in v.get("id", "") or "descriptors" in v.get("id", ""):
            mesh_view = v["id"]
    results["mesh_view"] = mesh_view

    # 1. viewport pan sequence across zoom levels, per principal -- narrow and medium first
    # (cheap), broad last (the expensive one, most likely to exhaust a small cap).
    for label in ("narrow", "medium", "broad"):
        token = tokens[label]

        def gen(token=token):
            for zoom, bbox in pan_sequence(q):
                s = viewport(args.viewer, token, view_id, zoom, bbox, k=30)
                yield s, {"zoom": zoom, "bbox": bbox, "sha256": s["sha256"], "bytes": s["bytes"], "counts": s["counts"]}

        if not record_kind(f"viewport_pan_{label}", gen):
            break

    # 2. match filter, common and rare tokens, narrow principal (cheapest to survive a cap)
    if results["died"] is None:
        for word in ("of", "zzzxyq_rare_token_probe"):

            def gen(word=word):
                for zoom, bbox in pan_sequence(q)[:5]:
                    s = viewport(
                        args.viewer, tokens["narrow"], view_id, zoom, bbox, k=30,
                        filters={"title": {"match": word}},
                    )
                    yield s, {"zoom": zoom, "bbox": bbox, "sha256": s["sha256"], "bytes": s["bytes"], "counts": s["counts"]}

            if not record_kind(f"match_title_{word}", gen):
                break

    # 3. drill-down
    if results["died"] is None:
        def gen():
            for h in range(1, 21):
                t0 = time.perf_counter()
                r = requests.post(
                    f"{args.viewer}/v1/items/{h}",
                    headers={"Authorization": f"Bearer {tokens['narrow']}"},
                    json={},
                    timeout=10,
                )
                dt = time.perf_counter() - t0
                r.raise_for_status() if r.status_code not in (200, 404) else None
                server_us = int(r.headers.get("x-tessera-server-us", "0")) if "x-tessera-server-us" in r.headers else 0
                yield {"wall_ms": dt * 1000, "server_ms": server_us / 1000}, {"status": r.status_code}

        record_kind("item_drilldown", gen)

    # 4. artifact frame -- mesh/descriptors. There is no separate browse route in this build
    # (`/v1/artifacts/browse` in reference/oracle/harness.py is not registered by
    # crates/tessera-server/src/viewer.rs here); the DAG layer's artifact frames ride the ordinary
    # `/v1/viewport` response when `layers` includes it, which `layers: "all"` already does above
    # -- so `viewport_pan_*` already exercises "the artifact frame with mesh/descriptors". This
    # step additionally drills into a few artifact ids via `POST /v1/artifacts/{tessera_id}`.
    if results["died"] is None:
        def gen():
            for h in range(1, 11):
                t0 = time.perf_counter()
                r = requests.post(
                    f"{args.viewer}/v1/artifacts/{h}",
                    headers={"Authorization": f"Bearer {tokens['narrow']}"},
                    json={},
                    timeout=10,
                )
                dt = time.perf_counter() - t0
                r.raise_for_status() if r.status_code not in (200, 404, 422) else None
                server_us = int(r.headers.get("x-tessera-server-us", "0")) if "x-tessera-server-us" in r.headers else 0
                yield {"wall_ms": dt * 1000, "server_ms": server_us / 1000}, {"status": r.status_code}

        record_kind("artifact_drilldown_mesh", gen)
    results["note_mesh_view"] = "mesh/descriptors is a layer, not a view; served via viewport layers=all"

    with open(args.out, "w") as f:
        json.dump(results, f, indent=2)
    print(f"wrote {args.out}")
    if results["died"]:
        print(f"  SERVER DIED during {results['died']['during']}: {results['died']['error']}")
    for k, v in results["kinds"].items():
        if v:
            print(f"  {k}: n={v['n']} server p50={v['server_p50_ms']:.3f}ms p99={v['server_p99_ms']:.3f}ms "
                  f"wall p50={v['wall_p50_ms']:.1f}ms p99={v['wall_p99_ms']:.1f}ms")


if __name__ == "__main__":
    sys.exit(main())
