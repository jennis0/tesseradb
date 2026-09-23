"""The demo notebook, run headless: every cell of `examples/notebook_marimo.py` at the sample scale.

A marimo notebook is importable Python: an `App` whose cells are decorated functions. This reads
the file instead, takes each `@app.cell` function's body and executes the bodies in file order in
one namespace. That is the order the walk is written in, and the order marimo runs them in: a cell
that reads a variable runs after the cell that defines it, and cells with no dependency between
them run in file order.

The one cell that assigns `SCALE` has its value replaced with `"sample"` before it runs, so the
walk builds `data/notebook-sample/`, which the notebook finds above its own directory. The test
skips, naming what is missing, where that directory or the `tessera` binary is absent.

`marimo` is stubbed while the cells run: `mo.md` returns its text, `mo.ui.anywidget` returns a
holder, and `mo.ui.slider` holds its starting value. The widget is built for real, against a stub
JavaScript bundle: there is no browser here, and nothing reads its text.

What is asserted is what each section prints, with the expected numbers read from the sample's
own files; that every map is served each layer it draws or colours by; and that a reader is
served each topic's title exactly when it may see every paper the title was written from.

`examples/notebook.ipynb` is generated from the marimo file by `marimo export ipynb`, and the last
test checks that it is what a fresh export makes.
"""

from __future__ import annotations

import ast
import json
import subprocess
import sys
import tempfile
import types
from pathlib import Path

import pytest

from conftest import binary
from tesseradb import Viewer

pytest.importorskip("pyarrow")
pd = pytest.importorskip("pandas")
pytest.importorskip("anywidget")
pytest.importorskip("sklearn")

NOTEBOOK = Path(__file__).resolve().parents[1] / "examples" / "notebook_marimo.py"
TWIN = NOTEBOOK.with_name("notebook.ipynb")

ASTRO_PH = {"astro-ph", "astro-ph.CO", "astro-ph.EP", "astro-ph.GA", "astro-ph.HE",
            "astro-ph.IM", "astro-ph.SR"}
LEARNING = {"cs.LG", "stat.ML"}


def cell_bodies(path: Path) -> list[ast.Module]:
    """Each `@app.cell` function's body, with its `return` cut and `SCALE` set to the sample."""
    bodies, scales = [], 0
    for node in ast.parse(path.read_text(encoding="utf-8"), filename=str(path)).body:
        if not isinstance(node, ast.FunctionDef):
            continue
        if not any(
            isinstance(d, ast.Attribute) and d.attr == "cell" for d in node.decorator_list
        ):
            continue
        body = [line for line in node.body if not isinstance(line, ast.Return)]
        for line in body:
            if isinstance(line, ast.Assign) and [getattr(t, "id", None) for t in line.targets] == [
                "SCALE"
            ]:
                line.value = ast.copy_location(ast.Constant("sample"), line.value)
                scales += 1
        bodies.append(ast.Module(body=body, type_ignores=[]))
    assert scales == 1, f"{path.name} should assign SCALE in exactly one cell"
    return bodies


class Held:
    """What `mo.ui.anywidget` gives a marimo cell: the widget, and its synced traits as `.value`."""

    def __init__(self, widget):
        self.widget = widget
        self.value: dict = {}


@pytest.fixture
def stub_marimo(monkeypatch):
    marimo = types.ModuleType("marimo")
    marimo.md = lambda text: text
    marimo.ui = types.SimpleNamespace(
        anywidget=Held,
        slider=lambda *, value, **_: types.SimpleNamespace(value=value),
    )
    monkeypatch.setitem(sys.modules, "marimo", marimo)
    return marimo


@pytest.fixture
def maps(monkeypatch) -> list:
    """Every map the notebook draws, as `(reader, widget)`, in the order the cells ran.

    Every map, whether from a database, a reader or a selection, is made by `Viewer.map`.
    """
    drawn = []
    make = Viewer.map

    def recorded(reader, *args, **kwargs):
        widget = make(reader, *args, **kwargs)
        drawn.append((reader, widget))
        return widget

    monkeypatch.setattr(Viewer, "map", recorded)
    return drawn


