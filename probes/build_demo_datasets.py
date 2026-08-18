"""The demo bundles' build inputs: the same corpus at four scales, with prose to search.

`run_demo.sh` serves what this writes. The demo exists to show the filter surface, so every column
here is one a control can be drawn for — a category with a value set, a date with a range, and
prose with a word search — and nothing here is present only because the corpus had it.

## The four scales, and why they carry different columns

| | items | prose |
|---|---|---|
| `2m4` | 2,422,486 — the real corpus | `title` **and** `abstract` |
| `25m` | 25,200,000 — the corpus 10.4x over | `title` only |
| `250m` | 250,000,000 — 103x over | none |
| `1b` | 1,000,000,000 — the design's target | none |

Abstracts stop at the small scale for a measured reason rather than a cautious one. An abstract is
954 characters against a title's 73, so the record blob holding 25,200,000 of them is ~26 GB, and
the index over them a further ~4 GB at `probes/2026-08-13-text-index-bytes`' measured 158 B/entity.
Titles at that scale are ~2.3 GB and ~0.6 GB. The viewer draws its filter controls from
`/v1/meta`'s `filter_operands`, so the abstract box is simply absent on the large bundle — the
honest rendering of a column that is not there, and not a case the client special-cases.

**Titles stop at 25,200,000, and that limit is the build's memory rather than the disk.** A 10⁹
bundle carrying titles would be ~107 GB, which is only a disk question; what stops it is that both
consumers of a text column — the record blob and the token index — read it from an entity-major
array that is held whole. At a measured mean of 73 characters that is ~24 GB of strings for
250,000,000 titles, before anything else the build holds, and the build is OOM-killed in the
attribute pass. The fixed-width columns are no longer the problem: typing that intermediate
(`EntityColumn`) took them from 32 B per value to the declared width, which is what lets these two
scales build at all. Text did not benefit, because its cost is the strings and not the tag around
them.

The points file still carries `title` at every scale, so this declaration is the only thing between
these bundles and prose search: streaming the text column to the blob and the index, rather than
materialising it, makes the column declarable here with no other change.

## The values are the corpus's

`probes/dataset.md` §4.3: the scaled corpus is the real 2,422,486 papers repeated as affine
transforms of their geometry, so `entity_id % 2,422,486` is the base paper an entity is a replica
of, and every value written here is that paper's own. This is `make_fixture.py`'s rule and this
file inherits its caveat verbatim: the marginal distribution at any scale is the corpus's own,
repeated, but the *vocabulary* does not grow — no replica introduces a category or a title word the
base corpus lacks. A claim about term-count growth at scale may not be read off this, and the claim
gets harder to resist the further the scales run: 10⁹ items still carry the 2,422,486 corpus's
distinct titles, 413 times each.

## Codes are pinned, and shared across every scale

`archive` and `primary_category` codes come from sorted key order from 1 (0 being the reserved
absent sentinel), written once and used by every build. Two bundles that disagreed about which code
`hep-th` is would recolour the map on a dataset switch, which reads as data changing rather than as
a fixture changing.

Usage:
    reference/.venv/bin/python probes/build_demo_datasets.py --data data [--scale 2m4|25m|250m|1b]
"""

import argparse
import json
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

BASE = 2_422_486
SCALES = {"2m4": 2_422_486, "25m": 25_200_000,
          "250m": 250_000_000, "1b": 1_000_000_000}

ap = argparse.ArgumentParser()
ap.add_argument("--data", default="data", help="the data/ directory (gitignored)")
ap.add_argument("--scale", action="append", choices=sorted(SCALES), default=None)
# 1M rather than a larger group because of the abstracts: at a 954-character mean, a batch of 2M
# lands within 12% of `string`'s 2 GB offset ceiling, and the margin is not worth the row groups.
ap.add_argument("--batch", type=int, default=1_000_000, help="rows per output row group")
args = ap.parse_args()

DATA = Path(args.data)
OUT = DATA / "demo"
OUT.mkdir(parents=True, exist_ok=True)
scales = args.scale or list(SCALES)


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


# ----------------------------------------------------------------- the base values, in entity order
log("reading corpus.parquet and prose.parquet")
corpus = pq.read_table(DATA / "corpus.parquet", columns=["entity_id", "categories", "surnames",
                                                         "v1_created"])
assert corpus.num_rows == BASE, f"corpus.parquet has {corpus.num_rows:,} rows, expected {BASE:,}"
order = pc.sort_indices(corpus.column("entity_id"))
corpus = corpus.take(order)

