"""The paged commit, against a real served database (python-sdk.md §6, §10.3, §10.4).

Every test here commits a database, stages a delta on it and reads the answer back through the
viewer plane. What is under test is that the SDK's pages are what the control plane takes, so
nothing here doubles the server: a fake control plane would check the SDK against the SDK's own
reading of the contract.
"""

from __future__ import annotations

import pyarrow as pa
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
    """A week of new papers, as the frame §10.3 stages."""
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


def stage_the_new_cluster(db, ids=None, key: str = "k-new", label: str = "Audio diffusion") -> None:
    """§10.3's other four stagings: a cluster, its members, its label and its generating set."""
    ids = list(NEW_IDS if ids is None else ids)
    db.stage(
        "kmeans",
        pa.table(
            {
                "level": pa.array([0], pa.uint32()),
                "key": pa.array([key], pa.string()),
            }
        ),
    )
    db.stage(
        "kmeans_members",
        pa.table(
            {
                "level": pa.array([0] * len(ids), pa.uint32()),
                "key": pa.array([key] * len(ids), pa.string()),
                "entity": pa.array(ids, pa.uint64()),
            }
        ),
    )
    db.stage(
        "kmeans_topics",
        pa.table(
            {
                "level": pa.array([0], pa.uint32()),
                "key": pa.array([f"{key}-label"], pa.string()),
                "contents": pa.array([[[label]]], pa.list_(pa.list_(pa.string()))),
                "attached_layer": pa.array(["clusters/kmeans"], pa.string()),
                "attached_key": pa.array([key], pa.string()),
            }
        ),
    )
    # A label's member table carries two grains: a null rank is the membership, and rank *k* is
    # content *k*'s generating set (annotation-write-cycle §6.1).
    db.stage(
        "kmeans_topic_members",
        pa.table(
            {
                "level": pa.array([0] * (2 * len(ids)), pa.uint32()),
                "key": pa.array([f"{key}-label"] * (2 * len(ids)), pa.string()),
                "rank": pa.array([None] * len(ids) + [0] * len(ids), pa.uint32()),
                "entity": pa.array(ids + ids, pa.uint64()),
            }
        ),
    )


def notebook(served, corpus):
    return served(lambda db: declare_notebook(db, corpus))


# ---------------------------------------------------------------------------- §10.3


def test_a_delta_of_new_papers_with_a_cluster_and_a_label_is_served(served, corpus):
    """python-sdk.md §10.3, through to the served answer."""
    db = notebook(served, corpus)
    before = viewport(db, "s0", whole_frame(db))["counts"]["visible"]

    db.stage("points", new_papers(db))
    stage_the_new_cluster(db)

    plan = db.check()
    assert plan.ok, plan
    assert not plan.sent
    # The order §6.2 fixes: points before the artifacts that name them, and the clustering before
    # its labels.
    kinds = [line.split()[0] for line in plan.plan]
    assert kinds[0] == "points"
    assert plan.plan[-1].startswith("flush")
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


@pytest.mark.xfail(
    strict=True,
    reason="issue #150: a content gated `all` whose generating set holds entities that arrived by "
    "ingest fails containment for every principal, so the label is served to nobody. The "
    "publication is accepted and the artifact exists; the same label over entities the build read "
    "is served, which `test_a_label_set_declared_after_the_first_commit_is_declared_and_served` "
    "shows",
)
def test_the_new_label_is_served_over_the_rows_the_same_commit_ingested(served, corpus):
    """§10.3's last step: the label's own text, as the viewport's artifacts frame carries it."""
    db = notebook(served, corpus)
    db.stage("points", new_papers(db))
    stage_the_new_cluster(db)
    assert db.commit().ok
    served_labels = [
        (layer, key, content)
        for layer, key, content, _ in artifact_rows_of(db, view="s0", frame=whole_frame(db))
        if key == "k-new-label"
    ]
    assert served_labels == [("topics/kmeans", "k-new-label", ["Audio diffusion"])]


