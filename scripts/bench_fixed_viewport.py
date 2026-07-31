#!/usr/bin/env python3
"""Experiment 1 of the tail-attribution investigation (2026-07-30): does the ~40 ms p99-p50 gap
survive when the viewport is held *fixed* across repeats, or does it collapse?

Leading hypothesis under test: the gap is *between-viewport work variance* (random bboxes at
random zooms 4..12 touch a wildly different number of tiles/rows per request), not a per-request
stall. `Engine::viewport` (crates/tessera-engine/src/viewport.rs) does `mask.count_range` over
every tile `tiles_for_bbox` resolves -- work that is independent of `k` -- then samples/gathers up
to `k` points per non-empty tile. If the count-loop's cost varies a lot across random geometry,
p99-p50 over a *random* sweep would show a gap that is roughly constant in k (because the
count-loop's cost doesn't depend on k) while p50 grows with k (because the gather does). A
*fixed* viewport, repeated many times, removes that geometry variance entirely: if the gap
collapses, the tail is workload variance, not a stall.

Same boot/authorise/warm-up recipe as bench_k_sweep.py (w=10^4, seed 0) so this is directly
comparable to the existing k-sweep baseline (docs/superpowers/plans/bench-baselines/
2026-07-29-1e9-k-sweep.json) at k=50 and k=1000. The fixed viewport is
`gen_viewports(seed=1, n=5)[0]` -- the same generator bench_k_sweep.py uses for its random sweep
(seed+1 where seed=0), first entry: zoom=6, bbox ~ a mid-sized span (the docstring's "~300 tiles"
target region), so this is a representative, not a cherry-picked extreme, member of the same
population the random sweep draws from.

Usage:
  reference/.venv/bin/python scripts/bench_fixed_viewport.py \
      --bundle /tmp/tessera-1e9 --out probes/fixed-viewport-repeat.json
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "reference"))
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import bench_k_sweep as bks  # noqa: E402 -- reused: gen_viewports, percentile, dictionary reader,
# spawn_with_long_boot_deadline, write_config_with_max_k

EXTENT = 65536.0
K_VALUES = [50, 1000]
N_REPEATS = 500


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", default="/tmp/tessera-1e9")
    ap.add_argument("--tmp", default="/tmp/tessera-1e9-serve-fixed")
    ap.add_argument("-n", "--n-repeats", type=int, default=N_REPEATS)
    ap.add_argument("--width", type=int, default=10_000, help="grant width w")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--boot-deadline", type=float, default=1800.0)
    ap.add_argument("--max-k-config", type=int, default=6000)
    ap.add_argument(
        "--k",
        default=",".join(str(k) for k in K_VALUES),
        help="comma-separated k values to sweep (default: %(default)s)",
    )
    ap.add_argument("--out", default=None)
    args = ap.parse_args()
    k_values = [int(k) for k in args.k.split(",") if k.strip()]

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
        "frozen_bundle_bytes": frozen_bytes,
        "boot_seconds": boot_elapsed,
        "n_repeats": args.n_repeats,
        "seed": args.seed,
        "fixed_viewport": None,
        "k_sweep_fixed": {},
    }

    try:
        descriptors = bks.read_dictionary_descriptors(bundle_root)
        print(f"Dictionary vocab: {len(descriptors)} terms")
        results["dictionary_vocab"] = len(descriptors)

        import random

        rng = random.Random(args.seed)
        w = min(args.width, len(descriptors))
        grant = rng.sample(descriptors, w)

        t0 = time.perf_counter()
        auth = srv.authorise(grant)
        token = auth["token"]
        authorise_s = time.perf_counter() - t0
        results["grant_width"] = w
        results["authorise_seconds"] = authorise_s
        print(f"authorise() [w={w}]: {authorise_s * 1000:.1f} ms")

        # Warm-up: same recipe as bench_k_sweep.py (row-projection cache fill).
        t0 = time.perf_counter()
        warm_resp = srv.viewport_response(token, "s0", 6, [0.0, 0.0, EXTENT, EXTENT], k=30)
        warmup_s = time.perf_counter() - t0
        warmup_server_us = int(warm_resp.headers.get("x-tessera-server-us", "0"))
        results["warmup_end_to_end_ms"] = warmup_s * 1000
        results["warmup_server_us"] = warmup_server_us
        print(f"Warm-up: {warmup_s * 1000:.1f} ms end-to-end, {warmup_server_us / 1000:.3f} ms server-side")

        # The SAME single viewport used for both k values, chosen as the first draw from the same
        # generator (seed+1) bench_k_sweep.py's random sweep uses -- a representative member of
        # that population, not an extreme.
        viewports = bks.gen_viewports(args.seed + 1, 5)
        zoom, bbox = viewports[0]
        results["fixed_viewport"] = {"zoom": zoom, "bbox": bbox}
        print(f"Fixed viewport: zoom={zoom} bbox={bbox}")

        for k in k_values:
            print(f"\n=== fixed viewport, k={k}, {args.n_repeats} repeats ===")
            server_us: list[float] = []
            e2e_us: list[float] = []
            resp_bytes: list[int] = []

            for i in range(args.n_repeats):
                t0 = time.perf_counter()
                resp = srv.viewport_response(token, "s0", zoom, bbox, k=k)
                e2e = (time.perf_counter() - t0) * 1e6
                e2e_us.append(e2e)
                server_us.append(float(resp.headers.get("x-tessera-server-us", "nan")))
                resp_bytes.append(len(resp.content))
                if (i + 1) % 100 == 0:
                    print(f"  {i + 1}/{args.n_repeats} issued...")

            k_result = {
                "server_us": {
                    "p50": bks.percentile(server_us, 0.50),
                    "p99": bks.percentile(server_us, 0.99),
                    "max": max(server_us),
                    "min": min(server_us),
                },
                "end_to_end_us": {
                    "p50": bks.percentile(e2e_us, 0.50),
                    "p99": bks.percentile(e2e_us, 0.99),
                    "max": max(e2e_us),
                },
                "response_bytes": {"mean": sum(resp_bytes) / len(resp_bytes), "max": max(resp_bytes)},
                "server_us_p99_minus_p50": bks.percentile(server_us, 0.99) - bks.percentile(server_us, 0.50),
            }
            results["k_sweep_fixed"][str(k)] = k_result
            print(json.dumps(k_result, indent=2, default=str))

        print("\n" + json.dumps(results, indent=2, default=str))
        if args.out:
            Path(args.out).write_text(json.dumps(results, indent=2, default=str))
        return 0
    finally:
        bks.harness.stop_server(proc)


if __name__ == "__main__":
    raise SystemExit(main())
