"""arXiv — 2,422,486 preprints, the ladder's bottom rung and the only one with an embedding.

One pass from the arXiv sources to a corpus `tessera build` consumes: the BGE embedding projected
by PCA and UMAP, two clusterings over the projection, arXiv's own taxonomy beside them, and a
TF-IDF label set over each clustering. A third clustering — Toponymy's layered one, named by a
language model — is the rung's optional second stage, [`toponymy.py`][], because it needs a chat
endpoint and costs an hour and a half over the whole corpus where this stage costs twenty minutes.

**This rung's source is derived, not staged.** `data/` in this checkout is what
`probes/build_corpus.py` and `probes/build_embeddings.py` produced; the share holds a mirror of it
rather than a publisher's bytes. See `sources.py` and `../README.md`.

**This rung is the ladder's one irreproducible corpus, and it is worth being exact about which
part.** A geographic rung's positions are a pure function of its input, so a frame change costs a
rerun. Here the projection is not: `data/geometry.parquet` was built with cuML on a GPU and is
bit-reproducible under no seed at all, and umap-learn is reproducible only when *seeded*, which
also makes it single-threaded — hours for the whole corpus against minutes. So a run produces one
of three geometries and the manifest says which:

- ``--umap reuse`` reads `geometry.parquet`, which is the projection every figure in
  `docs/evidence/` was taken against. Reach for it when a run has to be comparable with those.
- ``--umap recompute`` **seeded** is reproducible bit for bit from this script alone.
- ``--umap recompute`` **parallel** is a hashed artifact in the same sense `geometry.parquet` is:
  computed once, kept, and named by what produced it rather than re-derived.

``--seeded auto`` draws the line at 200,000 papers.

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
- **No distinctive terms is no label.** A placeholder string published as a topic is drawn as one;
  the client shows nothing for a cluster with no label, which is the honest rendering.
- **The access relation rides the points file.** One column of category names per paper, and the
  view declares `point_visibility = { field = "categories" }`. The terms are the category names
  themselves, so a grant is written `math.GT` and the build interns each name into its own
  dictionary — which is the only place a term id is decided, and why this script writes none.
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import shutil
import sys
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

from ..common.paths import ladder
from ..common.timing import Steps
from . import sources
from .writer import ArtifactSet

RUNG = "arxiv"
SEED = 0

KMEANS_LAYER, KMEANS_LABELS = "clusters/kmeans", "topics/kmeans"
HDBSCAN_LAYER, HDBSCAN_LABELS = "clusters/hdbscan", "topics/hdbscan"
TAXONOMY_LAYER = "taxonomy/arxiv"

KMEANS_K = 64
PCA_DIM = 64
UMAP_PARAMS = dict(n_neighbors=15, min_dist=0.1, n_components=2)

#: A node whose largest child holds more than this share of its members is not a level.
CHAIN = 0.9

LABEL_TERMS = 3  #: how many TF-IDF terms make up one label
LABEL_SAMPLE = 200  #: documents a TF-IDF label is generated from — its generating set
MAX_CORPUS_SHARE = 0.02  #: above this a term is corpus vocabulary, not cluster vocabulary
MIN_CLUSTER_SHARE = 0.02  #: below this it is a coincidence rather than a description


def min_cluster_size(n: int) -> int:
    """HDBSCAN's floor, scaled with the sample so the tree's shape is comparable across sizes
    rather than dissolving into noise at the small end and into a handful of giants at the large
    one."""
    return max(50, n // 400)


# ------------------------------------------------------------------------------------ geometry


def project(X: np.ndarray, seeded: bool, steps: Steps) -> tuple[np.ndarray, float]:
    """PCA to 64 components, then UMAP to 2 — the same shape as `probes/build_geometry.py`, on the
    CPU rather than the GPU. Returns the positions and the variance PCA kept."""
    n = len(X)
    with steps.step("pca"):
        # The covariance is accumulated in blocks: centring the whole matrix at once copies it.
        mu = X.mean(axis=0)
        cov = np.zeros((X.shape[1], X.shape[1]), dtype=np.float64)
        for lo in range(0, n, 200_000):
            d = (X[lo : lo + 200_000] - mu).astype(np.float64)
            cov += d.T @ d
        cov /= n
        evals, evecs = np.linalg.eigh(cov)
        basis = evecs[:, ::-1][:, :PCA_DIM].astype(np.float32)
        reduced = np.empty((n, PCA_DIM), dtype=np.float32)
        for lo in range(0, n, 200_000):
            reduced[lo : lo + 200_000] = (X[lo : lo + 200_000] - mu) @ basis
        kept = float(evals[::-1][:PCA_DIM].sum() / evals.sum())
        print(f"{PCA_DIM} components keep {100 * kept:.1f}% of the variance")

    with steps.step("umap"):
        import umap

        params = dict(UMAP_PARAMS, random_state=SEED) if seeded else dict(UMAP_PARAMS)
        print(
            f"umap-learn over {n:,} x {PCA_DIM}, "
            f"{'seeded and single-threaded' if seeded else 'parallel and unseeded'}"
        )
        xy = np.asarray(umap.UMAP(**params).fit_transform(reduced), dtype=np.float32)
    return xy, kept


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
    """

    def __init__(self, clusterer, n: int):
        tree = clusterer.condensed_tree_.to_pandas()
        self.n = n
        self.root = n  # the first cluster node is numbered after the papers
        self.noise_share = float((clusterer.labels_ == -1).mean())
        self.selected = int(len(set(clusterer.labels_)) - 1)

        edges = tree[tree.child_size > 1]
        self.parent_of = {int(r.child): int(r.parent) for r in edges.itertuples()}

        # Where each paper detaches — exactly one cluster each.
        leaves = tree[tree.child_size == 1]
        detach = np.full(n, -1, dtype=np.int64)
        detach[leaves.child.to_numpy().astype(np.int64)] = leaves.parent.to_numpy().astype(np.int64)
        assert (detach >= 0).all(), (
            "a paper detaches from no cluster, which the condensed tree cannot produce"
        )
        self.own = collections.defaultdict(list)
        for paper, at in enumerate(detach):
            self.own[int(at)].append(paper)

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
    """TF-IDF labels over the titles, by term frequency *within* a cluster against document
    frequency *across* the corpus.

    **Plain TF-IDF over each cluster treated as one long document gives unusable labels**, and it
    is worth recording because it is the first thing anyone writes. Concatenating a cluster's
    titles makes every term's frequency enormous, so the ranking is decided almost entirely by
    inverse document frequency across the 64-odd cluster "documents" — which rewards whatever is
    rarest. Measured on this corpus it returned `k_s chocs informatis`, `b_s0 tournament szeg`,
    `mast 892 ccd`: real tokens, each unique to one cluster, none describing anything.

    What works is the same two ingredients the other way round —

        score(t, C) = f(t, C) · log( f(t, C) / f(t, corpus) )

    where *f* is the share of documents containing the term. A term scores well by being both
    common in the cluster and disproportionately so.

    **Label quality tracks the clustering, not this function.** These clusters are drawn in a 2D
    UMAP projection, so they are spatially coherent and only roughly topical, and the labels say
    so. What the rung demonstrates is the mechanism — a label is an artifact with its own gate,
    its own generating set and its own lifecycle — and that is unaffected by how good the words
    are.
    """

    def __init__(self, titles: list[str]):
        from sklearn.feature_extraction.text import CountVectorizer

        # Binary occurrence rather than counts: the score is over the *share of documents*
        # carrying a term, so a title repeating a word does not make it more characteristic.
        vec = CountVectorizer(
            stop_words="english",
            # Alphabetic, three characters or more: without this the ranking fills with fragments
            # of identifiers and bare numbers (`b_s0`, `892`), distinctive and meaningless.
            token_pattern=r"(?u)\b[a-zA-Z][a-zA-Z-]{2,}\b",
            min_df=10,
            max_df=MAX_CORPUS_SHARE,
            binary=True,
        )
        self.occurs = vec.fit_transform(titles)
        self.vocab = np.array(vec.get_feature_names_out())
        self.corpus_share = np.asarray(self.occurs.sum(axis=0)).ravel() / self.occurs.shape[0]

    def label(self, groups: dict, terms: int = LABEL_TERMS) -> dict:
        """`{key: [row indices]}` -> `{key: "term term term"}`, omitting the keys with nothing
        distinctive to say."""
        out = {}
        for key, rows in groups.items():
            rows = np.asarray(rows)
            share = np.asarray(self.occurs[rows].sum(axis=0)).ravel() / max(1, len(rows))
            score = np.where(
                share >= MIN_CLUSTER_SHARE,
                share * np.log(share / (self.corpus_share + 1e-9) + 1e-9),
                -np.inf,
            )
            top = np.argsort(score)[::-1][:terms]
            chosen = [self.vocab[t] for t in top if np.isfinite(score[t])]
            if chosen:
                out[key] = " ".join(chosen)
        return out


