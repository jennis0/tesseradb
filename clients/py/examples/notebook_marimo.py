# TesseraDB's Python client, as a marimo notebook: `marimo edit clients/py/examples/notebook_marimo.py`.
#
# `notebook.ipynb` beside it is generated from this file, and is not edited by hand:
#
#     marimo export ipynb clients/py/examples/notebook_marimo.py -o clients/py/examples/notebook.ipynb
#
# Marimo re-runs a cell that reads a map widget's `.value` whenever the map settles, so the cells
# that draw maps are kept apart from the cells that build.
import marimo

__generated_with = "0.24.2"
app = marimo.App(width="medium")


@app.cell
def _():
    import datetime
    import os
    import pathlib
    import tempfile

    import marimo as mo
    import pandas as pd
    import pyarrow as pa
    import pyarrow.compute as pc
    import pyarrow.parquet as pq
    from sklearn.cluster import KMeans

    import tesseradb as td

    return KMeans, datetime, mo, os, pa, pathlib, pc, pd, pq, td, tempfile


@app.cell
def _(mo):
    mo.md("""
    # TesseraDB in a notebook

    TesseraDB is an open-source engine for interactive maps of large datasets. You give it
    records with 2D coordinates, which can be places on Earth or positions in an embedding
    space, and it serves a map you can pan, filter, search and cluster.

    Each person viewing the map sees it computed from only the records they may see. That
    covers every count, density, sample, cluster and label, as well as which points are drawn.

    This notebook maps arXiv papers. You look at the map as two readers with different access,
    ask it questions, step through it a year at a time, add a week of papers while it is
    serving, and save it.

    Three calls do the work. `declare_*` says what the database holds: a view, a column, a
    clustering. `insert` gives it data and names each column it reads. `commit` sends
    everything inserted since the last commit.

    The notebook maps all 2.4 million papers, and the first build takes a few minutes. Set
    `SCALE` below to `"sample"` to use a random 200,000 of them instead, which builds in
    seconds.
    """)
    return


@app.cell
def _():
    SCALE = "whole"  # or "sample"
    return (SCALE,)


@app.cell
def _(SCALE, os, pathlib):
    def corpus_directory():
        """`data/<corpus>/` above this file or the working directory, or `TESSERA_NOTEBOOK_DATA`."""
        named = os.environ.get("TESSERA_NOTEBOOK_DATA")
        if named:
            return pathlib.Path(named).expanduser()
        corpus = {"whole": "notebook-2m4-live", "sample": "notebook-sample"}[SCALE]
        starts = [pathlib.Path.cwd().resolve()]
        here = globals().get("__file__")
        if here:
            starts.append(pathlib.Path(here).resolve().parent)
        for start in starts:
            for directory in [start, *start.parents]:
                if (directory / "data" / corpus / "schema.toml").exists():
                    return directory / "data" / corpus
        raise FileNotFoundError(
            f"data/{corpus}/ is above neither this file nor the working directory. "
            "Set TESSERA_NOTEBOOK_DATA to the corpus directory"
        )

    DATA = corpus_directory()
    return (DATA,)


@app.cell
def _(mo):
    mo.md("""
    ## 1. A map from a DataFrame

    Start with a DataFrame with one row per paper: an id, a position, a title and the cluster
    the paper belongs to. The positions are a 2D layout of each paper's text embedding, so
    papers on similar subjects sit close together.
    """)
    return


@app.cell
def _(DATA, pq):
    _points = pq.read_table(DATA / "points.parquet", columns=["entity_id", "x", "y", "title"])
    _members = pq.read_table(DATA / "clusters-kmeans-members.parquet").to_pandas()

    frame = _points.to_pandas()
    frame["cluster"] = frame["entity_id"].map(_members.set_index("entity")["key"])
    frame.head()
    return (frame,)


@app.cell
def _(DATA, pq):
    # One line of text per cluster, keyed by the cluster it describes.
    _topics = pq.read_table(DATA / "topics-kmeans.parquet").to_pandas()
    topic_names = {row.attached_key: row.contents[0][0] for row in _topics.itertuples()}
    return (topic_names,)


