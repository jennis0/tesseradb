# A guided walk through the SDK, as a Marimo notebook:
# `marimo edit clients/py/examples/notebook_marimo.py`. `notebook.ipynb` is the same walk for
# Jupyter. The sections are python-sdk.md §10.1 to §10.5 and §10.7.
#
# Marimo folds every synced trait of a widget into one `.value`, so a cell that reads a map
# re-runs at every settle. The reading cells are kept apart from the ones that build for that
# reason.
import marimo

__generated_with = "0.24.0"
app = marimo.App(width="medium")


@app.cell
def _():
    import json
    import math
    import os
    import pathlib

    import marimo as mo
    import pandas as pd
    import pyarrow as pa
    import pyarrow.compute as pc
    import pyarrow.parquet as pq

    import tesseradb as td

    # `data/notebook/`: 50,000 arXiv papers, three clusterings over them and the topics over two.
    DATA = pathlib.Path(os.environ.get("TESSERA_NOTEBOOK_DATA", "../../../data/notebook"))

    def counts(table):
        """A served table's `visible`, `matched` and `served`, which its schema metadata carries.

        `visible` is what the principal may see, `matched` what the filters kept and `served` what
        came back under the point budget. A served set is not the whole set.
        """
        return json.loads(table.schema.metadata[b"tessera.counts"])

    return DATA, counts, math, mo, os, pa, pathlib, pc, pd, pq, td


@app.cell
def _(mo):
    mo.md(
        """
        # A Tessera database in a notebook

        Six steps. A frame of points becomes a served map; the arXiv corpus becomes a database
        with terms and three clusterings; a week of new papers goes into the running database; a
        second clustering is published over rows it already holds; one set of points is mapped
        under two projections; and the whole thing is saved, reopened and handed to `tessera
        serve`.

        Every map here is computed inside the viewer's own mask. `viewer(terms)` is the map of
        another principal, not this one's map filtered down.
        """
    )
    return


@app.cell
def _(mo):
    mo.md(
        """
        ## 1. A frame, a cluster column, a label for each cluster

        The starting point is a DataFrame: coordinates, a column of cluster keys and a title to
        hover. `from_column=` mints one artifact per distinct key in the column, so the clustering
        needs no table of its own.

        The cluster keys here come from the corpus's k-means membership file, joined onto the
        points so the frame carries one column of keys.
        """
    )
    return


@app.cell
def _(DATA, pa, pd, pq):
    _points = pq.read_table(DATA / "points.parquet").to_pandas()
    _members = pq.read_table(DATA / "clusters-kmeans-members.parquet").to_pandas()

    frame = _points[["entity_id", "x", "y", "title"]].copy()
    frame["cluster"] = frame["entity_id"].map(_members.set_index("entity")["key"])
    # pandas gives a string column as Arrow `large_string`, and a `text` attribute is stored at
    # `string` width, so the cast is what lets the build read the column.
    frame = frame.astype({"title": pd.ArrowDtype(pa.string()), "cluster": pd.ArrowDtype(pa.string())})
    frame.head()
    return (frame,)


@app.cell
def _(DATA, mo, pa, pq):
    # One line of text per cluster, as the `(key, contents)` table a label set reads. `attached_key`
    # is the cluster the label hangs from: a label is served only where that cluster is served.
    _topics = pq.read_table(DATA / "topics-kmeans.parquet").to_pandas()
    _named = {row.attached_key: row.contents[0][0] for row in _topics.itertuples()}

    topic_text = pa.table(
        {
            "level": pa.array([0] * len(_named), pa.uint32()),
            "key": pa.array([f"{key}-label" for key in _named], pa.string()),
            "contents": pa.array([[[text]] for text in _named.values()],
                                 pa.list_(pa.list_(pa.string()))),
            "attached_layer": pa.array(["clusters"] * len(_named), pa.string()),
            "attached_key": pa.array(list(_named), pa.string()),
        }
    )
    mo.md(f"{topic_text.num_rows} labels, one per cluster: {list(_named.values())[:3]}")
    return (topic_text,)


@app.cell
def _(frame, td, topic_text):
    simple = td.create()  # a temporary directory, on /dev/shm where the platform has one
    simple.stage("points", frame, default=True)
    simple.stage("topics", topic_text)
    simple.declare_view("map", source="points")
    simple.declare_layer("clusters", kind="flat", from_column="cluster")
    simple.declare_labels("topics", of="clusters", source="topics")
    print(simple.commit())  # tessera check, tessera build, tessera serve
    return (simple,)


