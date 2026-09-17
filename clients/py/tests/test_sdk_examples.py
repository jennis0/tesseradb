"""The demo notebook, run headless: every cell of `examples/notebook_marimo.py` (python-sdk.md
§10, §11.2 D).

A marimo notebook is importable Python: an `App` whose cells are decorated functions. Importing
one needs marimo, which is a dependency of neither extra and is absent from the check's
virtualenv. This reads the file instead, takes each `@app.cell` function's body and executes the
bodies in file order in one namespace. That is the order the walk is written in, and the order
marimo runs them in: a cell that reads a variable runs after the cell that defines it, and cells
with no dependency between them run in file order (marimo 0.24.2).

`marimo` is stubbed while they run: `mo.md` returns its text and `mo.ui.anywidget` returns a
holder. The widget is built for real, against a stub bundle: no browser here, and nothing reads
its text.

What is asserted is what each section shows: the counts it prints, and, for every principal and
layer a map is drawn for, that the principal is served artifacts in that layer rather than an
empty map. A notebook that drifts from the package, or a package that changes what it serves,
fails here rather than in front of a reader.

`examples/notebook.ipynb` is the same walk. Its markdown is compared here character for
character, and its code after the two transformations the twin makes: marimo's cell-local
underscore names, and the `mo.ui.anywidget` wrapper a Jupyter cell does not need.
"""

from __future__ import annotations

import ast
import io
import json
import re
import sys
import textwrap
import types
from pathlib import Path

import pytest

from conftest import browse, post

pytest.importorskip("pyarrow")
pytest.importorskip("pandas")
pytest.importorskip("anywidget")

NOTEBOOK = Path(__file__).resolve().parents[1] / "examples" / "notebook_marimo.py"
TWIN = NOTEBOOK.with_name("notebook.ipynb")

#: What the notebook's own sections print, as the corpus stands: 50,000 papers, 2,105 under
#: `astro-ph`, 3,951 under `cs.LG` or `stat.ML`, and the sixty §10.3 ingests.
PAPERS = 50_000
ASTRO_PH = 2_105
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
    """What `mo.ui.anywidget` gives a marimo cell: the widget, and its synced traits as `.value`.

    `.value` is empty here rather than the traits' initial values, which is less than a real run
    would show. Nothing sets a trait without a page, and a cell that reads one is checked against
    the widget itself.
    """

    def __init__(self, widget):
        self.widget = widget
        self.value: dict = {}


@pytest.fixture
def stub_marimo(monkeypatch):
    marimo = types.ModuleType("marimo")
    marimo.md = lambda text: text
    marimo.ui = types.SimpleNamespace(anywidget=Held)
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


def whole_frame(db, view: str) -> list[float]:
    box = next(v for v in db.meta()["views"] if v["id"] == view)["quantisation"]
    return [box["x_min"], box["y_min"], box["x_max"], box["y_max"]]


def artifact_rows(db, view: str, terms=None) -> list[tuple]:
    """The kind-5 artifacts frame of a whole-extent viewport: layer, key, content, masked count.

    A label set expands to a layer whose artifacts attach to the cluster they hang from, and
    `browse` refuses such a layer directly, so this is where a served label's text is read.
    """
    import pyarrow.ipc as ipc

    content = post(
        db.viewer_url + "/v1/viewport",
        db.token(terms).token,
        {"view": view, "zoom": 0, "bbox": whole_frame(db, view), "k": 16, "layers": "all"},
    )
    at, rows = 0, []
    while at + 5 <= len(content):
        kind = content[at]
        length = int.from_bytes(content[at + 1 : at + 5], "little")
        payload = content[at + 5 : at + 5 + length]
        at += 5 + length
        if kind == 5:
            table = ipc.open_stream(io.BytesIO(payload)).read_all().to_pydict()
            rows = list(
                zip(table["layer"], table["key"], table["content"], table["masked_count"])
            )
    return rows