@app.cell
def _(mo):
    mo.md("""
    `td` is the `tesseradb` package, imported in the first cell. `td.create()` makes a new
    database in a temporary directory, and everything after it is a call on that database.

    The declarations say what the database will hold, before it holds anything:

    - `declare_view("map")` declares a **view**: a set of points with 2D positions, drawn as
      one map. A database can hold several views of the same records, as section 2 does.
    - `declare_columns(frame, ...)` declares a column for each column of the frame, typed from
      the frame. `skip` leaves out the columns that the view and the clustering use in their
      own way. `index=["title"]` makes titles searchable and filterable. A column that is not
      indexed is stored with each point and shown when you open it.
    - `declare_layer("kmeans", kind="flat")` declares a **layer**: a set of groups of points
      drawn over the map, here a clustering. `kind="flat"` means one level of clusters with no
      hierarchy. Section 2 has layers of the other kinds, whose clusters sit inside larger ones.
    - `declare_labels("names", of="kmeans")` declares a line of text for each cluster of the
      `kmeans` layer.

    The inserts then give it the data, and each names the frame's columns it uses:

    - `insert("map", frame, id="entity_id", x="x", y="y")` makes each row a point. `id` names
      the column that identifies each paper, and `x` and `y` the columns holding its position.
      The view also takes `title`, because a column of that name was declared.
    - `insert("kmeans", frame, id="entity_id", key="cluster")` puts each paper in a cluster.
      `key` names the column that says which cluster, and each distinct key becomes one
      cluster.
    - `insert("names", topic_names)` takes a dictionary from cluster key to text.

    Each insert prints the columns it read and the columns it ignored.

    `commit()` sends everything inserted. The first commit checks the declarations against the
    data, builds the database's files, starts a local server for the database and prints what it
    did.
    """)
    return


@app.cell
def _(frame, td, topic_names):
    simple = td.create()
    simple.declare_view("map")
    simple.declare_columns(frame, skip=["entity_id", "x", "y", "cluster"], index=["title"])
    simple.declare_layer("kmeans", kind="flat")
    simple.declare_labels("names", of="kmeans")

    simple.insert("map", frame, id="entity_id", x="x", y="y")
    simple.insert("kmeans", frame, id="entity_id", key="cluster")
    simple.insert("names", topic_names)
    print(simple.commit())
    return (simple,)


@app.cell
def _(mo):
    mo.md("""
    `map()` draws the database's map in the notebook, served by that local server.
    Two settings decide what the map shows. `colour_by` is what the points are coloured by: a
    column, or `cluster:` followed by a layer's name, so `"cluster:kmeans"` colours each point
    by its cluster in the `kmeans` layer. `layers` is the layers drawn over the points: here the
    `kmeans` clusters with their names. The two are independent, so a map can colour by one
    layer and draw another, or colour by a layer and draw nothing over it.
    Hover over a point to see its title, and zoom in to see more points.
    """)
    return


@app.cell
def _(mo, simple):
    mo.ui.anywidget(simple.map(colour_by="cluster:kmeans", layers=["kmeans"], height=520))
    return


@app.cell
def _(mo):
    mo.md("""
    `view()` asks the same server from Python. `view("map")` is the whole of the view as this
    reader sees it, and later sections narrow it with filters. `count()` is the number of
    papers in it. `sample()` is the points a map draws at one zoom level, which is a sample
    for display, so it holds fewer rows than there are papers.
    """)
    return


@app.cell
def _(simple):
    on_map = simple.view("map")
    first_count = on_map.count()
    {"papers": first_count, "points drawn at zoom 0": on_map.sample().num_rows}
    return (first_count,)


@app.cell
def _(mo):
    mo.md("""
    ## 2. The arXiv database

    The same papers, now as a database with more to it. There are two kinds of view: `papers`
    holds every paper, and the view group `years` holds one view per year of submission. Each
    year's view uses the same coordinates as `papers`, so a place on the map means the same
    field in every year.

    Each paper's arXiv categories are its access terms. A reader who holds `cs.LG` sees the
    papers filed under `cs.LG` and no others.

    There are three clusterings. **Topics** is a hierarchy of clusters in four levels, each named
    by a language model from the papers in it. **arXiv classification** is arXiv's own archives
    and subject classes. **k-means** is a flat clustering of the layout.

    The last seven days of submissions are held back from the build, so that section 5 can add
    them to the running database.
    """)
    return


