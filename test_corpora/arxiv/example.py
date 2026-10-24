"""The arXiv example dataset in `examples/data/arxiv/`: 50,000 papers cut from the notebook corpus.

It is small enough to keep in git and is the shape `data/notebook-2m4-live/` has, so
`schema.toml` beside it declares the same layers. Four things differ from a plain sample.

**No abstracts.** arXiv's metadata is CC0, but an abstract may be its authors' copyright, so the
points file keeps the title and drops the abstract.

**The k-means and HDBSCAN labels are made again, over the sample.** The source's labels were made
from titles over the whole corpus, and each recorded 200 papers drawn from the whole corpus, few of
which are in the sample. Here `prepare.py`'s `Labeller` ranks words over the titles of every
sampled paper in each cluster. The label's member file then holds a seeded draw of up to 200 of
the cluster's sampled papers, and a third of those for the fallback text: those are the papers a
reader must be able to see before the label is served.

**Every Toponymy exemplar is in the sample.** A Toponymy name was written by a language model from
six exemplar papers' titles, and a reader is shown it only where they can see all six. The
exemplars are taken first and the rest of the sample is drawn uniformly, so every name ships with
its whole generating set. That puts 4,284 papers near cluster centres into the sample on top of
the uniform draw.

**A second layout of the same papers**, `points-umap200.parquet`, for a view of its own. It is
laid out from the papers' BGE embeddings (`data/arxiv_papers_embeds.parquet`) by PCA to 64
components and then CPU `umap-learn`, seeded, with 200 neighbours and a minimum distance of 0.9.
The source layout used 15 and 0.1, so this one keeps subjects in broad contiguous regions where the
other breaks them into many small islands. It carries each paper's categories, which a view reads
its access terms from. It needs `umap-learn` and about five minutes, most of it streaming the
4.7 GB embedding file.

Every member file is restricted to the sampled papers, and a cluster left with no member is dropped
with any label attached to it. Positions are stored as `float32`.

```bash
python -m test_corpora.arxiv.example --source data/notebook-2m4-live --out examples/data/arxiv \
    --papers 50000 --seed 0
```
"""

from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from .prepare import FALLBACK, LABEL_SAMPLE, Labeller
from .sources import DATA, load_embeddings, rows_for_ids

#: The second layout's projection, chosen to differ from the source layout's 15 and 0.1.
SECOND_LAYOUT = dict(pca=64, n_neighbors=200, min_dist=0.9)

POINT_COLUMNS = [
    "entity_id", "x", "y", "categories", "arxiv_id", "archive", "primary_category",
    "submitted_at", "title",
]
#: The clusterings kept, by file stem. Their artifact files carry no text, so they are filtered.
CLUSTERINGS = ["clusters-kmeans", "clusters-hdbscan", "clusters-toponymy", "taxonomy-arxiv"]
#: The labels made again from titles, and the clustering each names.
RELABELLED = {"topics-kmeans": "clusters-kmeans", "topics-hdbscan": "clusters-hdbscan"}
#: The label kept as written, every one of whose generating papers is in the sample.
KEPT_LABELS = {"topics-toponymy": "clusters-toponymy"}
VOCABULARIES = ["archive.parquet", "primary_category.parquet"]


def write(table: pa.Table, path: Path) -> None:
    pq.write_table(table, path, compression="zstd", compression_level=19)


def sampled_ids(source: Path, papers: int, seed: int) -> tuple[pa.Array, int]:
    """Every Toponymy exemplar, then a uniform draw from the other papers up to `papers`."""
    ids = pq.read_table(source / "points.parquet", columns=["entity_id"])["entity_id"].to_numpy()
    exemplars = np.unique(
        pq.read_table(source / "topics-toponymy-members.parquet", columns=["entity"])["entity"]
        .to_numpy()
    )
    rest = np.setdiff1d(ids, exemplars, assume_unique=True)
    if papers < len(exemplars):
        raise SystemExit(f"--papers {papers} is fewer than the {len(exemplars):,} exemplars; "
                         f"ask for at least that many")
    drawn = np.random.default_rng(seed).choice(rest, size=papers - len(exemplars), replace=False)
    return pa.array(np.sort(np.concatenate([exemplars, drawn])), pa.uint64()), len(exemplars)


