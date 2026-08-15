"""Entitlements and the recording diff — correctness-suite §10's comparison, §12.3's checker.

**A stage may change only what it is entitled to change, and for most stages that is nothing.**
The check is `diff(before, after) == stage.entitlement()`: two recordings of the battery, reduced
to a [`Delta`] in *item* identities, compared against what the stage declared. An entitlement
rather than a property list, because a property list enumerates what somebody thought to assert —
§10's argument is that equality-with-before covers every property nobody thought of, and admits
no sixth thing when five are listed.

The entitlement grammar is §12.3's — ``Nothing | Entity(e) | Rows(items)`` — with one
normalisation this module makes deliberately: all three construct the same [`Delta`] type, a pair
of item sets (appeared, vanished). The wire cannot distinguish "one suppressed entity returned"
from "one row ingested" — both are a single appeared row — and it does not need to: the stage's
*author* knows which stage they wrote, and equality against the declared form is exact either
way. Items are named by ``fx_key``, the planted join column (correctness-suite §12.1: that column
*is* the item identity, and a second name for it would be drift), which every served points row
carries and which is the only identity that survives a rebuild.

**What the diff refuses to launder.** ``Nothing`` is bytes-equal on every canonical surface —
never "the row sets agree". A byte change that moves no row (an order change, a re-encoding, a
count surface disagreeing with the points that back it) is returned as [`Unexplained`], which
equals no entitlement, because served order and the count/points agreement *are* contract
(contracts §3.2) and a diff that shrugged at them would pass the corruption classes §8.1 lists —
the mis-interleave that preserves counts, the count that moves without a row. Concretely, a
changed recording must decompose exactly:

- the points surfaces' added/removed rows, order-preserved in the residual (removing the vanished
  rows from *before* and the appeared rows from *after* must leave identical sequences);
- per-tile count deltas equal to those rows' tiles — ``visible`` from every appeared/vanished
  item (they are mask-visible by construction), ``matched``/``served`` from the rows this query
  itself served, so a filtered viewport that matches none of them is still held to its ``visible``
  movement;
- underlay cell deltas equal to the same items' cells at ``zoom + underlay_offset``;
- the trailer equal but for ``points`` (which must track the served delta) and ``flushes`` (a
  deterministic function of served bytes, so a stage entitled to change content is entitled to
  move it — but it must not move when the served bytes did not);
- ``/v1/items`` flips exactly at the appeared/vanished items, 404 being a real answer on that
  surface;
- ``/v1/meta`` and ``/v1/categories`` unchanged — no stage in this suite's plans is entitled to
  either, because the plans ingest only established vocabulary values and deny only items whose
  category values have other visible holders (a vocabulary-extending flush is real and out of
  this suite's scope).

**Preconditions inherited from §10, stated here because the diff silently depends on them:**
selection saturated (θ and both caps above the corpus total — otherwise removing a served row
admits the next one behind it and a one-row delta on the wire is not the corpus's one-row delta),
and every battery viewport covering the full extent (the cross-viewport consistency rule — every
unfiltered viewport must report the same appeared/vanished sets — is only sound when they all see
the whole corpus).
"""

from __future__ import annotations

import json
from collections import defaultdict
from dataclasses import dataclass
from typing import Iterable, NamedTuple

import pyarrow as pa
import pyarrow.ipc as ipc

from .battery import Categories, Item, Meta, Recorded, Viewport
from .canonical import Json, Streamed

#: The planted join column — the item identity (correctness-suite §12.1). The one column name
#: this module hard-codes, deliberately: the suite's corpus plants it under exactly this name so
#: that no second identity is minted beside it.
FX_COLUMN = "fx_key"


