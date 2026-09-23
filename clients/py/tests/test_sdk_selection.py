"""A selection's numbers, checked against the same numbers computed from the source table.

The notebook corpus is committed and served, and every count here is compared with a pandas
computation over `points.parquet`: the rows a reader's access terms reach, the rows a filter
matches, and the rows whose position lies inside a box.
"""

from __future__ import annotations

import numpy as np
import pytest

from tesseradb import Refusal

from test_sdk_corpus import declare_notebook

pd = pytest.importorskip("pandas")
pytest.importorskip("pyarrow")


@pytest.fixture(scope="module")
def points():
    from conftest import notebook_corpus

    frame = pd.read_parquet(notebook_corpus() / "points.parquet")
    # A row with no access label carries the view's default, which is `public`.
    frame["labels"] = [
        list(labels) if labels is not None and len(labels) else ["public"]
        for labels in frame["categories"]
    ]
    return frame


@pytest.fixture(scope="module")
def db(tmp_path_factory):
    """The notebook corpus, committed and served once for every test here that only reads it."""
    from conftest import binary, notebook_corpus
    from tesseradb._database import create

    binary()
    one = create(tmp_path_factory.mktemp("selection"))
    declare_notebook(one, notebook_corpus())
    report = one.commit()
    assert report.ok, report.output
    yield one
    one.close()


def seen_by(points, terms):
    """The rows a reader holding `terms` may see: those sharing at least one label with it."""
    held = set(terms)
    return points[[bool(held.intersection(labels)) for labels in points["labels"]]]


def clear_edge(values, near: float) -> float:
    """A coordinate near `near` halfway between two neighbouring values, so no row sits on it."""
    ordered = np.unique(values)
    at = int(np.searchsorted(ordered, near))
    return float((ordered[at - 1] + ordered[at]) / 2.0)


def box_of(points, low: float, high: float) -> tuple:
    """A box from the `low` to the `high` quantile on each axis, its edges clear of every row."""
    return (
        clear_edge(points["x"], points["x"].quantile(low)),
        clear_edge(points["y"], points["y"].quantile(low)),
        clear_edge(points["x"], points["x"].quantile(high)),
        clear_edge(points["y"], points["y"].quantile(high)),
    )


def inside(points, box) -> "pd.DataFrame":
    return points[
        (points["x"] >= box[0])
        & (points["x"] <= box[2])
        & (points["y"] >= box[1])
        & (points["y"] <= box[3])
    ]


def test_a_views_count_is_every_row_inserted_into_it(db, points):
    assert db.view("s0").count() == len(points)


def test_a_filtered_count_is_the_rows_the_filter_matches(db, points):
    expression = {"primary_category": {"in": ["cs.LG", "stat.ML"]}}
    expected = points["primary_category"].isin(["cs.LG", "stat.ML"]).sum()
    assert db.view("s0").filter(expression).count() == expected


def test_two_filters_must_both_match(db, points):
    both = db.view("s0").filter({"archive": {"eq": "cs"}}).filter(
        {"primary_category": {"eq": "cs.LG"}}
    )
    expected = ((points["archive"] == "cs") & (points["primary_category"] == "cs.LG")).sum()
    assert both.count() == expected
    assert both.filters == {
        "all_of": [{"archive": {"eq": "cs"}}, {"primary_category": {"eq": "cs.LG"}}]
    }


def test_within_counts_exactly_the_rows_inside_the_box(db, points):
    box = box_of(points, 0.3, 0.6)
    expected = len(inside(points, box))
    assert 0 < expected < len(points)
    assert db.view("s0").within(box).count() == expected


def test_a_filter_and_a_box_compose(db, points):
    box = box_of(points, 0.2, 0.8)
    expression = {"archive": {"eq": "math"}}
    expected = (inside(points, box)["archive"] == "math").sum()
    assert 0 < expected
    assert db.view("s0").filter(expression).within(box).count() == expected
    # The order they are applied in does not change the answer.
    assert db.view("s0").within(box).filter(expression).count() == expected


def test_two_boxes_count_their_overlap(db, points):
    first, second = box_of(points, 0.1, 0.6), box_of(points, 0.4, 0.9)
    narrowed = db.view("s0").within(first).within(second)
    overlap = narrowed.box
    assert overlap == (second[0], second[1], first[2], first[3])
    assert narrowed.count() == len(inside(inside(points, first), second))


@pytest.mark.parametrize("terms", [["cs.LG"], ["math.DS", "math.FA", "hep-th"]])
def test_one_selection_counts_what_each_reader_may_see(db, points, terms):
    box = box_of(points, 0.1, 0.9)
    expression = {"archive": {"in": ["cs", "math", "hep-th"]}}
    seen = seen_by(inside(points, box), terms)
    expected = seen["archive"].isin(["cs", "math", "hep-th"]).sum()
    assert 0 < expected
    assert db.viewer(terms).view("s0").filter(expression).within(box).count() == expected


