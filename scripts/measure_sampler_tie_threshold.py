#!/usr/bin/env python3
"""Confirmation probe (2026-07-30) for
docs/design-memos/2026-07-30-priority-as-identity-prefix.md.

Question: over the existing 10^9 bundle, count the (tile, principal) pairs with V (the
per-tile `visible` count, i.e. the masked cardinality of a tile before the k-cap) greater
than 2*10^6 -- the point above which every candidate in a tile shares the same 16-bit
`priority = high16(splitmix64(entity_id))` and the sampler's tiebreak (`entity_id`, which
is permanently signature-sorted under I9) decides the sample instead of priority.

Prior baselines were checked first and do NOT carry the needed granularity:
  - docs/superpowers/plans/bench-baselines/2026-07-29-1e9-k-sweep.json: only
    percentile latency/points-returned stats, no visible counts at all, single w=10^4.
  - probes/work-correlation.json: has `sigma_visible` (summed over a whole viewport's
    tiles) per request, but never a per-tile breakdown, and again only w=10^4.
Neither lets you recover a per-(tile, principal) V, so a new server run against the
EXISTING /tmp/tessera-1e9 bundle (not rebuilt) is required. This script decodes the
per-tile `visible` column directly from `decode_viewport` (reference/oracle/wire.py),
which already returns `tiles = [(tile_id, visible, matched), ...]` per Arrow batch.

k is fixed at 1 throughout: `visible`/`matched` are full masked tile counts computed
before the k-cap (bench_work_correlation.py's own comment: "Sigma visible ... over the
WHOLE viewport, uncapped by k"), so k does not affect the quantity under test and a small
k keeps point-payload decode cost down.

The memo's defect bites HEAD principals (large visible sets), so a single grant width is
not representative -- this sweeps a range of grant widths (coverage), from a narrow
tail-like grant to nearly the whole term dictionary, and measures the ACHIEVED coverage
fraction directly from a zoom-0 (whole-extent, single-tile) query rather than assuming
linearity between term count and visible fraction.

Usage:
  reference/.venv/bin/python scripts/measure_sampler_tie_threshold.py \
      --bundle /tmp/tessera-1e9 --out probes/sampler-tie-threshold.json
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
THRESHOLD = 2_000_000
FULL_BBOX = [0.0, 0.0, EXTENT, EXTENT]

# Grant widths spanning a wide coverage range (dictionary vocab ~47968 terms): from a very
# narrow tail-like grant up to nearly the whole dictionary. Actual achieved visible-fraction
# per width is MEASURED (zoom-0 full-extent visible / N), not assumed from w/vocab.
GRANT_WIDTHS = [5, 20, 100, 500, 2000, 8000, 16000, 30000, 47968]

# Zoom depths probed with a full-extent query per grant width. Full-extent tile count is
# 4^zoom, so this is capped where tile enumeration stays tractable (4^8 = 65536).
ZOOMS = list(range(0, 9))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", default="/tmp/tessera-1e9")
    ap.add_argument("--tmp", default="/tmp/tessera-1e9-serve-tiethresh")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--boot-deadline", type=float, default=600.0)
    ap.add_argument("--out", default=None)
    ap.add_argument("--zooms", default=",".join(str(z) for z in ZOOMS))
    ap.add_argument("--widths", default=",".join(str(w) for w in GRANT_WIDTHS))
    args = ap.parse_args()

    zooms = [int(z) for z in args.zooms.split(",")]
    widths = [int(w) for w in args.widths.split(",")]

    bundle_root = Path(args.bundle)
    if not (bundle_root / "CURRENT").exists():
        print(f"ERROR: no bundle at {bundle_root}", file=sys.stderr)
        return 1

    tmp_dir = Path(args.tmp)
    tmp_dir.mkdir(parents=True, exist_ok=True)

    print("Booting server ONCE against existing bundle (not rebuilding)...", flush=True)
    srv, proc, boot_elapsed = bks.spawn_with_long_boot_deadline(bundle_root, tmp_dir, args.boot_deadline, 200)
    print(f"Boot time: {boot_elapsed:.1f}s", flush=True)

    results: dict = {
        "bundle": str(bundle_root),
        "threshold": THRESHOLD,
        "boot_seconds": boot_elapsed,
        "seed": args.seed,
        "zooms_probed": zooms,
        "grant_widths": [],
        "per_grant": {},
    }

    try:
        descriptors = bks.read_dictionary_descriptors(bundle_root)
        vocab = len(descriptors)
        results["dictionary_vocab"] = vocab
        print(f"Dictionary vocab: {vocab} terms", flush=True)

        rng = random.Random(args.seed)

        overall_pairs = 0
        overall_exceed = 0
        by_depth: dict[int, dict[str, int]] = {z: {"pairs": 0, "exceed": 0} for z in zooms}

        for w in widths:
            w_eff = min(w, vocab)
            grant = rng.sample(descriptors, w_eff)
            t0 = time.perf_counter()
            auth = srv.authorise(grant)
            token = auth["token"]
            authorise_s = time.perf_counter() - t0
            print(f"\n=== grant width w={w_eff} (authorise {authorise_s * 1000:.1f} ms) ===", flush=True)

            grant_record: dict = {
                "grant_width": w_eff,
                "authorise_seconds": authorise_s,
                "by_zoom": {},
            }

            for z in zooms:
                t0 = time.perf_counter()
                resp = srv.viewport_response(token, "s0", z, FULL_BBOX, k=1)
                wall_s = time.perf_counter() - t0
                server_us = float(resp.headers.get("x-tessera-server-us", "nan"))
                tiles, _ = decode_viewport(resp.content)
                visible_counts = [t[1] for t in tiles]
                n_tiles = len(visible_counts)
                n_exceed = sum(1 for v in visible_counts if v > THRESHOLD)
                max_v = max(visible_counts) if visible_counts else 0
                total_v = sum(visible_counts)
                mean_v = (total_v / n_tiles) if n_tiles else 0
                if z == min(zooms):
                    grant_record["total_visible_at_min_zoom"] = total_v
                    grant_record["coverage_fraction"] = total_v / 1e9
                    print(
                        f"  coverage at zoom={z}: total_visible={total_v} "
                        f"({total_v / 1e9:.6%} of 1e9)",
                        flush=True,
                    )
                grant_record["by_zoom"][str(z)] = {
                    "n_tiles_nonempty": n_tiles,
                    "n_exceed_threshold": n_exceed,
                    "fraction_exceed": (n_exceed / n_tiles) if n_tiles else None,
                    "mean_visible": mean_v,
                    "max_visible": max_v,
                    "server_us": server_us,
                    "wall_seconds": wall_s,
                }
                overall_pairs += n_tiles
                overall_exceed += n_exceed
                by_depth[z]["pairs"] += n_tiles
                by_depth[z]["exceed"] += n_exceed
                print(
                    f"  zoom={z:2d}  tiles={n_tiles:8d}  exceed={n_exceed:8d}  "
                    f"mean_v={mean_v:14.1f}  max_v={max_v:12d}  wall={wall_s:6.2f}s",
                    flush=True,
                )

            results["grant_widths"].append(w_eff)
            results["per_grant"][str(w_eff)] = grant_record

        results["overall_pairs"] = overall_pairs
        results["overall_exceed"] = overall_exceed
        results["overall_fraction_exceed"] = (overall_exceed / overall_pairs) if overall_pairs else None
        results["by_depth"] = {
            str(z): {
                "pairs": by_depth[z]["pairs"],
                "exceed": by_depth[z]["exceed"],
                "fraction_exceed": (by_depth[z]["exceed"] / by_depth[z]["pairs"]) if by_depth[z]["pairs"] else None,
            }
            for z in zooms
        }

        print("\n=== SUMMARY ===", flush=True)
        print(
            json.dumps(
                {
                    "overall_pairs": overall_pairs,
                    "overall_exceed": overall_exceed,
                    "overall_fraction_exceed": results["overall_fraction_exceed"],
                    "by_depth": results["by_depth"],
                },
                indent=2,
            ),
            flush=True,
        )

        if args.out:
            Path(args.out).write_text(json.dumps(results, indent=2, default=str))
            print(f"\nWrote {args.out}", flush=True)
        return 0
    finally:
        bks.harness.stop_server(proc)


if __name__ == "__main__":
    raise SystemExit(main())