@app.cell
def _(DATA, datetime, pa, pc, pq):
    points = pq.read_table(DATA / "points.parquet")
    points = points.append_column(
        "year", pc.cast(pc.year(points["submitted_at"]), pa.string())
    )

    _cutoff = pc.subtract(pc.max(points["submitted_at"]), datetime.timedelta(days=7))
    _recent = pc.greater(points["submitted_at"], _cutoff)
    week = points.filter(_recent)
    points = points.filter(pc.invert(_recent))

    def _split(name):
        table = pq.read_table(DATA / f"{name}-members.parquet")
        in_week = pc.is_in(table["entity"], week["entity_id"])
        return table.filter(pc.invert(in_week)), table.filter(in_week)

    members, week_members = {}, {}
    for _name in ["clusters-toponymy", "topics-toponymy", "taxonomy-arxiv", "clusters-kmeans"]:
        members[_name], week_members[_name] = _split(_name)

    # A topic name written only from papers in the held-back week has none of them to be written
    # from yet, so it is left out.
    _written = members["topics-toponymy"].filter(pc.is_valid(members["topics-toponymy"]["rank"]))
    named_topics = pq.read_table(DATA / "topics-toponymy.parquet")
    named_topics = named_topics.filter(pc.is_in(named_topics["key"], _written["key"]))
    # The corpus's files call the clustering `clusters/toponymy`, and this database `topics`.
    named_topics = named_topics.set_column(
        named_topics.schema.get_field_index("attached_layer"), "attached_layer",
        pa.array(["topics"] * named_topics.num_rows),
    )
    members["topics-toponymy"] = members["topics-toponymy"].filter(
        pc.is_in(members["topics-toponymy"]["key"], named_topics["key"])
    )

    years = sorted(set(points["year"].to_pylist()))
    (points.num_rows, week.num_rows, f"{years[0]}-{years[-1]}")
    return members, points, named_topics, week, week_members, years


@app.cell
def _(mo):
    mo.md("""
    This time each column is declared on its own, with more control over each.

    `declare_view_group("years")` declares a **view group**: a set of views that share their
    settings and differ by a key, here the year. Its views are listed in a roster when the data
    is inserted.

    A **vocabulary** is the set of values a category column takes, such as arXiv's archives.
    `closed=True` means no other values are accepted, and `width` is how many bytes each value's
    code takes.

    An **attribute** is a column, and its `type` decides how it is stored and searched. A
    `category` takes its values from a vocabulary. `text` is searched word by word. A `keyword`
    is matched exactly, as an arXiv ID is. `render=True` keeps the value with each point, so the
    map can colour and filter by it. `index=True` makes the column searchable or filterable. A
    column with neither is stored with the paper and shown when you open it.

    Each clustering is a layer. `views` says which views it is drawn on. `kind="tiered"` means
    fixed levels, from coarse to fine, which `levels` names. `computed` is what the server works
    out for each cluster for each reader, such as its centre and its bounding box.

    Two settings decide what a reader is shown. `require_member_visibility` shows a cluster to
    a reader only when they can see enough of its papers. `content_requires="all"` shows a
    topic's name only to a reader who can see every paper the name was written from.
    """)
    return


@app.cell
def _(SCALE, td):
    db = td.create()
    db.declare_view("papers", title="arXiv")
    db.declare_view_group("years", title="arXiv, one year at a time")

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

    # How many of a topic's papers a reader must see before the topic is shown to them.
    _floor = {"whole": 50, "sample": 5}[SCALE]
    db.declare_layer("topics", kind="tiered", views=["papers", "years"],
                     levels=[(0, "coarsest"), (1, "coarse"), (2, "fine"), (3, "finest")],
                     require_member_visibility={"count": _floor}, title="Topics")
    db.declare_labels("topic_names", of="topics", content_requires="all",
                      title="Topic names")
    db.declare_layer("arxiv", kind="tiered", views=["papers", "years"],
                     levels=[(0, "archive"), (1, "subject class")],
                     require_member_visibility={"count": 1}, computed=("centroid", "box"),
                     title="arXiv classification")
    db.declare_layer("kmeans", kind="flat", views=["papers"],
                     require_member_visibility={"count": 50}, title="k-means clusters")
    return (db,)


@app.cell
def _(mo):
    mo.md("""
    The inserts take tables or paths to Parquet files, and a path is read where it lies.

    `access="categories"` names the column holding each paper's access terms, a list of arXiv
    categories. The `years` group takes a roster of its views, one row per year, and then the
    papers, with `view="year"` naming the column that says which year's view each paper goes
    in.

    A clustering takes two tables. `artifacts=` holds the clusters, one row each, and
    `members=` says which papers are in which cluster. Every column the call uses is named on
    it, as before: `parent` is a cluster's parent in the hierarchy, `contents` is a label's
    text, `attached_layer`, `attached_level` and `attached_key` name the cluster a label
    belongs to, and `rank` in a members table marks the papers a topic name was written from.
    """)
    return


