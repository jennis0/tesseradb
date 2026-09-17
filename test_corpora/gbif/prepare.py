"""GBIF occurrence — 3,654,488,638 records of what was found where, the ladder's largest rung.

One pass over the 8,369 staged parts to a corpus `tessera build` consumes: **one `geo` view** on
Web Mercator, a **three-level tiered taxonomy** over family → genus → species, four attributes —
one per type — and `countrycode` as the compartment.

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

`--from-points <rung>` rewrites an already prepared rung's `points.parquet` with the access column
derived from the `countrycode`, `year` and `specieskey` it already carries, and writes the rest of
the rung beside it. It exists because the source share is 258 GB over SMB and the prepared
`points.parquet` holds every column the access terms are built from, so a second pass over the
share buys nothing. Entity ids and every other column keep the values the first pass gave them,
which is what makes a bundle built from the rewrite comparable with one built from the original
(I9). `.env` is copied rather than minted, so the identity key, the `tessera_id` it blinds and the
row order it fixes are the source rung's.
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import itertools
import json
import math
import os
import resource
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

#: What prefixes a year term and a species term in the access column. A country code is two upper
#: case letters or `UNRECORDED`, so no prefixed term can spell one; the run refuses a country code
#: that does, for the reason it refuses one spelled `UNRECORDED`.
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

#: What `points.parquet` carries: identity, the publisher's coordinates in degrees, the four
#: attributes and the access column. Fixed rather than inferred, because it is written a batch at a
#: time and a batch whose `kingdom` column happened to be all-null would otherwise change it.
#: `countrycode`, `specieskey` and `year` stay as attributes; `access` is built from them.
POINTS_SCHEMA = pa.schema(
    [
        pa.field("entity_id", pa.uint64()),
        pa.field("lon", pa.float64()),
        pa.field("lat", pa.float64()),
        pa.field("countrycode", pa.string()),
        pa.field("kingdom", pa.string()),
        pa.field("specieskey", pa.string()),
        pa.field("year", pa.uint16()),
        pa.field("scientificname", pa.string()),
        pa.field("access", pa.list_(pa.string())),
    ]
)

#: The taxonomy layer's member file: one row per occurrence, `key` a three-entry list whose
#: positions are the declared levels.
MEMBER_SCHEMA = pa.schema(
    [pa.field("entity", pa.uint64()), pa.field("key", pa.list_(pa.string()))]
)


#: The columns of `points.parquet` a dictionary page is worth encoding. `access.list.element` is
#: the leaf of the list column, which is where parquet holds the strings: a row group's three
#: million terms are a few tens of thousands of distinct ones, so the dictionary is the difference
#: between holding a species key once and holding it once per occurrence.
POINTS_DICTIONARY = [
    "countrycode",
    "kingdom",
    "specieskey",
    "scientificname",
    "access.list.element",
]


def peak_gb() -> float:
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20


def true_count(mask) -> int:
    """How many `True`s, nulls not counted."""
    got = pc.sum(mask).as_py()
    return int(got or 0)


def access_lists(country, year, species_key) -> tuple[pa.ListArray, pa.Array]:
    """A batch's access column, and the flattened terms it holds.

    One row's terms are `[country, "y:"+year, "s:"+species_key]`, in that order, with a null year
    or species key contributing nothing. `country` is never null, so the list is never empty.

    The flattened terms come back beside the column because the census wants them and rebuilding
    them from the list would be a second pass. Assembled by index rather than row by row: a batch
    is a million rows and three million terms, and Python would spend a second a batch on it.
    """
    country = country.combine_chunks() if isinstance(country, pa.ChunkedArray) else country
    rows = len(country)
    # An empty separator, so the join is the prefix followed by the value. A null year or species
    # key makes the whole join null, which is how a row comes to carry fewer than three terms.
    years = pc.binary_join_element_wise(YEAR_PREFIX, pc.cast(year, pa.string()), "")
    species = pc.binary_join_element_wise(SPECIES_PREFIX, species_key, "")
    has_year = pc.is_valid(years).to_numpy(zero_copy_only=False)
    has_species = pc.is_valid(species).to_numpy(zero_copy_only=False)

    counts = np.ones(rows, dtype=np.int32) + has_year + has_species
    offsets = np.zeros(rows + 1, dtype=np.int32)
    np.cumsum(counts, out=offsets[1:])
    at = offsets[:-1]

    # `take` once over country ++ the present years ++ the present species, which is why the three
    # blocks are concatenated first and the index array says where each term lands.
    present_years = int(has_year.sum())
    index = np.empty(int(offsets[-1]), dtype=np.int64)
    index[at] = np.arange(rows)
    index[at[has_year] + 1] = rows + np.arange(present_years)
    index[at[has_species] + 1 + has_year[has_species]] = (
        rows + present_years + np.arange(int(has_species.sum()))
    )
    terms = pa.concat_arrays(
        [
            country.cast(pa.string()),
            years.drop_null().cast(pa.string()),
            species.drop_null().cast(pa.string()),
        ]
    ).take(pa.array(index))
    return pa.ListArray.from_arrays(pa.array(offsets, pa.int32()), terms), terms


# --------------------------------------------------------------------------------- the inputs


def read_parts(paths: list[Path], workers: int):
    """`(path, table)` per part, **in order**, with the next `READ_AHEAD` parts already reading.

    The share is SMB at ~67 MB/s and a single-threaded read of one part leaves it idle between
    round trips; the census measured eight threads at 200 parts in 70.7 s. The writer downstream
    is in order, because an entity id is the row's position in the part sequence, so the pool is
    consumed in submission order rather than as completions arrive.
    """
    with cf.ThreadPoolExecutor(max_workers=workers) as pool:
        remaining = iter(paths)
        pending: deque = deque(
            (p, pool.submit(pq.read_table, p, columns=sources.COLUMNS))
            for p in itertools.islice(remaining, READ_AHEAD)
        )
        while pending:
            path, future = pending.popleft()
            for nxt in itertools.islice(remaining, 1):
                pending.append((nxt, pool.submit(pq.read_table, nxt, columns=sources.COLUMNS)))
            yield path, future.result()


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
        """`key`, `n`, folded. One row per distinct value, so a caller can count or slice it in
        Arrow rather than materialise 1.4×10⁶ Python strings the way `ranked` does."""
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


def access_census_report(census: Census, pairs: int) -> dict:
    """The access column's figures: distinct terms and pairs, whole and by class.

    The pair count is what the build's `postings_write` stage is charged by and what the term
    images are derived from, so it is reported beside the term count rather than left to be
    inferred from the row count.
    """
    table = census.totals
    by_class: dict[str, dict] = {}
    if table is not None:
        keys = table.column("key").combine_chunks()
        counts = table.column("n").combine_chunks()
        prefixed = pc.fill_null(pc.starts_with(keys, YEAR_PREFIX), False)
        classes = {
            "year": prefixed,
            "species": pc.fill_null(pc.starts_with(keys, SPECIES_PREFIX), False),
        }
        classes["country"] = pc.invert(pc.or_(classes["year"], classes["species"]))
        for name, mask in classes.items():
            n = counts.filter(mask)
            by_class[name] = {
                "terms": len(n),
                "pairs": int(pc.sum(n).as_py() or 0),
            }
    return {"terms": census.distinct, "pairs": pairs, "by_class": by_class}


def print_access_census(report: dict, rows: int) -> None:
    by = report["by_class"]
    print(
        f"access: {report['terms']:,} distinct terms, {report['pairs']:,} pairs "
        f"({report['pairs'] / max(rows, 1):.3f} a row); "
        + ", ".join(f"{name} {v['terms']:,} terms / {v['pairs']:,} pairs"
                    for name, v in sorted(by.items())),
        flush=True,
    )


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
# and ports, and the *name* of the variable carrying the identity key, never the key.

[bundle]
path  = "bundle"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "corpus.toml"

[plugin]
module = "builtin:passthrough"

[identity]
env = "TESSERA_IDENTITY_KEY"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:8191"
session = "127.0.0.1:8192"
control = "127.0.0.1:8193"
max_k   = 5000
session_credential_env  = "TESSERA_GBIF_SESSION_CRED"
operator_credential_env = "TESSERA_GBIF_OPERATOR_CRED"

# Development only: the origin the demo viewer is served from (client-interaction §7). Without
# it the viewer loads and every request from it fails CORS, which reads like a broken server.
dev_cors_origins = ["http://localhost:PORT", "http://127.0.0.1:PORT"]
""".replace("PORT", os.environ.get("VITE_PORT", "5179"))
    )

    # The directory the WAL and the cache sit in, which the server does not create: it refuses to
    # start with `wal io error: No such file or directory` and names no path.
    (out / ".tessera").mkdir(exist_ok=True)

    # The identity key is created once and never overwritten: it is what every `tessera_id` this
    # corpus has served is derived from, so regenerating it would invalidate every identifier a
    # client holds and reorder every row (contracts §2.2).
    import secrets

    env = out / ".env"
    lines = env.read_text().splitlines() if env.exists() else []
    held = {line.split("=", 1)[0] for line in lines if "=" in line}
    minted = [
        f"{var}={secrets.token_hex(16)}"
        for var in ("TESSERA_IDENTITY_KEY", "TESSERA_GBIF_SESSION_CRED",
                    "TESSERA_GBIF_OPERATOR_CRED")
        if var not in held
    ]
    if minted:
        env.write_text("\n".join(lines + minted) + "\n")
        env.chmod(0o600)
        print(f"minted {', '.join(m.split('=')[0] for m in minted)} in {env}")


