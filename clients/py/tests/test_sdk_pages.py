"""The paged commit, against a real served database (python-sdk.md §6, §10.3, §10.4).

Every test here commits a database, inserts into it again and reads the answer back through the
viewer plane. What is under test is that the SDK's pages are what the control plane takes, so
nothing here doubles the server: a fake control plane would check the SDK against the SDK's own
reading of the contract.
"""

from __future__ import annotations

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from conftest import browse, item, viewport
from tesseradb._refusal import Refusal

from test_sdk_corpus import declare_notebook

pytest.importorskip("pyarrow")

#: Where the new papers land. The notebook corpus's frame is fitted to its own rows and widened,
#: so a point at the corpus's centre is inside it whatever the fit produced.
#: Sixty of them: `clusters/kmeans` declares `require_member_visibility = {count: 50}`, so a
#: cluster with fewer members exists for nobody and the test would be reading a gate rather than a
#: commit.
NEW_IDS = list(range(900_001, 900_061))


def centre(db) -> tuple[float, float]:
    frame = db.meta()["views"][0]["quantisation"]
    return (
        (frame["x_min"] + frame["x_max"]) / 2.0,
        (frame["y_min"] + frame["y_max"]) / 2.0,
    )


def whole_frame(db, view: str = "s0") -> list[float]:
    frame = next(v for v in db.meta()["views"] if v["id"] == view)["quantisation"]
    return [frame["x_min"], frame["y_min"], frame["x_max"], frame["y_max"]]


def new_papers(db, ids=None, x_offset: float = 0.0) -> pa.Table:
    """A week of new papers, as the frame §10.3 inserts."""
    ids = list(NEW_IDS if ids is None else ids)
    x, y = centre(db)
    return pa.table(
        {
            "entity_id": pa.array(ids, pa.uint64()),
            "x": pa.array([x + x_offset + i * 0.001 for i in range(len(ids))], pa.float64()),
            "y": pa.array([y + i * 0.001 for i in range(len(ids))], pa.float64()),
            "categories": pa.array([["cs.LG"] for _ in ids], pa.list_(pa.string())),
            "arxiv_id": pa.array([f"2609.{i:05d}" for i in ids], pa.string()),
            "archive": pa.array(["cs" for _ in ids], pa.string()),
            "primary_category": pa.array(["cs.LG" for _ in ids], pa.string()),
            "submitted_at": pa.array([1_757_000_000_000_000 + i for i in ids], pa.timestamp("us")),
            "title": pa.array([f"Diffusion models for audio, part {i}" for i in ids], pa.string()),
            "abstract": pa.array([f"An abstract about audio diffusion, {i}." for i in ids],
                                 pa.string()),
        }
    )


def insert_the_new_papers(db, ids=None, x_offset: float = 0.0) -> None:
    """The allocation view's own insert: geometry, labels, the id, and the columns by name."""
    db.insert(
        "s0",
        new_papers(db, ids, x_offset),
        id="entity_id",
        x="x",
        y="y",
        access="categories",
    )


def insert_the_new_cluster(db, ids=None, key: str = "k-new", label: str = "Audio diffusion"):
    """§10.3's other four inserts: a cluster, its members, its label and its generating set."""
    ids = list(NEW_IDS if ids is None else ids)
    db.insert(
        "clusters/kmeans",
        artifacts=pa.table(
            {
                "level": pa.array([0], pa.uint32()),
                "key": pa.array([key], pa.string()),
            }
        ),
        key="key",
        level="level",
    )
    db.insert(
        "clusters/kmeans",
        members=pa.table(
            {
                "level": pa.array([0] * len(ids), pa.uint32()),
                "key": pa.array([key] * len(ids), pa.string()),
                "entity": pa.array(ids, pa.uint64()),
            }
        ),
        id="entity",
        key="key",
        level="level",
    )
    db.insert(
        "topics/kmeans",
        pa.table(
            {
                "level": pa.array([0], pa.uint32()),
                "key": pa.array([f"{key}-label"], pa.string()),
                "contents": pa.array([[[label]]], pa.list_(pa.list_(pa.string()))),
                "attached_layer": pa.array(["clusters/kmeans"], pa.string()),
                "attached_key": pa.array([key], pa.string()),
            }
        ),
        key="key",
        level="level",
        contents="contents",
        attached_layer="attached_layer",
        attached_key="attached_key",
    )
    # A label's member table carries two grains: a null rank is the membership, and rank *k* is
    # content *k*'s generating set (annotation-write-cycle §6.1).
    db.insert(
        "topics/kmeans",
        members=pa.table(
            {
                "level": pa.array([0] * (2 * len(ids)), pa.uint32()),
                "key": pa.array([f"{key}-label"] * (2 * len(ids)), pa.string()),
                "rank": pa.array([None] * len(ids) + [0] * len(ids), pa.uint32()),
                "entity": pa.array(ids + ids, pa.uint64()),
            }
        ),
        id="entity",
        key="key",
        level="level",
        rank="rank",
    )


