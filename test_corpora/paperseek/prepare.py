"""PaperSeek + OpenAlex — 102,117,343 works, the ladder's largest rung by every measure.

One pass from the staged chunks (`stage.py`) to a corpus `tessera build` consumes: **one view**,
`knn`, over the Stella embedding laid out by the fit-and-place route in `routes.py`; a flat
`clusters/kmeans` layer over that layout with a c-TF-IDF title on every cluster; and OpenAlex's
four-level topic tree as `topics/openalex`, which the OpenAlex track's `openalex.py` builds and
this script calls.

**It is a demonstrator and a speed benchmark** (owner ruling, 2026-09-01: speed wins over
accuracy). Recall against an exact neighbour search is not measured and layout fidelity is not
judged. What the rung is for is the volume — and, specifically, **a bundle larger than the box's
memory**: nothing about the declaration is trimmed to make it fit, the abstracts included.

Four decisions this stage makes:

- **`licence` is the access column**, and it is the ladder's first compartment that is a property
  of the work rather than a synthetic stand-in: a work's label is its OpenAlex licence key, and a
  work with no licence carries **no label** and is public. So `point_visibility`'s `default` is the
  ordinary case here rather than the unreachable one it is at rung 3, and the principal ladder runs
  the other way — no terms already sees the unlicensed majority, and each licence key adds to it.
- **Abstracts are on** (owner ruling, 2026-09-02), and they are the reason the rung exists. 102M
  works carry 118.9 GB of abstract text uncompressed, which is what puts the bundle past the box.
- **Nothing holds a text column whole.** Rung 3 read its abstracts into Arrow and wrote
  `points.parquet` in one call; 118.9 GB will not fit on a 47 GB box, so this script streams — one
  source row group at a time, straight through a `ParquetWriter`, with the OpenAlex resolve and the
  topic layer's member write riding the same pass.
- **The titles are vectorised over a bounded sample.** c-TF-IDF is a share of a cluster's documents
  carrying a term, and a uniform subsample estimates that share without fitting a 10⁸-row
  document-term matrix (`LABEL_MAX_DOCS`).

⊘ **The layout is not reproducible under a seed**, for the arXiv rung's reason: CAGRA's index build
is approximate and takes none, so UMAP is handed a different graph each run.
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

RUNG = "paperseek"
SEED = 0

VIEWS = ("knn",)
KMEANS_LAYER = "clusters/kmeans"
TOPICS_LAYER = "topics/openalex"

#: Rows the OpenAlex resolve and the topic layer's member write handle at once. A work sits under
#: four levels, so a slice of this size is 4x10^6 member rows and costs seconds.
OA_SLICE = 1_000_000

#: k-means cells at full scale, scaled with the sample by the square root of the row ratio — rung
#: 3's rule, for its reason: a linear scaling puts a 1M sample at two cells, which draws nothing.
KMEANS_K_FULL = 256
KMEANS_K_MIN = 16

LABEL_TERMS = 4  #: terms in one title
LABEL_SAMPLE = 200  #: documents a title's generating set names
MAX_CORPUS_SHARE = 0.02  #: above this a term is corpus vocabulary, not cluster vocabulary
MIN_CLUSTER_SHARE = 0.02  #: below this it is a coincidence rather than a description

#: Documents the c-TF-IDF vectoriser sees. Above this a uniform sample stands in for the corpus:
#: the score is a *share* of a cluster's documents carrying a term, and a uniform subsample of a
#: cluster estimates that share.
LABEL_MAX_DOCS = 4_000_000

#: Tokens that describe a piece of scholarship rather than a subject. Kept short on purpose — the
#: c-TF-IDF denominator is what is meant to do this work, and a stoplist that grows is a labeller
#: being hand-tuned. Each is under the 2% corpus cap and each is disproportionately common in some
#: cluster of a general scholarly corpus.
CORPUS_STOPLIST = frozenset("""
study studies analysis effects effect research paper article review case cases report results
using use new two one based approach method methods model models data system systems
""".split())

FALLBACK = "a cluster of works"

#: Rows per row group in `points.parquet`. 262,144 rows of abstract is ~330 MB of characters, well
#: inside a 32-bit offset, and a row group is the granularity the build's own reader batches at.
POINTS_ROW_GROUP = 262_144


def contents_of(text: str | None) -> list[list[str]]:
    """A cluster's ranked contents: the description where there is one, the fallback last.

    Never empty. The layer declares a supplied kind, and an artifact carrying none of a kind its
    layer declares is refused at publication — it could not be told apart from one whose content
    was withheld.
    """
    return [[text], [FALLBACK]] if text else [[FALLBACK]]


def kmeans_k(n: int, corpus: int) -> int:
    return max(KMEANS_K_MIN, min(KMEANS_K_FULL, round(KMEANS_K_FULL * (n / corpus) ** 0.5)))


# ---------------------------------------------------------------------------------- the labels


class Labeller:
    """Cluster titles by **c-TF-IDF against the layer's own sibling clusters** — how much more a
    term occurs in this cluster than across the clusters it is drawn beside.

    Transplanted from the arXiv and MedCPT rungs, where the reasoning is written out at length:
    plain TF-IDF over each cluster as one long document ranks by whatever is rarest and returns
    real tokens that describe nothing, and a denominator taken over the whole corpus rather than
    the siblings returns the register of scholarly writing rather than the cluster's subject.

        score(t, C) = f(t, C) · log( f(t, C) / mean_C' f(t, C') )

    **Label quality tracks the clustering, not this function.** These clusters are drawn in a 2D
    UMAP projection of a general text embedding, so they are spatially coherent and only roughly
    topical. What the rung demonstrates is the mechanism — a title is supplied content with its own
    gate, its own generating set and its own lifecycle.
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


