"""Decode the framed `/v1/viewport` body `tessera_wire::payload` builds (contracts §3.2 r26,
`streamed-serving.md`): a sequence of frames, each `u8 kind` + `u32 LE payload length` + payload,
every payload a complete Arrow IPC stream (JSON for the trailer):

    kind 1  tiles      (tile, visible, matched, served,      exactly one, first
                        highlighted)
    kind 2  sub-cells  (cell, count)                          exactly one, iff underlay requested
    kind 3  points     (tessera_id, code, ...scalars)         zero or more; concatenate in order
    kind 4  trailer    JSON                                   exactly one, last
    kind 5  artifacts  (layer dict<u16,utf8>, tessera_id,  at most one, after tiles and before
                        key, masked_count, the derived           any points; absent when none served
                        geometry, content, parent_ids,
                        rung, matched, highlighted — then
                        shape_x/shape_y,
                        in the schema only when a served
                        row carries a drawn geometry; §3.2 r45)

Mirrors `crates/tessera-server/tests/common/mod.rs`'s `decode_viewport_frames` byte-for-byte,
independently implemented in Python (this is the client-side decode any real SDK would need, not
shared Rust logic), so `conformance/tests` can reuse it without copy-paste;
`reference/tests/wire.py` re-exports this module's `decode_viewport` unchanged.

Strictness is deliberate: a truncated body, an unknown kind, a misplaced tiles frame or a missing
trailer raises — a truncated stream must never decode as a plausible shorter response. The
trailer's presence is the completeness signal, and its key set is closed (asserted here), so the
one server-authored JSON region of the body cannot quietly acquire a field the conformance
comparator never sees.
"""

from __future__ import annotations

import io
import json
import struct
from typing import NamedTuple

import pyarrow.ipc as ipc


class Artifact(NamedTuple):
    """One served artifact, as the kind-5 frame carries it.

    Every field here is a fact about *this principal's* view of the artifact, and none is a fact
    about the artifact: `masked_count` is what they can see, and the geometry describes the members
    they can see. Two principals disagreeing about one `tessera_id` is correct, and a comparator
    that asserted agreement across principals would be asserting the bug.
    """

    layer: str
    tessera_id: int
    key: str | None
    masked_count: int
    centroid: tuple[float, float] | None
    box: tuple[int, int, int, int] | None
    #: The artifact's one drawn geometry, of the kind its layer declared (contracts §3.2 r45,
    #: `polygon-membership.md` §7.1): parts, then rings, then `(x, y)` vertices. A part's first
    #: ring is its outer and the rest are holes; two parts are two shapes, never a shape with a
    #: gap. A derived hull is one part per α-group with no holes, so a membership that is several
    #: separated clouds is several parts. The wire's `shape_x`/`shape_y` are
    #: `list<list<list<uint32>>>`, one column per axis, and the two agree at every level.
    shape: list[list[list[tuple[int, int]]]] | None
    #: One content, entire, positional to the layer's declared kinds. Empty means the layer
    #: declares no supplied content — never that content was withheld.
    content: list[str]
    #: The rung this artifact is drawn at (contracts §3.2 r44): the declared level on a levelled
    #: layer — a fact about the artifact, so two principals served it *do* agree on it — and the
    #: response-local depth on a treed one, the longest parent chain to this row in the forest the
    #: response's own `parent_ids` links form after the budget cut. `0` on a flat layer.
    rung: int
    #: The identifiers of this artifact's parents **that are in this same response**, ascending
    #: (contracts §3.2 r71, `dag-hierarchies.md` §7): at most one on a tree, several on a `dag`
    #: layer. Empty for a root, for a flat artifact, and for a parent the response withheld alike
    #: — the wire does not distinguish them (C29, per entry), and neither may a reader.
    parent_ids: list[int]
    #: Whether a member this principal may see, inside the request's tiles, matches the request's
    #: `filters` ([decision 0104](../../docs/decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md)).
    #: `None` — a null on the wire — where the request carried none: *there was no question*,
    #: never *no matches*. A boolean and never a count, and clipped to the request's tiles where
    #: `masked_count` is not.
    matched: bool | None
    #: The same bit for `all_of[filters, highlight]` (`highlight-and-hierarchy.md` §2), and `None`
    #: where the request carried no `highlight`.
    highlighted: bool | None
    #: The identifier of the artifact this row is attached to, a row of the same response, or
    #: `None` for a row attached to nothing.
    target: int | None = None