@dataclass(frozen=True)
class Delta:
    """What a stage changed, in item identities: the sets that appeared and vanished.

    ``Delta(∅, ∅)`` — [`Nothing`] — is the strongest assertion in the suite, and [`diff`] returns
    it only for bytes-equal recordings (module doc).
    """

    appeared: frozenset[int]
    vanished: frozenset[int]

    def __repr__(self) -> str:  # compact in assertion messages
        def show(s: frozenset[int]) -> str:
            if not s:
                return "∅"
            sample = ", ".join(f"{v:#x}" for v in sorted(s)[:3])
            return f"{{{sample}{', …' if len(s) > 3 else ''}}} ({len(s)})"

        return f"Delta(appeared={show(self.appeared)}, vanished={show(self.vanished)})"


def Nothing() -> Delta:
    """No response changes at all — the entitlement of load, coalesce, merge, rotation and the
    fold (§10's table). The fold's is deliberately not "minus the rows it deleted": an accepted
    deletion left the served surface at acceptance (write-path §5.4), so the fold's own delta is
    empty and stating anything else would measure the deny lane instead of the fold."""
    return Delta(frozenset(), frozenset())


def Entity(fx_key: int, *, restored: bool = False) -> Delta:
    """Exactly the item named — a deny-lane acceptance (`suppress`, `delete`), or its return
    (`unsuppress`, ``restored=True``). Observable only when the item is visible to the recording
    principal; a plan that denies an invisible item has declared an entitlement the wire cannot
    show, and fails honestly."""
    one = frozenset((fx_key,))
    return Delta(one, frozenset()) if restored else Delta(frozenset(), one)


def Rows(items: Iterable[int]) -> Delta:
    """Exactly the rows ingested since the last refresh — flush's entitlement, the one stage
    entitled to change answers, because the background refresh exists to change them
    (decision 0044 D1)."""
    return Delta(frozenset(items), frozenset())


@dataclass(frozen=True)
class Unexplained:
    """A recording change the entitlement algebra cannot express — a defect, not a delta.

    Equal to no [`Delta`], so ``diff(...) == entitlement`` fails for every declared entitlement,
    and the reasons carry the evidence to the assertion message.
    """

    reasons: tuple[str, ...]

    def __repr__(self) -> str:
        shown = "\n  - ".join(self.reasons[:8])
        more = f"\n  … and {len(self.reasons) - 8} more" if len(self.reasons) > 8 else ""
        return f"Unexplained(\n  - {shown}{more}\n)"


class _Row(NamedTuple):
    """One served point, keyed for the residual check by its full column tuple."""

    key: tuple
    tessera_id: int
    code: int
    fx: int


def _tables(concatenated: bytes) -> pa.Table | None:
    """Decode a canonical surface that is zero or more complete Arrow IPC streams, concatenated.

    The points surface keeps per-frame stream headers by design (§12.2 step 4 — frame boundaries
    are not contract, but their placement is a function of served content, so the bytes are
    comparable); each stream is self-delimiting, so repeated `open_stream` over one buffer reader
    walks them all. ``b""`` — a zero-point response, or an unrequested underlay — is `None`.
    """
    if not concatenated:
        return None
    reader_src = pa.BufferReader(concatenated)
    tables: list[pa.Table] = []
    while reader_src.tell() < len(concatenated):
        with ipc.open_stream(reader_src) as reader:
            tables.append(reader.read_all())
    return pa.concat_tables(tables) if len(tables) > 1 else tables[0]


def _point_rows(canon: Streamed, label: str, reasons: list[str]) -> list[_Row]:
    table = _tables(canon.points)
    if table is None:
        return []
    names = table.schema.names
    for required in ("tessera_id", "code", FX_COLUMN):
        if required not in names:
            reasons.append(f"{label}: points schema lacks `{required}` — cannot attribute rows")
            return []
    columns = [table.column(name).to_pylist() for name in names]
    ti, ci, fi = names.index("tessera_id"), names.index("code"), names.index(FX_COLUMN)
    rows = []
    for values in zip(*columns):
        rows.append(_Row(key=values, tessera_id=values[ti], code=values[ci], fx=values[fi]))
    return rows