def served_labels(db, view: str, layer: str, terms=None) -> list[tuple]:
    """The rows of a label layer that reached this principal with their text."""
    return [row for row in artifact_rows(db, view, terms) if row[0] == layer and row[2]]


def test_the_notebook_runs_and_serves_what_each_section_prints(walk):
    """One run of the walk, its counts, and the artifacts each map it draws is served.

    One test rather than six: the sections share a database and a server, and splitting them would
    build the corpus six times.
    """
    simple, db, turned = walk["simple"], walk["db"], walk["turned"]

    # §10.1: a frame with a cluster column, mapped. The map colours by `clusters`, so that layer
    # is what has to reach the one principal this database has.
    assert walk["frame"].shape[0] == PAPERS
    assert walk["simple_counts"]["visible"] == PAPERS
    assert isinstance(walk["simple_map"], Held)
    assert len(browse(simple, "map", "clusters")["artifacts"]) == 64
    assert len(served_labels(simple, "map", "topics")) == 64

    # §10.2: the corpus from files, and two principals beside the union. Each count is computed
    # inside its own mask.
    assert walk["whole_counts"]["visible"] == PAPERS
    assert walk["one_term_counts"]["visible"] == ASTRO_PH
    assert walk["two_term_counts"]["visible"] == CS_LG_STAT_ML
    # Each of the three maps draws the layer it colours by, for the principal it is drawn for.
    # The walk has run to its end by now, so the k-means layer carries §10.3's cluster too.
    assert len(browse(db, "s0", "clusters/kmeans")["artifacts"]) == 65
    assert len(browse(db, "s0", "clusters/kmeans", terms=["astro-ph"])["artifacts"]) == 14
    hdbscan = browse(db, "s0", "clusters/hdbscan", terms=["cs.LG", "stat.ML"])["artifacts"]
    assert len(hdbscan) > 0
    # The claim the section's prose makes about the topic lines: the union is served them, and a
    # single-category principal is served none, the generating set spanning categories.
    assert len(served_labels(db, "s0", "topics/kmeans")) > 0
    assert served_labels(db, "s0", "topics/kmeans", terms=["astro-ph"]) == []

    # §10.3: the delta, its cluster and its label, and the count before and after.
    report = walk["delta_report"]
    assert report.ok, report
    assert report.rows_accepted == {"s0": NEW_PAPERS}
    assert report.artifacts_minted == 2  # the cluster and the label
    assert walk["before_delta"] == PAPERS
    assert walk["after_delta"] == PAPERS + NEW_PAPERS
    # The map after it is `cs.LG`'s, and the new cluster is drawn there with its own members.
    audio = browse(db, "s0", "clusters/kmeans", terms=["cs.LG"], q="km-audio")["artifacts"]
    assert [row["key"] for row in audio] == ["km-audio"]
    assert audio[0]["masked_count"] == NEW_PAPERS
    # Issue #150, which the cell beside that map states: the line over it reaches nobody.
    assert not [row for row in artifact_rows(db, "s0", terms=["cs.LG"]) if row[1] == "km-audio"
                and row[0].startswith("topics/") and row[2]]

    # §10.4: a clustering over rows the database already holds, drawn for the union.
    era = walk["era_report"]
    assert era.ok, era
    assert era.artifacts_minted == 3
    assert era.rows_accepted == {}
    assert {row["key"] for row in browse(db, "s0", "clusters/era")["artifacts"]} == {
        "era-1990s", "era-2000s", "era-2010s"
    }

    # §10.5: one set of points under two projections, each view serving all of them and the one
    # layer laid out in both.
    assert walk["knn_counts"]["visible"] == PAPERS
    assert walk["rotated_counts"]["visible"] == PAPERS
    assert len(browse(turned, "knn", "clusters/kmeans")["artifacts"]) == 64
    assert len(browse(turned, "rotated", "clusters/kmeans")["artifacts"]) == 64

    # §10.7: saved and reopened, serving what it was saved with.
    assert walk["saved_at"].joinpath("tessera.toml").exists()
    assert walk["reopened_counts"]["visible"] == PAPERS + NEW_PAPERS


