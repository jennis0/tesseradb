"""The canonicalisation, pinned against synthetic framed bodies — no server, no fixture.

Synthetic on purpose: each test builds two bodies differing in exactly one respect, which no live
server can be asked to do. The properties pinned are the ones §12.2 makes load-bearing — the two
legitimate sources of variation are removed (elapsed time, tile emission order) and *nothing
else* is, so every other byte difference lands on exactly one named surface. The chunk-boundary
sensitivity test is the module's written-down assumption in executable form: the comparison is
deliberately sensitive to points chunking, which is sound only while boundaries are a function of
served content alone (`suite.canonical`'s module doc).
"""

from __future__ import annotations

import io
import json
import struct

import pyarrow as pa
import pyarrow.ipc as ipc
import pytest

from .canonical import Streamed, canonicalise_viewport

U64 = pa.uint64()
TILES = pa.schema([("tile", U64), ("visible", U64), ("matched", U64), ("served", U64)])
POINTS = pa.schema([("tessera_id", U64), ("code", U64)])
CELLS = pa.schema([("cell", U64), ("count", U64)])


def _stream(schema: pa.Schema, batches: list[list[tuple[int, ...]]]) -> bytes:
    """One Arrow IPC stream; zero batches gives the schema-only form contracts §3.2 r12 names."""
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        for rows in batches:
            columns = list(zip(*rows)) if rows else [[] for _ in schema]
            writer.write_batch(
                pa.record_batch([pa.array(c, type=U64) for c in columns], schema=schema)
            )
    return sink.getvalue()


def _frame(kind: int, payload: bytes) -> bytes:
    return bytes([kind]) + struct.pack("<I", len(payload)) + payload


def _body(
    tiles: list[list[tuple[int, int, int, int]]],
    points_chunks: list[list[tuple[int, int]]],
    subcells: list[tuple[int, int]] | None = None,
    stream_us: int = 1,
    arrow_serialise_ns: int = 2,
    flushes: int | None = None,
    trailer_extra: dict | None = None,
) -> bytes:
    """A framed body consistent with the decoder's arithmetic (served sum == points == trailer)."""
    frames = [_frame(1, _stream(TILES, tiles))]
    if subcells is not None:
        frames.append(_frame(2, _stream(CELLS, [subcells] if subcells else [])))
    frames += [_frame(3, _stream(POINTS, [chunk])) for chunk in points_chunks]
    trailer = {
        "stream_us": stream_us,
        "arrow_serialise_ns": arrow_serialise_ns,
        "points": sum(len(c) for c in points_chunks),
        "flushes": flushes if flushes is not None else len(points_chunks),
    }
    trailer.update(trailer_extra or {})
    frames.append(_frame(4, json.dumps(trailer).encode()))
    return b"".join(frames)


# Two tiles, three points, one underlay cell — the smallest body exercising all four surfaces.
BASE_TILES = [(0, 5, 5, 2), (3, 7, 7, 1)]
BASE_POINTS = [(1, 10), (2, 20), (3, 30)]
BASE_CELLS = [(0, 4)]


def _base(**overrides) -> bytes:
    kwargs = dict(
        tiles=[BASE_TILES], points_chunks=[BASE_POINTS], subcells=BASE_CELLS, flushes=1
    )
    kwargs.update(overrides)
    return _body(**kwargs)


def test_two_issues_of_one_request_canonicalise_equal():
    """The whole point: elapsed time and tile emission order are the two legitimate variations
    between issues, and removing exactly them makes two issues compare equal."""
    a = canonicalise_viewport(_base(stream_us=111, arrow_serialise_ns=222))
    b = canonicalise_viewport(
        _base(tiles=[list(reversed(BASE_TILES))], stream_us=999_999, arrow_serialise_ns=1)
    )
    assert a == b


def test_tile_batch_chunking_is_normalised():
    """Two record batches inside the one tiles payload canonicalise as one — emission chunking is
    no more contract than emission order, and sorting already reassembles the rows."""
    one = canonicalise_viewport(_base())
    split = canonicalise_viewport(_base(tiles=[[BASE_TILES[0]], [BASE_TILES[1]]]))
    assert one == split


def test_each_difference_lands_on_exactly_its_own_surface():
    """Three separately-addressable surfaces plus the trailer remainder — never one blob.

    This is the property whose absence a review caught: a single concatenated blob let a
    canonicalisation that dropped the points batch stay green, the control firing on whatever
    remained. Here, a one-value change in each region moves that region's surface and no other.
    """
    base = canonicalise_viewport(_base())
    moved_by = {
        "tiles": _base(tiles=[[(0, 6, 6, 2), (3, 7, 7, 1)]]),
        "points": _base(points_chunks=[[(1, 10), (2, 20), (3, 99)]]),
        "underlay": _base(subcells=[(0, 5)]),
        "trailer": _base(flushes=2),
    }
    for surface, body in moved_by.items():
        variant = canonicalise_viewport(body)
        differing = {
            name for name, bytes_ in variant.surfaces().items() if bytes_ != base.surfaces()[name]
        }
        assert differing == {surface}, (
            f"a change confined to the {surface} region moved {sorted(differing)} — each surface "
            f"must be addressable alone, or a comparator cannot say where a defect landed"
        )


def test_the_points_comparison_is_sensitive_to_chunk_boundaries():
    """The written-down assumption, as behaviour: concatenated kind-3 payloads carry per-frame
    stream headers, so a re-chunk of the same served rows changes the canonical points bytes.

    That sensitivity is accepted deliberately (§12.2): boundaries are a function of served content
    alone today, and comparing in served order without re-parsing is strictly stronger than any
    sorted comparison. If the emitter ever chunks on timing, this documented trade stops being
    sound — and the determinism test in `test_battery.py` is what starts flaking first.
    """
    one_chunk = canonicalise_viewport(_base())
    split = canonicalise_viewport(
        _base(points_chunks=[[(1, 10), (2, 20)], [(3, 30)]], flushes=1)
    )
    assert one_chunk.points != split.points
    assert one_chunk.tiles == split.tiles
    assert one_chunk.underlay == split.underlay
    assert one_chunk.trailer == split.trailer


def test_an_unrequested_underlay_is_distinct_from_a_requested_empty_one():
    """Contracts §3.2 r12: presence is a property of the request. Unrequested is no kind-2 frame
    (canonically ``b""``); requested-and-empty is a present schema-only stream, which is bytes."""
    unrequested = canonicalise_viewport(_base(subcells=None))
    empty = canonicalise_viewport(_base(subcells=[]))
    assert unrequested.underlay == b""
    assert empty.underlay != b""
    assert unrequested != empty


def test_canonicalisation_refuses_rather_than_canonicalises_a_malformed_body():
    """Strictness is inherited from `oracle.wire`, not reimplemented: a truncated body and a
    trailer key outside the closed set both refuse, so nothing malformed acquires a canonical
    form it could be compared under."""
    with pytest.raises(ValueError):
        canonicalise_viewport(_base()[:-1])
    with pytest.raises(ValueError):
        canonicalise_viewport(_base(trailer_extra={"surprise": 1}))


def test_the_canonical_form_names_its_surfaces():
    """`surfaces()` is the comparator's iteration order — the four names are contract within the
    suite, and the canary's per-surface control assertions key off them."""
    assert set(canonicalise_viewport(_base()).surfaces()) == {
        "tiles",
        "points",
        "underlay",
        "trailer",
    }
    assert isinstance(canonicalise_viewport(_base()), Streamed)
