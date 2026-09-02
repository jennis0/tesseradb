"""MedCPT / PubMed — 35,920,666 articles, the ladder's largest embedding rung.

One pass from the staged chunks (`stage.py`) to a corpus `tessera build` consumes: **one view**,
`knn`, over the MedCPT embedding laid out by the fit-and-place route in `routes.py`; a flat
`clusters/kmeans` layer over that layout with a c-TF-IDF title on every cluster; and the MeSH descriptor DAG
as `mesh/descriptors`, which the MeSH track's `mesh.py` builds and this script calls.

**It is a demonstrator and a speed benchmark** (owner ruling, 2026-09-01: speed wins over
accuracy). Recall against an exact neighbour search is not measured and layout fidelity is not
judged. What the rung is for is the volume: 15x the arXiv rung's rows, and an ancestor-closed
membership over a real polyhierarchy.

**The view is titled *Literature map*, not a topic map.** MedCPT was trained on query-article click
pairs for retrieval, and the publisher says so: its geometry is organised for search relevance,
which is not topical similarity (`docs/ingest-campaign.md` §4).

Four decisions this stage makes:

- **`branches` is the access column** — the MeSH top-level branch letters an article's resolved
  descriptors sit under, `A` through `N` plus `V` and `Z`. An article with no resolved descriptor
  carries the single term `unindexed` rather than an empty list, which is why `point_visibility`'s
  `default` never fires; the declaration says so at the field. MeSH coverage runs strongly with
  time and the chunks are in PMID order, so `unindexed` is not a scatter — it is the recent end of
  the corpus (`../../docs/ingest-campaign.md` §4.4).
- **Abstracts are off by default.** ⊘ Whether the rung takes them is an owner ruling that is open:
  36M x ~1 kB is ~30 GB of strings in an attribute pass that holds a text column whole, and the
  streaming text column does not exist. `--abstracts` takes them, and `README.md` carries the 1M
  sample's build measured both ways so the ruling can be made from numbers.
- **The titles are vectorised over a bounded sample.** c-TF-IDF is a share of a cluster's documents
  carrying a term, and a uniform subsample of a cluster's members estimates that share without
  fitting a 36M-row document-term matrix. Above `LABEL_MAX_DOCS` the labeller sees a uniform
  sample and the shares are computed over it; below it, every title.
- **The sample is uniform and then sorted.** The chunks are in PMID order, which is close to
  chronological, so a prefix takes the oldest articles — and MeSH coverage, abstract coverage and
  subject mix all run with time.

⊘ **The layout is not reproducible under a seed**, for the arXiv rung's reason: CAGRA's index build
is approximate and takes none, so UMAP is handed a different graph each run.
"""

from __future__ import annotations

import argparse
import collections
import json
import resource
import sys
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from ..common.paths import ladder
from ..common.timing import Steps
from ..arxiv.writer import ArtifactSet
from . import routes, sources

RUNG = "medcpt"
SEED = 0

VIEWS = ("knn",)
KMEANS_LAYER = "clusters/kmeans"

#: Rows the MeSH resolve, closure and member write handle at once. The closure is ~1.7x10^9 pairs
#: over the corpus, so it is never held whole; a slice of this size is ~5x10^7 and costs seconds.
MESH_SLICE = 1_000_000

#: The access term an article with no resolved MeSH descriptor carries. It exists so that the
#: access column is never empty, which is what keeps `point_visibility`'s `default` from firing.
UNINDEXED = "unindexed"

#: k-means cells at full scale, scaled with the sample by the square root of the row ratio — a
#: linear scaling puts a 1M sample at seven cells, which draws nothing.
KMEANS_K_FULL = 256
KMEANS_K_MIN = 16

LABEL_TERMS = 4  #: terms in one title
LABEL_SAMPLE = 200  #: documents a title's generating set names
MAX_CORPUS_SHARE = 0.02  #: above this a term is corpus vocabulary, not cluster vocabulary
MIN_CLUSTER_SHARE = 0.02  #: below this it is a coincidence rather than a description