# ------------------------------------------------------------------------------------ the twin


def unwrap(text: str, call: str) -> str:
    """Drop `call(` and its matching `)`, keeping the expression inside."""
    while call in text:
        at = text.index(call)
        depth = 0
        for i in range(at + len(call) - 1, len(text)):
            depth += 1 if text[i] == "(" else -1 if text[i] == ")" else 0
            if depth == 0:
                text = text[:at] + textwrap.dedent(text[at + len(call):i]).strip() + text[i + 1:]
                break
        else:
            raise AssertionError(f"unbalanced {call}")
    return text


def as_jupyter(code: str) -> str:
    """A marimo cell's code as the twin carries it, and a twin cell's code unchanged.

    The two transformations the twin makes: marimo's cell-local underscore names, which mean
    nothing in Jupyter, and the `mo.ui.anywidget` wrapper, which is marimo's way of reading a
    widget's traits. Both are idempotent, so the twin's own cells pass through them unchanged.
    """
    code = code.replace("import marimo as mo\n", "")
    code = unwrap(code, "mo.ui.anywidget(")
    code = code.replace('mo.md(f"', 'print(f"')
    code = re.sub(r"\b_([a-z][A-Za-z_0-9]*)", r"\1", code)
    return "\n".join(line.strip() for line in code.splitlines() if line.strip())


def marimo_cells() -> list[tuple[str, str]]:
    """Each cell of the marimo notebook as `(kind, text)`, a lone `mo.md` being markdown."""
    source = NOTEBOOK.read_text(encoding="utf-8")
    lines = source.splitlines()
    markdown = re.compile(r'^mo\.md\(\s*"""(.*?)"""\s*\)\s*$', re.S)
    cells = []
    for body in cell_bodies(NOTEBOOK):
        if not body.body:
            continue
        text = textwrap.dedent(
            "\n".join(lines[body.body[0].lineno - 1 : body.body[-1].end_lineno])
        )
        prose = markdown.match(text.strip())
        if prose:
            cells.append(("markdown", textwrap.dedent(prose.group(1)).strip()))
        else:
            cells.append(("code", as_jupyter(text).strip()))
    return cells


def test_the_jupyter_twin_is_the_same_walk_and_holds_no_output():
    """The `.ipynb` cell for cell against the marimo file, and no saved output in it: a notebook
    committed with outputs is a diff nobody can read and numbers nobody re-ran."""
    twin = json.loads(TWIN.read_text(encoding="utf-8"))
    for cell in twin["cells"]:
        if cell["cell_type"] == "code":
            assert cell["outputs"] == [] and cell["execution_count"] is None

    theirs = [
        (cell["cell_type"],
         "".join(cell["source"]).strip() if cell["cell_type"] == "markdown"
         else as_jupyter("".join(cell["source"])).strip())
        for cell in twin["cells"]
    ]
    ours = marimo_cells()

    # The twin carries two cells of its own: the one that reads the widget's traits, which marimo
    # reads as `.value`, and the closing one, which marimo has no use for because a cell that
    # closed a database would run at startup.
    reading = [i for i, (_, text) in enumerate(ours) if "simple_map.value" in text]
    assert len(reading) == 1
    assert len(theirs) == len(ours) + 1
    assert theirs[-1][0] == "code" and "close()" in theirs[-1][1]
    for i, (ours_cell, theirs_cell) in enumerate(zip(ours, theirs)):
        if i == reading[0]:
            traits = ("bbox", "layers", "colour_by", "selected", "selected_artifact", "region")
            assert all(f"simple_map.{trait}" in theirs_cell[1] for trait in traits)
            assert all(f'"{trait}"' in ours_cell[1] for trait in traits)
            continue
        assert ours_cell == theirs_cell, f"cell {i} differs between the two notebooks"
