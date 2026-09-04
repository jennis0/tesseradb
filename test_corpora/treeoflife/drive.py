#!/usr/bin/env python3
"""Drive a running `tessera serve` with a fixed request sequence, and report p50/p99 per kind.

**Rung 5's copy runs the sequence once per view.** This is the ladder's first corpus with two
geometries over one entity space, and the two are not interchangeable: `bioclip` holds every row
and `geo` holds the 75.9% the GBIF join placed on the ground, so a principal's visible count
differs between them by the join rate and not by the mask. Every viewport kind is therefore named
`viewport_pan_<view>_<principal>` and each view's own `quantisation` supplies its pan sequence.

**The system `python3`, not `~/venvs/projection`** — it needs `requests` and `pyarrow` and nothing
the rung's environment carries, and the probe this is a copy of ran the same way.

Rung 4's copy of `probes/2026-09-02-serve-under-memory-cap/drive.py`, with the principals and the
`match` field taken from the command line instead of MedCPT's MeSH branch letters — so the same
sequence runs uncapped and under a cgroup cap and the two are comparable request for request.

    python3 test_corpora/treeoflife/drive.py \\
        --viewer http://127.0.0.1:8141 --session http://127.0.0.1:8142 \\
        --session-cred "$TESSERA_TREEOFLIFE_SESSION_CRED" \\
        --principals '{"none": [], "iNaturalist": ["iNaturalist"], "all": [...]}' \\
        --match-field common_name --match-token butterfly --out results-nocap.json

**The correctness check is the masked counts, not the bytes.** `k` point sampling is not specified
as deterministic across processes and is not observed to be: two runs of the same request return
different bytes at the same length and identical `visible`/`matched` sums. Every count-bearing
response is decoded and summed, and a comparison across runs is over those sums.

⊘ **There is no `/v1/artifacts/browse` route in this build.** An artifact frame rides the ordinary
`/v1/viewport` response when `layers` includes it, which `layers: "all"` already asks for; the
artifact step below additionally drills into a few ids through `POST /v1/artifacts/{id}`.
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

#: The principals, when none are given. This rung's compartment is the publishing institution and
#: every row carries a term, so a principal holding none sees **nothing** — which is the ladder's
#: first rung and the ruling's whole point. Overridden by `--principals`.
DEFAULT_PRINCIPALS = {"none": [], "unpublished": ["unpublished"]}


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
    ap.add_argument("--principals", default=None,
                    help='JSON object of label -> access terms; default '
                         + json.dumps(DEFAULT_PRINCIPALS))
    ap.add_argument("--views", default=None,
                    help="comma-separated view names; default every view /v1/meta reports")
    ap.add_argument("--match-field", default="common_name",
                    help="the indexed text column the `match` filter is asked over")
    ap.add_argument("--match-token", default="butterfly",
                    help="a token expected to be common in that column")
    args = ap.parse_args()

    principals = json.loads(args.principals) if args.principals else dict(DEFAULT_PRINCIPALS)
    order = list(principals)

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
    for label, terms in principals.items():
        tokens[label] = authorise(args.session, args.session_cred, terms)
    results["principals"] = dict(principals)

    m = meta(args.viewer, tokens[order[-1]])
    wanted = args.views.split(",") if args.views else None
    views = [v for v in m["views"] if wanted is None or v["id"] in wanted]
    assert views, f"none of {wanted} is a view of this bundle: {[v['id'] for v in m['views']]}"
    results["views"] = [{"id": v["id"], "display_name": v.get("display_name")} for v in views]
    results["view"] = views[0]["id"]

    # 1. viewport pan sequence across zoom levels, per view and per principal -- narrowest
    # principal first, so a cap that cannot serve the broadest still leaves the cheaper ones
    # measured, and each view fitted to its own frame.
    for v in views:
        view_id, q, name = v["id"], v["quantisation"], v["id"]
        for label in order:
            token = tokens[label]

            def gen(token=token, view_id=view_id, q=q):
                for zoom, bbox in pan_sequence(q):
                    s = viewport(args.viewer, token, view_id, zoom, bbox, k=30)
                    yield s, {"zoom": zoom, "bbox": bbox, "sha256": s["sha256"], "bytes": s["bytes"], "counts": s["counts"]}

            if not record_kind(f"viewport_pan_{name}_{label}", gen):
                break
        if results["died"]:
            break

    # 2. match filter, common and absent tokens, on the **broadest** principal. The probe this is
    # a copy of used its narrowest, so that a cap too small for the broad one still measured
    # something; here the narrowest principal holds no term and sees nothing, so a `match` against
    # it would report zero matched at every zoom and measure the mask rather than the text index.
    if results["died"] is None:
        first = views[0]
        for word in (args.match_token, "zzzxyq_rare_token_probe"):

            def gen(word=word, view_id=first["id"], q=first["quantisation"]):
                for zoom, bbox in pan_sequence(q)[:5]:
                    s = viewport(
                        args.viewer, tokens[order[-1]], view_id, zoom, bbox, k=30,
                        filters={args.match_field: {"match": word}},
                    )
                    yield s, {"zoom": zoom, "bbox": bbox, "sha256": s["sha256"], "bytes": s["bytes"], "counts": s["counts"]}

            if not record_kind(f"match_{args.match_field}_{word}", gen):
                break

    # 3. drill-down
    if results["died"] is None:
        def gen():
            for h in range(1, 21):
                t0 = time.perf_counter()
                r = requests.post(
                    f"{args.viewer}/v1/items/{h}",
                    headers={"Authorization": f"Bearer {tokens[order[-1]]}"},
                    json={},
                    timeout=10,
                )
                dt = time.perf_counter() - t0
                r.raise_for_status() if r.status_code not in (200, 404) else None
                server_us = int(r.headers.get("x-tessera-server-us", "0")) if "x-tessera-server-us" in r.headers else 0
                yield {"wall_ms": dt * 1000, "server_ms": server_us / 1000}, {"status": r.status_code}

        record_kind("item_drilldown", gen)

    # 4. artifact drill-down. The layers' artifact frames already rode every `viewport_pan_*`
    # response above (`layers: "all"`); this drills into a few artifact ids directly.
    if results["died"] is None:
        def gen():
            for h in range(1, 11):
                t0 = time.perf_counter()
                r = requests.post(
                    f"{args.viewer}/v1/artifacts/{h}",
                    headers={"Authorization": f"Bearer {tokens[order[-1]]}"},
                    json={},
                    timeout=10,
                )
                dt = time.perf_counter() - t0
                r.raise_for_status() if r.status_code not in (200, 404, 422) else None
                server_us = int(r.headers.get("x-tessera-server-us", "0")) if "x-tessera-server-us" in r.headers else 0
                yield {"wall_ms": dt * 1000, "server_ms": server_us / 1000}, {"status": r.status_code}

        record_kind("artifact_drilldown", gen)

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
