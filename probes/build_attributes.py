"""Build the 2.4M points file carrying a declared attribute tail — `data/scaled/attrs/`.

The benchmark fixtures have never carried a per-item column: `tessera-build` wrote
`declared_scalars` empty unconditionally, so per-point-attributes §8's arms ("the fixtures carry
no attribute tail today, so no arm can see any of this") had nothing to measure. This produces
the first points file that does.

## Scope: replica 0 only

`entity_id < 2,422,486` is the real arXiv corpus entire (dataset.md §4.1) and the one scale whose
attributes are *real* rather than synthesised — the replicas above it are affine transforms of the
same geometry, and `corpus.parquet` has no rows for them. A wider scale would have to invent
attribute values for 99.8% of its items, which measures the generator rather than the corpus. The
larger scales stay attribute-free until there is a reason to fabricate a tail for them.

## The columns, and why these

Five columns spanning four widths, because the tail is written and read back **positionally**: a
schema whose columns are all one width cannot show a positional slip, since every value lands
somewhere legal. Different widths make a slip a type mismatch the readers refuse.

| column | type | source | distinct |
|---|---|---|---|
| `archive` | category `u8` | the primary category's archive (`hep-th`, `cs`, …) | ~20 |
| `primary_category` | category `u16` | the first listed category (`cs.LG`, …) | ~176 |
| `author_count` | `u8` | `len(surnames)`, saturating at 255 | — |
| `submitted_at` | `i64` | `v1_created` in microseconds since the epoch | — |
| `recency` | `f32` | **derived**: years from the corpus's first submission | — |

`recency` is the one value not present in the source. It exists so the `f32` column path is
exercised by a fixture at all; it is a deterministic function of `submitted_at` and carries no
information the timestamp does not. Stated here rather than left to be inferred, per the house
rule on distinguishing measured from derived.

`author_count` **saturates** at 255 rather than wrapping or widening. A `u8` is what §3.6 argues
for on a value whose domain is closed by nature, and 255 authors is the tail of a distribution
whose median is 3 — but saturation is a lossy choice and is made here, visibly, rather than
inside the build where it would be a silent truncation. The build itself refuses an out-of-range
value (`input::narrow`), which is why this script must do the clamping explicitly.

## Codes are pinned here and never re-derived

`columns.arrow` stores the code, not the key, so a build that re-derived codes from a re-supplied
vocabulary file would silently recolour the whole corpus with no error and no digest mismatch
(§3.4). Codes are therefore assigned **by sorted key order** — deterministic from the corpus
alone, so a regenerated vocabulary file is byte-identical — and written into a vocabulary parquet
that the build compiles into `MANIFEST.vocabularies`. The manifest is the durable mapping; this
file is its source, and the two disagreeing is a manifest digest change rather than a silent
recolouring.

Code 0 is the reserved *absent* sentinel and is never assigned, so codes start at 1.

Usage:
    reference/.venv/bin/python probes/build_attributes.py [--data=PATH]

`--data` names the `data/` directory, which is gitignored and lives in the main checkout — pass it
when running from a worktree.

Outputs, under `data/scaled/attrs/`:
    points.parquet            entity_id, morton, residual + the five columns
    archive.parquet           (key, code, label) — bound as `--values archive=…`
    primary_category.parquet  (key, code, label)
    schema.toml               the declaration `tessera build --schema` takes
"""

import sys
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

ROOT = Path(__file__).resolve().parent.parent
# `data/` is gitignored and exists only in the main checkout, so a run from a worktree must be
# told where it is. Defaults to this script's own repo root, which is the ordinary case.
DATA = Path(
    next(
        (a.split("=", 1)[1] for a in sys.argv[1:] if a.startswith("--data=")),
        ROOT / "data",
    )
)
SCALED = DATA / "scaled" / "geometry.parquet"
CORPUS = DATA / "corpus.parquet"
OUT = DATA / "scaled" / "attrs"

# Replica 0: the real arXiv corpus entire (dataset.md §4.1).
LIMIT = 2_422_486

# §3.6's usable ceilings. Code 0 is the absent sentinel, so each width carries one fewer value
# than its range. Checked rather than assumed: a vocabulary that outgrows its width is a rebuild
# at a wider declaration, and discovering that at build time wastes the build.
U8_MAX_CODE = 255
U16_MAX_CODE = 65535


