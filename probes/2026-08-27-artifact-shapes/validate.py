"""Check the Python reimplementation of the engine's dig against the engine's own measurement.

The figures on the right of the table are `docs/evidence/memos/2026-08-26-concave-hulls.md`, which
measured the built Rust construction over the same 197-artifact layer at full membership. If this
reproduction disagrees with them, nothing else in this probe can be trusted, because every family
here is compared against the shape `main` serves.

    python3 validate.py
"""

import json
import os
import sys
import time

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from load import Layer  # noqa: E402
from shapes import alpha_dig, convex, ring_area  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def main():
    layer = Layer("hdbscan")
    rows = []
    t0 = time.time()
    for i in range(len(layer)):
        p = layer.members(i)
        wrap = convex(p)[0]
        shape, trace = alpha_dig(p, trace=True)
        a_wrap = abs(ring_area(wrap))
        a_shape = abs(ring_area(shape[0]))
        rows.append(
            {
                "key": layer.keys[i],
                "members": int(len(p)),
                "wrap_vertices": len(wrap),
                "shape_vertices": len(shape[0]),
                "area_ratio": (a_shape / a_wrap) if a_wrap > 0 else 1.0,
                "at_budget": bool(trace["at_budget"]),
                "refused_digs": trace["refused_digs"],
                "bridges_left": trace["bridges_left"],
                "alpha": trace["alpha"],
            }
        )
        if (i + 1) % 25 == 0:
            print(f"  {i + 1}/{len(layer)} ({time.time() - t0:.0f}s)", file=sys.stderr)

    with open(os.path.join(HERE, "results-dig.json"), "w") as fh:
        json.dump(rows, fh, indent=1)

    ratios = np.array([r["area_ratio"] for r in rows])
    print("\n| quantity | this probe | memo (engine, Rust) |")
    print("|---|---|---|")
    print(f"| artifacts | {len(rows)} | 197 |")
    print(f"| wrap vertices, whole layer | {sum(r['wrap_vertices'] for r in rows)} | 3,278 |")
    print(f"| shape vertices, whole layer | {sum(r['shape_vertices'] for r in rows)} | 12,388 |")
    print(f"| area ratio, median | {np.median(ratios):.3f} | 0.870 |")
    print(f"| area ratio, mean | {ratios.mean():.3f} | 0.858 |")
    print(f"| area ratio, min | {ratios.min():.3f} | 0.290 |")
    print(f"| at the 64 budget | {sum(r['at_budget'] for r in rows)} / {len(rows)} | 108 / 197 |")
    print()
    print(f"digs refused, whole layer: {sum(r['refused_digs'] for r in rows)}")
    print(
        "artifacts finishing with a live edge still above alpha: "
        f"{sum(1 for r in rows if r['bridges_left'] > 0)} / {len(rows)}"
    )
    print(f"elapsed {time.time() - t0:.0f}s")


if __name__ == "__main__":
    main()
