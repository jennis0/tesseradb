"""Decode the framed `/v1/viewport` body `mosaica_wire::payload` builds (contracts §3.2 r26,
`streamed-serving.md`): a sequence of frames, each `u8 kind` + `u32 LE payload length` + payload,
every payload a complete Arrow IPC stream (JSON for the trailer):

    kind 1  tiles      (tile, visible, matched, served,      exactly one, first
                        highlighted)
    kind 2  sub-cells  (cell, count)                          exactly one, iff underlay requested
    kind 3  points     (tessera_id, code, ...scalars)         zero or more; concatenate in order;
                        a scalar with no value is null
    kind 4  trailer    JSON                                   exactly one, last

and the `/v1/artifacts/viewport` body, the same framing with one kind and a trailer of its own:

    kind 5  artifacts  (layer dict<u16,utf8>, tessera_id,  first, one with a null `tile` for the
                        key, masked_count, the centroid      treed layers, absent where it holds
                        and box, content, parent_ids,        none; then exactly one per tile in
                        rung, matched, highlighted,          request order, empty where the tile
                        target, tile)                        holds none
    kind 4  trailer    JSON (stream_us, arrow_serialise_ns,   exactly one, last
                        rows, frames)

Mirrors `crates/mosaica-server/tests/common/mod.rs`'s `decode_viewport_frames` byte-for-byte,
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
    """One served artifact, as a kind-5 frame carries it.

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
    #: Whether a member this principal may see, inside the frame's tile, matches the request's
    #: `filters` ([decision 0104](../../docs/decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md)).
    #: `None` — a null on the wire — where the request carried none: *there was no question*,
    #: never *no matches*. A boolean and never a count, and clipped to the tile where
    #: `masked_count` is not.
    matched: bool | None
    #: The same bit for `all_of[filters, highlight]` (`highlight-and-hierarchy.md` §2), and `None`
    #: where the request carried no `highlight`.
    highlighted: bool | None
    #: The identifier of the artifact this row is attached to, a row of the same frame or of the
    #: treed frame, or `None` for a row attached to nothing.
    target: int | None = None
    #: The tile, as its Morton prefix at the request's depth, whose visible members put the row in
    #: its frame; `None` in the treed frame.
    tile: int | None = None
    #: The palette slot this principal's colouring gives the artifact, or `None` where the request
    #: named no `palette_size`.
    slot: int | None = None


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
#: The `/v1/artifacts/viewport` trailer's closed key set.
ARTIFACTS_TRAILER_KEYS = frozenset({"stream_us", "arrow_serialise_ns", "rows", "frames"})
#: An artifacts frame's columns, in their fixed order.
ARTIFACT_COLUMNS = (
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
    "tile",
    "slot",
)


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
            raise ValueError(
                "an artifacts frame in a viewport body: artifacts are served by "
                "/v1/artifacts/viewport, and a viewport body carries none"
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
    return tiles, points, sub_cells, trailer


def decode_viewport(data: bytes):
    """`(tiles, points)`; tile rows are 5-tuples `(tile, visible, matched, served, highlighted)`."""
    tiles, points, _sub_cells, _trailer = decode_frames(data)
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
    tiles, points, sub_cells, _trailer = decode_frames(data)
    return tiles, points, sub_cells if sub_cells is not None else []


def _artifact_rows(payload: bytes) -> list[Artifact]:
    """One artifacts frame's rows, its eighteen columns read by name and checked by position.

    `masked_count` is what the *asking principal* can see, never the artifact's membership size,
    and the centroid and box are over the members they can see: two principals legitimately
    disagree about one `tessera_id`. A `None` geometry is *this layer declares no such property,
    or the request asked for none* — never *withheld*, since an artifact that cannot be served is
    absent whole. `layer` is dictionary-encoded; `to_pylist` resolves it.
    """
    rows: list[Artifact] = []
    with ipc.open_stream(io.BytesIO(payload)) as reader:
        if tuple(reader.schema.names) != ARTIFACT_COLUMNS:
            raise ValueError(f"an artifacts frame's columns are {reader.schema.names}")
        for batch in reader:
            columns = {name: batch.column(name).to_pylist() for name in ARTIFACT_COLUMNS}
            for row in range(batch.num_rows):
                cx = columns["centroid_x"][row]
                bx = columns["box_min_x"][row]
                rows.append(
                    Artifact(
                        layer=columns["layer"][row],
                        tessera_id=columns["tessera_id"][row],
                        key=columns["key"][row],
                        masked_count=columns["masked_count"][row],
                        centroid=None if cx is None else (cx, columns["centroid_y"][row]),
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
                        content=list(columns["content"][row] or []),
                        rung=columns["rung"][row],
                        parent_ids=list(columns["parent_ids"][row] or []),
                        matched=columns["matched"][row],
                        highlighted=columns["highlighted"][row],
                        target=columns["target"][row],
                        tile=columns["tile"][row],
                        slot=columns["slot"][row],
                    )
                )
    return rows


def decode_artifact_frames(data: bytes):
    """`(frames, trailer)` of a `/v1/artifacts/viewport` body: each frame as `(tile, rows)`. `tile`
    is `None` for the treed frame, and for a tile's frame of no rows, which names no tile.

    Strict as [`decode_frames`] is: a frame of another kind, rows of one frame naming two tiles, a
    treed frame anywhere but first or holding no row, a missing trailer, a trailer key outside the
    closed set, or a trailer whose counts disagree with the body all raise.
    """
    frames: list[tuple[int | None, list[Artifact]]] = []
    trailer: dict | None = None
    at = 0
    raw: list[tuple[int, bytes]] = []
    while at < len(data):
        if len(data) - at < 5:
            raise ValueError(f"truncated frame header at byte {at}")
        kind = data[at]
        (length,) = struct.unpack_from("<I", data, at + 1)
        end = at + 5 + length
        if end > len(data):
            raise ValueError(f"frame at byte {at} claims a payload past the end of the body")
        raw.append((kind, data[at + 5 : end]))
        at = end
    if not raw or raw[-1][0] != FRAME_TRAILER:
        raise ValueError("missing trailer: the response is incomplete")
    for index, (kind, payload) in enumerate(raw[:-1]):
        if kind != FRAME_ARTIFACTS:
            raise ValueError(f"a frame of kind {kind} in an artifacts viewport body")
        rows = _artifact_rows(payload)
        tiles = {row.tile for row in rows}
        if len(tiles) > 1:
            raise ValueError(f"one frame names the tiles {sorted(tiles, key=str)}")
        if rows and rows[0].tile is None and index != 0:
            raise ValueError("the treed frame comes first")
        frames.append((rows[0].tile if rows else -1, rows))
    trailer = json.loads(raw[-1][1])
    if set(trailer) != ARTIFACTS_TRAILER_KEYS:
        raise ValueError(f"the trailer's keys are {sorted(trailer)}")
    if trailer["frames"] != len(frames):
        raise ValueError(f"the trailer counts {trailer['frames']} frames, the body {len(frames)}")
    if trailer["rows"] != sum(len(rows) for _, rows in frames):
        raise ValueError("the trailer's rows disagree with the body")
    return [(None if tile == -1 else tile, rows) for tile, rows in frames], trailer


def decode_viewport_artifacts(data: bytes) -> list[Artifact]:
    """Every artifact a `/v1/artifacts/viewport` body served, once, in the order first served,
    with `matched` and `highlighted` taken over every tile it was served in and `tile` `None`: what
    one answer over the whole of the request's tiles says.

    `[]` where nothing was served — and *why* nothing is served (no layer reachable, none in the
    tiles, none clearing its existence criterion) is deliberately not on the wire.
    """
    frames, _trailer = decode_artifact_frames(data)
    merged: dict[int, Artifact] = {}
    for _tile, rows in frames:
        for row in rows:
            held = merged.get(row.tessera_id)
            if held is None:
                merged[row.tessera_id] = row._replace(tile=None)
                continue
            either = lambda a, b: None if a is None or b is None else (a or b)  # noqa: E731
            merged[row.tessera_id] = held._replace(
                matched=either(held.matched, row.matched),
                highlighted=either(held.highlighted, row.highlighted),
            )
    return list(merged.values())


# `POST /v1/items` is framed the viewport's way with kinds of its own:
#
#     kind 6  head       JSON {order, page_rows, visible?, matched?}   exactly one, first
#     kind 7  records    one Arrow stream of one batch                  one or more; none only
#                                                                    where cancelled at once
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


# `POST /v1/aggregate` is framed as an items body is, with a table head where the items head was,
# one per table and no response head:
#
#     kind 9  table head JSON {grouping, total, reference_total?, groups?, resumed}
#                                                  before a table's first page in this response
#     kind 7  records    one Arrow stream of one batch of the table's rows
#     kind 8  page end   JSON {next, ended_by}     one after each records frame
#     kind 4  trailer    JSON, as an items trailer, and `recomposed: true` where a page counted a
#                        changed corpus                                 exactly one, last
FRAME_TABLE_HEAD = 9

_AGGREGATE_KINDS = {FRAME_TABLE_HEAD, FRAME_RECORDS, FRAME_PAGE_END, FRAME_TRAILER}


class AggregateBody(NamedTuple):
    """One `POST /v1/aggregate` body, split and checked but not decoded past its JSON."""

    #: Each table head, parsed, with the `(records payload, page end)` pairs that follow it.
    tables: list[tuple[dict, list[tuple[bytes, dict]]]]
    trailer: dict


def split_aggregate_frames(data: bytes) -> AggregateBody:
    """An aggregate body split strictly: truncation, a kind outside the four, a trailer not last,
    a page before any table head, or a records frame without its page end all raise."""
    frames: list[tuple[int, bytes]] = []
    at = 0
    while at < len(data):
        if len(data) - at < 5:
            raise ValueError(f"truncated frame header at byte {at}")
        kind = data[at]
        if kind not in _AGGREGATE_KINDS:
            raise ValueError(f"unknown frame kind {kind} at byte {at} in an aggregate body")
        (length,) = struct.unpack_from("<I", data, at + 1)
        start = at + 5
        end = start + length
        if end > len(data):
            raise ValueError(f"frame at byte {at} claims a payload past the end of the body")
        frames.append((kind, data[start:end]))
        at = end
    if not frames or frames[-1][0] != FRAME_TRAILER:
        raise ValueError("an aggregate body ends with its trailer")
    tables: list[tuple[dict, list[tuple[bytes, dict]]]] = []
    middle = frames[:-1]
    index = 0
    while index < len(middle):
        kind, payload = middle[index]
        if kind == FRAME_TABLE_HEAD:
            tables.append((json.loads(payload), []))
            index += 1
            continue
        if kind != FRAME_RECORDS or index + 1 >= len(middle) or middle[index + 1][0] != FRAME_PAGE_END:
            raise ValueError("an aggregate body pairs each records frame with the page end after it")
        if not tables:
            raise ValueError("a page before any table head")
        tables[-1][1].append((payload, json.loads(middle[index + 1][1])))
        index += 2
    trailer = json.loads(frames[-1][1])
    if set(trailer) - {"recomposed"} != ITEMS_TRAILER_KEYS or trailer.get("recomposed", True) is not True:
        raise ValueError(f"the aggregate trailer is {trailer}")
    return AggregateBody(tables, trailer)