def log(message):
    print(message, flush=True)


def read_geometry():
    """`(entity_id, morton, residual)` for the replica-0 prefix, in file order.

    A full scan of a 10.4 GB file, and unavoidable: `geometry.parquet` is sorted by
    `(morton, entity_id)` (dataset.md §4.2), so the entity prefix is scattered across every row
    group and no statistic prunes it. Projected to three columns and consumed in batches, so the
    resident set is the *output* (2.4M rows), not the input.
    """
    f = pq.ParquetFile(SCALED)
    log(f"scanning {SCALED.name}: {f.metadata.num_rows:,} rows, "
        f"{f.metadata.num_row_groups} row groups")
    ids, mortons, residuals = [], [], []
    kept = 0
    for i, batch in enumerate(
        f.iter_batches(batch_size=1 << 20, columns=["entity_id", "morton", "residual"])
    ):
        eid = batch.column("entity_id").to_numpy(zero_copy_only=False)
        keep = eid < LIMIT
        if keep.any():
            ids.append(eid[keep])
            mortons.append(batch.column("morton").to_numpy(zero_copy_only=False)[keep])
            residuals.append(batch.column("residual").to_numpy(zero_copy_only=False)[keep])
            kept += int(keep.sum())
        if i % 200 == 0:
            log(f"  batch {i}: {kept:,} kept")
    if kept != LIMIT:
        sys.exit(f"expected {LIMIT:,} replica-0 rows, found {kept:,} — the corpus moved")
    return (
        np.concatenate(ids),
        np.concatenate(mortons),
        np.concatenate(residuals),
    )


def read_corpus():
    """`entity_id -> (primary category, author count, v1_created)` over the whole corpus."""
    table = pq.read_table(CORPUS, columns=["entity_id", "categories", "surnames", "v1_created"])
    log(f"read {CORPUS.name}: {table.num_rows:,} rows")
    eid = table.column("entity_id").to_numpy(zero_copy_only=False)
    if eid.max() + 1 != LIMIT or table.num_rows != LIMIT:
        sys.exit(f"{CORPUS.name} covers {table.num_rows:,} rows to id {eid.max()}, expected {LIMIT:,}")

    categories = table.column("categories").to_pylist()
    surnames = table.column("surnames").to_pylist()
    # `timestamp[us]` — microseconds since the epoch, which is the `i64` the column declares.
    stamps = table.column("v1_created").cast(pa.int64()).to_numpy(zero_copy_only=False)

    # `categories` is a space-separated list, most-relevant first; the primary category is the
    # first. An empty string cannot occur in this corpus (the join required a category), but it is
    # mapped to the absent sentinel rather than assumed away.
    primary = [c.split()[0] if c else "" for c in categories]
    # The archive is the part before the dot; the older archives (`hep-th`, `math-ph`) have none,
    # in which case the whole token is the archive.
    archive = [p.split(".")[0] if p else "" for p in primary]
    authors = np.array([min(len(s or []), 255) for s in surnames], dtype=np.uint8)

    order = np.argsort(eid)
    return (
        [primary[i] for i in order],
        [archive[i] for i in order],
        authors[order],
        stamps[order],
    )


def vocabulary(keys, name, max_code):
    """Assign codes to the distinct non-empty `keys`, in sorted order, starting at 1.

    Sorted rather than first-seen: first-seen order depends on the corpus's row order, so a
    re-sorted source file would reassign every code — the exact hazard §3.4 closes by pinning.
    Sorted order depends on the key set alone.
    """
    distinct = sorted({k for k in keys if k})
    if len(distinct) > max_code:
        sys.exit(
            f"{name}: {len(distinct)} distinct values exceeds the declared width's {max_code} "
            f"usable codes. Widen the declaration in schema.toml (a rebuild, per §2.2)"
        )
    codes = {key: i + 1 for i, key in enumerate(distinct)}
    log(f"{name}: {len(distinct)} distinct values, codes 1..{len(distinct)}")
    return codes