def notebook(served, corpus):
    return served(lambda db: declare_notebook(db, corpus))


# ---------------------------------------------------------------------------- §10.3


def test_new_papers_with_a_cluster_and_a_label_are_served(served, corpus):
    """python-sdk.md §10.3, through to the served answer."""
    db = notebook(served, corpus)
    before = viewport(db, "s0", whole_frame(db))["counts"]["visible"]

    insert_the_new_papers(db)
    insert_the_new_cluster(db)

    plan = db.check()
    assert plan.ok, plan
    assert not plan.sent
    # The order §6.2 fixes: points before the artifacts that name them, and the clustering before
    # its labels.
    kinds = [line.split()[0] for line in plan.plan]
    assert kinds[0] == "points"
    # The plan ends at the flush, which is where the commit waits (decision 0144).
    assert plan.plan[-1] == "flush, and wait for the publication it arms"
    assert plan.plan.index(next(p for p in plan.plan if "clusters/kmeans" in p)) < plan.plan.index(
        next(p for p in plan.plan if "topics/kmeans" in p)
    )

    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {"s0": len(NEW_IDS)}
    # One cluster and one label artifact, and the memberships each carried.
    assert report.artifacts_minted == 2
    assert report.flush_wait is not None and report.flush_reached
    assert len(report.tessera_ids) == len(NEW_IDS)

    after = viewport(db, "s0", whole_frame(db))["counts"]["visible"]
    assert after == before + len(NEW_IDS)

    # A drill-down on one of the rows the route answered with, by the id it returned.
    record = item(db, report.tessera_ids[0])
    assert record["fields"]["arxiv_id"].startswith("2609.")
    assert record["labels"] == ["cs.LG"]

    # The new cluster, as the browse verb serves it, with the masked count its members give it.
    clusters = browse(db, "s0", "clusters/kmeans", q="k-new")["artifacts"]
    assert [row["key"] for row in clusters] == ["k-new"]
    assert clusters[0]["masked_count"] == len(NEW_IDS)

    # Its label was published and given an identity, which is the only address it has (I10). Its
    # text is read through the cluster it attaches to: a label set expands to a layer whose
    # artifacts attach, and `browse` refuses such a layer directly.
    assert set(report.artifact_ids) == {"clusters/kmeans", "topics/kmeans"}
    assert set(report.artifact_ids["topics/kmeans"]) == {"k-new-label"}


def test_the_new_label_is_served_over_the_rows_the_same_commit_ingested(served, corpus):
    """§10.3's last step: the label's own text, as the viewport's artifacts frame carries it."""
    db = notebook(served, corpus)
    insert_the_new_papers(db)
    insert_the_new_cluster(db)
    assert db.commit().ok
    served_labels = [
        (layer, key, content)
        for layer, key, content, _ in artifact_rows_of(db, view="s0", frame=whole_frame(db))
        if key == "k-new-label"
    ]
    assert served_labels == [("topics/kmeans", "k-new-label", ["Audio diffusion"])]


def test_the_same_frame_inserted_again_is_sent_again_and_the_database_answers_for_it(
    served, corpus
):
    """§3: a re-run of a cell is a re-run, and the database is what says the rows are there.

    The SDK keeps no record of what it sent, so the same frame inserted again is sent again. What
    happens then is the server's to decide, and it is two different things: identical bytes under
    the batch id they were first sent under are a **replay** and land nothing (write-path §2.4),
    while a page naming ids the database holds is a `409` on the whole page, reported by the page
    it refused and applied nowhere.
    """
    db = notebook(served, corpus)
    delta = new_papers(db)
    db.insert("s0", delta, id="entity_id", x="x", y="y", access="categories")
    first = db.commit()
    assert first.ok and first.rows_accepted == {"s0": len(NEW_IDS)}
    after = viewport(db, "s0", whole_frame(db))["counts"]["visible"]

    db.insert("s0", delta, id="entity_id", x="x", y="y", access="categories")
    plan = db.check()
    # One page, and the flush that publishes it.
    assert len(plan.plan) == 2 and plan.plan[0].startswith("points"), plan
    replayed = db.commit()
    assert replayed.ok, replayed
    # The server says it applied nothing rather than the SDK inferring it from a count.
    assert len(replayed.replayed) == 1 and replayed.rows_accepted == {}
    assert viewport(db, "s0", whole_frame(db))["counts"]["visible"] == after

    # The same rows moved a little: different bytes, so a batch the server has not seen, and
    # every id on it is one it holds.
    insert_the_new_papers(db, x_offset=0.5)
    refused = db.commit()
    assert not refused.ok
    assert [r["status"] for r in refused.refusals] == [409]
    assert refused.rows_accepted == {}
    assert viewport(db, "s0", whole_frame(db))["counts"]["visible"] == after