def test_a_re_staged_frame_is_sent_again_and_the_database_answers_for_it(served, corpus):
    """§3: a re-run of a cell is a re-run, and the database is what says the rows are there.

    The SDK keeps no record of what it sent, so the same frame staged again is sent again. What
    happens then is the server's to decide, and it is two different things: identical bytes under
    the batch id they were first sent under are a **replay** and land nothing (write-path §2.4),
    while a page naming ids the database holds is a `409` on the whole page, reported by the page
    it refused and applied nowhere.
    """
    db = notebook(served, corpus)
    delta = new_papers(db)
    db.stage("points", delta)
    first = db.commit()
    assert first.ok and first.rows_accepted == {"s0": len(NEW_IDS)}
    after = viewport(db, "s0", whole_frame(db))["counts"]["visible"]

    db.stage("points", delta)
    plan = db.check()
    assert len(plan.plan) == 2 and plan.plan[0].startswith("points"), plan
    replayed = db.commit()
    assert replayed.ok, replayed
    assert viewport(db, "s0", whole_frame(db))["counts"]["visible"] == after

    # The same rows moved a little: different bytes, so a batch the server has not seen, and
    # every id on it is one it holds.
    db.stage("points", new_papers(db, x_offset=0.5))
    refused = db.commit()
    assert not refused.ok
    assert [r["status"] for r in refused.refusals] == [409]
    assert refused.rows_accepted == {}
    assert viewport(db, "s0", whole_frame(db))["counts"]["visible"] == after


# ---------------------------------------------------------------------------- §10.4


def test_a_second_clustering_over_held_rows_is_published_through_its_tables(served, corpus):
    """python-sdk.md §10.4, on §6.2 step 3's terms: a clustering over rows the database holds.

    `from_column=` mints artifacts from the rows that carry the column, which happens at the build
    and on the ingest route; rows that are already there carry nothing. So the layer is declared
    over its own tables and the keys are published with their members.
    """
    db = notebook(served, corpus)
    held = [7, 8, 9, 10, 11]
    db.declare_layer("clusters/second", kind="flat", source="second", members="second_members")
    db.stage(
        "second",
        pa.table({"level": pa.array([0, 0], pa.uint32()),
                  "key": pa.array(["c2-a", "c2-b"], pa.string())}),
    )
    db.stage(
        "second_members",
        pa.table(
            {
                "level": pa.array([0] * len(held), pa.uint32()),
                "key": pa.array(["c2-a", "c2-a", "c2-b", "c2-b", "c2-b"], pa.string()),
                "entity": pa.array(held, pa.uint64()),
            }
        ),
    )

    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {}
    assert report.artifacts_minted == 2

    rows = {row["key"]: row["masked_count"]
            for row in browse(db, "s0", "clusters/second")["artifacts"]}
    assert rows == {"c2-a": 2, "c2-b": 3}


def test_a_from_column_layer_over_held_rows_is_refused_naming_the_tables(served, corpus):
    """§6.2 step 3: the values route fills a column and mints no artifact."""
    db = notebook(served, corpus)
    with pytest.raises(Refusal, match="through its tables"):
        db.declare_layer("clusters/third", kind="flat", from_column="cluster3")


# ---------------------------------------------------------------------------- values


