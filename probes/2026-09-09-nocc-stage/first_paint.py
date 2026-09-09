#!/usr/bin/env python3
"""What a **cold session's first request** spends, stage by stage, from the response trailer.

One fresh `/session/authorise` per row, so every row is a session the server has never seen: a cold
row projection and a cold `N_occ` ladder, which is what a first paint is. `/v1/meta` is fetched in
between because a browser fetches it in between, and `--settle` inserts the rest of whatever gap a
client leaves before its first viewport — the gap the authorise-time occupancy stage
(`crates/tessera-engine/src/stage.rs`) has to finish inside.

`stage_ns` is a positional CSV and needs `serve.stage_timing = true` and a `bench-timing` build;
see `crates/tessera-server/src/viewer.rs::stage_header`, whose field order this mirrors.

**The window is centred on data and shrinks with depth.** `N_occ` is viewport-invariant by §7.2, so
`theta_occupancy_ns` is a function of the depth alone and the bbox cannot move it; the window is
sized and placed so that the *rest* of the request is one a client would actually make — a
full-extent request at depth 10 spans `4^10` tiles and is refused, and one placed in an empty corner
draws nothing and times a sweep over no rows.

    first_paint.py --viewer URL --session URL --credential C --terms terms.json
                   [--view bioclip] [--reps 3] [--settle 0] [--depths 3,4,6,...] [--json out.json]
"""
import argparse
import base64
import json
import os
import statistics
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "reference"))
import requests
from oracle import morton
from oracle.wire import decode_frames

NAMES = ["generation_resolve_ns", "stamp_compare_ns", "view_lookup_ns", "row_projection_ns",
         "compose_ns", "tiles_for_bbox_ns", "tile_ranges_ns", "count_ns", "select_ns", "gather_ns",
         "arrow_serialise_ns", "total_ns", "tiles_resolved", "tiles_nonempty", "sigma_visible",
         "rows_in_ranges", "select_rows_visited", "points_gathered", "row_projection_built",
         "theta_anchor_ns", "underlay_ns", "underlay_cells_evaluated", "shape_guard_fired",
         "theta_occupancy_ns"]

ap = argparse.ArgumentParser()
ap.add_argument("--viewer", required=True)
ap.add_argument("--session", required=True)
ap.add_argument("--credential", required=True)
ap.add_argument("--terms", required=True, help="JSON file: the credential's descriptor list")
ap.add_argument("--view", default="bioclip")
ap.add_argument("--reps", type=int, default=3)
ap.add_argument("--settle", type=float, default=0.0,
                help="seconds between authorise and the first viewport")
ap.add_argument("--depths", default="3,4,6,8,10,12,13,16")
ap.add_argument("--json", default=None)
ap.add_argument("--window-tiles", type=int, default=32,
                help="the window's side in tiles; 32x32 = 1,024, the order a budgeted client asks "
                     "for (B / m_target, clients/ts/core/src/budget.ts)")
args = ap.parse_args()
DEPTHS = [int(d) for d in args.depths.split(",")]
TERMS = json.load(open(args.terms))


def authorise():
    auth = base64.b64encode(json.dumps({"terms": TERMS}).encode()).decode()
    r = requests.post(f"{args.session}/session/authorise",
                      headers={"Authorization": f"Bearer {args.credential}"},
                      json={"auth_data": auth}, timeout=600)
    r.raise_for_status()
    return r.json()


def meta(token):
    r = requests.get(f"{args.viewer}/v1/meta", headers={"Authorization": f"Bearer {token}"},
                     timeout=120)
    r.raise_for_status()
    return r.json()


def viewport(token, zoom, bbox, k=500):
    r = requests.post(f"{args.viewer}/v1/viewport", headers={"Authorization": f"Bearer {token}"},
                      json={"view": args.view, "zoom": zoom, "bbox": list(bbox), "k": k},
                      timeout=1800)
    r.raise_for_status()
    tiles, _p, _s, _a, trailer = decode_frames(r.content)
    stats = dict(zip(NAMES, (int(x) for x in trailer["stage_ns"].split(","))))
    return tiles, stats


