"""Write one probe corpus: N points and one indexed keyword column of a chosen shape.

The three shapes are the ones the keyword emit's cost bends with — how many distinct keys the
column holds, which is what decided whether the superseded binary search stayed in cache:

    distinct   every row a fresh 36-character uuid (rung 5's `uuid`)
    many       240,000 distinct binomials, Zipf-distributed (rung 5's `scientific_name`)
    few        24 distinct short codes

Usage: make_corpus.py <out-dir> <shape> <rows>

Writes `<out-dir>/points.parquet` and `<out-dir>/corpus.toml`. Coordinates are a deterministic
scatter — the geometry is not what is being measured, and a fixed seed keeps two runs of the same
shape comparable byte for byte.
"""

import pathlib
import sys

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

OUT, SHAPE, ROWS = pathlib.Path(sys.argv[1]), sys.argv[2], int(sys.argv[3])
OUT.mkdir(parents=True, exist_ok=True)
rng = np.random.default_rng(20260908)

HEX = np.frombuffer(b"0123456789abcdef", dtype=np.uint8)


def fixed_width_strings(body: np.ndarray) -> pa.Array:
    """An (n, w) array of ASCII bytes as a `pa.string()` array, with no per-row Python object."""
    n, width = body.shape
    offsets = (np.arange(n + 1, dtype=np.int32) * width).tobytes()
    return pa.Array.from_buffers(
        pa.string(), n, [None, pa.py_buffer(offsets), pa.py_buffer(body.tobytes())]
    )


# A `pa.string()` array addresses its bytes with 32-bit offsets, so one array holds at most 2 GiB
# of characters — 59.6 million uuids. Past that the column is a chunked array of several.
UUIDS_PER_CHUNK = 40_000_000


def uuids(n: int) -> pa.ChunkedArray:
    """n distinct 36-character uuids. The first eight characters are the row index in hex, so
    distinctness is a property of the construction rather than of the generator's luck."""
    return pa.chunked_array(
        [uuid_chunk(lo, min(lo + UUIDS_PER_CHUNK, n)) for lo in range(0, n, UUIDS_PER_CHUNK)],
        type=pa.string(),
    )


def uuid_chunk(lo: int, hi: int) -> pa.Array:
    n = hi - lo
    body = np.empty((n, 36), dtype=np.uint8)
    hex_positions = [i for i in range(36) if i not in (8, 13, 18, 23)]
    body[:, hex_positions] = HEX[rng.integers(0, 16, size=(n, 32))]
    body[:, [8, 13, 18, 23]] = ord("-")
    index = np.arange(lo, hi, dtype=np.uint64)
    for k in range(8):
        body[:, 7 - k] = HEX[(index >> np.uint64(4 * k)) & np.uint64(0xF)]
    return fixed_width_strings(body)


def repeated(vocab: list[str], pick: np.ndarray) -> pa.Array:
    """`vocab[pick]`, built through a dictionary array so the n-row expansion stays in Arrow."""
    codes = pa.array(pick.astype(np.int32), type=pa.int32())
    return pa.DictionaryArray.from_arrays(codes, pa.array(vocab)).dictionary_decode()


def keys(shape: str, n: int) -> pa.Array:
    if shape == "distinct":
        return uuids(n)
    if shape == "many":
        vocab = [f"Genus{i // 400:04d} species{i % 400:03d}" for i in range(240_000)]
        # Zipf rather than uniform: a name column's mass sits on a few thousand names, and how
        # often a key repeats is what the merge's work per distinct key follows.
        pick = np.minimum(rng.zipf(1.2, size=n) - 1, len(vocab) - 1)
        return repeated(vocab, pick)
    if shape == "few":
        vocab = [f"code-{i:02d}" for i in range(24)]
        return repeated(vocab, rng.integers(0, len(vocab), size=n))
    raise SystemExit(f"unknown shape {shape!r}")


column = keys(SHAPE, ROWS)
table = pa.table(
    {
        "entity_id": pa.array(np.arange(ROWS, dtype=np.uint32)),
        "x": pa.array(rng.random(ROWS) * 1000.0),
        "y": pa.array(rng.random(ROWS) * 1000.0),
        "key": column,
    }
)
pq.write_table(table, OUT / "points.parquet", compression="zstd")

(OUT / "corpus.toml").write_text(
    """# Probe corpus: one view and one indexed keyword column. Written by `make_corpus.py`.
[sources]
points = "points.parquet"

[defaults]
source = "points"
entity_id_field = "entity_id"

[[view]]
name             = "map"
title            = "Probe"
projection       = "none"
extent           = "auto"
source           = "points"
visibility       = "public"
point_visibility = { default = "public" }

[[attribute]]
name  = "key"
title = "Key"
type  = "keyword"
index = true
"""
)
# The uuid shape carries the row index in the key, so its distinctness is a property of the
# construction; the other two are counted, over one array small enough to count over.
distinct = ROWS if SHAPE == "distinct" else len(pc.unique(column))
print(f"{OUT}: {ROWS:,} rows, shape {SHAPE}, {distinct:,} distinct keys")