def small(db) -> None:
    """A database whose attribute reads a source of its own, so a delta on it fills values."""
    db.stage(
        "points",
        pa.table(
            {
                "id": pa.array([f"p{i}" for i in range(20)], pa.string()),
                "x": pa.array([float(i) for i in range(20)], pa.float64()),
                "y": pa.array([0.0] * 20, pa.float64()),
                "labels": pa.array([["public"]] * 20, pa.list_(pa.string())),
            }
        ),
        id="id",
        default=True,
    )
    db.stage(
        "scores",
        pa.table(
            {
                "id": pa.array(["p0"], pa.string()),
                "score": pa.array([0.5], pa.float64()),
            }
        ),
        id="id",
    )
    db.declare_view("map", source="points", access="labels", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_attribute("score", type="f64", source="scores", index=True, render=False)


def test_a_values_delta_fills_an_indexed_attribute_and_a_filter_finds_it(served, corpus):
    db = served(small)
    frame = [-5.0, -5.0, 40.0, 40.0]
    assert viewport(db, "map", frame, filters={"score": {"range": {"gte": 0.0, "lte": 10.0}}})["counts"][
        "matched"
    ] == 1

    db.stage(
        "scores",
        pa.table(
            {
                "id": pa.array(["p1", "p2", "p3"], pa.string()),
                "score": pa.array([1.5, 2.5, 3.5], pa.float64()),
            }
        ),
        id="id",
    )
    plan = db.check()
    assert any("values on existing entities" in line for line in plan.plan), plan
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
    db.stage("scores", delta, id="id")
    first = db.commit()
    assert first.ok and first.values_filled == 2

    # The same frame staged again is sent again: a value that matches the cell it names is
    # accepted with no effect, which is the route's own dedupe rather than a log in the SDK.
    db.stage("scores", delta, id="id")
    again = db.commit()
    assert again.ok, again
    assert again.values_filled == 0

    # A changed value on a held cell is a `409` on that part, reported and not retried: an edit is
    # a delete and a re-ingest (decision 0047), and the SDK does not do that for the user.
    db.stage(
        "scores",
        pa.table(
            {"id": pa.array(["p4"], pa.string()), "score": pa.array([9.0], pa.float64())},
        ),
        id="id",
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
    """§6.2 step 5: the flush answers with the publication its cycle carries, and `commit()` waits
    for `/control/status` to reach it — a commit that wrote no row moves that counter too."""
    db = served(small)
    db.stage(
        "scores",
        pa.table(
            {"id": pa.array(["p8", "p9"], pa.string()),
             "score": pa.array([8.0, 9.0], pa.float64())}
        ),
        id="id",
    )
    report = db.commit()
    assert report.ok, report
    assert not any(line.startswith("points") for line in report.plan)
    assert report.flush_wait is not None and report.flush_reached
    # The cell after the commit sees the values, with no wait of its own.
    answer = viewport(db, "map", [-5.0, -5.0, 40.0, 40.0],
                      filters={"score": {"range": {"gte": 8.0}}})
    assert answer["counts"]["matched"] == 2


def test_an_artifacts_only_commit_returns_with_its_effect_visible(served, corpus):
    """The same wait, for a commit whose only work was publishing artifacts."""
    db = served(clustering)
    db.stage(
        "cl",
        pa.table({"level": pa.array([0], pa.uint32()), "key": pa.array(["c1"], pa.string())}),
    )
    db.stage(
        "clm",
        pa.table(
            {
                "level": pa.array([0] * 4, pa.uint32()),
                "key": pa.array(["c1"] * 4, pa.string()),
                "entity": pa.array([f"p{i}" for i in range(4)], pa.string()),
            }
        ),
    )
    report = db.commit()
    assert report.ok, report
    assert report.artifacts_minted == 1
    assert report.flush_wait is not None and report.flush_reached
    rows = {row["key"]: row["masked_count"] for row in browse(db, "map", "clusters")["artifacts"]}
    assert rows == {"c0": 20, "c1": 4}


# ---------------------------------------------------------------------------- §6.5


def test_remove_stops_a_row_being_served_and_a_removed_id_restages_as_a_point(served, corpus):
    db = notebook(served, corpus)
    frame = whole_frame(db)
    before = viewport(db, "s0", frame)["counts"]["visible"]

    report = db.remove([7])
    assert report.ok, report
    assert viewport(db, "s0", frame)["counts"]["visible"] == before - 1

    # A removed id staged again goes as a point row, which decision 0047 allows.
    db.stage("points", new_papers(db, ids=[7]))
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
    db.declare_labels("topics", of="clusters", source="lb", members="lbm", content_requires="all")
    db.stage("lb", label_rows())
    db.stage("lbm", label_members(n=5))
    assert db.commit().ok
    assert ("topics", "l0", ["A generated label"], 20) in artifact_rows_of(db)

    # Three of the five leave: the content is served against the two that remain.
    assert db.leave("topics", "l0", ["p0", "p1", "p2"], rank=0).ok
    assert ("topics", "l0", ["A generated label"], 20) in artifact_rows_of(db)

    # The page that empties the set withdraws the content, and it does not come back on its own.
    assert db.leave("topics", "l0", ["p3", "p4"], rank=0).ok
    assert not [row for row in artifact_rows_of(db) if row[0] == "topics" and row[2]]


# ---------------------------------------------------------------------------- §6.3


def test_the_pre_flight_reports_a_row_outside_the_frame_and_refuses_an_undeclared_column(
    served, corpus
):
    db = served(small)
    # Out of frame: dropped and listed, with the frame. The server would refuse the whole page.
    db.stage(
        "points",
        pa.table(
            {
                "id": pa.array(["far", "near"], pa.string()),
                "x": pa.array([9_000.0, 3.5], pa.float64()),
                "y": pa.array([0.0, 0.0], pa.float64()),
                "labels": pa.array([["public"]] * 2, pa.list_(pa.string())),
            }
        ),
        id="id",
    )
    plan = db.check()
    assert any("outside view 'map''s frame" in str(f) for f in plan.findings), plan
    report = db.commit()
    assert report.ok, report
    assert report.rows_accepted == {"map": 1}


def test_the_pre_flight_refuses_a_column_no_block_declares(served, corpus):
    db = served(small)
    db.stage(
        "scores",
        pa.table(
            {
                "id": pa.array(["p6"], pa.string()),
                "score": pa.array([6.0], pa.float64()),
                "sentiment": pa.array([0.1], pa.float64()),
            }
        ),
        id="id",
    )
    with pytest.raises(Refusal, match="sentiment"):
        db.check()


def test_a_labels_delta_whose_clustering_is_neither_held_nor_staged_is_refused(served, corpus):
    """§6.3: ordered after the clustering's pages, or refused where none are staged."""
    db = served(small)
    db.declare_layer("clusters/new", kind="flat", source="cl2", members="clm2")
    empty = pa.table(
        {"level": pa.array([], pa.uint32()), "key": pa.array([], pa.string())}
    )
    db.stage("cl2", empty)
    db.stage(
        "clm2",
        pa.table(
            {
                "level": pa.array([], pa.uint32()),
                "key": pa.array([], pa.string()),
                "entity": pa.array([], pa.string()),
            }
        ),
    )
    db.declare_labels("topics/new", of="clusters/new", source="lb2", content_requires="inherited")
    db.stage(
        "lb2",
        pa.table(
            {
                "level": pa.array([0], pa.uint32()),
                "key": pa.array(["orphan"], pa.string()),
                "contents": pa.array([[["Nothing to hang from"]]], pa.list_(pa.list_(pa.string()))),
                "attached_layer": pa.array(["clusters/new"], pa.string()),
                "attached_key": pa.array(["never-published"], pa.string()),
            }
        ),
    )
    plan = db.check()
    assert not plan.ok
    assert any("labels delta before its clustering" in str(f) for f in plan.findings), plan


def test_a_key_column_staged_for_a_layer_with_supplied_content_is_refused(served, corpus):
    """§6.3: an artifact served without content its layer declares cannot be told from one whose
    content was withheld, so such a layer takes an artifacts table."""

    def with_a_supplied_layer(db) -> None:
        db.stage(
            "points",
            pa.table(
                {
                    "id": pa.array([f"p{i}" for i in range(20)], pa.string()),
                    "x": pa.array([float(i) for i in range(20)], pa.float64()),
                    "y": pa.array([0.0] * 20, pa.float64()),
                    "labels": pa.array([["public"]] * 20, pa.list_(pa.string())),
                    "topic": pa.array(["t"] * 20, pa.string()),
                }
            ),
            id="id",
            default=True,
        )
        db.declare_view(
            "map", source="points", access="labels", extent={"x": [-5, 40], "y": [-5, 40]}
        )
        db.declare_layer(
            "topics/inline",
            kind="flat",
            from_column="topic",
            supplied=[("topic", "text", "inherited")],
        )

    db = served(with_a_supplied_layer)
    db.stage(
        "points",
        pa.table(
            {
                "id": pa.array(["p7"], pa.string()),
                "x": pa.array([7.0], pa.float64()),
                "y": pa.array([0.0], pa.float64()),
                "labels": pa.array([["public"]], pa.list_(pa.string())),
                "topic": pa.array(["t"], pa.string()),
            }
        ),
        id="id",
    )
    with pytest.raises(Refusal, match="artifacts table"):
        db.check()


# ---------------------------------------------------------------------------- declared after the
# first commit


def clustering(db) -> None:
    """A small database with one clustering, so a label set can be declared over it later."""
    n = 20
    db.stage(
        "points",
        pa.table(
            {
                "id": pa.array([f"p{i}" for i in range(n)], pa.string()),
                "x": pa.array([float(i) for i in range(n)], pa.float64()),
                "y": pa.array([0.0] * n, pa.float64()),
                "labels": pa.array([["public"]] * n, pa.list_(pa.string())),
            }
        ),
        id="id",
        default=True,
    )
    db.stage(
        "cl",
        pa.table({"level": pa.array([0], pa.uint32()), "key": pa.array(["c0"], pa.string())}),
    )
    db.stage(
        "clm",
        pa.table(
            {
                "level": pa.array([0] * n, pa.uint32()),
                "key": pa.array(["c0"] * n, pa.string()),
                "entity": pa.array([f"p{i}" for i in range(n)], pa.string()),
            }
        ),
    )
    db.declare_view("map", source="points", access="labels", extent={"x": [-5, 40], "y": [-5, 40]})
    db.declare_layer("clusters", kind="flat", source="cl", members="clm")


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


def test_a_label_set_declared_after_the_first_commit_is_declared_and_served(served, corpus):
    """§6.2 step 1: the runtime `PUT` body comes from `tessera check --payloads`."""
    db = served(clustering)
    db.declare_labels("topics", of="clusters", source="lb", members="lbm", content_requires="all")
    db.stage("lb", label_rows())
    db.stage("lbm", label_members())

    plan = db.check()
    assert plan.plan[0] == "declare layer 'topics' (flat)", plan
    report = db.commit()
    assert report.ok, report
    assert report.artifacts_minted == 1
    # The database is what says the layer is there, and the next commit reads it from there.
    assert "topics" in {layer["name"] for layer in db.meta()["layers"]}

    assert ("topics", "l0", ["A generated label"], 20) in artifact_rows_of(db)
