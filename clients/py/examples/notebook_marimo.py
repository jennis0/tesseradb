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
    import pyarrow as pa
    import pyarrow.compute as pc
    import pyarrow.parquet as pq

    import tesseradb as td

    def corpus_directory():
        """`data/notebook/`: 50,000 arXiv papers, three clusterings and the topics over two.

        Neither front end promises a working directory, so the corpus is looked for above this
        file and above the working directory, and `TESSERA_NOTEBOOK_DATA` names it anywhere else.
        """
        named = os.environ.get("TESSERA_NOTEBOOK_DATA")
        if named:
            return pathlib.Path(named).expanduser()
        starts = [pathlib.Path.cwd().resolve()]
        here = globals().get("__file__")
        if here:
            starts.append(pathlib.Path(here).resolve().parent)
        for start in starts:
            for directory in [start, *start.parents]:
                if (directory / "data" / "notebook" / "schema.toml").exists():
                    return directory / "data" / "notebook"
        raise FileNotFoundError(
            "data/notebook/ is above neither this file nor the working directory. "
            "Set TESSERA_NOTEBOOK_DATA to the corpus directory"
        )

    DATA = corpus_directory()

    def counts(table):
        """A served table's `visible`, `matched` and `served`, which its schema metadata carries.

        `visible` is what the principal may see, `matched` what the filters kept and `served` what
        came back under the point budget. A served set is not the whole set.
        """
        return json.loads(table.schema.metadata[b"tessera.counts"])

    return DATA, counts, math, mo, os, pa, pathlib, pc, pq, td


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

        Three verbs carry it, and each does one thing. `declare_*` says what exists and takes no
        data. `insert(target, table, **columns)` hands a table to a declared thing and names every
        column it reads. `commit()` sends what was inserted and forgets it, so adding more data is
        the same `insert` calls again.

        Every map here is computed inside the viewer's own mask. `viewer(terms)` is another
        principal's map, computed inside that principal's mask.
        """
    )
    return


@app.cell
def _(mo):
    mo.md(
        """
        ## 1. A frame, a cluster column, a label for each cluster

        The starting point is a DataFrame: coordinates, a column of cluster keys and a title to
        hover. `declare_columns` declares every column of it that is not skipped, from its dtype;
        `insert` hands the frame to the view and names the columns the view reads, and the title
        is filled by name because this is the allocation view's own frame.

        A layer given a table with an id column and a key column mints one artifact per distinct
        key and joins the rows, so the clustering needs no table of its own.

        The cluster keys here come from the corpus's k-means membership file, joined onto the
        points so the frame carries one column of keys.

        Each cluster has a line of text, inserted as a mapping from cluster key to text. A label
        with no members of its own is the label of its cluster (decision 0145): it is drawn where
        the cluster is drawn, counted over the cluster's members and served to whoever is served
        the cluster, so the mapping is the whole of it.
        """
    )
    return


@app.cell
def _(DATA, pq):
    _points = pq.read_table(DATA / "points.parquet").to_pandas()
    _members = pq.read_table(DATA / "clusters-kmeans-members.parquet").to_pandas()

    frame = _points[["entity_id", "x", "y", "title"]].copy()
    frame["cluster"] = frame["entity_id"].map(_members.set_index("entity")["key"])
    frame.head()
    return (frame,)


@app.cell
def _(DATA, mo, pq):
    # One line of text per cluster, keyed by the cluster it was written about.
    _topics = pq.read_table(DATA / "topics-kmeans.parquet").to_pandas()
    topic_names = {row.attached_key: row.contents[0][0] for row in _topics.itertuples()}
    mo.md(f"{len(topic_names)} labels, one per cluster: {list(topic_names.values())[:3]}")
    return (topic_names,)


@app.cell
def _(frame, td, topic_names):
    simple = td.create()  # a temporary directory, on /dev/shm where the platform has one
    simple.declare_view("map")
    simple.declare_columns(frame, skip=["entity_id", "x", "y", "cluster"], index=["title"])
    simple.declare_layer("clusters", kind="flat")
    simple.declare_labels("topics", of="clusters")

    simple.insert("map", frame, id="entity_id", x="x", y="y")  # title is read by name
    simple.insert("clusters", frame, id="entity_id", key="cluster")
    simple.insert("topics", topic_names)  # {cluster_key: text}
    return (simple,)


@app.cell
def _(simple):
    print(simple.commit())  # tessera check, tessera build, tessera serve
    return


@app.cell
def _(mo):
    mo.md(
        """
        Each call above printed what it did: `declare_columns` the table it declared, and each
        `insert` the columns it read and the columns it ignored. `cluster` is ignored by the
        view's insert and read by the layer's, and neither call guessed a name.

        The report is the build's own: what each declaration read, what the frame did to the
        coordinates, and the three addresses the server bound.

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

        The same corpus as `data/notebook/schema.toml`, declared as calls. A path is accepted
        wherever a table is and is read where it lies rather than copied.

        `access="categories"` on the view's insert makes each paper's arXiv categories its access
        terms. A viewer holding `astro-ph` sees the papers filed under `astro-ph` and nothing
        else. Every count, every cluster and every topic line is computed inside that mask.

        A layer takes two tables under their own keywords, `artifacts=` and `members=`, because
        both carry `key` and `level`, so each insert names its own columns.
        """
    )
    return


@app.cell
def _(td):
    db = td.create()
    db.declare_view("s0", title="arXiv, 50,000 papers")
    db.declare_vocabulary("archive", closed=True, width="u8", title="arXiv archive")
    db.declare_vocabulary("primary_category", closed=True, width="u16",
                          title="arXiv subject class")
    db.declare_attribute("archive", type="category", vocabulary="archive", render=True, index=True,
                         title="Archive")
    db.declare_attribute("primary_category", type="category", vocabulary="primary_category",
                         render=True, index=True, title="Primary category")
    db.declare_attribute("submitted_at", type="timestamp_us", render=True, title="Submitted")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("abstract", type="text", index=True)
    db.declare_attribute("arxiv_id", type="keyword", index=True, title="arXiv ID")

    # Three clusterings: flat, nested and tiered. Each states its `require_member_visibility`:
    # how much of a cluster a viewer must already see before that cluster is served to them, as a
    # floor on the count or a share of the cluster's own size.
    db.declare_layer("clusters/kmeans", kind="flat", value_set="open",  # §3 mints into it
                     require_member_visibility={"count": 50}, title="k-means clusters")
    db.declare_labels("topics/kmeans", of="clusters/kmeans", content_requires="all",
                      title="k-means topics")
    db.declare_layer("clusters/hdbscan", kind="nested",
                     require_member_visibility={"fraction": 0.05}, title="HDBSCAN clusters")
    db.declare_labels("topics/hdbscan", of="clusters/hdbscan", content_requires="all",
                      title="HDBSCAN topics")
    db.declare_layer("taxonomy/arxiv", kind="tiered",
                     levels=[(0, "archive"), (1, "subject class")],
                     require_member_visibility={"count": 1}, computed=("centroid", "box"),
                     title="arXiv classification")
    return (db,)


@app.cell
def _(DATA, db):
    db.insert("archive", str(DATA / "archive.parquet"), key="key", title="title", code="code")
    db.insert("primary_category", str(DATA / "primary_category.parquet"), key="key",
              title="title", code="code")
    # The six attribute columns are read by name from the frame inserted into the allocation view.
    db.insert("s0", str(DATA / "points.parquet"), id="entity_id", x="x", y="y",
              access="categories")
    for _layer, _name in [("clusters/kmeans", "clusters-kmeans"),
                          ("clusters/hdbscan", "clusters-hdbscan"),
                          ("taxonomy/arxiv", "taxonomy-arxiv")]:
        # Every column these tables carry is named, canonical or not: the build reads a
        # canonical column under its own name whatever the call says, so one passed over is
        # refused rather than read silently.
        db.insert(_layer, artifacts=str(DATA / f"{_name}.parquet"), key="key", level="level",
                  parent="parent", contents="contents", attached_layer="attached_layer",
                  attached_key="attached_key")
        db.insert(_layer, members=str(DATA / f"{_name}-members.parquet"), id="entity", key="key",
                  level="level", rank="rank")
    for _labels, _name in [("topics/kmeans", "topics-kmeans"),
                           ("topics/hdbscan", "topics-hdbscan")]:
        db.insert(_labels, str(DATA / f"{_name}.parquet"), key="key", level="level",
                  contents="contents", parent="parent", attached_layer="attached_layer",
                  attached_key="attached_key")
        db.insert(_labels, members=str(DATA / f"{_name}-members.parquet"), id="entity", key="key",
                  level="level", rank="rank")
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
    # The declaration the SDK wrote, which is `schema.toml` in the database's directory. Every
    # block names the source and the column names its inserts gave it.
    print(db.declaration)
    return


@app.cell
def _(mo):
    mo.md(
        """
        Three maps follow: the database's own principal, who holds every term the SDK inserted,
        and two arXiv categories. `astro-ph` is 2,105 papers in one region of the projection, and
        14 of the 64 k-means clusters clear its member requirement. `cs.LG` with `stat.ML` is
        about twice as many papers, somewhere else, drawn over the HDBSCAN clustering rather than
        all three layers.

        Neither category principal is served a topic line, where the first map's principal is
        served every one. A topic line is generated from the papers of its cluster, those papers
        span categories, and the line is read only by a viewer who may read every one of them. So
        the cluster is drawn and keyed, and the sentence written about it is not served.

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
    mo.ui.anywidget(db.viewer(["astro-ph"]).map(colour_by="cluster:clusters/kmeans", height=380))
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
    one_term_counts = counts(db.viewer(["astro-ph"]).viewport())
    two_term_counts = counts(db.viewer(["cs.LG", "stat.ML"]).viewport())
    (whole_counts["visible"], one_term_counts["visible"], two_term_counts["visible"])
    return one_term_counts, two_term_counts, whole_counts