@pytest.fixture
def walk(monkeypatch, tmp_path, stub_marimo, stub_bundle, maps):
    """The notebook's cells, executed in order, with every database it opened closed after."""
    binary()
    monkeypatch.delenv("TESSERA_NOTEBOOK_DATA", raising=False)
    # Section 6 saves the database under a fresh temporary directory; this puts it in tmp_path.
    monkeypatch.setattr(tempfile, "tempdir", str(tmp_path))
    namespace: dict = {"__name__": "notebook_marimo", "__file__": str(NOTEBOOK)}
    try:
        for body in cell_bodies(NOTEBOOK):
            try:
                exec(compile(body, str(NOTEBOOK), "exec"), namespace)  # noqa: S102
            except FileNotFoundError as why:
                if "DATA" in namespace:
                    raise
                pytest.skip(str(why))
        yield namespace
    finally:
        from tesseradb._database import Database

        for value in list(namespace.values()):
            if isinstance(value, Database):
                value.close()


def tokens(texts) -> list[list[str]]:
    """Each text's tokens, from `tessera tokenise`, the analyser the server indexes text with.

    The count of a `match` or `phrase` filter is computed from these in pandas. A regular
    expression places word boundaries differently from the analyser, and its counts on the
    sample differ from the server's.
    """
    lines = "\n".join(" ".join(str(text).split()) if text is not None else "" for text in texts)
    out = subprocess.run(
        [binary(), "tokenise"], input=lines, capture_output=True, text=True, check=True
    ).stdout.split("\n")[: len(texts)]
    return [line.split("\t") if line else [] for line in out]


def expected(data: Path, years: list[str]) -> dict:
    """The counts the notebook should print, read from the sample's files as pandas reads them."""
    papers = pd.read_parquet(
        data / "points.parquet",
        columns=["entity_id", "submitted_at", "categories", "archive", "title", "abstract"],
    )
    in_week = papers["submitted_at"] > papers["submitted_at"].max() - pd.Timedelta(days=7)
    built = papers[~in_week]
    last_year = papers["submitted_at"].dt.year.astype(str) == years[-1]

    def holding(frame, terms):
        return int(frame["categories"].map(lambda held: bool(terms & set(held))).sum())

    cs_since_2020 = (built["archive"] == "cs") & (
        built["submitted_at"] >= pd.Timestamp("2020-01-01")
    )
    transformer = pd.Series(["transformer" in words for words in tokens(built["title"])],
                            index=built.index)
    black_hole = [
        any(pair == ("black", "hole") for pair in zip(words, words[1:]))
        for words in tokens(built["abstract"])
    ]

    return {
        "papers": len(papers),
        "built": len(built),
        "astro": holding(built, ASTRO_PH),
        "learning": holding(built, LEARNING),
        "learning after the week": holding(papers, LEARNING),
        "cs since 2020": int(cs_since_2020.sum()),
        "black hole": sum(black_hole),
        "transformer since 2020": int((cs_since_2020 & transformer).sum()),
        "last year before the week": int((last_year & ~in_week).sum()),
        "last year": int(last_year.sum()),
    }


def names_seen(data: Path, terms: set[str] | None = None) -> dict[str, list[str]]:
    """The topic titles a reader holding `terms` should be shown, by the name's key.

    The notebook inserts each name's title, rank 0 of its contents, with the papers the title was
    written from. A reader is shown the title when it may see every one of those papers.
    `terms=None` is a reader who sees every paper.
    """
    papers = pd.read_parquet(data / "points.parquet", columns=["entity_id", "categories"])
    if terms is not None:
        papers = papers[papers["categories"].map(lambda held: bool(terms & set(held)))]
    seen = set(papers["entity_id"])
    sources = pd.read_parquet(data / "topics-toponymy-members.parquet")
    sources = sources[sources["rank"] == 0]
    shown = sources.groupby("key")["entity"].agg(lambda ids: set(ids) <= seen)
    contents = pd.read_parquet(data / "topics-toponymy.parquet").set_index("key")["contents"]
    return {key: list(contents[key][0]) for key, visible in shown.items() if visible}


def served_names(reader) -> dict[str, list[str]]:
    artifacts = reader.view("papers").sample(layers=["topic_names"]).artifacts
    if artifacts is None:
        return {}
    return dict(zip(artifacts.column("key").to_pylist(), artifacts.column("content").to_pylist()))