def members_of(source: Path, stem: str, keep: pa.Array) -> pa.Table:
    table = pq.read_table(source / f"{stem}-members.parquet")
    return table.filter(pc.is_in(table["entity"], value_set=keep))


def held_artifacts(source: Path, stem: str, members: pa.Table) -> pa.Table:
    """The artifacts of `stem` with at least one member left; a parent that was dropped is
    cleared on its children, which the sample cannot otherwise leave."""
    table = pq.read_table(source / f"{stem}.parquet")
    held = pc.unique(members["key"])
    kept = table.filter(pc.is_in(table["key"], value_set=held))
    orphaned = pc.and_(pc.is_valid(kept["parent"]),
                       pc.invert(pc.is_in(kept["parent"], value_set=kept["key"])))
    if pc.any(orphaned).as_py():
        parent = pc.if_else(orphaned, pa.scalar(None, pa.string()), kept["parent"])
        kept = kept.set_column(kept.schema.get_field_index("parent"), "parent", parent)
    print(f"  {stem}: {kept.num_rows:,} of {table.num_rows:,} clusters hold a sampled paper; "
          f"{pc.sum(orphaned).as_py() or 0} lost their parent")
    return kept


def relabel(source: Path, stem: str, clusters: pa.Table, members: pa.Table,
            labeller: Labeller, row_of: dict, rng) -> tuple[pa.Table, pa.Table]:
    """A label per cluster the source labelled, its words ranked over the titles of every sampled
    paper in the cluster.

    The contents are ranked as `prepare.py` ranks them: the description first and the fallback
    second. The member rows are a seeded draw of up to `LABEL_SAMPLE` of the cluster's papers for
    the description and a third of those for the fallback, and gate who may read each.
    """
    old = pq.read_table(source / f"{stem}.parquet")
    old = old.filter(pc.is_in(old["attached_key"], value_set=clusters["key"]))
    by_key = members.group_by("key").aggregate([("entity", "list")])
    entities_of = dict(zip(by_key["key"].to_pylist(), by_key["entity_list"].to_pylist()))
    attached = old["attached_key"].to_pylist()
    groups = {key: [row_of[e] for e in entities_of[key]] for key in attached}
    text = labeller.label(groups)

    artifacts, rows = [], []
    for record in old.to_pylist():
        cluster = record["attached_key"]
        described = text.get(cluster)
        contents = [[described], [FALLBACK]] if described else [[FALLBACK]]
        # Sorted, because a grouped list's order varies from run to run and the draw follows it.
        pool = np.sort(np.asarray(entities_of[cluster], dtype=np.uint64))
        pick = pool if len(pool) <= LABEL_SAMPLE else rng.choice(pool, LABEL_SAMPLE, replace=False)
        # The label's own membership is the draw, and each rank's set is a third of the one
        # above it, so the fallback is shown to a narrower reader than the description.
        sets = [(None, pick)] + [(rank, pick[: max(1, len(pick) // 3**rank)])
                                 for rank in range(len(contents))]
        for rank, chosen in sets:
            rows += [{"level": record["level"], "key": record["key"], "rank": rank,
                      "entity": int(e)} for e in chosen]
        artifacts.append({**record, "contents": contents})
    described = sum(1 for r in artifacts if len(r["contents"]) == 2)
    print(f"  {stem}: {len(artifacts)} labels made from titles, {described} with a description")
    return (pa.Table.from_pylist(artifacts, schema=old.schema),
            pa.Table.from_pylist(rows, schema=pq.read_schema(source / f"{stem}-members.parquet")))


def second_layout(points: pa.Table, data: Path, seed: int) -> pa.Table:
    """The sampled papers laid out again from their embeddings, with their categories."""
    from sklearn.decomposition import PCA
    from umap import UMAP

    corpus = pq.read_table(data / "corpus.parquet", columns=["entity_id", "id"])
    take = rows_for_ids(corpus, points["arxiv_id"].to_pylist())
    embeddings = load_embeddings(corpus, take, data)
    reduced = PCA(SECOND_LAYOUT["pca"], random_state=seed).fit_transform(embeddings)
    xy = UMAP(n_neighbors=SECOND_LAYOUT["n_neighbors"], min_dist=SECOND_LAYOUT["min_dist"],
              random_state=seed).fit_transform(reduced)
    return pa.table({"entity_id": points["entity_id"],
                     "x": pa.array(xy[:, 0], pa.float32()), "y": pa.array(xy[:, 1], pa.float32()),
                     "categories": points["categories"]})


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--source", type=Path, default=DATA / "notebook-2m4-live")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--papers", type=int, default=50_000)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--data", type=Path, default=DATA,
                    help="the directory holding corpus.parquet and arxiv_papers_embeds.parquet")
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    rng = np.random.default_rng(args.seed)

    keep, exemplars = sampled_ids(args.source, args.papers, args.seed)
    print(f"{len(keep):,} papers: {exemplars:,} Toponymy exemplars and "
          f"{len(keep) - exemplars:,} drawn uniformly with seed {args.seed}")

    points = pq.read_table(args.source / "points.parquet", columns=POINT_COLUMNS,
                           filters=pc.is_in(pc.field("entity_id"), value_set=keep))
    points = points.sort_by("entity_id")
    for axis in ("x", "y"):
        points = points.set_column(points.schema.get_field_index(axis), axis,
                                   pc.cast(points[axis], pa.float32()))
    write(points, args.out / "points.parquet")
    row_of = {e: i for i, e in enumerate(points["entity_id"].to_pylist())}

    clusters = {}
    for stem in CLUSTERINGS:
        members = members_of(args.source, stem, keep)
        clusters[stem] = held_artifacts(args.source, stem, members)
        write(clusters[stem], args.out / f"{stem}.parquet")
        write(members, args.out / f"{stem}-members.parquet")
        clusters[stem + "-members"] = members

    labeller = Labeller(points["title"].to_pylist())
    for stem, clustering in RELABELLED.items():
        artifacts, members = relabel(args.source, stem, clusters[clustering],
                                     clusters[clustering + "-members"], labeller, row_of, rng)
        write(artifacts, args.out / f"{stem}.parquet")
        write(members, args.out / f"{stem}-members.parquet")

    for stem, clustering in KEPT_LABELS.items():
        members = pq.read_table(args.source / f"{stem}-members.parquet")
        missing = pc.sum(pc.invert(pc.is_in(members["entity"], value_set=keep))).as_py()
        assert missing == 0, f"{missing} of {stem}'s generating papers are not in the sample"
        labels = pq.read_table(args.source / f"{stem}.parquet")
        assert pc.all(pc.is_in(labels["attached_key"], value_set=clusters[clustering]["key"])).as_py()
        write(labels, args.out / f"{stem}.parquet")
        write(members, args.out / f"{stem}-members.parquet")
        print(f"  {stem}: {labels.num_rows} names kept, every generating paper sampled")

    for name in VOCABULARIES:
        shutil.copyfile(args.source / name, args.out / name)

    write(second_layout(points, args.data, args.seed), args.out / "points-umap200.parquet")

    rows = {p.name: pq.ParquetFile(p).metadata.num_rows for p in sorted(args.out.glob("*.parquet"))}
    manifest = {
        "cut_from": args.source.name,
        "papers": len(keep),
        "toponymy_exemplars": exemplars,
        "seed": args.seed,
        "second_layout": {"file": "points-umap200.parquet", "embeddings": "BGE-large-en-v1.5",
                          **SECOND_LAYOUT},
        "rows": rows,
    }
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    total = sum(p.stat().st_size for p in args.out.glob("*.parquet"))
    for name, n in rows.items():
        print(f"  {name:<36} {n:>9,} rows  {(args.out / name).stat().st_size / 1e6:6.2f} MB")
    print(f"{total / 1e6:.2f} MB of Parquet in {args.out}")


if __name__ == "__main__":
    main()
