"""The demo notebook, run headless: every cell of `examples/notebook_marimo.py` at the sample scale.

A marimo notebook is importable Python: an `App` whose cells are decorated functions. This reads
the file instead, takes each `@app.cell` function's body and executes the bodies in file order in
one namespace. That is the order the walk is written in, and the order marimo runs them in: a cell
that reads a variable runs after the cell that defines it, and cells with no dependency between
them run in file order.

The one cell that assigns `SCALE` has its value replaced with `"sample"` before it runs, so the
walk builds `data/notebook-sample/`, which the notebook finds above its own directory. The test
skips, naming what is missing, where that directory or the `tessera` binary is absent.

`marimo` is stubbed while the cells run: `mo.md` returns its text, `mo.ui.anywidget` records the
widget it is given, and `mo.ui.slider` holds its starting value. The widget is built for real,
against a stub bundle: there is no browser here, and nothing reads its text.

What is asserted is what each section prints, with the expected numbers read from the sample's
own files, and that every map is served each layer it draws or colours by.

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
def maps() -> list:
    """Every widget a cell hands to `mo.ui.anywidget`, in the order the cells ran."""
    return []


@pytest.fixture
def stub_marimo(monkeypatch, maps):
    def anywidget(widget):
        maps.append(widget)
        return Held(widget)

    marimo = types.ModuleType("marimo")
    marimo.md = lambda text: text
    marimo.ui = types.SimpleNamespace(
        anywidget=anywidget,
        slider=lambda *, value, **_: types.SimpleNamespace(value=value),
    )
    monkeypatch.setitem(sys.modules, "marimo", marimo)
    return marimo


@pytest.fixture
def walk(monkeypatch, tmp_path, stub_marimo, stub_bundle):
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


def expected(data: Path, years: list[str]) -> dict:
    """The counts the notebook should print, read from the sample's files as pandas reads them."""
    papers = pd.read_parquet(
        data / "points.parquet", columns=["entity_id", "submitted_at", "categories", "archive"]
    )
    in_week = papers["submitted_at"] > papers["submitted_at"].max() - pd.Timedelta(days=7)
    built = papers[~in_week]
    last_year = papers["submitted_at"].dt.year.astype(str) == years[-1]

    def holding(frame, terms):
        return int(frame["categories"].map(lambda held: bool(terms & set(held))).sum())

    return {
        "papers": len(papers),
        "built": len(built),
        "astro": holding(built, ASTRO_PH),
        "learning": holding(built, LEARNING),
        "learning after the week": holding(papers, LEARNING),
        "cs since 2020": int(
            ((built["archive"] == "cs") & (built["submitted_at"] >= pd.Timestamp("2020-01-01")))
            .sum()
        ),
        "last year before the week": int((last_year & ~in_week).sum()),
        "last year": int(last_year.sum()),
    }


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
    import tesseradb as td

    want = expected(walk["DATA"], walk["years"])

    # Section 1: the frame, built and counted.
    assert walk["frame"].shape[0] == want["papers"]
    assert walk["first_count"] == want["papers"]

    # Section 2: the database without the held-back week, as three readers count it.
    assert walk["reader_counts"] == {
        "database": want["built"],
        "astro-ph.*": want["astro"],
        "cs.LG + stat.ML": want["learning"],
    }

    # Section 3: the filters narrow the count, and combining two narrows it further.
    counts = walk["filter_counts"]
    assert counts["cs since 2020"] == want["cs since 2020"]
    assert 0 < counts["'black hole' in the abstract"] < want["built"]
    assert 0 < counts["cs since 2020, 'transformer' in the title"] < counts["cs since 2020"]

    # Section 5: the week arrives in both views it was inserted into.
    last = f"years:{walk['years'][-1]}"
    assert walk["before_week"] == {"papers": want["built"], last: want["last year before the week"]}
    assert walk["after_week"] == {"papers": want["papers"], last: want["last year"]}

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

    # Every map, as the reader it was drawn for, over its view and filter, is served each layer
    # it draws or colours by.
    assert len(maps) == 7
    for widget in maps:
        reader = td.connect(widget.url, token=widget._token_source)
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
