#!/usr/bin/env python3
"""Crossover per family per label set, and regret arithmetic, over `tile_axis_sweep` output.

Reads the `CSV,` and `GRANT,` lines of any number of raw sweep files (the filename carries the
label set and grant mode: `tiles-<scale>-<labelset>-<mode>.txt`). Everything here is arithmetic
over MEASURED medians — no model, no fit.

Usage: python3 analyse.py raw/tiles-*.txt
"""
import sys
import os
from collections import defaultdict

FAMILY_ORDER = ["natural", "f01", "f04", "f12", "f35", "f100"]


def load(path):
    cells, grant = [], None
    for line in open(path):
        if line.startswith("GRANT,"):
            f = line.strip().split(",")
            grant = dict(
                universe=int(f[1]),
                ent_card=int(f[2]),
                ent_containers=int(f[3]),
                ent_run_ratio=float(f[4]),
                row_card=int(f[5]),
                row_containers=int(f[6]),
                row_run_ratio=float(f[7]),
            )
        elif line.startswith("CSV,") and not line.startswith("CSV,shape"):
            f = line.strip().split(",")
            cells.append(
                dict(
                    shape=f[1],
                    family=f[1].split("/")[0],
                    zoom=int(f[2]),
                    tiles=int(f[3]),
                    rows=int(f[4]),
                    serial=int(f[5]),
                    par=int(f[6]),
                    ratio=float(f[7]),
                )
            )
    return grant, cells


def crossover(cells):
    """Last tile count measuring SERIAL-favouring -> first measuring PAR-favouring, by tile count.

    Reported exactly as the memo's table is: the boundary pair. `PAR from N` when no cell in the
    family is serial-favouring; `SERIAL to N` when none is parallel-favouring.
    """
    s = sorted(cells, key=lambda c: c["tiles"])
    last_serial = None
    first_par_after = None
    for c in s:
        if c["ratio"] >= 1.0:
            last_serial = c["tiles"]
            first_par_after = None
        elif last_serial is not None and first_par_after is None:
            first_par_after = c["tiles"]
    if last_serial is None:
        return f"PAR from {s[0]['tiles']}"
    if first_par_after is None:
        return f"SERIAL to {last_serial}"
    return f"{last_serial} → {first_par_after}"


def regret(cells, rule):
    """Total ms slower than the better MEASURED arm, plus misclassification count and worst ratio."""
    total = 0.0
    wrong = 0
    worst = 1.0
    for c in cells:
        picked = c["par"] if rule(c) else c["serial"]
        best = min(c["par"], c["serial"])
        if picked != best:
            wrong += 1
            total += (picked - best) / 1e6
            worst = max(worst, picked / best)
    return total, wrong, worst


def main(paths):
    runs = {}
    for p in paths:
        stem = os.path.basename(p).rsplit(".", 1)[0]
        parts = stem.split("-")
        key = (parts[-2], parts[-1])  # (label set, grant mode)
        runs[key] = load(p)

    order = sorted(runs, key=lambda k: (k[0], k[1]))

    print("== premise check: the grant each sweep actually ran against (MEASURED) ==")
    print(f"{'label set':>20} {'grant':>7} {'cov %':>8} {'row run_ratio':>14} "
          f"{'row containers':>15} {'ent run_ratio':>14}")
    for k in order:
        g = runs[k][0]
        print(f"{k[0]:>20} {k[1]:>7} {100.0 * g['row_card'] / g['universe']:>8.4f} "
              f"{g['row_run_ratio']:>14.3f} {g['row_containers']:>15} {g['ent_run_ratio']:>14.1f}")

    print("\n== crossover per family per label set (MEASURED) ==")
    print(f"{'family':>10} " + " ".join(f"{k[0][:9] + '/' + k[1][:2]:>16}" for k in order))
    for fam in FAMILY_ORDER:
        cols = []
        for k in order:
            cells = [c for c in runs[k][1] if c["family"] == fam]
            cols.append(f"{crossover(cells):>16}" if cells else f"{'-':>16}")
        print(f"{fam:>10} " + " ".join(cols))

    print("\n== ratio by tile count, per family (par_ns/serial_ns; <1 favours PAR) ==")
    for fam in FAMILY_ORDER:
        print(f"\n-- {fam}")
        tiles = sorted({c["tiles"] for k in order for c in runs[k][1] if c["family"] == fam})
        print(f"{'tiles':>9} " + " ".join(f"{k[0][:9] + '/' + k[1][:2]:>16}" for k in order))
        for t in tiles:
            cols = []
            for k in order:
                m = [c for c in runs[k][1] if c["family"] == fam and c["tiles"] == t]
                cols.append(f"{m[0]['ratio']:>16.2f}" if m else f"{'-':>16}")
            print(f"{t:>9} " + " ".join(cols))

    print("\n== regret of candidate rules over these cells (MEASURED medians) ==")
    rules = [
        ("always serial", lambda c: False),
        ("always parallel", lambda c: True),
        ("rows >= 500M (status quo)", lambda c: c["rows"] >= 500_000_000),
        ("rows >= 500M OR tiles >= 1024", lambda c: c["rows"] >= 500_000_000 or c["tiles"] >= 1024),
        ("rows >= 500M OR tiles >= 2048", lambda c: c["rows"] >= 500_000_000 or c["tiles"] >= 2048),
        ("rows >= 500M OR tiles >= 4096", lambda c: c["rows"] >= 500_000_000 or c["tiles"] >= 4096),
        ("rows >= 500M OR tiles >= 8192", lambda c: c["rows"] >= 500_000_000 or c["tiles"] >= 8192),
        ("rows >= 500M OR tiles >= 16384", lambda c: c["rows"] >= 500_000_000 or c["tiles"] >= 16384),
    ]
    allcells = [c for k in order for c in runs[k][1]]
    print(f"{'rule':>34} {'regret ms':>10} {'wrong':>7} {'worst':>8}   per-label-set regret ms")
    for name, rule in rules:
        t, w, worst = regret(allcells, rule)
        per = []
        for ls in sorted({k[0] for k in order}):
            cs = [c for k in order if k[0] == ls for c in runs[k][1]]
            per.append(f"{ls}={regret(cs, rule)[0]:.2f}")
        print(f"{name:>34} {t:>10.2f} {w:>7} {worst:>8.2f}x   " + "  ".join(per))
    print(f"{'oracle (per-cell best)':>34} {0.0:>10.2f} {0:>7} {1.0:>8.2f}x")
    print(f"\ncells: {len(allcells)}")


if __name__ == "__main__":
    main(sys.argv[1:])
