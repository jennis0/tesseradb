"""The demo notebook, run headless: every cell of `examples/notebook_marimo.py` (python-sdk.md
§10, §11.2 D).

A marimo notebook is importable Python: an `App` whose cells are decorated functions. Importing
one needs marimo, which is a dependency of neither extra and is absent from the check's
virtualenv. This reads the file instead, takes each `@app.cell` function's body and
executes the bodies in file order in one namespace, which is the order the walk is written in and
marimo's own dependency order. `marimo` is stubbed while they run: `mo.md` returns its text and
`mo.ui.anywidget` returns a holder whose `.value` is empty, which is what a cell that reads a
widget sees before a browser has settled it.

The widget is built for real, against a stub bundle: no browser here, and nothing reads its text.
What is asserted is the served counts each section prints. A notebook that drifts from the
package, or a package that changes what it serves, fails here rather than in front of a reader.

`examples/notebook.ipynb` is the same walk cell for cell; it is not executed here, and the two are
kept in step by hand.
"""

from __future__ import annotations

import ast
import json
import sys
import types
from pathlib import Path

import pytest

from conftest import notebook_corpus  # noqa: F401  (the `corpus` fixture uses it)

pytest.importorskip("pyarrow")
pytest.importorskip("pandas")
pytest.importorskip("anywidget")

NOTEBOOK = Path(__file__).resolve().parents[1] / "examples" / "notebook_marimo.py"

#: What the notebook's own sections print, as the corpus stands: 50,000 papers, 1,077 under
#: `math.AG`, 3,951 under `cs.LG` or `stat.ML`, and the sixty §10.3 ingests.
PAPERS = 50_000
MATH_AG = 1_077
CS_LG_STAT_ML = 3_951
NEW_PAPERS = 60


def cell_bodies(path: Path) -> list[ast.Module]:
    """Each `@app.cell` function's body, with its `return` cut, as a module ready to compile."""
    bodies = []
    for node in ast.parse(path.read_text(encoding="utf-8"), filename=str(path)).body:
        if not isinstance(node, ast.FunctionDef):
            continue
        if not any(
            isinstance(d, ast.Attribute) and d.attr == "cell" for d in node.decorator_list
        ):
            continue
        body = [line for line in node.body if not isinstance(line, ast.Return)]
        bodies.append(ast.Module(body=body, type_ignores=[]))
    return bodies


class Held:
    """What `mo.ui.anywidget` gives a marimo cell: the widget, and every synced trait as `.value`.

    Empty here: the traits are set by the page, and there is no page.
    """

    def __init__(self, widget):
        self.widget = widget
        self.value: dict = {}


@pytest.fixture
def stub_marimo(monkeypatch):
    marimo = types.ModuleType("marimo")
    marimo.md = lambda text: text
    marimo.ui = types.SimpleNamespace(anywidget=Held)
    marimo.App = lambda **kwargs: None
    monkeypatch.setitem(sys.modules, "marimo", marimo)
    return marimo


@pytest.fixture
def stub_bundle(monkeypatch, tmp_path):
    """The components' bundle, stubbed: the widget is constructed, and nothing reads its text."""
    import tesseradb.widget as widget

    stub = tmp_path / "tessera-components.js"
    stub.write_text("export function render() {}")
    monkeypatch.setattr(widget, "bundle_path", lambda: stub)
    return stub


@pytest.fixture
def walk(monkeypatch, tmp_path, corpus, stub_marimo, stub_bundle):
    """The notebook's cells, executed in order, with every database it opened closed after."""
    monkeypatch.setenv("TESSERA_NOTEBOOK_DATA", str(corpus))
    monkeypatch.setenv("TESSERA_DEMO_HOME", str(tmp_path / "saved" / "arxiv"))
    namespace: dict = {"__name__": "notebook_marimo"}
    try:
        for body in cell_bodies(NOTEBOOK):
            exec(compile(body, str(NOTEBOOK), "exec"), namespace)  # noqa: S102
        yield namespace
    finally:
        from tesseradb._database import Database

        for value in list(namespace.values()):
            if isinstance(value, Database):
                value.close()


def test_the_notebook_runs_and_serves_what_each_section_prints(walk):
    """One run of the walk, and every count it prints along the way.

    One test rather than six: the sections share a database and a server, and splitting them would
    build the corpus six times.
    """
    # §10.1: a frame with a cluster column, mapped.
    assert walk["frame"].shape[0] == PAPERS
    assert walk["simple_counts"]["visible"] == PAPERS
    assert isinstance(walk["simple_map"], Held)

    # §10.2: the corpus from files, and two principals beside the union. Each count is computed
    # inside its own mask, so a term's count is smaller than the union's and larger than nothing.
    assert walk["whole_counts"]["visible"] == PAPERS
    assert walk["one_term_counts"]["visible"] == MATH_AG
    assert walk["two_term_counts"]["visible"] == CS_LG_STAT_ML
    assert 0 < MATH_AG < CS_LG_STAT_ML < PAPERS

    # §10.3: the delta, its cluster and its label, and the count before and after.
    report = walk["delta_report"]
    assert report.ok, report
    assert report.rows_accepted == {"s0": NEW_PAPERS}
    assert report.artifacts_minted == 2  # the cluster and the label
    assert walk["before_delta"] == PAPERS
    assert walk["after_delta"] == PAPERS + NEW_PAPERS

    # §10.4: a clustering over rows the database already holds: three artifacts, no new rows.
    era = walk["era_report"]
    assert era.ok, era
    assert era.artifacts_minted == 3
    assert era.rows_accepted == {}

    # §10.5: one set of points under two projections, each view serving all of them.
    assert walk["knn_counts"]["visible"] == PAPERS
    assert walk["rotated_counts"]["visible"] == PAPERS

    # §10.7: saved and reopened, serving what it was saved with.
    assert walk["saved_at"].joinpath("tessera.toml").exists()
    assert walk["reopened_counts"]["visible"] == PAPERS + NEW_PAPERS


def test_the_jupyter_twin_covers_the_same_sections_and_holds_no_output():
    """The `.ipynb` is the same walk, and carries no saved output: a notebook in the repository
    with outputs in it is a diff nobody can read and numbers nobody re-ran."""
    twin = json.loads(NOTEBOOK.with_name("notebook.ipynb").read_text(encoding="utf-8"))
    code = "\n".join(
        "".join(cell["source"]) for cell in twin["cells"] if cell["cell_type"] == "code"
    )
    for cell in twin["cells"]:
        if cell["cell_type"] == "code":
            assert cell["outputs"] == [] and cell["execution_count"] is None

    # The verbs the walk turns on, each in both notebooks.
    marimo = NOTEBOOK.read_text(encoding="utf-8")
    for verb in [
        "td.create(",
        "declare_view(",
        "declare_layer(",
        "declare_labels(",
        "from_column=",
        ".check()",
        ".commit()",
        ".declaration",
        "viewer([",
        "colour_by=",
        'view="rotated"',
        ".save(",
        "td.open(",
        "tessera serve --deployment",
    ]:
        assert verb in code or verb in "".join(
            "".join(cell["source"]) for cell in twin["cells"] if cell["cell_type"] == "markdown"
        ), f"the Jupyter twin does not carry {verb}"
        assert verb in marimo, f"the marimo notebook does not carry {verb}"