def _tile_map(canon: Streamed) -> dict[int, tuple[int, int, int]]:
    table = _tables(canon.tiles)
    if table is None:
        return {}
    return {
        t: (v, m, s)
        for t, v, m, s in zip(
            table.column("tile").to_pylist(),
            table.column("visible").to_pylist(),
            table.column("matched").to_pylist(),
            table.column("served").to_pylist(),
        )
    }


def _cell_map(canon: Streamed) -> dict[int, int]:
    table = _tables(canon.underlay)
    if table is None:
        return {}
    return dict(zip(table.column("cell").to_pylist(), table.column("count").to_pylist()))


def _prefix_of(code: int, depth: int) -> int:
    """The depth-`depth` Morton prefix of a served 64-bit position code.

    The code is ``(cell32 << 32) | residual`` with ``cell32`` the depth-16 interleave
    (`oracle.morton.split32`), so a depth-*d* tile — and an underlay cell, which is the same
    quantity at ``zoom + offset`` — is its top ``2d`` bits.
    """
    return code >> (64 - 2 * depth) if depth else 0


@dataclass
class _VpDiff:
    rows_before: list[_Row]
    rows_after: list[_Row]
    added: list[_Row]
    removed: list[_Row]


def _analyse_viewport(
    query: Viewport, before: Streamed, after: Streamed, reasons: list[str]
) -> _VpDiff:
    label = f"viewport(zoom={query.zoom}, filters={'yes' if query.filters else 'no'}, form={'tiles' if query.tiles else 'bbox'})"
    rows_before = _point_rows(before, label, reasons)
    rows_after = _point_rows(after, label, reasons)

    before_keys = {r.key for r in rows_before}
    after_keys = {r.key for r in rows_after}
    removed = [r for r in rows_before if r.key not in after_keys]
    added = [r for r in rows_after if r.key not in before_keys]

    # The residual must be identical *in order*: contracts §3.2 orders points ascending by
    # `tessera_id` within each tile, so an unexplained reordering is a defect, never noise.
    residual_before = [r.key for r in rows_before if r.key in after_keys]
    residual_after = [r.key for r in rows_after if r.key in before_keys]
    if residual_before != residual_after:
        reasons.append(
            f"{label}: beyond the {len(added)} added / {len(removed)} removed rows, the "
            f"surviving rows changed order or content — a permutation-preserving surface would "
            f"have hidden this"
        )
    if before.points != after.points and not added and not removed and residual_before == residual_after:
        reasons.append(
            f"{label}: points bytes differ with no row difference — encoding or chunking drift, "
            f"which §12.2 step 4's content-determinism assumption says must not happen"
        )
    return _VpDiff(rows_before, rows_after, added, removed)


