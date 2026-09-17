"""An attribute and a vocabulary declared after the first commit (python-sdk.md §4.4, §4.5, §6.2).

Every test here commits the notebook corpus, declares a column on the running service and reads
the answer back through the viewer plane. The routes are `PUT /control/attributes`,
`PUT /control/vocabularies/{name}` and `PATCH /control/vocabularies/{name}/values`; the values
arrive on `POST /control/values` as any values delta does, so what is under test is the plan's
order and the served answer, not a second reading of the contract.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

from conftest import categories, viewport
from tesseradb._refusal import Refusal

from test_sdk_corpus import declare_notebook
from test_sdk_pages import whole_frame

pytest.importorskip("pyarrow")

#: Entities the build wrote, which the values route fills. The notebook corpus names its rows by
#: `entity_id`, so these are the ids the staged delta carries.
HELD = list(range(1, 41))


def notebook(served, corpus):
    return served(lambda db: declare_notebook(db, corpus))


def fill(db, column: str, values) -> None:
    """A delta on the points source carrying ids and one column: no coordinates, so values."""
    db.stage(
        "points",
        pa.table({"entity_id": pa.array(HELD, pa.uint64()), column: values}),
    )


def matched(db, filters: dict) -> int:
    return viewport(db, "s0", whole_frame(db), filters=filters)["counts"]["matched"]


def declared(db, column: str) -> dict:
    return next(one for one in db.meta()["declared_scalars"] if one["name"] == column)


# ---------------------------------------------------------------------------- an indexed column


def test_an_indexed_attribute_declared_after_the_first_commit_is_filled_and_filters(
    served, corpus
):
    """`PUT /control/attributes`, then the values that fill it (§6.2 steps 1 and 3)."""
    db = notebook(served, corpus)
    db.declare_attribute("citations", type="u32", index=True, title="Citations")
    fill(db, "citations", pa.array([10 * (i % 4) for i in range(len(HELD))], pa.uint32()))

    plan = db.check()
    assert plan.ok, plan
    # The declaration is step 1 and the values page is step 3: the column exists for resolution
    # from the answer, so the page that fills it may name it.
    assert plan.plan[0] == "declare attribute 'citations' (u32)"
    assert any(line.startswith("values on existing entities") for line in plan.plan[1:])

    report = db.commit()
    assert report.ok, report
    assert report.values_filled == len(HELD)

    # `/v1/meta` lists the new column with the placement it was declared with.
    assert declared(db, "citations")["index"] is True
    assert declared(db, "citations")["render"] is False

    # The filter surface answers over it, inside the mask: ten of the forty rows took each value.
    assert matched(db, {"citations": {"range": {"gte": 30}}}) == len(
        [one for one in range(len(HELD)) if 10 * (one % 4) >= 30]
    )
    # An entity that predates the declaration reads absent rather than zero, so it matches nothing.
    assert matched(db, {"citations": {"range": {"gte": 0}}}) == len(HELD)


def test_a_render_column_declared_after_the_first_commit_is_refused_at_the_verb(served, corpus):
    """Decision 0136's amendment: `PUT /control/attributes` refuses `render` whatever the type.

    A rendered value is served from the hot column of the row that carries it, and the route
    declares a column against entities that already exist. The refusal is at the verb, where the
    declaration is still the user's to change, and it names the first commit.
    """
    db = notebook(served, corpus)
    with pytest.raises(Refusal, match="render=True is fixed at the first commit"):
        db.declare_attribute("hotness", type="u8", render=True)
    # Nothing was declared, so the next commit has nothing to send.
    assert db.check().plan == []


# ---------------------------------------------------------------------------- a category


def test_a_category_over_an_inline_closed_vocabulary_is_declared_filled_and_listed(
    served, corpus
):
    """A closed value set given inline, its attribute, and the values the two carry (§4.4)."""
    db = notebook(served, corpus)
    db.declare_vocabulary(
        "venue", values=["neurips", "icml", "iclr"], closed=True, width="u8", title="Venue"
    )
    db.declare_attribute(
        "venue", type="category", vocabulary="venue", index=True, title="Venue"
    )
    keys = ["neurips", "icml", "iclr"]
    fill(db, "venue", pa.array([keys[i % 3] for i in range(len(HELD))], pa.string()))

    plan = db.check()
    assert plan.ok, plan
    # The vocabulary goes before the attribute that names it, and both before the values page.
    assert plan.plan[0] == "declare vocabulary 'venue' (closed)"
    assert plan.plan[1] == "page 3 value(s) into vocabulary 'venue'"
    assert plan.plan[2] == "declare attribute 'venue' (category)"

    report = db.commit()
    assert report.ok, report
    assert report.values_filled == len(HELD)

    # `/v1/meta` names the vocabulary the category reads and carries none of its values.
    assert declared(db, "venue")["category"]["vocabulary"] == "venue"
    assert declared(db, "venue")["category"]["kind"] == "declared"

    # `/v1/categories/{column}` is where the values are, codes and all.
    listed = categories(db, "venue")["values"]
    assert sorted(one["key"] for one in listed) == sorted(keys)
    assert matched(db, {"venue": {"eq": "neurips"}}) == len(
        [i for i in range(len(HELD)) if keys[i % 3] == "neurips"]
    )


def test_a_category_over_a_sourced_closed_vocabulary_pages_the_tables_rows(served, corpus):
    """The emitter reports `values_source` rather than rows, so the SDK reads the table (§4.4).

    A sourced value set's keys are rows and never travel in a declaration payload, so the pages
    are the SDK's: `(key, title?)` from the staged table, through the same `PATCH` an inline set's
    page takes.
    """
    db = notebook(served, corpus)
    # The declaration comes first: a delta names a source some block of the declaration reads.
    db.declare_vocabulary("venue", source="venues", closed=True, width="u8", title="Venue")
    db.stage(
        "venues",
        pa.table(
            {
                "key": pa.array(["neurips", "icml", "iclr"], pa.string()),
                "title": pa.array(["NeurIPS", "ICML", "ICLR"], pa.string()),
            }
        ),
    )
    db.declare_attribute("venue", type="category", vocabulary="venue", index=True)
    keys = ["neurips", "icml", "iclr"]
    fill(db, "venue", pa.array([keys[i % 3] for i in range(len(HELD))], pa.string()))

    report = db.commit()
    assert report.ok, report
    assert report.plan[:3] == [
        "declare vocabulary 'venue' (closed)",
        "page 3 value(s) into vocabulary 'venue'",
        "declare attribute 'venue' (category)",
    ]
    assert report.values_filled == len(HELD)

    # The titles came from the table's own column, which is what a sourced set is for.
    listed = {one["key"]: one.get("title") for one in categories(db, "venue")["values"]}
    assert listed == {"neurips": "NeurIPS", "icml": "ICML", "iclr": "ICLR"}
    assert matched(db, {"venue": {"eq": "icml"}}) == len(
        [i for i in range(len(HELD)) if keys[i % 3] == "icml"]
    )


def test_a_declaration_the_database_already_holds_is_not_sent_again(served, corpus):
    """Which declarations are new is read from `/v1/meta` (§6.2 step 1).

    The corpus's own columns and value sets are there from the build, so a commit that declares
    nothing new sends no declaration page for them.
    """
    db = notebook(served, corpus)
    db.declare_attribute("citations", type="u32", index=True)
    fill(db, "citations", pa.array([1] * len(HELD), pa.uint32()))
    assert db.commit().ok
    # A second delta on the same column: the attribute is held now, so only the values page goes.
    fill(db, "citations", pa.array([2] * len(HELD), pa.uint32()))
    plan = db.check()
    assert [line for line in plan.plan if line.startswith("declare")] == []