# --------------------------------------------------------------------------------- the outputs


def write_points(out: Path, *, entity, xy, access, arxiv_id, archive, primary, created, title,
                 abstract) -> None:
    """One row per paper: identity, raw UMAP position, the attribute columns a client filters and
    draws on, and the `categories` column that **is** the access control.

    **Raw coordinates, written as they are.** The frame is the view's own `extent = "auto"`, which
    fits a square box around exactly these numbers, so there is nothing here to keep in step with a
    number on a command line. Scaling by hand is what this pipeline used to do, and it is the
    failure the extent moved into the declaration to prevent: coordinates spanning about -17..18
    written against a stated frame of 0..65536 put the whole corpus in a speck in one corner, with
    no clamps and no error anywhere.
    """
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(entity, pa.uint64()),
                "x": pa.array(xy[:, 0].astype(np.float64), pa.float64()),
                "y": pa.array(xy[:, 1].astype(np.float64), pa.float64()),
                "categories": pa.array(access, pa.list_(pa.string())),
                "arxiv_id": pa.array(arxiv_id, pa.string()),
                "archive": pa.array(archive, pa.string()),
                "primary_category": pa.array(primary, pa.string()),
                "submitted_at": pa.array(created.astype("datetime64[us]"), pa.timestamp("us")),
                "title": pa.array(title, pa.string()),
                "abstract": pa.array(abstract, pa.string()),
            }
        ),
        out / "points.parquet",
    )


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
"""
    )

    # **Created once and never overwritten.** The identity key is what every `tessera_id` this
    # corpus has ever served is derived from, so regenerating it on a rerun would invalidate every
    # identifier a client holds and reorder every row (contracts §2.2).
    env = out / ".env"
    if not env.exists():
        import secrets

        env.write_text(f"TESSERA_IDENTITY_KEY={secrets.token_hex(16)}\n")
        env.chmod(0o600)
        print(f"minted a new identity key in {env} — this corpus's lineage starts here")


# ------------------------------------------------------------------------------------- the run


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--sample", type=int, default=200_000,
                    help="papers to take, uniformly; 0 takes all 2,422,486")
    ap.add_argument("--umap", choices=("recompute", "reuse"), default="recompute",
                    help="'reuse' reads data/geometry.parquet, the projection the measurement "
                         "record was taken against")
    ap.add_argument("--seeded", choices=("auto", "1", "0"), default="auto",
                    help="seed UMAP, which also makes it single-threaded; 'auto' seeds 200,000 or "
                         "fewer")
    ap.add_argument("--out", type=Path, default=None, help=f"default $TESSERA_LADDER/{RUNG}")
    args = ap.parse_args()

    out = args.out or ladder(RUNG)
    out.mkdir(parents=True, exist_ok=True)
    steps = Steps()
    print(f"source  {sources.DATA}\noutput  {out}")

    # --------------------------------------------------------------------- the corpus and sample
    with steps.step("load metadata"):
        corpus, prose = sources.load_metadata()
        n_full = corpus.num_rows

    with steps.step("sample"):
        take = sources.sample_rows(n_full, args.sample or None, SEED)
        n = len(take)
        arxiv_id = np.asarray(corpus.column("id"))[take]
        categories = np.asarray(corpus.column("categories"))[take]
        created = corpus.column("v1_created").to_numpy(zero_copy_only=False)[take]
        title = np.asarray(prose.column("title"))[take]
        abstract = np.asarray(prose.column("abstract"))[take]
    print(f"{n:,} papers sampled from {n_full:,}")

    # ------------------------------------------------------------------------------- the geometry
    # The embeddings are loaded whatever the geometry setting, because the optional Toponymy stage
    # needs them and because reading them once here says early whether they are readable at all.
    with steps.step("load embeddings"):
        X = sources.load_embeddings(corpus, take)

    if args.umap == "reuse":
        with steps.step("umap (reused)"):
            geo = pq.read_table(sources.DATA / "geometry.parquet", columns=["entity_id", "x", "y"])
            assert geo.num_rows == n_full
            xy = np.column_stack(
                [geo.column("x").to_numpy()[take], geo.column("y").to_numpy()[take]]
            ).astype(np.float32)
        seeded, variance_kept = None, None
    else:
        seeded = {"1": True, "0": False}.get(args.seeded, n <= 200_000)
        xy, variance_kept = project(X, seeded, steps)
    print(
        f"geometry {xy.shape}, x {xy[:, 0].min():.2f}..{xy[:, 0].max():.2f}, "
        f"y {xy[:, 1].min():.2f}..{xy[:, 1].max():.2f}"
    )

    # ---------------------------------------------------------------------------- the clusterings
    from sklearn.cluster import KMeans

    with steps.step("k-means"):
        kmeans_label = (
            KMeans(n_clusters=KMEANS_K, random_state=SEED, n_init="auto").fit(xy).labels_
        ).astype(np.int32)
    sizes = np.bincount(kmeans_label, minlength=KMEANS_K)
    print(
        f"{KMEANS_K} clusters, sizes {sizes.min():,}..{sizes.max():,} "
        f"(median {int(np.median(sizes)):,})"
    )

    import hdbscan

    with steps.step("hdbscan"):
        # The `hdbscan` package rather than scikit-learn's, for one reason: it exposes
        # `condensed_tree_`, which **is** the hierarchy. Reconstructing it from a single-linkage
        # tree would be reimplementing the algorithm's own output.
        mcs = min_cluster_size(n)
        clusterer = hdbscan.HDBSCAN(
            min_cluster_size=mcs, min_samples=10, core_dist_n_jobs=os.cpu_count()
        ).fit(xy)
        tree = Tree(clusterer, n)
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

    # -------------------------------------------------------------------------------- the labels
    with steps.step("vectorise titles"):
        labeller = Labeller(title.tolist())
    print(f"{len(labeller.vocab):,} candidate terms after dropping corpus vocabulary")

    with steps.step("labels"):
        kmeans_groups = {
            int(k): np.flatnonzero(kmeans_label == k).tolist() for k in range(KMEANS_K)
        }
        # Labelling every node of a deep tree means labelling near-duplicates of each other; the
        # clusters worth naming are the ones with enough members to have a distinctive vocabulary.
        hdbscan_groups = {c: tree.members_of[c] for c in tree.nodes if len(tree.members_of[c]) >= mcs}
        kmeans_text = labeller.label(kmeans_groups)
        hdbscan_text = labeller.label(hdbscan_groups)
    print(f"{len(kmeans_text)} k-means labels, {len(hdbscan_text)} hdbscan labels")

    # -------------------------------------------------------------------------- the build inputs
    entity = np.arange(n, dtype=np.uint64)
    with steps.step("write points"):
        # `archive` is the part of a category before the dot (`math`), `primary_category` the whole
        # of the first one (`math.GT`) — the two grains a client filters at.
        primary = np.array([c.split()[0] if c else "unknown" for c in categories])
        archive = np.array([p.split(".")[0] for p in primary])
        access = [c.split() if c else [] for c in categories]
        write_points(out, entity=entity, xy=xy, access=access, arxiv_id=arxiv_id, archive=archive,
                     primary=primary, created=created, title=title, abstract=abstract)
    access_terms = sorted({t for cats in access for t in cats})
    print(f"{sum(len(c) for c in access):,} (paper, category) labels over {len(access_terms)} terms")

    artifacts = ArtifactSet()

    def cluster_key(prefix, k):
        return f"{prefix}-{k:06d}"

    with steps.step("write artifacts"):
        # **The taxonomy: two levels, and edges that run between them.** arXiv's own classification
        # is the *tiered* shape — an archive contains its subject classes, a subject class is a
        # subject class everywhere on the map, and the resolution is *semantic* rather than
        # something a budget trades for. An archive with no subclass carries the same key at both
        # levels, which is legitimate: a key is unique per `(layer, level)`.
        for name in sorted(set(archive)):
            artifacts.artifact(TAXONOMY_LAYER, name, level=0)
        for name in sorted(set(primary)):
            artifacts.artifact(TAXONOMY_LAYER, name, level=1, parent=name.split(".")[0])

        for k in range(KMEANS_K):
            artifacts.artifact(KMEANS_LAYER, cluster_key("km", k))
        for c in tree.nodes:
            parent = tree.parent_of.get(c)
            artifacts.artifact(
                HDBSCAN_LAYER,
                cluster_key("hdb", c),
                parent=cluster_key("hdb", parent) if parent is not None else None,
            )

        # Labels, two ranked contents each: the specific description first, a generic fallback
        # second. A viewer is served the first whose generating set they hold entirely, or nothing
        # — never the cluster's identity with its description missing.
        for k, text in kmeans_text.items():
            artifacts.artifact(KMEANS_LABELS, f"kml-{k:06d}",
                               contents=[[text], ["a cluster of papers"]],
                               attached=(KMEANS_LAYER, 0, cluster_key("km", k)))
        for c, text in hdbscan_text.items():
            artifacts.artifact(HDBSCAN_LABELS, f"hdbl-{c:06d}",
                               contents=[[text], ["a cluster of papers"]],
                               attached=(HDBSCAN_LAYER, 0, cluster_key("hdb", c)))

    with steps.step("write members"):
        by_archive, by_primary = collections.defaultdict(list), collections.defaultdict(list)
        for row, (a, p) in enumerate(zip(archive, primary)):
            by_archive[a].append(row)
            by_primary[p].append(row)
        for name, rows in by_archive.items():
            artifacts.members(TAXONOMY_LAYER, name, rows, level=0)
        for name, rows in by_primary.items():
            artifacts.members(TAXONOMY_LAYER, name, rows, level=1)

        for k in range(KMEANS_K):
            artifacts.members(KMEANS_LAYER, cluster_key("km", k), kmeans_groups[k])
        for c in tree.nodes:
            artifacts.members(HDBSCAN_LAYER, cluster_key("hdb", c), tree.members_of[c])

        rng = np.random.default_rng(SEED)
        for k in kmeans_text:
            artifacts.label_members(KMEANS_LABELS, f"kml-{k:06d}", kmeans_groups[k],
                                    rng=rng, sample=LABEL_SAMPLE)
        for c in hdbscan_text:
            artifacts.label_members(HDBSCAN_LABELS, f"hdbl-{c:06d}", tree.members_of[c],
                                    rng=rng, sample=LABEL_SAMPLE)

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
        "umap": (
            "reused from geometry.parquet"
            if args.umap == "reuse"
            else "computed here, " + ("seeded" if seeded else "parallel and unseeded")
        ),
        "umap_params": (
            None if args.umap == "reuse"
            else dict(UMAP_PARAMS, pca=PCA_DIM, seed=SEED if seeded else None,
                      variance_kept=round(variance_kept, 4))
        ),
        "kmeans": {"k": KMEANS_K, "labels": len(kmeans_text)},
        "hdbscan": {
            "min_cluster_size": int(mcs),
            "clusters_in_tree": len(tree.nodes),
            "selected_clusters": tree.selected,
            "max_depth": int(max(tree.depth_of.values())),
            "noise_share": round(tree.noise_share, 4),
            "stray_share_mean": round(float(stray.mean()), 4),
            "labels": len(hdbscan_text),
            "chains_collapsed": collapsed,
        },
        "access_terms": len(access_terms),
        "access_labels": sum(len(c) for c in access),
        "bounds": {
            "x": [float(xy[:, 0].min()), float(xy[:, 0].max())],
            "y": [float(xy[:, 1].min()), float(xy[:, 1].max())],
        },
        "artifact_rows": artifact_rows,
        "member_rows": member_rows,
        "seconds": dict(steps),
        "total_seconds": steps.total(),
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")

    print(f"\nwrote to {out}:")
    for f in sorted(out.iterdir()):
        if f.is_file():
            print(f"  {f.name:34} {f.stat().st_size / 1e6:8.2f} MB")
    print(f"\nnext:\n  cd {out}\n  tessera check\n  tessera build")
    print(f"\nor, for the named-topic layer first:\n"
          f"  python -m test_corpora.arxiv.toponymy --out {out}")


if __name__ == "__main__":
    sys.exit(main())
