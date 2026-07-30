"""Brute-force tile counts and §7.2 selection — the entity-space vs row-space differential.

`counts` walks every row of a slice's segment (row-space) and buckets it into whichever depth-`d`
tile it belongs to, counting only rows whose entity is in the caller-supplied mask. This is
deliberately the slow, obviously-correct construction (module scope's "obviously correct" over
"fast") — no bitmap arithmetic, no cached row projection, just a loop over the sorted Morton
array with binary search for tile boundaries (the boundaries are found by searching a sorted
array — reading, not indexing tricks with authorisation semantics).

`served` implements design §7.2's selection definition — floor ∪ threshold ∪ cap over `tessera_id`,
evaluated inside the mask. It replaces the pre-2026-07-30 `first_k`, which mirrored the engine's
placeholder row-order sampler; that placeholder is gone, and with it the whole reason the point-set
differential used to be expected to disagree.

**This is written from the definition, not from the Rust.** The arithmetic here is deliberately
shaped differently from `crates/tessera-engine/src/select.rs` — a brute-force sort of the tile's
visible identities and a slice, against the engine's single-pass bounded heap — because a
differential between two transcriptions of the same code proves only that copy-paste works. What is
shared is the *definition*, which is the contract between them.
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


def visible_total(bundle: Bundle, mask: set[int], slice_id: str) -> int:
    """The viewer's total visible count over the whole slice — θ's anchor input.

    Computed independently from the segment and the pairs-derived mask, deliberately **not** read
    back from a server response: θ's anchor is the one input the oracle would otherwise have to take
    on trust, and taking it from the thing under test would make the differential circular for every
    density assertion.
    """
    seg = bundle.segment(slice_id)
    return sum(1 for row in range(seg.row_count) if int(seg.entity_id[row]) in mask)


def theta_cut(v_total: int, m_target: int, depth: int) -> int | None:
    """θ_d as a cut point over the identity space, or `None` for "saturated: admits everything".

    `P_0 = m_target * 2**64 // v_total`, then `P_d = P_0 << 2d`, saturating at `2**64` — §7.2's
    closed-form anchor. The ×4 per depth is what makes the per-tile expectation depth-stable, and
    saturation is a distinct state rather than a clamp to `2**64 - 1`, because at θ ≥ 1 the threshold
    must admit *every* identity including `2**64 - 1`.

    `v_total` is the viewer's **composed** visible total over the whole slice — the mask after the
    overlay diff, not the raw fragment. I2: the pre-overlay figure is not computable from inside
    `M_auth`.
    """
    if v_total <= 0:
        return None
    p0 = (m_target << 64) // v_total
    if p0 >= 1 << 64:
        return None
    p_d = p0 << (2 * depth)
    if p_d >= 1 << 64:
        return None
    return p_d


def served(
    bundle: Bundle,
    mask: set[int],
    slice_id: str,
    zoom: int,
    tile: int,
    *,
    k_min: int,
    cap: int,
    v_total: int,
    m_target: int,
) -> list[tuple[float, float]]:
    """Design §7.2's served set for one tile, as `(x, y)` pairs in served order.

        C_theta = |{ i in vis(T) : tessera_id(i) < P_d }|
        m       = min(cap, max(min(k_min, cap), C_theta))
        served  = the min(m, |vis(T)|) smallest of vis(T) by tessera_id

    Brute force on purpose: collect the tile's visible rows, sort them by stored `tessera_id`, count
    how many fall below the cut, and slice. The engine reaches the same answer with a single pass and
    a bounded heap; the differential is only evidence because the two constructions differ.

    Returned in ascending `tessera_id` order, which is the order the payload must arrive in — both
    engine routes emit it, and the nesting argument's client-truncation clause depends on the served
    set being a prefix.
    """
    seg = bundle.segment(slice_id)
    lo, hi = morton.code_range(tile, zoom)
    lo_idx = int(np.searchsorted(seg.morton, lo, side="left"))
    hi_idx = int(np.searchsorted(seg.morton, hi, side="left"))

    if seg.tessera_id is None:
        raise ValueError("segment has no stored tessera_id column (pre-r6 bundle)")

    visible: list[tuple[int, int]] = []
    for row in range(lo_idx, hi_idx):
        if int(seg.entity_id[row]) in mask:
            visible.append((int(seg.tessera_id[row]), row))
    visible.sort()

    cut = theta_cut(v_total, m_target, zoom)
    if cut is None:
        c_theta = len(visible)
    else:
        c_theta = sum(1 for ident, _ in visible if ident < cut)

    floor = min(k_min, cap)
    m = min(cap, max(floor, c_theta))
    m = min(m, len(visible))

    return [(float(seg.x[row]), float(seg.y[row])) for _, row in visible[:m]]