def test_a_commit_of_rows_and_values_flushes_between_them(served, corpus):
    """§6.2 step 2: a value addresses a row the database holds, so the rows go visible first."""
    db = served(small)
    db.insert(
        "map",
        pa.table(
            {
                "id": pa.array(["p20", "p21"], pa.string()),
                "x": pa.array([20.0, 21.0], pa.float64()),
                "y": pa.array([0.0, 0.0], pa.float64()),
                "labels": pa.array([["public"]] * 2, pa.list_(pa.string())),
            }
        ),
        id="id",
        x="x",
        y="y",
        access="labels",
    )
    db.insert(
        "score",
        pa.table(
            {"id": pa.array(["p20", "p21"], pa.string()), "score": pa.array([2.0, 2.5], pa.float64())}
        ),
        id="id",
        value="score",
    )
    plan = db.check()
    lines = plan.plan
    rows_at = next(i for i, line in enumerate(lines) if line.startswith("points"))
    flush_at = next(i for i, line in enumerate(lines) if line.startswith("flush the rows"))
    values_at = next(i for i, line in enumerate(lines) if line.startswith("values into"))
    assert rows_at < flush_at < values_at, lines

    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {"map": 2} and report.values_filled == 2
    answer = viewport(db, "map", [-5.0, -5.0, 40.0, 40.0],
                      filters={"score": {"range": {"gte": 2.0}}})
    assert answer["counts"]["matched"] == 2


# ---------------------------------------------------------------------------- §10.4


def test_a_second_clustering_over_held_rows_is_one_key_column(served, corpus):
    """python-sdk.md §10.4: a table with an id column and a key column, over rows it holds.

    The values route reads a layer column by the ingest route's own rule: a key an artifact holds
    joins the entity to it, and a key no artifact holds mints the artifact it names on a layer
    whose value set is `open` (contracts §3.4). So the clustering is one insert.
    """
    db = notebook(served, corpus)
    held = [7, 8, 9, 10, 11]
    db.declare_layer("clusters/second", kind="flat")
    db.insert(
        "clusters/second",
        pa.table(
            {
                "entity_id": pa.array(held, pa.uint64()),
                "cluster2": pa.array(["c2-a", "c2-a", "c2-b", "c2-b", "c2-b"], pa.string()),
            }
        ),
        id="entity_id",
        key="cluster2",
    )
    plan = db.check()
    assert plan.ok, plan
    assert any("values into 'clusters/second'" in line for line in plan.plan), plan.plan

    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {}
    # The two keys no artifact held were minted at this commit, and the five rows joined them.
    assert report.artifacts_minted == 2

    rows = {row["key"]: row["masked_count"]
            for row in browse(db, "s0", "clusters/second")["artifacts"]}
    assert rows == {"c2-a": 2, "c2-b": 3}


def test_a_key_column_into_a_layer_the_database_holds_joins_its_artifacts(served, corpus):
    """The other arm of the same route: a key the layer's artifacts already declare."""
    db = notebook(served, corpus)
    key = browse(db, "s0", "clusters/kmeans")["artifacts"][0]["key"]
    before = browse(db, "s0", "clusters/kmeans", q=key)["artifacts"][0]["masked_count"]
    fresh = list(range(910_001, 910_011))
    db.insert(
        "s0",
        new_papers(db, ids=fresh, x_offset=1.5),
        id="entity_id",
        x="x",
        y="y",
        access="categories",
    )
    db.insert(
        "clusters/kmeans",
        pa.table(
            {
                "entity_id": pa.array(fresh, pa.uint64()),
                "cluster": pa.array([key] * len(fresh), pa.string()),
            }
        ),
        id="entity_id",
        key="cluster",
    )
    report = db.commit()
    assert report.ok, report
    assert report.memberships_joined == len(fresh)
    after = browse(db, "s0", "clusters/kmeans", q=key)["artifacts"][0]["masked_count"]
    assert after == before + len(fresh)


# ---------------------------------------------------------------------------- values


def small(db) -> None:
    """A database with one indexed column, so an insert into it fills values."""
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_attribute("score", type="f64", index=True, render=False)
    db.insert(
        "map",
        pa.table(
            {
                "id": pa.array([f"p{i}" for i in range(20)], pa.string()),
                "x": pa.array([float(i) for i in range(20)], pa.float64()),
                "y": pa.array([0.0] * 20, pa.float64()),
                "labels": pa.array([["public"]] * 20, pa.list_(pa.string())),
            }
        ),
        id="id",
        x="x",
        y="y",
        access="labels",
    )
    db.insert(
        "score",
        pa.table({"id": pa.array(["p0"], pa.string()), "score": pa.array([0.5], pa.float64())}),
        id="id",
        value="score",
    )


