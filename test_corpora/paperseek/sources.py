"""Where rung 4's bytes are, and the two facts about them that every later step depends on.

**The publisher's own bytes are on the share and are read exactly once** (`../README.md`): 53
`chunk_N.parquet` files holding 102,117,343 OpenAlex works, each row an `id` (the full
`https://openalex.org/W…` URL), a `title`, an `abstract` and a 1024-dimensional `float32`
`embedding` stored as a `fixed_size_list`. 235.6 GB compressed, 550.6 GB uncompressed
(`../../probes/2026-09-02-rung-4-share-reads/`). `stage.py` makes the one pass and writes
everything the rest of the rung reads to the local ladder directory.

**Chunk order is row order, and it is numeric with gaps.** `entity_id` is the global row index over
the 53 chunks taken in ascending *numeric* chunk order — `chunk_0`, `chunk_1`, `chunk_2`, `chunk_4`,
… — and the numbering skips 3 and 45. A lexicographic directory listing puts `chunk_10` after
`chunk_1` and would hand every work another work's vector; `CHUNKS` is written out rather than
sorted at runtime, and the share is checked against it.

**The projected read is fast here and slow on `works`.** `id`, `title` and `abstract` are three
plain, non-nested string columns, so each is one contiguous range read per row group and SMB serves
it at ~99 MB/s — *above* the 67 MB/s sequential baseline. The embedding column is one large leaf and
reads at ~41 MB/s while its float decode runs. Both measured 2026-09-02; the OpenAlex track's own
scan of `works` is the read that is slow, and its numbers are not these.
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np

from ..common.paths import ladder, staged

DATASET, VINTAGE = "paperseek-openalex", "2026-08-27"

#: Stella V5 1.5B's width. The publisher's card says bfloat16 precision; the file is `float32`.
EMBED_DIM = 1024

#: The 53 chunks, in the order that defines `entity_id`. 3 and 45 do not exist.
CHUNKS = tuple([0, 1, 2] + list(range(4, 45)) + list(range(46, 55)))

#: Counted from the 53 footers, 2026-09-02 (`probes/2026-09-02-rung-4-share-reads/`). Asserted
#: rather than trusted: `chunk_offsets` re-reads the footers and refuses a different total.
ROWS = 102_117_343

RUNG = "paperseek"

#: Every id on both sides of the OpenAlex join is the full URL. The staged parquet keeps the `W…`
#: part alone — 11 bytes against 32 over 10⁸ rows — and `prepare.py` puts the prefix back a slice at
#: a time for `OpenAlex.resolve`, which wants the spelling the extract carries.
ID_PREFIX = "https://openalex.org/"


def share() -> Path:
    """The staged acquisition. Never built or served from — read once by `stage.py`."""
    return staged(DATASET, VINTAGE)


def staging() -> Path:
    """`$TESSERA_LADDER/paperseek/staging/`, the local copy everything after `stage.py` reads.

    **It belongs to the rung, not to a run**, exactly as rung 3's does: `prepare.py --out` moves
    where a run writes its corpus, and every run reads the same staged bytes.
    """
    path = ladder(RUNG) / "staging"
    path.mkdir(parents=True, exist_ok=True)
    return path


def chunk_path(share_dir: Path, n: int) -> Path:
    return share_dir / f"chunk_{n}.parquet"


def chunk_offsets(share_dir: Path) -> tuple[list[int], list[int]]:
    """`(rows per chunk, global offset of each chunk)`, from the 53 footers.

    A footer is 5–60 ms over SMB, so this is seconds rather than the hours a row count from the
    data would be. The embedding column's shape and type are checked here because a chunk of
    another width would be written into the flat matrix at the wrong stride.
    """
    import pyarrow as pa
    import pyarrow.parquet as pq

    rows = []
    for n in CHUNKS:
        path = chunk_path(share_dir, n)
        f = pq.ParquetFile(path)
        field = f.schema_arrow.field("embedding")
        assert pa.types.is_fixed_size_list(field.type), f"chunk {n}: embedding is {field.type}"
        assert field.type.list_size == EMBED_DIM, f"chunk {n} is {field.type.list_size}-dimensional"
        assert pa.types.is_float32(field.type.value_type), f"chunk {n} is {field.type.value_type}"
        rows.append(int(f.metadata.num_rows))
    offsets, at = [], 0
    for r in rows:
        offsets.append(at)
        at += r
    assert at == ROWS, f"the share holds {at:,} rows against the recorded {ROWS:,}"
    return rows, offsets


def staged_rows(meta: dict) -> int:
    """Rows staged contiguously from row 0 — how much of the matrix a partial run may read.

    A chunk that is not written is a hole of zeros in a sparse file rather than a short one, so
    nothing may read past the first hole.
    """
    at = 0
    for n in CHUNKS:
        held = meta["chunks"].get(str(n))
        if held is None or held["offset"] != at:
            break
        at += held["rows"]
    return at


def vectors(*, complete: bool = True) -> tuple[np.memmap, dict]:
    """The staged matrix and its sidecar, opened read-only.

    **Refuses a partial matrix by default**, and refuses an absent one with the reason: 209 GB of
    `vectors.f16` is deleted by `prepare.py --drop-vectors` once the layout is written, because
    nothing after the layout reads it. A later run that wants a new layout stages again.
    """
    dirpath = staging()
    meta = json.loads((dirpath / "vectors.json").read_text())
    path = dirpath / "vectors.f16"
    assert path.exists(), (
        f"{path} is not there. It is 209 GB and `prepare.py --drop-vectors` deletes it once the "
        f"layout is written; a run that needs the vectors again must re-stage."
    )
    missing = [n for n in CHUNKS if str(n) not in meta["chunks"]]
    if complete:
        assert not missing, f"staging is incomplete: chunks {missing} are not written"
    return (
        np.memmap(path, dtype=np.float16, mode="r", shape=(meta["rows"], meta["dim"])),
        meta,
    )