# ------------------------------------------------------------------------------------- the run


def select_parts(every: list[Path], take: int, spread: bool) -> list[Path]:
    """Which parts this run reads.

    A **prefix** is the default: entity ids are then a prefix of the whole corpus's, so a fraction
    run and the whole run agree on every row they share. ⊘ A prefix is not a uniform sample — the
    part order is the publisher's export order, not ours — so a figure extrapolated from one
    carries that. `--spread` takes the same number evenly spaced across all 8,369 instead, which
    is what a coverage fraction should be read off.
    """
    if take <= 0 or take >= len(every):
        return every
    if not spread:
        return every[:take]
    at = (np.arange(take) * (len(every) / take)).astype(np.int64)
    return [every[i] for i in at]


#: What `--from-points` links rather than copies, where the two rungs are on one filesystem. A
#: prepared rung's files are written once and never edited, and `members-taxonomy.parquet` is
#: 22.9 GB at the whole corpus; a copy of it buys nothing and costs the disk the build needs.
LINKED = ("members-taxonomy.parquet", "vocab-kingdom.parquet")

#: What `--from-points` copies as it stands. `.env` is copied rather than minted so the rewritten
#: rung keeps the source rung's identity key, and with it the same `tessera_id` and the same row
#: order (contracts §2.2).
COPIED = (".env", "country-ranks.json", "country-terms.txt")


