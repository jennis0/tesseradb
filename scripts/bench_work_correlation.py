#!/usr/bin/env python3
"""Experiment 3 of the tail-attribution investigation (2026-07-30): characterise the
per-request work distribution of a random-viewport sweep and correlate it against latency.

For every request, records (decoding the full Arrow response, not a 50-sample subset, because
Sigma-visible/Sigma-matched must be summed over ALL tiles, not just the point-sample rows):

  - tiles resolved (len(tiles_for_bbox) -- recovered as len(tile rows in the first Arrow batch))
  - non-empty tiles (== len(tile rows); Engine::viewport only emits a TileCount for non-empty tiles)
  - Sigma visible, Sigma matched (masked density -- I2-safe: this is M_auth-filtered, never the
    raw corpus density) over the WHOLE viewport, uncapped by k
  - points returned (k-capped per tile -- saturates as tile density exceeds k)
  - response bytes
  - server_us latency

Then reports Pearson correlation of server_us against each work measure, and latency normalised
per unit of each (us per row-visible, us per point-gathered, us per tile). Falsifiable prediction
under test (coordinator note, 2026-07-30): at FIXED k, latency should correlate with
Sigma-visible/tiles and NOT with points-returned once points-returned saturates; if instead
latency tracks points-returned and is flat against Sigma-visible, the tail lives in the
gather/serialisation path, not the count loop.

Same boot/authorise recipe as bench_k_sweep.py (w=10^4, seed 0), same viewport generator
(gen_viewports) so this sweep's geometry is drawn from the same population as the existing
baseline.

Usage:
  reference/.venv/bin/python scripts/bench_work_correlation.py \
      --bundle /tmp/tessera-1e9 --out probes/work-correlation.json
"""

from __future__ import annotations

import argparse
import json
import random
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "reference"))
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import bench_k_sweep as bks  # noqa: E402
from oracle.wire import decode_viewport  # noqa: E402

EXTENT = 65536.0
K_VALUES = [50, 1000]


