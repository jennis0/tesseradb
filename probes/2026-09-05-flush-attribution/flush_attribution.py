#!/usr/bin/env python3
"""The flush's stages across one ingest phase, per flush and per row.

Runs `test_corpora.common.ingest_cycle` unchanged, with one addition: `/control/status`'s
`write_executor` block is read before and after the ingest phase, and the difference of its
`flush_stages` map is written into the result as `flush_laps` and printed as a table. The
executor's own laps (`executor_laps`) are the driver's and are printed beside them for context.

Usage is the driver's, with the same flags:

    python3 probes/2026-09-05-flush-attribution/flush_attribution.py \\
        --rung-dir data/ladder/medcpt-1m --work <scratch> --binary <bench-timing tessera> \\
        --out <cell>.json --fraction 0.10 --concurrency 8 --port0 8171 \\
        --stop-after-ingest --reuse-base

The figures are process totals differenced across the phase. A pool stage per flush divides by
`executions` (returns of `execute_flush` on the pool) and per row by `rows_executed`; a
publication stage per flush divides by `flushes` (publications that swapped) and per row by
`rows_published`; the tick's `plan` and `dispatch` divide by `executions`. A flush still running
when the phase ends is in none of the counts: the pool commits its laps only when it returns.
All zero, with `bench_timing: false`, from a binary built without the feature.

The after-status is read once the flushes the ingest triggered have landed: the row trigger asks
again at each publication while the buffer holds `flush_max_items` or more, so the phase ends
when no flush is in flight, every execution has published, and the buffer is still across two
polls half a second apart. On a small cell the ingest phase is shorter than one flush and every
flush lands in this drain; on a large one most land during the ingest. `drain_s` in the result
is how long the drain took, and the `*_at_ingest_end` counts say how many had landed before it.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

from test_corpora.common import ingest_cycle  # noqa: E402

# The order `FlushStage` declares them in. `plan` and `dispatch` run at the tick; the rest of the
# executor's stages partition `publish_wall`; the pool's stages partition `pool_wall`.
TICK = ("plan", "dispatch")
PUBLISH = (
    "compose", "manifest", "manifest_commit", "with_segment", "shapes_install",
    "buffer_rebase", "denied", "artifacts", "swap", "rotate", "drop_superseded", "discarded",
)
EXECUTE = (
    "promote", "rows", "segment", "delta_tier", "filter_extents", "entity_terms",
    "scoped_extents", "record_extent", "text_extents", "digests", "reopen", "shapes",
    "drop_plan", "failed",
)
# The sub-stages that partition `text_extents`, printed indented under it. They are in
# `pool_nanos` and not in `EXECUTE`: counted there they would double `text_extents`.
TEXT = ("text_rows", "text_tokenise_terms", "text_dict", "text_postings", "text_presence")


def _per(nanos: int, count: int, rows: int) -> dict:
    return {
        "nanos": nanos,
        "ms_per_flush": round(nanos / count / 1e6, 3) if count else None,
        "us_per_row": round(nanos / rows / 1e3, 3) if rows else None,
    }


def flush_laps(before: dict, after: dict) -> dict:
    """Difference two `write_executor` blocks' `flush_stages` maps across an ingest phase."""
    fb = before.get("flush_stages")
    fa = after.get("flush_stages")
    if fb is None or fa is None:
        raise SystemExit(
            "this binary's /control/status has no write_executor.flush_stages block; build "
            "tessera from a tree that carries the flush laps"
        )
    executions = fa["executions"] - fb["executions"]
    flushes = fa["flushes"] - fb["flushes"]
    rows_executed = fa["rows_executed"] - fb["rows_executed"]
    rows_published = fa["rows_published"] - fb["rows_published"]

    def diff(key: str) -> dict[str, int]:
        return {name: fa[key][name] - fb[key].get(name, 0) for name in fa[key]}

    executor = diff("executor_nanos")
    pool = diff("pool_nanos")
    execute_sum = sum(pool[s] for s in EXECUTE)
    publish_sum = sum(executor[s] for s in PUBLISH)
    # A binary from before the sub-laps has no `text_*` keys; the table then shows them absent.
    text_sum = sum(pool.get(s, 0) for s in TEXT)
    return {
        "bench_timing": bool(fa.get("bench_timing")),
        "executions": executions,
        "flushes": flushes,
        "rows_executed": rows_executed,
        "rows_published": rows_published,
        "pool": {name: _per(n, executions, rows_executed) for name, n in pool.items()},
        "execute_sum": _per(execute_sum, executions, rows_executed),
        "execute_unattributed": _per(pool["pool_wall"] - execute_sum, executions, rows_executed),
        "text_sum": _per(text_sum, executions, rows_executed),
        "text_unattributed": _per(pool["text_extents"] - text_sum, executions, rows_executed),
        "executor": {
            name: (
                _per(n, executions, rows_executed)
                if name in TICK
                else _per(n, flushes, rows_published)
            )
            for name, n in executor.items()
        },
        "publish_sum": _per(publish_sum, flushes, rows_published),
        "publish_unattributed": _per(
            executor["publish_wall"] - publish_sum, flushes, rows_published
        ),
        "status_before": fb,
        "status_after": fa,
    }


DRAIN_TIMEOUT_S = 1800.0