@app.cell
def _(mo):
    mo.md(
        """
        The report above is the build's own: what each declaration read, what the frame did to the
        coordinates, and the three addresses the server bound. No column was declared: `title` and
        `submitted_at` were inferred from the frame, and the report says what was inferred and how.

        **Try**: hover a point, then drag a box (shift-drag) or a lasso over one cluster and read
        the next cell.
        """
    )
    return


@app.cell
def _(mo, simple):
    simple_map = mo.ui.anywidget(simple.map(colour_by="cluster:clusters", height=520))
    simple_map
    return (simple_map,)


@app.cell
def _(simple_map):
    # Re-runs at every settle: `.value` is every synced trait at once.
    {
        key: simple_map.value.get(key)
        for key in ("bbox", "layers", "colour_by", "selected", "selected_artifact", "region")
    }
    return


@app.cell
def _(counts, simple):
    # The same numbers without a browser: `viewport()` goes through the viewer plane with a token.
    simple_counts = counts(simple.viewport())
    simple_counts
    return (simple_counts,)


@app.cell
def _(mo):
    mo.md(
        """
        ## 2. The corpus from files, with terms and three clusterings

        The same corpus as `data/notebook/schema.toml`, declared as calls. The files carry
        `entity_id`, so they are staged as paths and read where they lie rather than copied.

        `access="categories"` makes each paper's arXiv categories its access terms. A viewer holding
        `math.AG` sees the papers filed under `math.AG` and nothing else. Every count, every cluster
        and every topic line is computed inside that mask.
        """
    )
    return


@app.cell
def _(DATA, td):
    db = td.create()
    db.stage("points", str(DATA / "points.parquet"), default=True)
    for _name, _file in [
        ("archive", "archive"),
        ("primary_category", "primary_category"),
        ("kmeans", "clusters-kmeans"),
        ("kmeans_members", "clusters-kmeans-members"),
        ("kmeans_topics", "topics-kmeans"),
        ("kmeans_topic_members", "topics-kmeans-members"),
        ("hdbscan", "clusters-hdbscan"),
        ("hdbscan_members", "clusters-hdbscan-members"),
        ("hdbscan_topics", "topics-hdbscan"),
        ("hdbscan_topic_members", "topics-hdbscan-members"),
        ("taxonomy", "taxonomy-arxiv"),
        ("taxonomy_members", "taxonomy-arxiv-members"),
    ]:
        db.stage(_name, str(DATA / f"{_file}.parquet"))
    return (db,)


@app.cell
def _(db):
    db.declare_view("s0", source="points", access="categories", title="arXiv, 50,000 papers")
    db.declare_vocabulary("archive", source="archive", closed=True, width="u8",
                          title="arXiv archive")
    db.declare_vocabulary("primary_category", source="primary_category", closed=True, width="u16",
                          title="arXiv subject class")
    db.declare_attribute("archive", type="category", vocabulary="archive", render=True, index=True,
                         title="Archive")
    db.declare_attribute("primary_category", type="category", vocabulary="primary_category",
                         render=True, index=True, title="Primary category")
    db.declare_attribute("submitted_at", type="timestamp_us", render=True, title="Submitted")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("abstract", type="text", index=True)
    db.declare_attribute("arxiv_id", type="keyword", index=True, title="arXiv ID")

    # Three clusterings: flat, nested and tiered. Each states the two disclosure controls that have
    # no default: who may know the layer exists, and how much of a cluster a viewer must already
    # see before that cluster is served to them.
    db.declare_layer("clusters/kmeans", kind="flat", source="kmeans", members="kmeans_members",
                     require_member_visibility={"count": 50}, title="k-means clusters")
    db.declare_labels("topics/kmeans", of="clusters/kmeans", source="kmeans_topics",
                      members="kmeans_topic_members", content_requires="all",
                      title="k-means topics")
    db.declare_layer("clusters/hdbscan", kind="nested", source="hdbscan", members="hdbscan_members",
                     require_member_visibility={"fraction": 0.05}, title="HDBSCAN clusters")
    db.declare_labels("topics/hdbscan", of="clusters/hdbscan", source="hdbscan_topics",
                      members="hdbscan_topic_members", content_requires="all",
                      title="HDBSCAN topics")
    db.declare_layer("taxonomy/arxiv", kind="tiered", source="taxonomy", members="taxonomy_members",
                     levels=[(0, "archive"), (1, "subject class")],
                     require_member_visibility={"count": 1}, computed=("centroid", "box"),
                     title="arXiv classification")
    return