#: Documents the c-TF-IDF vectoriser sees. Above this a uniform sample stands in for the corpus:
#: the score is a *share* of a cluster's documents carrying a term, and a uniform subsample of a
#: cluster estimates that share. Fitting the full 36M x 45k document-term matrix buys precision in
#: a number that is then used to rank four words.
LABEL_MAX_DOCS = 4_000_000

#: Tokens that describe a biomedical paper rather than a topic. Kept short on purpose — the
#: c-TF-IDF denominator is what is meant to do this work, and a stoplist that grows is a labeller
#: being hand-tuned. Each of these is under the 2% corpus cap and each is disproportionately
#: common in some cluster.
CORPUS_STOPLIST = frozenset("""
patients patient study studies case cases report effects effect treatment clinical analysis
results using use new two one role human rats mice cells cell factor levels
""".split())

FALLBACK = "a cluster of articles"


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

    Transplanted from the arXiv rung, where the reasoning is written out at length: plain TF-IDF
    over each cluster as one long document ranks by whatever is rarest and returns real tokens that
    describe nothing, and a denominator taken over the whole corpus rather than the siblings
    returns the discipline's register rather than the cluster's. What is different here is the
    stoplist, which is biomedical, and `documents` — the vectoriser is fitted over a bounded
    uniform sample of the corpus rather than all of it (`LABEL_MAX_DOCS`).

        score(t, C) = f(t, C) · log( f(t, C) / mean_C' f(t, C') )

    **Label quality tracks the clustering, not this function.** These clusters are drawn in a 2D
    UMAP projection of a retrieval embedding, so they are spatially coherent and only roughly
    topical. What the rung demonstrates is the mechanism — a title is supplied content with its own
    gate, its own generating set and its own lifecycle.
    """

    def __init__(self, titles: pa.Array, rows: np.ndarray, n: int):
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
        self.occurs = vec.fit_transform("" if t is None else t for t in titles.to_pylist())
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


#: The columns every run needs. `mesh` is here and is dropped the moment it is resolved: the raw
#: `m` field averages ~300 bytes an article, which is 11 GB at 36M rows and larger than the titles.
BASE_COLUMNS = ("row", "pmid", "published", "title", "mesh")


def read_staged(out: Path, take: np.ndarray, columns) -> pa.Table:
    """Named columns for the sample's rows, read one staged chunk at a time.

    `take` is sorted global row indices, and a chunk's parquet is in global row order, so the rows
    wanted from each chunk are a contiguous span of `take` and a slice of the chunk. Nothing is
    read twice and no chunk is held after its rows are taken.

    **This is a memory dial, which is why the columns are the caller's.** At 36M rows the abstracts
    are ~30 GB of Arrow buffers and the raw MeSH field ~11 GB, against the titles' ~4 GB — so the
    run reads the base columns before the route, drops `mesh` as soon as it is resolved, and reads
    the abstracts, if it takes them at all, in the step that writes them.
    """
    staging = sources.staging(out)
    meta = json.loads((staging / "vectors.json").read_text())
    pieces = []
    for n in sources.CHUNKS:
        held = meta["chunks"].get(str(n))
        if held is None:  # not staged, so `take` names none of its rows; the row count is asserted
            continue
        lo, hi = held["offset"], held["offset"] + held["rows"]
        first = int(np.searchsorted(take, lo))
        last = int(np.searchsorted(take, hi))
        if first == last:
            continue
        table = pq.read_table(staging / f"chunk_{n:02d}.parquet", columns=list(columns))
        pieces.append(table.take(pa.array(take[first:last] - lo)))
        del table
    return pa.concat_tables(pieces)


# --------------------------------------------------------------------------------- the MeSH join


def joined_names(mesh, major) -> pa.Array:
    """`mesh_major`: the major-topic descriptor names joined with `; `, in Arrow throughout.

    The names are taken by index and re-listed against the same offsets, so nothing materialises a
    list of strings per row — the same trick the arXiv rung joins its author surnames with.
    """
    values = major.combine_chunks() if isinstance(major, pa.ChunkedArray) else major
    names = pa.array(mesh.descriptors, pa.string())
    taken = names.take(pa.array(np.asarray(values.values), pa.int32()))
    listed = pa.ListArray.from_arrays(pa.array(np.asarray(values.offsets), pa.int32()), taken)
    joined = pc.binary_join(listed, "; ")
    return pc.if_else(pc.greater(pc.list_value_length(listed), 0), joined,
                      pa.nulls(len(listed), pa.string()))


# --------------------------------------------------------------------------------- the outputs


def write_points(out: Path, *, entity, xy, access: pa.Array, extra) -> None:
    """The view's positions and every entity-space column, one row per article.

    `access` is the `branches` column as Arrow already — the MeSH module builds it a slice at a
    time and nothing turns it back into Python.

    **Raw coordinates, written as they are.** `extent = "auto"` fits a square box around exactly
    these numbers; scaling by hand is the failure the extent moved into the declaration to prevent
    (`../arxiv/README.md`).
    """
    cols = {
        "entity_id": pa.array(entity, pa.uint64()),
        "x": pa.array(xy[:, 0].astype(np.float64), pa.float64()),
        "y": pa.array(xy[:, 1].astype(np.float64), pa.float64()),
        "branches": access,
    }
    cols.update(extra)
    pq.write_table(pa.table(cols), out / "points.parquet")


def write_vocabulary(out: Path, branches: list[str]) -> None:
    """The `branches` vocabulary: MeSH's own top-level letters plus `unindexed`. Code 0 is the
    *absent* sentinel and is refused in a declared set, so the codes start at one."""
    pq.write_table(
        pa.table(
            {
                "key": pa.array(branches, pa.string()),
                "code": pa.array(np.arange(1, len(branches) + 1, dtype=np.uint32), pa.uint32()),
                "title": pa.array(branches, pa.string()),
            }
        ),
        out / "branch.parquet",
    )


def write_demo_terms(out: Path, counts: collections.Counter) -> None:
    """**The demo's candidate terms, ranked by coverage.** `run_demo.sh --ranks` composes its
    sparse/medium/heavy principals out of this list and `--terms` is the candidate set.

    A term id names a different set in every dictionary, so a corpus with its own dictionary has to
    name its own terms or every principal measures empty against the demo's synthetic `0..200` and
    the viewer opens on a blank map with nothing to say why.
    """
    ranks = counts.most_common()
    (out / "branch-ranks.json").write_text(
        json.dumps([{"term": t, "pairs": n} for t, n in ranks], indent=None) + "\n"
    )
    (out / "branch-terms.txt").write_text(",".join(t for t, _ in ranks) + "\n")
    print(f"{len(ranks)} terms; top five " + ", ".join(f"{t} {n:,}" for t, n in ranks[:5]))


def write_deployment(out: Path) -> None:
    """`tessera.toml`, generated rather than committed — every value in it is a path or a port on
    this machine, which is the split `configuration.md` §3 draws."""
    (out / "tessera.toml").write_text(
        """# Generated by `test_corpora/medcpt/prepare.py`. Machine-specific by construction: paths
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
viewer  = "127.0.0.1:8111"
session = "127.0.0.1:8112"
control = "127.0.0.1:8113"
max_k   = 5000
session_credential_env  = "TESSERA_MEDCPT_SESSION_CRED"
operator_credential_env = "TESSERA_MEDCPT_OPERATOR_CRED"

