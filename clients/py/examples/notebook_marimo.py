# A guided walk through the SDK, as a Marimo notebook:
# `marimo edit clients/py/examples/notebook_marimo.py`.
#
# Marimo folds every synced trait of a widget into one `.value`, so a cell that reads a map
# re-runs at every settle. The reading cells are kept apart from the ones that build for that
# reason.
import marimo

__generated_with = "0.24.2"
app = marimo.App(width="medium")


@app.cell
def _():
    import datetime
    import json
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

    #: `whole` is the 2.4 million papers; `sample` is 50,000 of them, drawn at random.
    SCALE = os.environ.get("TESSERA_NOTEBOOK_SCALE", "whole")
    CORPORA = {"whole": "notebook-2m4-live", "sample": "notebook-sample"}

    def corpus_directory():
        """`data/<corpus>/` above this file or the working directory, or `TESSERA_NOTEBOOK_DATA`."""
        named = os.environ.get("TESSERA_NOTEBOOK_DATA")
        if named:
            return pathlib.Path(named).expanduser()
        starts = [pathlib.Path.cwd().resolve()]
        here = globals().get("__file__")
        if here:
            starts.append(pathlib.Path(here).resolve().parent)
        for start in starts:
            for directory in [start, *start.parents]:
                if (directory / "data" / CORPORA[SCALE] / "schema.toml").exists():
                    return directory / "data" / CORPORA[SCALE]
        raise FileNotFoundError(
            f"data/{CORPORA[SCALE]}/ is above neither this file nor the working directory. "
            "Set TESSERA_NOTEBOOK_DATA to the corpus directory"
        )

    DATA = corpus_directory()

    def counts(table):
        """A served table's `visible`, `matched`, `highlighted` and `served` counts."""
        return json.loads(table.schema.metadata[b"tessera.counts"])

    return DATA, KMeans, SCALE, counts, datetime, mo, pa, pc, pd, pq, td, tempfile


@app.cell
def _(mo):
    mo.md("""
    # A Tessera database in a notebook
    """)
    return


