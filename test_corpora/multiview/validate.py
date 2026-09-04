"""Structural checks over a prepared `multiview` fixture — no build, no engine, just the parquets
and `corpus.toml` `prepare.py` wrote. Asserts the claims a two-view build would refuse to violate
(views.md §4/§7's label byte-agreement, per-(entity, view) row uniqueness) and the shape claims
this fixture exists to exercise (the entity-overlap pattern, the roster/discriminator agreement,
the presence bitmap on each group-scoped family, and the per-view disagreement that makes a scoped
category's value list and a scoped text column's index say anything).

    python -m test_corpora.multiview.validate
"""

from __future__ import annotations

import datetime
import sys
from collections import defaultdict
from pathlib import Path

try:
    import tomllib  # 3.11+
except ModuleNotFoundError:
    import tomli as tomllib  # 3.10 and earlier

import pyarrow.parquet as pq

from ..common.paths import ladder

QUARTERS = ["2026-Q1", "2026-Q2", "2026-Q3", "2026-Q4"]


def fail(msg: str) -> None:
    raise AssertionError(msg)


def main() -> None:
    out = ladder("multiview")
    print(f"validating {out}")

    corpus = tomllib.loads((out / "corpus.toml").read_text())

    world = pq.read_table(out / "world.parquet")
    quarters = {q: pq.read_table(out / f"quarter-{q}.parquet") for q in QUARTERS}
    quarter_alt = pq.read_table(out / "quarter-alt.parquet")
    attrs = pq.read_table(out / "attrs-constant.parquet")
    vocab_kind = pq.read_table(out / "vocab-kind.parquet")
    collections = pq.read_table(out / "collections.parquet")
    clusters_q = pq.read_table(out / "clusters-quarter.parquet")

    checks = 0

    # --- per-(entity, view) row uniqueness (views.md §7: "a row is unique per (external_id, view)") ---
    def assert_unique(table, label):
        nonlocal checks
        ids = table["entity_id"].to_pylist()
        assert len(ids) == len(set(ids)), f"{label}: duplicate entity_id within one view"
        checks += 1

    assert_unique(world, "world")
    for q in QUARTERS:
        assert_unique(quarters[q], f"quarter:{q}")
    # quarter_alt: unique per (entity_id, quarter)
    pairs = list(zip(quarter_alt["entity_id"].to_pylist(), quarter_alt["quarter"].to_pylist()))
    assert len(pairs) == len(set(pairs)), "quarter_alt: duplicate (entity_id, quarter) pair"
    checks += 1
    print(f"  [{checks}] per-(entity, view) row uniqueness — world, four quarters, quarter_alt: OK")

    # --- label (access) byte-agreement for an entity across every file it appears in ---
    access_by_entity: dict[int, object] = {}
    disagreements = []

    def record(table, label):
        eids = table["entity_id"].to_pylist()
        accs = table["access"].to_pylist()
        for eid, acc in zip(eids, accs):
            if eid in access_by_entity:
                if access_by_entity[eid] != acc:
                    disagreements.append((eid, label, access_by_entity[eid], acc))
            else:
                access_by_entity[eid] = acc

    record(world, "world")
    for q in QUARTERS:
        record(quarters[q], f"quarter:{q}")
    record(quarter_alt, "quarter_alt")
    if disagreements:
        fail(f"access (label) disagreement on {len(disagreements)} entities, e.g. {disagreements[:3]}")
    checks += 1
    print(f"  [{checks}] label byte-agreement across {len(access_by_entity):,} distinct entities: OK")

    some_null = sum(1 for v in access_by_entity.values() if v is None)
    assert some_null > 0, "expected some entities with no access term (the default case)"
    print(f"      {some_null:,} entities ({some_null * 100 / len(access_by_entity):.1f}%) carry no access term")

    # --- roster/discriminator consistency: quarter_alt's per-quarter entity set matches quarter's ---
    for q in QUARTERS:
        group_ids = set(quarters[q]["entity_id"].to_pylist())
        alt_mask_ids = set(
            eid for eid, qq in zip(quarter_alt["entity_id"].to_pylist(), quarter_alt["quarter"].to_pylist())
            if qq == q
        )
        assert group_ids == alt_mask_ids, f"{q}: quarter_alt's entity set disagrees with quarter's"
    checks += 1
    print(f"  [{checks}] quarter_alt's discriminator agrees with quarter's roster, all 4 keys: OK")

    discriminator_values = set(quarter_alt["quarter"].to_pylist())
    assert discriminator_values == set(QUARTERS), f"unexpected discriminator values: {discriminator_values}"

    # --- the entity-overlap pattern (spec §4/§7's join rule; this fixture's four buckets) ---
    world_ids = set(world["entity_id"].to_pylist())
    quarter_ids = {q: set(quarters[q]["entity_id"].to_pylist()) for q in QUARTERS}
    any_quarter = set().union(*quarter_ids.values())
    all_quarters = set.intersection(*quarter_ids.values())

    full = world_ids & all_quarters
    world_and_one = world_ids & any_quarter - all_quarters
    world_only = world_ids - any_quarter
    quarters_only_never_world = any_quarter - world_ids

    assert full, "expected some entities in the world view and every quarter"
    assert world_and_one, "expected some entities in the world view and exactly one quarter"
    assert world_only, "expected some entities in the world view only"
    assert quarters_only_never_world, "expected some entities in quarters only, never the world view"
    checks += 1
    print(
        f"  [{checks}] entity-overlap pattern: full={len(full):,} world+one={len(world_and_one):,} "
        f"world-only={len(world_only):,} quarters-only={len(quarters_only_never_world):,}"
    )

    # a quarters-only entity must be in >= 2 quarters (this fixture's D bucket is exactly 2)
    counts = defaultdict(int)
    for q in QUARTERS:
        for eid in quarter_ids[q]:
            counts[eid] += 1
    bad = [eid for eid in quarters_only_never_world if counts[eid] < 2]
    assert not bad, f"{len(bad)} quarters-only entities appear in fewer than 2 quarters"
    checks += 1
    print(f"  [{checks}] every quarters-only entity appears in >= 2 quarters: OK")

    # --- constant attribute coverage: attrs_constant covers every entity in every view ---
    all_entities = world_ids | any_quarter
    attrs_ids = set(attrs["entity_id"].to_pylist())
    missing = all_entities - attrs_ids
    assert not missing, f"{len(missing)} entities have no row in attrs_constant"
    checks += 1
    print(f"  [{checks}] constant attribute (importance, kind) covers all {len(all_entities):,} entities: OK")

    # --- category vocabulary closure: every `kind` value in attrs_constant is in vocab_kind ---
    vocab_keys = set(vocab_kind["key"].to_pylist())
    kind_values = set(v for v in attrs["kind"].to_pylist() if v is not None)
    unknown = kind_values - vocab_keys
    assert not unknown, f"kind values not in vocab_kind: {unknown}"
    checks += 1
    print(f"  [{checks}] `kind` vocabulary closure — {len(vocab_keys)} declared, {len(kind_values)} used: OK")

    # --- group-scoped attribute (sentiment): present for some rows, absent for others, per quarter ---
    for q in QUARTERS:
        sentiment = quarters[q]["sentiment"].to_pylist()
        n_present = sum(1 for v in sentiment if v is not None)
        n_absent = sum(1 for v in sentiment if v is None)
        assert n_present > 0 and n_absent > 0, f"{q}: sentiment should have both present and absent values"
    checks += 1
    print(f"  [{checks}] sentiment (group-scoped) has both present and absent values in every quarter: OK")

    # --- the category family (mood): every value is in the declared set, and the sets a quarter
    # uses differ between quarters — which is what makes a per-view value list say anything.
    (mood_vocab,) = [v for v in corpus["vocabulary"] if v["name"] == "mood"]
    declared_moods = set(mood_vocab["values"])
    used = {}
    for q in QUARTERS:
        values = [v for v in quarters[q]["mood"].to_pylist() if v is not None]
        assert values, f"{q}: mood carries no value at all"
        assert len(values) < quarters[q].num_rows, f"{q}: mood should have absent values too"
        unknown = set(values) - declared_moods
        assert not unknown, f"{q}: mood values outside the closed vocabulary: {unknown}"
        used[q] = set(values)
    assert len({frozenset(v) for v in used.values()}) > 1, (
        "every quarter uses the same mood values, so a per-view value list would prove nothing"
    )
    checks += 1
    print(f"  [{checks}] mood (group-scoped category) is closed and differs by quarter: {[sorted(used[q]) for q in QUARTERS]}")

    # --- the text family (note): each quarter's prose carries its own word and no other's, so a
    # `match` answered from the wrong view's index answers the empty set.
    words = {q: f"the {w}" for q, w in zip(QUARTERS, ["alpha", "beta", "gamma", "delta"])}
    for q in QUARTERS:
        prose = [v for v in quarters[q]["note"].to_pylist() if v is not None]
        assert prose, f"{q}: note carries no prose"
        assert all(words[q] in p for p in prose), f"{q}: prose does not carry this quarter's word"
        for other in QUARTERS:
            if other != q:
                assert not any(words[other] in p for p in prose), f"{q}: prose carries {other}'s word"
    checks += 1
    print(f"  [{checks}] note (group-scoped text) carries each quarter's own word and no other's: OK")

    # --- a scoped attribute's own source: one row per (entity, view), the discriminator closed
    # over the roster, and every entity it names in that quarter's own row space.
    scoped_src = pq.read_table(out / "attrs-scoped.parquet")
    scoped_pairs = list(zip(scoped_src["entity_id"].to_pylist(), scoped_src["quarter"].to_pylist()))
    assert len(scoped_pairs) == len(set(scoped_pairs)), "attrs_scoped: duplicate (entity_id, quarter) pair"
    assert set(scoped_src["quarter"].to_pylist()) == set(QUARTERS), "attrs_scoped: discriminator outside the roster"
    for q in QUARTERS:
        rows = {eid for eid, qq in scoped_pairs if qq == q}
        assert rows == set(quarters[q]["entity_id"].to_pylist()), f"{q}: attrs_scoped's rows are not this view's"
    coverage = scoped_src["coverage"].to_pylist()
    assert any(v is None for v in coverage) and any(v is not None for v in coverage)
    checks += 1
    print(f"  [{checks}] attrs_scoped is one row per (entity, view) with a closed discriminator: OK")

    # --- typed metadata in corpus.toml: label/starts/ends on every [[view_group.view]] block ---
    (group,) = [g for g in corpus["view_group"] if g["name"] == "quarter"]
    assert group["metadata"] == {"label": "text", "starts": "timestamp_us", "ends": "timestamp_us"}
    view_blocks = group["view"]
    assert len(view_blocks) == 4
    for block in view_blocks:
        assert set(("key", "source", "label", "starts", "ends")) <= set(block.keys())
        assert isinstance(block["label"], str)
        assert isinstance(block["starts"], datetime.datetime)
        assert isinstance(block["ends"], datetime.datetime)
        assert block["starts"] < block["ends"]
    checks += 1
    print(f"  [{checks}] view_group.view metadata typed and present on all 4 blocks: OK")

    (alt_group,) = [g for g in corpus["view_group"] if g["name"] == "quarter_alt"]
    assert alt_group.get("members") == "quarter"
    assert "metadata" not in alt_group, "a members group must not declare its own metadata (spec §3.3)"
    checks += 1
    print(f"  [{checks}] quarter_alt declares members=quarter and no roster of its own: OK")

    # --- layers: collections' members are real entity ids; clusters_q's quarter column is closed ---
    coll_members = collections["members"].to_pylist()
    coll_ids = set(eid for members in coll_members for eid in members)
    assert coll_ids <= attrs_ids, "collections layer names entities outside the corpus"
    checks += 1
    print(f"  [{checks}] collections layer's {len(coll_ids):,} member ids are all real entities: OK")

    clus_quarters = set(clusters_q["quarter"].to_pylist())
    assert clus_quarters == set(QUARTERS)
    clus_keys = clusters_q["key"].to_pylist()
    assert len(clus_keys) == len(set(clus_keys)), "clusters_q: duplicate artifact key"
    for q in QUARTERS:
        clus_members = [
            eid
            for row_q, members in zip(clusters_q["quarter"].to_pylist(), clusters_q["members"].to_pylist())
            if row_q == q
            for eid in members
        ]
        assert set(clus_members) == quarter_ids[q], f"{q}: cluster membership doesn't partition the quarter"
    checks += 1
    print(f"  [{checks}] quarter_clusters partitions each quarter's own entity set: OK")

    # --- the shape layer spans two frames (decision 0111) ---------------------------------
    (regions,) = [l for l in corpus["layer"] if l["name"] == "regions"]
    assert regions["membership"] == "spatial"
    projections = {
        v["name"]: v["projection"] for v in corpus["view"] if v["name"] in regions["views"]
    }
    assert len(projections) == 2, "the shape layer is drawn on two plain views"
    assert len(set(projections.values())) == 2, (
        "the two views must place their points by different functions, or the layer spans "
        "no frames at all (decision 0111)"
    )
    assert "none" not in set(projections.values()), (
        "a layer's views are all projected or all `none`, never the mix (decision 0111)"
    )
    assert all(a["space"] == "wgs84" for a in regions["artifacts"]), (
        "`view` space is refused over unequal frames; `wgs84` is the spelling that spans"
    )
    checks += 1
    print(
        f"  [{checks}] regions layer spans {len(projections)} frames "
        f"({', '.join(f'{k}={v}' for k, v in sorted(projections.items()))}), all `wgs84`: OK"
    )

    print(f"\n{checks} checks passed.")


if __name__ == "__main__":
    try:
        main()
    except AssertionError as e:
        print(f"\nFAILED: {e}", file=sys.stderr)
        sys.exit(1)
