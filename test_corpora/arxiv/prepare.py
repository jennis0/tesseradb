"""arXiv — 2,422,486 preprints, the ladder's bottom rung and the only one with an embedding.

One pass from the arXiv sources to a corpus `tessera build` consumes: the BGE embedding laid out
**twice**, two clusterings over the first layout, and a c-TF-IDF title on every cluster. A third
clustering — Toponymy's layered one, named by a language model — is the rung's optional second
stage, [`toponymy.py`][], because it needs a chat endpoint and costs an hour and a half over the
whole corpus where this stage costs minutes.

**This rung's source is derived, not staged.** `data/` in this checkout is what
`probes/build_corpus.py` and `probes/build_embeddings.py` produced; the share holds a mirror of it
rather than a publisher's bytes. See `sources.py` and `../README.md`.

**Two views over one entity space** (owner direction, 2026-09-01), and they are the same papers
positioned two ways rather than two corpora:

- `knn` — a cosine kNN graph in **full dimension** on the GPU, handed to UMAP as a
  `precomputed_knn` so UMAP does the layout and nothing else.
- `pca64` — PCA to 64 components first, which is the route `data/geometry.parquet` was built on.

Both run cuML's UMAP with a fixed `random_state` (`routes.py`). **They are here to exercise the
multi-view machinery on a real corpus**, and because `knn` is the route that scales to the ladder's
later embedding rungs — not to settle which projection is more faithful. It is also the faster of
the two end to end, and the one whose clusters stay contiguous in row space; the README has both
figures.

**This rung is the ladder's one irreproducible corpus, and it is worth being exact about which
part.** A geographic rung's positions are a pure function of its input, so a frame change costs a
rerun. Here they are not, and the seed only covers half of it: `pca64` reproduced bit for bit
across two runs at 200,000 on 2026-09-01, PCA being deterministic and cuML's UMAP being seeded, and
⊘ **`knn` did not** — CAGRA's index build is an approximate GPU construction that takes no seed, so
the graph UMAP is handed differs run to run and the layout with it. Neither view is
`data/geometry.parquet` either, which was built by a different UMAP under no seed at all. A figure
taken against one geometry is not comparable with a figure taken against another, and the manifest
records which run produced which. See the README for the two runs' bounds.

Four other decisions this stage makes, each because the data forced it:

- **The sample is uniform and then sorted.** The corpus is in submission order, so a prefix takes
  the oldest papers and changes the category skew and the density structure the clusterings
  depend on.
- **The condensed tree's chains are collapsed.** HDBSCAN emits a cluster node for every point that
  detaches near the root, so the tree's top is forty-odd nodes each holding all but a few hundred
  of the previous one's members. A budgeted cut spends its whole budget there and never reaches a
  split. A node whose largest child holds more than 90% of its members is dropped and its children
  re-parented to the nearest kept ancestor; the root is kept whatever its shape, because a cut
  climbs to it, and a leaf has no child to be dominated by, so the selected clusters are intact.
- **No distinctive terms is the fallback alone.** A cluster the labeller has nothing to say about
  carries `a cluster of papers` and nothing above it. Dropping its content entirely is refused:
  the layer declares a supplied kind, and an artifact served without content its layer declares
  cannot be told apart from one whose content was withheld.
- **The access relation rides the points file.** One column of category names per paper, and each
  view declares `point_visibility = { field = "categories" }`. The terms are the category names
  themselves, so a grant is written `math.GT` and the build interns each name into its own
  dictionary — which is the only place a term id is decided, and why this script writes none.
"""

from __future__ import annotations

import argparse
import collections
import json
import resource
import shutil
import sys
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from ..common.paths import ladder
from ..common.timing import Steps
from . import routes, sources
from .writer import ArtifactSet

RUNG = "arxiv"
SEED = 0

KMEANS_LAYER = "clusters/kmeans"
HDBSCAN_LAYER = "clusters/hdbscan"

# **`taxonomy/arxiv` is withdrawn** (owner ruling, 2026-09-01), on the rule Overture's taxonomy was
# withdrawn on: a layer earns its place by *drawing* something in the view it is declared over. An
# archive's members are scattered across the whole map, so its box covers a tenth of it and its
# hull is the map — 97-98% of its artifacts were `everywhere` in the build's report, all 209 served
# on every viewport request for outlines that show nothing. The classification is still here and
# still useful: it is the `archive` and `primary_category` attributes, which colour and filter.
# The figures are in `README.md`.