def pearson(xs: list[float], ys: list[float]) -> float:
    n = len(xs)
    if n < 2:
        return float("nan")
    mx = sum(xs) / n
    my = sum(ys) / n
    cov = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    vx = sum((x - mx) ** 2 for x in xs)
    vy = sum((y - my) ** 2 for y in ys)
    if vx == 0 or vy == 0:
        return float("nan")
    return cov / (vx**0.5 * vy**0.5)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", default="/tmp/tessera-1e9")
    ap.add_argument("--tmp", default="/tmp/tessera-1e9-serve-workcorr")
    ap.add_argument("-n", "--n-viewports", type=int, default=1000)
    ap.add_argument("--width", type=int, default=10_000)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--boot-deadline", type=float, default=1800.0)
    ap.add_argument("--max-k-config", type=int, default=6000)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    bundle_root = Path(args.bundle)
    if not (bundle_root / "CURRENT").exists():
        print(f"ERROR: no bundle at {bundle_root}", file=sys.stderr)
        return 1

    tmp_dir = Path(args.tmp)
    tmp_dir.mkdir(parents=True, exist_ok=True)

    frozen_bytes = sum(f.stat().st_size for f in bundle_root.rglob("*") if f.is_file())
    print(f"Bundle on-disk size: {frozen_bytes / 1e9:.2f} GB ({bundle_root})")

    print("Booting server ONCE...")
    srv, proc, boot_elapsed = bks.spawn_with_long_boot_deadline(
        bundle_root, tmp_dir, args.boot_deadline, args.max_k_config
    )
    print(f"Boot time: {boot_elapsed:.1f}s")

    results = {
        "bundle": str(bundle_root),
        "boot_seconds": boot_elapsed,
        "n_viewports": args.n_viewports,
        "seed": args.seed,
        "k_sweep": {},
    }

    try:
        descriptors = bks.read_dictionary_descriptors(bundle_root)
        rng = random.Random(args.seed)
        w = min(args.width, len(descriptors))
        grant = rng.sample(descriptors, w)

        t0 = time.perf_counter()
        auth = srv.authorise(grant)
        token = auth["token"]
        results["authorise_seconds"] = time.perf_counter() - t0
        results["grant_width"] = w
        print(f"authorise() [w={w}]: {results['authorise_seconds'] * 1000:.1f} ms")

        warm_resp = srv.viewport_response(token, "s0", 6, [0.0, 0.0, EXTENT, EXTENT], k=30)
        results["warmup_server_us"] = int(warm_resp.headers.get("x-tessera-server-us", "0"))
        print(f"Warm-up server-side: {results['warmup_server_us'] / 1000:.3f} ms")

        viewports = bks.gen_viewports(args.seed + 1, args.n_viewports)

        for k in K_VALUES:
            print(f"\n=== k={k}, n={len(viewports)} ===")
            per_request = []
            for i, (zoom, bbox) in enumerate(viewports):
                resp = srv.viewport_response(token, "s0", zoom, bbox, k=k)
                server_us = float(resp.headers.get("x-tessera-server-us", "nan"))
                body = resp.content
                tiles, points = decode_viewport(body)
                n_tiles = len(tiles)
                sigma_visible = sum(t[1] for t in tiles)
                sigma_matched = sum(t[2] for t in tiles)
                n_points = len(points)
                per_request.append(
                    {
                        "zoom": zoom,
                        "server_us": server_us,
                        "n_tiles_nonempty": n_tiles,
                        "sigma_visible": sigma_visible,
                        "sigma_matched": sigma_matched,
                        "points_returned": n_points,
                        "response_bytes": len(body),
                        "visible_over_points": (sigma_visible / n_points) if n_points else None,
                    }
                )
                if (i + 1) % 200 == 0:
                    print(f"  {i + 1}/{len(viewports)} issued...")

            server_us_list = [r["server_us"] for r in per_request]
            tiles_list = [r["n_tiles_nonempty"] for r in per_request]
            visible_list = [r["sigma_visible"] for r in per_request]
            matched_list = [r["sigma_matched"] for r in per_request]
            points_list = [r["points_returned"] for r in per_request]
            bytes_list = [r["response_bytes"] for r in per_request]

            correlations = {
                "server_us_vs_tiles": pearson(server_us_list, tiles_list),
                "server_us_vs_sigma_visible": pearson(server_us_list, visible_list),
                "server_us_vs_sigma_matched": pearson(server_us_list, matched_list),
                "server_us_vs_points_returned": pearson(server_us_list, points_list),
                "server_us_vs_response_bytes": pearson(server_us_list, bytes_list),
            }

            total_server_us = sum(server_us_list)
            normalised = {
                "us_per_row_visible": total_server_us / sum(visible_list) if sum(visible_list) else None,
                "us_per_point_gathered": total_server_us / sum(points_list) if sum(points_list) else None,
                "us_per_tile": total_server_us / sum(tiles_list) if sum(tiles_list) else None,
            }

            ratio_list = [r["visible_over_points"] for r in per_request if r["visible_over_points"]]

            k_result = {
                "server_us": {
                    "p50": bks.percentile(server_us_list, 0.50),
                    "p99": bks.percentile(server_us_list, 0.99),
                    "max": max(server_us_list),
                },
                "n_tiles_nonempty": {"mean": sum(tiles_list) / len(tiles_list), "max": max(tiles_list)},
                "sigma_visible": {"mean": sum(visible_list) / len(visible_list), "max": max(visible_list)},
                "sigma_matched": {"mean": sum(matched_list) / len(matched_list), "max": max(matched_list)},
                "points_returned": {"mean": sum(points_list) / len(points_list), "max": max(points_list)},
                "visible_over_points_returned": {
                    "mean": sum(ratio_list) / len(ratio_list) if ratio_list else None,
                    "min": min(ratio_list) if ratio_list else None,
                    "max": max(ratio_list) if ratio_list else None,
                },
                "correlations": correlations,
                "normalised_cost": normalised,
            }
            results["k_sweep"][str(k)] = k_result
            results.setdefault("per_request_samples", {})[str(k)] = per_request[:50]
            print(json.dumps(k_result, indent=2, default=str))

        print("\n" + json.dumps({k2: v for k2, v in results.items() if k2 != "per_request_samples"}, indent=2, default=str))
        if args.out:
            Path(args.out).write_text(json.dumps(results, indent=2, default=str))
        return 0
    finally:
        bks.harness.stop_server(proc)


if __name__ == "__main__":
    raise SystemExit(main())
