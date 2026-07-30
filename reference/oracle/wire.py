"""Decode the framed Arrow payload `tessera_wire::viewport_ipc` builds (Reference Sheet R5):
a 4-byte LE length, the tile stream, then the points stream.

Mirrors `crates/tessera-server/tests/http.rs`'s `decode_viewport` byte-for-byte, independently
implemented in Python (this is the client-side decode any real SDK would need, not shared Rust
logic). Moved here from `reference/tests/wire.py` (Task 15's refactor) so `conformance/tests` can
reuse it without copy-paste; `reference/tests/wire.py` re-exports this module's `decode_viewport`
unchanged.
"""

from __future__ import annotations

import io
import struct

import pyarrow.ipc as ipc


def split_frames(data: bytes) -> tuple[bytes, bytes]:
    """Split the framed payload into `(tile_bytes, points_bytes)` without decoding either."""
    (tile_len,) = struct.unpack_from("<I", data, 0)
    tile_bytes = data[4 : 4 + tile_len]
    points_bytes = data[4 + tile_len :]
    return tile_bytes, points_bytes


def decode_viewport(data: bytes):
    tile_bytes, points_bytes = split_frames(data)

    tiles = []
    with ipc.open_stream(io.BytesIO(tile_bytes)) as reader:
        for batch in reader:
            tile_col = batch.column("tile").to_pylist()
            visible_col = batch.column("visible").to_pylist()
            matched_col = batch.column("matched").to_pylist()
            for t, v, m in zip(tile_col, visible_col, matched_col):
                tiles.append((t, v, m))

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