def test_an_insert_into_an_attribute_fills_it_and_a_filter_finds_it(served, corpus):
    db = served(small)
    frame = [-5.0, -5.0, 40.0, 40.0]
    assert viewport(db, "map", frame, filters={"score": {"range": {"gte": 0.0, "lte": 10.0}}})["counts"][
        "matched"
    ] == 1

    db.insert(
        "score",
        pa.table(
            {
                "id": pa.array(["p1", "p2", "p3"], pa.string()),
                "score": pa.array([1.5, 2.5, 3.5], pa.float64()),
            }
        ),
        id="id",
        value="score",
    )
    plan = db.check()
    assert any("values into 'score'" in line for line in plan.plan), plan
    report = db.commit()
    assert report.ok, report
    assert report.values_filled == 3

    answer = viewport(db, "map", frame, filters={"score": {"range": {"gte": 0.0, "lte": 10.0}}})
    assert answer["counts"]["matched"] == 4


# ---------------------------------------------------------------------------- §3, §6.4


def test_a_values_cell_re_run_is_sent_again_and_lands_on_the_cells_it_landed_on(served, corpus):
    db = served(small)
    delta = pa.table(
        {"id": pa.array(["p4", "p5"], pa.string()), "score": pa.array([4.0, 5.0], pa.float64())}
    )
    db.insert("score", delta, id="id", value="score")
    first = db.commit()
    assert first.ok and first.values_filled == 2

    # The same frame inserted again is sent again: a value that matches the cell it names is
    # accepted with no effect, which is the route's own dedupe rather than a log in the SDK.
    db.insert("score", delta, id="id", value="score")
    again = db.commit()
    assert again.ok, again
    assert again.values_filled == 0

    # A changed value on a held cell is a `409` on that part, reported and not retried: an edit is
    # a delete and a re-ingest (decision 0047), and the SDK does not do that for the user.
    db.insert(
        "score",
        pa.table({"id": pa.array(["p4"], pa.string()), "score": pa.array([9.0], pa.float64())}),
        id="id",
        value="score",
    )
    conflicted = db.commit()
    assert not conflicted.ok
    assert [r["status"] for r in conflicted.refusals] == [409]
    assert "score" in conflicted.refusals[0]["detail"]
    # And the cell still holds what it held.
    answer = viewport(db, "map", [-5.0, -5.0, 40.0, 40.0],
                      filters={"score": {"range": {"gte": 8.0}}})
    assert answer["counts"]["matched"] == 0


def test_a_values_only_commit_returns_with_its_effect_visible(served, corpus):
    """§6.2 step 5: the closing flush waits for the publication it arms, and a commit whose only
    work was filling cells reaches it like any other (decision 0144)."""
    db = served(small)
    db.insert(
        "score",
        pa.table(
            {"id": pa.array(["p8", "p9"], pa.string()),
             "score": pa.array([8.0, 9.0], pa.float64())}
        ),
        id="id",
        value="score",
    )
    report = db.commit()
    assert report.ok, report
    assert not any(line.startswith("points") for line in report.plan)
    assert report.flush_wait is not None and report.flush_reached
    # The flush named the publication this commit's work is visible at, and the report prints it.
    assert report.publication is not None
    assert f"for publication {report.publication}" in str(report)
    # The cell after the commit sees the values, with no wait of its own.
    answer = viewport(db, "map", [-5.0, -5.0, 40.0, 40.0],
                      filters={"score": {"range": {"gte": 8.0}}})
    assert answer["counts"]["matched"] == 2


def test_an_artifacts_only_commit_returns_with_its_effect_visible(served, corpus):
    """The same wait, for a commit whose only work was publishing artifacts."""
    db = served(clustering)
    db.insert(
        "clusters",
        artifacts=pa.table(
            {"level": pa.array([0], pa.uint32()), "key": pa.array(["c1"], pa.string())}
        ),
        key="key",
        level="level",
    )
    db.insert(
        "clusters",
        members=pa.table(
            {
                "level": pa.array([0] * 4, pa.uint32()),
                "key": pa.array(["c1"] * 4, pa.string()),
                "entity": pa.array([f"p{i}" for i in range(4)], pa.string()),
            }
        ),
        id="entity",
        key="key",
        level="level",
    )
    report = db.commit()
    assert report.ok, report
    assert report.artifacts_minted == 1
    assert report.flush_wait is not None and report.flush_reached
    rows = {row["key"]: row["masked_count"] for row in browse(db, "map", "clusters")["artifacts"]}
    assert rows == {"c0": 20, "c1": 4}