# **`abstract` is read as `large_string`, and it has to be.** 2,422,486 abstracts at a measured mean
# of 954 characters is ~2.3 GB in one array, past the 2^31 ceiling on `string`'s 32-bit offsets —
# `combine_chunks` refuses outright rather than truncating, which is the good failure. The write path
# below casts each batch back down, where the row count keeps it under the limit.
prose = pq.read_table(
    OUT / "prose.parquet",
    columns=["entity_id", "title", "abstract"],
    schema=pa.schema([("entity_id", pa.uint32()), ("title", pa.string()),
                      ("abstract", pa.large_string())]),
)
assert prose.num_rows == BASE, f"prose.parquet has {prose.num_rows:,} rows — run build_prose.py"
# Asserted rather than sorted: `build_prose.py` emits in entity order, and a `take` here to
# re-establish an order the file already has would build a second 2.3 GB copy of the abstracts.
assert pc.all(pc.equal(prose.column("entity_id"), corpus.column("entity_id"))).as_py(), \
    "prose.parquet and corpus.parquet disagree about the entity space or its order"

# `combine_chunks` once, here: every scale below `take`s from these, and a chunked array makes each
# take walk its chunk index per batch.
title_base = prose.column("title").combine_chunks()
abstract_base = prose.column("abstract").combine_chunks()
stamps = corpus.column("v1_created").cast(pa.int64()).to_numpy(zero_copy_only=False)

categories = corpus.column("categories").to_pylist()
split = [c.split() if c else [] for c in categories]
primary = [c[0] if c else "" for c in split]
# arXiv's older archives (`hep-th`) carry no dot, in which case the whole token is the archive.
archive = [p.split(".")[0] if p else "" for p in primary]
# Author count is a genuine small-integer column with a long tail (one paper lists 2,832 authors),
# which is what makes it worth keeping: it is the demo's only numeric range that is not a date.
author_count = np.array([min(len(s), 255) if s else 0 for s in corpus.column("surnames").to_pylist()],
                        dtype=np.uint8)


def vocabulary(keys, width_max, name):
    """key -> code, pinned by sorted key order from 1. Code 0 is the reserved absent sentinel."""
    live = sorted(k for k in set(keys) if k)
    assert len(live) <= width_max, f"{name}: {len(live)} values exceeds the declared width"
    return {k: i + 1 for i, k in enumerate(live)}


archive_codes = vocabulary(archive, 255, "archive")
primary_codes = vocabulary(primary, 65535, "primary_category")
log(f"archive: {len(archive_codes)} codes; primary_category: {len(primary_codes)} codes")

for name, codes in [("archive", archive_codes), ("primary_category", primary_codes)]:
    pq.write_table(
        pa.table({
            "key": pa.array(list(codes), pa.string()),
            "code": pa.array(list(codes.values()), pa.uint32()),
            "label": pa.array(list(codes), pa.string()),
        }),
        OUT / f"{name}.parquet",
    )

archive_base = pa.array(archive, pa.string())
primary_base = pa.array(primary, pa.string())


# ----------------------------------------------------------------------------- the schema per scale
# `str.format` fills `{scale}` and `{rows}` below, so every literal brace in this template is
# doubled — an inline table's braces included.
SCHEMA_HEAD = '''# The demo bundle's declaration — {scale}, {rows:,} items. Generated by
# `probes/build_demo_datasets.py`; edit that, not this.
#
# Every column is one the viewer can draw a control for. The two categories carry both homes:
# `render` puts the code in the hot column so a mark can be coloured by it, and `index` builds the
# derived postings so the same predicate is answered in entity space rather than by scanning the
# request's rows. Declaring both is what lets decision 0068 route on cost.

# Every source is a path **relative to this file**, which sits in `data/demo/` beside the files it
# names (configuration.md §3). The points file carries identity, geometry and every declared
# column, so `[corpus]` and the view name one file; the exploded `(entity_id, term_id)` relation is
# the shared one two directories over, and the two vocabularies name their own.
#
# `extent`: the points file stores Morton codes rather than coordinates, and codes are exact only
# against the grid's own extent (contracts §2.5).
[corpus]
source = "points-{scale}.parquet"

[[view]]
name             = "s0"
title            = "arXiv"
extent           = {{ min = 0.0, max = 65536.0 }}
source           = "points-{scale}.parquet"
point_visibility = {{ source = "../scaled/pairs/categories-subclass.pairs.parquet", default = "public" }}

# `visibility = "public"`: arXiv's archive names are published taxonomy, so their existence
# discloses nothing about this corpus's contents. It is safe here because the set is `closed` —
# authored rather than inferred from whatever the corpus happens to hold.
[[vocabulary]]
name       = "archive"
title      = "Archive"
width      = "u8"
value_set  = "closed"
visibility = "public"
source     = "archive.parquet"

[[vocabulary]]
name       = "primary_category"
title      = "Primary category"
width      = "u16"
value_set  = "closed"
visibility = "public"
source     = "primary_category.parquet"

[[attribute]]
name       = "archive"
type       = "category"
render     = true
index      = true
vocabulary = "archive"

[[attribute]]
name       = "primary_category"
type       = "category"
render     = true
index      = true
vocabulary = "primary_category"

# `timestamp_us` rather than `i64`: the unit is then a fact the client reads off `/v1/meta` instead
# of a convention it has to be told, which is what lets the viewer show a date picker and a date
# axis without a per-column rule of its own.
[[attribute]]
name   = "submitted_at"
type   = "timestamp_us"
render = true

[[attribute]]
name   = "author_count"
type   = "u8"
render = true

'''

