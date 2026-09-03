"""TreeOfLife-200M — 233,055,986 specimen images, the ladder's largest rung.

One pass from `staging/metadata.parquet` and `staging/gbif-coordinates.parquet` (the join track's
outputs) to a corpus `tessera build` consumes: **two views over one entity space** — `bioclip`, the
BioCLIP-2 embedding laid out by the fit-and-place route in `routes.py`, and `geo`, the GBIF-joined
rows on Web Mercator — a **seven-level tiered taxonomy** over every row, a flat `publishers/source`
predicate layer over the compartment, and a `clusters/kmeans` layer on the embedding view with a
c-TF-IDF title on every cluster.

**It is a demonstrator and a speed benchmark** (owner ruling, 2026-09-01: speed wins over
accuracy). Recall against an exact neighbour search is not measured and layout fidelity is not
judged. What the rung is for is the volume, the second geometry, and the first seven-level tiered
layer at scale.

Five decisions this stage makes:

- **`publisher` is the access column**, and every row carries a term: a row the source gives no
  publisher — every `bioscan`, `eol` and `fathomnet` row, and any GBIF row whose record names none
  — carries `unpublished`, a key of the same closed vocabulary (owner ruling, 2026-09-03). So the
  column is never empty, `point_visibility`'s `default` never fires, and a principal holding no
  term sees nothing.
- **The `geo` view holds a subset of the entity space.** Only the rows the GBIF join matched *and*
  that carry a coordinate are in `points-geo.parquet`. An entity absent from a view is what
  views.md allows, and it is the point of the rung.
- **Nothing holds a column whole.** `metadata.parquet` is streamed one source row group at a time —
  350,000 rows — with the taxonomy layer's member rows and both points files riding the same pass.
- **The taxonomy keys are built in Arrow, not in Python.** Seven cumulative path strings over
  2.33x10^8 rows is 1.6x10^9 string joins; `pc.binary_join_element_wise` does them per row group in
  C++ and the member file's list column is assembled from dictionary codes with one `take`.
- **The titles are vectorised over a bounded sample** drawn in the same pass. c-TF-IDF is a share
  of a cluster's documents carrying a term, and a uniform subsample estimates that share without
  fitting a 10^8-row document-term matrix (`LABEL_MAX_DOCS`).

⊘ **The layout is not reproducible under a seed**, for rungs 1, 3 and 4's reason: CAGRA's index
build is approximate and takes none, so UMAP is handed a different graph each run.

    ~/venvs/projection/bin/python -m test_corpora.treeoflife.stage --fit    # ~30 min off the share
    ~/venvs/projection/bin/python -m test_corpora.treeoflife.prepare --sample 1000000
    ~/venvs/projection/bin/python -m test_corpora.treeoflife.prepare --sample 0 --layout-only
    ~/venvs/projection/bin/python -m test_corpora.treeoflife.prepare --sample 0 --reuse-layout

**The layout is separable from the rest on purpose.** The placement pass is ~3.5 hours of share
I/O and a GPU that may run beside another track's build; the k-means and the corpus write may not.
`--layout-only` stops after `layout-bioclip.npy` and `--reuse-layout` starts from it.
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import resource
import sys
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from ..arxiv.writer import ArtifactSet
from ..common.paths import ladder
from ..common.timing import Steps
from . import routes, sources

RUNG = sources.RUNG
SEED = 0

VIEWS = ("bioclip", "geo")
KMEANS_LAYER = "clusters/kmeans"
TAXONOMY_LAYER = "taxonomy/tree"

#: The access term a row with no publisher carries, and a key of the closed `publisher` vocabulary
#: (owner ruling, 2026-09-03). It exists so that the access column is never empty: **a principal
#: holding no term must see nothing.** Written as a real key rather than left to the view's
#: `default` because the campaign's principal ladder starts at 1% of the corpus and cannot be
#: composed under a floor every principal holds for free.
UNPUBLISHED = "unpublished"

#: The key a level takes where the source recorded no name for it but recorded one below it.
#: GeoNames' rule and GeoNames' reason: `parent_edges` is `windows(2)` and does not read past a
#: gap, so a null there would state a containment no row makes. A level whose chain simply *ends*
#: stays null.
NOT_RECORDED = "NOT_RECORDED"

#: What separates the levels of a cumulative taxonomy key. A vertical bar rather than GeoNames'
#: dot: a scientific name carries dots (`cf.`, `sp.`) and would split a key into the wrong levels.
SEPARATOR = "|"

#: k-means cells at full scale, scaled with the sample by the square root of the row ratio — rung
#: 3's rule, for its reason: a linear scaling puts a 1M sample at one cell, which draws nothing.
KMEANS_K_FULL = 256
KMEANS_K_MIN = 16

LABEL_TERMS = 4  #: terms in one title
LABEL_SAMPLE = 200  #: documents a title's generating set names
MAX_CORPUS_SHARE = 0.02  #: above this a term is corpus vocabulary, not cluster vocabulary
MIN_CLUSTER_SHARE = 0.02  #: below this it is a coincidence rather than a description

#: Documents the c-TF-IDF vectoriser sees, sampled uniformly in the corpus pass.
LABEL_MAX_DOCS = 4_000_000

#: Tokens that name a specimen record rather than a clade. Kept short on purpose — the c-TF-IDF
#: denominator is what is meant to do this work, and a stoplist that grows is a labeller being
#: hand-tuned.
CORPUS_STOPLIST = frozenset("""
specimen specimens sp spp cf var subsp indet unknown unidentified common
""".split())

FALLBACK = "a cluster of specimens"

#: Rows per row group in the two points files — one source row group of `metadata.parquet`, which
#: is one source file of the share.
POINTS_ROW_GROUP = 350_000

#: `points.parquet`'s schema, fixed rather than inferred, because it is written a row group at a
#: time and a group whose `common_name` column happened to be all-null would otherwise change it.
POINTS_SCHEMA = pa.schema(
    [
        pa.field("entity_id", pa.uint64()),
        pa.field("x", pa.float64()),
        pa.field("y", pa.float64()),
        pa.field("publisher", pa.string()),
        *[pa.field(rank, pa.string()) for rank in sources.RANKS],
        pa.field("source_dataset", pa.string()),
        pa.field("basisOfRecord", pa.string()),
        pa.field("img_type", pa.string()),
        pa.field("scientific_name", pa.string()),
        pa.field("common_name", pa.string()),
        pa.field("uuid", pa.string()),
    ]
)

#: The `geo` view's own file: identity, the publisher's coordinates in degrees, and the access
#: column. Every attribute is entity-space and rides the anchor view's file alone.
GEO_SCHEMA = pa.schema(
    [
        pa.field("entity_id", pa.uint64()),
        pa.field("lon", pa.float64()),
        pa.field("lat", pa.float64()),
        pa.field("publisher", pa.string()),
    ]
)

#: The taxonomy layer's member file: one row per specimen, `key` a seven-entry list whose positions
#: are the declared levels.
MEMBER_SCHEMA = pa.schema(
    [pa.field("entity", pa.uint64()), pa.field("key", pa.list_(pa.string()))]
)


def contents_of(text: str | None) -> list[list[str]]:
    """A cluster's ranked contents: the description where there is one, the fallback last.

    Never empty. The layer declares a supplied kind, and an artifact carrying none of a kind its
    layer declares is refused at publication — it could not be told apart from one whose content
    was withheld.
    """
    return [[text], [FALLBACK]] if text else [[FALLBACK]]


def kmeans_k(n: int, corpus: int) -> int:
    return max(KMEANS_K_MIN, min(KMEANS_K_FULL, round(KMEANS_K_FULL * (n / corpus) ** 0.5)))


def peak_gb() -> float:
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20


# ---------------------------------------------------------------------------------- the labels


class Labeller:
    """Cluster titles by **c-TF-IDF against the layer's own sibling clusters** — how much more a
    term occurs in this cluster than across the clusters it is drawn beside.

    Transplanted from rungs 1, 3 and 4, where the reasoning is written out at length: plain TF-IDF
    over each cluster as one long document ranks by whatever is rarest and returns real tokens that
    describe nothing, and a denominator taken over the whole corpus rather than the siblings
    returns the register of the writing rather than the cluster's subject.

        score(t, C) = f(t, C) · log( f(t, C) / mean_C' f(t, C') )

    **The documents here are names**, not prose: a specimen's scientific name and its common name
    joined. So a cluster's title is drawn from the clade names its members carry, which is the
    right vocabulary for a map of the tree of life and is also the only text this rung has.
    """

    def __init__(self, titles: list[str | None], rows: np.ndarray, n: int):
        """`rows` are the corpus rows `titles` came from, over a corpus of `n`, so a caller can map
        a cluster's members into the vectoriser's row space with one array lookup."""
        from sklearn.feature_extraction.text import ENGLISH_STOP_WORDS, CountVectorizer

        self.rows = rows
        self.of_row = np.full(n, -1, dtype=np.int64)
        self.of_row[rows] = np.arange(len(rows))
        vec = CountVectorizer(
            stop_words=list(ENGLISH_STOP_WORDS | CORPUS_STOPLIST),
            token_pattern=r"(?u)\b[a-zA-Z][a-zA-Z-]{2,}\b",
            min_df=10,
            max_df=MAX_CORPUS_SHARE,
            binary=True,
        )
        self.occurs = vec.fit_transform("" if t is None else t for t in titles)
        self.vocab = np.array(vec.get_feature_names_out())

    def sampled(self, members: np.ndarray) -> np.ndarray:
        """A cluster's members, mapped into the vectoriser's rows and dropping those it never saw."""
        got = self.of_row[members]
        return got[got >= 0]

    def label(self, groups: dict, terms: int = LABEL_TERMS) -> dict:
        """`{key: [vectoriser rows]}` -> `{key: "term term term term"}`, omitting the keys with
        nothing distinctive to say.

        **The whole layer at once, because the denominator is the layer.** Every group's share is
        computed first and the background is their mean, each cluster counting once whatever its
        size — pooling instead would let one cluster holding half the layer set the background it
        is then scored against.
        """
        keys = [k for k, rows in groups.items() if len(rows)]
        shares = np.empty((len(keys), len(self.vocab)), dtype=np.float64)
        for i, key in enumerate(keys):
            rows = np.asarray(groups[key])
            shares[i] = np.asarray(self.occurs[rows].sum(axis=0)).ravel() / len(rows)
        background = shares.mean(axis=0)

        out = {}
        for i, key in enumerate(keys):
            share = shares[i]
            score = np.where(
                share >= MIN_CLUSTER_SHARE,
                share * np.log(share / (background + 1e-9) + 1e-9),
                -np.inf,
            )
            top = np.argsort(score)[::-1][:terms]
            chosen = [self.vocab[t] for t in top if np.isfinite(score[t])]
            if chosen:
                out[key] = " ".join(chosen)
        return out


# --------------------------------------------------------------------------------- the inputs


def stream_metadata(take: np.ndarray | None, columns: list[str]):
    """`(global rows, table)` per row group of `staging/metadata.parquet`, in entity order.

    `take` is a sorted array of global rows, or `None` for every row. A row group is one source
    file — 350,000 rows — and the rows wanted from it are a contiguous span of `take`, so the group
    is taken with one gather and released before the next is read.

    An identity gather is skipped rather than performed: at `--sample 0` every row is wanted, and
    `take` over a whole group would copy the group to produce what was read.
    """
    path = sources.staging() / "metadata.parquet"
    f = pq.ParquetFile(path)
    at = 0
    for g in range(f.metadata.num_row_groups):
        rows = f.metadata.row_group(g).num_rows
        if take is None:
            table = f.read_row_group(g, columns=columns)
            yield np.arange(at, at + rows, dtype=np.int64), table
        else:
            a = int(np.searchsorted(take, at))
            b = int(np.searchsorted(take, at + rows))
            if b > a:
                table = f.read_row_group(g, columns=columns)
                want = take[a:b]
                if len(want) != rows:
                    table = table.take(pa.array(want - at))
                yield want, table
                del table
        at += rows
    assert at == sources.TOTAL_ROWS, f"metadata.parquet holds {at:,} rows"


def load_geo(take: np.ndarray | None, n: int) -> tuple[np.ndarray, np.ndarray, int]:
    """`(lon, lat, matched)` per entity index, NaN where the row is not in the `geo` view.

    Streams `staging/gbif-coordinates.parquet` — 205,901,893 rows, of which 176,899,537 carry a
    coordinate — and keeps two float64 arrays of `n`. At full scale that is 3.73 GB and it is the
    largest thing this script holds beside the layout; a run with a `take` keeps only the sample's.

    ⊘ **A matched row with no latitude is not in the view.** The join records 205,901,893 matches
    against 176,899,537 with coordinates, and the difference is GBIF occurrences with no
    `decimallatitude` — an absence of geography, not a geography of zero.
    """
    lon = np.full(n, np.nan, dtype=np.float64)
    lat = np.full(n, np.nan, dtype=np.float64)
    path = sources.staging() / "gbif-coordinates.parquet"
    f = pq.ParquetFile(path)
    matched = 0
    for g in range(f.metadata.num_row_groups):
        batch = f.read_row_group(g, columns=["row", "lat", "lon"])
        matched += batch.num_rows
        keep = pc.and_(pc.is_valid(batch.column("lat")), pc.is_valid(batch.column("lon")))
        batch = batch.filter(keep)
        if not batch.num_rows:
            continue
        rows = np.asarray(batch.column("row").combine_chunks(), dtype=np.int64)
        got_lat = np.asarray(batch.column("lat").combine_chunks(), dtype=np.float64)
        got_lon = np.asarray(batch.column("lon").combine_chunks(), dtype=np.float64)
        if take is None:
            lat[rows] = got_lat
            lon[rows] = got_lon
        else:
            at = np.searchsorted(take, rows)
            np.clip(at, 0, len(take) - 1, out=at)
            hit = take[at] == rows
            lat[at[hit]] = got_lat[hit]
            lon[at[hit]] = got_lon[hit]
        del batch, rows, got_lat, got_lon
    return lon, lat, matched


# --------------------------------------------------------------------------------- the outputs


def taxonomy_keys(table: pa.Table) -> pa.Array:
    """A row group's member-file list column — the seven cumulative path keys, per row.

    Level `j`'s key is the first `j + 1` ranks joined, each rank replaced by `NOT_RECORDED` where
    the source left it null, and the whole key null where **nothing at or below level `j`** was
    recorded — GeoNames' rule: a hole in the chain becomes an artifact and a chain that simply ends
    stays null.

    **Built in Arrow.** The joins are `pc.binary_join_element_wise` per level, and the list column
    is one `take` against the seven levels' pooled dictionaries — a Python loop over 2.33x10^8 rows
    and seven levels is 1.6x10^9 string operations and would be the whole run.
    """
    n = table.num_rows
    ranks = [table.column(r).combine_chunks().cast(pa.string()) for r in sources.RANKS]
    marked = [pc.fill_null(r, NOT_RECORDED) for r in ranks]

    # `present[j]` — is anything at or below level j recorded? Folded from the deepest rank up, so
    # each level is one OR against the level below rather than a fresh scan of the tail.
    present: list[pa.Array] = [None] * len(ranks)
    below = pc.is_valid(ranks[-1])
    present[-1] = below
    for j in range(len(ranks) - 2, -1, -1):
        below = pc.or_(pc.is_valid(ranks[j]), below)
        present[j] = below

    keys: list[pa.Array] = []
    joined = marked[0]
    for j in range(len(ranks)):
        if j:
            joined = pc.binary_join_element_wise(joined, marked[j], SEPARATOR)
        keys.append(pc.if_else(present[j], joined, pa.nulls(n, pa.string())))

    # One pooled dictionary over the seven levels, so the interleave below is an integer transpose
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
    return listed


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


def write_demo_terms(out: Path, counts: collections.Counter) -> None:
    """**The demo's and the measurement driver's candidate terms, ranked by coverage.** A term id
    names a different set in every dictionary, so a corpus with its own dictionary has to name its
    own terms or every principal measures empty against a synthetic `0..200` and the viewer opens
    on a blank map. `serve_battery.py` composes its 1–100% principals from this file.
    """
    ranks = counts.most_common()
    (out / "branch-ranks.json").write_text(
        json.dumps([{"term": t, "pairs": n} for t, n in ranks], indent=None) + "\n"
    )
    # ⊘ **No `branch-terms.txt` on this rung.** The other rungs write the same roster a second
    # time as one comma-separated line, which is what `run_demo.sh --terms` reads. A publisher's
    # name carries commas — "Museum of Comparative Zoology, Harvard University" — so that file
    # would split 376 terms into 444 and every principal composed from it would name institutions
    # that do not exist. `branch-ranks.json` is JSON and is what the measurement drivers read.
    commas = sum(1 for t, _ in ranks if "," in t)
    (out / "branch-ranks.md").write_text(
        f"`branch-terms.txt` is not written for this rung: {commas} of {len(ranks)} publisher "
        f"names contain a comma, and the comma-separated form the other rungs write would split "
        f"them into terms nobody holds. Read `branch-ranks.json`.\n"
    )
    print(f"{len(ranks)} publisher terms ({commas} carry a comma, so no branch-terms.txt); "
          "top five " + ", ".join(f"{t} {n:,}" for t, n in ranks[:5]))


def vocabulary_toml(sizes: dict[str, int]) -> str:
    """The eleven `[[vocabulary]]` blocks, with the sizes this run actually minted.

    A width is the size of the code space and not bytes in every row, so it follows the count: a
    rank with more than 65,535 values takes `u32` whether or not it is rendered.
    """
    def width(n: int) -> str:
        return "u8" if n < 255 else ("u16" if n < 65_535 else "u32")

    titles = {
        "publisher": "Publisher",
        "source-dataset": "Source dataset",
        "basis": "Basis of record",
        "img-type": "Image type",
        **{r: r.capitalize() for r in sources.RANKS},
    }
    sources_of = {
        "publisher": "publisher",
        "source-dataset": "source_dataset",
        "basis": "basis",
        "img-type": "img_type",
        **{r: r for r in sources.RANKS},
    }
    names = {
        "publisher": "publisher",
        "source-dataset": "source_dataset",
        "basis": "basis",
        "img-type": "img_type",
        **{r: r for r in sources.RANKS},
    }
    blocks = [f"# Minted by this run from {sources.TOTAL_ROWS:,} rows of staging/metadata.parquet."]
    for key in ("publisher", "source-dataset", "basis", "img-type", *sources.RANKS):
        n = sizes[key]
        blocks.append(
            "[[vocabulary]]\n"
            f'name       = "{names[key]}"\n'
            f'title      = "{titles[key]}"  # {n:,} keys\n'
            f'width      = "{width(n)}"\n'
            'value_set  = "closed"\n'
            f'visibility = "{"derived" if key == "publisher" else "public"}"\n'
            f'source     = "{sources_of[key]}"'
        )
    return "\n\n".join(blocks)


KMEANS_TOML = """# ---------------------------------------------------------------------------------------------
# **The layer that certainly draws.** k-means over the 2D layout is compact in it by construction,
# which is the test a layer has to pass to earn its place (the ruling that withdrew Overture's and
# arXiv's taxonomies). The taxonomy above is the layer the corpus exists for; this one is what
# guarantees the embedding view has something on it.
#
# **On `bioclip` alone.** The cells are cut in the embedding layout, so on the world map they would
# be seven hundred disjoint patches with no meaning — a cluster is a property of one geometry here,
# where a clade is a property of the specimen.
#
# **A cluster carries its own title** as ranked supplied content — the c-TF-IDF description first,
# a generic fallback second — rather than a label artifact on a layer of its own that a client had
# to join by attachment.
# ---------------------------------------------------------------------------------------------