# ---------------------------------------------------------------------------- §6.5


def test_remove_stops_a_row_being_served_and_a_removed_id_is_inserted_as_a_point(served, corpus):
    db = notebook(served, corpus)
    frame = whole_frame(db)
    before = viewport(db, "s0", frame)["counts"]["visible"]

    report = db.remove([7])
    assert report.ok, report
    assert viewport(db, "s0", frame)["counts"]["visible"] == before - 1

    # A removed id inserted again goes as a point row, which decision 0047 allows.
    insert_the_new_papers(db, ids=[7])
    plan = db.check()
    assert any(line.startswith("points") for line in plan.plan), plan
    again = db.commit()
    assert again.ok, again
    assert again.rows_accepted == {"s0": 1}
    assert viewport(db, "s0", frame)["counts"]["visible"] == before


def test_suppress_hides_a_row_and_unsuppress_returns_it(served, corpus):
    db = notebook(served, corpus)
    frame = whole_frame(db)
    before = viewport(db, "s0", frame)["counts"]["visible"]
    assert db.suppress([8]).ok
    assert viewport(db, "s0", frame)["counts"]["visible"] == before - 1
    assert db.unsuppress([8]).ok
    assert viewport(db, "s0", frame)["counts"]["visible"] == before


def test_leave_shrinks_a_generating_set_and_emptying_it_withdraws_the_content(served, corpus):
    """The one set that may shrink (decision 0135, §6.5), read back from what is served."""
    db = served(clustering)
    db.declare_labels("topics", of="clusters", content_requires="all")
    db.insert(
        "topics",
        label_rows(),
        key="key",
        level="level",
        contents="contents",
        attached_layer="attached_layer",
        attached_key="attached_key",
    )
    db.insert(
        "topics", members=label_members(n=5), id="entity", key="key", level="level", rank="rank"
    )
    assert db.commit().ok
    assert ("topics", "l0", ["A generated label"], 20) in artifact_rows_of(db)

    # Three of the five leave: the content is served against the two that remain.
    assert db.leave("topics", "l0", ["p0", "p1", "p2"], rank=0).ok
    assert ("topics", "l0", ["A generated label"], 20) in artifact_rows_of(db)

    # The page that empties the set withdraws the content, and it does not come back on its own.
    assert db.leave("topics", "l0", ["p3", "p4"], rank=0).ok
    assert not [row for row in artifact_rows_of(db) if row[0] == "topics" and row[2]]


# ---------------------------------------------------------------------------- §6.3


def test_a_row_outside_the_frame_refuses_the_commit_and_nothing_is_sent(served, corpus):
    """§6.3: the pre-flight reports and sends nothing, and drops no row to send the rest."""
    db = served(small)
    before = viewport(db, "map", [-5.0, -5.0, 40.0, 40.0])["counts"]["visible"]
    db.insert(
        "map",
        pa.table(
            {
                "id": pa.array(["far", "near"], pa.string()),
                "x": pa.array([9_000.0, 3.5], pa.float64()),
                "y": pa.array([0.0, 0.0], pa.float64()),
                "labels": pa.array([["public"]] * 2, pa.list_(pa.string())),
            }
        ),
        id="id",
        x="x",
        y="y",
        access="labels",
    )
    plan = db.check()
    assert not plan.ok
    assert any("outside view 'map''s frame" in str(f) for f in plan.findings), plan
    report = db.commit()
    assert not report.ok, report
    assert report.rows_accepted == {} and not report.refusals
    # Neither row was sent: the one inside the frame is not a commit the user asked for on its own.
    assert viewport(db, "map", [-5.0, -5.0, 40.0, 40.0])["counts"]["visible"] == before


def test_a_row_with_no_id_where_the_insert_names_one_is_listed(served, corpus):
    """§6.3: a row whose id is null is a row no member table and no value can reach."""
    db = served(small)
    db.insert(
        "score",
        pa.table(
            {"id": pa.array(["p6", None], pa.string()), "score": pa.array([6.0, 7.0], pa.float64())}
        ),
        id="id",
        value="score",
    )
    plan = db.check()
    assert not plan.ok
    assert any("rows with no id" in str(f) for f in plan.findings), plan


