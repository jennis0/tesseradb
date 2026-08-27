"""Load the `notebook-2m4` corpus into per-artifact member positions, on the engine's grid.

The corpus is read from `data/notebook-2m4-live/*.parquet` rather than through the engine: the
question here is about shape families, and every family in this probe is a function of the member
positions alone. Positions are quantised **exactly as the build quantises** — `fixed32(v) =
clamp(floor((v - min) / (max - min) * 2^32), 0, 2^32 - 1)` (`tessera-spatial/src/morton.rs`)
against the bounds the built bundle records — so a shape computed here is over the same integer
lattice the engine's is, and the two are comparable vertex for vertex.

The bounds come from the bundle's own `MANIFEST.json`, not from the parquet's extent: the build's
extent is padded, and quantising against a tighter box would move every position.

Output: one `.npz` per layer under `cache/`, holding a concatenated `xy` array (uint32, N x 2), an
`offsets` array, and the artifact keys, so the family survey does not re-read 1.5 GB of points.
"""

import json
import os
import sys

import numpy as np
import pyarrow.parquet as pq

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
DATA = os.environ.get("SHAPES_DATA", "/home/joe/code/tessera/data/notebook-2m4-live")
BUNDLE = os.environ.get(
    "SHAPES_BUNDLE",
    "/home/joe/code/tessera/.claude/worktrees/client-corpus/clients/ts/.dev/"
    "bundle-notebook-2m4/v00000/MANIFEST.json",
)
CACHE = os.path.join(HERE, "cache")


def quantisation():
    """The build's own extent, as the bundle records it."""
    with open(BUNDLE) as fh:
        q = json.load(fh)["quantisation"]
    return q["x_min"], q["x_max"], q["y_min"], q["y_max"]


def fixed32(v, lo, hi):
    """`tessera_spatial::morton::fixed32`, vectorised. Computed in f64, floored, clamped."""
    scaled = np.floor((v - lo) / (hi - lo) * 4294967296.0)
    return np.clip(scaled, 0.0, 4294967295.0).astype(np.uint32)


def build_cache(layer="hdbscan"):
    os.makedirs(CACHE, exist_ok=True)
    out = os.path.join(CACHE, f"{layer}.npz")
    if os.path.exists(out):
        return out

    x_min, x_max, y_min, y_max = quantisation()
    pts = pq.read_table(os.path.join(DATA, "points.parquet"), columns=["entity_id", "x", "y"])
    eid = pts["entity_id"].to_numpy()
    qx = fixed32(pts["x"].to_numpy(), x_min, x_max)
    qy = fixed32(pts["y"].to_numpy(), y_min, y_max)
    del pts

    # entity_id -> row, by sort rather than by dict: 2.4M entries, and the members table is 12.8M.
    order = np.argsort(eid, kind="stable")
    eid_sorted = eid[order]

    mem = pq.read_table(
        os.path.join(DATA, f"clusters-{layer}-members.parquet"), columns=["key", "entity"]
    )
    keys = mem["key"].to_numpy(zero_copy_only=False)
    ent = mem["entity"].to_numpy()
    del mem

    idx = np.searchsorted(eid_sorted, ent)
    ok = (idx < len(eid_sorted)) & (eid_sorted[np.clip(idx, 0, len(eid_sorted) - 1)] == ent)
    if not ok.all():
        print(f"  {(~ok).sum()} member rows name an entity with no point row; dropped", file=sys.stderr)
    rows = order[idx[ok]]
    keys = keys[ok]

    # Group by key. Sort the key array once and take contiguous runs.
    korder = np.argsort(keys, kind="stable")
    keys_s = keys[korder]
    rows_s = rows[korder]
    uniq, starts = np.unique(keys_s, return_index=True)
    offsets = np.append(starts, len(rows_s)).astype(np.int64)

    xy = np.stack([qx[rows_s], qy[rows_s]], axis=1)
    np.savez_compressed(out, xy=xy, offsets=offsets, keys=uniq)
    return out


def corpus():
    """Every point in the corpus, quantised, deduplicated — the denominator for `precision`.

    Read from `points.parquet` rather than from a layer: only `clusters/hdbscan`'s root happens to
    hold the whole corpus, and taking a layer's largest artifact for the corpus is right there and
    wrong everywhere else.
    """
    out = os.path.join(CACHE, "corpus.npz")
    if not os.path.exists(out):
        os.makedirs(CACHE, exist_ok=True)
        x_min, x_max, y_min, y_max = quantisation()
        pts = pq.read_table(os.path.join(DATA, "points.parquet"), columns=["x", "y"])
        xy = np.stack(
            [
                fixed32(pts["x"].to_numpy(), x_min, x_max),
                fixed32(pts["y"].to_numpy(), y_min, y_max),
            ],
            axis=1,
        )
        np.savez_compressed(out, xy=np.unique(xy, axis=0))
    return np.load(out)["xy"].astype(np.float64)


class Layer:
    def __init__(self, layer="hdbscan"):
        z = np.load(build_cache(layer), allow_pickle=True)
        self.xy = z["xy"]
        self.offsets = z["offsets"]
        self.keys = [str(k) for k in z["keys"]]

    def __len__(self):
        return len(self.keys)

    def members(self, i):
        return self.xy[self.offsets[i] : self.offsets[i + 1]]

    def sizes(self):
        return np.diff(self.offsets)


if __name__ == "__main__":
    for name in sys.argv[1:] or ["hdbscan"]:
        lay = Layer(name)
        s = lay.sizes()
        print(f"{name}: {len(lay)} artifacts, {s.min()} … {s.max()} members, {s.sum()} member rows")