# Development only: the origin the demo viewer is served from (client-interaction §7). Without
# it the viewer loads and every request from it fails CORS, which reads like a broken server.
dev_cors_origins = ["http://localhost:5173", "http://127.0.0.1:5173"]
"""
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
        for var in ("TESSERA_IDENTITY_KEY", "TESSERA_MEDCPT_SESSION_CRED",
                    "TESSERA_MEDCPT_OPERATOR_CRED")
        if var not in held
    ]
    if minted:
        env.write_text("\n".join(lines + minted) + "\n")
        env.chmod(0o600)
        print(f"minted {', '.join(m.split('=')[0] for m in minted)} in {env}")


def write_declaration(out: Path, *, abstracts: bool, mesh_toml: str | None) -> None:
    """`corpus.toml`, copied beside the data and edited for what this run actually wrote.

    `[sources]` is relative to the declaring document (`configuration.md` §3), so the copy beside
    the parquets is what `tessera build` reads and the one in git is what gets reviewed. Two
    markers are filled here rather than committed conditional: the abstract attribute, which is an
    open owner ruling, and the MeSH layer, whose sources and block the MeSH track owns.
    """
    def fill(text: str, marker: str, block: str) -> str:
        """Replace the **whole line** that is the marker. A plain `str.replace` would also hit the
        header comment that names the marker, which is how a TOML block landed inside a comment
        and refused a build."""
        lines = text.splitlines()
        hit = [i for i, line in enumerate(lines) if line.strip() == marker]
        assert len(hit) == 1, f"{marker} appears {len(hit)} times as a line of its own"
        lines[hit[0]] = block
        return "\n".join(lines) + "\n"

    text = (Path(__file__).parent / "corpus.toml").read_text()
    abstract_block = """