def test_a_rendered_column_filled_after_the_first_commit_refuses_the_commit(served, corpus):
    """§6.3: the route refuses a rendered column, and the SDK does not send the page without it."""

    def with_a_rendered_score(db) -> None:
        db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
        db.declare_attribute("note", type="f64", render=True)
        db.insert(
            "map",
            pa.table(
                {
                    "id": pa.array([f"p{i}" for i in range(20)], pa.string()),
                    "x": pa.array([float(i) for i in range(20)], pa.float64()),
                    "y": pa.array([0.0] * 20, pa.float64()),
                    "labels": pa.array([["public"]] * 20, pa.list_(pa.string())),
                    "note": pa.array([0.5] * 20, pa.float64()),
                }
            ),
            id="id",
            x="x",
            y="y",
            access="labels",
        )

    db = served(with_a_rendered_score)
    db.insert(
        "note",
        pa.table({"id": pa.array(["p1"], pa.string()), "note": pa.array([1.0], pa.float64())}),
        id="id",
        value="note",
    )
    plan = db.check()
    assert not plan.ok
    assert any("rendered column" in str(f) for f in plan.findings), plan
    assert plan.plan == []


def test_a_column_no_target_reads_is_ignored_and_printed_as_ignored(served, corpus, capsys):
    """§3: a frame with more columns than the target reads is the ordinary case."""
    db = served(small)
    insert = db.insert(
        "score",
        pa.table(
            {
                "id": pa.array(["p6"], pa.string()),
                "score": pa.array([6.0], pa.float64()),
                "sentiment": pa.array([0.1], pa.float64()),
            }
        ),
        id="id",
        value="score",
    )
    assert insert.ignored == ["sentiment"]
    assert "ignored: sentiment" in capsys.readouterr().out
    report = db.commit()
    assert report.ok, report
    assert report.values_filled == 1


def test_a_labels_insert_whose_clustering_is_neither_held_nor_inserted_is_refused(served, corpus):
    """§6.3: ordered after the clustering's pages, or refused where none are inserted."""
    db = served(small)
    db.declare_layer("clusters/new", kind="flat")
    db.declare_labels("topics/new", of="clusters/new", content_requires="all")
    db.insert(
        "topics/new",
        pa.table(
            {
                "level": pa.array([0], pa.uint32()),
                "key": pa.array(["orphan"], pa.string()),
                "contents": pa.array([[["Nothing to hang from"]]], pa.list_(pa.list_(pa.string()))),
                "attached_layer": pa.array(["clusters/new"], pa.string()),
                "attached_key": pa.array(["never-published"], pa.string()),
            }
        ),
        key="key",
        level="level",
        contents="contents",
        attached_layer="attached_layer",
        attached_key="attached_key",
    )
    db.insert(
        "topics/new",
        members=pa.table(
            {
                "key": pa.array(["orphan"], pa.string()),
                "entity": pa.array(["p0"], pa.string()),
            }
        ),
        id="entity",
        key="key",
    )
    plan = db.check()
    assert not plan.ok
    assert any("labels insert before its clustering" in str(f) for f in plan.findings), plan


def test_a_key_column_inserted_into_a_layer_with_supplied_content_is_refused(tmp_path, corpus):
    """§6.3: an artifact served without content its layer declares cannot be told from one whose
    content was withheld, so such a layer takes an artifacts table."""
    from tesseradb._database import create

    db = create(tmp_path / "db")
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_layer(
        "topics/inline", kind="flat", supplied=[("topic", "text", "inherited")]
    )
    points = pa.table(
        {
            "id": pa.array([f"p{i}" for i in range(20)], pa.string()),
            "x": pa.array([float(i) for i in range(20)], pa.float64()),
            "y": pa.array([0.0] * 20, pa.float64()),
            "labels": pa.array([["public"]] * 20, pa.list_(pa.string())),
            "topic": pa.array(["t"] * 20, pa.string()),
        }
    )
    db.insert("map", points, id="id", x="x", y="y", access="labels")
    db.insert("topics/inline", points, id="id", key="topic")
    report = db.check()
    assert not report.ok
    assert any("supplied content" in str(f) for f in report.findings), report
    with pytest.raises(Refusal, match="insert\\('topics/inline', artifacts="):
        db.commit()


# ---------------------------------------------------------------------------- declared after the
# first commit


def clustering(db) -> None:
    """A small database with one clustering, so a label set can be declared over it later."""
    n = 20
    db.declare_view("map", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_layer("clusters", kind="flat")
    db.insert(
        "map",
        pa.table(
            {
                "id": pa.array([f"p{i}" for i in range(n)], pa.string()),
                "x": pa.array([float(i) for i in range(n)], pa.float64()),
                "y": pa.array([0.0] * n, pa.float64()),
                "labels": pa.array([["public"]] * n, pa.list_(pa.string())),
            }
        ),
        id="id",
        x="x",
        y="y",
        access="labels",
    )
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
                "level": pa.array([0] * n, pa.uint32()),
                "key": pa.array(["c0"] * n, pa.string()),
                "entity": pa.array([f"p{i}" for i in range(n)], pa.string()),
            }
        ),
        id="entity",
        key="key",
        level="level",
    )