def stream_staged(take: np.ndarray, columns):
    """`(global rows, table)` per staged row group, for the sample's rows, in entity order.

    **The whole rung's memory story is this generator.** `read_staged` at rung 3 held every column
    for every sampled row; the abstract column alone is 118.9 GB here, so nothing may be assembled.
    A staged chunk's parquet is in global row order and so is `take`, so the rows wanted from each
    source row group are a contiguous span of `take` and a slice of the group — and the group is
    released before the next is read.

    An identity gather is skipped rather than performed: at `--sample 0` every row is wanted, and
    `take` over a whole group would copy ~330 MB of characters to produce what was read.
    """
    staging = sources.staging()
    meta = json.loads((staging / "vectors.json").read_text())
    for n in sources.CHUNKS:
        held = meta["chunks"].get(str(n))
        if held is None:
            continue
        lo, hi = held["offset"], held["offset"] + held["rows"]
        if int(np.searchsorted(take, lo)) == int(np.searchsorted(take, hi)):
            continue
        f = pq.ParquetFile(staging / f"chunk_{n:02d}.parquet")
        at = lo
        for g in range(f.metadata.num_row_groups):
            rows = f.metadata.row_group(g).num_rows
            a = int(np.searchsorted(take, at))
            b = int(np.searchsorted(take, at + rows))
            if b > a:
                table = f.read_row_group(g, columns=list(columns))
                want = take[a:b]
                if len(want) != rows:
                    table = table.take(pa.array(want - at))
                yield want, table
                del table
            at += rows
        assert at == hi, f"chunk {n}: row groups sum to {at - lo} against {hi - lo}"


def read_staged(take: np.ndarray, columns) -> pa.Table:
    """Named columns for the sample's rows, assembled. **Small columns only** — this is the label
    pass's 4,000,000 titles and nothing else; `stream_staged` is what the corpus goes through."""
    return pa.concat_tables([t for _, t in stream_staged(take, columns)])


# --------------------------------------------------------------------------------- the outputs