def write_vocabulary(path, codes):
    keys = sorted(codes)
    table = pa.table(
        {
            "key": pa.array(keys, pa.string()),
            "code": pa.array([codes[k] for k in keys], pa.uint32()),
            # The label is the key here — arXiv category tokens are already the published display
            # names. It is written anyway so the column exists in the artifact the build reads,
            # and so a deployment that wants friendlier names has somewhere to put them without a
            # format change.
            "label": pa.array(keys, pa.string()),
        }
    )
    pq.write_table(table, path, compression="zstd")
    log(f"wrote {path} ({len(keys)} values)")


SCHEMA_TOML = """\
# The 2.4M arXiv fixture's per-item columns (per-point-attributes §4.2).
#
# Generated by `probes/build_attributes.py` alongside the vocabulary files it binds. Codes are
# pinned in those files and compiled into `MANIFEST.vocabularies`; nothing re-derives them.
#
# Build with:
#   tessera build --points data/scaled/attrs/points.parquet \\
#                 --pairs  data/scaled/pairs/<set>.pairs.parquet \\
#                 --schema data/scaled/attrs/schema.toml \\
#                 --values archive=data/scaled/attrs/archive.parquet \\
#                 --values primary_category=data/scaled/attrs/primary_category.parquet \\
#                 --extent 0,65536,0,65536 --slice s0 --limit 2422486 ...
#
# Residency, which `tessera build` also reports: 16 B/row over the 12 B fixed row (+133%).

[[attribute]]
name       = "archive"
type       = "category"
width      = "u8"
used_for   = ["render"]
vocabulary = "declared"
values_key = "archive"
# `public`: arXiv's archive names are published taxonomy, so their *existence* discloses nothing
# about this corpus's contents. `public` additionally requires `declared` (§3.8), which it is.
listing    = "public"

[[attribute]]
name       = "primary_category"
type       = "category"
width      = "u16"
used_for   = ["render"]
vocabulary = "declared"
values_key = "primary_category"
listing    = "public"

[[attribute]]
name     = "author_count"
type     = "u8"
used_for = ["render"]

[[attribute]]
name     = "submitted_at"
type     = "i64"
used_for = ["render"]

[[attribute]]
name     = "recency"
type     = "f32"
used_for = ["render"]
"""


def main():
    for path in (SCALED, CORPUS):
        if not path.exists():
            sys.exit(f"missing {path} — see probes/dataset.md")
    OUT.mkdir(parents=True, exist_ok=True)

    ids, mortons, residuals = read_geometry()
    primary, archive, authors, stamps = read_corpus()

    archive_codes = vocabulary(archive, "archive", U8_MAX_CODE)
    primary_codes = vocabulary(primary, "primary_category", U16_MAX_CODE)
    write_vocabulary(OUT / "archive.parquet", archive_codes)
    write_vocabulary(OUT / "primary_category.parquet", primary_codes)

    # Derived, not sourced — see the module docstring. Years from the earliest submission, which
    # makes the value independent of when this script runs.
    epoch = stamps.min()
    recency = ((stamps - epoch) / (365.25 * 24 * 3600 * 1_000_000)).astype(np.float32)

    # The points file is emitted in the geometry file's own order, which is Morton order for this
    # prefix — the build sorts regardless, so this costs nothing and keeps the two passes over
    # this file (geometry, then attributes) reading the same layout.
    index = ids.astype(np.int64)
    table = pa.table(
        {
            "entity_id": pa.array(ids, pa.uint32()),
            "morton": pa.array(mortons, pa.uint32()),
            "residual": pa.array(residuals, pa.uint32()),
            # Categories travel as their **keys**, never as codes (§3.1): the code is assigned
            # once, in the vocabulary, and a points file carrying codes would be a second place
            # codes are decided.
            "archive": pa.array([archive[i] for i in index], pa.string()),
            "primary_category": pa.array([primary[i] for i in index], pa.string()),
            "author_count": pa.array(authors[index], pa.uint8()),
            "submitted_at": pa.array(stamps[index], pa.int64()),
            "recency": pa.array(recency[index], pa.float32()),
        }
    )
    points = OUT / "points.parquet"
    pq.write_table(table, points, compression="zstd", use_dictionary=False)
    log(f"wrote {points}: {table.num_rows:,} rows, "
        f"{points.stat().st_size / 1e6:.1f} MB")

    (OUT / "schema.toml").write_text(SCHEMA_TOML)
    log(f"wrote {OUT / 'schema.toml'}")


if __name__ == "__main__":
    main()
