"""The filter-lifecycle campaign's build input: real arXiv attribute values, at two scales.

Produces one points file covering `entity_id < --total`, from which both scales are taken as
`--limit` prefixes (probes/dataset.md §5 rule 1: a scale is a prefix filter on the source entity
id, never a separate file). The build gets a prefix; the campaign's ingest draws its values from
the rows above it, so a flushed entity's value is as real as a built one's.

## The values are the corpus's, not a generator's

`data/corpus.parquet` carries 2,422,486 real arXiv items. The scaled corpus is that corpus
replicated 413 times with an affine geometric transform per replica, and entity ids are assigned
replica-block by replica-block (`probes/build_scaled_corpus.py`), so `entity_id % 2,422,486` is
the base item an entity is a replica of. Attribute values follow that identity: entity *e*
carries base item *e* mod 2,422,486's values. Every value is real and the marginal distribution
at 25,000,000 is the corpus's own, ten times over — which is what the campaign needs, since the
skew of `categories` and `surnames` is the thing being measured against.

What this does *not* reproduce is a larger corpus's *vocabulary growth*: replica 4 introduces no
category the base corpus lacks. Stated here rather than left to be inferred — a campaign claim
about value-set size at 25M would be measuring the replication rule, not the data.

## The four columns, and which route each exercises

| column | type | route |
|---|---|---|
| `archive` | category `u8`, `listing = "public"` | the derived postings (decision 0061) |
| `primary_category` | category `u16`, `listing = "per_viewer"` | the masked scan, and `/v1/categories`' membership |
| `secondary_category` | category `u16`, `listing = "public"` | **partial presence**, on real data |
| `first_author` | `utf8` | `eq`, `prefix`, `contains` |
| `submitted_at` | `i64` | `range`, open and closed |

`secondary_category` is the second listed arXiv category, which roughly half this corpus does not
have. It is here because every other column in this corpus is universally present, and partial
presence is a *different* addressing path through all three write paths under test — the affine
rank inside a run, the coalesce's presence-union guard, and the fold's presence normalisation.
Without it the campaign would exercise the dense case only and could not say so.

All four are `used_for = ["filter"]` alone: the campaign measures the entity-space index, and a
`render` placement would put the same data in the hot column and price the build's row space
instead.

Codes are pinned by sorted key order from code 1 (0 is the reserved absent sentinel), so a
regenerated vocabulary file is byte-identical and no build silently recolours the corpus.

Usage:
    reference/.venv/bin/python probes/2026-08-10-filter-lifecycle/make_fixture.py \
        --data /home/joe/code/tessera/data --total 25200000
"""

import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

BASE = 2_422_486

ap = argparse.ArgumentParser()
ap.add_argument("--data", required=True, help="the data/ directory (gitignored, main checkout)")
ap.add_argument("--total", type=int, default=25_200_000, help="rows to emit: entity_id < total")
ap.add_argument("--out", default=None, help="output dir (default: <data>/filter-lifecycle)")
# A smoke run: stop after this many geometry batches and skip the row-count check. Exists so the
# script's logic is validated in seconds rather than after the full scan of a 10.4 GB file.
ap.add_argument("--smoke-batches", type=int, default=0)
args = ap.parse_args()

DATA = Path(args.data)
OUT = Path(args.out) if args.out else DATA / "filter-lifecycle"
OUT.mkdir(parents=True, exist_ok=True)
TOTAL = args.total


def log(m):
    print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)


# ---------------------------------------------------------------------------- the base values
log("reading corpus.parquet")
corpus = pq.read_table(
    DATA / "corpus.parquet", columns=["entity_id", "categories", "surnames", "v1_created"]
)
if corpus.num_rows != BASE:
    sys.exit(f"corpus.parquet has {corpus.num_rows:,} rows, expected {BASE:,}")
eid = corpus.column("entity_id").to_numpy(zero_copy_only=False)
if not np.array_equal(np.sort(eid), np.arange(BASE)):
    sys.exit("corpus.parquet's entity ids are not 0..BASE-1")
order = np.argsort(eid)

categories = corpus.column("categories").to_pylist()
surnames = corpus.column("surnames").to_pylist()
stamps = corpus.column("v1_created").cast(pa.int64()).to_numpy(zero_copy_only=False)

# Reindex everything into base-item order, so index i is base item i.
categories = [categories[j] for j in order]
surnames = [surnames[j] for j in order]
stamps = stamps[order]

split = [c.split() if c else [] for c in categories]
primary = [c[0] if c else "" for c in split]
# The second listed category, absent for roughly half the corpus — the real partial-presence case.
secondary = [c[1] if len(c) > 1 else "" for c in split]
# arXiv's older archives (`hep-th`) have no dot, in which case the whole token is the archive.
archive = [p.split(".")[0] if p else "" for p in primary]
# The first listed author's surname. `None` where the item lists none — the absent case, real
# rather than constructed, and the one that makes presence partial on this column.
first_author = [(s[0] if s else None) for s in surnames]

log(f"archives={len(set(archive))} primary_categories={len(set(primary))} "
    f"authors_distinct={len(set(a for a in first_author if a)):,} "
    f"absent_author={sum(1 for a in first_author if not a):,}")