@app.cell
def _(db):
    # `check()` is a commit with nothing sent: the schemas each declaration reads and the
    # disclosure decision it makes, from Parquet headers alone. No rows are read, so a clean check
    # is not a clean build.
    print(db.check())
    return


@app.cell
def _(db):
    print(db.commit())
    return


@app.cell
def _(db):
    # The declaration the SDK wrote, which is `schema.toml` in the database's directory.
    print(db.declaration)
    return


@app.cell
def _(mo):
    mo.md(
        """
        Three maps follow: the database's own principal, who holds every term the SDK staged, and
        two arXiv categories. `math.AG` is 1,077 papers in one region of the projection; `cs.LG`
        with `stat.ML` is about four times that, somewhere else, and is drawn over the HDBSCAN
        clustering rather than all three layers.

        **Try**: read the cluster counts on the second and third maps. They are smaller than the
        first map's, and they are counted over each principal's own rows rather than taken from
        the first map's numbers.
        """
    )
    return


@app.cell
def _(db, mo):
    arxiv_map = mo.ui.anywidget(db.map(colour_by="cluster:clusters/kmeans", height=520))
    arxiv_map
    return (arxiv_map,)


@app.cell
def _(db, mo):
    mo.ui.anywidget(db.viewer(["math.AG"]).map(colour_by="cluster:clusters/kmeans", height=380))
    return


@app.cell
def _(db, mo):
    mo.ui.anywidget(
        db.viewer(["cs.LG", "stat.ML"]).map(layers=["clusters/hdbscan"], height=380)
    )
    return


@app.cell
def _(counts, db):
    # What each of the three was served, as numbers: the union, one term, two terms.
    whole_counts = counts(db.viewport())
    one_term_counts = counts(db.viewer(["math.AG"]).viewport())
    two_term_counts = counts(db.viewer(["cs.LG", "stat.ML"]).viewport())
    (whole_counts["visible"], one_term_counts["visible"], two_term_counts["visible"])
    return one_term_counts, two_term_counts, whole_counts


@app.cell
def _(mo):
    mo.md(
        """
        ## 3. A week of new papers, into the database that is already serving

        The same verbs. `stage` on a source the declaration knows is a delta: rows to add to what
        the source holds. The commit pages them through the control plane and waits for the
        publication that makes them visible, so the cell after it sees them.

        Sixty papers, one new cluster, one topic line over it and the generating set that line
        was written from. Sixty because `clusters/kmeans` requires 50 visible members, so a
        smaller cluster would exist for nobody.

        Not built yet: a label whose content is gated `all` fails containment for every principal
        when its generating set is rows that arrived by ingest (issue #150). The line below is
        published and addressable, and no principal is served its text. The cluster it hangs from
        is served, and so are the papers.
        """
    )
    return


@app.cell
def _(counts, db, pa):
    before_delta = counts(db.viewport())["visible"]

    _quantisation = db.meta()["views"][0]["quantisation"]
    _x = (_quantisation["x_min"] + _quantisation["x_max"]) / 2.0
    _y = (_quantisation["y_min"] + _quantisation["y_max"]) / 2.0
    new_ids = list(range(900_001, 900_061))

    new_papers = pa.table(
        {
            "entity_id": pa.array(new_ids, pa.uint64()),
            "x": pa.array([_x + i * 0.001 for i in range(len(new_ids))], pa.float64()),
            "y": pa.array([_y + i * 0.001 for i in range(len(new_ids))], pa.float64()),
            "categories": pa.array([["cs.LG"] for _ in new_ids], pa.list_(pa.string())),
            "arxiv_id": pa.array([f"2609.{i:05d}" for i in new_ids], pa.string()),
            "archive": pa.array(["cs"] * len(new_ids), pa.string()),
            "primary_category": pa.array(["cs.LG"] * len(new_ids), pa.string()),
            "submitted_at": pa.array([1_757_000_000_000_000 + i for i in new_ids],
                                     pa.timestamp("us")),
            "title": pa.array([f"Diffusion models for audio, part {i}" for i in new_ids],
                              pa.string()),
            "abstract": pa.array([f"An abstract about audio diffusion, {i}." for i in new_ids],
                                 pa.string()),
        }
    )
    (before_delta, new_papers.num_rows)
    return before_delta, new_ids, new_papers