@app.cell
def _(mo):
    mo.md(
        """
        ## 3. A week of new papers, into the database that is already serving

        The same verbs: adding more data is the same `insert` calls again. The commit pages them
        through the control plane and waits for the publication that makes them visible, so the
        cell after it sees them.

        Sixty papers, one new cluster, one topic line over it and the generating set that line
        was written from. Sixty because `clusters/kmeans` requires 50 visible members, so a
        smaller cluster would exist for nobody. They land in a patch a few hundred units across
        at the middle of the frame, about a thousandth of its width, so the map below needs
        zooming in to see them apart.

        The new cluster is one key column beside the papers: `clusters/kmeans` was declared
        `value_set = "open"` in section 2, so a key its artifacts do not declare mints the cluster
        it names, with the batch's rows as its first members. The topic line over it attaches to
        that key in the same commit, so the commit flushes between the two: an artifact is
        resolvable from its publication, and this one is published by the values page.
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
    # An eight-wide grid at 80 units a step: a patch about 560 units across, rather than the
    # sixty points on top of each other that a 0.001 step would give.
    _at = [(_x + 80.0 * (i % 8) - 280.0, _y + 80.0 * (i // 8) - 260.0)
           for i in range(len(new_ids))]

    new_papers = pa.table(
        {
            "entity_id": pa.array(new_ids, pa.uint64()),
            "x": pa.array([x for x, _ in _at], pa.float64()),
            "y": pa.array([y for _, y in _at], pa.float64()),
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
            "cluster": pa.array(["km-audio"] * len(new_ids), pa.string()),
        }
    )
    (before_delta, new_papers.num_rows)
    return before_delta, new_ids, new_papers


@app.cell
def _(db, new_ids, new_papers, pa):
    db.insert("s0", new_papers, id="entity_id", x="x", y="y", access="categories")
    db.insert("clusters/kmeans", new_papers, id="entity_id", key="cluster")
    db.insert(
        "topics/kmeans",
        pa.table({"level": pa.array([0], pa.uint32()),
                  "key": pa.array(["km-audio-label"], pa.string()),
                  "contents": pa.array([[["Audio diffusion"]]], pa.list_(pa.list_(pa.string()))),
                  "attached_layer": pa.array(["clusters/kmeans"], pa.string()),
                  "attached_key": pa.array(["km-audio"], pa.string())}),
        key="key",
        level="level",
        contents="contents",
        attached_layer="attached_layer",
        attached_key="attached_key",
    )
    # A label's member table carries two grains: a null rank is the membership, and rank *k* is
    # the set content *k* was generated from.
    db.insert(
        "topics/kmeans",
        members=pa.table({"level": pa.array([0] * (2 * len(new_ids)), pa.uint32()),
                          "key": pa.array(["km-audio-label"] * (2 * len(new_ids)), pa.string()),
                          "rank": pa.array([None] * len(new_ids) + [0] * len(new_ids),
                                           pa.uint32()),
                          "entity": pa.array(new_ids + new_ids, pa.uint64())}),
        id="entity",
        key="key",
        level="level",
        rank="rank",
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
        The new papers carry `cs.LG`, so the map below is that principal's: the cluster `km-audio`
        is drawn among the clusters they already held.

        Not built yet: a label whose content is gated `all` fails containment for every principal
        when its generating set is rows that arrived by ingest (issue #150). The line written
        about this cluster is published and addressable, and no principal is served its text. The
        cluster is drawn, and so are the papers.
        """
    )
    return


