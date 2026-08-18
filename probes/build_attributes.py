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

## Two fixtures, and why the second exists

**`points.parquet` (five columns) is the realistic one**; **`points-wide.parquet` (nineteen) is
the adversarial one.** They share a corpus and differ only in how much of the type system and how
many of its edges they touch, so a cost measured against both isolates *tail width* from
everything else about the corpus.

The wide fixture is not five columns padded to nineteen. Every column earns its place by
exercising something the lean one cannot:

| what it exercises | column |
|---|---|
| the **absent sentinel** — code 0, on real data | `secondary_category`, absent for a paper with one category |
| **`bool` bit-packing**, at a row count that ends mid-byte | `is_cross_listed`, `is_single_author` (2,422,486 = 8·302,810 + 6) |
| **signed** values, at four widths | `decade_offset`, `years_from_median`, `days_from_median`, `submitted_offset_us` |
| **`u32` above `u16::MAX`** | `entity_rank`, dense over 2.4M |
| **`f64` precision an `f32` cannot hold** | `exact_offset_days` |
| **`timestamp_us`**, at real instants | `submitted_at` |
| a **third** column sharing a vocabulary (§3.9) | `secondary_category` shares `primary_category`'s |
| a **discovered** vocabulary, minted at scale rather than declared | `category_suffix`, from data already loaded — no new source, no `values_key` |

Outputs, under `data/scaled/attrs/`:
    points.parquet            entity_id, morton, residual + the five lean columns
    points-wide.parquet       the same, + all nineteen
    archive.parquet           (key, code, label) — bound as `--values archive=…`
    primary_category.parquet  (key, code, label); `secondary_category` binds it too
    schema.toml               the lean declaration
    schema-wide.toml          the packed declaration
"""

import hashlib
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
    table = pq.read_table(
        CORPUS, columns=["entity_id", "id", "categories", "surnames", "v1_created"]
    )
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
    split = [c.split() if c else [] for c in categories]
    primary = [c[0] if c else "" for c in split]
    # **The second listed category, or `None`** — which becomes the reserved *absent* code 0 at
    # build. Roughly half this corpus has one category, so this is the one column that exercises
    # the sentinel against real data rather than a constructed row.
    secondary = [c[1] if len(c) > 1 else None for c in split]
    # The archive is the part before the dot; the older archives (`hep-th`, `math-ph`) have none,
    # in which case the whole token is the archive.
    archive = [p.split(".")[0] if p else "" for p in primary]
    # The sub-token after the dot (`cs.LG` -> `LG`); the older archives with no dot (`hep-th`) have
    # no sub-token, so the whole token stands in, matching `archive`'s own fallback. `primary` is
    # never empty in this corpus (see above), so `None` here is a defensive fallback rather than an
    # observed case — kept because the mint routine refuses an empty key outright (§3.4).
    suffix = [(p.split(".", 1)[1] if "." in p else p) or None for p in primary]
    authors = np.array([min(len(s or []), 255) for s in surnames], dtype=np.uint8)
    n_categories = np.array([min(len(c), 255) for c in split], dtype=np.uint8)
    # A stable 64-bit value per item, from the arXiv id. Not a function of the entity id — the
    # `fx_key` argument (`reference/oracle/catalogue.py`): a per-item value derived from the entity
    # id would *be* an encoding of it.
    ids = table.column("id").to_pylist()
    id_hash = np.array(
        [int.from_bytes(hashlib.blake2b(i.encode(), digest_size=8).digest(), "little") for i in ids],
        dtype=np.uint64,
    )

    order = np.argsort(eid)
    return {
        "primary": [primary[i] for i in order],
        "secondary": [secondary[i] for i in order],
        "archive": [archive[i] for i in order],
        "suffix": [suffix[i] for i in order],
        "authors": authors[order],
        "n_categories": n_categories[order],
        "id_hash": id_hash[order],
        "stamps": stamps[order],
    }


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
#                 --view s0 --limit 2422486 ...   (the extent is the view's own now)
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


SCHEMA_WIDE_TOML = """\
# The 2.4M arXiv fixture's *packed* per-item columns — the adversarial tail (§4.2).
#
# Generated by `probes/build_attributes.py`. Nineteen columns, each exercising something the
# five-column `schema.toml` cannot; see that script's docstring for the table. Codes are pinned in
# the vocabulary files and compiled into `MANIFEST.vocabularies`; nothing re-derives them.
#
# Residency, which `tessera build` also reports: 59.25 B/row over the 12 B fixed row (+494%).

# ---- categories ------------------------------------------------------------------------------

[[attribute]]
name       = "archive"
type       = "category"
width      = "u8"
used_for   = ["render"]
vocabulary = "declared"
values_key = "archive"
listing    = "public"

[[attribute]]
name       = "primary_category"
type       = "category"
width      = "u16"
used_for   = ["render"]
vocabulary = "declared"
values_key = "primary_category"
listing    = "public"