def test_two_readers_of_one_selection_get_different_counts(db, points):
    narrow = db.viewer(["cs.LG"]).view("s0").count()
    broad = db.viewer(["cs.LG", "math.DS"]).view("s0").count()
    assert narrow == len(seen_by(points, ["cs.LG"]))
    assert broad == len(seen_by(points, ["cs.LG", "math.DS"]))
    assert narrow < broad < db.view("s0").count()


def test_two_boxes_that_do_not_overlap_select_nothing(db, points):
    apart = db.view("s0").within(box_of(points, 0.1, 0.3)).within(box_of(points, 0.6, 0.9))
    assert apart.box is None
    assert apart.count() == 0


def test_a_selection_is_unchanged_by_the_selections_made_from_it(db):
    papers = db.view("s0")
    papers.filter({"archive": {"eq": "cs"}})
    papers.within((0.0, 0.0, 1.0, 1.0))
    assert papers.filters is None and papers.box is None


def test_an_unknown_view_is_refused(db):
    with pytest.raises(Refusal):
        db.view("no-such-view")
    with pytest.raises(Refusal):
        db.viewer(["cs.LG"]).view("no-such-view")


def test_a_box_given_backwards_is_refused(db):
    with pytest.raises(Refusal):
        db.view("s0").within((10.0, 0.0, 0.0, 10.0))


def test_a_sample_is_bounded_by_k_and_carries_the_counts_it_came_from(db, points):
    import json

    box = box_of(points, 0.2, 0.8)
    sample = db.view("s0").within(box).sample(zoom=2, k=16)
    counts = json.loads(sample.schema.metadata[b"tessera.counts"])
    assert 0 < sample.num_rows == counts["served"] < counts["matched"]
    assert counts["matched"] == len(inside(points, box))
    sent = json.loads(sample.schema.metadata[b"tessera.request"])
    assert sent["bbox"] == list(box) and sent["zoom"] == 2 and sent["k"] == 16


def test_categories_lists_every_value_of_a_vocabulary(db):
    from conftest import notebook_corpus

    vocabulary = pd.read_parquet(notebook_corpus() / "primary_category.parquet")
    listed = db.categories("primary_category")
    assert list(listed.columns) == ["key", "code", "title"]
    assert sorted(listed["key"]) == sorted(vocabulary["key"])
    assert dict(zip(listed["key"], listed["code"])) == dict(
        zip(vocabulary["key"], vocabulary["code"])
    )


def test_categories_resolves_the_codes_it_is_given(db):
    listed = db.categories("primary_category")
    held = listed.iloc[[7, 0, 3]]
    found = db.categories("primary_category", codes=list(held["code"]) + [65000])
    assert list(found.columns) == ["key", "code", "title"]
    assert found.to_dict("records") == held.sort_values("key").to_dict("records")
    assert db.categories("primary_category", codes=[]).empty
    with pytest.raises(Refusal):
        db.categories("primary_category", codes=[1], prefix="cs")


def test_categories_with_a_prefix_counts_the_items_carrying_each_value(db, points):
    found = db.categories("archive", prefix="ma")
    assert list(found.columns) == ["key", "code", "title", "count"]
    assert "math" in set(found["key"])
    carried = points["archive"].value_counts()
    for key, count in zip(found["key"], found["count"]):
        assert count == carried.get(key, 0)


def test_categories_with_a_prefix_counts_what_the_reader_may_see(db, points):
    found = db.viewer(["cs.LG"]).categories("archive", prefix="c")
    seen = seen_by(points, ["cs.LG"])["archive"].value_counts()
    assert dict(zip(found["key"], found["count"])) == {
        key: seen.get(key, 0) for key in found["key"]
    }


def test_a_map_opens_on_the_selections_view_filters_and_box(db, points, stub_bundle):
    box = box_of(points, 0.2, 0.8)
    expression = {"archive": {"eq": "cs"}}
    m = db.view("s0").filter(expression).within(box).map(
        colour_by="primary_category", layers=[], height=320
    )
    assert m.view == "s0"
    assert m.filters == expression
    assert m.bbox == list(box)
    assert m.colour_by == "primary_category" and m.layers == [] and m.height == 320


def test_a_box_counted_over_a_cover_counts_at_least_the_items_inside(served, corpus, points):
    """A server whose outline budget is one cell counts a box over the cells covering it."""
    from tesseradb import open as reopen

    db = served(lambda one: declare_notebook(one, corpus))
    box = box_of(points, 0.3, 0.6)
    exact = len(inside(points, box))
    path = db.path
    db.close()
    deployment = path / "tessera.toml"
    deployment.write_text(
        deployment.read_text().replace("[serve]\n", "[serve]\nmax_region_cells = 1\n")
    )
    again = reopen(path)
    try:
        count = again.view("s0").within(box).count()
        assert type(count) is int and count >= exact
    finally:
        again.close()
