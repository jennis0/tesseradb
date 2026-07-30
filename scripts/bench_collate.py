#!/usr/bin/env python3
"""Collate a run's JSONL into a committed baseline, and apply the gates.

Reads every `*.jsonl` under a run directory, emits one baseline JSON per arm in the shape
`docs/superpowers/plans/bench-baselines/2m4-criterion-baseline.json` already uses (`note`,
`generated_at`, `history`, `benchmarks{name: {..., delta_vs_baseline_pct}}`), and returns a
non-zero exit if a gate fails.

# Why the gates are in this order

`docs/design-memos/2026-07-30-tail-attribution.md` showed the exit criterion in use at the time --
"p99 over uniformly-random viewports < 10 ms" -- is mostly a measurement of the *input
distribution*: repeating one fixed viewport collapsed p99-p50 from 47.4 ms to 5.1 ms, and
Sigma-visible spans six orders of magnitude across random bboxes. Its recommendation was a fixed
representative-viewport battery plus normalised-cost ceilings, with any single random-sweep p99
reported bucketed rather than bare. These gates implement that, most-authoritative first:

  G0   work invariance -- container counts and Sigma-visible must be identical across runs of the
       same (scale, label_set, seed). If they move, the corpus or the grant construction changed
       and EVERY latency comparison in the run is void. This gate fails the run, not a cell.
  G0b  control arm -- `hash-flat`'s run ratio must be 1.00 (probes/results.md §5). If the
       estimator is broken, no other ratio in the run is interpretable.
  G1   normalised-cost ceilings -- ns per row-visible / point-gathered / tile / container. These
       stay near-flat where raw p99 swings wildly, so a move here is the system, not the mix.
  G2   battery p99-p50 -- the workload-variance-free stall detector.
  G3   stage share -- each stage's percentage of total. Catches "one stage got 3x slower while
       another got faster", which a total-latency gate cannot see.

A raw random-sweep p99 is emitted for the dashboard but is never a gate, and is always carried
alongside its Sigma-visible distribution so a reader can tell a workload shift from a regression.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import defaultdict
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
BASELINE_DIR = REPO_ROOT / "docs" / "superpowers" / "plans" / "bench-baselines"

# G1 regression threshold: a normalised cost may drift this much before it is a regression.
G1_TOLERANCE_PCT = 15.0
# G3: a stage's share of total may move this many percentage points.
G3_TOLERANCE_POINTS = 10.0


def load_records(run_dir: Path) -> list[dict]:
    records = []
    for path in sorted(run_dir.glob("*.jsonl")):
        if path.name == "ledger.jsonl":
            continue
        for line in path.read_text().splitlines():
            if line.strip():
                records.append(json.loads(line))
    return records


def gate_g0_work_invariance(records: list[dict], baseline: dict | None) -> list[str]:
    """Container counts and Sigma-visible must match the baseline for the same cell."""
    if not baseline:
        return []
    failures = []
    prior = baseline.get("benchmarks", {})
    for rec in records:
        old = prior.get(rec["cell_id"])
        if not old or "work" not in old:
            continue
        for field in ("containers", "sigma_visible", "mask_cardinality"):
            was, now = old["work"].get(field), rec["work"].get(field)
            if was is None or now is None or was == now:
                continue
            failures.append(
                f"G0 {rec['cell_id']}: {field} changed {was} -> {now}. The corpus or the grant "
                f"construction moved; every latency comparison in this run is void."
            )
    return failures


def gate_g0b_control(records: list[dict]) -> list[str]:
    """hash-flat's run ratio validates the estimator (probes/results.md §5: exactly 1.00)."""
    failures = []
    seen = False
    for rec in records:
        if rec["label_set"] != "hash-flat":
            continue
        ratio = rec["work"].get("run_ratio", 0.0)
        if ratio == 0.0:
            continue
        seen = True
        if abs(ratio - 1.0) > 0.05:
            failures.append(
                f"G0b {rec['cell_id']}: hash-flat run ratio {ratio:.4f}, expected 1.00. The "
                f"estimator or the corpus is broken; no other ratio in this run is interpretable."
            )
    if not seen:
        print("  note: no hash-flat cell with a run ratio — G0b did not run", file=sys.stderr)
    return failures


def gate_g1_normalised(records: list[dict], baseline: dict | None) -> list[str]:
    if not baseline:
        return []
    failures = []
    prior = baseline.get("benchmarks", {})
    for rec in records:
        if "low_container_resolution" in rec.get("flags", []):
            continue  # too few containers for the ratio to mean anything
        if "degenerate" in rec.get("flags", []):
            continue
        old = prior.get(rec["cell_id"])
        if not old or "normalised" not in old:
            continue
        for metric, now in rec["normalised"].items():
            was = old["normalised"].get(metric)
            if not was or not now:
                continue
            delta = 100.0 * (now - was) / was
            if delta > G1_TOLERANCE_PCT:
                failures.append(
                    f"G1 {rec['cell_id']}: {metric} {was:.4f} -> {now:.4f} (+{delta:.1f}%)"
                )
    return failures


