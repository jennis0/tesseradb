#!/usr/bin/env python3
"""A worked decode of a `POST /v1/viewport` body, in Python with `pyarrow` and nothing of
Tessera's — not the oracle's `wire.py`, deliberately, so that a stranger holding only this file
and contracts §5 can read a response. `docs/openapi/README.md` walks it.

    python3 reference/examples/decode_viewport.py <body.bin>

prints every frame — kind, payload length, and for an Arrow payload its row count and columns —
then the first rows of each batch. A `tessera_id` is a `u64` and is printed as a decimal integer.

The framing (contracts §5): `u8 kind`, `u32` little-endian payload length, payload, repeated.
Every Arrow payload is a complete IPC stream, decodable alone; the trailer is JSON. Kinds:

    1  tiles       exactly one, first          (tile, visible, matched, served)
    2  sub-cells   exactly one iff requested   (cell, count)
    5  artifacts   at most one, before points  (layer, tessera_id, key, masked_count, ...)
    3  points      zero or more                (tessera_id, code, ...render columns)
    4  trailer     exactly one, last           JSON — its presence marks the response complete

Strict on purpose: a truncated body, a missing trailer or an unknown kind raises. Every prefix of
a stream is sound to draw — the counts are exact from the first frame — but it must not be
mistaken for the whole answer.
"""

from __future__ import annotations

import io
import json
import struct
import sys
from dataclasses import dataclass, field

import pyarrow as pa
import pyarrow.ipc as ipc

TILES, SUB_CELLS, POINTS, TRAILER, ARTIFACTS = 1, 2, 3, 4, 5
KIND_NAMES = {TILES: "tiles", SUB_CELLS: "sub-cells", POINTS: "points", TRAILER: "trailer",
              ARTIFACTS: "artifacts"}


def split_frames(body: bytes) -> list[tuple[int, bytes]]:
    """The frames of a body, as `(kind, payload)`. No Arrow is parsed here."""
    frames: list[tuple[int, bytes]] = []
    offset = 0
    while offset < len(body):
        if offset + 5 > len(body):
            raise ValueError(f"truncated frame header at byte {offset}")
        kind = body[offset]
        (length,) = struct.unpack_from("<I", body, offset + 1)
        if kind not in KIND_NAMES:
            raise ValueError(f"unknown frame kind {kind} at byte {offset}")
        end = offset + 5 + length
        if end > len(body):
            raise ValueError(f"frame of kind {kind} at byte {offset} claims {length} bytes past the end")
        frames.append((kind, body[offset + 5:end]))
        offset = end
    if not frames or frames[-1][0] != TRAILER:
        raise ValueError("no trailing kind-4 frame: the response is incomplete")
    if frames[0][0] != TILES:
        raise ValueError("the first frame must be the tiles frame")
    return frames


@dataclass
class Viewport:
    frames: list[int]
    tiles: pa.Table
    sub_cells: pa.Table | None
    artifacts: pa.Table | None
    points: list[pa.Table] = field(default_factory=list)
    trailer: dict = field(default_factory=dict)

    @property
    def point_rows(self) -> int:
        return sum(t.num_rows for t in self.points)


def decode_viewport(body: bytes) -> Viewport:
    """Every batch of one body. Points frames concatenate; their boundaries are not contract."""
    frames = split_frames(body)
    tiles = sub_cells = artifacts = None
    points: list[pa.Table] = []
    trailer: dict = {}
    for kind, payload in frames:
        if kind == TRAILER:
            trailer = json.loads(payload)
            continue
        table = ipc.open_stream(io.BytesIO(payload)).read_all()
        if kind == TILES:
            tiles = table
        elif kind == SUB_CELLS:
            sub_cells = table
        elif kind == ARTIFACTS:
            artifacts = table
        else:
            points.append(table)
    assert tiles is not None
    return Viewport([k for k, _ in frames], tiles, sub_cells, artifacts, points, trailer)


def first_rows(tables: list[pa.Table], n: int) -> list[dict]:
    """The first `n` rows across tables that concatenate, as `{column: value}`."""
    rows: list[dict] = []
    for table in tables:
        for i in range(min(table.num_rows, n - len(rows))):
            rows.append({name: table.column(name)[i].as_py() for name in table.column_names})
        if len(rows) >= n:
            break
    return rows


def main(path: str) -> None:
    body = open(path, "rb").read()
    frames = split_frames(body)
    print(f"{path}: {len(body)} bytes, {len(frames)} frames")
    for kind, payload in frames:
        if kind == TRAILER:
            print(f"  kind {kind} {KIND_NAMES[kind]}: {len(payload)} B  {payload.decode()}")
            continue
        table = ipc.open_stream(io.BytesIO(payload)).read_all()
        print(f"  kind {kind} {KIND_NAMES[kind]}: {len(payload)} B, {table.num_rows} rows, "
              f"columns {table.column_names}")
    v = decode_viewport(body)
    print("first tiles rows:", first_rows([v.tiles], 3))
    if v.sub_cells is not None:
        print("first sub-cells rows:", first_rows([v.sub_cells], 3))
    if v.artifacts is not None:
        print("first artifacts rows:", first_rows([v.artifacts], 3))
    print("first points rows:", first_rows(v.points, 3))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: decode_viewport.py <body.bin>", file=sys.stderr)
        sys.exit(2)
    main(sys.argv[1])