def free_gb(path: Path) -> float:
    stats = os.statvfs(path)
    return stats.f_bavail * stats.f_frsize / 1e9


def place(src: Path, out: Path, name: str, link: bool) -> str:
    """One of the source rung's files beside the rewritten one, linked or copied."""
    source, target = src / name, out / name
    if not source.exists():
        return f"  {name:34} absent from {src}"
    if target.exists():
        target.unlink()
    if link:
        try:
            os.link(source, target)
            return f"  {name:34} {source.stat().st_size / 1e6:10.2f} MB linked"
        except OSError:
            pass
    import shutil

    shutil.copy2(source, target)
    target.chmod(source.stat().st_mode & 0o777)
    return f"  {name:34} {target.stat().st_size / 1e6:10.2f} MB copied"


def rewrite_from_points(src: Path, out: Path) -> None:
    """A prepared rung rewritten with the access column, without reading the share again.

    One row group in, one row group out, so nothing holds more than a million rows. Every column
    but `access` is written back with the value the first pass gave it, and the entity ids are
    checked contiguous per row group: a bundle built from the rewrite assigns the same entity id to
    the same occurrence as one built from the source (I9), which is what makes the two comparable.

    The census the rewrite prints is the access column's, and it is the pre-flight figure for the
    build: the dictionary is its distinct count and `postings_write` is charged by its pairs.
    """
    points = src / "points.parquet"
    assert points.exists(), f"{points} does not exist; --from-points takes a prepared rung"
    out.mkdir(parents=True, exist_ok=True)
    steps = Steps()

    reader = pq.ParquetFile(points)
    have = set(reader.schema_arrow.names)
    missing = {"entity_id", "countrycode", "year", "specieskey"} - have
    assert not missing, f"{points} carries no {sorted(missing)}; it is not a prepared gbif rung"
    print(f"source {points} ({points.stat().st_size / 1e9:.1f} GB, "
          f"{reader.metadata.num_rows:,} rows, {reader.num_row_groups:,} row groups)\n"
          f"output {out}\n"
          f"free   {free_gb(out):.1f} GB", flush=True)

    census = Census()
    pairs = 0
    rows = 0
    writer = pq.ParquetWriter(
        out / "points.parquet", POINTS_SCHEMA, compression="zstd",
        use_dictionary=POINTS_DICTIONARY,
    )
    with steps.step("rewrite points.parquet"):
        for group in range(reader.num_row_groups):
            batch = reader.read_row_group(group)
            m = batch.num_rows
            entity = batch.column("entity_id").combine_chunks()
            first, last = entity[0].as_py(), entity[m - 1].as_py()
            assert first == rows and last == rows + m - 1, (
                f"row group {group} runs {first:,}..{last:,} over {m:,} rows, against "
                f"{rows:,} written: the source's entity ids are not one contiguous run"
            )
            access, terms = access_lists(
                batch.column("countrycode").combine_chunks(),
                batch.column("year").combine_chunks(),
                batch.column("specieskey").combine_chunks(),
            )
            pairs += len(terms)
            census.add(terms)
            writer.write_table(
                pa.table(
                    {name: batch.column(name) for name in POINTS_SCHEMA.names if name != "access"}
                    | {"access": access},
                    schema=POINTS_SCHEMA,
                ),
                row_group_size=ROW_GROUP,
            )
            rows += m
            if (group + 1) % 200 == 0:
                print(f"    {group + 1:,}/{reader.num_row_groups:,} row groups, {rows:,} rows, "
                      f"{(out / 'points.parquet').stat().st_size / 1e9:.1f} GB written "
                      f"({peak_gb():.1f} GB, {free_gb(out):.0f} GB free)", flush=True)
    writer.close()

    report = access_census_report(census, pairs)
    print_access_census(report, rows)

    with steps.step("the rest of the rung"):
        for name in COPIED:
            print(place(src, out, name, link=False), flush=True)
        for name in LINKED:
            print(place(src, out, name, link=True), flush=True)
        source_manifest = json.loads((src / "manifest.json").read_text())
        write_declaration(
            out,
            size=pq.read_metadata(out / "vocab-kingdom.parquet").num_rows,
            rows=rows,
        )
        write_deployment(out)

    manifest = dict(source_manifest)
    manifest["access"] = report
    manifest["from_points"] = {
        "source": str(src),
        "rows": rows,
        "linked": list(LINKED),
    }
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
    ap.add_argument("--from-points", type=Path, default=None, metavar="RUNG",
                    help="rewrite an already prepared rung's points.parquet with the access "
                         "column instead of reading the share; the default output is the rung's "
                         "directory with -terms after it")
    ap.add_argument("--out", type=Path, default=None,
                    help=f"default $TESSERA_LADDER/{RUNG}, or $TESSERA_LADDER/{RUNG}-<n>p for a "
                         f"run that reads part of the corpus")
    args = ap.parse_args()

    if args.from_points is not None:
        # The flag names the points file or the rung that holds it; both reach the same rung.
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
    steps = Steps()
    print(f"parts  {len(chosen):,} of {len(every):,} "
          f"({'the whole corpus' if whole else 'evenly spaced' if args.spread else 'a prefix'})\n"
          f"output {out}", flush=True)

    kingdom_census = Census()
    country_census = Census()
    species_key_census = Census()
    level_census = [Census() for _ in sources.RANKS]
    year_census = Census()
    access_census = Census()
    access_pairs = 0

    rows_read = 0
    rows_placed = 0
    no_coordinate = 0
    out_of_range = 0
    beyond_mercator = 0
    year_out_of_range = 0
    unrecorded_country = 0
    country_collisions = 0
    named = 0
    rank_collisions: dict[str, set] = {rank: set() for rank in sources.RANKS}
    prefixed_countries: set[str] = set()
    lon_bounds = [float("inf"), float("-inf")]
    lat_bounds = [float("inf"), float("-inf")]

    points = pq.ParquetWriter(
        out / "points.parquet", POINTS_SCHEMA, compression="zstd",
        use_dictionary=POINTS_DICTIONARY,
    )
    members = pq.ParquetWriter(out / "members-taxonomy.parquet", MEMBER_SCHEMA,
                               compression="zstd")

    def flush(batch: pa.Table) -> None:
        """One row group of `points.parquet` and one of the member file, from a batch of parts."""
        nonlocal rows_placed, beyond_mercator, year_out_of_range, unrecorded_country, named
        nonlocal country_collisions, access_pairs
        m = batch.num_rows
        entity = np.arange(rows_placed, rows_placed + m, dtype=np.uint64)

        lat = batch.column("decimallatitude").combine_chunks().cast(pa.float64())
        lon = batch.column("decimallongitude").combine_chunks().cast(pa.float64())
        beyond_mercator += true_count(pc.greater(pc.abs(lat), MAX_LATITUDE))
        lon_bounds[0] = min(lon_bounds[0], float(pc.min(lon).as_py()))
        lon_bounds[1] = max(lon_bounds[1], float(pc.max(lon).as_py()))
        lat_bounds[0] = min(lat_bounds[0], float(pc.min(lat).as_py()))
        lat_bounds[1] = max(lat_bounds[1], float(pc.max(lat).as_py()))

        # The access column, filled here rather than left to the view's `default` so that a
        # principal holding no term sees nothing. Trimmed for the reason the build trims a label:
        # ` GB` and `GB` are one term rather than two that no credential spells the same way.
        country = pc.fill_null(
            pc.utf8_trim_whitespace(batch.column("countrycode").combine_chunks().cast(pa.string())),
            "",
        )
        country_collisions += true_count(pc.equal(country, UNRECORDED))
        # A country code spelled `y:1999` or `s:2482598` would be the same access term as a year
        # or a species, merging two compartments. Checked over the batch's distinct codes.
        codes = pc.unique(country)
        for prefix in (YEAR_PREFIX, SPECIES_PREFIX):
            hit = pc.fill_null(pc.starts_with(codes, prefix), False)
            if true_count(hit):
                prefixed_countries.update(codes.filter(hit).to_pylist())
        blank = pc.equal(country, "")
        unrecorded_country += true_count(blank)
        country = pc.if_else(blank, UNRECORDED, country)

        # The taxonomy markers must not collide with a name the source wrote. Checked over the
        # batch's distinct values rather than its rows: 10⁶ rows hold ~10⁴ family names.
        for rank in sources.RANKS:
            uniq = pc.unique(batch.column(rank).combine_chunks().cast(pa.string()))
            bad = pc.fill_null(
                pc.or_(pc.equal(uniq, NOT_RECORDED), pc.match_substring(uniq, SEPARATOR)), False
            )
            if true_count(bad):
                rank_collisions[rank].update(uniq.filter(bad).to_pylist())

        # `year` is `int32` on the share and `u16` in the declaration. A value outside the code
        # space is nulled and counted rather than wrapped: a wrapped year would place an
        # observation in a range filter it does not belong to.
        year = batch.column("year").combine_chunks().cast(pa.int32())
        sane = pc.fill_null(
            pc.and_(pc.greater_equal(year, 1), pc.less_equal(year, 65_535)), False
        )
        year_out_of_range += (m - year.null_count) - true_count(sane)
        year = pc.if_else(sane, year, pa.nulls(m, pa.int32())).cast(pa.uint16())

        kingdom = batch.column("kingdom").combine_chunks().cast(pa.string())
        species_key = batch.column("specieskey").combine_chunks().cast(pa.string())
        scientific = batch.column("scientificname").combine_chunks().cast(pa.string())
        named += m - scientific.null_count

        access, terms = access_lists(country, year, species_key)
        access_pairs += len(terms)

        kingdom_census.add(kingdom)
        country_census.add(country)
        species_key_census.add(species_key)
        year_census.add(year)
        access_census.add(terms)

        listed, keys = taxonomy_keys(batch)
        for census, key in zip(level_census, keys):
            census.add(key)
        members.write_table(
            pa.table({"entity": pa.array(entity, pa.uint64()), "key": listed},
                     schema=MEMBER_SCHEMA),
            row_group_size=ROW_GROUP,
        )

        points.write_table(
            pa.table(
                {
                    "entity_id": pa.array(entity, pa.uint64()),
                    "lon": lon,
                    "lat": lat,
                    "countrycode": country,
                    "kingdom": kingdom,
                    "specieskey": species_key,
                    "year": year,
                    "scientificname": scientific,
                    "access": access,
                },
                schema=POINTS_SCHEMA,
            ),
            row_group_size=ROW_GROUP,
        )
        rows_placed += m

    with steps.step("read, place and write"):
        buffer: list[pa.Table] = []
        buffered = 0
        for at, (_path, table) in enumerate(read_parts(chosen, args.workers)):
            rows_read += table.num_rows
            lat = table.column("decimallatitude").combine_chunks().cast(pa.float64())
            lon = table.column("decimallongitude").combine_chunks().cast(pa.float64())
            finite = pc.fill_null(pc.and_(pc.is_finite(lat), pc.is_finite(lon)), False)
            # Filled rather than left null: `filter` drops a null selection and the two counters
            # below read the mask, so a three-valued mask would have them disagree with the file.
            inside = pc.fill_null(
                pc.and_(
                    finite,
                    pc.and_(pc.less_equal(pc.abs(lat), 90.0), pc.less_equal(pc.abs(lon), 180.0)),
                ),
                False,
            )
            placed = true_count(inside)
            no_coordinate += table.num_rows - true_count(finite)
            out_of_range += true_count(finite) - placed
            if placed:
                kept = table.filter(inside)
                buffer.append(kept)
                buffered += placed
            del table
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
    members.close()

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
    # merge real records into the term that stands for *no country*, and one spelled with a year or
    # species prefix would merge two compartments — both access decisions. Each would move records
    # between artifacts or between compartments with no error, so each is a refusal rather than a
    # report.
    collided = {rank: sorted(got)[:5] for rank, got in rank_collisions.items() if got}
    country_bad = []
    if country_collisions:
        country_bad.append(f"{country_collisions:,} rows spell {UNRECORDED!r}")
    country_bad += sorted(prefixed_countries)[:5]
    if country_bad:
        collided["countrycode"] = country_bad
    if collided:
        raise SystemExit(
            f"source value(s) collide with this script's markers ({NOT_RECORDED!r} for a level "
            f"the source did not record, {SEPARATOR!r} between levels, {UNRECORDED!r} for a "
            f"record with no country, {YEAR_PREFIX!r} and {SPECIES_PREFIX!r} before an access "
            f"term's year and species key): {collided}. Choose other markers; a placeholder that "
            f"merges with a real value would move records between artifacts and between "
            f"compartments."
        )

    with steps.step("vocabulary"):
        kingdoms = sorted(k for k, _ in kingdom_census.ranked())
        vocab_size = write_vocabulary(out, "kingdom", kingdoms)
    print(f"kingdom vocabulary: {vocab_size} keys, {kingdom_census.nulls:,} rows carry none "
          f"({kingdom_census.nulls / max(rows_placed, 1):.2%})", flush=True)

    with steps.step("the demo's terms"):
        country_ranks = country_census.ranked()
        write_demo_terms(out, country_ranks)
        access_report = access_census_report(access_census, access_pairs)
    print_access_census(access_report, rows_placed)

    write_declaration(out, size=vocab_size, rows=rows_placed)
    write_deployment(out)

    levels = [
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
        "part_selection": "whole" if whole else ("spread" if args.spread else "prefix"),
        "rows_read": rows_read,
        "rows_placed": rows_placed,
        "placed_share": round(rows_placed / rows_read, 6) if rows_read else None,
        "no_coordinate": no_coordinate,
        "coordinate_out_of_range": out_of_range,
        "beyond_mercator": beyond_mercator,
        "bounds": {"lon": lon_bounds, "lat": lat_bounds},
        "countrycode": {
            "terms": len(country_ranks),
            "unrecorded": unrecorded_country,
            "top": [{"term": t, "pairs": n} for t, n in country_ranks[:5]],
        },
        "access": access_report,
        "kingdom": {"keys": vocab_size, "null_rows": kingdom_census.nulls},
        "specieskey": {
            "distinct": species_key_census.distinct,
            "null_rows": species_key_census.nulls,
        },
        "year": {
            "distinct": year_census.distinct,
            "null_rows": year_census.nulls,
            "out_of_range": year_out_of_range,
        },
        "scientificname": {"present": named},
        "taxonomy": {
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