#: The two views, in declaration order. The view name is the route name: a figure quoted against a
#: view says which projection produced it without a lookup. The first is the **anchor**
#: (`[defaults].allocation_view`, decision 0112) and carries the attribute columns.
VIEWS = ("knn", "pca64")

KMEANS_K = 64

#: A node whose largest child holds more than this share of its members is not a level.
CHAIN = 0.9

LABEL_TERMS = 4  #: how many c-TF-IDF terms make up one title
LABEL_SAMPLE = 200  #: documents a title is generated from — its generating set
MAX_CORPUS_SHARE = 0.02  #: above this a term is corpus vocabulary, not cluster vocabulary
MIN_CLUSTER_SHARE = 0.02  #: below this it is a coincidence rather than a description

#: Tokens that describe a physics or maths paper rather than a topic. They survive every
#: frequency rule this file has — each is under the 2% corpus cap and each is disproportionately
#: common in some clusters — so they are named. **Kept short on purpose**: a stoplist that grows
#: is a labeller being hand-tuned, and the c-TF-IDF denominator is what is meant to do this work.
CORPUS_STOPLIST = frozenset("""
production sqrt measurement results based using model models method methods approach study
analysis data paper show new two one problem problems case time
""".split())

#: The lowest-ranked content, served to a viewer who holds its narrower generating set but not the
#: whole of the specific one's. It names the kind of thing without describing this one.
FALLBACK = "a cluster of papers"


def contents_of(text: str | None) -> list[list[str]]:
    """A cluster's ranked contents: the description first where there is one, the fallback last.

    Never empty. The clustering layers declare a supplied kind, and an artifact carrying none of a
    kind its layer declares is refused at publication — it could not be told apart from one whose
    content was withheld.
    """
    return [[text], [FALLBACK]] if text else [[FALLBACK]]