# 36M x ~1 kB of prose. ⊘ Whether the rung takes this is an **open owner ruling** — the attribute
# pass holds a text column whole and the streaming text column does not exist. Written only when
# `prepare.py` ran with `--abstracts`; `README.md` carries the build measured both ways.
[[attribute]]
name  = "abstract"
type  = "text"
index = true
"""
    text = fill(text, "# <abstract-attribute>", abstract_block if abstracts else
                "# ⊘ `abstract` is not declared: this run took `--abstracts` off (the default),\n"
                "# so the column is not in `points.parquet`.")
    text = fill(text, "# <mesh-layer>", mesh_toml or
                "# ⊘ `mesh/descriptors` is not declared: `mesh.py` was not present when this run\n"
                "# wrote the corpus, so every article carries `unindexed`.")
    (out / "corpus.toml").write_text(text)


# ------------------------------------------------------------------------------------- the run


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--sample", type=int, default=1_000_000,
                    help="articles to take, uniformly; 0 takes all 35,920,666")
    ap.add_argument("--out", type=Path, default=None, help=f"default $TESSERA_LADDER/{RUNG}")
    ap.add_argument("--no-half", action="store_true",
                    help="accepted for the arXiv rung's CLI shape; the staged matrix is float16")
    ap.add_argument("--abstracts", action="store_true",
                    help="take the abstracts — ⊘ an open owner ruling, off by default")
    ap.add_argument("--fit", type=int, default=routes.FIT_ROWS,
                    help="rows UMAP is fitted over; the rest are placed against the fit set")
    ap.add_argument("--shard", type=int, default=routes.SHARD_ROWS,
                    help="rows per CAGRA index in the sharded kNN graph")
    ap.add_argument("--managed", action="store_true",
                    help="RMM managed memory for the layout, so the card oversubscribes")
    args = ap.parse_args()

    out = args.out or ladder(RUNG)
    out.mkdir(parents=True, exist_ok=True)
    steps = Steps()
    print(f"staging {sources.staging(out)}\noutput  {out}")

    # ------------------------------------------------------------------ the MeSH track's module
    try:
        from .mesh import LAYER as MESH_LAYER
        from .mesh import LAYER_TOML as MESH_TOML
        from .mesh import Mesh
    except ImportError:
        Mesh, MESH_TOML, MESH_LAYER = None, None, None
        print("⊘ test_corpora/medcpt/mesh.py is not present — no MeSH layer, no branches, "
              f"every article carries {UNINDEXED!r}")

    # ---------------------------------------------------------------------- the corpus and sample
    matrix, meta = sources.vectors(out)
    n_full = meta["rows"]
    with steps.step("sample"):
        take = (
            np.arange(n_full)
            if not args.sample or args.sample >= n_full
            else np.sort(np.random.default_rng(SEED).choice(n_full, args.sample, replace=False))
        )
        n = len(take)
    print(f"{n:,} articles sampled from {n_full:,}")

    # ------------------------------------------------------------------------------- the route
    # **Before the columns.** The route holds the fit set and the graph; the columns are tens of
    # gigabytes of Arrow buffers and are not wanted until it is done.
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
    print(f"  route knn: x [{xy[:, 0].min():.2f}, {xy[:, 0].max():.2f}] "
          f"y [{xy[:, 1].min():.2f}, {xy[:, 1].max():.2f}]  (peak so far "
          f"{resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20:.1f} GB)", flush=True)

    with steps.step("read staged columns"):
        table = read_staged(out, take, BASE_COLUMNS)
    assert table.num_rows == n, f"{table.num_rows} staged rows against {n} sampled"
    assert np.array_equal(table.column("row").to_numpy(), take.astype(np.uint32)), (
        "the staged chunks and the sample disagree about row order"
    )

    # ---------------------------------------------------------------------------------- MeSH
    #
    # **A slice at a time, because the closure does not fit.** `closure` expands each article's
    # descriptors to its ancestors — ~1.7x10^9 pairs over the corpus (`dag-hierarchies.md` §8),
    # which is 14 GB of int64 held at once. So the resolve, the closure and the member write run
    # over `MESH_SLICE` rows at a time and only the access column and the major-topic names, both
    # small, survive the loop. `write_layer` streams its member rows straight to the layer's
    # parquet and re-declares its artifacts each call, so it needs no last-call signal;
    # `close_streams` finishes the file.
    artifacts = ArtifactSet()
    with steps.step("mesh"):
        if Mesh is None:
            mesh = None
            mesh_stats = {"module": "absent"}
            access = pa.array([[UNINDEXED]] * n, pa.list_(pa.string()))
            major_names = pa.nulls(n, pa.string())
        else:
            mesh = Mesh()
            raw = table.column("mesh")
            access_parts, major_parts = [], []
            mesh_stats = collections.Counter()
            unresolved = collections.Counter()
            for lo in range(0, n, MESH_SLICE):
                hi = min(lo + MESH_SLICE, n)
                explicit, major, stats = mesh.resolve(raw.slice(lo, hi - lo))
                closed = mesh.closure(explicit)
                mesh.write_layer(out, artifacts, closed,
                                 np.arange(lo, hi, dtype=np.uint64))
                access_parts.append(mesh.branches(explicit, empty=UNINDEXED))
                major_parts.append(joined_names(mesh, major))
                mesh_stats.update({k: v for k, v in stats.items() if k != "unresolved_top"})
                unresolved.update(dict(stats["unresolved_top"]))
                print(f"    mesh {hi:,}/{n:,}: {len(closed.values):,} closed pairs", flush=True)
                del explicit, major, closed
            artifacts.close_streams()
            access = pa.chunked_array(access_parts).combine_chunks()
            major_names = pa.chunked_array(major_parts).combine_chunks()
            mesh_stats = dict(mesh_stats) | {
                "unresolved_top": unresolved.most_common(20),
                "tree": mesh.shape(),
                "layer": mesh.artifact_shape(),
                "streamed_member_rows": int(artifacts.streamed[MESH_LAYER]),
            }
        table = table.drop_columns(["mesh"])
    flat_access = pc.list_flatten(access)
    counts = collections.Counter(
        dict(zip(pc.value_counts(flat_access).field("values").to_pylist(),
                 pc.value_counts(flat_access).field("counts").to_pylist()))
    )
    print(f"MeSH: { {k: v for k, v in mesh_stats.items() if k != 'unresolved_top'} }; "
          f"{counts[UNINDEXED]:,} articles ({counts[UNINDEXED] / n:.1%}) carry {UNINDEXED!r}")

    # ----------------------------------------------------------------------------- the clustering
    from cuml.cluster import KMeans

    k = kmeans_k(n, n_full)
    with steps.step("k-means"):
        label = np.asarray(
            KMeans(n_clusters=k, random_state=SEED, n_init=1).fit(xy).labels_
        ).astype(np.int32)
    sizes = np.bincount(label, minlength=k)
    # One stable argsort rather than `k` passes over the label array: at 36M rows and 256 cells
    # the naive form is 9x10^9 comparisons for a grouping the sort already gives.
    order = np.argsort(label, kind="stable")
    bounds = np.concatenate([[0], np.cumsum(sizes)])
    members = {c: order[bounds[c] : bounds[c + 1]] for c in range(k)}
    print(f"{k} clusters, sizes {sizes.min():,}..{sizes.max():,} "
          f"(median {int(np.median(sizes)):,})")

    # -------------------------------------------------------------------------------- the titles
    with steps.step("vectorise titles"):
        label_rows = (
            np.arange(n)
            if n <= LABEL_MAX_DOCS
            else np.sort(np.random.default_rng(SEED + 1).choice(n, LABEL_MAX_DOCS, replace=False))
        )
        labeller = Labeller(table.column("title").take(pa.array(label_rows)), label_rows, n)
    print(f"{len(labeller.vocab):,} candidate terms over {len(label_rows):,} titles")

    with steps.step("titles"):
        # `Labeller.sampled` maps a corpus row to the vectoriser's row space and drops the rows it
        # never saw; where every title was seen the two spaces are the same and this is a no-op.
        text = labeller.label({c: labeller.sampled(rows) for c, rows in members.items()})
    print(f"{len(text)} of {k} clusters got a distinctive title")

    # -------------------------------------------------------------------------- the build inputs
    entity = np.arange(n, dtype=np.uint64)
    extra = {
        # Cast in Arrow rather than through a list of 36M Python strings.
        "pmid": pc.cast(table.column("pmid"), pa.string()),
        "published": table.column("published"),
        "title": table.column("title"),
        "mesh_major": major_names,
    }
    with steps.step("write points"):
        if args.abstracts:
            # Read here rather than with the base columns: ~30 GB of Arrow buffers at 36M rows,
            # wanted by the write and by nothing before it.
            extra["abstract"] = read_staged(out, take, ["abstract"]).column("abstract")
        write_points(out, entity=entity, xy=xy, access=access, extra=extra)
    branch_terms = sorted(counts)
    print(f"{len(flat_access):,} (article, branch) labels over {len(branch_terms)} terms")

    with steps.step("write artifacts"):
        for c in range(k):
            artifacts.artifact(KMEANS_LAYER, f"km-{c:06d}", contents=contents_of(text.get(c)))
    with steps.step("write members"):
        rng = np.random.default_rng(SEED)
        for c in range(k):
            artifacts.members(KMEANS_LAYER, f"km-{c:06d}", members[c])
            artifacts.generating_sets(KMEANS_LAYER, f"km-{c:06d}", members[c], rng=rng,
                                      sample=LABEL_SAMPLE, ranks=len(contents_of(text.get(c))))

    artifacts.check(n)
    artifact_rows, member_rows = artifacts.write(out)
    write_vocabulary(out, branch_terms)
    print(f"{artifact_rows:,} artifacts, {member_rows:,} member rows across "
          f"{len(artifacts.per_layer)} layers")

    with steps.step("write the demo's terms"):
        write_demo_terms(out, counts)

    write_declaration(out, abstracts=args.abstracts,
                      mesh_toml=MESH_TOML if mesh is not None else None)
    write_deployment(out)

    manifest = {
        "rung": RUNG,
        "sample": n,
        "corpus": n_full,
        "seed": SEED,
        "views": list(VIEWS),
        "abstracts": args.abstracts,
        "umap": dict(routes.UMAP_PARAMS, seed=routes.SEED, implementation="cuml",
                     graph="cagra fp16, sharded", shard_rows=args.shard,
                     managed_memory=args.managed),
        "routes": timings,
        "bounds": {"knn": {"x": [float(xy[:, 0].min()), float(xy[:, 0].max())],
                           "y": [float(xy[:, 1].min()), float(xy[:, 1].max())]}},
        "kmeans": {"k": k, "titles": len(text),
                   "sizes": [int(sizes.min()), int(np.median(sizes)), int(sizes.max())]},
        "mesh": mesh_stats,
        "access_terms": len(branch_terms),
        "access_labels": len(flat_access),
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
    print(f"\nnext:\n  cd {out} && tessera check --payloads && tessera build\n"
          f"  ./run_demo.sh --deployment {out}/tessera.toml \\\n"
          f"      --terms \"$(cat {out}/branch-terms.txt)\" --ranks {out}/branch-ranks.json \\\n"
          f"      --label 'PubMed: a literature map' --prose title")


if __name__ == "__main__":
    sys.exit(main())