@app.cell
def _(DATA, db, members, pa, points, named_topics, years):
    db.insert("archive", str(DATA / "archive.parquet"), key="key", title="title", code="code")
    db.insert("primary_category", str(DATA / "primary_category.parquet"), key="key",
              title="title", code="code")

    db.insert("papers", points, id="entity_id", x="x", y="y", access="categories")
    db.insert("years", roster=pa.table({"year": years}), key="year")
    db.insert("years", points.select(["entity_id", "x", "y", "categories", "year"]),
              id="entity_id", x="x", y="y", access="categories", view="year")

    _columns = dict(key="key", level="level", parent="parent", contents="contents",
                    attached_layer="attached_layer", attached_level="attached_level",
                    attached_key="attached_key")
    for _layer, _name in [("topics", "clusters-toponymy"),
                          ("arxiv", "taxonomy-arxiv"),
                          ("kmeans", "clusters-kmeans")]:
        db.insert(_layer, artifacts=str(DATA / f"{_name}.parquet"), **_columns)
        db.insert(_layer, members=members[_name], id="entity", key="key", level="level",
                  rank="rank")
    db.insert("topic_names", named_topics, **_columns)
    db.insert("topic_names", members=members["topics-toponymy"], id="entity", key="key",
              level="level", rank="rank")
    return


@app.cell
def _(mo):
    mo.md("""
    The commit checks the declaration against the tables, builds the database and starts its
    server. At the whole scale this took about three minutes on a 12-core machine.
    """)
    return


@app.cell
def _(db):
    print(db.commit())
    return


@app.cell
def _(mo):
    mo.md("""
    The first map is the database's own view. Its reader holds every access term, so it sees
    every paper, every topic and every topic name.
    """)
    return


@app.cell
def _(db, mo):
    mo.ui.anywidget(db.map(view="papers", colour_by="cluster:topics", layers=["topics"], height=520))
    return


@app.cell
def _(mo):
    mo.md("""
    `viewer(terms)` is the map of a reader who holds only those terms. The two below are an
    astronomer, who holds every `astro-ph` category, and a machine-learning reader, who holds
    `cs.LG` and `stat.ML`.

    Compare them with the map above. Each shows its own part of the layout, each cluster count
    is the number of papers that reader can see, and a topic appears only where the reader
    can see enough of its papers. A topic's name appears only where the reader can see every
    paper it was written from, so a reader can see fewer names than the database's own map.
    """)
    return


@app.cell
def _(db):
    astro = db.viewer(["astro-ph", "astro-ph.CO", "astro-ph.EP", "astro-ph.GA", "astro-ph.HE",
                       "astro-ph.IM", "astro-ph.SR"])
    learning = db.viewer(["cs.LG", "stat.ML"])
    return astro, learning


@app.cell
def _(astro, mo):
    mo.ui.anywidget(astro.map(view="papers", colour_by="cluster:topics", layers=["topics"], height=380))
    return


@app.cell
def _(learning, mo):
    mo.ui.anywidget(learning.map(view="papers", colour_by="cluster:topics", layers=["topics"], height=380))
    return


@app.cell
def _(mo):
    mo.md("""
    The same comparison in numbers.
    """)
    return


@app.cell
def _(astro, db, learning):
    reader_counts = {
        "database": db.view("papers").count(),
        "astro-ph.*": astro.view("papers").count(),
        "cs.LG + stat.ML": learning.view("papers").count(),
    }
    reader_counts
    return (reader_counts,)


@app.cell
def _(mo):
    mo.md("""
    ## 3. Asking questions

    A filter is a dictionary. A condition names one column and one operator: `eq` or `in` for
    a category, `range` for a number or a date, `match` or `phrase` for text. `all_of`,
    `any_of` and `none_of` combine conditions. A highlight takes the same form and lights up
    the points that match it without hiding the others.

    `filter()` narrows a view to the papers that match, and filters added one after another
    must all match. A highlight changes only how a map draws its points, so the last count
    below uses both conditions as filters.
    """)
    return