# A paper with one category has no second, so this column is *absent* — code 0 — for roughly half
# the corpus. It is the only column here that exercises the sentinel against real data.
#
# It also shares `primary_category`'s vocabulary by binding the same key, which is §3.9's shared
# case: keys, codes and properties shared, membership per column.
[[attribute]]
name       = "secondary_category"
type       = "category"
width      = "u16"
used_for   = ["render"]
vocabulary = "declared"
values_key = "primary_category"
listing    = "public"

# The sub-token after `primary_category`'s dot (`cs.LG` -> `LG`; a token with no dot, such as
# `hep-th`, is the whole token) — derived from data already loaded, no new source or join.
# `discovered` rather than `declared`: no `values_key`, no `[attribute.values]`, so the build mints
# and scatters codes from the corpus alone, at the 2.4M scale this fixture exists to exercise
# (§3.4, §4.4). `per_viewer` rather than `public`: `public` + `discovered` is legal but warns on
# every build (§3.8, owner ruling 2026-08-07), and this fixture should not warn by default.
[[attribute]]
name       = "category_suffix"
type       = "category"
width      = "u16"
used_for   = ["render"]
vocabulary = "discovered"
listing    = "per_viewer"

# ---- booleans: bit-packed, and the row count ends mid-byte -------------------------------------

[[attribute]]
name     = "is_cross_listed"
type     = "bool"
used_for = ["render"]

[[attribute]]
name     = "is_single_author"
type     = "bool"
used_for = ["render"]

# ---- unsigned integers, one per width ----------------------------------------------------------

[[attribute]]
name     = "author_count"
type     = "u8"
used_for = ["render"]

[[attribute]]
name     = "category_count"
type     = "u8"
used_for = ["render"]

[[attribute]]
name     = "submitted_month"
type     = "u8"
used_for = ["render"]

[[attribute]]
name     = "submitted_year"
type     = "u16"
used_for = ["render"]

# Dense over the corpus, so it exceeds u16::MAX for all but the first 65,535 items — the one
# column that would be silently wrong under a narrower width.
[[attribute]]
name     = "entity_rank"
type     = "u32"
used_for = ["render"]

# A stable per-item value with no relation to the entity id.
[[attribute]]
name     = "id_hash"
type     = "u64"
used_for = ["render"]

# ---- signed integers, one per width. All are offsets from the corpus median submission --------
# instant, so every one is genuinely negative for the older half of the corpus.

[[attribute]]
name     = "decade_offset"
type     = "i8"
used_for = ["render"]

[[attribute]]
name     = "years_from_median"
type     = "i16"
used_for = ["render"]

[[attribute]]
name     = "days_from_median"
type     = "i32"
used_for = ["render"]

[[attribute]]
name     = "submitted_offset_us"
type     = "i64"
used_for = ["render"]

# ---- time, and the two floats ------------------------------------------------------------------

# The unit is in the type rather than in a convention: `i64` would carry the same bytes and say
# nothing about what they mean.
[[attribute]]
name     = "submitted_at"
type     = "timestamp_us"
used_for = ["render"]

[[attribute]]
name     = "recency"
type     = "f32"
used_for = ["render"]

