"""Where rung 5's two staged sources are, and the constants both scripts share.

**TreeOfLife-200M** is 666 files, `train-NNNNN-of-00666.parquet`, sorted globally by
`source_dataset > kingdom > phylum > class > order > family > genus > species > common_name`
(`README-upstream.md` §schema) — so a source or a rank is a contiguous run of files, not scattered
across all 666, but no file is purely one source or one rank near a boundary. 350,000 rows a file
bar the last, 233,055,986 rows total (interface.md). `entity_id` is the global row index in file
order, which is why the file order below is numeric on the zero-padded index and not the
directory's own lexicographic listing (`train-00010` before `train-00002` would misnumber every
row after it).

**GBIF** is 8,369 parts, `occurrence.parquet/NNNNNN` with no suffix, ~212k rows each, one row group
apiece, `gbifid` a plain digit-string column (`probes/2026-09-02-rung-4-share-reads/README.md`
measured the sibling OpenAlex join's cost model — small projected columns over SMB, not a whole-file
read).
"""

from __future__ import annotations

from pathlib import Path

from ..common.paths import ladder, staged

RUNG = "treeoflife"

TREEOFLIFE_DATASET, TREEOFLIFE_VINTAGE = "treeoflife-200m", "2026-08-27"
GBIF_DATASET, GBIF_VINTAGE = "gbif", "2026-06-01"

#: 233,055,986 across 666 files, 350,000 rows each bar the last (interface.md).
FILE_COUNT = 666
ROWS_PER_FILE = 350_000
TOTAL_ROWS = 233_055_986

#: Every column but `emb` — the vector track's job, not this one's.
METADATA_COLUMNS = [
    "uuid",
    "source_dataset",
    "source_id",
    "kingdom",
    "phylum",
    "class",
    "order",
    "family",
    "genus",
    "species",
    "scientific_name",
    "common_name",
    "publisher",
    "basisOfRecord",
    "identifier",
    "img_type",
]

#: The seven taxonomic ranks, kingdom to species — what the tiered layer walks.
RANKS = ["kingdom", "phylum", "class", "order", "family", "genus", "species"]

#: Columns worth dictionary encoding: everything but the near-unique identifiers.
DICTIONARY_COLUMNS = [
    "source_dataset",
    *RANKS,
    "scientific_name",
    "publisher",
    "basisOfRecord",
    "img_type",
]

#: What the GBIF join reads off the occurrence snapshot.
GBIF_COLUMNS = [
    "gbifid",
    "decimallatitude",
    "decimallongitude",
    "coordinateuncertaintyinmeters",
    "license",
]


def treeoflife_files() -> list[Path]:
    """The 666 files, in `entity_id` order."""
    base = staged(TREEOFLIFE_DATASET, TREEOFLIFE_VINTAGE) / "bioclip-2_float16"
    files = [base / f"train-{i:05d}-of-{FILE_COUNT:05d}.parquet" for i in range(FILE_COUNT)]
    missing = [f for f in files if not f.exists()]
    assert not missing, f"{len(missing)} of {FILE_COUNT} expected files missing, e.g. {missing[0]}"
    return files


def gbif_parts() -> list[Path]:
    """The 8,369 GBIF parts, in directory order (the id has no meaning as a sort key)."""
    base = staged(GBIF_DATASET, GBIF_VINTAGE) / "occurrence.parquet"
    return sorted(p for p in base.iterdir() if p.is_file())


def rung() -> Path:
    return ladder(RUNG)


def staging() -> Path:
    path = rung() / "staging"
    path.mkdir(parents=True, exist_ok=True)
    return path


#: The rows UMAP is fitted over, staged by `stage.py --fit` and read by `routes.py`. 2,500,000 x
#: 768 float16 is 3.84 GB — rung 3's fit size at rung 3's width, and the size `README.md` records a
#: measurement for.
FIT_ROWS = 2_500_000

#: BioCLIP-2's width.
EMBED_DIM = 768


def fit_matrix():
    """The staged fit sample and its sidecar: `(memmap (rows, 768) float16, global row ids, meta)`.

    **Refuses a partial sample rather than reading its zeros.** The memmap is written by 666
    workers into their own slots, so an interrupted pass leaves holes rather than a short file.
    """
    import json

    import numpy as np

    dirpath = staging()
    meta = json.loads((dirpath / "fit.json").read_text())
    assert meta["complete"], (
        f"{dirpath / 'fit.f16'} is a partial sample: {meta['files_done']} of {FILE_COUNT} files. "
        f"Finish `stage.py --fit` — it is resumable per file."
    )
    matrix = np.memmap(dirpath / "fit.f16", dtype=np.float16, mode="r",
                       shape=(meta["rows"], meta["dim"]))
    rows = np.load(dirpath / "fit-rows.npy")
    return matrix, rows, meta