#: `points.parquet`'s schema, fixed rather than inferred, because it is written a row group at a
#: time and a group whose `type` column happened to be all-null would otherwise change it.
POINTS_SCHEMA = pa.schema(
    [
        pa.field("entity_id", pa.uint64()),
        pa.field("x", pa.float64()),
        pa.field("y", pa.float64()),
        pa.field("licence", pa.string()),
        pa.field("publication_year", pa.int32()),
        pa.field("type", pa.string()),
        pa.field("is_oa", pa.bool_()),
        # **`string`, not `large_string`.** The build bakes a width from the declaration and
        # refuses a `LargeUtf8` column against a `keyword` or `text` attribute. A row group of
        # 262,144 abstracts is ~330 MB of characters, an order of magnitude inside what a 32-bit
        # offset addresses, so the cast at the boundary is safe at any corpus size this rung
        # reaches — it is the *staged* column, read whole, that needed the wide offsets.
        pa.field("openalex_id", pa.string()),
        pa.field("title", pa.string()),
        pa.field("abstract", pa.string()),
    ]
)


def write_vocabulary(out: Path, name: str, keys: list[str]) -> None:
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


def write_demo_terms(out: Path, counts: collections.Counter) -> None:
    """**The demo's candidate terms, ranked by coverage.** A term id names a different set in every
    dictionary, so a corpus with its own dictionary has to name its own terms or every principal
    measures empty against a synthetic `0..200` and the viewer opens on a blank map."""
    ranks = counts.most_common()
    (out / "licence-ranks.json").write_text(
        json.dumps([{"term": t, "pairs": n} for t, n in ranks], indent=None) + "\n"
    )
    (out / "licence-terms.txt").write_text(",".join(t for t, _ in ranks) + "\n")
    print(f"{len(ranks)} terms; top five " + ", ".join(f"{t} {n:,}" for t, n in ranks[:5]))


def write_deployment(out: Path) -> None:
    """`tessera.toml`, generated rather than committed — every value in it is a path or a port on
    this machine, which is the split `configuration.md` §3 draws.

    **Ports 8131–8133**, so this rung's server can run beside rung 3's on 8111–8113 and beside the
    memory-cap probe's on 8121–8123 without either being disturbed.
    """
    (out / "tessera.toml").write_text(
        """# Generated by `test_corpora/paperseek/prepare.py`. Machine-specific by construction: paths
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
viewer  = "127.0.0.1:8131"
session = "127.0.0.1:8132"
control = "127.0.0.1:8133"
max_k   = 5000
session_credential_env  = "TESSERA_PAPERSEEK_SESSION_CRED"
operator_credential_env = "TESSERA_PAPERSEEK_OPERATOR_CRED"

# Development only: the origin the demo viewer is served from (client-interaction §7). Without
# it the viewer loads and every request from it fails CORS, which reads like a broken server.
dev_cors_origins = ["http://localhost:PORT", "http://127.0.0.1:PORT"]
"""
    .replace("PORT", os.environ.get("VITE_PORT", "5177"))
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
        for var in ("TESSERA_IDENTITY_KEY", "TESSERA_PAPERSEEK_SESSION_CRED",
                    "TESSERA_PAPERSEEK_OPERATOR_CRED")
        if var not in held
    ]
    if minted:
        env.write_text("\n".join(lines + minted) + "\n")
        env.chmod(0o600)
        print(f"minted {', '.join(m.split('=')[0] for m in minted)} in {env}")


def write_declaration(out: Path, *, licences: list[str], types: int, topics_toml: str | None) -> None:
    """`corpus.toml`, copied beside the data and edited for what this run actually wrote.

    `[sources]` is relative to the declaring document (`configuration.md` §3), so the copy beside
    the parquets is what `tessera build` reads and the one in git is what gets reviewed. One marker
    is filled here rather than committed conditional: the topic layer, whose sources and block the
    OpenAlex track owns.
    """
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
    have = bool(licences)
    text = fill(text, "# <topics-layer>", topics_toml or
                "# ⊘ `topics/openalex` is not declared: `openalex.py` was not present when this run\n"
                "# wrote the corpus, so no work carries a topic.")
    text = fill(text, "# <point-visibility>",
                'point_visibility = { field = "licence", default = "public" }' if have else
                "# ⊘ `point_visibility` names no field: `openalex.py` was not present when this run\n"
                "# wrote the corpus, so no work carries a licence and there is no compartment. Every\n"
                "# point takes the declared default and the whole corpus is public.\n"
                'point_visibility = { default = "public" }')
    text = fill(text, "# <licence-vocabulary>",
                f"# This run wrote {len(licences)} licence key(s) and {types} work type(s).\n"
                '[[vocabulary]]\n'
                'name       = "licence"\n'
                'title      = "Licence"\n'
                'width      = "u8"\n'
                'value_set  = "closed"\n'
                'visibility = "public"\n'
                'source     = "licence"' if have else
                "# ⊘ `licence` is not declared: `openalex.py` was not present when this run wrote the\n"
                f"# corpus, so the column is null throughout. {types} work type(s) were written.")
    (out / "corpus.toml").write_text(text)