# **Derived, and deliberately at a precision `f32` cannot hold**: days from the median instant
# carried to microsecond resolution, so a column that silently narrowed to `f32` would be visibly
# wrong rather than plausibly rounded.
[[attribute]]
name     = "exact_offset_days"
type     = "f64"
used_for = ["render"]
"""


def main():
    for path in (SCALED, CORPUS):
        if not path.exists():
            sys.exit(f"missing {path} — see probes/dataset.md")
    OUT.mkdir(parents=True, exist_ok=True)

    ids, mortons, residuals = read_geometry()
    c = read_corpus()

    archive_codes = vocabulary(c["archive"], "archive", U8_MAX_CODE)
    # `secondary_category` binds this same vocabulary (§3.9), so its keys must be in it — a paper's
    # second category is drawn from the same taxonomy as its first, and `vocabulary()` is given
    # both lists so a key appearing only in second position still gets a code.
    primary_codes = vocabulary(
        c["primary"] + [k for k in c["secondary"] if k],
        "primary_category",
        U16_MAX_CODE,
    )
    write_vocabulary(OUT / "archive.parquet", archive_codes)
    write_vocabulary(OUT / "primary_category.parquet", primary_codes)

    stamps = c["stamps"]
    # Derived, not sourced — see the module docstring. Years from the earliest submission, which
    # makes the value independent of when this script runs.
    epoch = stamps.min()
    recency = ((stamps - epoch) / (365.25 * 24 * 3600 * 1_000_000)).astype(np.float32)

    # **Every signed column is an offset from the corpus median instant**, which is what makes them
    # genuinely negative for the older half rather than signed types carrying unsigned data.
    median = int(np.median(stamps))
    offset_us = (stamps - median).astype(np.int64)
    US_PER_DAY = 24 * 3600 * 1_000_000
    days_from_median = (offset_us // US_PER_DAY).astype(np.int32)
    # f64 and not f32: the fractional part is microsecond-resolution, so the value needs more
    # mantissa than an f32 has. A column that silently narrowed would be visibly wrong here.
    exact_offset_days = offset_us / float(US_PER_DAY)

    when = pa.array(stamps, pa.timestamp("us"))
    years = np.array(pa.compute.year(when).to_numpy(zero_copy_only=False), dtype=np.int32)
    months = np.array(pa.compute.month(when).to_numpy(zero_copy_only=False), dtype=np.uint8)
    median_year = int(np.median(years))
    years_from_median = (years - median_year).astype(np.int16)
    # Decades from the 2010s, which spans roughly -3..+1 over this corpus — comfortably inside an
    # `i8`, and refused loudly by the build if a future corpus ever exceeds it.
    decade_offset = ((years - 2015) // 10).astype(np.int8)

    n = len(ids)
    index = ids.astype(np.int64)

    def column(values):
        """`values` in the points file's own row order (which is Morton order for this prefix)."""
        return values[index]

    lean = {
        "entity_id": pa.array(ids, pa.uint32()),
        "morton": pa.array(mortons, pa.uint32()),
        "residual": pa.array(residuals, pa.uint32()),
        # Categories travel as their **keys**, never as codes (§3.1): the code is assigned once,
        # in the vocabulary, and a points file carrying codes would be a second place codes are
        # decided.
        "archive": pa.array([c["archive"][i] for i in index], pa.string()),
        "primary_category": pa.array([c["primary"][i] for i in index], pa.string()),
        "author_count": pa.array(column(c["authors"]), pa.uint8()),
        "submitted_at": pa.array(column(stamps), pa.int64()),
        "recency": pa.array(column(recency), pa.float32()),
    }
    points = OUT / "points.parquet"
    pq.write_table(pa.table(lean), points, compression="zstd", use_dictionary=False)
    log(f"wrote {points}: {n:,} rows, {points.stat().st_size / 1e6:.1f} MB (5 columns)")
    (OUT / "schema.toml").write_text(SCHEMA_TOML)
    log(f"wrote {OUT / 'schema.toml'}")

    wide = dict(lean)
    wide["submitted_at"] = pa.array(column(stamps), pa.timestamp("us"))
    wide.update(
        {
            # **Null where a paper has one category**, which the build maps to the reserved
            # *absent* code 0. Written as a genuine null rather than an empty string: an empty
            # key is a typo trap the schema refuses, and absence is not a value.
            "secondary_category": pa.array(
                [c["secondary"][i] for i in index], pa.string()
            ),
            # Discovered: no vocabulary file is bound for this column, so the build mints codes for
            # every distinct key it finds. Null is written where the derived key is empty (it isn't,
            # for this corpus — see `read_corpus`) rather than an empty string, which the mint
            # routine refuses outright.
            "category_suffix": pa.array([c["suffix"][i] for i in index], pa.string()),
            "is_cross_listed": pa.array(column(c["n_categories"]) > 1, pa.bool_()),
            "is_single_author": pa.array(column(c["authors"]) == 1, pa.bool_()),
            "category_count": pa.array(column(c["n_categories"]), pa.uint8()),
            "submitted_month": pa.array(column(months), pa.uint8()),
            "submitted_year": pa.array(column(years).astype(np.uint16), pa.uint16()),
            "entity_rank": pa.array(ids, pa.uint32()),
            "id_hash": pa.array(column(c["id_hash"]), pa.uint64()),
            "decade_offset": pa.array(column(decade_offset), pa.int8()),
            "years_from_median": pa.array(column(years_from_median), pa.int16()),
            "days_from_median": pa.array(column(days_from_median), pa.int32()),
            "submitted_offset_us": pa.array(column(offset_us), pa.int64()),
            "exact_offset_days": pa.array(column(exact_offset_days), pa.float64()),
        }
    )
    points_wide = OUT / "points-wide.parquet"
    pq.write_table(pa.table(wide), points_wide, compression="zstd", use_dictionary=False)
    log(
        f"wrote {points_wide}: {n:,} rows, "
        f"{points_wide.stat().st_size / 1e6:.1f} MB (19 columns)"
    )
    (OUT / "schema-wide.toml").write_text(SCHEMA_WIDE_TOML)
    log(f"wrote {OUT / 'schema-wide.toml'}")

    absent = sum(1 for k in c["secondary"] if not k)
    log(
        f"\nsecondary_category is absent for {absent:,} of {n:,} items "
        f"({absent / n:.0%}) — the sentinel's coverage"
    )
    log(f"signed columns span {days_from_median.min():,}..{days_from_median.max():,} days from median")
    log(f"{n} rows is {n % 8} bits past a byte boundary — the bool columns end mid-byte")


if __name__ == "__main__":
    main()
