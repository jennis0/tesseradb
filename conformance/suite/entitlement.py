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

**Saturation is decided per tile, from the response itself — never by configuration.** A point-set
comparison is sound only over a tile whose served set is complete: under a truncated selection,
removing a served row admits the next-priority row behind it, so rows move on the wire that never
moved in the corpus — displacement, not change (§10). No configuration arranges completeness. The
suite once pinned θ and the mark caps "above any fixture total", which is true at fixture size and
false at ten million items, where the whole-extent tile holds ten million visible rows against a
million-mark budget. But every tile's own row in the tiles batch already says whether truncation
happened: ``served == matched`` means it did not — `matched` is the selection's pool, and on an
unfiltered viewport it equals `visible` (`tessera-engine`'s viewport module states the pair) — and
``served < matched`` means it did. The diff therefore:

- takes **membership evidence** only from untruncated tiles (and from drill-down flips, which no
  selection truncates): a row added or removed there is a genuine appear or vanish, and every
  untruncated tile covering an evidenced item must agree with the evidence;
- treats a row entering or leaving a **capped window** as displacement — unattributable, and
  deliberately not a defect — unless an untruncated tile elsewhere contradicts it;
- holds every tile, capped or not, to the **counts** the cap cannot touch: `served` must track
  this query's own point rows everywhere; `visible` and `matched` are mask arithmetic,
  independent of the cap, checked per tile wherever the evidence over that tile's region is
  complete and summed over the full extent always — the sums must agree across viewports, with
  the underlay, and (through [`CappedDelta`]'s equality) with the entitlement's own net movement;
- **refuses to go soft**: when every non-empty tile of every unfiltered viewport is capped, no
  point-set entitlement is checkable at all, and the result is [`Uncheckable`] — equal to no
  entitlement — rather than a count comparison quietly presented as the full check. In a capped
  region counts can be compensated by construction (a dropped row and a leaked row sum to zero),
  which is why the battery carries a viewport deep enough to hold saturated tiles
  (`suite.battery`'s deep viewport) and why its absence must be loud rather than absorbed.

At fixture size every tile is untruncated, the evidence is the corpus's whole delta, and the
result is an exact [`Delta`]. At scale the result is a [`CappedDelta`] — membership where it was
observable, the net count everywhere — whose equality against a declared [`Delta`] is
subset-plus-net rather than set equality, and which never equals ``Nothing()``: bytes-equal is
the only way to have changed nothing.

**What the diff refuses to launder.** ``Nothing`` is bytes-equal on every canonical surface —
never "the row sets agree". A byte change that moves no row (an order change, a re-encoding, a
count surface disagreeing with the points that back it) is returned as [`Unexplained`], which
equals no entitlement, because served order and the count/points agreement *are* contract
(contracts §3.2) and a diff that shrugged at them would pass the corruption classes §8.1 lists —
the mis-interleave that preserves counts, the count that moves without a row. Concretely, a
changed recording must decompose exactly:

- the points surfaces' added/removed rows, order-preserved in the residual (removing the vanished
  rows from *before* and the appeared rows from *after* must leave identical sequences);
- per-tile count deltas consistent with those rows: ``served`` moving with the rows this query
  itself served, in every tile; ``matched`` moving with ``visible`` on unfiltered viewports and
  with the served rows in untruncated filtered tiles; ``visible`` moving with the evidenced
  items wherever the tile's evidence is complete, its full-extent sum agreeing everywhere;
- underlay cell deltas equal to the evidenced items' cells at ``zoom + underlay_offset`` wherever
  the covering tile's evidence is complete, the cell sum agreeing with the tiles' sum;
- the trailer equal but for ``points`` (which must track the served delta) and ``flushes`` (a
  deterministic function of served bytes, so a stage entitled to change content is entitled to
  move it — but it must not move when the served bytes did not);
- ``/v1/items`` flips exactly at the appeared/vanished items, 404 being a real answer on that
  surface;
- ``/v1/meta`` and ``/v1/categories`` unchanged — no stage in this suite's plans is entitled to
  either, because the plans ingest only established vocabulary values and deny only items whose
  category values have other visible holders (a vocabulary-extending flush is real and out of
  this suite's scope).

**One precondition inherited from §10, stated here because the diff silently depends on it:**
every battery viewport covers the full extent, so each one's `visible` column sums to the same
composed total and an evidenced item lies inside every viewport's tiling. Saturation is *not* a
precondition — it is observed per tile, above.
"""

from __future__ import annotations

import json
from collections import defaultdict
from dataclasses import dataclass
from typing import Iterable, NamedTuple

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.ipc as ipc

from .battery import Categories, Item, Meta, Recorded, Viewport
from .canonical import Json, Streamed

#: The planted join column — the item identity (correctness-suite §12.1). The one column name
#: this module hard-codes, deliberately: the suite's corpus plants it under exactly this name so
#: that no second identity is minted beside it.
FX_COLUMN = "fx_key"


def _show_set(s: frozenset[int]) -> str:
    if not s:
        return "∅"
    sample = ", ".join(f"{v:#x}" for v in sorted(s)[:3])
    return f"{{{sample}{', …' if len(s) > 3 else ''}}} ({len(s)})"


@dataclass(frozen=True)
class Delta:
    """What a stage changed, in item identities: the sets that appeared and vanished.

    ``Delta(∅, ∅)`` — [`Nothing`] — is the strongest assertion in the suite, and [`diff`] returns
    it only for bytes-equal recordings (module doc). [`diff`] returns this type at all only when
    every unfiltered viewport's tiles were untruncated in both recordings, so the sets are the
    corpus's whole delta and equality against a declared entitlement is exact.
    """

    appeared: frozenset[int]
    vanished: frozenset[int]

    def __repr__(self) -> str:  # compact in assertion messages
        return f"Delta(appeared={_show_set(self.appeared)}, vanished={_show_set(self.vanished)})"


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


@dataclass(frozen=True, eq=False)
class CappedDelta:
    """A changed recording pair in which some tiles were capped: membership evidence where the
    tiles were complete, the net count everywhere (module doc).

    ``appeared``/``vanished`` are the *evidenced* sets — items whose movement an untruncated tile
    or a drill-down flip proved — and ``net_visible`` is the full-extent sum of every `visible`
    delta, which every viewport and the underlay were already held to agree on. Equality against
    a declared [`Delta`] is therefore subset-plus-net: nothing unentitled moved where membership
    was observable, and the counts account for every entitled item, everywhere. It never equals
    ``Nothing()`` — a recording that changed at all is not "changed nothing", and displacement
    under a stage entitled to no corpus movement is a defect the selection's determinism forbids.

    The displacement tallies and tile counts are diagnostic, carried so an assertion message
    shows how much of the comparison was membership and how much fell back to counts.
    """

    appeared: frozenset[int]
    vanished: frozenset[int]
    net_visible: int
    displaced_in: int
    displaced_out: int
    truncated_tiles: int
    comparable_tiles: int

    def __eq__(self, other) -> bool:
        if isinstance(other, CappedDelta):
            return (self.appeared, self.vanished, self.net_visible) == (
                other.appeared,
                other.vanished,
                other.net_visible,
            )
        if isinstance(other, Delta):
            if not other.appeared and not other.vanished:
                return False  # Nothing() means bytes-equal, and these recordings differ
            return (
                self.appeared <= other.appeared
                and self.vanished <= other.vanished
                and self.net_visible == len(other.appeared) - len(other.vanished)
            )
        return NotImplemented

    def __repr__(self) -> str:
        return (
            f"CappedDelta(appeared={_show_set(self.appeared)}, "
            f"vanished={_show_set(self.vanished)}, net visible {self.net_visible:+d}; "
            f"{self.displaced_in} row(s) displaced into / {self.displaced_out} out of "
            f"{self.truncated_tiles} capped tile(s), membership compared over "
            f"{self.comparable_tiles})"
        )


@dataclass(frozen=True)
class Uncheckable:
    """Every non-empty tile of every unfiltered viewport was capped: the point-set half of any
    entitlement is uncheckable, and this result says so instead of quietly passing on counts.

    Equal to no [`Delta`], so ``diff(...) == entitlement`` fails for every declared entitlement —
    a check that silently stopped checking would be worse than one that fails (module doc). The
    remedy is a battery surface, not a bigger cap: deepen the battery until some tiles saturate
    (`suite.battery`'s deep viewport carries the arithmetic).
    """

    reasons: tuple[str, ...]

    def __repr__(self) -> str:
        shown = "\n  - ".join(self.reasons)
        return f"Uncheckable(\n  - {shown}\n)"


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


def _points_table(canon: Streamed, label: str, reasons: list[str]) -> pa.Table | None:
    """The served points as Arrow, or `None` for an empty surface or one that cannot be
    attributed. The schema check lives here so both routes below share it and a malformed
    surface is reported exactly once per recording."""
    table = _tables(canon.points)
    if table is None:
        return None
    for required in ("tessera_id", "code", FX_COLUMN):
        if required not in table.schema.names:
            reasons.append(f"{label}: points schema lacks `{required}` — cannot attribute rows")
            return None
    return table


def _rows_at(table: pa.Table, positions: np.ndarray) -> list[_Row]:
    """Decode the named rows to Python.

    **Everything expensive in this module is a `to_pylist` on a served surface**, and this is the
    only one left on the hot path — which is why it takes positions rather than a table. A
    recording holds every point a viewport served; a stage changes a few thousand of them. Cost
    proportional to the change rather than to the recording is the whole point of the vectorised
    route in [`_split_rows`]. Measured on the endurance corpus: 0.59 s per 200,000 points across
    seven columns, and a deep-zoom recording carries an order of magnitude more than that.
    """
    if len(positions) == 0:
        return []
    taken = table.take(pa.array(positions, pa.int64()))
    names = taken.schema.names
    columns = [taken.column(name).to_pylist() for name in names]
    ti, ci, fi = names.index("tessera_id"), names.index("code"), names.index(FX_COLUMN)
    return [
        _Row(key=values, tessera_id=values[ti], code=values[ci], fx=values[fi])
        for values in zip(*columns)
    ]


def _point_rows(canon: Streamed, label: str, reasons: list[str]) -> list[_Row]:
    """Every served point, decoded. The reference route: obviously the tuple semantics the diff
    is specified in, and the fallback whenever [`_split_rows`] declines."""
    table = _points_table(canon, label, reasons)
    if table is None:
        return []
    return _rows_at(table, np.arange(table.num_rows))


def _column_equal(before: pa.ChunkedArray, after: pa.ChunkedArray) -> np.ndarray:
    """Elementwise equality with **Python's** null semantics, which is what the tuple route means
    by equal: two nulls are equal, a null and a value are not. `pyarrow.compute.equal` yields
    *null* when either side is null, and a null read as False would report every null-bearing row
    as changed — a whole surface of spurious added/removed pairs."""
    both_null = pc.and_(pc.is_null(before), pc.is_null(after))
    equal = pc.fill_null(pc.equal(before, after), False)
    return pc.or_(equal, both_null).to_numpy(zero_copy_only=False)


def _split_rows(tb: pa.Table, ta: pa.Table) -> tuple[np.ndarray, np.ndarray, bool] | None:
    """`(removed positions in tb, added positions in ta, residual order held)` — the tuple route's
    answer, computed without decoding either recording.

    A row is *removed* when its full column tuple is absent from the other side, which — given a
    unique served identity — is exactly "no row there carries this `tessera_id`, or one does and
    some column differs". Both halves are set operations over the identity column and a
    columnwise comparison at the matched positions, so nothing is decoded but the rows that
    actually moved.

    **Returns `None` rather than an answer when `tessera_id` is not unique in either recording**,
    because the identity join above is then not the tuple semantics: two rows sharing an identity
    would match one position and hide the other. The contract makes the served identity unique
    per recording, so this is a guard against a defect, not a supported shape — and declining
    into the reference route means such a defect is still *caught*, just slowly, rather than
    silently mis-attributed.
    """
    if ta.schema.names != tb.schema.names:
        return None
    identity = (tb.column("tessera_id"), ta.column("tessera_id"))
    # An identity that is not a non-null integer cannot key the join: a null decodes to NaN and
    # compares unequal to itself, and a non-integer decodes to an object array whose sortedness
    # says nothing. Both are contract violations rather than shapes to support, so decline into
    # the reference route, which still answers.
    if not all(pa.types.is_integer(c.type) and c.null_count == 0 for c in identity):
        return None
    ids_b = identity[0].combine_chunks().to_numpy(zero_copy_only=False)
    ids_a = identity[1].combine_chunks().to_numpy(zero_copy_only=False)
    order_b, order_a = np.argsort(ids_b, kind="stable"), np.argsort(ids_a, kind="stable")
    sorted_b, sorted_a = ids_b[order_b], ids_a[order_a]
    for run in (sorted_b, sorted_a):
        if run.size > 1 and (np.diff(run) == 0).any():
            return None

    hit_b, hit_a = np.isin(ids_b, ids_a), np.isin(ids_a, ids_b)
    pos_b = np.flatnonzero(hit_b)
    pos_a = order_a[np.searchsorted(sorted_a, ids_b[pos_b])]

    same = np.ones(pos_b.size, dtype=bool)
    if pos_b.size:
        sub_b = tb.take(pa.array(pos_b, pa.int64()))
        sub_a = ta.take(pa.array(pos_a, pa.int64()))
        for name in tb.schema.names:
            same &= _column_equal(sub_b.column(name), sub_a.column(name))

    removed = np.sort(np.concatenate([np.flatnonzero(~hit_b), pos_b[~same]]))
    added = np.sort(np.concatenate([np.flatnonzero(~hit_a), pos_a[~same]]))
    # The surviving rows must appear in the same order on both sides; their contents are already
    # known equal, so the identity sequence carries the whole comparison.
    residual_ok = bool(np.array_equal(ids_b[pos_b[same]], ids_a[np.sort(pos_a[same])]))
    return removed, added, residual_ok


def _resolve_ids(analysed: dict, wanted: set[int]) -> list[_Row]:
    """The served rows carrying the given identities, taken from wherever a viewport served them.

    Only the battery's drill-down items need this. Every other identity the diff looks up belongs
    to a row that *changed*, and those are decoded already — but a drill-down item is normally
    the one thing that did not change, so its identity would otherwise be unresolvable now that
    unchanged rows are never decoded.
    """
    if not wanted:
        return []
    out: list[_Row] = []
    want = pa.array(sorted(wanted), pa.uint64())
    for vd in analysed.values():
        for table in vd.tables:
            if table is None:
                continue
            column = table.column("tessera_id")
            mask = pc.is_in(column, value_set=want.cast(column.type))
            out.extend(_rows_at(table, np.flatnonzero(mask.to_numpy(zero_copy_only=False))))
    return out


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


def _truncated(tiles: dict[int, tuple[int, int, int]]) -> frozenset[int]:
    """The tiles this recording's selection truncated: ``served < matched``. `matched` rather
    than `visible` because it is the selection's own pool — on an unfiltered viewport the two are
    equal (module doc), and on a filtered one a complete *matched* window is exactly what makes
    the tile's point set comparable."""
    return frozenset(t for t, (_v, m, s) in tiles.items() if s < m)


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


def _label(query: Viewport) -> str:
    return (
        f"viewport(zoom={query.zoom}, filters={'yes' if query.filters else 'no'}, "
        f"form={'tiles' if query.tiles else 'bbox'})"
    )


@dataclass
class _VpDiff:
    added: list[_Row]
    removed: list[_Row]
    #: The two recordings, kept as Arrow for [`_resolve_ids`]. Unchanged rows are never decoded,
    #: so a later lookup of an *unchanged* identity has to come back here for it.
    tables: tuple[pa.Table | None, pa.Table | None]


def _rows_by_tuple(
    rows_before: list[_Row], rows_after: list[_Row]
) -> tuple[list[_Row], list[_Row], bool]:
    """The diff's semantics, stated directly over decoded rows: a row belongs to the change when
    its full column tuple is absent from the other side. [`_split_rows`] computes this without
    decoding; this is what it must agree with, and the route taken when it declines."""
    before_keys = {r.key for r in rows_before}
    after_keys = {r.key for r in rows_after}
    removed = [r for r in rows_before if r.key not in after_keys]
    added = [r for r in rows_after if r.key not in before_keys]
    residual_before = [r.key for r in rows_before if r.key in after_keys]
    residual_after = [r.key for r in rows_after if r.key in before_keys]
    return removed, added, residual_before == residual_after


def _analyse_viewport(
    query: Viewport, before: Streamed, after: Streamed, reasons: list[str]
) -> _VpDiff:
    label = _label(query)
    tb = _points_table(before, label, reasons)
    ta = _points_table(after, label, reasons)

    if before.points == after.points:
        # Identical bytes are identical rows, and the surfaces here run to millions of points:
        # not decoding them is worth the special case.
        return _VpDiff([], [], (tb, ta))

    split = None if tb is None or ta is None else _split_rows(tb, ta)
    if split is not None:
        removed_pos, added_pos, residual_ok = split
        removed, added = _rows_at(tb, removed_pos), _rows_at(ta, added_pos)
    else:
        rows_before = [] if tb is None else _rows_at(tb, np.arange(tb.num_rows))
        rows_after = [] if ta is None else _rows_at(ta, np.arange(ta.num_rows))
        removed, added, residual_ok = _rows_by_tuple(rows_before, rows_after)

    # The residual must be identical *in order*: contracts §3.2 orders points ascending by
    # `tessera_id` within each tile, so an unexplained reordering is a defect, never noise.
    if not residual_ok:
        reasons.append(
            f"{label}: beyond the {len(added)} added / {len(removed)} removed rows, the "
            f"surviving rows changed order or content — a permutation-preserving surface would "
            f"have hidden this"
        )
    if not added and not removed and residual_ok:
        reasons.append(
            f"{label}: points bytes differ with no row difference — encoding or chunking drift, "
            f"which §12.2 step 4's content-determinism assumption says must not happen"
        )
    return _VpDiff(added, removed, (tb, ta))


def _check_viewport_counts(
    query: Viewport,
    before: Streamed,
    after: Streamed,
    vd: _VpDiff,
    placed_appeared: dict[int, int],
    placed_vanished: dict[int, int],
    truncated: frozenset[int],
    visible_truncated: frozenset[int] | None,
    reasons: list[str],
) -> int:
    """One viewport's count surfaces against the evidence, per the module doc's rules; returns
    the full-extent sum of the `visible` deltas — the net corpus movement this viewport reports,
    which [`diff`] holds equal across every viewport.

    ``placed_appeared``/``placed_vanished`` are the evidenced items that have a served code (a
    flip-evidenced item nothing served has no position and can join no per-tile expectation — it
    still counts through the measured net). ``truncated`` is this viewport's own truncated-tile
    set over the pair; ``visible_truncated`` is the truncation set governing `visible`
    attribution — the viewport's own for an unfiltered query, the same-zoom unfiltered
    viewport's for a filtered one (its own truncation says nothing about *visible*
    completeness), or `None` when no such viewport exists and only the sums bind.
    """
    label = _label(query)

    tiles_before = _tile_map(before)
    tiles_after = _tile_map(after)

    # This query's own served movement per tile — what `served` (and a complete filtered tile's
    # `matched`) must track.
    own: dict[int, int] = defaultdict(int)
    for row in vd.added:
        own[_prefix_of(row.code, query.zoom)] += 1
    for row in vd.removed:
        own[_prefix_of(row.code, query.zoom)] -= 1

    # The evidenced corpus movement per tile — what `visible` must track where attribution is
    # complete. Evidenced items are mask-visible by construction (they were, or became, served —
    # or their drill-down answered 200).
    placed: dict[int, int] = defaultdict(int)
    for code in placed_appeared.values():
        placed[_prefix_of(code, query.zoom)] += 1
    for code in placed_vanished.values():
        placed[_prefix_of(code, query.zoom)] -= 1

    net = 0
    for tile in sorted(set(tiles_before) | set(tiles_after) | set(placed) | set(own)):
        b = tiles_before.get(tile, (0, 0, 0))
        a = tiles_after.get(tile, (0, 0, 0))
        dv, dm, ds = a[0] - b[0], a[1] - b[1], a[2] - b[2]
        net += dv
        if ds != own[tile]:
            reasons.append(
                f"{label}: tile {tile} moved `served` by {ds:+d} where its own point rows "
                f"moved {own[tile]:+d} — the count surface disagrees with the rows that back it"
            )
        if query.filters is None:
            if dm != dv:
                reasons.append(
                    f"{label}: tile {tile} moved `matched` by {dm:+d} against `visible` "
                    f"{dv:+d} — on an unfiltered viewport the two are one quantity"
                )
        elif tile not in truncated and dm != own[tile]:
            reasons.append(
                f"{label}: tile {tile} was not truncated, yet `matched` moved {dm:+d} where "
                f"its complete served window moved {own[tile]:+d}"
            )
        if visible_truncated is not None and tile not in visible_truncated and dv != placed[tile]:
            reasons.append(
                f"{label}: tile {tile} moved `visible` by {dv:+d}, but the evidenced items say "
                f"{placed[tile]:+d}"
            )
    if before.tiles != after.tiles and tiles_before == tiles_after:
        reasons.append(f"{label}: tiles bytes differ without a value difference")

    # The underlay counts visible items per cell at depth `zoom + offset` (§3.3): per cell where
    # the covering tile's evidence is complete, and in sum always — two counts of one visible set.
    if query.underlay_offset:
        depth = query.zoom + query.underlay_offset
        cell_placed: dict[int, int] = defaultdict(int)
        for code in placed_appeared.values():
            cell_placed[_prefix_of(code, depth)] += 1
        for code in placed_vanished.values():
            cell_placed[_prefix_of(code, depth)] -= 1
        cells_before = _cell_map(before)
        cells_after = _cell_map(after)
        cell_net = 0
        for cell in sorted(set(cells_before) | set(cells_after) | set(cell_placed)):
            actual_delta = cells_after.get(cell, 0) - cells_before.get(cell, 0)
            cell_net += actual_delta
            covering = cell >> (2 * query.underlay_offset)
            if (
                visible_truncated is not None
                and covering not in visible_truncated
                and actual_delta != cell_placed[cell]
            ):
                reasons.append(
                    f"{label}: underlay cell {cell} moved by {actual_delta:+d}, expected "
                    f"{cell_placed[cell]:+d}"
                )
        if cell_net != net:
            reasons.append(
                f"{label}: the underlay's cells moved {cell_net:+d} in total where the tiles' "
                f"`visible` moved {net:+d} — two counts of one visible set disagree"
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
    return net


def diff(before: Recorded, after: Recorded) -> Delta | CappedDelta | Uncheckable | Unexplained:
    """Reduce two recordings of one battery to what changed between them (module doc).

    Returns [`Nothing`]'s delta only when every surface is bytes-equal; an exact [`Delta`] when
    every unfiltered viewport's tiles were untruncated in both recordings — fixture size — and
    every changed byte is attributable to the appeared/vanished sets; a [`CappedDelta`] when some
    tiles were capped but a membership surface remains; [`Uncheckable`] when none does; and
    [`Unexplained`] whenever a change is attributable to no entitlement at all.
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

    # Per-viewport truncation over the pair: a tile admits membership claims only when neither
    # recording truncated it — a set comparison needs both sides complete.
    trunc: dict[Viewport, frozenset[int]] = {}
    nonempty: dict[Viewport, set[int]] = {}
    for q in viewports:
        tiles_b, tiles_a = _tile_map(before[q]), _tile_map(after[q])
        trunc[q] = _truncated(tiles_b) | _truncated(tiles_a)
        nonempty[q] = set(tiles_b) | set(tiles_a)

    # Item identity is carried by every served row; both sides of every viewport contribute, so a
    # row that exists only before (a vanished item) still names itself. Only two kinds of identity
    # are ever looked up here — one that moved, and a drill-down item's — so the map is built from
    # the rows that moved plus a targeted lookup for the battery's items, rather than from every
    # served row. The distinction is not academic at endurance scale: the full form decodes
    # millions of rows to answer a few hundred questions.
    code_of_fx: dict[int, int] = {}
    fx_of_tessera: dict[int, int] = {}
    for vd in analysed.values():
        for row in (*vd.removed, *vd.added):
            code_of_fx[row.fx] = row.code
            fx_of_tessera[row.tessera_id] = row.fx
    for row in _resolve_ids(analysed, {q.tessera_id for q in before if isinstance(q, Item)}):
        # A moved row's own code wins: an item that both moved and is drilled into is described
        # by the recording that changed, which is what the full form's before-then-after order
        # also yielded.
        code_of_fx.setdefault(row.fx, row.code)
        fx_of_tessera.setdefault(row.tessera_id, row.fx)

    # -- membership evidence (module doc): untruncated tiles of every viewport — an unfiltered
    # one's complete tile is the visible set, a filtered one's the matched set, and a matched
    # appear/vanish is the row's own because no stage edits a row in place (decision 0047: edit
    # is delete + re-ingest) — plus drill-down flips, which no selection truncates.
    appeared: set[int] = set()
    vanished: set[int] = set()
    for q in viewports:
        for row in analysed[q].added:
            if _prefix_of(row.code, q.zoom) not in trunc[q]:
                appeared.add(row.fx)
        for row in analysed[q].removed:
            if _prefix_of(row.code, q.zoom) not in trunc[q]:
                vanished.add(row.fx)

    item_payloads: dict[Item, tuple[dict | None, dict | None]] = {}
    for q in before:
        if not isinstance(q, Item):
            continue
        payload_before = before[q].payload if isinstance(before[q], Json) else None
        payload_after = after[q].payload if isinstance(after[q], Json) else None
        item_payloads[q] = (payload_before, payload_after)
        status_before = payload_before["status"] if payload_before else None
        status_after = payload_after["status"] if payload_after else None
        if (status_before, status_after) in ((200, 404), (404, 200)):
            fx = fx_of_tessera.get(q.tessera_id)
            if fx is None:
                reasons.append(
                    f"item {q.tessera_id} flipped {status_before} -> {status_after} but no "
                    f"recorded viewport ever served it — the flip cannot be attributed to any "
                    f"item identity"
                )
            elif status_after == 404:
                vanished.add(fx)
            else:
                appeared.add(fx)

    # -- displacement, and the contradictions it may not hide: a row entering or leaving a capped
    # window is unattributable and not a defect — unless complete evidence elsewhere says the
    # item moved the other way, in which case two surfaces disagree about the corpus.
    displaced_in = displaced_out = 0
    for q in viewports:
        label = _label(q)
        for row in analysed[q].added:
            if _prefix_of(row.code, q.zoom) in trunc[q]:
                if row.fx in vanished:
                    reasons.append(
                        f"{label}: item {row.fx:#x} entered a capped window while a complete "
                        f"tile elsewhere shows it vanished"
                    )
                elif row.fx not in appeared:
                    displaced_in += 1
        for row in analysed[q].removed:
            if _prefix_of(row.code, q.zoom) in trunc[q]:
                if row.fx in appeared:
                    reasons.append(
                        f"{label}: item {row.fx:#x} left a capped window while a complete tile "
                        f"elsewhere shows it appeared"
                    )
                elif row.fx not in vanished:
                    displaced_out += 1

    # -- cross-viewport consistency: every unfiltered viewport whose covering tile of an
    # evidenced item is untruncated must show the same movement — its point set over that tile
    # is complete, so silence there contradicts the evidence. (At fixture size, where nothing is
    # truncated, this is exactly "the unfiltered viewports must agree". A *filtered* viewport is
    # never required to show an item — the item may simply not match its filter.)
    unfiltered = [q for q in viewports if q.filters is None]
    for q in unfiltered:
        label = _label(q)
        added_fx = {r.fx for r in analysed[q].added}
        removed_fx = {r.fx for r in analysed[q].removed}
        for fx in sorted(appeared):
            code = code_of_fx.get(fx)
            if code is None:
                continue
            if _prefix_of(code, q.zoom) not in trunc[q] and fx not in added_fx:
                reasons.append(
                    f"{label}: item {fx:#x} appeared per complete evidence elsewhere, but this "
                    f"viewport's covering tile is complete and does not serve it as new"
                )
        for fx in sorted(vanished):
            code = code_of_fx.get(fx)
            if code is None:
                continue
            if _prefix_of(code, q.zoom) not in trunc[q] and fx not in removed_fx:
                reasons.append(
                    f"{label}: item {fx:#x} vanished per complete evidence elsewhere, but this "
                    f"viewport's covering tile is complete and still serves it"
                )

    # -- counts, per viewport, and the net every full-extent surface must agree on.
    placed_appeared = {fx: code_of_fx[fx] for fx in appeared if fx in code_of_fx}
    placed_vanished = {fx: code_of_fx[fx] for fx in vanished if fx in code_of_fx}
    unfiltered_trunc_at = {q.zoom: trunc[q] for q in unfiltered}
    nets: dict[Viewport, int] = {}
    for q in viewports:
        visible_truncated = (
            trunc[q] if q.filters is None else unfiltered_trunc_at.get(q.zoom)
        )
        nets[q] = _check_viewport_counts(
            q,
            before[q],
            after[q],
            analysed[q],
            placed_appeared,
            placed_vanished,
            trunc[q],
            visible_truncated,
            reasons,
        )
    if len(set(nets.values())) > 1:
        summary = ", ".join(f"{_label(q)}: {n:+d}" for q, n in nets.items())
        reasons.append(
            f"the viewports disagree on the corpus's net visible movement — every one covers "
            f"the full extent, so their `visible` sums must move together ({summary})"
        )

    for q in changed:
        if isinstance(q, (Meta, Categories)):
            surface = "/v1/meta" if isinstance(q, Meta) else f"/v1/categories/{q.column}"
            reasons.append(
                f"{surface} changed — no stage in this suite's plans is entitled to it"
            )

    # Drill-down consistency in both directions: a stable item may not sit in an evidenced set,
    # a flipped item may not contradict one, and a body may not move under a stable status.
    for q, (payload_before, payload_after) in item_payloads.items():
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
            if fx in appeared:
                reasons.append(
                    f"item {q.tessera_id} (fx {fx:#x}) flipped 200 -> 404 while complete "
                    f"evidence shows it appearing"
                )
        elif status_before == 404 and status_after == 200:
            if fx in vanished:
                reasons.append(
                    f"item {q.tessera_id} (fx {fx:#x}) flipped 404 -> 200 while complete "
                    f"evidence shows it vanishing"
                )
        else:
            reasons.append(
                f"item {q.tessera_id} changed its body without changing status "
                f"({status_before} -> {status_after}) — a record field moved under a stable item"
            )

    truncation_seen = any(trunc[q] for q in unfiltered)
    comparable_surface = any(nonempty[q] - trunc[q] for q in unfiltered)
    blind_statement = (
        "every non-empty tile in every unfiltered viewport was capped (served < visible) in at "
        "least one recording — no point-set entitlement is checkable over these recordings, and "
        "counts alone can be compensated; the battery needs a viewport deep enough to hold "
        "saturated tiles (suite.battery's deep viewport)"
    )
    if reasons:
        if truncation_seen and not comparable_surface:
            reasons.append(blind_statement)
        return Unexplained(tuple(reasons))
    if not truncation_seen:
        if not appeared and not vanished:
            return Unexplained(
                (
                    "recordings differ but no served row appeared or vanished — the change is "
                    "not expressible as an entitlement",
                )
            )
        return Delta(frozenset(appeared), frozenset(vanished))
    if not comparable_surface:
        return Uncheckable((blind_statement,))
    net = next(iter(nets.values())) if nets else 0
    return CappedDelta(
        appeared=frozenset(appeared),
        vanished=frozenset(vanished),
        net_visible=net,
        displaced_in=displaced_in,
        displaced_out=displaced_out,
        truncated_tiles=sum(len(trunc[q]) for q in viewports),
        comparable_tiles=sum(len(nonempty[q] - trunc[q]) for q in viewports),
    )


__all__ = [
    "CappedDelta",
    "Delta",
    "Entity",
    "FX_COLUMN",
    "Nothing",
    "Rows",
    "Uncheckable",
    "Unexplained",
    "diff",
]