# The densest depth-6 tile, so every deeper window is placed over data rather than over empty
# extent. Taken once, on a throwaway session, and it doubles as the page-cache warm-up: the first
# pass over a 932 MB Morton column and a 8.3 GB column file reads from disk and everything after it
# does not.
warm = authorise()["token"]
q = next(v["quantisation"] for v in meta(warm)["views"] if v["id"] == args.view)
FULL = [q["x_min"], q["y_min"], q["x_max"], q["y_max"]]
W, H = q["x_max"] - q["x_min"], q["y_max"] - q["y_min"]
tiles, _ = viewport(warm, 6, FULL)
# `(tile, visible, matched, served, highlighted)` — `oracle.wire.decode_frames`.
best = max(tiles, key=lambda t: t[1])
tx, ty = morton.deinterleave(best[0])
cx = q["x_min"] + (tx + 0.5) * W / 64
cy = q["y_min"] + (ty + 0.5) * H / 64
print(f"# densest depth-6 tile ({tx}, {ty}) holds {best[1]} visible; "
      f"windows centre on ({cx:.2f}, {cy:.2f})", file=sys.stderr, flush=True)


def window(depth):
    side = args.window_tiles
    if side >= (1 << depth):
        return FULL
    w, h = side * W / (1 << depth), side * H / (1 << depth)
    return [cx - w / 2, cy - h / 2, cx + w / 2, cy + h / 2]


for depth in DEPTHS:  # warm the page cache at every depth this run will measure
    viewport(warm, depth, window(depth))

rows = []
print(f"{'depth':>5} {'rep':>3} {'N_occ':>9} {'tiles':>6} {'proj_ms':>8} {'built':>5} "
      f"{'walk_ms':>8} {'sweep_ms':>9} {'total_ms':>9} {'warm_ms':>8}", flush=True)
for depth in DEPTHS:
    box = window(depth)
    for rep in range(args.reps):
        s = authorise()
        meta(s["token"])
        if args.settle:
            time.sleep(args.settle)
        _t, cold = viewport(s["token"], depth, box)
        _t, warm_stats = viewport(s["token"], depth, box)
        sweep = cold["count_ns"] + cold["select_ns"] + cold["gather_ns"]
        rows.append({"depth": depth, "rep": rep, "settle": args.settle, **cold,
                     "sweep_ns": sweep, "warm_total_ns": warm_stats["total_ns"]})
        print(f"{depth:>5} {rep:>3} {cold['tiles_nonempty']:>9} {cold['tiles_resolved']:>6} "
              f"{cold['row_projection_ns']/1e6:>8.1f} {cold['row_projection_built']:>5} "
              f"{cold['theta_occupancy_ns']/1e6:>8.1f} {sweep/1e6:>9.1f} "
              f"{cold['total_ns']/1e6:>9.1f} {warm_stats['total_ns']/1e6:>8.1f}", flush=True)

print("\n-- medians over reps, milliseconds --", flush=True)
print(f"{'depth':>5} {'proj':>8} {'walk':>8} {'sweep':>8} {'total':>9} {'warm':>8} "
      f"{'walk/total':>10} {'walk/warm':>10}")
summary = []
for depth in DEPTHS:
    d = [r for r in rows if r["depth"] == depth]
    med = lambda k: statistics.median(r[k] for r in d)
    total, walk, wrm = med("total_ns"), med("theta_occupancy_ns"), med("warm_total_ns")
    summary.append({"depth": depth, "settle": args.settle, "n_occ": med("tiles_nonempty"),
                    "row_projection_ns": med("row_projection_ns"),
                    "projection_built": med("row_projection_built"),
                    "theta_occupancy_ns": walk, "sweep_ns": med("sweep_ns"),
                    "total_ns": total, "warm_total_ns": wrm})
    print(f"{depth:>5} {med('row_projection_ns')/1e6:>8.1f} {walk/1e6:>8.1f} "
          f"{med('sweep_ns')/1e6:>8.1f} {total/1e6:>9.1f} {wrm/1e6:>8.1f} "
          f"{100 * walk / total:>9.1f}% {100 * walk / (wrm + walk):>9.1f}%")

if args.json:
    with open(args.json, "w") as f:
        json.dump({"settle": args.settle, "view": args.view, "rows": rows, "summary": summary},
                  f, indent=1)