@app.cell
def _(db, new_ids, new_papers, pa):
    db.stage("points", new_papers)
    db.stage(
        "kmeans",
        pa.table({"level": pa.array([0], pa.uint32()),
                  "key": pa.array(["km-audio"], pa.string())}),
    )
    db.stage(
        "kmeans_members",
        pa.table({"level": pa.array([0] * len(new_ids), pa.uint32()),
                  "key": pa.array(["km-audio"] * len(new_ids), pa.string()),
                  "entity": pa.array(new_ids, pa.uint64())}),
    )
    db.stage(
        "kmeans_topics",
        pa.table({"level": pa.array([0], pa.uint32()),
                  "key": pa.array(["km-audio-label"], pa.string()),
                  "contents": pa.array([[["Audio diffusion"]]], pa.list_(pa.list_(pa.string()))),
                  "attached_layer": pa.array(["clusters/kmeans"], pa.string()),
                  "attached_key": pa.array(["km-audio"], pa.string())}),
    )
    # A label's member table carries two grains: a null rank is the membership, and rank *k* is
    # the set content *k* was generated from.
    db.stage(
        "kmeans_topic_members",
        pa.table({"level": pa.array([0] * (2 * len(new_ids)), pa.uint32()),
                  "key": pa.array(["km-audio-label"] * (2 * len(new_ids)), pa.string()),
                  "rank": pa.array([None] * len(new_ids) + [0] * len(new_ids), pa.uint32()),
                  "entity": pa.array(new_ids + new_ids, pa.uint64())}),
    )
    # The plan: the pages this commit would send, in the order §6.2 fixes. Points come before the
    # artifacts that name them, and a clustering before its labels.
    print(db.check())
    return


@app.cell
def _(db):
    delta_report = db.commit()
    print(delta_report)
    return (delta_report,)


@app.cell
def _(counts, db):
    # The commit waited for the publication its flush armed, so this needs no wait of its own.
    after_delta = counts(db.viewport())["visible"]
    after_delta
    return (after_delta,)


@app.cell
def _(mo):
    mo.md(
        """
        ## 4. A second clustering over rows the database already holds

        `from_column=` reads its keys from the rows being ingested, and these rows arrived at the
        first commit. So a clustering over rows the database holds is declared over its own tables:
        an artifacts table of keys and a members table of `(key, entity)` pairs.

        This one splits the corpus by decade of submission, which every principal can see some of.

        **Try**: colour by decade, then open the k-means clustering beside it. The same points,
        cut two ways.
        """
    )
    return


@app.cell
def _(DATA, db, pa, pc, pq):
    _points = pq.read_table(DATA / "points.parquet", columns=["entity_id", "submitted_at"])
    _years = pc.year(_points.column("submitted_at")).to_pylist()
    _entities = _points.column("entity_id").to_pylist()
    _keys = ["era-1990s" if y < 2000 else "era-2000s" if y < 2010 else "era-2010s"
             for y in _years]

    db.declare_layer("clusters/era", kind="flat", source="era", members="era_members",
                     title="By decade")
    db.stage(
        "era",
        pa.table({"level": pa.array([0, 0, 0], pa.uint32()),
                  "key": pa.array(["era-1990s", "era-2000s", "era-2010s"], pa.string())}),
    )
    db.stage(
        "era_members",
        pa.table({"level": pa.array([0] * len(_entities), pa.uint32()),
                  "key": pa.array(_keys, pa.string()),
                  "entity": pa.array(_entities, pa.uint64())}),
    )
    era_report = db.commit()
    print(era_report)
    return (era_report,)


@app.cell
def _(db, era_report, mo):
    era_report  # the layer is declared and its three artifacts published before this map is drawn
    mo.ui.anywidget(db.map(colour_by="cluster:clusters/era", height=440))
    return


@app.cell
def _(mo):
    mo.md(
        """
        ## 5. One set of points, two projections

        A view is a frame and a projection. Two views over the same papers are two sources with the
        same identity column, each carrying the access column: a view's mask is read from its own
        points file, so a second view either carries that column or is refused.

        The second projection here is the first turned 30 degrees about its centre. It is cheap and
        deterministic, so the two maps are recognisably the same corpus in two arrangements. A
        real second view is a second embedding.
        """
    )
    return