FRAME_TILES = 1
FRAME_SUB_CELLS = 2
FRAME_POINTS = 3
FRAME_TRAILER = 4
FRAME_ARTIFACTS = 5

_KNOWN_KINDS = {
    FRAME_TILES,
    FRAME_SUB_CELLS,
    FRAME_POINTS,
    FRAME_TRAILER,
    FRAME_ARTIFACTS,
}

#: The trailer's closed key set (contracts §3.2 r26). ``stage_ns`` is the one optional key,
#: double-gated behind the server's timing feature and configuration.
TRAILER_REQUIRED_KEYS = frozenset({"stream_us", "arrow_serialise_ns", "points", "flushes"})
TRAILER_OPTIONAL_KEYS = frozenset({"stage_ns"})


def split_frames(data: bytes) -> list[tuple[int, bytes]]:
    """The raw `(kind, payload)` sequence, refusing truncation and unknown kinds.

    Framing only — no Arrow decode and no grammar check beyond the kinds; `decode_frames` is the
    layer that enforces tiles-first / trailer-last.
    """
    frames: list[tuple[int, bytes]] = []
    at = 0
    while at < len(data):
        if len(data) - at < 5:
            raise ValueError(f"truncated frame header at byte {at}")
        kind = data[at]
        if kind not in _KNOWN_KINDS:
            raise ValueError(f"unknown frame kind {kind} at byte {at} — refused, never skipped")
        (length,) = struct.unpack_from("<I", data, at + 1)
        start = at + 5
        end = start + length
        if end > len(data):
            raise ValueError(f"frame at byte {at} claims a payload past the end of the body")
        frames.append((kind, data[start:end]))
        at = end
    return frames


def _zip_shape(sx, sy):
    """Parts → rings → `(x, y)`, refusing the two axes disagreeing at any level.

    Contracts §3.2 r45: the axes carry the same structure by construction and a decoder checks
    it rather than assumes it — `zip` would silently truncate to the shorter side.
    """
    if len(sx) != len(sy):
        raise ValueError("shape axes disagree on the number of parts")
    parts = []
    for px, py in zip(sx, sy):
        if px is None or py is None or len(px) != len(py):
            raise ValueError("shape axes disagree on the number of rings in a part")
        rings = []
        for rx, ry in zip(px, py):
            if rx is None or ry is None or len(rx) != len(ry):
                raise ValueError("shape axes disagree on the number of vertices in a ring")
            rings.append(list(zip(rx, ry)))
        parts.append(rings)
    return parts


def _batches(payload: bytes):
    with ipc.open_stream(io.BytesIO(payload)) as reader:
        yield from reader