def _check_viewport_counts(
    query: Viewport,
    before: Streamed,
    after: Streamed,
    vd: _VpDiff,
    appeared: frozenset[int],
    vanished: frozenset[int],
    code_of_fx: dict[int, int],
    reasons: list[str],
) -> None:
    label = f"viewport(zoom={query.zoom}, filters={'yes' if query.filters else 'no'}, form={'tiles' if query.tiles else 'bbox'})"

    # Expected per-tile movement. `visible` moves with every appeared/vanished item — they are
    # mask-visible by construction (they were, or became, *served* under a saturated selection in
    # an unfiltered viewport) — while `matched`/`served` move only with the rows this query itself
    # served, which is what lets one rule cover filtered and unfiltered viewports alike.
    expected: dict[int, list[int]] = defaultdict(lambda: [0, 0, 0])
    for fx in appeared:
        expected[_prefix_of(code_of_fx[fx], query.zoom)][0] += 1
    for fx in vanished:
        expected[_prefix_of(code_of_fx[fx], query.zoom)][0] -= 1
    for row in vd.added:
        tile = _prefix_of(row.code, query.zoom)
        expected[tile][1] += 1
        expected[tile][2] += 1
    for row in vd.removed:
        tile = _prefix_of(row.code, query.zoom)
        expected[tile][1] -= 1
        expected[tile][2] -= 1

    tiles_before = _tile_map(before)
    tiles_after = _tile_map(after)
    for tile in sorted(set(tiles_before) | set(tiles_after) | set(expected)):
        b = tiles_before.get(tile, (0, 0, 0))
        a = tiles_after.get(tile, (0, 0, 0))
        actual = (a[0] - b[0], a[1] - b[1], a[2] - b[2])
        if actual != tuple(expected.get(tile, [0, 0, 0])):
            reasons.append(
                f"{label}: tile {tile} moved (visible, matched, served) by {actual}, but the "
                f"attributable rows say {tuple(expected.get(tile, [0, 0, 0]))}"
            )
    if before.tiles != after.tiles and tiles_before == tiles_after:
        reasons.append(f"{label}: tiles bytes differ without a value difference")

    # The underlay counts visible items per cell at depth `zoom + offset` (§3.3), so it moves
    # with the same item sets as `visible`.
    depth = query.zoom + query.underlay_offset
    cell_expected: dict[int, int] = defaultdict(int)
    for fx in appeared:
        cell_expected[_prefix_of(code_of_fx[fx], depth)] += 1
    for fx in vanished:
        cell_expected[_prefix_of(code_of_fx[fx], depth)] -= 1
    cells_before = _cell_map(before)
    cells_after = _cell_map(after)
    for cell in sorted(set(cells_before) | set(cells_after) | set(cell_expected)):
        actual_delta = cells_after.get(cell, 0) - cells_before.get(cell, 0)
        if actual_delta != cell_expected.get(cell, 0):
            reasons.append(
                f"{label}: underlay cell {cell} moved by {actual_delta}, expected "
                f"{cell_expected.get(cell, 0)}"
            )
    if before.underlay != after.underlay and cells_before == cells_after:
        reasons.append(f"{label}: underlay bytes differ without a value difference")

    # The trailer's remainder: `points` must track the served delta exactly; `flushes` is a
    # deterministic function of served bytes (canonical module doc), so it may move only when
    # they did.
    trailer_before = json.loads(before.trailer)
    trailer_after = json.loads(after.trailer)
    served_delta = len(vd.added) - len(vd.removed)
    if trailer_after.get("points") != trailer_before.get("points", 0) + served_delta:
        reasons.append(
            f"{label}: trailer points went {trailer_before.get('points')} -> "
            f"{trailer_after.get('points')}, but the served delta is {served_delta:+d}"
        )
    content_changed = (
        before.points != after.points
        or before.tiles != after.tiles
        or before.underlay != after.underlay
    )
    for key in set(trailer_before) | set(trailer_after):
        if key == "points":
            continue
        if key == "flushes" and content_changed:
            continue
        if trailer_before.get(key) != trailer_after.get(key):
            reasons.append(
                f"{label}: trailer `{key}` went {trailer_before.get(key)!r} -> "
                f"{trailer_after.get(key)!r} with no served-content change to carry it"
            )


