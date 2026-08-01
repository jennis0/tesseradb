#!/usr/bin/env python3
"""Evaluate candidate serial/parallel predictors against the two-axis sweep's measured cells.

Input: the `CSV,` lines emitted by `crates/tessera-engine/examples/tile_axis_sweep.rs`, one file
per (scale, grant). Every number this reads is a measured median; the only derived quantity is the
per-cell REGRET — how much slower the arm a rule selects is than the better of the two measured
arms for that same shape. Regret is arithmetic on measured values, not a model.

Usage: python3 analyse.py raw/*.txt
"""
import sys
import glob
from collections import defaultdict

ROWS_500M = 500_000_000


def load(paths):
    cells = []
    for p in paths:
        name = p.split("/")[-1].replace("tiles-", "").replace(".txt", "")
        scale, grant = name.rsplit("-", 1)
        for line in open(p):
            if not line.startswith("CSV,"):
                continue
            f = line.strip().split(",")
            if f[1] == "shape":
                continue
            cells.append(
                dict(
                    scale=scale,
                    grant=grant,
                    shape=f[1],
                    zoom=int(f[2]),
                    tiles=int(f[3]),
                    rows=int(f[4]),
                    serial=int(f[5]),
                    par=int(f[6]),
                    ratio=float(f[7]),
                )
            )
    return cells


def regret(c, go_parallel):
    """ns lost by taking `go_parallel` instead of the better measured arm, and the ratio."""
    chosen = c["par"] if go_parallel else c["serial"]
    best = min(c["par"], c["serial"])
    return chosen - best, chosen / best


def evaluate(cells, name, rule):
    tot = 0
    worst = (1.0, None)
    worst_abs = (0, None)
    n_wrong = 0
    for c in cells:
        d, r = regret(c, rule(c))
        tot += d
        if r > 1.001:
            n_wrong += 1
        if r > worst[0]:
            worst = (r, c)
        if d > worst_abs[0]:
            worst_abs = (d, c)
    w, wc = worst
    a, ac = worst_abs
    if wc is None:
        print(f"{name:<34} total_regret={tot/1e6:9.2f} ms  misclassified=  0/{len(cells)}")
        return
    print(
        f"{name:<34} total_regret={tot/1e6:9.2f} ms  misclassified={n_wrong:3d}/{len(cells)}  "
        f"worst_ratio={w:6.2f}x ({wc['scale']}/{wc['grant']} {wc['shape']} "
        f"t={wc['tiles']} r={wc['rows']})  worst_abs={a/1e6:7.3f} ms "
        f"({ac['scale']}/{ac['grant']} {ac['shape']} t={ac['tiles']})"
    )


def main():
    paths = sys.argv[1:] or sorted(glob.glob("raw/*.txt"))
    cells = load(paths)
    print(f"{len(cells)} measured cells from {len(paths)} runs\n")

    # --- crossover location per family, per run ------------------------------------------------
    print("Tile crossover per (scale, grant, family): last SERIAL tile count -> first PAR")
    fams = defaultdict(list)
    for c in cells:
        fams[(c["scale"], c["grant"], c["shape"].split("/")[0])].append(c)
    for key in sorted(fams):
        fam = sorted(fams[key], key=lambda c: c["tiles"])
        last_serial = max((c["tiles"] for c in fam if c["ratio"] >= 1.0), default=None)
        first_par = min((c["tiles"] for c in fam if c["ratio"] < 1.0), default=None)
        print(f"  {key[0]:>3}/{key[1]:<6} {key[2]:<8} last_serial={last_serial}  first_par={first_par}")

    print("\nGlobal bracket over every measured cell:")
    ser = [c for c in cells if c["ratio"] >= 1.0]
    par = [c for c in cells if c["ratio"] < 1.0]
    print(f"  highest tile count that measured SERIAL-favouring: {max(c['tiles'] for c in ser)}")
    print(f"  lowest  tile count that measured PAR-favouring   : {min(c['tiles'] for c in par)}")
    print(f"  highest row  count that measured SERIAL-favouring: {max(c['rows'] for c in ser)}")
    print(f"  lowest  row  count that measured PAR-favouring   : {min(c['rows'] for c in par)}")

    # --- rule evaluation -----------------------------------------------------------------------
    print("\nCandidate rules (parallel iff the predicate holds):")
    evaluate(cells, "always serial (pre-parallel)", lambda c: False)
    evaluate(cells, "always parallel", lambda c: True)
    evaluate(cells, "rows >= 500M  [status quo]", lambda c: c["rows"] >= ROWS_500M)
    for t in (1024, 2048, 4096, 8192, 16384):
        evaluate(
            cells,
            f"rows >= 500M OR tiles >= {t}",
            lambda c, t=t: c["rows"] >= ROWS_500M or c["tiles"] >= t,
        )
    for t in (4096, 8192):
        evaluate(cells, f"tiles >= {t} only", lambda c, t=t: c["tiles"] >= t)
    evaluate(cells, "oracle (per-cell best)", lambda c: c["par"] < c["serial"])

    # --- where the chosen rule errs -------------------------------------------------------------
    for t in (4096, 8192):
        print(f"\nEvery cell misclassified by `rows >= 500M OR tiles >= {t}` (ratio > 1.05):")
        for c in sorted(cells, key=lambda c: -regret(c, c["rows"] >= ROWS_500M or c["tiles"] >= t)[1]):
            go = c["rows"] >= ROWS_500M or c["tiles"] >= t
            d, r = regret(c, go)
            if r <= 1.05:
                continue
            print(
                f"  {c['scale']:>3}/{c['grant']:<6} {c['shape']:<10} tiles={c['tiles']:>6} "
                f"rows={c['rows']:>10}  chose={'PAR' if go else 'SERIAL'}  "
                f"{r:5.2f}x  +{d/1e6:6.3f} ms  (serial={c['serial']/1e6:.3f} par={c['par']/1e6:.3f})"
            )

    # --- the natural family, which the 500M constant exists to protect ---------------------------
    print("\nThe `natural` family (289 tiles at every zoom) under every candidate:")
    for c in [c for c in cells if c["shape"].startswith("natural")]:
        print(
            f"  {c['scale']:>3}/{c['grant']:<6} {c['shape']:<12} tiles={c['tiles']} "
            f"rows={c['rows']:>10} ratio={c['ratio']:5.2f} -> serial is correct; "
            f"tile arm fires at >= {c['tiles'] + 1}, so no threshold >= 1024 touches it"
        )

    # --- per-tile serial cost, the quantity that makes tiles a stable axis ----------------------
    print("\nSerial ns per tile, by scale/grant, for the low-row families (mask-independent part):")
    for key in sorted({(c["scale"], c["grant"]) for c in cells}):
        sel = [c for c in cells if (c["scale"], c["grant"]) == key and c["tiles"] >= 1000]
        if not sel:
            continue
        v = sorted(c["serial"] / c["tiles"] for c in sel)
        print(f"  {key[0]:>3}/{key[1]:<6} n={len(v):2d}  min={v[0]:7.1f}  median={v[len(v)//2]:7.1f}  max={v[-1]:7.1f}")


main()
