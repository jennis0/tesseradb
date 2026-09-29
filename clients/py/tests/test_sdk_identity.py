"""How a row is named, from the frame to the served answer.

A table names each row's item by the columns it carries of the attributes the user declared
`unique`, at the build and on every later route. A row of the points that carries none is an item
of its own, addressable by the `tessera_id` the server hands back. The SDK keeps no map between
them: what is inserted is what is written, and what is sent is what the column holds.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

from conftest import browse, item, viewport

pytest.importorskip("pyarrow")

FRAME = [-5.0, -5.0, 40.0, 40.0]


def papers(keys, x: float = 0.0) -> pa.Table:
    return pa.table(
        {
            "paper": pa.array(list(keys), pa.string()),
            "x": pa.array([x + i for i in range(len(keys))], pa.float64()),
            "y": pa.array([0.0] * len(keys), pa.float64()),
            "labels": pa.array([["public"]] * len(keys), pa.list_(pa.string())),
        }
    )


def string_ids(db) -> None:
    """A corpus named by a string column, with a clustering whose members name the same keys."""
    keys = [f"p{i}" for i in range(20)]
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_attribute("paper", type="keyword", unique=True)
    db.declare_layer("clusters", kind="flat")
    db.insert("map", papers(keys), x="x", y="y", access="labels")
    db.insert(
        "clusters",
        artifacts=pa.table(
            {"level": pa.array([0], pa.uint32()), "key": pa.array(["c0"], pa.string())}
        ),
        key="key",
        level="level",
    )
    db.insert(
        "clusters",
        members=pa.table(
            {
                "level": pa.array([0] * len(keys), pa.uint32()),
                "key": pa.array(["c0"] * len(keys), pa.string()),
                "paper": pa.array(keys, pa.string()),
            }
        ),
        key="key",
        level="level",
    )


def test_a_string_id_column_names_the_rows_the_members_name_and_reaches_the_drill_down(
    served, corpus
):
    """The column is the unique attribute's value, and the members table names the same values."""
    db = served(string_ids)
    # Every table carries `paper` under its own name, so no block's `fields` moves it.
    assert 'paper = "' not in db.declaration

    answer = viewport(db, "map", FRAME)
    assert answer["counts"]["visible"] == 20
    assert browse(db, "map", "clusters")["artifacts"][0]["masked_count"] == 20

    record = item(db, answer["ids"][0])
    assert record["fields"]["paper"].startswith("p")


def test_a_delta_of_string_ids_is_ingested_and_a_second_page_of_them_edits_them(served, corpus):
    """A delta names its rows the same way, and a row naming an item the database holds edits it."""
    db = served(string_ids)
    fresh = [f"q{i}" for i in range(5)]
    db.insert("map", papers(fresh, x=25.0), x="x", y="y", access="labels")
    db.insert(
        "clusters",
        members=pa.table(
            {
                "level": pa.array([0] * len(fresh), pa.uint32()),
                "key": pa.array(["c0"] * len(fresh), pa.string()),
                "paper": pa.array(fresh, pa.string()),
            }
        ),
        key="key",
        level="level",
    )
    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {"map": 5}
    assert report.flush_wait is not None and report.flush_reached
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 25
    assert browse(db, "map", "clusters")["artifacts"][0]["masked_count"] == 25

    # The same keys again, moved a little: each row names its item and moves it.
    db.insert("map", papers(fresh, x=26.0), x="x", y="y", access="labels")
    again = db.commit()
    assert again.ok, again
    assert again.rows_accepted == {"map": 0} and again.items_edited == len(fresh), again
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 25
    assert browse(db, "map", "clusters")["artifacts"][0]["masked_count"] == 25


def unnamed(db) -> None:
    """A frame carrying no unique column: each row is an item of its own."""
    import pandas as pd

    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.insert(
        "map",
        pd.DataFrame(
            {
                "x": [float(i) for i in range(20)],
                "y": [0.0] * 20,
                "labels": [["public"]] * 20,
            }
        ),
        x="x",
        y="y",
        access="labels",
    )


def test_an_unnamed_index_is_the_tessera_id_route_and_remove_addresses_by_it(served, corpus):
    """No attribute is declared unique, and a row is addressed by the id the server hands back."""
    pytest.importorskip("pandas")
    db = served(unnamed)
    assert "unique" not in db.declaration

    answer = viewport(db, "map", FRAME)
    assert answer["counts"]["visible"] == 20
    picked = answer["ids"][0]

    report = db.remove([picked])
    assert report.ok, report
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 19