@app.cell
def _(DATA, math, pa, pc, pq, td):
    _points = pq.read_table(DATA / "points.parquet",
                            columns=["entity_id", "x", "y", "categories"])
    _members = pq.read_table(DATA / "clusters-kmeans-members.parquet", columns=["key", "entity"])
    _cluster_of = dict(zip(_members.column("entity").to_pylist(),
                           _members.column("key").to_pylist()))
    _keys = pa.array([_cluster_of.get(e) for e in _points.column("entity_id").to_pylist()],
                     pa.string())

    _angle = math.radians(30)
    _x, _y = _points.column("x"), _points.column("y")
    _cx, _cy = pc.mean(_x).as_py(), pc.mean(_y).as_py()
    _dx, _dy = pc.subtract(_x, _cx), pc.subtract(_y, _cy)

    turned = td.create()
    turned.stage("points", _points.append_column("cluster", _keys), default=True)
    turned.stage(
        "points_rotated",
        pa.table(
            {
                "entity_id": _points.column("entity_id"),
                "x": pc.add(pc.add(pc.multiply(_dx, math.cos(_angle)),
                                   pc.multiply(_dy, -math.sin(_angle))), _cx),
                "y": pc.add(pc.add(pc.multiply(_dx, math.sin(_angle)),
                                   pc.multiply(_dy, math.cos(_angle))), _cy),
                "categories": _points.column("categories"),
            }
        ),
    )
    turned.declare_view("knn", source="points", access="categories", title="k-NN projection")
    turned.declare_view("rotated", source="points_rotated", access="categories",
                        title="the same points, turned 30 degrees")
    turned.declare_layer("clusters/kmeans", kind="flat", from_column="cluster",
                         views=["knn", "rotated"], title="k-means clusters")
    print(turned.commit())
    return (turned,)


@app.cell
def _(mo, turned):
    mo.ui.anywidget(turned.map(view="knn", colour_by="cluster:clusters/kmeans", height=380))
    return


@app.cell
def _(mo, turned):
    mo.ui.anywidget(turned.map(view="rotated", colour_by="cluster:clusters/kmeans", height=380))
    return


@app.cell
def _(counts, turned):
    # One layer across both views: the artifacts are laid out per view, over the same members.
    knn_counts = counts(turned.viewport(view="knn"))
    rotated_counts = counts(turned.viewport(view="rotated"))
    (knn_counts["visible"], rotated_counts["visible"])
    return knn_counts, rotated_counts


@app.cell
def _(mo):
    mo.md(
        """
        ## 6. Keep it, reopen it, serve it elsewhere

        A temporary database is removed at `close()`. `save(path)` copies it out; `open(path)`
        reads it back, serves it, and its next `commit()` ingests, the bundle being there.

        The directory is the whole database: the declaration, the sources, the bundle and the
        deployment file. The same directory on another machine is

        ```
        tessera serve --deployment <path>/tessera.toml
        ```
        """
    )
    return


@app.cell
def _(counts, db, os, pathlib, td):
    saved_at = pathlib.Path(os.environ.get("TESSERA_DEMO_HOME", "~/tessera/arxiv")).expanduser()
    if saved_at.exists() and any(saved_at.iterdir()):
        print(f"{saved_at} already holds a database; opening that rather than saving over it")
    else:
        db.save(saved_at)
        print(f"saved to {saved_at}")

    reopened = td.open(saved_at)
    reopened_counts = counts(reopened.viewport())
    reopened_counts
    return reopened, reopened_counts, saved_at


@app.cell
def _(mo):
    mo.md(
        """
        A deployment somebody else runs is the same widget and the same read verbs, against a
        token that deployment issued you. There is no `viewer(terms)` there: minting another
        principal needs the session credential.

        ```python
        v = td.connect("https://tessera.example/viewer", token=my_token)
        v.map(colour_by="cluster:clusters/kmeans")
        ```

        The page there calls the viewer plane from this page's origin, so that origin must be in
        the deployment's CORS list. A database made here needs no list: its three planes are on
        loopback, and `serve.cors_loopback` admits a page served from a loopback address.

        Four servers are running by now, one per database. `close()` stops one and removes a
        temporary directory; quitting the kernel stops them all.
        """
    )
    return


if __name__ == "__main__":
    app.run()