def vocabulary(keys, width_max, name):
    """key -> code, pinned by sorted key order from 1. Code 0 is the absent sentinel."""
    live = sorted(k for k in set(keys) if k)
    if len(live) > width_max:
        sys.exit(f"{name}: {len(live)} values exceeds the declared width's {width_max}")
    return {k: i + 1 for i, k in enumerate(live)}


archive_codes = vocabulary(archive, 255, "archive")
primary_codes = vocabulary(primary, 65535, "primary_category")
secondary_codes = vocabulary(secondary, 65535, "secondary_category")
log(f"archive: {len(archive_codes)} codes; primary_category: {len(primary_codes)} codes; "
    f"secondary_category: {len(secondary_codes)} codes, "
    f"absent for {sum(1 for s in secondary if not s):,}")

for name, codes in [("archive", archive_codes), ("primary_category", primary_codes),
                    ("secondary_category", secondary_codes)]:
    pq.write_table(
        pa.table({
            "key": pa.array(list(codes), pa.string()),
            "code": pa.array(list(codes.values()), pa.uint32()),
            "label": pa.array(list(codes), pa.string()),
        }),
        OUT / f"{name}.parquet",
    )

# Arrays in base-item order, ready to be gathered by `entity_id % BASE`.
archive_arr = np.array(archive, dtype=object)
primary_arr = np.array(primary, dtype=object)
secondary_arr = np.array([s if s else None for s in secondary], dtype=object)
author_arr = np.array([a if a else None for a in first_author], dtype=object)

# ---------------------------------------------------------------------------- the points file
log(f"scanning geometry.parquet for entity_id < {TOTAL:,}")
geo = pq.ParquetFile(DATA / "scaled" / "geometry.parquet")
schema = pa.schema([
    ("entity_id", pa.uint32()),
    ("morton", pa.uint32()),
    ("residual", pa.uint32()),
    ("archive", pa.string()),
    ("primary_category", pa.string()),
    ("secondary_category", pa.string()),
    ("first_author", pa.string()),
    ("submitted_at", pa.int64()),
])
writer = pq.ParquetWriter(OUT / "points.parquet", schema, compression="zstd")
kept = 0
for i, batch in enumerate(geo.iter_batches(batch_size=1 << 21,
                                           columns=["entity_id", "morton", "residual"])):
    e = batch.column("entity_id").to_numpy(zero_copy_only=False)
    keep = e < TOTAL
    if not keep.any():
        continue
    e = e[keep]
    base = (e % BASE).astype(np.int64)
    writer.write_table(pa.table({
        "entity_id": pa.array(e, pa.uint32()),
        "morton": pa.array(batch.column("morton").to_numpy(zero_copy_only=False)[keep], pa.uint32()),
        "residual": pa.array(batch.column("residual").to_numpy(zero_copy_only=False)[keep],
                             pa.uint32()),
        "archive": pa.array(archive_arr[base].tolist(), pa.string()),
        "primary_category": pa.array(primary_arr[base].tolist(), pa.string()),
        "secondary_category": pa.array(secondary_arr[base].tolist(), pa.string()),
        "first_author": pa.array(author_arr[base].tolist(), pa.string()),
        "submitted_at": pa.array(stamps[base], pa.int64()),
    }, schema=schema))
    kept += int(keep.sum())
    if i % 100 == 0:
        log(f"  batch {i}: {kept:,} kept")
    if args.smoke_batches and i + 1 >= args.smoke_batches:
        log("smoke run: stopping early")
        break
writer.close()
log(f"wrote points.parquet: {kept:,} rows")
if not args.smoke_batches and kept != TOTAL:
    sys.exit(f"expected {TOTAL:,} rows below the limit, found {kept:,}")

# ---------------------------------------------------------------------------- the declaration
(OUT / "schema.toml").write_text('''# The filter-lifecycle campaign's declaration (probes/2026-08-10-filter-lifecycle/).
#
# Four columns, one per route: a `public` category answered through its derived postings, a
# `per_viewer` category answered by the masked scan and serving `/v1/categories`' membership
# question, a string carrying eq/prefix/contains, and a numeric carrying range.
#
# `used_for = ["filter"]` alone throughout: this campaign measures the entity-space index, and a
# `render` placement would price the hot column instead.

[[attribute]]
name       = "archive"
type       = "category"
width      = "u8"
used_for   = ["filter"]
vocabulary = "declared"
values_key = "archive"
listing    = "public"

[[attribute]]
name       = "primary_category"
type       = "category"
width      = "u16"
used_for   = ["filter"]
vocabulary = "declared"
values_key = "primary_category"
listing    = "per_viewer"

[[attribute]]
name       = "secondary_category"
type       = "category"
width      = "u16"
used_for   = ["filter"]
vocabulary = "declared"
values_key = "secondary_category"
listing    = "public"

[[attribute]]
name     = "first_author"
type     = "utf8"
used_for = ["filter"]

[[attribute]]
name     = "submitted_at"
type     = "i64"
used_for = ["filter"]
''')

(OUT / "fixture.json").write_text(json.dumps({
    "total": TOTAL,
    "base_points": BASE,
    "archive_codes": len(archive_codes),
    "primary_codes": len(primary_codes),
    "secondary_codes": len(secondary_codes),
    "absent_secondary_category": int(sum(1 for s in secondary if not s)),
    "absent_first_author": int(sum(1 for a in first_author if not a)),
    "submitted_at_min": int(stamps.min()),
    "submitted_at_max": int(stamps.max()),
}, indent=2))
log("done")
