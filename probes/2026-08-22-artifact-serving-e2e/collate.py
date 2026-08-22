"""Fold a tier's JSON records into the CSVs the README's tables are read from.

Every figure in the README has a CSV beside it and this is what writes them. Nothing here computes
anything the drivers did not already record — it reshapes, and it is deliberately dull, because a
collation that derives is a second place a number can be wrong.

The one judgement it makes is the **probe comparison**: `artifact-serving-at-scale.md` §7.1's grid
is quoted here as a table, and a cell running more than twice its probe-side figure is flagged.
Only the cells whose configuration aligns are compared, and the alignment is loose — the probe's
grid is `hoisted` over a synthetic membership at one artifact count, this is a real layer over a
real bundle — so the flag is a prompt to look, never a verdict.

Run: `python3 collate.py --work DIR --tier 1e7 --out data/`
"""

from __future__ import annotations

import argparse
import csv
import json
from pathlib import Path

#: `artifact-serving-at-scale.md` §7.1's full grid at 10⁶ artifacts over 10⁹ points, milliseconds,
#: median of three runs — the closest published configuration to the campaign's tiers. Rows are the
#: principal's visible fraction, columns the viewport's.
PROBE_1E9_A1E6 = {
    "100%": {"100%": 131, "75%": 131, "50%": 94.6, "25%": 44.3, "6.25%": 13.0, "0.39%": 1.9, "0.024%": 0.9},
    "75%": {"100%": 209, "75%": 187, "50%": 132, "25%": 62.5, "6.25%": 17.6, "0.39%": 2.4, "0.024%": 1.0},
    "50%": {"100%": 198, "75%": 177, "50%": 125, "25%": 60.2, "6.25%": 15.9, "0.39%": 2.6, "0.024%": 1.0},
    "25%": {"100%": 144, "75%": 127, "50%": 89.3, "25%": 42.6, "6.25%": 12.2, "0.39%": 2.1, "0.024%": 0.9},
    "9.4%": {"100%": 84.0, "75%": 75.7, "50%": 53.8, "25%": 26.1, "6.25%": 7.5, "0.39%": 1.6, "0.024%": 0.8},
    "3.1%": {"100%": 54.3, "75%": 48.2, "50%": 34.7, "25%": 16.7, "6.25%": 4.7, "0.39%": 1.0, "0.024%": 0.7},
}

#: The campaign's principal rungs mapped onto the probe grid's rows. The broadest rung is 93.75%
#: and the probe's broadest is 100%; they are compared as the nearest neighbours they are, and the
#: README says so.
RUNG_TO_PROBE_ROW = {
    "0.9375": "100%",
    "0.75": "75%",
    "0.5": "50%",
    "0.25": "25%",
    "0.094": "9.4%",
    "0.031": "3.1%",
}