def decode_frames(data: bytes):
    """`(tiles, points, sub_cells, trailer)` — the full grammar-checked decode.

    - `tiles`: `(tile, visible, matched, served, highlighted)` per row.
    - `points`: `(tessera_id, code)` per point, concatenated across every points frame in order.
    - `sub_cells`: `(cell, count)` rows, or `None` when no kind-2 frame was present (underlay
      unrequested — distinct from `[]`, a present-but-empty frame; contracts §3.2's r12 rule).
    - `trailer`: the parsed JSON object, key set validated.
    """
    frames = split_frames(data)
    if not frames:
        raise ValueError("empty body: a response carries at least tiles + trailer")
    if frames[0][0] != FRAME_TILES:
        raise ValueError("the tiles frame must be first")
    if frames[-1][0] != FRAME_TRAILER:
        raise ValueError("missing trailer: the response is incomplete")

    tiles: list[tuple[int, int, int, int]] = []
    points: list[tuple[int, int]] = []
    sub_cells: list[tuple[int, int]] | None = None
    artifacts: list[Artifact] | None = None
    trailer: dict | None = None

    for index, (kind, payload) in enumerate(frames):
        if kind == FRAME_TILES:
            # Exactly one, first — a second tiles frame anywhere would silently concatenate
            # into the count surface, which is precisely the laxity a second reader must not
            # have (this decoder mirrors the shipped TS client's strictness deliberately).
            if index != 0:
                raise ValueError("more than one tiles frame")
            for batch in _batches(payload):
                tiles.extend(
                    zip(
                        batch.column("tile").to_pylist(),
                        batch.column("visible").to_pylist(),
                        batch.column("matched").to_pylist(),
                        batch.column("served").to_pylist(),
                        # `highlighted` is always present and equals `matched` where the request
                        # carried no `highlight` (`highlight-and-hierarchy.md` §2): an absent
                        # highlight is the identity for this quantity, so a reader needs no
                        # schema branch and a response without one is not a special case.
                        batch.column("highlighted").to_pylist(),
                    )
                )
        elif kind == FRAME_SUB_CELLS:
            if sub_cells is not None:
                raise ValueError("more than one sub-cells frame")
            if index != 1:
                raise ValueError("the sub-cells frame must immediately follow tiles")
            sub_cells = []
            for batch in _batches(payload):
                sub_cells.extend(
                    zip(
                        batch.column("cell").to_pylist(),
                        batch.column("count").to_pylist(),
                    )
                )
        elif kind == FRAME_ARTIFACTS:
            # At most one, and it sits between the counts and the points. A second would silently
            # concatenate into the artifact surface, which is the same laxity the tiles rule above
            # refuses.
            if artifacts is not None:
                raise ValueError("more than one artifacts frame")
            if any(k == FRAME_POINTS for k, _ in frames[:index]):
                raise ValueError("the artifacts frame precedes every points frame")
            artifacts = []
            for batch in _batches(payload):
                # `masked_count` is what the *asking principal* can see, never the artifact's
                # membership size. The oracle must not treat it as a cardinality of anything it
                # can enumerate independently: no unmasked quantity reaches this wire at all, by
                # design, so there is nothing here to reconcile against a corpus-wide figure.
                #
                # The geometry columns carry the same warning in a shape that hides it better: a
                # centroid or a derived hull is computed over `membership ∩ M_auth`, so two
                # principals legitimately disagree about the same `tessera_id` here too, and
                # neither shape is the artifact's (a predicate or authored shape agrees across
                # principals, but this reader does not know the kind and must not assume it). A
                # `None` is *this layer declares no such property* — never *withheld*, since an
                # artifact whose content could not be served is absent whole. `layer` is
                # dictionary-encoded (contracts §3.2 r44); `to_pylist` resolves the keys to their
                # utf8 values, so the encoding is invisible from here on. The two shape columns
                # TRAIL the fixed columns and are absent from the schema entirely when no served
                # row carries a drawn geometry (§3.2 r45) — an absent column is distinguishable
                # from a null one, so 0076's null rule gains no third reading.
                names = set(batch.schema.names)
                if ("shape_x" in names) != ("shape_y" in names):
                    raise ValueError("a shape with one axis column and not the other")
                columns = {
                    name: batch.column(name).to_pylist()
                    for name in (
                        "layer",
                        "tessera_id",
                        "key",
                        "masked_count",
                        "centroid_x",
                        "centroid_y",
                        "box_min_x",
                        "box_min_y",
                        "box_max_x",
                        "box_max_y",
                        "content",
                        "parent_ids",
                        "rung",
                        "matched",
                        "highlighted",
                        "target",
                    )
                }
                shapes = "shape_x" in names
                shape_x = batch.column("shape_x").to_pylist() if shapes else None
                shape_y = batch.column("shape_y").to_pylist() if shapes else None
                for row in range(batch.num_rows):
                    cx = columns["centroid_x"][row]
                    bx = columns["box_min_x"][row]
                    sx = shape_x[row] if shapes else None
                    sy = shape_y[row] if shapes else None
                    if (sx is None) != (sy is None):
                        raise ValueError("a shape with one axis and not the other")
                    artifacts.append(
                        Artifact(
                            layer=columns["layer"][row],
                            tessera_id=columns["tessera_id"][row],
                            key=columns["key"][row],
                            masked_count=columns["masked_count"][row],
                            centroid=(
                                None if cx is None else (cx, columns["centroid_y"][row])
                            ),
                            box=(
                                None
                                if bx is None
                                else (
                                    bx,
                                    columns["box_min_y"][row],
                                    columns["box_max_x"][row],
                                    columns["box_max_y"][row],
                                )
                            ),
                            shape=None if sx is None else _zip_shape(sx, sy),
                            content=list(columns["content"][row] or []),
                            rung=columns["rung"][row],
                            parent_ids=list(columns["parent_ids"][row] or []),
                            matched=columns["matched"][row],
                            highlighted=columns["highlighted"][row],
                            target=columns["target"][row],
                        )
                    )
            if not artifacts:
                raise ValueError(
                    "an empty artifacts frame: the server omits the frame when nothing is served, "
                    "so a present-but-empty one means the emitter and this reader disagree"
                )
        elif kind == FRAME_POINTS:
            for batch in _batches(payload):
                # `tessera_id` (u64), not `handle` (u32): contracts r6 retires the per-session
                # handle from the viewer plane and puts the stable wire identity at the row. The
                # oracle must not translate it — it is opaque here, and the differential compares
                # point sets by position code precisely so that agreement never depends on either
                # side interpreting an identifier.
                #
                # **`code` is absent under `point_rows: "highlight"`** — that projection is
                # `(tessera_id, highlighted)` and nothing else (`highlight-and-hierarchy.md` §2),
                # the client joining the bits to points it already holds. The row *set* and the
                # per-tile `served` split are identical under either projection, which is what the
                # consistency checks below actually test, so this reads a `None` position rather
                # than refusing a well-formed body.
                names = set(batch.schema.names)
                codes = (
                    batch.column("code").to_pylist()
                    if "code" in names
                    else [None] * batch.num_rows
                )
                points.extend(zip(batch.column("tessera_id").to_pylist(), codes))
        elif kind == FRAME_TRAILER:
            if trailer is not None:
                raise ValueError("more than one trailer frame")
            trailer = json.loads(payload)
            keys = set(trailer)
            if not TRAILER_REQUIRED_KEYS <= keys:
                raise ValueError(f"trailer missing required keys: {sorted(TRAILER_REQUIRED_KEYS - keys)}")
            extra = keys - TRAILER_REQUIRED_KEYS - TRAILER_OPTIONAL_KEYS
            if extra:
                raise ValueError(f"trailer carries keys outside the closed set: {sorted(extra)}")

    assert trailer is not None  # frames[-1] checked above
    if trailer["points"] != len(points):
        raise ValueError(
            f"trailer claims {trailer['points']} points but the body carries {len(points)}"
        )
    served_total = sum(t[3] for t in tiles)
    if served_total != len(points):
        raise ValueError(
            f"sum of served ({served_total}) != number of points ({len(points)})"
        )
    return tiles, points, sub_cells, artifacts, trailer


