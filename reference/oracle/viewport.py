"""Brute-force tile counts and first-k sampling — the entity-space vs row-space differential.

`counts` walks every row of a slice's segment (row-space) and buckets it into whichever depth-`d`
tile it belongs to, counting only rows whose entity is in the caller-supplied mask. This is
deliberately the slow, obviously-correct construction (module scope's "obviously correct" over
"fast") — no bitmap arithmetic, no cached row projection, just a loop over the sorted Morton
array with binary search for tile boundaries (the boundaries are found by searching a sorted
array — reading, not indexing tricks with authorisation semantics).

`first_k` mirrors the *placeholder* first-k sampler the Phase 1 engine actually ships
(`tessera-engine`'s `sample_tile` doc: "deliberately wrong... not the priority-sample
definition" — R3's priority order is Phase 2 work). It takes the first `k` visible rows in
ascending row (Morton) order within the tile's range, which is what the server currently returns.
When the differential test compares point sets it must therefore either request `k` at least as
large as a tile's visible count (an untruncated comparison, safe to compare as full sets) or
compare containment rather than exact order — see `tests/test_differential.py`'s comments at the
call site.
"""

from __future__ import annotations

import numpy as np

from . import morton
from .bundle import Bundle


def _membership(mask: set[int], bound: int) -> np.ndarray:
    """A boolean lookup array over entity ids [0, bound) — the vectorised form of `in mask`."""
    arr = np.zeros(bound, dtype=bool)
    if mask:
        idx = np.fromiter((e for e in mask if e < bound), dtype=np.uint64)
        arr[idx] = True
    return arr


def counts(
    bundle: Bundle,
    mask: set[int],
    slice_id: str,
    zoom: int,
    bbox: tuple[float, float, float, float],
) -> dict[int, int]:
    """tile -> count of mask-visible rows, for every depth-`zoom` tile overlapping bbox."""
    seg = bundle.segment(slice_id)
    # `_membership`'s bound must cover the largest entity id ever seen — the mask (pairs-derived,
    # possibly widened by a predicate change onto an id outside the segment's own range) and the
    # segment's own entity ids alike.
    bound = max((max(mask) + 1) if mask else 0, int(seg.entity_id.max()) + 1 if seg.row_count else 0)
    member = _membership(mask, bound)

    tiles = morton.tiles_for_bbox(bbox, zoom, bundle.extent)
    out: dict[int, int] = {}
    for tile in tiles:
        lo, hi = morton.code_range(tile, zoom)
        lo_idx = int(np.searchsorted(seg.morton, lo, side="left"))
        hi_idx = int(np.searchsorted(seg.morton, hi, side="left"))
        if hi_idx <= lo_idx:
            continue
        entities_in_range = seg.entity_id[lo_idx:hi_idx].astype(np.int64)
        visible = int(member[entities_in_range].sum())
        if visible > 0:
            out[tile] = visible
    return out


def first_k(
    bundle: Bundle,
    mask: set[int],
    slice_id: str,
    zoom: int,
    tile: int,
    k: int,
) -> list[tuple[float, float]]:
    """The first `k` mask-visible rows in ascending row (Morton) order within `tile`.

    Matches the engine's current placeholder sampler (`sample_tile`'s doc) — see this module's
    doc for why this, and not an R3 priority order, is what the differential compares against.
    """
    seg = bundle.segment(slice_id)
    lo, hi = morton.code_range(tile, zoom)
    lo_idx = int(np.searchsorted(seg.morton, lo, side="left"))
    hi_idx = int(np.searchsorted(seg.morton, hi, side="left"))

    out: list[tuple[float, float]] = []
    for row in range(lo_idx, hi_idx):
        if int(seg.entity_id[row]) in mask:
            out.append((float(seg.x[row]), float(seg.y[row])))
            if len(out) >= k:
                break
    return out
