"""A rendered value an item does not hold is a null on the viewport's points frame.

What each point should carry is read from the corpus, never from the server: every value is a
function of the item's source id (`declared_fixture`), and a point names its item by the rendered
`fx` column. Each column holds absent values and genuine zeros, so serving a zero for an absence
and serving a null for a zero both fail.

The deployment is built over part of the corpus and takes the rest through `/control/ingest`, so
absences written by the build and by a flush are both read. One column has a value for every item
the build holds and absences only among the ingested ones, so a tile draws from a segment with no
absence and a segment with some in the same column. The same answer is required after a restart
and after a fold, which rewrites every segment.

A segment with no column at all cannot be built from a declaration: the build refuses a source
lacking a declared column, and only a sharing group's own key, which a declaration cannot name,
leaves a view's built segment without a family it later gains. That case is tested in Rust, over a
bundle built without a declaration.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

import declared_fixture as fx
from declared_fixture import Column, Corpus, Deployment
from oracle.wire import decode_viewport_points

BUILT = list(range(fx.N_BUILT))
INGESTED = list(range(fx.N_BUILT, fx.N_ITEMS))

COLUMNS = [
    Column("heat", "f32", pa.float32(), lambda i: None if i % 4 == 0 else float(i % 3), render=True),
    Column("count", "u32", pa.uint32(), lambda i: None if i % 5 == 0 else i % 2, render=True),
    Column(
        "seen", "timestamp_us", pa.timestamp("us"),
        lambda i: None if i % 6 == 0 else (i % 2) * 1_000_000, render=True,
    ),
    Column("flag", "bool", pa.bool_(), lambda i: None if i % 7 == 0 else i % 2 == 0, render=True),
    Column(
        "late", "f64", pa.float64(),
        lambda i: None if i >= fx.N_BUILT and i % 2 == 0 else float(i % 3), render=True,
    ),
]


def build(work) -> Deployment:
    blocks = [fx.plain_view_toml(), fx.fx_column().toml(), *(c.toml() for c in COLUMNS)]
    points = {fx.WORLD: (BUILT, [fx.fx_column(), *COLUMNS], 0, fx.WORLD_EXTENT)}
    d = Deployment(work, Corpus(blocks, points))
    d.rows(
        "/control/ingest",
        fx.point_rows(INGESTED, [fx.fx_column(), *COLUMNS]),
        "absent-world",
    )
    d.publish()
    return d


def served(d: Deployment, view: str) -> pa.Table:
    token = d.server.authorise(list(fx.PRINCIPALS["everyone"]))["token"]
    return decode_viewport_points(d.server.viewport(token, view, 0, fx.BBOX, k=fx.K))


def values(table: pa.Table, name: str) -> list:
    column = table.column(name)
    if pa.types.is_timestamp(column.type):
        column = column.cast(pa.int64())
    return column.to_pylist()


def check(d: Deployment, stage: str) -> None:
    world = served(d, fx.WORLD)
    items = values(world, "fx")
    assert sorted(items) == BUILT + INGESTED, stage
    for column in COLUMNS:
        got = dict(zip(items, values(world, column.name)))
        want = {i: column.value(i) for i in items}
        assert got == want, f"{stage}: {column.name}"
        present = [v for v in want.values() if v is not None]
        assert None in want.values() and 0 in present, f"{column.name} has absences and zeros"
    assert all(COLUMNS[-1].value(i) is not None for i in BUILT), "the build holds every `late`"


@pytest.fixture(scope="module")
def deployment(tmp_path_factory):
    d = build(tmp_path_factory.mktemp("absent-render-values"))
    yield d
    Deployment.stop_all()


def test_an_absent_rendered_value_is_null_through_a_restart_and_a_fold(deployment):
    check(deployment, "live")
    deployment.restart()
    check(deployment, "restart")
    deployment.fold()
    check(deployment, "fold")