def drawn_layers(widget) -> set[str]:
    """The layers a map draws, and the layer it colours by when that is a clustering."""
    layers = set(widget.layers or [])
    if widget.colour_by and widget.colour_by.startswith("cluster:"):
        layers.add(widget.colour_by.removeprefix("cluster:"))
    return layers


def test_the_notebook_runs_and_serves_what_each_section_prints(walk, maps):
    """One run of the walk, the counts it prints, and the layers each of its maps is served.

    One test rather than six: the sections share a database and a server, and splitting them
    would build the sample six times.
    """

    want = expected(walk["DATA"], walk["years"])

    # Section 1: the frame, built and counted.
    assert walk["first_count"] == want["papers"]

    # Section 2: the database without the held-back week, as three readers count it.
    assert walk["reader_counts"] == {
        "database": want["built"],
        "astro-ph.*": want["astro"],
        "cs.LG + stat.ML": want["learning"],
    }

    # Section 3: the filters narrow the count, and combining two narrows it further.
    counts = walk["filter_counts"]
    assert counts == {
        "cs since 2020": want["cs since 2020"],
        "'black hole' in the abstract": want["black hole"],
        "cs since 2020, 'transformer' in the title": want["transformer since 2020"],
    }

    # Section 4 and section 5 both commit, and neither was refused.
    assert walk["yearly_report"].ok, walk["yearly_report"]
    assert walk["week_report"].ok, walk["week_report"]

    # Section 5: the week arrives in both views it was inserted into.
    last = f"years:{walk['years'][-1]}"
    assert walk["before_week"] == {"papers": want["built"], last: want["last year before the week"]}
    assert walk["after_week"] == {"papers": want["papers"], last: want["last year"]}
    # Every paper of the last year, the week's included, is in one of that year's clusters.
    clusters = walk["db"].view(last).sample(layers=["yearly"]).artifacts
    assert sum(clusters.column("masked_count").to_pylist()) == want["last year"]

    # Suppressing five machine-learning papers hides them from both readers, and lifting the
    # suppression shows them again.
    suppression = walk["suppression"]
    before = {"database": want["papers"], "cs.LG + stat.ML": want["learning after the week"]}
    assert suppression["before"].to_dict() == before
    assert suppression["suppressed"].to_dict() == {key: n - 5 for key, n in before.items()}
    assert suppression["unsuppressed"].to_dict() == before

    # Section 6: the saved copy serves what it was saved with.
    assert walk["saved_at"].joinpath("tessera.toml").exists()
    assert walk["reopened"].view("papers").count() == want["papers"]

    # A topic's title reaches a reader only where it may see every paper the title was written
    # from. After section 5 the database holds every name in the sample, the week's included,
    # and its own reader is served all of them.
    database_names = served_names(walk["db"])
    assert database_names == names_seen(walk["DATA"])
    for reader, terms in (("astro", ASTRO_PH), ("learning", LEARNING)):
        names = served_names(walk[reader])
        assert names == names_seen(walk["DATA"], terms), reader
        assert 0 < len(names) < len(database_names), reader

    # Every map, as the reader it was drawn for, over its view and filter, is served each layer
    # it draws or colours by.
    assert len(maps) == 7
    for reader, widget in maps:
        # A map given no view opens on the first one.
        selection = reader.view(widget.view or reader.meta()["views"][0]["id"])
        if widget.filters:
            selection = selection.filter(widget.filters)
        for layer in drawn_layers(widget):
            served = selection.sample(layers=[layer]).artifacts
            assert served is not None and layer in served.column("layer").to_pylist(), (
                f"the map of {widget.view} is served nothing in {layer}"
            )


def test_the_jupyter_notebook_is_the_export_of_the_marimo_file(tmp_path):
    """`notebook.ipynb` is what `marimo export ipynb` makes of the marimo file today."""
    pytest.importorskip("marimo")
    fresh = tmp_path / "notebook.ipynb"
    subprocess.run(
        [sys.executable, "-m", "marimo", "export", "ipynb", str(NOTEBOOK), "-o", str(fresh)],
        check=True,
        capture_output=True,
    )

    def read(path: Path) -> dict:
        notebook = json.loads(path.read_text(encoding="utf-8"))
        notebook["metadata"].get("marimo", {}).pop("marimo_version", None)
        return notebook

    assert read(TWIN) == read(fresh)