[[layer]]
name       = "clusters/kmeans"
title      = "k-means clusters"
source     = "kmeans"
views      = ["bioclip"]
membership = "enumerated"
hierarchy  = { kind = "flat", prune_children = false }

visibility          = "public"
artifact_visibility = { default = "inherited" }
# The absolute form: a fixed floor, whatever the cluster's size.
require_member_visibility = { count = 50 }

  [layer.members]
  source = "kmeans_members"

  [layer.content]
  computed = ["centroid", "box", "hull"]

    [[layer.content.supplied]]
    name                      = "topic"
    type                      = "text"
    require_member_visibility = "all"
"""


def write_declaration(out: Path, *, sizes: dict[str, int], kmeans: bool) -> None:
    """`corpus.toml`, copied beside the data and edited for what this run actually wrote."""
    def fill(text: str, marker: str, block: str) -> str:
        """Replace the **whole line** that is the marker. A plain `str.replace` would also hit the
        header comment that names the marker, which is how a TOML block landed inside a comment and
        refused a build at rung 3."""
        lines = text.splitlines()
        hit = [i for i, line in enumerate(lines) if line.strip() == marker]
        assert len(hit) == 1, f"{marker} appears {len(hit)} times as a line of its own"
        lines[hit[0]] = block
        return "\n".join(lines) + "\n"

    text = (Path(__file__).parent / "corpus.toml").read_text()
    text = fill(text, "# <vocabularies>", vocabulary_toml(sizes))
    text = fill(text, "# <kmeans-layer>", KMEANS_TOML if kmeans else
                "# ⊘ `clusters/kmeans` is not declared: this run reused a layout and wrote no\n"
                "# clustering, so the embedding view carries the taxonomy and the publishers alone.")
    (out / "corpus.toml").write_text(text)


def write_deployment(out: Path) -> None:
    """`tessera.toml`, generated rather than committed — every value in it is a path or a port on
    this machine, which is the split `configuration.md` §3 draws.

    **Ports 8141–8143**, so this rung's server can run beside rung 3's on 8111–8113, the memory-cap
    probe's on 8121–8123 and rung 4's on 8131–8133 without any of them being disturbed.
    """
    (out / "tessera.toml").write_text(
        """# Generated by `test_corpora/treeoflife/prepare.py`. Machine-specific by construction: paths
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
viewer  = "127.0.0.1:8141"
session = "127.0.0.1:8142"
control = "127.0.0.1:8143"
max_k   = 5000
session_credential_env  = "TESSERA_TREEOFLIFE_SESSION_CRED"
operator_credential_env = "TESSERA_TREEOFLIFE_OPERATOR_CRED"