def label_rows(key: str = "l0", text: str = "A generated label") -> pa.Table:
    return pa.table(
        {
            "level": pa.array([0], pa.uint32()),
            "key": pa.array([key], pa.string()),
            "contents": pa.array([[[text]]], pa.list_(pa.list_(pa.string()))),
            "attached_layer": pa.array(["clusters"], pa.string()),
            "attached_key": pa.array(["c0"], pa.string()),
        }
    )


def label_members(key: str = "l0", n: int = 5, ranked: bool = True) -> pa.Table:
    """A label's member table: a null rank is the membership, rank 0 the generating set."""
    ranks = [None] * n + ([0] * n if ranked else [])
    entities = [f"p{i}" for i in range(n)] * (2 if ranked else 1)
    return pa.table(
        {
            "level": pa.array([0] * len(ranks), pa.uint32()),
            "key": pa.array([key] * len(ranks), pa.string()),
            "rank": pa.array(ranks, pa.uint32()),
            "entity": pa.array(entities, pa.string()),
        }
    )


def artifact_rows_of(db, view: str = "map", frame=None) -> list[tuple]:
    """The kind-5 artifacts frame of a whole-extent viewport: layer, key, content, masked count."""
    import io

    import pyarrow.ipc as ipc

    from conftest import post

    box = [-5.0, -5.0, 40.0, 40.0] if frame is None else list(frame)
    content = post(
        db.viewer_url + "/v1/viewport",
        db.token().token,
        {"view": view, "zoom": 0, "bbox": box, "k": 16, "layers": "all"},
    )
    at = 0
    rows: list[tuple] = []
    while at + 5 <= len(content):
        kind = content[at]
        length = int.from_bytes(content[at + 1 : at + 5], "little")
        payload = content[at + 5 : at + 5 + length]
        at += 5 + length
        if kind == 5:
            table = ipc.open_stream(io.BytesIO(payload)).read_all().to_pydict()
            rows = list(
                zip(
                    table["layer"],
                    table["key"],
                    table["content"],
                    table["masked_count"],
                )
            )
    return rows


def test_one_members_insert_means_the_same_at_the_first_commit_and_at_the_second(served, corpus):
    """§3: what the call named is what both doors read, so the same table is the same membership.

    The same `(level, key, rank, entity)` table is inserted into one layer at the first commit and
    into another at a later one, with the same column names on the call. The masked counts the two
    layers serve are equal: the build and the publication route read the columns the call named
    and nothing else.
    """
    n = 20
    table = pa.table(
        {
            "level": pa.array([0] * n, pa.uint32()),
            "key": pa.array(["c0"] * n, pa.string()),
            "rank": pa.array([None] * n, pa.uint32()),
            "entity": pa.array([f"p{i}" for i in range(n)], pa.string()),
        }
    )

    def declare(db):
        clustering(db)
        db.declare_layer("at_build", kind="flat")
        db.insert(
            "at_build",
            artifacts=pa.table(
                {"level": pa.array([0], pa.uint32()), "key": pa.array(["c0"], pa.string())}
            ),
            key="key",
            level="level",
        )
        db.insert("at_build", members=table, id="entity", key="key", level="level", rank="rank")

    db = served(declare)
    db.declare_layer("at_ingest", kind="flat")
    db.insert(
        "at_ingest",
        artifacts=pa.table(
            {"level": pa.array([0], pa.uint32()), "key": pa.array(["c0"], pa.string())}
        ),
        key="key",
        level="level",
    )
    db.insert("at_ingest", members=table, id="entity", key="key", level="level", rank="rank")
    report = db.commit()
    assert report.ok, report

    built = {row["key"]: row["masked_count"] for row in browse(db, "map", "at_build")["artifacts"]}
    ingested = {
        row["key"]: row["masked_count"] for row in browse(db, "map", "at_ingest")["artifacts"]
    }
    assert built == ingested == {"c0": n}