# Prose. `render` on a text column is refused at the declaration — the hot column is a fixed-width
# slot per row and prose is not — so a text column's value lives in the record blob and reaches a
# client at drill-down. `index` is what builds the token index that answers `match` and `phrase`.
SCHEMA_TITLE = '''
[[attribute]]
name   = "title"
type   = "text"
index  = true
'''

SCHEMA_ABSTRACT = '''
[[attribute]]
name   = "abstract"
type   = "text"
index  = true
'''


# ------------------------------------------------------------------------------ the points per scale
POINTS_SCHEMA = [
    ("entity_id", pa.uint32()),
    ("morton", pa.uint32()),
    ("residual", pa.uint32()),
    ("archive", pa.string()),
    ("primary_category", pa.string()),
    ("submitted_at", pa.int64()),
    ("author_count", pa.uint8()),
    ("title", pa.string()),
]


def write_points(scale: str, total: int) -> None:
    """One points file, streamed from the scaled geometry in entity batches."""
    with_abstract = scale == "2m4"
    # **Prose stops at 25,200,000, and the reason is the build rather than the disk.** The record
    # blob and the token index both read the text column from an entity-major array held whole, so
    # a text column costs its own bytes in RAM — ~24 GB for 250,000,000 titles at a measured mean
    # of 73 characters — on top of everything else the build holds. The fixed-width columns became
    # cheap when the entity-major intermediate was typed (`EntityColumn`); text did not, because
    # its cost is the strings themselves and not the tag around them. The points file still carries
    # `title` at every scale, so declaring it here is all that stands between these bundles and
    # prose search once the text column is streamed rather than materialised.
    with_title = scale in ("2m4", "25m")
    fields = list(POINTS_SCHEMA) + ([("abstract", pa.string())] if with_abstract else [])
    schema = pa.schema(fields)
    out = OUT / f"points-{scale}.parquet"
    log(f"{scale}: scanning geometry.parquet for entity_id < {total:,} -> {out.name}")

    geo = pq.ParquetFile(DATA / "scaled" / "geometry.parquet")
    writer = pq.ParquetWriter(out, schema, compression="zstd")
    kept = 0
    for i, batch in enumerate(geo.iter_batches(batch_size=args.batch,
                                               columns=["entity_id", "morton", "residual"])):
        e = batch.column("entity_id").to_numpy(zero_copy_only=False)
        keep = e < total
        if not keep.any():
            continue
        e = e[keep]
        # The base paper each entity is a replica of. `take` rather than a Python-level gather:
        # at 25.2M rows the list round-trip allocates one Python string per row per column.
        base = pa.array((e % BASE).astype(np.int64))
        columns = {
            "entity_id": pa.array(e, pa.uint32()),
            "morton": pa.array(batch.column("morton").to_numpy(zero_copy_only=False)[keep],
                               pa.uint32()),
            "residual": pa.array(batch.column("residual").to_numpy(zero_copy_only=False)[keep],
                                 pa.uint32()),
            "archive": archive_base.take(base),
            "primary_category": primary_base.take(base),
            "submitted_at": pa.array(stamps[e % BASE], pa.int64()),
            "author_count": pa.array(author_count[e % BASE], pa.uint8()),
            "title": title_base.take(base),
        }
        if with_abstract:
            # Back to 32-bit offsets for the output file: `--batch` rows of abstract is under the
            # ceiling that forced `large_string` on the whole-corpus array above, and the build
            # reads a plain `string` column.
            columns["abstract"] = abstract_base.take(base).cast(pa.string())
        writer.write_table(pa.table(columns, schema=schema))
        kept += int(keep.sum())
        if i % 20 == 0:
            log(f"  {scale}: {kept:,} rows")
    writer.close()
    assert kept == total, f"{scale}: expected {total:,} rows below the limit, found {kept:,}"
    log(f"{scale}: wrote {kept:,} rows, {out.stat().st_size / 1e9:.2f} GB")

    (OUT / f"config-{scale}.toml").write_text(
        SCHEMA_HEAD.format(scale=scale, rows=total)
        + (SCHEMA_TITLE if with_title else "")
        + (SCHEMA_ABSTRACT if with_abstract else "")
    )
    (OUT / f"{scale}.json").write_text(json.dumps({
        "scale": scale,
        "items": total,
        "prose": (["title", "abstract"] if with_abstract else ["title"] if with_title else []),
        "archive_codes": len(archive_codes),
        "primary_codes": len(primary_codes),
        "submitted_at_min": int(stamps.min()),
        "submitted_at_max": int(stamps.max()),
    }, indent=2) + "\n")


for scale in scales:
    write_points(scale, SCALES[scale])
log("done")