@app.cell
def _(mo):
    mo.md("""
    ## 1. A map from a DataFrame

    A frame with coordinates, a title and a cluster column becomes a served map.
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
    _topics = pq.read_table(DATA / "topics-kmeans.parquet").to_pandas()
    topic_names = {row.attached_key: row.contents[0][0] for row in _topics.itertuples()}
    return (topic_names,)


@app.cell
def _(frame, td, topic_names):
    simple = td.create()
    simple.declare_view("map")
    simple.declare_columns(frame, skip=["entity_id", "x", "y", "cluster"], index=["title"])
    simple.declare_layer("clusters", kind="flat")
    simple.declare_labels("topics", of="clusters")

    simple.insert("map", frame, id="entity_id", x="x", y="y")
    simple.insert("clusters", frame, id="entity_id", key="cluster")
    simple.insert("topics", topic_names)
    print(simple.commit())
    return (simple,)


@app.cell
def _(mo, simple):
    mo.ui.anywidget(simple.map(colour_by="cluster:clusters", height=520))
    return


@app.cell
def _(counts, simple):
    counts(simple.viewport())
    return


@app.cell
def _(mo):
    mo.md("""
    ## 2. The arXiv database

    One view of every paper, a view per year, three clusterings, and two readers with different access.
    """)
    return


@app.cell
def _(DATA, datetime, pa, pc, pq):
    points = pq.read_table(DATA / "points.parquet")
    points = points.append_column(
        "year", pc.cast(pc.year(points["submitted_at"]), pa.string())
    )

    # The final seven days of submissions are held back for section 5.
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

    years = sorted(set(points["year"].to_pylist()))
    (points.num_rows, week.num_rows, f"{years[0]}-{years[-1]}")
    return members, points, week, week_members, years


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

    # How many of a cluster's papers a reader must see before the cluster is shown to them.
    _floor = {"whole": 50, "sample": 5}[SCALE]
    db.declare_layer("clusters/toponymy", kind="tiered", views=["papers", "years"],
                     levels=[(0, "coarsest"), (1, "coarse"), (2, "fine"), (3, "finest")],
                     require_member_visibility={"count": _floor}, title="Topics")
    db.declare_labels("topics/toponymy", of="clusters/toponymy", content_requires="all",
                      title="Topic names")
    db.declare_layer("taxonomy/arxiv", kind="tiered", views=["papers", "years"],
                     levels=[(0, "archive"), (1, "subject class")],
                     require_member_visibility={"count": 1}, computed=("centroid", "box"),
                     title="arXiv classification")
    db.declare_layer("clusters/kmeans", kind="flat", views=["papers"], value_set="open",
                     require_member_visibility={"count": 50}, title="k-means clusters")
    # Filled in section 4, one clustering per year computed after the database is serving.
    db.declare_layer("clusters/yearly", kind="flat", scope={"group": "years"},
                     require_member_visibility={"count": 1}, title="Clusters of each year")
    return (db,)


@app.cell
def _(DATA, db, members, pa, points, years):
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
    for _layer, _name in [("clusters/toponymy", "clusters-toponymy"),
                          ("taxonomy/arxiv", "taxonomy-arxiv"),
                          ("clusters/kmeans", "clusters-kmeans")]:
        db.insert(_layer, artifacts=str(DATA / f"{_name}.parquet"), **_columns)
        db.insert(_layer, members=members[_name], id="entity", key="key", level="level",
                  rank="rank")
    db.insert("topics/toponymy", str(DATA / "topics-toponymy.parquet"), **_columns)
    db.insert("topics/toponymy", members=members["topics-toponymy"], id="entity", key="key",
              level="level", rank="rank")
    return


@app.cell
def _(db):
    print(db.commit())
    return


@app.cell
def _(db, mo):
    mo.ui.anywidget(db.map(view="papers", colour_by="cluster:clusters/toponymy", height=520))
    return


@app.cell
def _(db):
    astro = db.viewer(["astro-ph", "astro-ph.CO", "astro-ph.EP", "astro-ph.GA", "astro-ph.HE",
                       "astro-ph.IM", "astro-ph.SR"])
    learning = db.viewer(["cs.LG", "stat.ML"])
    return astro, learning


@app.cell
def _(astro, mo):
    mo.ui.anywidget(astro.map(view="papers", colour_by="cluster:clusters/toponymy", height=380))
    return


@app.cell
def _(learning, mo):
    mo.ui.anywidget(learning.map(view="papers", colour_by="cluster:clusters/toponymy", height=380))
    return


@app.cell
def _(astro, counts, db, learning, pd):
    pd.DataFrame({
        "database": counts(db.viewport(view="papers")),
        "astro-ph.*": counts(astro.viewport(view="papers")),
        "cs.LG + stat.ML": counts(learning.viewport(view="papers")),
    })
    return


@app.cell
def _(mo):
    mo.md("""
    ## 3. Asking questions

    Filters, text search, highlight, an item card, and counts per topic for each reader.
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
def _(black_holes, counts, db, pd, recent_cs, transformers):
    pd.DataFrame({
        "cs since 2020": counts(db.viewport(view="papers", filters=recent_cs)),
        "'black hole' in the abstract": counts(db.viewport(view="papers", filters=black_holes)),
        "cs since 2020, 'transformer' lit": counts(
            db.viewport(view="papers", filters=recent_cs, highlight=transformers)
        ),
    })
    return


@app.cell
def _(db, mo, recent_cs):
    mo.ui.anywidget(db.map(view="papers", filters=recent_cs,
                           colour_by="cluster:clusters/toponymy", height=440))
    return


@app.cell
def _(black_holes, db):
    _served = db.viewport(view="papers", filters=black_holes, k=16)
    db.item(_served["tessera_id"][0].as_py())
    return


@app.cell
def _(DATA, astro, learning, pd, pq, since):
    _names = {row.attached_key: row.contents[0][0]
              for row in pq.read_table(DATA / "topics-toponymy.parquet").to_pandas().itertuples()}
    _since_2020 = {"submitted_at": {"range": {"gte": since("2020-01-01")}}}

    def per_topic(reader):
        page = reader.browse_artifacts("papers", "clusters/toponymy", level=0,
                                       filters=_since_2020)
        return pd.DataFrame(page["artifacts"]).set_index("key")[["masked_count", "matched_count"]]

    pd.concat({"astro-ph.*": per_topic(astro), "cs.LG + stat.ML": per_topic(learning)},
              axis=1).rename(index=_names).astype("Int64")
    return


@app.cell
def _(mo):
    mo.md("""
    ## 4. Time

    One view per year, and a clustering computed for each year and added to the running database.
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
    year_map = db.map(view=f"years:{years[-1]}", colour_by="cluster:clusters/toponymy",
                      height=440)
    mo.ui.anywidget(year_map)
    return (year_map,)


@app.cell
def _(year, year_map):
    year_map.view = f"years:{year.value}"
    return


@app.cell
def _(KMeans, db, pd, points):
    def _clusters(papers):
        # About one cluster per 250 papers, at least one and at most twenty.
        k = min(20, max(1, len(papers) // 250))
        labels = KMeans(n_clusters=k, n_init=1, random_state=0).fit_predict(papers[["x", "y"]])
        year = papers.name
        return pd.Series([f"{year}-{label:02d}" for label in labels], index=papers.index)

    yearly = points.select(["entity_id", "x", "y", "year"]).to_pandas()
    yearly["cluster"] = yearly.groupby("year", group_keys=False)[["x", "y"]].apply(_clusters)

    db.insert("clusters/yearly", artifacts=yearly[["year", "cluster"]].drop_duplicates(),
              key="cluster", view="year")
    db.insert("clusters/yearly", members=yearly, id="entity_id", key="cluster", view="year")
    yearly_report = db.commit()
    print(yearly_report)
    return (yearly_report,)


@app.cell
def _(db, mo, yearly_report):
    _ = yearly_report
    mo.ui.anywidget(db.map(view="years:2010", colour_by="cluster:clusters/yearly", height=440))
    return


@app.cell
def _(mo):
    mo.md("""
    ## 5. Changes while it serves

    The held-back week goes into the running database, and a few papers are suppressed and restored.
    """)
    return


@app.cell
def _(counts, db, years):
    def visible(reader):
        return {view: counts(reader.viewport(view=view))["visible"]
                for view in ("papers", f"years:{years[-1]}")}

    before_week = visible(db)
    return before_week, visible


@app.cell
def _(before_week, db, pd, visible, week, week_members):
    db.insert("papers", week, id="entity_id", x="x", y="y", access="categories")
    db.insert("years", week.select(["entity_id", "x", "y", "categories", "year"]),
              id="entity_id", x="x", y="y", access="categories", view="year")
    db.insert("clusters/kmeans", members=week_members["clusters-kmeans"], id="entity", key="key",
              level="level", rank="rank")
    print(db.commit())
    pd.DataFrame({"before": before_week, "after": visible(db)})
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
    pd.DataFrame({"before": _before, "suppressed": _suppressed, "unsuppressed": both()})
    return


@app.cell
def _(mo):
    mo.md("""
    ## 6. Save it and serve it elsewhere

    The database is a directory: save it, reopen it, or serve it with `tessera serve`.
    """)
    return


@app.cell
def _(counts, db, td, tempfile):
    saved_at = db.save(tempfile.mkdtemp(prefix="tessera-arxiv-"))
    reopened = td.open(saved_at)
    print(saved_at)
    counts(reopened.viewport(view="papers"))
    return


@app.cell
def _(mo):
    mo.md("""
    ```python
    v = td.connect("https://tessera.example/viewer", token=my_token)
    v.map(colour_by="cluster:clusters/toponymy")
    ```
    """)
    return


if __name__ == "__main__":
    app.run()
