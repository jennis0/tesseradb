"""A categorical column, from pandas or as an Arrow dictionary, read as the strings it holds.

Each target that reads a string column (a category attribute's values, a layer's key, a view's
access labels) is given a categorical one, at the first commit and at a later one, and what is
served is compared with what the same strings would give. `declare_columns` declares a
categorical column as a category.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

pd = pytest.importorskip("pandas")

VENUES = ["neurips", "icml", "iclr"]
LABELS = ["open", "closed"]


def papers(first: int, count: int, dictionary: bool) -> pa.Table:
    """Papers `p<first>…`, their venue, cluster and label cycling, as categorical columns.

    `dictionary` builds the Arrow dictionary columns directly; otherwise the frame is pandas with
    `Categorical` columns, which reaches Arrow as dictionary columns too.
    """
    ids = [f"p{i}" for i in range(first, first + count)]
    venue = [VENUES[i % 3] for i in range(first, first + count)]
    cluster = [f"c{i % 2}" for i in range(first, first + count)]
    label = [LABELS[i % 2] for i in range(first, first + count)]
    if dictionary:
        return pa.table(
            {
                "id": pa.array(ids),
                "x": pa.array([float(i % 30) for i in range(first, first + count)]),
                "y": pa.array([1.0] * count),
                "venue": pa.array(venue).dictionary_encode(),
                "cluster": pa.array(cluster).dictionary_encode(),
                "label": pa.array(label).dictionary_encode(),
            }
        )
    return pa.Table.from_pandas(
        pd.DataFrame(
            {
                "id": ids,
                "x": [float(i % 30) for i in range(first, first + count)],
                "y": [1.0] * count,
                "venue": pd.Categorical(venue),
                "cluster": pd.Categorical(cluster),
                "label": pd.Categorical(label),
            }
        ),
        preserve_index=False,
    )


def expected(first: int, count: int) -> dict:
    rows = range(first, first + count)
    return {
        "neurips": sum(1 for i in rows if VENUES[i % 3] == "neurips"),
        "open": sum(1 for i in rows if LABELS[i % 2] == "open"),
        "c0": sum(1 for i in rows if i % 2 == 0),
    }


def declare(db, frame: pa.Table) -> None:
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]}, default_label=None)
    db.declare_vocabulary("venue")
    db.declare_attribute("venue", type="category", vocabulary="venue", index=True)
    db.declare_layer("clusters", kind="flat")
    insert(db, frame)


def insert(db, frame: pa.Table) -> None:
    db.insert("map", frame, id="id", x="x", y="y", access="label")
    db.insert("clusters", frame, id="id", key="cluster")


def served_counts(db) -> dict:
    papers = db.view("map")
    clusters = {
        row["key"]: row["masked_count"]
        for row in db.viewer().browse_artifacts("map", "clusters")["artifacts"]
    }
    return {
        "neurips": papers.filter({"venue": {"eq": "neurips"}}).count(),
        "open": db.viewer(["open"]).view("map").count(),
        "c0": clusters["c0"],
    }


@pytest.mark.parametrize("dictionary", [False, True], ids=["pandas", "arrow"])
def test_a_categorical_column_is_read_as_its_strings_at_each_commit(served, corpus, dictionary):
    db = served(lambda one: declare(one, papers(0, 30, dictionary)))
    assert sorted(db.categories("venue").column("key").to_pylist()) == sorted(VENUES)
    assert served_counts(db) == expected(0, 30)

    insert(db, papers(30, 12, dictionary))
    report = db.commit()
    assert report.ok, report
    assert served_counts(db) == expected(0, 42)


def test_declare_columns_declares_a_categorical_column_a_category(served, corpus):
    frame = papers(0, 30, dictionary=False)

    def declare_from_the_frame(db):
        db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]}, default_label=None)
        db.declare_columns(frame, skip=["id", "x", "y", "cluster", "label"], index=["venue"])
        db.insert("map", frame, id="id", x="x", y="y", access="label")

    db = served(declare_from_the_frame)
    assert sorted(db.categories("venue").column("key").to_pylist()) == sorted(VENUES)
    assert db.view("map").filter({"venue": {"eq": "icml"}}).count() == 10


def test_a_pandas_string_frame_inserted_after_the_first_commit_lands(served, corpus):
    """pandas 3 makes every string column a large string, lists of strings included."""
    def strings(first: int, count: int) -> pa.Table:
        rows = range(first, first + count)
        return pa.Table.from_pandas(
            pd.DataFrame(
                {
                    "id": [f"p{i}" for i in rows],
                    "x": [float(i % 30) for i in rows],
                    "y": [1.0] * count,
                    "venue": [VENUES[i % 3] for i in rows],
                    "cluster": [f"c{i % 2}" for i in rows],
                    "label": [[LABELS[i % 2]] for i in rows],
                    "doi": [f"10.1/{i}" for i in rows],
                }
            ),
            preserve_index=False,
        )

    def first_commit(db):
        declare(db, strings(0, 30))
        db.declare_attribute("doi", type="keyword", index=True)
        db.insert("doi", strings(0, 30), id="id", value="doi")

    later = strings(30, 12)
    assert pa.types.is_large_string(later.schema.field("id").type)
    db = served(first_commit)
    insert(db, later)
    db.insert("doi", later, id="id", value="doi")
    report = db.commit()
    assert report.ok, report
    assert served_counts(db) == expected(0, 42)
    assert db.view("map").filter({"doi": {"eq": "10.1/40"}}).count() == 1