@app.cell
def _(pd):
    def since(day):
        """A date as the microseconds `submitted_at` holds."""
        return int(pd.Timestamp(day).value // 1000)

    recent_cs = {"all_of": [
        {"archive": {"eq": "cs"}},
        {"submitted_at": {"range": {"gte": since("2020-01-01")}}},
    ]}
    black_holes = {"abstract": {"phrase": "black hole"}}
    transformers = {"title": {"match": "transformer"}}
    return black_holes, recent_cs, since, transformers


@app.cell
def _(black_holes, db, recent_cs, transformers):
    papers = db.view("papers")
    filter_counts = {
        "cs since 2020": papers.filter(recent_cs).count(),
        "'black hole' in the abstract": papers.filter(black_holes).count(),
        "cs since 2020, 'transformer' in the title":
            papers.filter(recent_cs).filter(transformers).count(),
    }
    filter_counts
    return filter_counts, papers


@app.cell
def _(mo):
    mo.md("""
    `map()` on a filtered view draws it with the filter applied. Change the filter from Python
    by setting the map's `filters`, or use the filter panel on the map.
    """)
    return


@app.cell
def _(mo, papers, recent_cs):
    mo.ui.anywidget(papers.filter(recent_cs).map(colour_by="cluster:topics", layers=["topics"],
                                                 height=440))
    return


@app.cell
def _(mo):
    mo.md("""
    `item()` returns one paper's record: its fields, the labels this reader holds, and the
    views it is in. The cell below uses `sample()` to take one paper with "black hole" in its
    abstract from the points the map would draw.
    """)
    return


@app.cell
def _(black_holes, db, papers):
    _drawn = papers.filter(black_holes).sample(k=16)
    db.item(_drawn["tessera_id"][0].as_py())
    return


@app.cell
def _(mo):
    mo.md("""
    `browse_artifacts()` lists a clustering's clusters with two counts for the reader who asks:
    `masked_count` is how many of the cluster's papers they can see, and `matched_count` how
    many of those pass the filter. The table below puts the two readers side by side over the
    coarsest level of the topics, counting papers submitted since 2020.

    The topic names in this table come from the corpus's own file, since the table is for you,
    the notebook's operator. The server gives a reader a topic's name only where they can see
    every paper it was written from.

    Not built yet: a request that returns every matching paper as a table. `sample()`
    returns a sample for drawing, so analysis here uses the counts.
    """)
    return


@app.cell
def _(DATA, astro, learning, pd, pq, since):
    _names = {row.attached_key: row.contents[0][0]
              for row in pq.read_table(DATA / "topics-toponymy.parquet").to_pandas().itertuples()}
    _since_2020 = {"submitted_at": {"range": {"gte": since("2020-01-01")}}}

    def per_topic(reader):
        page = reader.browse_artifacts("papers", "topics", level=0,
                                       filters=_since_2020)
        return pd.DataFrame(page["artifacts"]).set_index("key")[["masked_count", "matched_count"]]

    pd.concat({"astro-ph.*": per_topic(astro), "cs.LG + stat.ML": per_topic(learning)},
              axis=1).rename(index=_names).astype("Int64")
    return


@app.cell
def _(mo):
    mo.md("""
    ## 4. A year at a time

    Each year has its own view in the `years` group, named `years:<year>`. `map(view=...)`
    draws one of them, and setting the map's `view` from Python switches it to another. The
    slider below does that, and the map keeps its position as you move, because every year
    uses the same coordinates.
    """)
    return


@app.cell
def _(mo, years):
    year = mo.ui.slider(steps=[int(one) for one in years], value=int(years[-1]), label="Year",
                        show_value=True)
    year
    return (year,)


@app.cell
def _(db, mo, years):
    year_map = db.map(view=f"years:{years[-1]}", colour_by="cluster:topics", layers=["topics"],
                      height=440)
    mo.ui.anywidget(year_map)
    return (year_map,)


@app.cell
def _(year, year_map):
    year_map.view = f"years:{year.value}"
    return


@app.cell
def _(mo):
    mo.md("""
    A view of its own lets each year have its own clusters. The cell below runs scikit-learn's
    k-means on each year's positions, with about one cluster per 250 papers and at most 20,
    and adds the result to the running database as a new layer, `yearly`.

    `scope={"group": "years"}` puts the layer on every view of the `years` group, with separate
    clusters in each, and on no other view. The insert names the cluster column with `key`, as
    in section 1, and the year column with `view`. Each year numbers its clusters from `"00"`,
    and the same key in two years names two different clusters.

    The clusters are computed from each paper's position on the map. They group papers that
    sit together in that year, which is not the same as clustering the papers' text.
    """)
    return


@app.cell
def _(KMeans, db, pd, points):
    def _clusters(papers):
        k = min(20, max(1, len(papers) // 250))
        labels = KMeans(n_clusters=k, n_init=1, random_state=0).fit_predict(papers[["x", "y"]])
        return pd.Series([f"{label:02d}" for label in labels], index=papers.index)

    yearly = points.select(["entity_id", "x", "y", "year"]).to_pandas()
    yearly["cluster"] = yearly.groupby("year", group_keys=False)[["x", "y"]].apply(_clusters)

    db.declare_layer("yearly", kind="flat", scope={"group": "years"},
                     require_member_visibility={"count": 1}, title="Clusters of each year")
    db.insert("yearly", yearly, id="entity_id", key="cluster", view="year")
    yearly_report = db.commit()
    print(yearly_report)
    return (yearly_report,)


@app.cell
def _(db, mo, yearly_report):
    _ = yearly_report
    mo.ui.anywidget(db.map(view="years:2010", colour_by="cluster:yearly", height=440))
    return


@app.cell
def _(mo):
    mo.md("""
    ## 5. Changes while it serves

    The week held back in section 2 goes into the running database with the same calls as
    before: `insert` into `papers`, into the `years` group, and into the k-means clusters the
    papers belong to. The k-means insert takes a table of the week's papers and their clusters,
    with `key` naming the cluster column, as in section 1.

    After the first commit, a commit sends its inserts to the running server, which takes them
    while it goes on answering readers. There is no rebuild. The commit waits until the new
    papers are visible, so the counts after it include them.
    """)
    return


@app.cell
def _(db, years):
    def visible(reader):
        return {view: reader.view(view).count() for view in ("papers", f"years:{years[-1]}")}

    before_week = visible(db)
    return before_week, visible


@app.cell
def _(before_week, db, pd, visible, week, week_members):
    db.insert("papers", week, id="entity_id", x="x", y="y", access="categories")
    db.insert("years", week.select(["entity_id", "x", "y", "categories", "year"]),
              id="entity_id", x="x", y="y", access="categories", view="year")
    db.insert("kmeans", week_members["clusters-kmeans"], id="entity", key="key")
    print(db.commit())
    after_week = visible(db)
    pd.DataFrame({"before": before_week, "after": after_week})
    return (after_week,)


@app.cell
def _(mo):
    mo.md("""
    `suppress()` hides papers from every reader from the moment it is accepted, without
    deleting them, and `unsuppress()` shows them again. `remove()` deletes. The cell below
    suppresses five of the new machine-learning papers and counts what the database and the
    machine-learning reader see at each step.
    """)
    return


@app.cell
def _(db, learning, pd, visible, week):
    _is_learning = [bool({"cs.LG", "stat.ML"} & set(one)) for one in week["categories"].to_pylist()]
    hidden = [one for one, keep in zip(week["entity_id"].to_pylist(), _is_learning) if keep][:5]

    def both():
        return {"database": visible(db)["papers"], "cs.LG + stat.ML": visible(learning)["papers"]}

    _before = both()
    print(db.suppress(hidden))
    _suppressed = both()
    print(db.unsuppress(hidden))
    suppression = pd.DataFrame(
        {"before": _before, "suppressed": _suppressed, "unsuppressed": both()}
    )
    suppression
    return hidden, suppression


@app.cell
def _(mo):
    mo.md("""
    ## 6. Save it and serve it elsewhere

    A database is a directory holding its declaration, its data and its server settings.
    `create()` with no path makes a temporary one, which is removed when you call `close()` or
    when Python exits. `save(path)` copies it somewhere permanent, and `open(path)` starts it
    again.
    """)
    return


@app.cell
def _(db, td, tempfile):
    saved_at = db.save(tempfile.mkdtemp(prefix="tessera-arxiv-"))
    reopened = td.open(saved_at)
    print(saved_at)
    reopened.view("papers").count()
    return reopened, saved_at


@app.cell
def _(mo):
    mo.md("""
    The saved directory runs on any machine with the `tessera` binary:

    ```
    tessera serve --deployment <path>/tessera.toml
    ```

    To use a database somebody else runs, connect with the token its operator gave you. The
    maps and the questions above work the same way, over what that token may see.

    ```python
    v = td.connect("https://tessera.example/viewer", token=my_token)
    v.map(colour_by="cluster:topics", layers=["topics"])
    ```
    """)
    return


if __name__ == "__main__":
    app.run()