# Development only: the origin the demo viewer is served from (client-interaction §7). Without
# it the viewer loads and every request from it fails CORS, which reads like a broken server.
dev_cors_origins = ["http://localhost:PORT", "http://127.0.0.1:PORT"]
"""
        .replace("PORT", os.environ.get("VITE_PORT", "5178"))
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
        for var in ("TESSERA_IDENTITY_KEY", "TESSERA_TREEOFLIFE_SESSION_CRED",
                    "TESSERA_TREEOFLIFE_OPERATOR_CRED")
        if var not in held
    ]
    if minted:
        env.write_text("\n".join(lines + minted) + "\n")
        env.chmod(0o600)
        print(f"minted {', '.join(m.split('=')[0] for m in minted)} in {env}")


def repeated(key: str, n: int) -> pa.Array:
    """`n` copies of one artifact key, without `n` Python strings.

    A dictionary array taken against a one-value dictionary is a vectorised gather; the alternative
    spelling — `pa.array([key] * n)` — materialises a list of 10^6 interned strings per cluster.
    """
    return pa.DictionaryArray.from_arrays(
        pa.array(np.zeros(n, dtype=np.int32), pa.int32()), pa.array([key], pa.string())
    ).cast(pa.string())


def cluster(xy: np.ndarray, k: int) -> tuple[np.ndarray, str]:
    """k-means over the layout, whole where the card takes it and fitted on a sample where it does
    not.

    2.33x10^8 x 2 float32 is 1.86 GB of coordinates on a 10 GB card before the algorithm's own
    working set, so the whole-corpus fit is **tried** and the route that ran is recorded rather than
    assumed — an out-of-memory here would otherwise cost the run the placement pass it has already
    paid three hours for.
    """
    from cuml.cluster import KMeans

    try:
        model = KMeans(n_clusters=k, random_state=SEED, n_init=1).fit(xy)
        return np.asarray(model.labels_).astype(np.int32), "whole"
    except Exception as exc:  # noqa: BLE001 — any device failure takes the sampled route
        print(f"  ⊘ whole-corpus k-means failed ({type(exc).__name__}: {exc}); "
              "fitting on a sample and predicting in batches", flush=True)
    sample = np.sort(np.random.default_rng(SEED).choice(len(xy), 10_000_000, replace=False))
    model = KMeans(n_clusters=k, random_state=SEED, n_init=1).fit(xy[sample])
    out = np.empty(len(xy), dtype=np.int32)
    for lo in range(0, len(xy), 10_000_000):
        out[lo : lo + 10_000_000] = np.asarray(model.predict(xy[lo : lo + 10_000_000]))
    return out, "fitted on 10,000,000 and predicted in batches"


# ------------------------------------------------------------------------------------- the run


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--sample", type=int, default=1_000_000,
                    help=f"rows to take, drawn from the staged fit sample; 0 takes all "
                         f"{sources.TOTAL_ROWS:,}")
    ap.add_argument("--out", type=Path, default=None, help=f"default $TESSERA_LADDER/{RUNG}")
    ap.add_argument("--managed", action="store_true",
                    help="RMM managed memory for the layout, so the card oversubscribes")
    ap.add_argument("--layout-only", action="store_true",
                    help="stop once layout-bioclip.npy is written — the share pass and the GPU, "
                         "which may run beside another track's build where the rest may not")
    ap.add_argument("--reuse-layout", action="store_true",
                    help="read layout-bioclip.npy instead of routing")
    ap.add_argument("--no-kmeans", action="store_true",
                    help="write no clustering — for a run on a box with no free card")
    args = ap.parse_args()

    out = args.out or ladder(RUNG)
    out.mkdir(parents=True, exist_ok=True)
    steps = Steps()
    print(f"staging {sources.staging()}\noutput  {out}", flush=True)

    n_full = sources.TOTAL_ROWS
    fit_matrix, fit_rows, fit_meta = sources.fit_matrix()

    # ---------------------------------------------------------------------- the corpus and sample
    #
    # **A sample is drawn from the staged fit rows and not from the corpus.** 2,500,000 of the
    # 233,055,986 rows are local; every other row's vector is on the share, and a scattered
    # 10^6-row draw would touch every data page of every one of the 666 files — a three-hour pass
    # to sample what the staged file already holds uniformly over all of them.
    with steps.step("sample"):
        if not args.sample or args.sample >= n_full:
            take, n, whole = None, n_full, True
        else:
            whole = False
            assert args.sample <= len(fit_rows), (
                f"--sample {args.sample:,} is more than the {len(fit_rows):,} staged fit rows and "
                f"less than the corpus. Either stage a larger fit sample or take the whole corpus."
            )
            at_fit = np.sort(
                np.random.default_rng(SEED).choice(len(fit_rows), args.sample, replace=False)
            )
            take = fit_rows[at_fit].astype(np.int64)
            n = len(take)
    print(f"{n:,} specimens {'(the whole corpus)' if whole else 'sampled from the fit set'}",
          flush=True)

    # ------------------------------------------------------------------------------- the route
    # **Before anything else.** The route holds the fit block and the graph; everything after it is
    # Arrow buffers, and the two peaks should not meet.
    timings: dict = {}
    layout_path = out / "layout-bioclip.npy"
    if args.reuse_layout:
        xy = np.load(layout_path)
        assert len(xy) == n, f"{layout_path} holds {len(xy):,} positions against {n:,} rows"
        print(f"  reusing {layout_path}", flush=True)
    else:
        with steps.step("route bioclip"):
            t: dict = {}
            if whole:
                block = np.ascontiguousarray(fit_matrix)
                fit_xy = routes.fit_layout(block, t, managed=args.managed)
                xy = routes.place_from_share(
                    block, fit_xy, fit_rows.astype(np.int64), n, t, sources.treeoflife_files()
                )
                del block, fit_xy
            else:
                block = np.ascontiguousarray(fit_matrix[at_fit])
                xy = routes.fit_layout(block, t, managed=args.managed)
                del block
            timings["bioclip"] = t
        np.save(layout_path, xy)
    del fit_matrix
    print(f"  route bioclip: x [{xy[:, 0].min():.2f}, {xy[:, 0].max():.2f}] "
          f"y [{xy[:, 1].min():.2f}, {xy[:, 1].max():.2f}]  (peak so far {peak_gb():.1f} GB)",
          flush=True)
    if args.layout_only:
        print(f"\n--layout-only: wrote {layout_path} "
              f"({layout_path.stat().st_size / 1e9:.2f} GB). Rerun with --reuse-layout.")
        return

    # ------------------------------------------------------------------ the GBIF join's geometry
    with steps.step("geo coordinates"):
        lon, lat, matched = load_geo(take, n)
        in_geo = int(np.isfinite(lat).sum())
    print(f"geo view: {in_geo:,} of {n:,} ({in_geo / n:.2%}) rows carry a coordinate, from "
          f"{matched:,} matched GBIF records  (peak {peak_gb():.1f} GB)", flush=True)

    # ------------------------------------------------------- the one pass: points, layer, labels
    #
    # **Everything the staged metadata is needed for happens here, once.** For each row group: the
    # taxonomy layer's member rows, one row group of `points.parquet`, the `geo` view's rows, the
    # vocabularies' value sets, the publisher census, and the label sample's names.
    label_rows = (
        np.arange(n)
        if n <= LABEL_MAX_DOCS
        else np.sort(np.random.default_rng(SEED + 1).choice(n, LABEL_MAX_DOCS, replace=False))
    )
    label_text: list[str] = []
    values: dict[str, set] = {k: set() for k in
                             ("publisher", "source_dataset", "basis", "img_type", *sources.RANKS)}
    publisher_counts: collections.Counter = collections.Counter()
    named = {"scientific_name": 0, "common_name": 0}
    columns = ["publisher", "source_dataset", "basisOfRecord", "img_type", "scientific_name",
               "common_name", "uuid", *sources.RANKS]

    points = pq.ParquetWriter(out / "points.parquet", POINTS_SCHEMA, compression="zstd",
                              use_dictionary=["publisher", "source_dataset", "basisOfRecord",
                                              "img_type", "scientific_name", *sources.RANKS])
    geo = pq.ParquetWriter(out / "points-geo.parquet", GEO_SCHEMA, compression="zstd",
                           use_dictionary=["publisher"])
    members = pq.ParquetWriter(out / "members-taxonomy.parquet", MEMBER_SCHEMA,
                               compression="zstd")
    at = 0
    with steps.step("points, taxonomy and the geo view"):
        for want, table in stream_metadata(take, columns):
            m = table.num_rows
            entity = np.arange(at, at + m, dtype=np.uint64)

            publisher = pc.fill_null(
                table.column("publisher").combine_chunks().cast(pa.string()), UNPUBLISHED
            )
            counts = pc.value_counts(publisher)
            publisher_counts.update(dict(zip(counts.field("values").to_pylist(),
                                             counts.field("counts").to_pylist())))
            for name, column in (("publisher", publisher),
                                 ("source_dataset", table.column("source_dataset")),
                                 ("basis", table.column("basisOfRecord")),
                                 ("img_type", table.column("img_type")),
                                 *[(r, table.column(r)) for r in sources.RANKS]):
                values[name].update(
                    v for v in pc.unique(column).to_pylist() if v is not None
                )
            named["scientific_name"] += m - table.column("scientific_name").null_count
            named["common_name"] += m - table.column("common_name").null_count

            listed = taxonomy_keys(table)
            members.write_table(
                pa.table({"entity": pa.array(entity, pa.uint64()), "key": listed},
                         schema=MEMBER_SCHEMA)
            )

            points.write_table(
                pa.table(
                    {
                        "entity_id": pa.array(entity, pa.uint64()),
                        "x": pa.array(xy[at : at + m, 0].astype(np.float64), pa.float64()),
                        "y": pa.array(xy[at : at + m, 1].astype(np.float64), pa.float64()),
                        "publisher": publisher,
                        **{r: table.column(r).combine_chunks().cast(pa.string())
                           for r in sources.RANKS},
                        "source_dataset":
                            table.column("source_dataset").combine_chunks().cast(pa.string()),
                        "basisOfRecord":
                            table.column("basisOfRecord").combine_chunks().cast(pa.string()),
                        "img_type": table.column("img_type").combine_chunks().cast(pa.string()),
                        "scientific_name":
                            table.column("scientific_name").combine_chunks().cast(pa.string()),
                        "common_name":
                            table.column("common_name").combine_chunks().cast(pa.string()),
                        "uuid": table.column("uuid").combine_chunks().cast(pa.string()),
                    },
                    schema=POINTS_SCHEMA,
                ),
                row_group_size=POINTS_ROW_GROUP,
            )

            here = np.flatnonzero(np.isfinite(lat[at : at + m]))
            if len(here):
                geo.write_table(
                    pa.table(
                        {
                            "entity_id": pa.array(entity[here], pa.uint64()),
                            "lon": pa.array(lon[at + here], pa.float64()),
                            "lat": pa.array(lat[at + here], pa.float64()),
                            "publisher": publisher.take(pa.array(here)),
                        },
                        schema=GEO_SCHEMA,
                    ),
                    row_group_size=POINTS_ROW_GROUP,
                )

            # The label sample's documents: a scientific name and a common name joined, for the
            # rows this run drew. Taken here rather than in a second pass over 8.8 GB of metadata.
            a = int(np.searchsorted(label_rows, at))
            b = int(np.searchsorted(label_rows, at + m))
            if b > a:
                picked = label_rows[a:b] - at
                sci = table.column("scientific_name").combine_chunks().take(pa.array(picked))
                com = table.column("common_name").combine_chunks().take(pa.array(picked))
                label_text.extend(
                    " ".join(x for x in (s, c) if x)
                    for s, c in zip(sci.to_pylist(), com.to_pylist())
                )
                del sci, com

            at += m
            del table, publisher, listed
            if (at // 10_000_000) != ((at - m) // 10_000_000):
                print(f"    {at:,}/{n:,}  ({peak_gb():.1f} GB)", flush=True)
    points.close()
    geo.close()
    members.close()
    assert at == n, f"wrote {at:,} points against {n:,}"
    assert len(label_text) == len(label_rows), (
        f"the label sample lost rows: {len(label_text)} against {len(label_rows)}"
    )
    del lon, lat
    print(f"named: {named['scientific_name']:,} scientific ({named['scientific_name'] / n:.1%}), "
          f"{named['common_name']:,} common ({named['common_name'] / n:.1%})", flush=True)

    # ------------------------------------------------------------------------------ the clustering
    artifacts = ArtifactSet()
    k = kmeans_k(n, n_full)
    text: dict = {}
    drawn: list[int] = []
    kmeans_route = "not run"
    sizes_kmeans = np.zeros(0, dtype=np.int64)
    if not args.no_kmeans:
        with steps.step("k-means"):
            label, kmeans_route = cluster(xy, k)
        sizes_kmeans = np.bincount(label, minlength=k)
        # One stable argsort rather than `k` passes over the label array: at 2.3x10^8 rows and 256
        # cells the naive form is 6x10^10 comparisons for a grouping the sort already gives.
        order = np.argsort(label, kind="stable")
        bounds = np.concatenate([[0], np.cumsum(sizes_kmeans)])
        grouped = {c: order[bounds[c] : bounds[c + 1]] for c in range(k)}
        del label
        print(f"{k} clusters by {kmeans_route}, sizes {sizes_kmeans.min():,}.."
              f"{sizes_kmeans.max():,} (median {int(np.median(sizes_kmeans)):,})", flush=True)

        with steps.step("vectorise names"):
            labeller = Labeller(label_text, label_rows, n)
        print(f"{len(labeller.vocab):,} candidate terms over {len(label_rows):,} names", flush=True)
        with steps.step("titles"):
            text = labeller.label({c: labeller.sampled(rows) for c, rows in grouped.items()})
        print(f"{len(text)} of {k} clusters got a distinctive title", flush=True)
        del labeller

        # **A cluster with no members is not declared.** Its content would carry a generating set
        # of nothing, and content under `require_member_visibility = "all"` with an empty set is
        # satisfied by every viewer — which the build refuses, rightly.
        drawn = [c for c in range(k) if len(grouped[c])]
        if len(drawn) != k:
            print(f"⊘ {k - len(drawn)} of {k} k-means cells hold no member and are not declared",
                  flush=True)
        with steps.step("write artifacts"):
            for c in drawn:
                artifacts.artifact(KMEANS_LAYER, f"km-{c:06d}", contents=contents_of(text.get(c)))
        with steps.step("write members"):
            rng = np.random.default_rng(SEED)
            for c in drawn:
                key = f"km-{c:06d}"
                rows = grouped[c]
                artifacts.stream_members(KMEANS_LAYER, out, repeated(key, len(rows)), rows)
                pick = rows if len(rows) <= LABEL_SAMPLE else rng.choice(rows, LABEL_SAMPLE,
                                                                        replace=False)
                for rank in range(len(contents_of(text.get(c)))):
                    sub = pick[: max(1, len(pick) // 3**rank)]
                    artifacts.stream_members(KMEANS_LAYER, out, repeated(key, len(sub)), sub,
                                             rank=rank)
            artifacts.close_streams()
        del grouped, order
    artifacts.check(n)
    artifact_rows, member_rows = artifacts.write(out) if artifacts.per_layer else (0, 0)

    # ---------------------------------------------------------------------------- the vocabularies
    #
    # **The markers must not collide with a name the source wrote.** A rank value spelled
    # `NOT_RECORDED` would merge with the placeholder for a level nobody recorded, and one carrying
    # the separator would split a key at the wrong level. Both are refusals rather than reports:
    # either would put specimens in a clade they are not in, silently.
    clashes = {
        rank: sorted(v for v in values[rank] if v == NOT_RECORDED or SEPARATOR in v)[:5]
        for rank in sources.RANKS
    }
    clashes = {rank: got for rank, got in clashes.items() if got}
    if clashes:
        raise SystemExit(
            f"rank value(s) collide with this script's taxonomy-key markers "
            f"({NOT_RECORDED!r} for a level the source did not record, {SEPARATOR!r} between "
            f"levels): {clashes}. Choose other markers; a placeholder that merges with a real "
            f"clade would move specimens between artifacts with no error."
        )

    with steps.step("vocabularies"):
        vocab_sizes = {}
        for name in ("source_dataset", "basis", "img_type", *sources.RANKS):
            file = {"source_dataset": "source-dataset", "img_type": "img-type"}.get(name, name)
            vocab_sizes[file] = write_vocabulary(out, file, sorted(values[name]))
        # The compartment's roster, `unpublished` included: it is a key of the vocabulary rather
        # than the absence of one, which is what makes a principal holding no term see nothing.
        vocab_sizes["publisher"] = write_vocabulary(out, "publisher", sorted(values["publisher"]))
    print("vocabularies: " + ", ".join(f"{k} {v:,}" for k, v in vocab_sizes.items()), flush=True)

    with steps.step("the demo's terms"):
        write_demo_terms(out, publisher_counts)

    write_declaration(out, sizes=vocab_sizes, kmeans=bool(drawn))
    write_deployment(out)

    taxonomy_rows = pq.ParquetFile(out / "members-taxonomy.parquet").metadata.num_rows
    manifest = {
        "rung": RUNG,
        "sample": n,
        "corpus": n_full,
        "seed": SEED,
        "views": list(VIEWS),
        "geo_rows": in_geo,
        "geo_share": round(in_geo / n, 6),
        "gbif_matched": matched,
        "named": named,
        "umap": dict(routes.UMAP_PARAMS, seed=routes.SEED, implementation="cuml",
                     route=("fit on the staged 2.5M sample, place every row against its CAGRA "
                            "index off the share" if whole else "fit over the sample itself"),
                     graph="cagra fp16", fit_rows=fit_meta["rows"],
                     reused_layout=bool(args.reuse_layout), managed_memory=args.managed),
        "routes": timings,
        "bounds": {"bioclip": {"x": [float(xy[:, 0].min()), float(xy[:, 0].max())],
                               "y": [float(xy[:, 1].min()), float(xy[:, 1].max())]}},
        "kmeans": {"k": k, "declared": len(drawn), "route": kmeans_route, "titles": len(text),
                   "sizes": ([int(sizes_kmeans.min()), int(np.median(sizes_kmeans)),
                              int(sizes_kmeans.max())] if len(sizes_kmeans) else None)},
        "vocabularies": vocab_sizes,
        "publisher_terms": len(publisher_counts),
        "unpublished": publisher_counts.get(UNPUBLISHED, 0),
        "taxonomy_member_rows": taxonomy_rows,
        "artifact_rows": artifact_rows,
        "member_rows": member_rows,
        "seconds": dict(steps),
        "total_seconds": steps.total(),
        "peak_rss_gb": round(peak_gb(), 2),
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2, default=str) + "\n")

    print(f"\nwrote to {out}:")
    for f in sorted(out.iterdir()):
        if f.is_file():
            print(f"  {f.name:34} {f.stat().st_size / 1e6:10.2f} MB")
    print(f"\nnext:\n  cd {out} && tessera check --payloads && tessera build --stage-timings")


if __name__ == "__main__":
    sys.exit(main())
