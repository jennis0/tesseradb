"""Decode the framed `/v1/viewport` body `tessera_wire::payload` builds (contracts §3.2 r26,
`streamed-serving.md`): a sequence of frames, each `u8 kind` + `u32 LE payload length` + payload,
every payload a complete Arrow IPC stream (JSON for the trailer):

    kind 1  tiles      (tile, visible, matched, served)      exactly one, first
    kind 2  sub-cells  (cell, count)                          exactly one, iff underlay requested
    kind 3  points     (tessera_id, code, ...scalars)         zero or more; concatenate in order
    kind 4  trailer    JSON                                   exactly one, last
    kind 5  artifacts  (layer, tessera_id, key,        at most one, after tiles and before
                        masked_count, and the derived            any points; absent when none served
                        geometry columns)

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
    hull: list[tuple[int, int]] | None
    #: One content, entire, positional to the layer's declared kinds. Empty means the layer
    #: declares no supplied content — never that content was withheld.
    content: list[str]


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


def _batches(payload: bytes):
    with ipc.open_stream(io.BytesIO(payload)) as reader:
        yield from reader


def decode_frames(data: bytes):
    """`(tiles, points, sub_cells, trailer)` — the full grammar-checked decode.

    - `tiles`: `(tile, visible, matched, served)` per row.
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
                # centroid or a hull is computed over `membership ∩ M_auth`, so two principals
                # legitimately disagree about the same `tessera_id` here too, and neither shape is
                # the artifact's. A `None` is *this layer declares no such property* — never
                # *withheld*, since an artifact whose content could not be served is absent whole.
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
                        "hull_x",
                        "hull_y",
                        "content",
                    )
                }
                for row in range(batch.num_rows):
                    cx = columns["centroid_x"][row]
                    bx = columns["box_min_x"][row]
                    hx, hy = columns["hull_x"][row], columns["hull_y"][row]
                    if (hx is None) != (hy is None):
                        raise ValueError("a hull with one axis and not the other")
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
                            hull=None if hx is None else list(zip(hx, hy)),
                            content=list(columns["content"][row] or []),
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
                points.extend(
                    zip(
                        batch.column("tessera_id").to_pylist(),
                        batch.column("code").to_pylist(),
                    )
                )
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
    """`(tiles, points)`; tile rows are 4-tuples `(tile, visible, matched, served)`."""
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
