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
#: `entity_id`, so these are the ids the inserted table carries.
HELD = list(range(1, 41))


def notebook(served, corpus):
    return served(lambda db: declare_notebook(db, corpus))


def fill(db, column: str, values) -> None:
    """An insert into the attribute itself: the ids it fills, and the column the values are in."""
    db.insert(
        column,
        pa.table({"entity_id": pa.array(HELD, pa.uint64()), column: values}),
        id="entity_id",
        value=column,
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
    assert any(line.startswith("values into 'citations'") for line in plan.plan[1:])

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
    # The vocabulary goes before the attribute that names it, and both before the values page. A
    # closed set is refused at the route with no values, so its three travel on the declaration
    # and no page follows them.
    assert plan.plan[0] == "declare vocabulary 'venue' (closed)"
    assert plan.plan[1] == "declare attribute 'venue' (category)"
    assert not [line for line in plan.plan if line.startswith("page ")]

    report = db.commit()
    assert report.ok, report
    assert report.values_filled == len(HELD)
    assert report.values_bound == 3

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
    """A closed set declared at a running service carries its keys inline (§3, §4.4).

    The route refuses a closed value set declared with no values, so the keys the insert carries
    travel in the declaration and the same values follow with their titles, through the `PATCH`
    an inline set's page takes.
    """
    db = notebook(served, corpus)
    db.declare_vocabulary("venue", closed=True, width="u8", title="Venue")
    db.insert(
        "venue",
        pa.table(
            {
                "key": pa.array(["neurips", "icml", "iclr"], pa.string()),
                "title": pa.array(["NeurIPS", "ICML", "ICLR"], pa.string()),
            }
        ),
        key="key",
        title="title",
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
    assert report.values_bound == 3

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
    # A second insert into the same column: it is held now, so only the values page goes.
    fill(db, "citations", pa.array([2] * len(HELD), pa.uint32()))
    plan = db.check()
    assert [line for line in plan.plan if line.startswith("declare")] == []


def test_an_open_vocabulary_declared_after_the_first_commit_pages_its_titles(served, corpus):
    """An open set is declared with no values and its titles are the page (§4.4).

    An open value set mints a code for each key that arrives, so nothing has to travel with the
    declaration. The titles do: a key bound by an ingest has no title, and `PATCH
    /control/vocabularies/{name}/values` is where one is given.
    """
    db = notebook(served, corpus)
    db.declare_vocabulary("venue", width="u8", title="Venue")
    db.insert(
        "venue",
        pa.table(
            {
                "key": pa.array(["neurips", "icml"], pa.string()),
                "title": pa.array(["NeurIPS", "ICML"], pa.string()),
            }
        ),
        key="key",
        title="title",
    )
    db.declare_attribute("venue", type="category", vocabulary="venue", index=True)
    fill(db, "venue", pa.array(["neurips"] * len(HELD), pa.string()))

    plan = db.check()
    assert plan.plan[:3] == [
        "declare vocabulary 'venue' (open)",
        "page 2 value(s) into vocabulary 'venue'",
        "declare attribute 'venue' (category)",
    ]
    report = db.commit()
    assert report.ok, report
    assert report.values_bound == 2
    assert {one["key"]: one.get("title") for one in categories(db, "venue")["values"]} == {
        "neurips": "NeurIPS",
        "icml": "ICML",
    }


def test_a_value_set_over_the_bodys_cap_is_paged_by_bytes(served, corpus):
    """The route's unit is bytes, so long titles decide the page (§6.2 step 1).

    `limits.declarations.max_body_bytes` is 2 MiB and the row figure is 10,000, so 600 values with
    a 6 KB title each are one row page and several body pages. What is measured is what is sent.
    """
    db = notebook(served, corpus)
    keys = [f"v{i:04d}" for i in range(600)]
    db.declare_vocabulary("venue", width="u16", title="Venue")
    db.insert(
        "venue",
        pa.table(
            {
                "key": pa.array(keys, pa.string()),
                "title": pa.array([f"{key} " + "long " * 1200 for key in keys], pa.string()),
            }
        ),
        key="key",
        title="title",
    )
    db.declare_attribute("venue", type="category", vocabulary="venue", index=True)
    fill(db, "venue", pa.array([keys[i % len(keys)] for i in range(len(HELD))], pa.string()))

    pages = [line for line in db.check().plan if line.startswith("page ")]
    assert len(pages) > 1, pages
    assert sum(int(line.split()[1]) for line in pages) == len(keys)

    report = db.commit()
    assert report.ok, report
    assert report.values_bound == len(keys)
    listed = categories(db, "venue", limit=1000)["values"]
    assert len(listed) == len(keys)


def test_a_vocabulary_no_column_names_is_redeclared_and_answered_as_held(served, corpus):
    """`/v1/meta` publishes a value set through the column that reads it (§6.2 step 1).

    A vocabulary no attribute names yet is not on that document, so the next commit declares it
    again. The route answers an identical redeclaration as held and applies nothing, which the
    report counts under the parts already present rather than as a value bound.
    """
    db = notebook(served, corpus)
    db.declare_vocabulary("venue", values=["neurips", "icml"], closed=True, width="u8")
    first = db.commit()
    assert first.ok, first
    assert first.values_bound == 2 and first.already_present == 0

    again = db.commit()
    assert again.ok, again
    assert again.plan[0] == "declare vocabulary 'venue' (closed)"
    assert again.values_bound == 0
    assert again.already_present == 1

    # Named by a column, it is held: the declaration is not sent a third time.
    db.declare_attribute("venue", type="category", vocabulary="venue", index=True)
    assert db.commit().ok
    assert [line for line in db.check().plan if "vocabulary" in line] == []