def decode_viewport(data: bytes):
    """`(tiles, points)`; tile rows are 5-tuples `(tile, visible, matched, served, highlighted)`."""
    tiles, points, _sub_cells, _artifacts, _trailer = decode_frames(data)
    return tiles, points


def decode_viewport_points(data: bytes):
    """The points stream as a `pyarrow.Table` — **every** column, not the two
    [`decode_viewport`] names.

    [`decode_viewport`] projects `(tessera_id, code)` because that is all the differential
    compares. A declared scalar (contracts §2.6 — the fixture's `fx_key`, the handle→item join)
    arrives as an additional column, and a test that means to assert on it has to see the schema
    rather than a fixed projection. Returning the table rather than widening the tuple keeps
    every existing caller's arity. Batches concatenate across points frames in frame order, which
    is served order.
    """
    import pyarrow as pa  # noqa: PLC0415 — only this function needs the table type

    frames = split_frames(data)
    batches = []
    schema = None
    for kind, payload in frames:
        if kind != FRAME_POINTS:
            continue
        with ipc.open_stream(io.BytesIO(payload)) as reader:
            schema = reader.schema
            batches.extend(reader)
    if schema is None:
        raise ValueError(
            "no points frame in this body: a zero-point response carries no points schema at all"
        )
    return pa.Table.from_batches(batches, schema)