def test_a_label_attached_to_a_cluster_minted_in_the_same_commit_is_served(served, corpus):
    """§6.2 step 4: a publication naming a key step 3 mints waits for the mint to be visible.

    The clustering is one key column, which the values route mints the artifact from; the label
    attaches to that key in the same commit, and a minted artifact is resolvable only from its
    publication, so the commit flushes between the two. The label carries no members of its own
    and is served over the cluster's (decision 0145).
    """
    db = served(clustering)
    keys = pa.table(
        {
            "id": pa.array([f"p{i}" for i in range(8)], pa.string()),
            "cluster": pa.array(["c-new"] * 8, pa.string()),
        }
    )
    db.declare_layer("clusters/fresh", kind="flat", title="Minted here")
    db.declare_labels("topics/fresh", of="clusters/fresh")
    db.insert("clusters/fresh", keys, id="id", key="cluster")
    db.insert("topics/fresh", {"c-new": "A cluster minted at the values route"})

    plan = db.check()
    assert plan.ok, plan
    lines = plan.plan
    values_at = next(i for i, line in enumerate(lines) if line.startswith("values into"))
    flush_at = next(i for i, line in enumerate(lines) if line.startswith("flush the values"))
    publish_at = next(i for i, line in enumerate(lines) if line.startswith("publish"))
    assert values_at < flush_at < publish_at, lines

    report = db.commit()
    assert report.ok, report
    # One artifact minted by the values page, and one published by the labels page.
    assert report.artifacts_minted == 2
    rows = {(row[0], row[1]): row for row in artifact_rows_of(db)}
    assert rows[("clusters/fresh", "c-new")][3] == 8
    # The label is served with its text, over the members its cluster holds and none of its own.
    assert rows[("topics/fresh", "c-new")][2] == ["A cluster minted at the values route"]
    assert rows[("topics/fresh", "c-new")][3] == 8


def test_a_label_set_declared_after_the_first_commit_is_declared_and_served(served, corpus):
    """§6.2 step 1: the runtime `PUT` body comes from `tessera check --payloads`."""
    db = served(clustering)
    db.declare_labels("topics", of="clusters", content_requires="all")
    db.insert(
        "topics",
        label_rows(),
        key="key",
        level="level",
        contents="contents",
        attached_layer="attached_layer",
        attached_key="attached_key",
    )
    db.insert(
        "topics", members=label_members(), id="entity", key="key", level="level", rank="rank"
    )

    plan = db.check()
    assert plan.plan[0] == "declare layer 'topics' (flat)", plan
    report = db.commit()
    assert report.ok, report
    assert report.artifacts_minted == 1
    # The database is what says the layer is there, and the next commit reads it from there.
    assert "topics" in {layer["name"] for layer in db.meta()["layers"]}

    assert ("topics", "l0", ["A generated label"], 20) in artifact_rows_of(db)


def test_a_label_set_from_a_mapping_builds_and_is_served_with_its_text(served, corpus):
    """§4.7's mapping form, through the build and out of the viewport.

    `insert(labels, {key: text})` is an artifacts table the SDK writes, and each row carries the
    attachment as well as the text: a label set expands to a layer that depends on its clustering,
    so every artifact it publishes attaches to one and a row naming none is refused at the build.
    The key the mapping gives is the cluster's, which is what the label hangs from and what names
    the label's own artifact.
    """
    held = list(range(1, 200))
    keys = ["c2-a"] * 100 + ["c2-b"] * (len(held) - 100)

    def declare(db):
        declare_notebook(db, corpus)
        db.declare_layer("clusters/second", kind="flat")
        db.declare_labels("topics/second", of="clusters/second", content_requires="all")
        db.insert(
            "clusters/second",
            artifacts=pa.table({"level": pa.array([0, 0], pa.uint32()),
                                "key": pa.array(["c2-a", "c2-b"], pa.string())}),
            key="key",
            level="level",
        )
        db.insert(
            "clusters/second",
            members=pa.table(
                {
                    "level": pa.array([0] * len(held), pa.uint32()),
                    "key": pa.array(keys, pa.string()),
                    "entity": pa.array(held, pa.uint64()),
                }
            ),
            id="entity",
            key="key",
            level="level",
        )
        db.insert("topics/second", {"c2-a": "Audio diffusion", "c2-b": "Graph learning"})
        # Both grains, as any label's member table carries them: a null rank is the membership the
        # label is drawn over, and rank 0 is content 0's generating set.
        db.insert(
            "topics/second",
            members=pa.table(
                {
                    "level": pa.array([0] * (2 * len(held)), pa.uint32()),
                    "key": pa.array(keys + keys, pa.string()),
                    "rank": pa.array([None] * len(held) + [0] * len(held), pa.uint32()),
                    "entity": pa.array(held + held, pa.uint64()),
                }
            ),
            id="entity",
            key="key",
            level="level",
            rank="rank",
        )

    db = served(declare)
    # The table the mapping was written to: the text, and where each row attaches.
    written = next(one for one in db.inserts if one.target == "topics/second" and one.role == "text")
    table = pq.read_table(written.path)
    assert table.column_names == ["level", "key", "contents", "attached_layer", "attached_key"]
    assert table["attached_layer"].to_pylist() == ["clusters/second"] * 2
    assert table["attached_key"].to_pylist() == ["c2-a", "c2-b"]

    rows = artifact_rows_of(db, view="s0", frame=whole_frame(db))
    served_text = {key: content for layer, key, content, _ in rows if layer == "topics/second"}
    assert served_text == {"c2-a": ["Audio diffusion"], "c2-b": ["Graph learning"]}