def drain_flushes(control, log) -> dict:
    """Wait for the flushes the ingest phase triggered to land, and say how long that took.

    Complete means: nothing on the pool, every execution published, and the buffer unchanged
    across two polls half a second apart (a publication that leaves `flush_max_items` or more
    buffered dispatches the next unit at the very next loop iteration, so a still buffer with
    nothing in flight is the row trigger having nothing more to ask).
    """
    t0 = time.perf_counter()
    first = control.status()["write_executor"]
    at_end = {
        "executions_at_ingest_end": first["flush_stages"]["executions"],
        "flushes_at_ingest_end": first["flush"]["flushes"],
        "buffered_at_ingest_end": first["flush"]["buffered_items"],
    }
    deadline = t0 + DRAIN_TIMEOUT_S
    previous = None
    while True:
        st = control.status()["write_executor"]
        settled = (
            not st["flush"]["in_flight"]
            and st["flush"]["flushes"] == st["flush_stages"]["executions"]
        )
        # `flushes` moves inside the publication, before its last stages are lapped, so the two
        # walls are in the key: a still wall is a publication that has returned.
        key = (
            st["flush"]["flushes"],
            st["flush_stages"]["executions"],
            st["flush"]["buffered_items"],
            st["flush_stages"]["executor_nanos"]["publish_wall"],
            st["flush_stages"]["pool_nanos"]["pool_wall"],
        )
        if settled and previous == key:
            break
        if time.perf_counter() > deadline:
            log(f"drain timed out after {DRAIN_TIMEOUT_S} s; the laps below are what had landed")
            at_end["drain_timed_out"] = True
            break
        previous = key if settled else None
        time.sleep(0.5)
    at_end["drain_s"] = round(time.perf_counter() - t0, 2)
    at_end["buffered_after_drain"] = st["flush"]["buffered_items"]
    return at_end


class FlushCycle(ingest_cycle.Cycle):
    """The driver's cycle, reading the flush laps around its ingest phase and its drain."""

    def run_ingest(self, control, source, label: str) -> dict:
        before = control.status()["write_executor"]
        result = super().run_ingest(control, source, label)
        drain = drain_flushes(control, self.log)
        self.log(
            f"  flushes drained in {drain['drain_s']} s "
            f"({drain['flushes_at_ingest_end']} had published by the end of ingest)"
        )
        after = control.status()["write_executor"]
        laps = flush_laps(before, after)
        laps.update(drain)
        self.result["flush_laps"] = laps
        return result


def _row(name: str, cell: dict, indent: str = "  ") -> str:
    ms = cell["ms_per_flush"]
    us = cell["us_per_row"]
    ms_s = f"{ms:12.3f}" if ms is not None else f"{'-':>12}"
    us_s = f"{us:10.3f}" if us is not None else f"{'-':>10}"
    return f"{indent}{name:<20}{ms_s}{us_s}"


def print_table(result: dict) -> None:
    laps = result.get("flush_laps")
    if not laps:
        print("no flush_laps in the result (the run stopped before its ingest phase)")
        return
    ingest = result.get("ingest") or {}
    ex = result.get("executor_laps") or {}
    print()
    print(
        f"flush attribution: {laps['executions']} executions, {laps['flushes']} flushes, "
        f"{laps['rows_executed']:,} rows executed, {laps['rows_published']:,} rows published, "
        f"bench_timing={laps['bench_timing']}"
    )
    if "drain_s" in laps:
        print(
            f"drain: {laps['flushes_at_ingest_end']} flushes had published when ingest ended with "
            f"{laps['buffered_at_ingest_end']:,} rows buffered; the rest landed in "
            f"{laps['drain_s']} s, leaving {laps['buffered_after_drain']:,} buffered"
        )
    if ingest:
        print(
            f"ingest: {ingest.get('accepted', 0):,} rows accepted at {ingest.get('items_per_s')} "
            f"rows/s; executor {ex.get('executor_sum_us_per_row')} µs/row, queueing "
            f"{ex.get('queueing_us_per_row')} µs/row"
        )
    header = f"{'':<22}{'ms/flush':>12}{'µs/row':>10}"
    print()
    print("pool (execute_flush)")
    print(header)
    for name in EXECUTE:
        print(_row(name, laps["pool"][name]))
        if name == "text_extents" and all(s in laps["pool"] for s in TEXT):
            for sub in TEXT:
                print(_row(f".{sub[5:]}", laps["pool"][sub], indent="      "))
            print(_row("= text sum", laps["text_sum"], indent="      "))
            print(_row("unattributed", laps["text_unattributed"], indent="      "))
    print(_row("= execute sum", laps["execute_sum"]))
    print(_row("pool_wall", laps["pool"]["pool_wall"]))
    print(_row("unattributed", laps["execute_unattributed"]))
    print()
    print("executor (the tick, per execution)")
    print(header)
    for name in TICK:
        print(_row(name, laps["executor"][name]))
    print()
    print("executor (publish_flush, per publication)")
    print(header)
    for name in PUBLISH:
        print(_row(name, laps["executor"][name]))
    print(_row("= publish sum", laps["publish_sum"]))
    print(_row("publish_wall", laps["executor"]["publish_wall"]))
    print(_row("unattributed", laps["publish_unattributed"]))
    print()


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] == "--print":
        # Re-print a finished run's table from its result file.
        for path in argv[1:]:
            print(f"== {path}")
            print_table(json.loads(Path(path).read_text()))
        return 0
    pre = argparse.ArgumentParser(add_help=False)
    pre.add_argument("--out", required=True)
    known, _ = pre.parse_known_args(argv)
    # `main` looks `Cycle` up in the driver's module at call time, so this substitution is the
    # whole of the change to the driver's run.
    ingest_cycle.Cycle = FlushCycle
    rc = ingest_cycle.main(argv)
    out = Path(known.out)
    if out.exists():
        print_table(json.loads(out.read_text()))
    return rc


if __name__ == "__main__":
    sys.exit(main())