@app.cell
def _(after_delta, db, mo):
    _ = after_delta  # the delta is visible before this map asks for it
    mo.ui.anywidget(db.viewer(["cs.LG"]).map(colour_by="cluster:clusters/kmeans", height=440))
    return


@app.cell
def _(mo):
    mo.md(
        """
        ## 4. A second clustering over rows the database already holds

        §10.4 is one insert: a table with an id column and a key column, over rows the database
        holds. This layer declares no artifacts table, so its value set is `open`, and the values
        route mints one artifact per key the column names and joins the rows that name it, which
        is what the build and the ingest route do with the same column.

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
    _by_decade = pa.table({"entity_id": pa.array(_entities, pa.uint64()),
                           "era": pa.array(_keys, pa.string())})

    db.declare_layer("clusters/era", kind="flat", title="By decade")
    db.insert("clusters/era", _by_decade, id="entity_id", key="era")
    era_report = db.commit()
    print(era_report)
    return (era_report,)


@app.cell
def _(db, era_report, mo):
    _ = era_report  # the layer and its three artifacts exist before this map asks for them
    mo.ui.anywidget(db.map(colour_by="cluster:clusters/era", height=440))
    return


@app.cell
def _(mo):
    mo.md(
        """
        ## 5. One set of points, two projections

        A view is a frame and a projection. Two views over the same papers are two inserts with
        the same id column, a second pair of coordinates, and the same access column: a view's
        mask is read from the frame inserted into it, so a second view either names that column
        or is refused.

        The second projection here is the first turned 30 degrees about its centre. It is cheap and
        deterministic, so the two maps are recognisably the same corpus in two arrangements. A
        real second view is a second embedding.

        The clustering is one key column over the first view's frame, which is the build's own
        route: a layer drawn on both views, over one set of members.
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
    _knn = _points.append_column("cluster", _keys)

    _angle = math.radians(30)
    _x, _y = _points.column("x"), _points.column("y")
    _cx, _cy = pc.mean(_x).as_py(), pc.mean(_y).as_py()
    _dx, _dy = pc.subtract(_x, _cx), pc.subtract(_y, _cy)
    _rotated = pa.table(
        {
            "entity_id": _points.column("entity_id"),
            "x": pc.add(pc.add(pc.multiply(_dx, math.cos(_angle)),
                               pc.multiply(_dy, -math.sin(_angle))), _cx),
            "y": pc.add(pc.add(pc.multiply(_dx, math.sin(_angle)),
                               pc.multiply(_dy, math.cos(_angle))), _cy),
            "categories": _points.column("categories"),
        }
    )

    turned = td.create()
    turned.declare_view("knn", title="k-NN projection")
    turned.declare_view("rotated", title="the same points, turned 30 degrees")
    turned.declare_layer("clusters/kmeans", kind="flat", views=["knn", "rotated"],
                         title="k-means clusters")
    turned.insert("knn", _knn, id="entity_id", x="x", y="y", access="categories")
    turned.insert("rotated", _rotated, id="entity_id", x="x", y="y", access="categories")
    turned.insert("clusters/kmeans", _knn, id="entity_id", key="cluster")
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

        The cell below saves to `~/tessera/arxiv`, in your own home directory, and opens that
        rather than saving over it where a database is already there. `TESSERA_DEMO_HOME` names
        somewhere else.
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
