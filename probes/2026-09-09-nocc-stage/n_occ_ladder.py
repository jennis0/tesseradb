#!/usr/bin/env python3
"""`N_occ(d)` for every depth, exactly, straight off a segment's `morton.u32`.

**Independent of the engine's walk and of the sketch.** The column is stored in Morton order, so
the depth-*d* tile index `code >> (32 - 2d)` is non-decreasing along it and the number of distinct
values is one plus the number of positions where it steps. That is a `numpy` diff, not a hash set,
and it shares no code with `crates/tessera-engine/src/occupancy.rs`.

Whole-column only: it answers for the *unmasked* view, which is what a full-coverage principal
sees, and it is the ladder shape the staging policy's depth cap is argued from — not a masked
figure.

    n_occ_ladder.py <morton.u32> [more...]
"""
import sys
import numpy as np

for path in sys.argv[1:]:
    codes = np.fromfile(path, dtype=np.uint32)
    print(f"# {path}\n# {codes.size:,} rows ({codes.nbytes / 2**30:.2f} GiB)")
    print(f"{'depth':>5} {'N_occ':>14} {'4^d':>16} {'N_occ/4^d':>10} {'growth':>8} "
          f"{'share of N_occ(16)':>19}")
    deepest = None
    previous = None
    for d in range(17):
        counts = codes >> np.uint32(32 - 2 * d)
        n = 1 + int(np.count_nonzero(counts[1:] != counts[:-1])) if codes.size else 0
        if d == 16:
            deepest = n
        grid = 4 ** d
        growth = "" if previous in (None, 0) else f"{n / previous:.2f}x"
        print(f"{d:>5} {n:>14,} {grid:>16,} {n / grid:>10.4f} {growth:>8}", flush=True)
        previous = n
    print(f"# N_occ(16) = {deepest:,}")
