"""GBIF occurrence — 3,654,488,638 records of what was found where, the ladder's largest rung.

One pass over the 8,369 staged parts to a corpus `tessera build` consumes: **one `geo` view** on
Web Mercator, a **three-level tiered taxonomy** over family → genus → species, four attributes —
one per type — plus GBIF's own key `gbifid` declared unique, and an access list whose country
term is the compartment.

    python3 -m test_corpora.gbif.prepare --parts 64      # a prefix, for a run that finishes
    python3 -m test_corpora.gbif.prepare --parts 64 --spread   # …evenly spaced instead
    python3 -m test_corpora.gbif.prepare                 # all 8,369
    python3 -m test_corpora.gbif.prepare --from-points $TESSERA_LADDER/gbif   # §"From points"

**It is a demonstrator and a speed benchmark** (owner ruling, 2026-09-01). What the rung is for is
the row count: 3.65×10⁹ occurrences is 85.1% of one `u32` entity space, and artifact ids allocate
downward into the same space, so it is the first rung whose entity space is nearly spent.

Six decisions this stage makes:

- **A record with no coordinate is dropped, not carried.** There is one view. A row with no
  position could be drawn nowhere, counted in no tile and sampled by nothing, and it would still
  spend an entity id out of a space that is 85.1% used. `manifest.json` and `README.md` carry the
  count and the fraction. Rung 5 carried its unplaceable rows because it has a second view they
  appear in.
- **A record beyond ±85.0511° latitude is kept.** Web Mercator has no position for it and
  quantisation clamps it to the boundary, so the polar records land on a line. They are real
  observations and dropping them would be a rendering decision taken in the pipeline; the count is
  reported instead.
- **The access column is a list of up to three terms, and every row carries at least one.** A
  row's terms are its country, `y:` and its year, and `s:` and its species key. A null year or a
  null species key contributes no term; a record whose country is null or empty carries
  `UNRECORDED`, so the list is never empty and a principal holding no term sees nothing. The
  country term is the compartment. The year and species terms give the measurement drivers
  principals shaped like a user's term set — a few hundred years, a thousand or a hundred thousand
  species — over a dictionary of about 1.4×10⁶ terms (owner ruling, 2026-09-17).
- **The taxonomy starts at family.** `merge_member_runs` holds the largest single artifact's
  members resident while it sorts them, and kingdom Animalia is 2,809,414,577 of them — 22.5 GB on
  a 47 GB box (`probes/2026-09-09-gbif-census/`). `kingdom` rides as a rendered category instead.
- **Nothing holds a column whole.** Parts are read on a thread pool with a bounded look-ahead,
  concatenated into row groups of a million rows, written and released. The only things resident
  for the whole pass are the six censuses below, each bounded by its column's distinct count.
- **The censuses fold in Arrow, not in Python.** A `collections.Counter` updated per part is
  ~6×10⁸ dictionary operations over the whole corpus for the three taxonomy levels alone;
  `value_counts` per batch and one `group_by` per `FOLD_EVERY` batches does the same arithmetic in
  C++.

## From points

`--from-points <rung>` rewrites an already prepared rung's `points.parquet`, and its
`holdout.parquet` and `duplicates.parquet` where it has them, with the access column derived from
the `countrycode`, `year` and `specieskey` they already carry, and writes the rest of the rung
beside them. The source share is 258 GB over SMB and a prepared rung holds every column the access
terms are built from, so a second pass over the share buys nothing. Every other column keeps the
value the first pass gave it, `gbifid` among them, so the two rungs name the same occurrences.
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import itertools
import json
import math
import os
import resource
import shutil
import sys
from collections import deque
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from ..common.paths import ladder
from ..common.projection import MAX_LATITUDE
from ..common.timing import Steps
from . import sources

RUNG = sources.RUNG

#: The access term a record with no country carries, and a term the view's `default` names so that
#: the default never fires. It exists so the access column is never empty: **a principal holding no
#: term must see nothing.** Rung 5's `unpublished` and rung 4's `unlicensed` have the same shape.
UNRECORDED = "UNRECORDED"

#: What prefixes a year term and a species term in the access column. A country code that begins
#: with either would be the same term as a year or a species, merging two compartments, so the run
#: refuses one as it refuses a country spelled `UNRECORDED`.
YEAR_PREFIX = "y:"
SPECIES_PREFIX = "s:"

#: The key a level takes where the source recorded no name for it but recorded one below it.
#: GeoNames' rule and GeoNames' reason: `parent_edges` is `windows(2)` and does not read past a
#: gap, so a null there would state a containment no row makes. A level whose chain simply *ends*
#: stays null.
NOT_RECORDED = "NOT_RECORDED"

#: What separates the levels of a cumulative taxonomy key. A vertical bar rather than a dot: a
#: scientific name carries dots (`cf.`, `sp.`) and would split a key into the wrong levels.
SEPARATOR = "|"

#: Rows per row group in `points.parquet` and `members-taxonomy.parquet`. A part is between 26,717
#: and 1,039,405 rows, so parts are concatenated up to this before a group is written — 8,369 row
#: groups would put 80,000 column chunks in one footer.
ROW_GROUP = 1_000_000

#: Parts read ahead of the one being written, on `--workers` threads. Bounded because the writer
#: is in order and a pool that ran free would hold every part it had finished; sixteen parts is
#: under a gigabyte of Arrow at the largest part size.
READ_AHEAD = 16

#: Batches a census accumulates before it folds. The fold is a `group_by` over the running total
#: plus the pending batches, so a larger number is fewer, wider folds.
FOLD_EVERY = 32

#: What `points.parquet` carries: the publisher's coordinates in degrees, the five attributes,
#: `gbifid` among them, which every file of the corpus names its item by, and the access list built
#: from `countrycode`, `year` and `specieskey`. Fixed rather than inferred, because it is written a batch at a time and a batch whose
#: `kingdom` column happened to be all-null would otherwise change it.
POINTS_SCHEMA = pa.schema(
    [
        pa.field("lon", pa.float64()),
        pa.field("lat", pa.float64()),
        pa.field("countrycode", pa.string()),
        pa.field("kingdom", pa.string()),
        pa.field("specieskey", pa.string()),
        pa.field("year", pa.uint16()),
        pa.field("scientificname", pa.string()),
        pa.field("gbifid", pa.uint64()),
        pa.field("access", pa.list_(pa.string())),
    ]
)

#: The columns of `points.parquet` a dictionary page is worth encoding. `access.list.element` is
#: the leaf of the list column, where parquet holds its strings: a row group's three million terms
#: are a few tens of thousands of distinct ones.
POINTS_DICTIONARY = [
    "countrycode",
    "kingdom",
    "specieskey",
    "scientificname",
    "access.list.element",
]

#: The publisher's own identifier, carried only under `--occurrenceid`.
OCCURRENCE_ID = pa.field("occurrenceid", pa.string())

#: The declaration `--occurrenceid` appends: a keyword read from the record store, not unique,
#: because GBIF does not require a publisher's identifier to be unique.
OCCURRENCE_ID_TOML = """
[[attribute]]
name  = "occurrenceid"
title = "Occurrence ID"
type  = "keyword"
"""

#: The taxonomy layer's member file: one row per occurrence, `entity` its `gbifid` and `key` a
#: three-entry list whose positions are the declared levels.
MEMBER_SCHEMA = pa.schema(
    [pa.field("entity", pa.uint64()), pa.field("key", pa.list_(pa.string()))]
)

#: What the member file's `entity` holds, written to the manifest so `--reuse-taxonomy` refuses a
#: file keyed any other way.
MEMBERS_KEYED_BY = "gbifid"


def peak_gb() -> float:
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20


def true_count(mask) -> int:
    """How many `True`s, nulls not counted."""
    got = pc.sum(mask).as_py()
    return int(got or 0)


# --------------------------------------------------------------------------------- the inputs


def read_parts(paths: list[Path], workers: int, columns: list[str] = sources.COLUMNS):
    """`(path, table)` per part, **in order**, with the next `READ_AHEAD` parts already reading.

    The share is SMB at ~67 MB/s and a single-threaded read of one part leaves it idle between
    round trips; the census measured eight threads at 200 parts in 70.7 s. The writer downstream
    is in order, because the held rows are chosen by position in the part sequence, so the pool is
    consumed in submission order rather than as completions arrive.
    """
    with cf.ThreadPoolExecutor(max_workers=workers) as pool:
        remaining = iter(paths)
        pending: deque = deque(
            (p, pool.submit(pq.read_table, p, columns=columns))
            for p in itertools.islice(remaining, READ_AHEAD)
        )
        while pending:
            path, future = pending.popleft()
            for nxt in itertools.islice(remaining, 1):
                pending.append((nxt, pool.submit(pq.read_table, nxt, columns=columns)))
            yield path, future.result()


def placed(
    table: pa.Table, want_unplaced: bool
) -> tuple[pa.Table | None, pa.Table | None, int, int]:
    """The part's rows with a coordinate inside ±90/±180, the rows without one (`None` unless
    `want_unplaced`), and how many rows had no finite coordinate and how many had one out of
    range."""
    lat = table.column("decimallatitude").combine_chunks().cast(pa.float64())
    lon = table.column("decimallongitude").combine_chunks().cast(pa.float64())
    finite = pc.fill_null(pc.and_(pc.is_finite(lat), pc.is_finite(lon)), False)
    # Filled rather than left null: `filter` drops a null selection and the two counts read the
    # mask, so a three-valued mask would have them disagree with the file.
    inside = pc.fill_null(
        pc.and_(
            finite,
            pc.and_(pc.less_equal(pc.abs(lat), 90.0), pc.less_equal(pc.abs(lon), 180.0)),
        ),
        False,
    )
    kept = true_count(inside)
    finite_rows = true_count(finite)
    return (
        table.filter(inside) if kept else None,
        table.filter(pc.invert(inside)) if want_unplaced and kept < table.num_rows else None,
        table.num_rows - finite_rows,
        finite_rows - kept,
    )


def prefixed(country) -> pa.Array:
    """Where a country code begins with a year or species prefix, and would be that term."""
    return pc.fill_null(
        pc.or_(pc.starts_with(country, YEAR_PREFIX), pc.starts_with(country, SPECIES_PREFIX)),
        False,
    )


def collides(country) -> pa.Array:
    """Where a source's country code would merge with another term: spelled `UNRECORDED`, or
    [`prefixed`]."""
    return pc.or_(pc.fill_null(pc.equal(country, UNRECORDED), False), prefixed(country))


def country_terms(column) -> tuple[pa.Array, int, int]:
    """The country term of each row, with how many rows carried no country and how many
    [`collides`] refuses.

    Filled here rather than left to the view's `default` so that a principal holding no term sees
    nothing. Trimmed for the reason the build trims a label: ` GB` and `GB` are one term rather
    than two that no credential spells the same way.
    """
    country = pc.fill_null(
        pc.utf8_trim_whitespace(column.combine_chunks().cast(pa.string())), ""
    )
    collisions = true_count(collides(country))
    blank = pc.equal(country, "")
    return pc.if_else(blank, UNRECORDED, country), true_count(blank), collisions


def access_lists(country, year, species_key) -> pa.ListArray:
    """The access column: each row's `[country, "y:"+year, "s:"+species_key]`, in that order, a
    null year or species key contributing nothing. `country` is never null, so no list is empty.

    Assembled by index rather than row by row: a batch is a million rows and three million terms.
    """
    rows = len(country)
    # An empty separator, so the join is the prefix followed by the value, and null where the value
    # is null.
    years = pc.binary_join_element_wise(YEAR_PREFIX, pc.cast(year, pa.string()), "")
    species = pc.binary_join_element_wise(SPECIES_PREFIX, pc.cast(species_key, pa.string()), "")
    has_year = pc.is_valid(years).to_numpy(zero_copy_only=False)
    has_species = pc.is_valid(species).to_numpy(zero_copy_only=False)

    offsets = np.zeros(rows + 1, dtype=np.int32)
    np.cumsum(1 + has_year.astype(np.int32) + has_species, out=offsets[1:])
    at = offsets[:-1]

    # One `take` over country ++ the present years ++ the present species; `index` says where each
    # term lands.
    present_years = int(has_year.sum())
    index = np.empty(int(offsets[-1]), dtype=np.int64)
    index[at] = np.arange(rows)
    index[at[has_year] + 1] = rows + np.arange(present_years)
    index[at[has_species] + 1 + has_year[has_species]] = (
        rows + present_years + np.arange(int(has_species.sum()))
    )
    terms = pa.concat_arrays(
        [country.cast(pa.string()), years.drop_null(), species.drop_null()]
    ).take(pa.array(index))
    return pa.ListArray.from_arrays(pa.array(offsets, pa.int32()), terms)


def with_access(table: pa.Table, schema: pa.Schema) -> pa.Table:
    """`table` with its access column built from its own `countrycode`, `year` and `specieskey`."""
    access = access_lists(
        table.column("countrycode").combine_chunks(),
        table.column("year").combine_chunks(),
        table.column("specieskey").combine_chunks(),
    )
    columns = {name: table.column(name) for name in schema.names if name != "access"}
    return pa.table(columns | {"access": access}, schema=schema)


class Census:
    """`value -> count` over a column, folded in Arrow.

    Holds one row per distinct value and nothing per input row, so its size is the column's
    cardinality: nine for `kingdom`, 251 for `countrycode`, ~10⁶ for `specieskey` and for the
    species level of the taxonomy. Nulls are counted and are not values.
    """

    def __init__(self, fold_every: int = FOLD_EVERY):
        self._pending: list[pa.Table] = []
        self._total: pa.Table | None = None
        self._fold_every = fold_every
        self.nulls = 0

    def add(self, values) -> None:
        if isinstance(values, pa.ChunkedArray):
            values = values.combine_chunks()
        self.nulls += values.null_count
        counts = pc.value_counts(values)
        keys = counts.field("values")
        table = pa.table({"key": keys, "n": counts.field("counts").cast(pa.int64())})
        self._pending.append(table.filter(pc.is_valid(keys)))
        if len(self._pending) >= self._fold_every:
            self._fold()

    def _fold(self) -> pa.Table | None:
        if self._pending:
            tables = ([self._total] if self._total is not None else []) + self._pending
            grouped = pa.concat_tables(tables).group_by("key").aggregate([("n", "sum")])
            self._total = grouped.select(["key", "n_sum"]).rename_columns(["key", "n"])
            self._pending = []
        return self._total

    def ranked(self) -> list[tuple]:
        """`(value, count)` descending, materialised in Python — bounded by the distinct count."""
        table = self._fold()
        if table is None:
            return []
        table = table.take(pc.sort_indices(table, sort_keys=[("n", "descending")]))
        return list(zip(table.column("key").to_pylist(), table.column("n").to_pylist()))

    @property
    def totals(self) -> pa.Table | None:
        """`key`, `n`, folded: one row per distinct value, left in Arrow."""
        return self._fold()

    @property
    def distinct(self) -> int:
        table = self._fold()
        return 0 if table is None else table.num_rows

    @property
    def largest(self) -> int:
        table = self._fold()
        if table is None or not table.num_rows:
            return 0
        return int(pc.max(table.column("n")).as_py())


# --------------------------------------------------------------------------------- the outputs


def point_rows(batch: pa.Table, schema: pa.Schema) -> tuple[pa.Table, dict]:
    """A batch's `points.parquet` columns, and what was counted while they were made."""
    m = batch.num_rows
    lat = batch.column("decimallatitude").combine_chunks().cast(pa.float64())
    lon = batch.column("decimallongitude").combine_chunks().cast(pa.float64())
    country, blank, collided = country_terms(batch.column("countrycode"))

    # `year` is `int32` on the share and `u16` in the declaration. A value outside the code space
    # is nulled and counted rather than wrapped: a wrapped year would place an observation in a
    # range filter it does not belong to.
    year = batch.column("year").combine_chunks().cast(pa.int32())
    sane = pc.fill_null(pc.and_(pc.greater_equal(year, 1), pc.less_equal(year, 65_535)), False)
    year_out_of_range = (m - year.null_count) - true_count(sane)
    year = pc.if_else(sane, year, pa.nulls(m, pa.int32())).cast(pa.uint16())

    scientific = batch.column("scientificname").combine_chunks().cast(pa.string())
    # `gbifid` is a string on the share. One that is not an unsigned integer raises here.
    gbifid = batch.column("gbifid").combine_chunks().cast(pa.string()).cast(pa.uint64())
    columns = {
        "lon": lon,
        "lat": lat,
        "countrycode": country,
        "kingdom": batch.column("kingdom").combine_chunks().cast(pa.string()),
        "specieskey": batch.column("specieskey").combine_chunks().cast(pa.string()),
        "year": year,
        "scientificname": scientific,
        "gbifid": gbifid,
    }
    if OCCURRENCE_ID.name in schema.names:
        columns[OCCURRENCE_ID.name] = (
            batch.column(OCCURRENCE_ID.name).combine_chunks().cast(pa.string())
        )
    columns["access"] = access_lists(country, year, columns["specieskey"])
    counts = {
        "beyond_mercator": true_count(pc.greater(pc.abs(lat), MAX_LATITUDE)),
        "year_out_of_range": year_out_of_range,
        "unrecorded_country": blank,
        "country_collisions": collided,
        "named": m - scientific.null_count,
        "gbifid_null": gbifid.null_count,
    }
    return pa.table(columns, schema=schema), counts