def write_csv(path: Path, rows: list[dict], columns: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=columns, extrasaction="ignore")
        writer.writeheader()
        for row in rows:
            writer.writerow(row)
    print(f"  {path.name}: {len(rows)} row(s)")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--tier", required=True, help="the tier's label, e.g. 1e7")
    ap.add_argument("--out", type=Path, default=Path(__file__).parent / "data")
    args = ap.parse_args()

    work: Path = args.work
    out: Path = args.out
    tier = args.tier
    out.mkdir(parents=True, exist_ok=True)

    fixture_report = work / "fixture-report.json"
    if fixture_report.exists():
        report = json.loads(fixture_report.read_text())
        write_csv(out / f"{tier}-fixture.csv", [{
            "tier": tier,
            "n": report["n"],
            "terms_per_level": report["terms_per_level"],
            "materialise_seconds": report["materialise"]["seconds"],
            "materialise_peak_rss_bytes": report["materialise"]["peak_rss_bytes"],
            "input_bytes": report["materialise"]["input_bytes"],
            "build_seconds": report["build"]["seconds"],
            "build_peak_rss_bytes": report["build"]["peak_rss_bytes"],
            "bundle_bytes": report["build"]["bundle_bytes"],
            "flat_artifacts": report["fixture"]["flat_artifacts"],
            "partition_artifacts": report["fixture"]["partition_artifacts"],
            "boundary_artifacts": report["fixture"]["boundary_artifacts"],
            "boundary_depth": report["fixture"]["boundary_depth"],
            "treed_artifacts": report["fixture"]["treed_artifacts"],
        }], [
            "tier", "n", "terms_per_level", "materialise_seconds", "materialise_peak_rss_bytes",
            "input_bytes", "build_seconds", "build_peak_rss_bytes", "bundle_bytes",
            "flat_artifacts", "partition_artifacts", "boundary_artifacts", "boundary_depth",
            "treed_artifacts",
        ])

    grants_path = work / "fixture" / "fixture.json"
    if grants_path.exists():
        grants = json.loads(grants_path.read_text())["grants"]
        write_csv(out / f"{tier}-principals.csv", [
            {
                "tier": tier,
                "rung": g["target"],
                "terms": g["terms"],
                "analytic_fraction": g["analytic_fraction"],
                "visible": g["visible"],
                "measured_fraction": g["measured_fraction"],
            }
            for g in grants
        ], ["tier", "rung", "terms", "analytic_fraction", "visible", "measured_fraction"])

    census = work / "census-report.json"
    if census.exists():
        rows = json.loads(census.read_text())
        write_csv(out / f"{tier}-census.csv", [{"tier": tier, **r} for r in rows], [
            "tier", "principal", "principal_terms", "principal_visible", "layer", "arm",
            "served", "census", "served_not_in_census", "census_not_served",
            "count_disagreements", "exact", "authorise_seconds", "census_seconds",
            "request_seconds",
        ])
        bad = [r for r in rows if not r["exact"]]
        print(f"  census: {len(rows) - len(bad)} of {len(rows)} cells exact")

    grid = work / "grid.json"
    if grid.exists():
        payload = json.loads(grid.read_text())
        rows = []
        for r in payload["rows"]:
            probe_row = RUNG_TO_PROBE_ROW.get(r["principal"])
            probe = PROBE_1E9_A1E6.get(probe_row, {}).get(r["viewport"]) if probe_row else None
            rows.append({
                "tier": tier, **r,
                "probe_ms": probe,
                "ratio_to_probe": round(r["p50_ms"] / probe, 2) if probe else None,
                "over_2x_probe": bool(probe and r["p50_ms"] > 2 * probe),
            })
        write_csv(out / f"{tier}-grid.csv", rows, [
            "tier", "layer", "principal", "principal_terms", "principal_fraction", "viewport",
            "viewport_fraction", "zoom", "artifacts_served", "body_bytes", "cold_ms", "p50_ms",
            "p99_ms", "min_ms", "max_ms", "iterations", "server_p50_ms", "stream_p50_ms",
            "serialise_p50_ms", "rss_bytes", "probe_ms", "ratio_to_probe",
            "over_2x_probe",
        ])
        flagged = [r for r in rows if r["over_2x_probe"]]
        print(f"  grid: {len(rows)} cells, {len(flagged)} above 2x the probe's figure")

    conc = work / "concurrency.json"
    if conc.exists():
        rows = json.loads(conc.read_text())
        write_csv(out / f"{tier}-concurrency.csv", [{
            "tier": tier,
            "concurrency": r["concurrency"],
            "mix": json.dumps(r["mix"]),
            "layer": r["layer"],
            "seconds": round(r["seconds"], 2),
            "requests": r["requests"],
            "throughput_rps": round(r["throughput_rps"], 2),
            "p50_ms": round(r["p50_ms"], 2),
            "p99_ms": round(r["p99_ms"], 2),
            "max_ms": round(r["max_ms"], 2),
            "errors": r["errors"],
            "server_peak_rss_bytes": r["server_peak_rss_bytes"],
            "server_cores_busy": round(r["server_cores_busy"], 2),
            "server_cpu_per_request": round(r["server_cpu_per_request"], 4),
            "bytes_per_second": round(r["bytes_per_second"]),
            "authorise_seconds": round(r["authorise_seconds"], 2),
            "gate_shed_total": (r.get("compute_gate") or {}).get("shed_total"),
            "gate_admission": (r.get("compute_gate") or {}).get("admission"),
            "gate_queue": (r.get("compute_gate") or {}).get("queue"),
            "projection_cache": json.dumps(r.get("row_projection_cache")),
            "fragment_cache": json.dumps(r.get("fragment_cache")),
        } for r in rows], [
            "tier", "concurrency", "mix", "layer", "seconds", "requests", "throughput_rps",
            "p50_ms", "p99_ms", "max_ms", "errors", "server_peak_rss_bytes", "server_cores_busy",
            "server_cpu_per_request", "bytes_per_second", "authorise_seconds", "gate_shed_total",
            "gate_admission", "gate_queue", "projection_cache", "fragment_cache",
        ])
        write_csv(out / f"{tier}-concurrency-by-breadth.csv", [
            {"tier": tier, "concurrency": r["concurrency"], "breadth": b,
             "requests": v["requests"], "p50_ms": round(v["p50_ms"], 2),
             "p99_ms": round(v["p99_ms"], 2)}
            for r in rows for b, v in r["per_breadth"].items()
        ], ["tier", "concurrency", "breadth", "requests", "p50_ms", "p99_ms"])

    fold = work / "fold-under-load.json"
    if fold.exists():
        r = json.loads(fold.read_text())
        write_csv(out / f"{tier}-fold-under-load.csv", [{
            "tier": tier,
            "sessions": r["sessions"],
            "layer": r["layer"],
            "fold_seconds": (r.get("fold") or {}).get("duration_seconds"),
            "fold_completed": (r.get("fold") or {}).get("completed"),
            "ingest_rows": (r.get("ingest") or {}).get("rows_accepted"),
            "ingest_rows_per_second": (r.get("ingest") or {}).get("rows_per_second"),
            "count_before_attribute": (r.get("before") or {}).get("attribute"),
            "count_after_flush_attribute": (r.get("after_flush") or {}).get("attribute"),
            "count_after_fold_attribute": (r.get("after_fold") or {}).get("attribute"),
            "count_before_enumerated": (r.get("before") or {}).get("enumerated"),
            "count_after_flush_enumerated": (r.get("after_flush") or {}).get("enumerated"),
            "count_after_fold_enumerated": (r.get("after_fold") or {}).get("enumerated"),
            **{f"{w}_{k}": v for w, stats in (r.get("windows") or {}).items()
               for k, v in stats.items()},
            "errors": r.get("errors"),
            "peak_rss_bytes": r.get("peak_rss_bytes"),
        }], [
            "tier", "sessions", "layer", "fold_seconds", "fold_completed", "ingest_rows",
            "ingest_rows_per_second",
            "count_before_attribute", "count_after_flush_attribute", "count_after_fold_attribute",
            "count_before_enumerated", "count_after_flush_enumerated", "count_after_fold_enumerated",
            "before_requests", "before_p50_ms", "before_p99_ms", "before_max_ms",
            "ingest_and_flush_requests", "ingest_and_flush_p50_ms", "ingest_and_flush_p99_ms",
            "ingest_and_flush_max_ms",
            "during_fold_requests", "during_fold_p50_ms", "during_fold_p99_ms", "during_fold_max_ms",
            "after_requests", "after_p50_ms", "after_p99_ms", "after_max_ms",
            "errors", "peak_rss_bytes",
        ])

    ingest = work / "ingest-during-serving.json"
    if ingest.exists():
        r = json.loads(ingest.read_text())
        write_csv(out / f"{tier}-ingest-during-serving.csv", [{
            "tier": tier,
            "sessions": r["sessions"],
            "layer": r["layer"],
            "rows_accepted": (r.get("ingest") or {}).get("rows_accepted"),
            "rows_per_second": (r.get("ingest") or {}).get("rows_per_second"),
            "batch_p50_ms": (r.get("ingest") or {}).get("batch_p50_ms"),
            "batch_p99_ms": (r.get("ingest") or {}).get("batch_p99_ms"),
            "batch_max_ms": (r.get("ingest") or {}).get("batch_max_ms"),
            "read_quiet_p50_ms": r["read_path"]["quiet"].get("p50_ms"),
            "read_quiet_p99_ms": r["read_path"]["quiet"].get("p99_ms"),
            "read_loaded_p50_ms": r["read_path"]["under_ingest"].get("p50_ms"),
            "read_loaded_p99_ms": r["read_path"]["under_ingest"].get("p99_ms"),
            "count_before_attribute": (r.get("before") or {}).get("attribute"),
            "count_after_flush_attribute": (r.get("after_flush") or {}).get("attribute"),
            "count_before_enumerated": (r.get("before") or {}).get("enumerated"),
            "count_after_flush_enumerated": (r.get("after_flush") or {}).get("enumerated"),
            "errors": r.get("errors"),
            "peak_rss_bytes": r.get("peak_rss_bytes"),
        }], [
            "tier", "sessions", "layer", "rows_accepted", "rows_per_second", "batch_p50_ms",
            "batch_p99_ms", "batch_max_ms", "read_quiet_p50_ms", "read_quiet_p99_ms",
            "read_loaded_p50_ms", "read_loaded_p99_ms", "count_before_attribute",
            "count_after_flush_attribute", "count_before_enumerated",
            "count_after_flush_enumerated", "errors", "peak_rss_bytes",
        ])

    print(f"collated tier {tier} -> {out}")


if __name__ == "__main__":
    main()