# ------------------------------------------------------------------------------------- the run


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--sample", type=int, default=1_000_000,
                    help="works to take, uniformly; 0 takes all 102,117,343")
    ap.add_argument("--out", type=Path, default=None, help=f"default $TESSERA_LADDER/{RUNG}")
    ap.add_argument("--fit", type=int, default=routes.FIT_ROWS,
                    help="rows UMAP is fitted over; the rest are placed against the fit set")
    ap.add_argument("--shard", type=int, default=routes.SHARD_ROWS,
                    help="rows per CAGRA index in the sharded kNN graph")
    ap.add_argument("--managed", action="store_true",
                    help="RMM managed memory for the layout, so the card oversubscribes")
    ap.add_argument("--partial", action="store_true",
                    help="run against the staged prefix while the staging pass is still going — "
                         "for a smoke, and the manifest records that the corpus is a prefix")
    ap.add_argument("--drop-vectors", action="store_true",
                    help="delete staging/vectors.f16 once the layout is written — 209 GB that "
                         "nothing after the layout reads")
    args = ap.parse_args()

    out = args.out or ladder(RUNG)
    out.mkdir(parents=True, exist_ok=True)
    steps = Steps()
    print(f"staging {sources.staging()}\noutput  {out}")

    # ------------------------------------------------------------------ the OpenAlex track's module
    try:
        from .openalex import LAYER_TOML as TOPICS_TOML
        from .openalex import OpenAlex
    except ImportError as exc:
        OpenAlex, TOPICS_TOML = None, None
        print(f"⊘ test_corpora/paperseek/openalex.py is not usable ({exc}) — no topic layer, no "
              "licence, and every work public")

    # ---------------------------------------------------------------------- the corpus and sample
    matrix, meta = sources.vectors(complete=not args.partial)
    n_full = sources.staged_rows(meta) if args.partial else meta["rows"]
    if args.partial:
        print(f"⊘ the corpus is the staged prefix: {n_full:,} of {meta['rows']:,} rows", flush=True)
    with steps.step("sample"):
        take = (
            np.arange(n_full)
            if not args.sample or args.sample >= n_full
            else np.sort(np.random.default_rng(SEED).choice(n_full, args.sample, replace=False))
        )
        n = len(take)
    print(f"{n:,} works sampled from {n_full:,}", flush=True)

    # ------------------------------------------------------------------------------- the route
    # **Before anything else.** The route holds the fit set and the graph; everything after it is
    # Arrow buffers, and the two peaks should not meet.
    timings: dict = {}
    with steps.step("route knn"):
        X = (
            routes.Vectors.memmap(matrix)
            if n == n_full
            else routes.Vectors.gathered(matrix, take)
        )
        t: dict = {}
        xy = routes.knn(X, t, fit=args.fit, shard_rows=args.shard, managed=args.managed)
        timings["knn"] = t
        del X
    np.save(out / "layout-knn.npy", xy)
    del matrix
    print(f"  route knn: x [{xy[:, 0].min():.2f}, {xy[:, 0].max():.2f}] "
          f"y [{xy[:, 1].min():.2f}, {xy[:, 1].max():.2f}]  (peak so far "
          f"{resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20:.1f} GB)", flush=True)

    # **The layout is written, so the vectors are spent.** 209 GB, and nothing below this line
    # reads one — the clustering is over the 2D layout and the columns come from the staged
    # parquet. A later run that wants a different layout re-stages.
    dropped_vectors = False
    if args.drop_vectors:
        assert not args.partial, "--drop-vectors on a prefix run would strand the staging pass"
        path = sources.staging() / "vectors.f16"
        size = path.stat().st_size
        path.unlink()
        dropped_vectors = True
        print(f"  dropped {path} — {size / 2**30:.1f} GiB", flush=True)

    # ----------------------------------------------------------------------------- the clustering
    k = kmeans_k(n, n_full)
    with steps.step("k-means"):
        label, kmeans_route = cluster(xy, k)
    sizes = np.bincount(label, minlength=k)
    # One stable argsort rather than `k` passes over the label array: at 10^8 rows and 256 cells
    # the naive form is 2.6x10^10 comparisons for a grouping the sort already gives.
    order = np.argsort(label, kind="stable")
    bounds = np.concatenate([[0], np.cumsum(sizes)])
    members = {c: order[bounds[c] : bounds[c + 1]] for c in range(k)}
    print(f"{k} clusters by {kmeans_route}, sizes {sizes.min():,}..{sizes.max():,} "
          f"(median {int(np.median(sizes)):,})", flush=True)

    # -------------------------------------------------------------------------------- the titles
    with steps.step("vectorise titles"):
        label_rows = (
            np.arange(n)
            if n <= LABEL_MAX_DOCS
            else np.sort(np.random.default_rng(SEED + 1).choice(n, LABEL_MAX_DOCS, replace=False))
        )
        titles = read_staged(take[label_rows], ["title"])
        assert titles.num_rows == len(label_rows), "the label sample lost rows in the staged read"
        labeller = Labeller(titles.column("title").to_pylist(), label_rows, n)
        del titles
    print(f"{len(labeller.vocab):,} candidate terms over {len(label_rows):,} titles", flush=True)

    with steps.step("titles"):
        text = labeller.label({c: labeller.sampled(rows) for c, rows in members.items()})
    print(f"{len(text)} of {k} clusters got a distinctive title", flush=True)
    del labeller

    # ------------------------------------------------------- the one pass: resolve, layer, points
    #
    # **Everything the corpus's own bytes are needed for happens here, once.** For each staged row
    # group: the OpenAlex join for this slice's ids, the topic layer's member rows streamed
    # straight to parquet, and one row group of `points.parquet`. The abstract column is never
    # assembled — it is read, written and released a group at a time.
    artifacts = ArtifactSet()
    oa = OpenAlex() if OpenAlex is not None else None
    licence_counts: collections.Counter = collections.Counter()
    type_counts: collections.Counter = collections.Counter()
    oa_stats = {"module": "absent"} if oa is None else {}
    years = oa_true = 0
    topic_all = np.full(n, -1, dtype=np.int32)

    writer = pq.ParquetWriter(out / "points.parquet", POINTS_SCHEMA, compression="zstd",
                              use_dictionary=["licence", "type"])
    at = 0
    with steps.step("resolve, layer and points"):
        for want, table in stream_staged(take, ["row", "id", "title", "abstract"]):
            m = table.num_rows
            entity = np.arange(at, at + m, dtype=np.uint64)
            assert np.array_equal(table.column("row").to_numpy(), want.astype(np.uint32)), (
                "the staged chunks and the sample disagree about row order"
            )
            ids = table.column("id").combine_chunks()

            if oa is None:
                licence = pa.nulls(m, pa.string())
                year = pa.nulls(m, pa.int32())
                kind = pa.nulls(m, pa.string())
                is_oa = pa.nulls(m, pa.bool_())
            else:
                # `resolve` wants the id spelling the extract carries, which is the full URL; the
                # staged column holds the `W…` part alone. Rebuilt in Arrow a slice at a time.
                urls = pc.binary_join_element_wise(sources.ID_PREFIX, ids, "")
                got = oa.resolve(urls)
                del urls
                assert got.num_rows == m, f"resolve returned {got.num_rows} rows against {m}"
                licence = got.column("licence").combine_chunks()
                year = got.column("publication_year").combine_chunks()
                kind = got.column("type").combine_chunks()
                is_oa = got.column("is_oa").combine_chunks()
                topic = np.asarray(
                    got.column("topic").combine_chunks().fill_null(-1).to_numpy(zero_copy_only=False),
                    dtype=np.int32,
                )
                topic_all[at : at + m] = topic
                held = topic >= 0
                if held.any():
                    oa.write_layer(out, artifacts, topic[held], entity[held])
                del got

            counts = pc.value_counts(licence)
            licence_counts.update(dict(zip(counts.field("values").to_pylist(),
                                           counts.field("counts").to_pylist())))
            counts = pc.value_counts(kind)
            type_counts.update(dict(zip(counts.field("values").to_pylist(),
                                        counts.field("counts").to_pylist())))
            years += m - year.null_count
            oa_true += int(pc.sum(is_oa).as_py() or 0)

            writer.write_table(
                pa.table(
                    {
                        "entity_id": pa.array(entity, pa.uint64()),
                        "x": pa.array(xy[at : at + m, 0].astype(np.float64), pa.float64()),
                        "y": pa.array(xy[at : at + m, 1].astype(np.float64), pa.float64()),
                        "licence": licence.cast(pa.string()),
                        "publication_year": year.cast(pa.int32()),
                        "type": kind.cast(pa.string()),
                        "is_oa": is_oa.cast(pa.bool_()),
                        "openalex_id": ids.cast(pa.string()),
                        "title": table.column("title").combine_chunks().cast(pa.string()),
                        "abstract": table.column("abstract").combine_chunks().cast(pa.string()),
                    },
                    schema=POINTS_SCHEMA,
                ),
                row_group_size=POINTS_ROW_GROUP,
            )
            at += m
            del table, ids, licence, year, kind, is_oa
            if (at // OA_SLICE) != ((at - m) // OA_SLICE):
                print(f"    {at:,}/{n:,}  ({resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20:.1f} GB)",
                      flush=True)
    writer.close()
    assert at == n, f"wrote {at} points against {n}"
    if oa is not None:
        artifacts.close_streams()
        oa_stats = dict(oa.stats) | {"streamed_member_rows": int(artifacts.streamed[TOPICS_LAYER])}

    licence_counts.pop(None, None)
    type_counts.pop(None, None)
    with_licence = sum(licence_counts.values())
    print(f"licence: {with_licence:,} of {n:,} ({with_licence / n:.1%}) carry one, over "
          f"{len(licence_counts)} keys; {years:,} ({years / n:.1%}) carry a year; "
          f"{oa_true:,} ({oa_true / n:.1%}) are open access; "
          f"{int((topic_all >= 0).sum()):,} carry a topic", flush=True)

    # -------------------------------------------------------------------- the clustering's rows
    #
    # **Streamed, not accumulated.** `ArtifactSet.members` holds one Python list entry per member
    # row, which is 10^8 of them here; the streaming path writes a cluster's members as one Arrow
    # batch and keeps nothing.
    with steps.step("write artifacts"):
        for c in range(k):
            artifacts.artifact(KMEANS_LAYER, f"km-{c:06d}", contents=contents_of(text.get(c)))
    with steps.step("write members"):
        rng = np.random.default_rng(SEED)
        for c in range(k):
            key = f"km-{c:06d}"
            rows = members[c]
            artifacts.stream_members(KMEANS_LAYER, out, repeated(key, len(rows)), rows)
            pick = rows if len(rows) <= LABEL_SAMPLE else rng.choice(rows, LABEL_SAMPLE,
                                                                    replace=False)
            for rank in range(len(contents_of(text.get(c)))):
                sub = pick[: max(1, len(pick) // 3**rank)]
                artifacts.stream_members(KMEANS_LAYER, out, repeated(key, len(sub)), sub,
                                         rank=rank)
        artifacts.close_streams()

    artifacts.check(n)
    artifact_rows, member_rows = artifacts.write(out)
    licences = oa.licences() if oa is not None else []
    # **A closed vocabulary and the column that fills it must agree.** A licence key `resolve`
    # produced and `licences()` does not name would refuse the build far downstream, with the
    # message naming a value rather than the disagreement between the two halves of one module.
    stray = sorted(set(licence_counts) - set(licences))
    assert not stray, (
        f"openalex.py's resolve() produced licence key(s) {stray} that its licences() does not "
        f"name. The vocabulary is closed and the access column is built from it, so this is a "
        f"disagreement inside the module rather than a property of the data."
    )
    if licences:
        write_vocabulary(out, "licence", licences)
    kinds = sorted(type_counts)
    write_vocabulary(out, "type", kinds)
    print(f"{artifact_rows:,} artifacts, {member_rows:,} member rows across "
          f"{len(artifacts.per_layer)} layers")

    with steps.step("write the demo's terms"):
        write_demo_terms(out, licence_counts)

    write_declaration(out, licences=licences, types=len(kinds),
                      topics_toml=TOPICS_TOML if oa is not None else None)
    write_deployment(out)

    manifest = {
        "rung": RUNG,
        "sample": n,
        "corpus": n_full,
        "corpus_full": meta["rows"],
        "prefix_only": bool(args.partial),
        "seed": SEED,
        "views": list(VIEWS),
        "abstracts": True,
        "dropped_vectors": dropped_vectors,
        "umap": dict(routes.UMAP_PARAMS, seed=routes.SEED, implementation="cuml",
                     route="fit on a uniform sample, place the rest against its CAGRA index",
                     graph="cagra fp16", fit_rows=min(args.fit, n), shard_rows=args.shard,
                     managed_memory=args.managed),
        "routes": timings,
        "bounds": {"knn": {"x": [float(xy[:, 0].min()), float(xy[:, 0].max())],
                           "y": [float(xy[:, 1].min()), float(xy[:, 1].max())]}},
        "kmeans": {"k": k, "route": kmeans_route, "titles": len(text),
                   "sizes": [int(sizes.min()), int(np.median(sizes)), int(sizes.max())]},
        "openalex": oa_stats,
        "coverage": {
            "with_licence": with_licence,
            "with_year": years,
            "is_oa": oa_true,
            "with_topic": int((topic_all >= 0).sum()),
        },
        "licence_terms": len(licence_counts),
        "type_terms": len(kinds),
        "artifact_rows": artifact_rows,
        "member_rows": member_rows,
        "seconds": dict(steps),
        "total_seconds": steps.total(),
        "peak_rss_gb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20, 2),
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2, default=str) + "\n")

    print(f"\nwrote to {out}:")
    for f in sorted(out.iterdir()):
        if f.is_file():
            print(f"  {f.name:34} {f.stat().st_size / 1e6:10.2f} MB")
    print(f"\nnext:\n  cd {out} && tessera check --payloads && tessera build --stage-timings")


def repeated(key: str, n: int) -> pa.Array:
    """`n` copies of one artifact key, without `n` Python strings.

    A dictionary array taken against a one-value dictionary is a vectorised gather; the alternative
    spelling — `pa.array([key] * n)` — materialises a list of 10⁶ interned strings per cluster for
    a column parquet then dictionary-encodes anyway.
    """
    return pa.DictionaryArray.from_arrays(
        pa.array(np.zeros(n, dtype=np.int32), pa.int32()), pa.array([key], pa.string())
    ).cast(pa.string())


def cluster(xy: np.ndarray, k: int) -> tuple[np.ndarray, str]:
    """k-means over the layout, whole where the card takes it and fitted on a sample where it does
    not.

    Rung 3 fitted cuML's k-means over all 3.6×10⁷ points in 22 s. 1.02×10⁸ × 2 float32 is 817 MB of
    coordinates before the algorithm's own working set, so the whole-corpus fit is **tried** and
    the route that ran is recorded rather than assumed — an out-of-memory here would otherwise cost
    the run the route it already paid twenty minutes for.
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


if __name__ == "__main__":
    sys.exit(main())