def decode_viewport_with_subcells(data: bytes):
    """`(tiles, points, sub_cells)` — `sub_cells` is `[]` when the underlay was not requested.

    A separate function rather than a wider return from `decode_viewport`, so existing callers
    keep their arity. The `[]`-for-unrequested flattening is this function's compatibility
    contract with its existing callers; `decode_frames` is the layer that distinguishes
    unrequested (`None`) from present-but-empty (`[]`).
    """
    tiles, points, sub_cells, _artifacts, _trailer = decode_frames(data)
    return tiles, points, sub_cells if sub_cells is not None else []


def decode_viewport_artifacts(data: bytes):
    """The artifacts a response served, as [`Artifact`] rows.

    `[]` when the response carried no artifacts frame — and that is not a loss of information: the
    server omits the frame precisely when nothing is served, and *why* nothing is served (no layer
    reachable, none intersecting the viewport, none clearing its existence criterion) is
    deliberately not on the wire. A caller wanting to distinguish those has asked a question the
    response is designed not to answer.

    **`masked_count` is the asking principal's own count**, so two principals legitimately disagree
    about the same `tessera_id`, and neither figure is the artifact's membership size. A comparator
    that asserted agreement across principals would be asserting the bug.
    """
    _tiles, _points, _sub_cells, artifacts, _trailer = decode_frames(data)
    return artifacts if artifacts is not None else []


# `POST /v1/items` is framed the viewport's way with kinds of its own:
#
#     kind 6  head       JSON {order, page_rows, visible?, matched?}   exactly one, first
#     kind 7  records    one Arrow stream of one batch                  zero or more
#     kind 8  page end   JSON {next, ended_by}                          one after each records frame
#     kind 4  trailer    JSON {pages, rows, next, ended_by, stream_us}  exactly one, last
#
# The viewport's decoder above does not accept these kinds, and this one accepts only these.
FRAME_ITEMS_HEAD = 6
FRAME_RECORDS = 7
FRAME_PAGE_END = 8

_ITEMS_KINDS = {FRAME_ITEMS_HEAD, FRAME_RECORDS, FRAME_PAGE_END, FRAME_TRAILER}
ITEMS_TRAILER_KEYS = frozenset({"pages", "rows", "next", "ended_by", "stream_us"})


class ItemsBody(NamedTuple):
    """One `POST /v1/items` body, split and checked but not decoded past its JSON."""

    head: dict
    #: Each records frame's payload with the page end that follows it, parsed.
    pages: list[tuple[bytes, dict]]
    trailer: dict
    #: The head's, the page ends' and the trailer's payloads as sent.
    json_payloads: list[bytes]


def split_items_frames(data: bytes) -> ItemsBody:
    """An items body split strictly: truncation, a kind outside the four, a head not first, a
    trailer not last, a records frame without its page end or a page end without its records
    frame all raise, so a truncated body never reads as a shorter response."""
    frames: list[tuple[int, bytes]] = []
    at = 0
    while at < len(data):
        if len(data) - at < 5:
            raise ValueError(f"truncated frame header at byte {at}")
        kind = data[at]
        if kind not in _ITEMS_KINDS:
            raise ValueError(f"unknown frame kind {kind} at byte {at} in an items body")
        (length,) = struct.unpack_from("<I", data, at + 1)
        start = at + 5
        end = start + length
        if end > len(data):
            raise ValueError(f"frame at byte {at} claims a payload past the end of the body")
        frames.append((kind, data[start:end]))
        at = end
    if len(frames) < 2 or frames[0][0] != FRAME_ITEMS_HEAD or frames[-1][0] != FRAME_TRAILER:
        raise ValueError("an items body is a head first and a trailer last")
    middle = frames[1:-1]
    if len(middle) % 2:
        raise ValueError("a records frame without its page end")
    pages = []
    for (records_kind, records), (end_kind, end) in zip(middle[::2], middle[1::2]):
        if records_kind != FRAME_RECORDS or end_kind != FRAME_PAGE_END:
            raise ValueError("an items body pairs each records frame with the page end after it")
        pages.append((records, json.loads(end)))
    trailer = json.loads(frames[-1][1])
    if set(trailer) != ITEMS_TRAILER_KEYS:
        raise ValueError(f"the items trailer's keys are {sorted(trailer)}")
    json_payloads = [frames[0][1], *(end for (_, end) in middle[1::2]), frames[-1][1]]
    return ItemsBody(json.loads(frames[0][1]), pages, trailer, json_payloads)