class HeldRows:
    """Rows kept aside for an ingest to send after the build.

    **Hold-out:** every `holdout_every`-th row that has no coordinate, up to `holdout` of them,
    given the coordinate of a placed row of the same part. Its identifiers are the publisher's
    and appear nowhere in the build, since the build keeps placed rows only. **Duplicates:** every
    `duplicates_every`-th placed row, up to `duplicates`, copied, so each sets a `gbifid` that an
    item already holds. Both are chosen by position in the part sequence, so a rerun chooses the
    same rows.
    """

    def __init__(self, args):
        self.holdout, self.holdout_every = args.holdout, args.holdout_every
        self.duplicates, self.duplicates_every = args.duplicates, args.duplicates_every
        self.unplaced_seen = self.placed_seen = 0
        self.held: list[pa.Table] = []
        self.copied: list[pa.Table] = []

    def take(self, kept: pa.Table | None, unkept: pa.Table | None) -> None:
        room = self.holdout - sum(t.num_rows for t in self.held)
        if unkept is not None:
            at = np.arange(self.unplaced_seen, self.unplaced_seen + unkept.num_rows)
            pick = np.flatnonzero((at + 1) % self.holdout_every == 0)[: max(room, 0)]
            if pick.size and kept is not None:
                rows = unkept.take(pick)
                donor = kept.take(pa.array(pick % kept.num_rows))
                for name in ("decimallatitude", "decimallongitude"):
                    i = rows.schema.get_field_index(name)
                    rows = rows.set_column(i, rows.field(i), donor.column(name))
                self.held.append(rows)
            self.unplaced_seen += unkept.num_rows
        if kept is not None:
            room = self.duplicates - sum(t.num_rows for t in self.copied)
            at = np.arange(self.placed_seen, self.placed_seen + kept.num_rows)
            pick = np.flatnonzero(at % self.duplicates_every == self.duplicates_every // 2)
            if room > 0 and pick.size:
                self.copied.append(kept.take(pick[:room]))
            self.placed_seen += kept.num_rows

    def write(self, out: Path, schema: pa.Schema) -> dict:
        written = {}
        for name, tables in (("holdout", self.held), ("duplicates", self.copied)):
            with pq.ParquetWriter(out / f"{name}.parquet", schema, compression="zstd") as w:
                for table in tables:
                    w.write_table(point_rows(table, schema)[0])
            written[name] = sum(t.num_rows for t in tables)
        return written


def access_report(census: Census) -> dict:
    """The access column's distinct terms and (row, term) pairs, whole and by class."""
    table = census.totals
    by_class: dict[str, dict] = {}
    if table is not None:
        keys = table.column("key").combine_chunks()
        counts = table.column("n").combine_chunks()
        year = pc.fill_null(pc.starts_with(keys, YEAR_PREFIX), False)
        species = pc.fill_null(pc.starts_with(keys, SPECIES_PREFIX), False)
        classes = {"country": pc.invert(pc.or_(year, species)), "year": year, "species": species}
        for name, mask in classes.items():
            n = counts.filter(mask)
            by_class[name] = {"terms": len(n), "pairs": int(pc.sum(n).as_py() or 0)}
    pairs = sum(c["pairs"] for c in by_class.values())
    return {"terms": census.distinct, "pairs": pairs, "by_class": by_class}


def print_access(report: dict, rows: int) -> None:
    print(
        f"access: {report['terms']:,} distinct terms, {report['pairs']:,} pairs "
        f"({report['pairs'] / max(rows, 1):.3f} a row); "
        + ", ".join(f"{name} {c['terms']:,} terms / {c['pairs']:,} pairs"
                    for name, c in report["by_class"].items()),
        flush=True,
    )


def taxonomy_keys(table: pa.Table) -> tuple[pa.Array, list[pa.Array]]:
    """A batch's member-file list column, and the three level key arrays it was built from.

    Level `j`'s key is the first `j + 1` ranks joined, each rank replaced by `NOT_RECORDED` where
    the source left it null, and the whole key null where **nothing at or below level `j`** was
    recorded — GeoNames' rule: a hole in the chain becomes an artifact and a chain that simply ends
    stays null.

    **Built in Arrow.** The joins are `pc.binary_join_element_wise` per level and the list column
    is one `take` against the three levels' pooled dictionaries; a Python loop over 3.65×10⁹ rows
    and three levels is 1.1×10¹⁰ string operations and would be the whole run.
    """
    n = table.num_rows
    ranks = [table.column(r).combine_chunks().cast(pa.string()) for r in sources.RANKS]
    marked = [pc.fill_null(r, NOT_RECORDED) for r in ranks]

    # `present[j]` — is anything at or below level j recorded? Folded from the deepest rank up, so
    # each level is one OR against the level below rather than a fresh scan of the tail.
    present: list = [None] * len(ranks)
    below = pc.is_valid(ranks[-1])
    present[-1] = below
    for j in range(len(ranks) - 2, -1, -1):
        below = pc.or_(pc.is_valid(ranks[j]), below)
        present[j] = below

    keys: list = []
    joined = marked[0]
    for j in range(len(ranks)):
        if j:
            joined = pc.binary_join_element_wise(joined, marked[j], SEPARATOR)
        keys.append(pc.if_else(present[j], joined, pa.nulls(n, pa.string())))

    # One pooled dictionary over the three levels, so the interleave below is an integer transpose
    # and the strings are moved once.
    encoded = [pc.dictionary_encode(k) for k in keys]
    pooled = pa.concat_arrays([e.dictionary.cast(pa.string()) for e in encoded])
    offsets = np.cumsum([0] + [len(e.dictionary) for e in encoded])[:-1]
    codes = np.full((n, len(ranks)), -1, dtype=np.int64)
    for j, e in enumerate(encoded):
        got = np.asarray(pc.fill_null(e.indices, -1), dtype=np.int64)
        held = got >= 0
        codes[held, j] = got[held] + offsets[j]
    flat = codes.reshape(-1)
    values = pooled.take(pa.array(np.where(flat >= 0, flat, 0), mask=flat < 0))
    listed = pa.ListArray.from_arrays(
        pa.array(np.arange(0, n * len(ranks) + 1, len(ranks), dtype=np.int32), pa.int32()), values
    )
    return listed, keys


def write_vocabulary(out: Path, name: str, keys: list[str]) -> int:
    """One vocabulary file. Code 0 is the *absent* sentinel and is refused in a declared set, so
    the codes start at one."""
    pq.write_table(
        pa.table(
            {
                "key": pa.array(keys, pa.string()),
                "code": pa.array(np.arange(1, len(keys) + 1, dtype=np.uint32), pa.uint32()),
                "title": pa.array(keys, pa.string()),
            }
        ),
        out / f"vocab-{name}.parquet",
    )
    return len(keys)


def write_demo_terms(out: Path, ranks: list[tuple]) -> None:
    """**The demo's and the measurement drivers' candidate terms, ranked by coverage.** A term id
    names a different set in every dictionary, so a corpus with its own dictionary has to name its
    own terms or every principal measures empty against a synthetic `0..200` and the viewer opens
    on a blank map. `serve_battery.py` composes its 1–100% principals from the ranks file.

    The candidate list is **one term per line**, which is what `run_demo.sh --terms-file` reads. A
    country code carries no comma, so the comma-joined form the earlier rungs write would work
    here; one term per line is the form that works whatever the key holds.
    """
    (out / "country-ranks.json").write_text(
        json.dumps([{"term": t, "pairs": n} for t, n in ranks], indent=None) + "\n"
    )
    (out / "country-terms.txt").write_text("".join(f"{t}\n" for t, _ in ranks))
    print(
        f"{len(ranks)} country terms; top five "
        + ", ".join(f"{t} {n:,}" for t, n in ranks[:5]),
        flush=True,
    )


def vocabulary_toml(size: int, rows: int) -> str:
    """The `[[vocabulary]]` block, with the size this run actually minted.

    A width is the size of the code space and not bytes in every row, so it follows the count.
    """
    width = "u8" if size < 255 else ("u16" if size < 65_535 else "u32")
    return (
        f"# Minted by this run from {rows:,} placed occurrences.\n"
        "[[vocabulary]]\n"
        'name       = "kingdom"\n'
        f'title      = "Kingdom"  # {size:,} keys\n'
        f'width      = "{width}"\n'
        'value_set  = "closed"\n'
        'visibility = "public"\n'
        'source     = "kingdom"'
    )


def write_declaration(out: Path, *, size: int, rows: int) -> None:
    """`corpus.toml`, copied beside the data and edited for what this run actually wrote."""

    def fill(text: str, marker: str, block: str) -> str:
        """Replace the **whole line** that is the marker. A plain `str.replace` would also hit the
        header comment that names the marker, which is how a TOML block landed inside a comment
        and refused a build at rung 3."""
        lines = text.splitlines()
        hit = [i for i, line in enumerate(lines) if line.strip() == marker]
        assert len(hit) == 1, f"{marker} appears {len(hit)} times as a line of its own"
        lines[hit[0]] = block
        return "\n".join(lines) + "\n"

    text = (Path(__file__).parent / "corpus.toml").read_text()
    (out / "corpus.toml").write_text(fill(text, "# <vocabulary>", vocabulary_toml(size, rows)))


def write_deployment(out: Path) -> None:
    """`tessera.toml`, generated rather than committed — every value in it is a path or a port on
    this machine, which is the split `configuration.md` §3 draws.

    **Ports 8191–8193**, clear of every rung already on this box (rung 3 on 8111–8113, the
    memory-cap probe on 8121–8123, rung 4 on 8131–8133, rung 5 on 8141–8143) and clear of the two
    drivers that boot their own servers (`serve_battery.py` on 8151, `ingest_cycle.py` on 8161).
    """
    (out / "tessera.toml").write_text(
        """# Generated by `test_corpora/gbif/prepare.py`. Machine-specific by construction: paths
# and ports, and the *names* of the variables carrying the credentials, never the values.

[bundle]
path  = "bundle"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "corpus.toml"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[catalogue]
dir = ".tessera/catalogue"

[serve]
viewer  = "127.0.0.1:8191"
session = "127.0.0.1:8192"
control = "127.0.0.1:8193"
max_k   = 5000
operator_credential_env = "TESSERA_GBIF_OPERATOR_CRED"

# Development only: the origin the demo viewer is served from (client-interaction §7). Without
# it the viewer loads and every request from it fails CORS, which reads like a broken server.
dev_cors_origins = ["http://localhost:PORT", "http://127.0.0.1:PORT"]
""".replace("PORT", os.environ.get("VITE_PORT", "5179"))
    )

    # The directory the WAL and the cache sit in, which the server does not create: it refuses to
    # start with `wal io error: No such file or directory` and names no path.
    (out / ".tessera").mkdir(exist_ok=True)

    # The operator credential is minted once, only if absent, so a rerun keeps the
    # values a running client already holds.
    import secrets

    env = out / ".env"
    lines = env.read_text().splitlines() if env.exists() else []
    held = {line.split("=", 1)[0] for line in lines if "=" in line}
    minted = [
        f"{var}={secrets.token_hex(16)}"
        for var in ("TESSERA_GBIF_OPERATOR_CRED",)
        if var not in held
    ]
    if minted:
        env.write_text("\n".join(lines + minted) + "\n")
        env.chmod(0o600)
        print(f"minted {', '.join(m.split('=')[0] for m in minted)} in {env}")


# ------------------------------------------------------------------------------------- the run


def reusable_manifest(out: Path, parts_read: int, selection: str) -> dict:
    """The manifest of the run that wrote the member file and the kingdom vocabulary in `out`,
    refused unless it read the same dataset and the same parts in the same order.

    The member file names each row by its `gbifid`, and a run that read other parts would place
    rows the kept file has no taxon for. A manifest that does not say its member file is keyed by
    `gbifid` is refused: such a file named rows by their position in the part sequence. The placed
    row count is checked after the pass.
    """
    for name in ("manifest.json", "members-taxonomy.parquet", "vocab-kingdom.parquet"):
        if not (out / name).exists():
            raise SystemExit(f"--reuse-taxonomy: no {name} in {out}; run without the flag")
    kept = json.loads((out / "manifest.json").read_text())
    wanted = {
        "dataset": f"{sources.DATASET}/{sources.VINTAGE}",
        "parts_read": parts_read,
        "part_selection": selection,
        "members_keyed_by": MEMBERS_KEYED_BY,
    }
    differs = {k: (kept.get(k), v) for k, v in wanted.items() if kept.get(k) != v}
    if "members_keyed_by" in differs:
        raise SystemExit(
            f"--reuse-taxonomy: the member file in {out} names each occurrence by "
            f"{kept.get('members_keyed_by') or 'its position'}, not by {MEMBERS_KEYED_BY}; run "
            f"without the flag"
        )
    if differs or "taxonomy" not in kept:
        raise SystemExit(
            f"--reuse-taxonomy: the kept files in {out} were written by another selection "
            f"(kept, this run: {differs or 'no taxonomy block'}); run without the flag"
        )
    return kept


def select_parts(every: list[Path], take: int, spread: bool) -> list[Path]:
    """Which parts this run reads.

    A **prefix** is the default: its rows are then a prefix of the whole corpus's, so a fraction
    run and the whole run hold the same rows as far as the fraction reaches. ⊘ A prefix is not a
    uniform sample — the part order is the publisher's export order, not ours — so a figure
    extrapolated from one carries that. `--spread` takes the same number evenly spaced across all 8,369 instead, which
    is what a coverage fraction should be read off.
    """
    if take <= 0 or take >= len(every):
        return every
    if not spread:
        return every[:take]
    at = (np.arange(take) * (len(every) / take)).astype(np.int64)
    return [every[i] for i in at]


#: What `--from-points` links rather than copies where the two rungs share a filesystem. A prepared
#: rung's files are written once and never edited, and the member file is 22.9 GB at the whole
#: corpus.
LINKED = ("members-taxonomy.parquet", "vocab-kingdom.parquet")

#: What `--from-points` copies as it stands.
COPIED = ("country-ranks.json", "country-terms.txt")

#: What `--from-points` rewrites with the access column, where the source rung has it.
REWRITTEN = ("points.parquet", "holdout.parquet", "duplicates.parquet")


def free_gb(path: Path) -> float:
    stats = os.statvfs(path)
    return stats.f_bavail * stats.f_frsize / 1e9


def place(src: Path, out: Path, name: str, link: bool) -> str:
    """One of the source rung's files beside the rewritten ones, linked or copied."""
    source, target = src / name, out / name
    target.unlink(missing_ok=True)
    if link:
        try:
            os.link(source, target)
            return f"  {name:34} {source.stat().st_size / 1e6:10.2f} MB linked"
        except OSError:
            pass
    shutil.copy2(source, target)
    return f"  {name:34} {target.stat().st_size / 1e6:10.2f} MB copied"


def rewrite_from_points(src: Path, out: Path) -> None:
    """A prepared rung rewritten with the access column, without reading the share again.

    One row group in, one row group out, so nothing holds more than a million rows. Every column
    but `access` is written back as the first pass wrote it. The census printed is the access
    column's, and it is the pre-flight figure for the build: the dictionary is its distinct count
    and `postings_write` is charged by its pairs.
    """
    if not (src / "points.parquet").exists():
        raise SystemExit(f"no points.parquet in {src}; --from-points takes a prepared rung")
    if src.resolve() == out.resolve():
        raise SystemExit(f"--from-points {src} would rewrite its own files; give --out")
    for name in ("manifest.json", *LINKED, *COPIED):
        if not (src / name).exists():
            raise SystemExit(f"no {name} in {src}; --from-points takes a prepared rung")
    out.mkdir(parents=True, exist_ok=True)
    steps = Steps()
    census = Census()
    written = {}

    for name in REWRITTEN:
        if not (src / name).exists():
            continue
        reader = pq.ParquetFile(src / name)
        have = reader.schema_arrow
        missing = {"countrycode", "year", "specieskey", "gbifid"} - set(have.names)
        if missing:
            raise SystemExit(f"{src / name} carries no {sorted(missing)}; it is not a prepared gbif "
                             f"rung of this layout")
        schema = pa.schema([f for f in have if f.name != "access"]).append(
            POINTS_SCHEMA.field("access"))
        print(f"{name}: {reader.metadata.num_rows:,} rows in {reader.num_row_groups:,} row groups, "
              f"{free_gb(out):.1f} GB free", flush=True)
        n = 0
        with steps.step(f"rewrite {name}"), pq.ParquetWriter(
            out / name, schema, compression="zstd", use_dictionary=POINTS_DICTIONARY
        ) as writer:
            for group in range(reader.num_row_groups):
                table = reader.read_row_group(group)
                if refused := true_count(prefixed(table.column("countrycode"))):
                    raise SystemExit(
                        f"{src / name}: {refused:,} rows carry a country code beginning "
                        f"{YEAR_PREFIX!r} or {SPECIES_PREFIX!r}, which would be a year or species "
                        f"term; prepare the rung again from the share with other prefixes"
                    )
                table = with_access(table, schema)
                if name == "points.parquet":
                    census.add(pc.list_flatten(table.column("access")))
                writer.write_table(table, row_group_size=ROW_GROUP)
                n += table.num_rows
        written[name] = n
    rows = written["points.parquet"]
    access = access_report(census)
    print_access(access, rows)

    with steps.step("the rest of the rung"):
        for name in COPIED:
            print(place(src, out, name, link=False), flush=True)
        for name in LINKED:
            print(place(src, out, name, link=True), flush=True)
        write_declaration(
            out, size=pq.read_metadata(out / "vocab-kingdom.parquet").num_rows, rows=rows
        )
        if OCCURRENCE_ID.name in pq.read_schema(out / "points.parquet").names:
            with (out / "corpus.toml").open("a") as declaration:
                declaration.write(OCCURRENCE_ID_TOML)
        write_deployment(out)

    manifest = json.loads((src / "manifest.json").read_text())
    manifest["access"] = access
    manifest["from_points"] = {"source": str(src), "rows": written, "linked": list(LINKED)}
    manifest["bytes"] = {p.name: p.stat().st_size for p in out.iterdir() if p.is_file()}
    manifest["seconds"] = dict(steps)
    manifest["total_seconds"] = steps.total()
    manifest["peak_rss_gb"] = round(peak_gb(), 2)
    manifest.pop("extrapolated_whole_corpus", None)
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2, default=str) + "\n")

    print(f"\nwrote to {out}:")
    for f in sorted(out.iterdir()):
        if f.is_file():
            print(f"  {f.name:34} {f.stat().st_size / 1e6:10.2f} MB")
    print(f"\n{steps.total() / 60:.1f} min, {free_gb(out):.1f} GB free")
    print(f"\nnext:\n  cd {out} && tessera check --payloads && tessera build --stage-timings")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--parts", type=int, default=0,
                    help=f"parts to read; 0 takes all {sources.PART_COUNT:,}")
    ap.add_argument("--fraction", type=float, default=None,
                    help="parts as a fraction of the whole, rounded up; --parts says it as a count")
    ap.add_argument("--spread", action="store_true",
                    help="take the parts evenly spaced rather than as a prefix — a prefix is not "
                         "a uniform sample of a corpus somebody else ordered")
    ap.add_argument("--workers", type=int, default=8,
                    help="threads reading the share; the census measured eight")
    ap.add_argument("--out", type=Path, default=None,
                    help=f"default $TESSERA_LADDER/{RUNG}, or $TESSERA_LADDER/{RUNG}-<n>p for a "
                         f"run that reads part of the corpus")
    ap.add_argument("--occurrenceid", action="store_true",
                    help="also carry the publisher's occurrenceid, declared as a keyword; about "
                         "42 bytes a row")
    ap.add_argument("--reuse-taxonomy", action="store_true",
                    help="keep the member file and the kingdom vocabulary already in the output "
                         "directory rather than writing them; refused unless the manifest beside "
                         "them names the same dataset and parts and the same rows are placed")
    ap.add_argument("--holdout", type=int, default=0,
                    help="rows without a coordinate to write to holdout.parquet, each given a "
                         "placed row's coordinate, for an ingest to send after the build")
    ap.add_argument("--holdout-every", type=int, default=158,
                    help="take every this-many-th row without a coordinate; 158 spreads 10^6 "
                         "across the whole corpus")
    ap.add_argument("--duplicates", type=int, default=0,
                    help="placed rows to copy to duplicates.parquet, each setting a gbifid an "
                         "item holds")
    ap.add_argument("--duplicates-every", type=int, default=349_000,
                    help="copy every this-many-th placed row; 349,000 spreads 10^4 across the "
                         "whole corpus")
    ap.add_argument("--from-points", type=Path, default=None, metavar="RUNG",
                    help="rewrite an already prepared rung's points with the access column "
                         "instead of reading the share; the default output is the rung's "
                         "directory with -terms after it")
    args = ap.parse_args()

    if args.from_points is not None:
        src = args.from_points
        src = src.parent if src.is_file() else src
        return rewrite_from_points(src, args.out or ladder(f"{src.name}-terms"))

    every = sources.parts()
    if args.fraction is not None:
        assert 0 < args.fraction <= 1, f"--fraction {args.fraction} is not in (0, 1]"
        args.parts = max(1, math.ceil(args.fraction * len(every)))
    chosen = select_parts(every, args.parts, args.spread)
    whole = len(chosen) == len(every)
    if whole:
        assert len(every) == sources.PART_COUNT, (
            f"{len(every):,} parts on the share against {sources.PART_COUNT:,} recorded"
        )

    out = args.out or ladder(RUNG if whole else f"{RUNG}-{len(chosen)}p")
    out.mkdir(parents=True, exist_ok=True)
    selection = "whole" if whole else ("spread" if args.spread else "prefix")
    print(f"parts  {len(chosen):,} of {len(every):,} "
          f"({'the whole corpus' if whole else 'evenly spaced' if args.spread else 'a prefix'})\n"
          f"output {out}", flush=True)
    steps = Steps()
    schema = POINTS_SCHEMA.append(OCCURRENCE_ID) if args.occurrenceid else POINTS_SCHEMA
    columns = sources.COLUMNS + ([OCCURRENCE_ID.name] if args.occurrenceid else [])
    kept_manifest = reusable_manifest(out, len(chosen), selection) if args.reuse_taxonomy else None
    if kept_manifest is not None:
        columns = [c for c in columns if c not in sources.RANKS]
    held = HeldRows(args)

    kingdom_census = Census()
    country_census = Census()
    species_key_census = Census()
    level_census = [Census() for _ in sources.RANKS]
    year_census = Census()
    access_census = Census()

    rows_read = 0
    rows_placed = 0
    no_coordinate = 0
    out_of_range = 0
    counted = dict.fromkeys(
        ["beyond_mercator", "year_out_of_range", "unrecorded_country", "country_collisions",
         "named", "gbifid_null"], 0)
    gbifid_bounds = [2**64, -1]
    rank_collisions: dict[str, set] = {rank: set() for rank in sources.RANKS}
    lon_bounds = [float("inf"), float("-inf")]
    lat_bounds = [float("inf"), float("-inf")]

    points = pq.ParquetWriter(
        out / "points.parquet", schema, compression="zstd", use_dictionary=POINTS_DICTIONARY,
    )
    members = None if kept_manifest is not None else pq.ParquetWriter(
        out / "members-taxonomy.parquet", MEMBER_SCHEMA, compression="zstd")

    def flush(batch: pa.Table) -> None:
        """One row group of `points.parquet` and one of the member file, from a batch of parts."""
        nonlocal rows_placed
        m = batch.num_rows
        rows, counts = point_rows(batch, schema)
        for key, n in counts.items():
            counted[key] += n

        lat, lon, gbifid = rows.column("lat"), rows.column("lon"), rows.column("gbifid")
        lon_bounds[0] = min(lon_bounds[0], float(pc.min(lon).as_py()))
        lon_bounds[1] = max(lon_bounds[1], float(pc.max(lon).as_py()))
        lat_bounds[0] = min(lat_bounds[0], float(pc.min(lat).as_py()))
        lat_bounds[1] = max(lat_bounds[1], float(pc.max(lat).as_py()))
        if gbifid.null_count < m:
            low, high = pc.min_max(gbifid).values()
            gbifid_bounds[0] = min(gbifid_bounds[0], low.as_py())
            gbifid_bounds[1] = max(gbifid_bounds[1], high.as_py())

        kingdom_census.add(rows.column("kingdom"))
        country_census.add(rows.column("countrycode"))
        species_key_census.add(rows.column("specieskey"))
        year_census.add(rows.column("year"))
        access_census.add(pc.list_flatten(rows.column("access")))

        if members is not None:
            # The taxonomy markers must not collide with a name the source wrote. Checked over
            # the batch's distinct values rather than its rows: 10⁶ rows hold ~10⁴ family names.
            for rank in sources.RANKS:
                uniq = pc.unique(batch.column(rank).combine_chunks().cast(pa.string()))
                bad = pc.fill_null(
                    pc.or_(pc.equal(uniq, NOT_RECORDED), pc.match_substring(uniq, SEPARATOR)),
                    False,
                )
                if true_count(bad):
                    rank_collisions[rank].update(uniq.filter(bad).to_pylist())
            listed, keys = taxonomy_keys(batch)
            for census, key in zip(level_census, keys):
                census.add(key)
            members.write_table(
                pa.table({"entity": gbifid, "key": listed},
                         schema=MEMBER_SCHEMA),
                row_group_size=ROW_GROUP,
            )

        points.write_table(rows, row_group_size=ROW_GROUP)
        rows_placed += m

    with steps.step("read, place and write"):
        buffer: list[pa.Table] = []
        buffered = 0
        for at, (_path, table) in enumerate(read_parts(chosen, args.workers, columns)):
            rows_read += table.num_rows
            kept, unkept, unplaced, outside = placed(table, held.holdout > 0)
            no_coordinate += unplaced
            out_of_range += outside
            if held.holdout or held.duplicates:
                held.take(kept, unkept)
            if kept is not None:
                buffer.append(kept)
                buffered += kept.num_rows
            del table, kept, unkept
            if buffered >= ROW_GROUP:
                flush(pa.concat_tables(buffer))
                buffer, buffered = [], 0
            if (at + 1) % 500 == 0:
                print(f"    {at + 1:,}/{len(chosen):,} parts, {rows_placed:,} placed "
                      f"({peak_gb():.1f} GB)", flush=True)
        if buffered:
            flush(pa.concat_tables(buffer))
        buffer = []
    points.close()
    if members is not None:
        members.close()
    with steps.step("held rows"):
        held_rows = held.write(out, schema)
    print(f"held out {held_rows['holdout']:,} rows without a coordinate, copied "
          f"{held_rows['duplicates']:,} placed rows as duplicates", flush=True)
    beyond_mercator = counted["beyond_mercator"]
    country_collisions = counted["country_collisions"]

    assert rows_read == rows_placed + no_coordinate + out_of_range, (
        f"{rows_read:,} read against {rows_placed:,} placed, {no_coordinate:,} with no coordinate "
        f"and {out_of_range:,} out of range"
    )
    if whole:
        assert rows_read == sources.TOTAL_ROWS, (
            f"the share holds {rows_read:,} rows against {sources.TOTAL_ROWS:,} recorded"
        )
    print(f"placed {rows_placed:,} of {rows_read:,} ({rows_placed / rows_read:.2%}); "
          f"{no_coordinate:,} carry no coordinate ({no_coordinate / rows_read:.2%}), "
          f"{out_of_range:,} lie outside ±90/±180", flush=True)
    print(f"  {beyond_mercator:,} placed rows are beyond ±{MAX_LATITUDE:.4f}° and clamp to the "
          f"extent boundary ({beyond_mercator / max(rows_placed, 1):.3%})", flush=True)

    # **The markers must not collide with a name the source wrote.** A rank value spelled
    # `NOT_RECORDED` would merge with the placeholder for a level nobody recorded, and one carrying
    # the separator would split a key at the wrong level; a country code spelled `UNRECORDED` would
    # merge real records into the term that stands for *no country*, and one beginning with a year
    # or species prefix would merge two compartments. Each would move records between artifacts or
    # between compartments with no error, so each is a refusal rather than a report.
    collided = {rank: sorted(got)[:5] for rank, got in rank_collisions.items() if got}
    if country_collisions:
        collided["countrycode"] = [f"{country_collisions:,} rows"]
    if collided:
        raise SystemExit(
            f"source value(s) collide with this script's markers ({NOT_RECORDED!r} for a level "
            f"the source did not record, {SEPARATOR!r} between levels, {UNRECORDED!r} for a "
            f"record with no country, {YEAR_PREFIX!r} and {SPECIES_PREFIX!r} before a year and a "
            f"species access term): {collided}. Choose other markers; a placeholder that merges "
            f"with a real value would move records between artifacts and between compartments."
        )

    with steps.step("vocabulary"):
        kingdoms = sorted(k for k, _ in kingdom_census.ranked())
        if kept_manifest is not None:
            kept_keys = pq.read_table(out / "vocab-kingdom.parquet").column("key").to_pylist()
            assert set(kingdoms) <= set(kept_keys), (
                f"--reuse-taxonomy: kingdoms {sorted(set(kingdoms) - set(kept_keys))} are not in "
                f"the vocabulary kept in {out}; run without the flag"
            )
            vocab_size = len(kept_keys)
            member_rows = pq.ParquetFile(out / "members-taxonomy.parquet").metadata.num_rows
            for what, kept_rows in (("member file", member_rows),
                                    ("manifest", kept_manifest["rows_placed"])):
                assert kept_rows == rows_placed, (
                    f"--reuse-taxonomy: the {what} kept in {out} has {kept_rows:,} placed rows "
                    f"and this run placed {rows_placed:,}; run without the flag"
                )
        else:
            vocab_size = write_vocabulary(out, "kingdom", kingdoms)
    print(f"kingdom vocabulary: {vocab_size} keys, {kingdom_census.nulls:,} rows carry none "
          f"({kingdom_census.nulls / max(rows_placed, 1):.2%})", flush=True)

    with steps.step("the demo's terms"):
        country_ranks = country_census.ranked()
        write_demo_terms(out, country_ranks)
        access = access_report(access_census)
    print_access(access, rows_placed)

    write_declaration(out, size=vocab_size, rows=rows_placed)
    if args.occurrenceid:
        with (out / "corpus.toml").open("a") as declaration:
            declaration.write(OCCURRENCE_ID_TOML)
    write_deployment(out)

    levels = [] if kept_manifest is not None else [
        {
            "level": j,
            "title": ["Family", "Genus", "Species"][j],
            "artifacts": census.distinct,
            "largest_artifact": census.largest,
            "rows_in_no_artifact": census.nulls,
        }
        for j, census in enumerate(level_census)
    ]
    for level in levels:
        print(f"  level {level['level']} {level['title']:<8} {level['artifacts']:>10,} artifacts, "
              f"largest {level['largest_artifact']:,}, "
              f"{level['rows_in_no_artifact']:,} rows in none", flush=True)

    sizes = {p.name: p.stat().st_size for p in out.iterdir() if p.is_file()}
    points_bytes = sizes.get("points.parquet", 0)
    members_bytes = sizes.get("members-taxonomy.parquet", 0)
    scale = sources.TOTAL_ROWS / rows_read if rows_read else 0.0
    manifest = {
        "rung": RUNG,
        "dataset": f"{sources.DATASET}/{sources.VINTAGE}",
        "parts_read": len(chosen),
        "parts_total": len(every),
        "part_selection": selection,
        "members_keyed_by": MEMBERS_KEYED_BY,
        "rows_read": rows_read,
        "rows_placed": rows_placed,
        "placed_share": round(rows_placed / rows_read, 6) if rows_read else None,
        "no_coordinate": no_coordinate,
        "coordinate_out_of_range": out_of_range,
        "beyond_mercator": beyond_mercator,
        "bounds": {"lon": lon_bounds, "lat": lat_bounds},
        "countrycode": {
            "terms": len(country_ranks),
            "unrecorded": counted["unrecorded_country"],
            "top": [{"term": t, "pairs": n} for t, n in country_ranks[:5]],
        },
        "access": access,
        "kingdom": {"keys": vocab_size, "null_rows": kingdom_census.nulls},
        "specieskey": {
            "distinct": species_key_census.distinct,
            "null_rows": species_key_census.nulls,
        },
        "year": {
            "distinct": year_census.distinct,
            "null_rows": year_census.nulls,
            "out_of_range": counted["year_out_of_range"],
        },
        "scientificname": {"present": counted["named"]},
        "gbifid": {"null_rows": counted["gbifid_null"], "bounds": gbifid_bounds},
        "held_rows": held_rows,
        "taxonomy": kept_manifest["taxonomy"] if kept_manifest is not None else {
            "member_rows": rows_placed,
            "membership_entries": sum(rows_placed - lv["rows_in_no_artifact"] for lv in levels),
            "levels": levels,
        },
        "bytes": sizes,
        "seconds": dict(steps),
        "total_seconds": steps.total(),
        "peak_rss_gb": round(peak_gb(), 2),
        # ⊘ **Modelled, not measured.** Every figure here is this run's own scaled by the row
        # ratio, which is sound for sizing and is not a substitute for the whole run's report.
        # A prefix selection is not a uniform sample and `part_selection` says which this was.
        "extrapolated_whole_corpus": {
            "rows_read": sources.TOTAL_ROWS,
            "rows_placed": round(rows_placed * scale),
            "points_bytes": round(points_bytes * scale),
            "members_bytes": round(members_bytes * scale),
            "seconds": round(steps.total() * scale, 1),
        },
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2, default=str) + "\n")

    print(f"\nwrote to {out}:")
    for f in sorted(out.iterdir()):
        if f.is_file():
            print(f"  {f.name:34} {f.stat().st_size / 1e6:10.2f} MB")
    if not whole:
        ex = manifest["extrapolated_whole_corpus"]
        print(f"\nmodelled whole corpus (this run x {scale:,.1f}): "
              f"{ex['rows_placed']:,} placed rows, "
              f"points.parquet {ex['points_bytes'] / 1e9:.1f} GB, "
              f"members-taxonomy.parquet {ex['members_bytes'] / 1e9:.1f} GB, "
              f"{ex['seconds'] / 3600:.1f} h")
    print(f"\nnext:\n  cd {out} && tessera check --payloads && tessera build --stage-timings")


if __name__ == "__main__":
    sys.exit(main())