def gate_g2_battery(records: list[dict], baseline: dict | None) -> list[str]:
    """p99 - p50 on the fixed battery: the stall detector with workload variance removed."""
    if not baseline:
        return []
    failures = []
    prior = baseline.get("benchmarks", {})
    for rec in records:
        if rec.get("params", {}).get("mode") != "battery":
            continue
        old = prior.get(rec["cell_id"])
        if not old:
            continue
        was = old["timing"]["p99_ns"] - old["timing"]["median_ns"]
        now = rec["timing"]["p99_ns"] - rec["timing"]["median_ns"]
        if was > 0 and now > was * (1 + G1_TOLERANCE_PCT / 100.0):
            failures.append(
                f"G2 {rec['cell_id']}: p99-p50 {was/1e6:.2f} ms -> {now/1e6:.2f} ms "
                f"(a per-request stall, not a workload shift — geometry is fixed here)"
            )
    return failures


def gate_g3_stage_share(records: list[dict], baseline: dict | None) -> list[str]:
    if not baseline:
        return []
    failures = []
    prior = baseline.get("benchmarks", {})
    for rec in records:
        stages = rec.get("stages")
        old = prior.get(rec["cell_id"])
        if not stages or not old or not old.get("stages"):
            continue
        total_now = max(stages.get("total_ns", 0), 1)
        total_was = max(old["stages"].get("total_ns", 0), 1)
        for name, now_ns in stages.items():
            if not name.endswith("_ns") or name == "total_ns":
                continue
            was_ns = old["stages"].get(name, 0)
            share_now = 100.0 * now_ns / total_now
            share_was = 100.0 * was_ns / total_was
            if abs(share_now - share_was) > G3_TOLERANCE_POINTS:
                failures.append(
                    f"G3 {rec['cell_id']}: {name} share {share_was:.1f}% -> {share_now:.1f}%"
                )
    return failures


def collate(records: list[dict]) -> dict:
    by_arm = defaultdict(dict)
    for rec in records:
        by_arm[rec["arm"]][rec["cell_id"]] = {
            "min_ns": rec["timing"]["min_ns"],
            "median_ns": rec["timing"]["median_ns"],
            "p99_ns": rec["timing"]["p99_ns"],
            "work": rec["work"],
            "normalised": rec["normalised"],
            "stages": rec.get("stages"),
            "params": rec.get("params"),
            "flags": rec.get("flags", []),
        }
    return by_arm


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--run-dir", type=Path, required=True)
    ap.add_argument("--baseline-dir", type=Path, default=BASELINE_DIR)
    ap.add_argument("--tag", default="tier1", help="baseline file stem")
    ap.add_argument("--update", action="store_true", help="write the baseline (else compare only)")
    ap.add_argument("--gates", action="store_true", help="exit non-zero on gate failure")
    args = ap.parse_args()

    records = load_records(args.run_dir)
    if not records:
        print(f"no records under {args.run_dir}", file=sys.stderr)
        return 1
    print(f"{len(records)} cells from {args.run_dir}")

    baseline_path = args.baseline_dir / f"{args.tag}-baseline.json"
    baseline = json.loads(baseline_path.read_text()) if baseline_path.exists() else None
    if baseline:
        print(f"comparing against {baseline_path}")
    else:
        print(f"no baseline at {baseline_path} — first run, gates will not fire")

    failures = []
    failures += gate_g0_work_invariance(records, baseline)
    failures += gate_g0b_control(records)
    if failures:
        # G0/G0b invalidate the run itself; the later gates would be comparing noise.
        print("\nRUN INVALID — G0/G0b failed:")
        for f in failures:
            print(f"  {f}")
        return 2 if args.gates else 0

    failures += gate_g1_normalised(records, baseline)
    failures += gate_g2_battery(records, baseline)
    failures += gate_g3_stage_share(records, baseline)

    by_arm = collate(records)
    for arm, cells in sorted(by_arm.items()):
        print(f"  {arm:<18} {len(cells)} cells")

    env = records[0].get("env", {})
    out = {
        "note": (
            "Regenerate with scripts/bench_collate.py --update. Gate on the `normalised` block, "
            "not on p99: the tail-attribution memo showed a raw random-sweep p99 mostly measures "
            "the input distribution. Cells flagged low_container_resolution or degenerate are "
            "excluded from G1."
        ),
        "tag": args.tag,
        "git_sha": env.get("git_sha"),
        "dirty": env.get("dirty"),
        "env": env,
        "history": ((baseline or {}).get("history", []) + [
            {"git_sha": env.get("git_sha"), "cells": len(records)}
        ])[-10:],
        "benchmarks": {cid: cell for cells in by_arm.values() for cid, cell in cells.items()},
    }

    if args.update:
        args.baseline_dir.mkdir(parents=True, exist_ok=True)
        baseline_path.write_text(json.dumps(out, indent=2, sort_keys=True))
        print(f"wrote {baseline_path}")

    if failures:
        print(f"\n{len(failures)} gate failure(s):")
        for f in failures:
            print(f"  {f}")
        return 1 if args.gates else 0

    print("\nall gates passed" if baseline else "\nbaseline recorded; no gates to check")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
