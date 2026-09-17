# The same example as widget.ipynb, as a Marimo notebook: `marimo edit clients/py/examples/widget_marimo.py`.
# A database made here, committed, and read in the cell below. Marimo folds every synced trait into
# one `.value`, so the reading cell re-runs at every settle.
import marimo

__generated_with = "0.24.0"
app = marimo.App()


@app.cell
def _():
    import os
    import pathlib

    import marimo as mo
    import tesseradb as td

    # `data/notebook/`: 50,000 arXiv papers, their k-means clusters and the topics over them.
    DATA = pathlib.Path(os.environ.get("TESSERA_NOTEBOOK_DATA", "../../../data/notebook"))
    return DATA, mo, td


@app.cell
def _(DATA, td):
    db = td.create()  # a temporary directory, on /dev/shm where there is one
    db.stage("points", str(DATA / "points.parquet"), default=True)
    for _name, _file in [
        ("kmeans", "clusters-kmeans"),
        ("kmeans_members", "clusters-kmeans-members"),
        ("kmeans_topics", "topics-kmeans"),
        ("kmeans_topic_members", "topics-kmeans-members"),
    ]:
        db.stage(_name, str(DATA / f"{_file}.parquet"))

    db.declare_view("s0", source="points", access="categories", title="arXiv, 50,000 papers")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("arxiv_id", type="keyword", index=True, title="arXiv ID")
    db.declare_layer(
        "clusters/kmeans",
        kind="flat",
        source="kmeans",
        members="kmeans_members",
        require_member_visibility={"count": 50},
        title="k-means clusters",
    )
    db.declare_labels(
        "topics/kmeans",
        of="clusters/kmeans",
        source="kmeans_topics",
        members="kmeans_topic_members",
        content_requires="all",
        title="k-means topics",
    )
    print(db.commit())  # tessera check, tessera build, tessera serve
    return (db,)


@app.cell
def _(db, mo):
    # The database's own principal: every access label it staged. The token is minted in the
    # kernel and handed to the page as a message; the session credential never leaves here.
    m = mo.ui.anywidget(db.map(colour_by="cluster:clusters/kmeans", height=520))
    m
    return (m,)


@app.cell
def _(m):
    # Re-runs at every settle: `.value` is every synced trait.
    {k: m.value.get(k) for k in ("bbox", "layers", "colour_by", "selected", "selected_artifact", "region")}
    return


@app.cell
def _(db):
    # What one arXiv category's principal sees, computed inside their own mask and not filtered
    # down from the operator's: `db.viewer(terms)` mints for exactly those terms.
    db.viewer(["cs.LG"]).map(height=380)
    return


@app.cell
def _(db):
    # The query verbs, through the same plane with the same token: never by reading the bundle.
    _points = db.viewport(k=64)
    _points.column_names, _points.num_rows, db.item(_points.column("tessera_id")[0].as_py())["fields"]
    return


@app.cell
def _(mo):
    mo.md(
        """
        A deployment somebody else runs is the same widget and the same read verbs, against a
        token that deployment issued you — and no `viewer(terms)`, minting another principal
        needing the session credential:

        ```python
        v = td.connect("https://tessera.example/viewer", token=my_token)
        v.map(colour_by="cluster:clusters/kmeans")
        ```

        The page there calls the viewer plane from this page's origin, so that origin must be in
        the deployment's CORS list. A database made here needs no list: its three planes are on
        loopback and `serve.cors_loopback` admits a page served from one.
        """
    )
    return


if __name__ == "__main__":
    app.run()
