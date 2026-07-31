"""Decode the framed Arrow payload `tessera_wire::viewport_ipc` builds (Reference Sheet R5):
a 4-byte LE length, the tile stream, the points stream, and — only when the request asked for the
§3.3 underlay — a trailing sub-cell stream.

Mirrors `crates/tessera-server/tests/http.rs`'s `decode_viewport` byte-for-byte, independently
implemented in Python (this is the client-side decode any real SDK would need, not shared Rust
logic). Moved here from `reference/tests/wire.py` (Task 15's refactor) so `conformance/tests` can
reuse it without copy-paste; `reference/tests/wire.py` re-exports this module's `decode_viewport`
unchanged.

**Only the tile boundary is length-prefixed.** The sub-cell stream is appended, so recovering it
means parsing the points stream to its end-of-stream marker and taking the cursor position — which
is what `decode_viewport_with_subcells` does. `pyarrow.ipc.open_stream` stops at that marker without
inspecting what follows, which is exactly why appending is backward-compatible: a reader that treats
"everything after the tile stream" as the points stream still decodes it correctly.
"""

from __future__ import annotations

import io
import struct

import pyarrow.ipc as ipc


def split_frames(data: bytes) -> tuple[bytes, bytes]:
    """Split the framed payload into `(tile_bytes, rest)` without decoding either.

    `rest` is the points stream *and anything appended after it* — the sub-cell stream, when the
    request asked for the underlay. This is deliberately the pre-underlay behaviour: passing `rest`
    straight to `open_stream` yields the points batch regardless, because the reader stops at the
    end-of-stream marker.
    """
    (tile_len,) = struct.unpack_from("<I", data, 0)
    tile_bytes = data[4 : 4 + tile_len]
    rest = data[4 + tile_len :]
    return tile_bytes, rest


def _decode_tiles(tile_bytes: bytes) -> list[tuple[int, int, int, int]]:
    """`(tile, visible, matched, served)` per row.

    `served` is contracts r7's addition, appended after `matched`: how many of this tile's points
    are in the flat points batch. It is the only way to split that batch under §7.2's density rule,
    where the per-tile count is no longer `min(k, visible)`.
    """
    tiles = []
    with ipc.open_stream(io.BytesIO(tile_bytes)) as reader:
        for batch in reader:
            tile_col = batch.column("tile").to_pylist()
            visible_col = batch.column("visible").to_pylist()
            matched_col = batch.column("matched").to_pylist()
            served_col = batch.column("served").to_pylist()
            for t, v, m, s in zip(tile_col, visible_col, matched_col, served_col):
                tiles.append((t, v, m, s))
    return tiles


def decode_viewport(data: bytes):
    """`(tiles, points)`; tile rows are 4-tuples `(tile, visible, matched, served)`."""
    tile_bytes, points_bytes = split_frames(data)

    tiles = _decode_tiles(tile_bytes)

    points = []
    with ipc.open_stream(io.BytesIO(points_bytes)) as reader:
        for batch in reader:
            # `tessera_id` (u64), not `handle` (u32): contracts r6 retires the per-session
            # handle from the viewer plane and puts the stable wire identity at the row. The
            # oracle must not translate it — it is opaque here, and the differential compares
            # point sets by `(x, y)` multiset precisely so that agreement never depends on
            # either side interpreting an identifier.
            id_col = batch.column("tessera_id").to_pylist()
            x_col = batch.column("x").to_pylist()
            y_col = batch.column("y").to_pylist()
            for ident, x, y in zip(id_col, x_col, y_col):
                points.append((ident, x, y))

    return tiles, points


def decode_viewport_points(data: bytes):
    """The points stream as a `pyarrow.Table` — **every** column, not the three [`decode_viewport`]
    names.

    [`decode_viewport`] projects `(tessera_id, x, y)` because that is all the differential compares.
    A declared scalar (contracts §2.6 — the fixture's `fx_key`, the handle→item join) arrives as an
    additional column, and a test that means to assert on it has to see the schema rather than a
    fixed projection. Returning the table rather than widening the tuple keeps every existing
    caller's arity.
    """
    import pyarrow as pa  # noqa: PLC0415 — only this function needs the table type

    _tile_bytes, points_bytes = split_frames(data)
    with ipc.open_stream(io.BytesIO(points_bytes)) as reader:
        return pa.Table.from_batches(list(reader), reader.schema)


def decode_viewport_with_subcells(data: bytes):
    """`(tiles, points, sub_cells)` — `sub_cells` is `[]` when the underlay was not requested.

    A separate function rather than a wider return from `decode_viewport`, so existing callers keep
    their arity. Recovering the third stream needs the points stream parsed to its end, because only
    the tile boundary carries a length prefix (see this module's doc).
    """
    tile_bytes, rest = split_frames(data)
    tiles = _decode_tiles(tile_bytes)

    points = []
    buf = io.BytesIO(rest)
    with ipc.open_stream(buf) as reader:
        for batch in reader:
            id_col = batch.column("tessera_id").to_pylist()
            x_col = batch.column("x").to_pylist()
            y_col = batch.column("y").to_pylist()
            for ident, x, y in zip(id_col, x_col, y_col):
                points.append((ident, x, y))

    # Whatever the points reader did not consume. Zero bytes means the underlay was not requested —
    # `viewport_ipc` emits nothing at all rather than an empty stream, precisely so that this is
    # unambiguous.
    trailing = rest[buf.tell() :]
    sub_cells: list[tuple[int, int]] = []
    if trailing:
        with ipc.open_stream(io.BytesIO(trailing)) as reader:
            for batch in reader:
                cell_col = batch.column("cell").to_pylist()
                count_col = batch.column("count").to_pylist()
                for cell, count in zip(cell_col, count_col):
                    sub_cells.append((cell, count))

    return tiles, points, sub_cells
