"""What each call returns: a report that shows as a short summary, and no call prints.

Each report kind is checked for the numbers that matter in its summary, and for every finding
and refusal it holds. The wording is not asserted.
"""

import pyarrow as pa
import pytest

from tesseradb._database import create
from tesseradb._refusal import Refusal


def papers(ids, x=None, labels="public"):
    n = len(ids)
    return pa.table(
        {
            "id": pa.array(ids, pa.string()),
            "x": pa.array(x if x is not None else [float(i) for i in range(n)], pa.float64()),
            "y": pa.array([0.0] * n, pa.float64()),
            "labels": pa.array([[labels]] * n, pa.list_(pa.string())),
            "note": pa.array(["n"] * n, pa.string()),
        }
    )


def small(db) -> None:
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_attribute("score", type="f64", index=True, render=False)
    db.insert("map", papers([f"p{i}" for i in range(20)]), id="id", x="x", y="y",
              access="labels")
    db.insert(
        "score",
        pa.table({"id": pa.array(["p0"], pa.string()), "score": pa.array([0.5], pa.float64())}),
        id="id",
        value="score",
    )


def test_declare_columns_reports_every_column_and_prints_nothing(tmp_path, capsys):
    db = create(tmp_path / "db")
    db.declare_view("map")
    frame = pa.table({"id": ["p"], "x": [0.0], "y": [0.0], "title": ["t"], "year": [2001]})
    report = db.declare_columns(frame, skip=["id", "x", "y"], index=["title"])
    summary = str(report)
    assert str(len(report.columns)) in summary
    assert all(column.name in summary for column in report.columns)
    assert repr(report) == summary
    assert capsys.readouterr().out == ""


def test_an_insert_reports_its_rows_and_columns_and_prints_nothing(tmp_path, capsys):
    db = create(tmp_path / "db")
    db.declare_view("map")
    insert = db.insert("map", papers([f"p{i}" for i in range(1500)]), id="id", x="x", y="y")
    summary = str(insert)
    assert insert.rows == 1500 and f"{insert.rows:,}" in summary
    assert all(column in summary for column in insert.read)
    assert insert.ignored == ["labels", "note"]
    assert all(column in summary for column in insert.ignored)
    assert repr(insert) == summary
    assert capsys.readouterr().out == ""


def test_a_check_before_the_first_commit_reports_its_rows_and_every_finding(tmp_path, capsys):
    db = create(tmp_path / "db")
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.insert("map", pa.table({"id": pa.array(["p", None], pa.string()), "x": [0.0, 1.0],
                               "y": [0.0, 0.0]}), id="id", x="x", y="y")
    report = db.check()
    summary = str(report)
    assert not report.ok and report.findings
    assert all(str(finding) in summary for finding in report.findings)
    assert report.rows == {"map": 2} and "2 rows" in summary
    assert capsys.readouterr().out == ""


def test_a_check_names_a_declared_column_nothing_fills_and_the_commit_shows_its_failure(
    tmp_path, capsys
):
    db = create(tmp_path / "db")
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_attribute("score", type="f64", index=True)
    db.insert("map", papers(["p0", "p1"]), id="id", x="x", y="y", access="labels")
    report = db.check()
    empty = [note for note in report.notes if "score" in note and "empty" in note]
    assert empty and all(note in str(report) for note in empty)
    with pytest.raises(Refusal) as raised:
        db.commit()
    failed = raised.value.report
    assert not failed.ok
    refused = [line for line in failed.log.splitlines() if "refused" in line]
    assert refused and all(line in str(failed) for line in refused)
    assert capsys.readouterr().out == ""


def test_the_first_commit_reports_what_it_built_and_prints_nothing(tmp_path, corpus, capsys):
    db = create(tmp_path / "db")
    try:
        small(db)
        report = db.commit()
        summary = str(report)
        assert report.ok, report.log
        assert report.rows == {"map": 20} and "20 rows" in summary
        assert report.items == 20 and "20 items" in summary
        assert report.views == {"map": 1} and "map" in summary
        assert report.layers == []
        assert report.seconds is not None and f"{report.seconds:.1f}" in summary
        assert report.viewer in summary
        # The build's log stays on the report.
        assert "built" in report.log
        assert capsys.readouterr().out == ""
    finally:
        db.close()


def test_a_later_check_and_commit_report_their_plan_findings_and_refusals(served, corpus, capsys):
    db = served(small)
    capsys.readouterr()
    # A row outside the view's frame is a finding, and the check and the commit both carry it.
    db.insert("map", papers(["far"], x=[9_000.0]), id="id", x="x", y="y", access="labels")
    plan = db.check()
    assert plan.findings and all(str(finding) in str(plan) for finding in plan.findings)
    assert str(len(plan.plan)) in str(plan)
    with pytest.raises(Refusal) as raised:
        db.commit()
    refused = raised.value.report
    assert all(str(finding) in str(refused) for finding in refused.findings)

    # `p0` holds 0.5, so another value on it is refused, and the two new rows land. The refused
    # commit left its rows pending, so this is a second database.
    db = served(small)
    capsys.readouterr()
    db.insert(
        "score",
        pa.table({"id": pa.array(["p0"], pa.string()), "score": pa.array([9.0], pa.float64())}),
        id="id",
        value="score",
    )
    db.insert("map", papers(["q0", "q1"], x=[1.5, 2.5]), id="id", x="x", y="y", access="labels")
    report = db.commit()
    summary = str(report)
    assert report.rows_accepted == {"map": 2} and "2 rows to map" in summary
    assert report.refusals and all(
        str(refusal["status"]) in summary and refusal["detail"] in summary
        for refusal in report.refusals
    )
    assert f"{report.flush_wait:.2f}" in summary
    assert capsys.readouterr().out == ""


def test_a_change_reports_how_many_ids_it_was_given(served, corpus, capsys):
    db = served(small)
    capsys.readouterr()
    for report in (db.suppress(["p1", "p2", "p3"]), db.unsuppress(["p1", "p2", "p3"])):
        assert report.ok and report.requested == 3 and "3 ids" in str(report)
    assert capsys.readouterr().out == ""
