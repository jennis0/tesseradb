"""Where rung 3's bytes are, and the two facts about them that every later step depends on.

**The publisher's own bytes are on the share and are read exactly once** (`../README.md`): 38
chunks of `embeds_chunk_N.npy` (768-dimensional `float32`, 105 GB), `pmids_chunk_N.json` (the row
order, and the join key to everything else) and `pubmed_chunk_N.json` (per PMID, the date, the
title, the abstract and the MeSH descriptors — 52 GB). SMB at ~67 MB/s measured 2026-08-27, so one
pass over the three of them is ~40 minutes and there must not be a second: `stage.py` writes
everything the rest of the rung reads to the local ladder directory.

**Chunk order is row order, and it is numeric.** `entity_id` is the global row index over the 38
chunks taken as `0, 1, … 37` — not the lexicographic order the directory listing gives, which puts
chunk 10 after chunk 1. The offsets are computed from the `.npy` headers alone, which cost 38 small
reads rather than 105 GB, and are written into `staging/vectors.json` so that nothing downstream
recomputes them.
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np

from ..common.paths import ladder, staged

DATASET, VINTAGE = "medcpt-pubmed", "2026-08-27"

#: The MedCPT article encoder's width.
EMBED_DIM = 768

#: The 38 chunks, in the order that defines `entity_id`.
CHUNKS = tuple(range(38))

RUNG = "medcpt"


def share() -> Path:
    """The staged acquisition. Never built or served from — read once by `stage.py`."""
    return staged(DATASET, VINTAGE)


def staging(out: Path | None = None) -> Path:
    """`$TESSERA_LADDER/medcpt/staging/`, the local copy everything after `stage.py` reads."""
    path = (out or ladder(RUNG)) / "staging"
    path.mkdir(parents=True, exist_ok=True)
    return path


def npy_header(path: Path) -> tuple[tuple[int, int], str, bytes]:
    """One chunk's shape, dtype and the raw header bytes, without reading a vector.

    The header bytes are hashed into `vectors.json` so a partial or re-fetched staging run is
    detected rather than trusted: a chunk written from a different file has a different header.
    """
    with path.open("rb") as f:
        major, minor = np.lib.format.read_magic(f)
        reader = {
            (1, 0): np.lib.format.read_array_header_1_0,
            (2, 0): np.lib.format.read_array_header_2_0,
        }[(major, minor)]
        shape, fortran, dtype = reader(f)
        end = f.tell()
        f.seek(0)
        head = f.read(end)
    assert not fortran, f"{path.name} is Fortran-ordered"
    return shape, str(dtype), head


def chunk_offsets(share_dir: Path) -> tuple[list[int], list[int]]:
    """`(rows per chunk, global offset of each chunk)`, from the 38 headers."""
    rows = []
    for n in CHUNKS:
        shape, dtype, _ = npy_header(share_dir / f"embeds_chunk_{n}.npy")
        assert shape[1] == EMBED_DIM, f"chunk {n} is {shape[1]}-dimensional"
        assert dtype == "float32", f"chunk {n} is {dtype}"
        rows.append(int(shape[0]))
    offsets, at = [], 0
    for r in rows:
        offsets.append(at)
        at += r
    return rows, offsets


def vectors(out: Path | None = None) -> tuple[np.memmap, dict]:
    """The staged matrix and its sidecar, opened read-only.

    Refuses a matrix whose sidecar does not say every chunk is written: a partially staged run
    reads as zeros, which is a silent wrong answer rather than a loud one.
    """
    dirpath = staging(out)
    meta = json.loads((dirpath / "vectors.json").read_text())
    missing = [n for n in CHUNKS if str(n) not in meta["chunks"]]
    assert not missing, f"staging is incomplete: chunks {missing} are not written"
    return (
        np.memmap(
            dirpath / "vectors.f16",
            dtype=np.float16,
            mode="r",
            shape=(meta["rows"], meta["dim"]),
        ),
        meta,
    )
