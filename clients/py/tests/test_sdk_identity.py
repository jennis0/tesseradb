"""How a row is named, from the frame to the served answer (python-sdk.md §3).

Two routes and no third. A source names its rows by an id column, whose bytes are the external id
at the build and on every later route; or it names them by nothing, and a row is addressable by
the `tessera_id` the server hands back. The SDK keeps no map between them: what is staged is what
is written, and what is sent is what the column holds.
"""

from __future__ import annotations

import base64

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
    db.stage("points", papers(keys), id="paper", default=True)
    db.stage(
        "clusters",
        pa.table({"level": pa.array([0], pa.uint32()), "key": pa.array(["c0"], pa.string())}),
    )
    db.stage(
        "members",
        pa.table(
            {
                "level": pa.array([0] * len(keys), pa.uint32()),
                "key": pa.array(["c0"] * len(keys), pa.string()),
                "paper": pa.array(keys, pa.string()),
            }
        ),
        id="paper",
    )
    db.declare_view("map", source="points", access="labels", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_layer("clusters", kind="flat", source="clusters", members="members")


def test_a_string_id_column_names_the_rows_the_members_name_and_reaches_the_drill_down(
    served, corpus
):
    """§3: the column's bytes are the external id, and the members table joins on the same bytes."""
    db = served(string_ids)
    # The declaration is what says where identity is; nothing was rewritten to say it.
    assert 'entity_id = "paper"' in db.declaration
    assert 'entity = "paper"' in db.declaration

    answer = viewport(db, "map", FRAME)
    assert answer["counts"]["visible"] == 20
    assert browse(db, "map", "clusters")["artifacts"][0]["masked_count"] == 20

    record = item(db, answer["ids"][0])
    assert base64.b64decode(record["external_id"]).decode().startswith("p")


def test_a_delta_of_string_ids_is_ingested_and_a_second_page_of_them_is_refused(served, corpus):
    """A delta names its rows the same way, and a row the database holds is a `409` per page."""
    db = served(string_ids)
    fresh = [f"q{i}" for i in range(5)]
    db.stage("points", papers(fresh, x=25.0), id="paper")
    db.stage(
        "members",
        pa.table(
            {
                "level": pa.array([0] * len(fresh), pa.uint32()),
                "key": pa.array(["c0"] * len(fresh), pa.string()),
                "paper": pa.array(fresh, pa.string()),
            }
        ),
        id="paper",
    )
    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {"map": 5}
    assert report.flush_wait is not None and report.flush_reached
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 25
    assert browse(db, "map", "clusters")["artifacts"][0]["masked_count"] == 25

    # The same keys again, moved a little so the bytes are a batch the server has not replayed.
    db.stage("points", papers(fresh, x=26.0), id="paper")
    again = db.commit()
    assert not again.ok
    assert [r["status"] for r in again.refusals] == [409]
    # The refusal names the ids it read, base64 as every external id on this plane is.
    assert base64.b64encode(b"q0").decode() in again.refusals[0]["detail"]
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 25


def unnamed(db) -> None:
    """A frame whose index names nothing: the Tessera-id route (§3)."""
    import pandas as pd

    db.stage(
        "points",
        pd.DataFrame(
            {
                "x": [float(i) for i in range(20)],
                "y": [0.0] * 20,
                "labels": [["public"]] * 20,
            }
        ),
        default=True,
    )
    db.declare_view("map", source="points", access="labels", extent={"x": [-5, 40], "y": [-5, 40]})


@pytest.mark.xfail(
    strict=True,
    reason="`tessera check` requires a view's `entity_id` column unconditionally "
    "(tessera-build check.rs, the `require(ENTITY_ID)` at the view), while the build accepts a "
    "points file that carries none and takes the positional route (configuration.md §8). The SDK "
    "runs check before build, so the Tessera-id route cannot be committed through it",
)
def test_an_unnamed_index_is_the_tessera_id_route_and_remove_addresses_by_it(served, corpus):
    """§3: no external id is written, and a row is addressed by the id the server hands back."""
    pytest.importorskip("pandas")
    db = served(unnamed)
    entities = db.path / "bundle" / "v00000" / "partitions" / "default" / "entities"
    assert not list(entities.glob("external-ids-*.arrow"))
    assert not (entities / "ext-locator.u32").exists()

    answer = viewport(db, "map", FRAME)
    assert answer["counts"]["visible"] == 20
    picked = answer["ids"][0]
    assert "external_id" not in item(db, picked)

    report = db.remove([picked])
    assert report.ok, report
    assert viewport(db, "map", FRAME)["counts"]["visible"] == 19