def min_cluster_size(n: int) -> int:
    """HDBSCAN's floor, scaled with the sample so the tree's shape is comparable across sizes
    rather than dissolving into noise at the small end and into a handful of giants at the large
    one."""
    return max(50, n // 400)


# ---------------------------------------------------------------------------------- the tree


class Tree:
    """HDBSCAN's condensed tree, read as a hierarchy and then collapsed.

    Its rows are `(parent, child, lambda_val, child_size)`. Nodes numbered below the point count
    are individual papers; nodes at or above it are clusters, and the first of them is the root.
    A row whose `child_size > 1` is a cluster edge — this is the lineage. A row whose
    `child_size == 1` is a paper *detaching* from a cluster: that paper belongs to that cluster and
    to all of its ancestors, and **to none of its children**.

    That second point is where the non-covering property comes from, and it is the property this
    corpus exists to exercise: a rollup that unions the children and calls the result the parent is
    wrong on every real hierarchy while passing on every planted one.

    **It is given the tree and the labelling rather than the clusterer**, because the two HDBSCAN
    implementations hand them over differently — cuML's `_condensed_tree` is the record array this
    reads, and `hdbscan`'s `condensed_tree_.to_pandas()` has the same four columns. Nothing here
    depends on which produced them.
    """

    def __init__(self, condensed: np.ndarray, labels: np.ndarray, n: int):
        self.n = n
        self.root = n  # the first cluster node is numbered after the papers
        self.noise_share = float((labels == -1).mean())
        self.selected = int(len(set(labels.tolist())) - 1)

        parent = condensed["parent"].astype(np.int64)
        child = condensed["child"].astype(np.int64)
        size = condensed["child_size"].astype(np.int64)

        edge = size > 1
        self.parent_of = dict(zip(child[edge].tolist(), parent[edge].tolist()))

        # Where each paper detaches — exactly one cluster each.
        leaf = ~edge
        detach = np.full(n, -1, dtype=np.int64)
        detach[child[leaf]] = parent[leaf]
        assert (detach >= 0).all(), (
            "a paper detaches from no cluster, which the condensed tree cannot produce"
        )
        self.own = collections.defaultdict(list)
        for paper, at in enumerate(detach.tolist()):
            self.own[at].append(paper)

        self._rebuild()
        # Members: the papers detaching at a cluster, plus everything under its descendants.
        # Bottom-up, so each node is visited once rather than once per ancestor.
        self.members_of = {}
        for c in sorted(self.nodes, key=lambda c: -self.depth_of[c]):
            acc = list(self.own.get(c, ()))
            for kid in self.children_of[c]:
                acc.extend(self.members_of[kid])
            self.members_of[c] = acc

    def _rebuild(self) -> None:
        self.nodes = sorted({self.root} | set(self.parent_of))
        self.children_of = collections.defaultdict(list)
        for c, p in self.parent_of.items():
            self.children_of[p].append(c)
        self.depth_of = {c: self._depth(c) for c in self.nodes}

    def _depth(self, c: int) -> int:
        d = 0
        while c in self.parent_of:
            c = self.parent_of[c]
            d += 1
        return d

    def collapse_chains(self) -> dict:
        """Drop every node whose largest child dominates it, re-parenting its children to the
        nearest kept ancestor. Returns what was done, for the manifest.

        **A collapsed node's own detached papers move to the ancestor that absorbs them.** They
        are members of it either way — `members_of` is unchanged by a collapse — but `own` is what
        the stray share is computed from, and leaving them behind understates the very property
        this corpus is here to exercise.
        """
        before_nodes, before_depth = len(self.nodes), max(self.depth_of.values())
        chain = {
            c
            for c in self.nodes
            if c != self.root
            and self.children_of[c]
            and max(len(self.members_of[k]) for k in self.children_of[c])
            > CHAIN * len(self.members_of[c])
        }
        kept_ancestor = {}
        for c in chain:
            p = self.parent_of[c]
            while p in chain:
                p = self.parent_of[p]
            kept_ancestor[c] = p

        collapsed_parent = {}
        for c, p in self.parent_of.items():
            while p in chain:
                p = self.parent_of[p]
            collapsed_parent[c] = p
        self.parent_of = {c: p for c, p in collapsed_parent.items() if c not in chain}
        self._rebuild()

        for c in chain:
            self.own[kept_ancestor[c]].extend(self.own.pop(c, []))
        self.members_of = {c: self.members_of[c] for c in self.nodes}

        return {
            "nodes_before": before_nodes,
            "depth_before": before_depth,
            "collapsed": len(chain),
            "rule": f"largest child > {CHAIN:.0%} of the parent",
        }

    def stray_shares(self) -> np.ndarray:
        """Per internal cluster, the share of its members that belong to it and to **no child** —
        the papers a split leaves behind."""
        internal = [c for c in self.nodes if self.children_of[c]]
        return np.array(
            [len(self.own.get(c, ())) / max(1, len(self.members_of[c])) for c in internal]
        )


# ---------------------------------------------------------------------------------- the labels


class Labeller:
    """Cluster titles by **c-TF-IDF against the layer's own sibling clusters** — how much more a
    term occurs in this cluster than across the clusters it is drawn beside.

    **Plain TF-IDF over each cluster treated as one long document gives unusable labels**, and it
    is worth recording because it is the first thing anyone writes. Concatenating a cluster's
    titles makes every term's frequency enormous, so the ranking is decided almost entirely by
    inverse document frequency across the 64-odd cluster "documents" — which rewards whatever is
    rarest. Measured on this corpus it returned `k_s chocs informatis`, `b_s0 tournament szeg`,
    `mast 892 ccd`: real tokens, each unique to one cluster, none describing anything.

    What works is the same two ingredients the other way round —

        score(t, C) = f(t, C) · log( f(t, C) / mean_C' f(t, C') )

    where *f* is the share of a cluster's documents containing the term, and the denominator runs
    over **every cluster of the same layer**, each weighted equally. A term scores well by being
    both common in this cluster and disproportionately so among its siblings.

    **The denominator was the whole corpus and that is what made the titles thin.** Against a
    corpus average, a term common to a whole discipline still clears the bar in every cluster of
    that discipline, so the first run over the full corpus produced `production measurement sqrt`,
    `tev sqrt search` and `brauer production modular` — the physics-paper register, not the
    cluster. Against the siblings, a term every neighbouring cluster also carries cancels.

    **Label quality tracks the clustering, not this function.** These clusters are drawn in a 2D
    UMAP projection, so they are spatially coherent and only roughly topical, and the labels say
    so. What the rung demonstrates is the mechanism — a title is supplied content with its own
    gate, its own generating set and its own lifecycle — and that is unaffected by how good the
    words are.
    """

    def __init__(self, titles: list[str]):
        from sklearn.feature_extraction.text import ENGLISH_STOP_WORDS, CountVectorizer

        # Binary occurrence rather than counts: the score is over the *share of documents*
        # carrying a term, so a title repeating a word does not make it more characteristic.
        vec = CountVectorizer(
            stop_words=list(ENGLISH_STOP_WORDS | CORPUS_STOPLIST),
            # Alphabetic, three characters or more: without this the ranking fills with fragments
            # of identifiers and bare numbers (`b_s0`, `892`), distinctive and meaningless.
            token_pattern=r"(?u)\b[a-zA-Z][a-zA-Z-]{2,}\b",
            min_df=10,
            max_df=MAX_CORPUS_SHARE,
            binary=True,
        )
        self.occurs = vec.fit_transform(titles)
        self.vocab = np.array(vec.get_feature_names_out())

    def label(self, groups: dict, terms: int = LABEL_TERMS) -> dict:
        """`{key: [row indices]}` -> `{key: "term term term term"}`, omitting the keys with nothing
        distinctive to say.

        **The whole layer at once, because the denominator is the layer.** A cluster cannot be
        titled on its own here: what makes a term worth printing is that its siblings do not carry
        it, so every group's share is computed first and the background is their mean.
        """
        keys = list(groups)
        shares = np.empty((len(keys), len(self.vocab)), dtype=np.float64)
        for i, key in enumerate(keys):
            rows = np.asarray(groups[key])
            shares[i] = np.asarray(self.occurs[rows].sum(axis=0)).ravel() / max(1, len(rows))
        # Each cluster counts once, whatever its size: pooling the documents instead would let one
        # cluster holding half the layer set the background it is then scored against.
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


# --------------------------------------------------------------------------------- the outputs


def write_points(out: Path, view: str, *, entity, xy, access, extra=None) -> None:
    """One view's positions, one row per paper.

    **Every view file carries the access column.** `point_visibility` is declared per view and
    reads the view's own source, so the column has to be on each. The attribute columns are
    entity-space and sit on the anchor view's file alone, which `[defaults].source` names — a
    paper's title is not a property of a projection.

    **Raw coordinates, written as they are.** Each view's `extent = "auto"` fits a square box
    around exactly these numbers, so there is nothing here to keep in step with a number on a
    command line, and the two layouts have different ranges — which is precisely why the two views
    cannot share a frame. Scaling by hand is what this pipeline used to do, and it is the failure
    the extent moved into the declaration to prevent: coordinates spanning about -17..18 written
    against a stated frame of 0..65536 put the whole corpus in a speck in one corner, with no
    clamps and no error anywhere.
    """
    cols = {
        "entity_id": pa.array(entity, pa.uint64()),
        "x": pa.array(xy[:, 0].astype(np.float64), pa.float64()),
        "y": pa.array(xy[:, 1].astype(np.float64), pa.float64()),
        "categories": pa.array(access, pa.list_(pa.string())),
    }
    cols.update(extra or {})
    name = "points.parquet" if view == VIEWS[0] else f"points-{view}.parquet"
    pq.write_table(pa.table(cols), out / name)


def write_vocabularies(out: Path, archive, primary) -> None:
    """The two category vocabularies: `key`, the code it is pinned at, and a title. Code 0 is the
    *absent* sentinel and is refused in a declared set, so the codes start at one."""
    for name, values in [("archive", sorted(set(archive))), ("primary_category", sorted(set(primary)))]:
        pq.write_table(
            pa.table(
                {
                    "key": pa.array(values, pa.string()),
                    "code": pa.array(np.arange(1, len(values) + 1, dtype=np.uint32), pa.uint32()),
                    "title": pa.array(values, pa.string()),
                }
            ),
            out / f"{name}.parquet",
        )


def write_demo_terms(out: Path, access) -> None:
    """**The demo's candidate terms, ranked by coverage.** `run_demo.sh --ranks` composes its
    sparse/medium/heavy principals out of this list, and `--terms` is the candidate set.

    A term id names a different set in every dictionary, so a corpus with its own dictionary has to
    name its own terms or every principal measures empty against the demo's synthetic `0..200` and
    the viewer opens on a blank map with nothing to say why. The access term here is the arXiv
    category, which is the same synthetic-policy-over-real-data shape the geographic rungs use: a
    real column standing in for a compartment scheme the source does not carry.
    """
    counts = collections.Counter(t for row in access for t in row)
    ranks = counts.most_common()
    (out / "category-ranks.json").write_text(
        json.dumps([{"term": t, "pairs": n} for t, n in ranks], indent=None) + "\n"
    )
    (out / "category-terms.txt").write_text(",".join(t for t, _ in ranks) + "\n")
    print(f"{len(ranks)} terms; top five " + ", ".join(f"{t} {n:,}" for t, n in ranks[:5]))


def write_deployment(out: Path) -> None:
    """`tessera.toml`, generated rather than committed — every value in it is a path or a port on
    this machine, which is the split `configuration.md` §3 draws. Its paths are relative because
    they resolve against **its own directory**: a relative `bundle.path` that moved with the
    shell's working directory would make `cd crates && tessera serve` open a different bundle from
    the one `tessera build` had just written."""
    (out / "tessera.toml").write_text(
        """# Generated by `test_corpora/arxiv/prepare.py`. Machine-specific by construction: paths
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
viewer  = "127.0.0.1:8091"
session = "127.0.0.1:8092"
control = "127.0.0.1:8093"
max_k   = 5000
session_credential_env  = "TESSERA_ARXIV_SESSION_CRED"
operator_credential_env = "TESSERA_ARXIV_OPERATOR_CRED"

# Development only: the origin the demo viewer is served from (client-interaction §7). Without
# it the viewer loads and every request from it fails CORS, which reads like a broken server.
dev_cors_origins = ["http://localhost:5173", "http://127.0.0.1:5173"]
"""
    )

    # **The directory the WAL and the cache sit in**, which the server does not create: it refuses
    # to start with `wal io error: No such file or directory` and names no path. The declaration
    # above is the only place those two paths are written, so the `mkdir` belongs beside it.
    (out / ".tessera").mkdir(exist_ok=True)

    # **The identity key is created once and never overwritten.** It is what every `tessera_id`
    # this corpus has ever served is derived from, so regenerating it on a rerun would invalidate
    # every identifier a client holds and reorder every row (contracts §2.2). The two plane
    # credentials are ordinary secrets and are minted the same way, each only if absent — so a
    # directory written before they existed gains them without losing its lineage.
    import secrets

    env = out / ".env"
    lines = env.read_text().splitlines() if env.exists() else []
    held = {line.split("=", 1)[0] for line in lines if "=" in line}
    minted = [
        f"{var}={secrets.token_hex(16)}"
        for var in ("TESSERA_IDENTITY_KEY", "TESSERA_ARXIV_SESSION_CRED",
                    "TESSERA_ARXIV_OPERATOR_CRED")
        if var not in held
    ]
    if minted:
        env.write_text("\n".join(lines + minted) + "\n")
        env.chmod(0o600)
        print(f"minted {', '.join(m.split('=')[0] for m in minted)} in {env}")


# ------------------------------------------------------------------------------------- the run


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--sample", type=int, default=200_000,
                    help="papers to take, uniformly; 0 takes all 2,422,486")
    ap.add_argument("--out", type=Path, default=None, help=f"default $TESSERA_LADDER/{RUNG}")
    ap.add_argument("--no-half", action="store_true",
                    help="build the kNN index in fp32; needs the vectors to fit the card")
    args = ap.parse_args()

    out = args.out or ladder(RUNG)
    out.mkdir(parents=True, exist_ok=True)
    steps = Steps()
    print(f"source  {sources.DATA}\noutput  {out}")

    # --------------------------------------------------------------------- the corpus and sample
    with steps.step("load metadata"):
        # The prose columns are *not* read here. They are Python-object columns of 2,422,486 rows
        # and the routes below need none of them, so reading them now would hold several GB
        # through both UMAP runs for nothing; only the entity-order check is wanted at this point.
        corpus, _ = sources.load_metadata(prose_columns=())
        n_full = corpus.num_rows
        take = sources.sample_rows(n_full, args.sample or None, SEED)
        n = len(take)
    print(f"{n:,} papers sampled from {n_full:,}")

    with steps.step("load embeddings"):
        X = sources.load_embeddings(corpus, take)
    print(f"{n:,} papers x {X.shape[1]} dimensions")

    # ------------------------------------------------------------------------------ the two routes
    #
    # **Nothing else is materialised yet**, for the same reason: hold one large structure at a
    # time, not two. The columns come after `del X`.
    timings, positions = {}, {}
    for view in VIEWS:
        print(f"  route {view}", flush=True)
        t, t0 = {}, time.time()
        kw = {} if view == "pca64" else {"half": not args.no_half}
        with steps.step(f"route {view}"):
            positions[view] = routes.ROUTES[view](X, t, **kw)
        t["total"] = time.time() - t0
        timings[view] = t
        xy = positions[view]
        print(f"  route {view}: {t['total']:.0f}s  x [{xy[:, 0].min():.2f}, {xy[:, 0].max():.2f}] "
              f"y [{xy[:, 1].min():.2f}, {xy[:, 1].max():.2f}]  (peak so far "
              f"{resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20:.1f} GB)", flush=True)
    del X

    # **The clusterings run over the anchor view's layout**, and are drawn in both. That is the
    # experiment: one membership, resolved per view, so a cluster's spread in each projection is a
    # property of the artifacts the server serves rather than a statistic about the point cloud.
    xy = positions[VIEWS[0]]

    with steps.step("materialise columns"):
        arxiv_id = np.asarray(corpus.column("id"))[take]
        categories = np.asarray(corpus.column("categories"))[take]
        created = corpus.column("v1_created").to_numpy(zero_copy_only=False)[take]
        # `archive` is the part of a category before the dot (`math`), `primary_category` the whole
        # of the first one (`math.GT`) — the two grains a client filters at.
        primary = np.array([c.split()[0] if c else "unknown" for c in categories])
        archive = np.array([p.split(".")[0] for p in primary])
        access = [c.split() if c else [] for c in categories]
        # **The authors, joined in Arrow rather than in Python.** `corpus.parquet` carries them as
        # a `list<string>` of surnames — 2,832 of them on one paper — and `binary_join` turns the
        # whole column into one string column without materialising a list per row. Nothing
        # surfaced them before; a paper map is asked "who wrote this" as often as "when".
        authors = pc.binary_join(corpus.column("surnames").take(take), ", ")
        title = sources.load_prose_column("title", take)
        abstract = sources.load_prose_column("abstract", take)

    # ---------------------------------------------------------------------------- the clusterings
    # **cuML's k-means and HDBSCAN**, on the GPU and in the same environment as the routes. The
    # `hdbscan` package is what this rung used and it exposed `condensed_tree_`, which *is* the
    # hierarchy; cuML exposes the same four columns as a record array (`Tree`), so nothing about
    # the tree's shape or the non-covering property depends on which of the two ran.
    from cuml.cluster import HDBSCAN, KMeans

    with steps.step("k-means"):
        kmeans_label = np.asarray(
            KMeans(n_clusters=KMEANS_K, random_state=SEED, n_init=1).fit(xy).labels_
        ).astype(np.int32)
    sizes = np.bincount(kmeans_label, minlength=KMEANS_K)
    print(
        f"{KMEANS_K} clusters, sizes {sizes.min():,}..{sizes.max():,} "
        f"(median {int(np.median(sizes)):,})"
    )

    with steps.step("hdbscan"):
        mcs = min_cluster_size(n)
        clusterer = HDBSCAN(min_cluster_size=mcs, min_samples=10).fit(xy)
        tree = Tree(clusterer._condensed_tree, np.asarray(clusterer.labels_), n)
        del clusterer
    print(
        f"min_cluster_size {mcs}, {tree.selected} selected clusters, "
        f"{tree.noise_share:.1%} of papers in none of them; "
        f"{len(tree.nodes)} in the tree, depth up to {max(tree.depth_of.values())}"
    )
    collapsed = tree.collapse_chains()
    print(
        f"{collapsed['collapsed']} chain nodes collapsed: {len(tree.nodes)} clusters, "
        f"depth up to {max(tree.depth_of.values())}"
    )

    stray = tree.stray_shares()
    print(
        f"stray share over {len(stray)} internal clusters: mean {stray.mean():.1%}, "
        f"median {np.median(stray):.1%}, max {stray.max():.1%}; "
        f"{(stray == 0).sum()} whose children exhaust them"
    )

    # -------------------------------------------------------------------------------- the titles
    with steps.step("vectorise titles"):
        labeller = Labeller(title.tolist())
    print(f"{len(labeller.vocab):,} candidate terms after dropping corpus vocabulary")

    with steps.step("titles"):
        kmeans_groups = {
            int(k): np.flatnonzero(kmeans_label == k).tolist() for k in range(KMEANS_K)
        }
        # Naming every node of a deep tree means naming near-duplicates of each other; the clusters
        # worth naming are the ones with enough members to have a distinctive vocabulary.
        hdbscan_groups = {c: tree.members_of[c] for c in tree.nodes if len(tree.members_of[c]) >= mcs}
        kmeans_text = labeller.label(kmeans_groups)
        hdbscan_text = labeller.label(hdbscan_groups)
    print(f"{len(kmeans_text)} k-means titles, {len(hdbscan_text)} hdbscan titles")

    # -------------------------------------------------------------------------- the build inputs
    entity = np.arange(n, dtype=np.uint64)
    with steps.step("write points"):
        write_points(out, VIEWS[0], entity=entity, xy=positions[VIEWS[0]], access=access, extra={
            "arxiv_id": pa.array(arxiv_id, pa.string()),
            "archive": pa.array(archive, pa.string()),
            "primary_category": pa.array(primary, pa.string()),
            "submitted_at": pa.array(created.astype("datetime64[us]"), pa.timestamp("us")),
            "authors": authors,
            "title": pa.array(title, pa.string()),
            "abstract": pa.array(abstract, pa.string()),
        })
        for view in VIEWS[1:]:
            write_points(out, view, entity=entity, xy=positions[view], access=access)
    access_terms = sorted({t for cats in access for t in cats})
    print(f"{sum(len(c) for c in access):,} (paper, category) labels over {len(access_terms)} terms")

    artifacts = ArtifactSet()

    def cluster_key(prefix, k):
        return f"{prefix}-{k:06d}"

    with steps.step("write artifacts"):
        # **A cluster carries its own title**, as ranked supplied content: the c-TF-IDF description
        # first, the generic fallback second. A viewer is served the first whose generating set
        # they hold entirely, or nothing — never the cluster's identity with its description
        # missing. The title used to be an artifact of its own on a `topics/*` layer that the
        # client joined by attachment; as content on the cluster it needs no join, and
        # `clients/ts/deck/src/layer.ts` names an artifact from its own `content[0]`.
        #
        # **A cluster the labeller has nothing distinctive to say about carries the fallback and
        # nothing above it**, rather than no content at all. The layer declares a supplied kind, so
        # an artifact carrying none of it is refused at publication: an artifact served without
        # content its layer declares cannot be told apart from one whose content was withheld. That
        # refusal is the disclosure boundary doing its job, and the fallback is what the ranking
        # was for — a true statement about the cluster that describes no document in it.
        for k in range(KMEANS_K):
            artifacts.artifact(KMEANS_LAYER, cluster_key("km", k),
                               contents=contents_of(kmeans_text.get(k)))
        for c in tree.nodes:
            parent = tree.parent_of.get(c)
            artifacts.artifact(
                HDBSCAN_LAYER,
                cluster_key("hdb", c),
                contents=contents_of(hdbscan_text.get(c)),
                parent=cluster_key("hdb", parent) if parent is not None else None,
            )

    with steps.step("write members"):
        # A cluster's own membership is the cluster; each content's generating set is a sample
        # drawn from it, and so a subset of that membership by construction. The sample is what is
        # recorded as the provenance a viewer must hold, and it is the same draw the label layers
        # made — the c-TF-IDF ranking itself was computed over the whole cluster.
        rng = np.random.default_rng(SEED)
        for k in range(KMEANS_K):
            artifacts.members(KMEANS_LAYER, cluster_key("km", k), kmeans_groups[k])
            artifacts.generating_sets(KMEANS_LAYER, cluster_key("km", k), kmeans_groups[k],
                                      rng=rng, sample=LABEL_SAMPLE,
                                      ranks=len(contents_of(kmeans_text.get(k))))
        for c in tree.nodes:
            artifacts.members(HDBSCAN_LAYER, cluster_key("hdb", c), tree.members_of[c])
            artifacts.generating_sets(HDBSCAN_LAYER, cluster_key("hdb", c), tree.members_of[c],
                                      rng=rng, sample=LABEL_SAMPLE,
                                      ranks=len(contents_of(hdbscan_text.get(c))))

    # --------------------------------------------------------------------------------- the checks
    # A child's members are a subset of its parent's. This is what makes rollup sound under an
    # absolute criterion, and the build reports every edge that breaks it. The condensed tree
    # cannot produce a violation; the assertion is here because the member computation could.
    for c, p in tree.parent_of.items():
        if not set(tree.members_of[c]) <= set(tree.members_of[p]):
            raise AssertionError(f"cluster {c} escapes its parent {p}")
    # And they do not exhaust it, which is the property a planted tree would not have.
    assert (stray > 0).sum() > 0, "no cluster keeps members its children do not — the tree is covering"
    artifacts.check(n)

    artifact_rows, member_rows = artifacts.write(out)
    write_vocabularies(out, archive, primary)
    print(f"{artifact_rows:,} artifacts, {member_rows:,} member rows across {len(artifacts.per_layer)} layers")
    print(f"{len(set(archive))} archives, {len(set(primary))} subject classes")

    with steps.step("write the demo's terms"):
        write_demo_terms(out, access)

    # ------------------------------------------------------------------- the declaration and manifest
    # The declaration is copied rather than referenced: `[sources]` is relative to the declaring
    # document (`configuration.md` §3), so the copy beside the parquets is what resolves. The git
    # one stays the reviewed original — and `toponymy.py` appends its layer to the copy, never to
    # the original.
    shutil.copy(Path(__file__).parent / "corpus.toml", out / "corpus.toml")
    write_deployment(out)

    manifest = {
        "rung": RUNG,
        "sample": n,
        "corpus": n_full,
        "seed": SEED,
        "views": list(VIEWS),
        "umap": dict(routes.UMAP_PARAMS, seed=routes.SEED, pca=routes.PCA_DIM,
                     implementation="cuml"),
        "routes": timings,
        "bounds": {
            v: {"x": [float(positions[v][:, 0].min()), float(positions[v][:, 0].max())],
                "y": [float(positions[v][:, 1].min()), float(positions[v][:, 1].max())]}
            for v in VIEWS
        },
        "kmeans": {"k": KMEANS_K, "titles": len(kmeans_text)},
        "hdbscan": {
            "min_cluster_size": int(mcs),
            "clusters_in_tree": len(tree.nodes),
            "selected_clusters": tree.selected,
            "max_depth": int(max(tree.depth_of.values())),
            "noise_share": round(tree.noise_share, 4),
            "stray_share_mean": round(float(stray.mean()), 4),
            "titles": len(hdbscan_text),
            "chains_collapsed": collapsed,
        },
        "access_terms": len(access_terms),
        "access_labels": sum(len(c) for c in access),
        "artifact_rows": artifact_rows,
        "member_rows": member_rows,
        "seconds": dict(steps),
        "total_seconds": steps.total(),
        "peak_rss_gb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 2**20, 2),
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")

    print(f"\nwrote to {out}:")
    for f in sorted(out.iterdir()):
        if f.is_file():
            print(f"  {f.name:34} {f.stat().st_size / 1e6:8.2f} MB")
    print(f"\nnext:\n  cd {out} && tessera check --payloads && tessera build\n"
          f"  ./run_demo.sh --deployment {out}/tessera.toml \\\n"
          f"      --terms \"$(cat {out}/category-terms.txt)\" --ranks {out}/category-ranks.json \\\n"
          f"      --label 'arXiv: two projections' --prose title,abstract")
    print(f"\nor, for the named-topic layer first:\n"
          f"  python -m test_corpora.arxiv.toponymy --out {out}")


if __name__ == "__main__":
    sys.exit(main())