def diff(before: Recorded, after: Recorded) -> Delta | Unexplained:
    """Reduce two recordings of one battery to the [`Delta`] between them (module doc).

    Returns [`Nothing`]'s delta only when every surface is bytes-equal; a [`Delta`] when every
    changed byte is attributable to the appeared/vanished item sets; [`Unexplained`] otherwise.
    """
    if set(before) != set(after):
        return Unexplained(
            ("the two recordings answer different batteries — the driver recorded them wrong",)
        )
    changed = {q for q in before if before[q] != after[q]}
    if not changed:
        return Nothing()

    reasons: list[str] = []
    viewports = [q for q in before if isinstance(q, Viewport)]
    analysed = {
        q: _analyse_viewport(q, before[q], after[q], reasons) for q in viewports
    }

    # Item identity is carried by every served row; both sides of every viewport contribute, so a
    # row that exists only before (a vanished item) still names itself.
    code_of_fx: dict[int, int] = {}
    fx_of_tessera: dict[int, int] = {}
    for vd in analysed.values():
        for row in (*vd.rows_before, *vd.rows_after):
            code_of_fx[row.fx] = row.code
            fx_of_tessera[row.tessera_id] = row.fx

    # The global sets come from the unfiltered viewports, which must agree exactly — they all
    # cover the full extent (module doc), so the same items appeared to each of them.
    unfiltered = [q for q in viewports if q.filters is None]
    appeared = frozenset(
        fx for q in unfiltered for fx in (r.fx for r in analysed[q].added)
    )
    vanished = frozenset(
        fx for q in unfiltered for fx in (r.fx for r in analysed[q].removed)
    )
    for q in unfiltered:
        got_a = frozenset(r.fx for r in analysed[q].added)
        got_v = frozenset(r.fx for r in analysed[q].removed)
        if got_a != appeared or got_v != vanished:
            reasons.append(
                f"unfiltered viewports disagree about what changed: zoom {q.zoom} saw "
                f"appeared={sorted(got_a)[:4]} vanished={sorted(got_v)[:4]} against the union "
                f"appeared={sorted(appeared)[:4]} vanished={sorted(vanished)[:4]}"
            )
    for q in viewports:
        if q.filters is None:
            continue
        got_a = frozenset(r.fx for r in analysed[q].added)
        got_v = frozenset(r.fx for r in analysed[q].removed)
        if not (got_a <= appeared and got_v <= vanished):
            reasons.append(
                f"the filtered viewport served a change no unfiltered viewport saw: "
                f"appeared={sorted(got_a - appeared)[:4]} vanished={sorted(got_v - vanished)[:4]}"
            )

    for q in viewports:
        _check_viewport_counts(
            q, before[q], after[q], analysed[q], appeared, vanished, code_of_fx, reasons
        )

    for q in changed:
        if isinstance(q, (Meta, Categories)):
            surface = "/v1/meta" if isinstance(q, Meta) else f"/v1/categories/{q.column}"
            reasons.append(
                f"{surface} changed — no stage in this suite's plans is entitled to it"
            )

    # Drill-down flips must be exactly the appeared/vanished items, in both directions: a flip
    # nothing explains, and an item that should have flipped and did not.
    for q in before:
        if not isinstance(q, Item):
            continue
        payload_before = before[q].payload if isinstance(before[q], Json) else None
        payload_after = after[q].payload if isinstance(after[q], Json) else None
        fx = fx_of_tessera.get(q.tessera_id)
        if payload_before == payload_after:
            if fx in vanished and payload_before and payload_before["status"] == 200:
                reasons.append(
                    f"item {q.tessera_id} (fx {fx:#x}) vanished from the viewports but its "
                    f"drill-down still answers 200"
                )
            if fx in appeared and payload_before and payload_before["status"] == 404:
                reasons.append(
                    f"item {q.tessera_id} (fx {fx:#x}) appeared in the viewports but its "
                    f"drill-down still answers 404"
                )
            continue
        status_before = payload_before["status"] if payload_before else None
        status_after = payload_after["status"] if payload_after else None
        if status_before == 200 and status_after == 404:
            if fx not in vanished:
                reasons.append(
                    f"item {q.tessera_id} flipped 200 -> 404 but no viewport lost it "
                    f"(fx {fx if fx is None else hex(fx)})"
                )
        elif status_before == 404 and status_after == 200:
            if fx not in appeared:
                reasons.append(
                    f"item {q.tessera_id} flipped 404 -> 200 but no viewport gained it"
                )
        else:
            reasons.append(
                f"item {q.tessera_id} changed its body without changing status "
                f"({status_before} -> {status_after}) — a record field moved under a stable item"
            )

    if reasons:
        return Unexplained(tuple(reasons))
    if not appeared and not vanished:
        return Unexplained(
            (
                "recordings differ but no served row appeared or vanished — the change is not "
                "expressible as an entitlement",
            )
        )
    return Delta(appeared, vanished)


__all__ = [
    "Delta",
    "Entity",
    "FX_COLUMN",
    "Nothing",
    "Rows",
    "Unexplained",
    "diff",
]
