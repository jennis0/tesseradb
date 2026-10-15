"""The oracle reads every shard of a bundle, and what it computes over them is what it computes
over one shard holding the same items.

The multi-view corpus is built twice with the CLI: once as one shard, and once with a `[shards]
size` that splits its 6,144 items over seven. Items are compared across the two through the
source id each was planted under, since the shard and number the build gave an item, and so its
key and its identifier, differ between them. Identifiers and the samples drawn by rank are the
only answers allowed to differ.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

import numpy as np
import pytest

from oracle import aggregate as agg
from oracle import filters as filt
from oracle import multiview as mv
from oracle import occupancy, viewport
from oracle.bundle import Bundle
from oracle.harness import cli_build, ensure_cli_built, write_deployment
from oracle.identity import ROUNDS, by_shard, invert_item, item_of_key
from oracle.layout import partition_dir, shard_dir
from oracle.mask import mask_of
from oracle.tenancy import HolderCount

#: Seven shards: six of 1,000 and one of 144.
SIZE = 1000

#: Principals, as the compartments they hold.
GRANTS = [("cc0",), ("cc1", "cc3"), ("cc2", "cc4", "cc5"), tuple(f"cc{c}" for c in range(6))]


# -- the side-manifest's shard records -----------------------------------------------------------


def _bundle(root: Path, records: list[dict]) -> Path:
    """A bundle naming no file, with `records` as its side-manifest's shard records."""
    prefix = root / "v00000"
    partition = partition_dir(prefix)
    partition.mkdir(parents=True)
    identity = {
        "construction": "feistel-splitmix64-v1",
        "rounds": ROUNDS,
        "key": "000102030405060708090a0b0c0d0e0f",
    }
    manifest = json.dumps({"files": {}, "views": [], "identity": identity}).encode()
    (prefix / "MANIFEST.json").write_bytes(manifest)
    (root / "CURRENT").write_text(
        json.dumps({"prefix": "v00000", "manifest_digest": hashlib.sha256(manifest).hexdigest()})
    )
    named = []
    for record in records:
        data = json.dumps({**record, "files": {}}).encode()
        shard = shard_dir(prefix, shard=record["id"])
        shard.mkdir(parents=True, exist_ok=True)
        (shard / "SEGMENTS-0.json").write_bytes(data)
        digest = hashlib.sha256(data).hexdigest()
        named.append(
            {"id": record["id"], "n": 0, "size": len(data), "sha256": digest, "denies": False}
        )
    next_id = max((record["id"] for record in records), default=-1) + 1
    side = {
        "files": {},
        "dict_extents": [],
        "shards": {"size": 7, "next_id": next_id, "files": named},
        "layer_spaces": [],
    }
    (partition / "SEGMENTS-0.json").write_text(json.dumps(side))
    return root


def _record(shard: int) -> dict:
    return {
        "id": shard,
        "state": "open",
        "identity": "00" * 32,
        "high_water": 12,
        "segments": [],
    }


@pytest.mark.parametrize(
    "ids",
    [
        pytest.param([0], id="one shard numbered 0"),
        pytest.param([1], id="one shard numbered 1"),
        pytest.param([0, 1, 2], id="three shards"),
    ],
)
def test_every_shard_the_side_manifest_records_is_read(tmp_path, ids):
    bundle = Bundle(_bundle(tmp_path, [_record(i) for i in ids]))
    assert [record["id"] for record in bundle.shards] == ids
    assert HolderCount.of_bundle(bundle).built == {i: 12 for i in ids}


@pytest.mark.parametrize(
    "ids",
    [
        pytest.param([], id="no shard"),
        pytest.param([0, 0], id="a shard twice"),
        pytest.param([1, 0], id="descending"),
    ],
)
def test_shard_records_that_are_not_ascending_and_distinct_are_refused(tmp_path, ids):
    with pytest.raises(ValueError):
        Bundle(_bundle(tmp_path, [_record(i) for i in ids]))


# -- one corpus built as one shard and as seven --------------------------------------------------


def _build(work: Path, size: int | None) -> Bundle:
    mv.write_corpus(work)
    shards = "" if size is None else f"[shards]\nsize = {size}\n"
    (work / mv.SCHEMA_NAME).write_text(shards + mv.SCHEMA_TOML)
    write_deployment(
        work / mv.DEPLOYMENT_NAME, bundle=work / "bundle", schema=work / mv.SCHEMA_NAME
    )
    cli_build(work / mv.DEPLOYMENT_NAME, work / "bundle", capture_output=True)
    return mv.attach_geometry(Bundle(work / "bundle"), work)


@pytest.fixture(scope="module")
def built(tmp_path_factory) -> dict[str, Bundle]:
    ensure_cli_built()
    return {
        "one": _build(tmp_path_factory.mktemp("one-shard"), None),
        "many": _build(tmp_path_factory.mktemp("many-shards"), SIZE),
    }


def _sources(bundle: Bundle, items) -> set[int]:
    source_of = {item: source for source, item in mv.entities_by_source(bundle).items()}
    return {source_of[item] for item in items}


def _mask(bundle: Bundle, grant: tuple[str, ...]) -> set[int]:
    return mask_of({bundle.term_id_of(name.encode()) for name in grant}, bundle.pairs())


def test_a_build_past_the_size_is_read_shard_by_shard(built):
    one, many = built["one"], built["many"]
    assert [r["id"] for r in one.shards] == [0]
    assert [r["id"] for r in many.shards] == list(range(7))
    assert [r["high_water"] for r in many.shards] == [SIZE] * 6 + [mv.N_ITEMS - 6 * SIZE]
    for view_id in mv.ALL_VIEW_IDS:
        assert [shard for shard, _ in one.segment(view_id).parts] == [0]
        assert [shard for shard, _ in many.segment(view_id).parts] == list(range(7))
    # Every item is read from the shard holding it, under the number it holds there.
    held = by_shard(mv.entities_by_source(many).values())
    assert all(held.get(r["id"]) == set(range(r["high_water"])) for r in many.shards)


@pytest.mark.parametrize("which", ["one", "many"])
def test_every_identifier_inverts_to_the_shard_and_number_its_row_is_read_from(built, which):
    bundle = built[which]
    numbers = HolderCount.of_bundle(bundle)
    for view_id in mv.ALL_VIEW_IDS:
        segment = bundle.segment(view_id)
        for shard, lo, hi in segment.part_ranges():
            for row in range(lo, hi):
                item = int(segment.entity_id[row])
                stored = int(segment.mosaica_id[row])
                assert item_of_key(item)[0] == shard
                assert invert_item(bundle.identity_key, stored) == (shard, 0, item_of_key(item)[1])
                assert bundle.mosaica_id_of(item) == stored
                assert numbers.named(stored) == item
        bundle.verify_identity_cross_check(view_id)
        order = bundle.derive_row_order(view_id)
        assert np.array_equal(order, np.arange(segment.row_count))
        assert np.array_equal(segment.cuts, segment.cut_starts())


def test_the_corpus_verifies_alike_over_one_shard_and_seven(built):
    one, many = (mv.verify(built[which]) for which in ("one", "many"))
    assert one.views == many.views
    assert one.decorrelation == many.decorrelation
    for name, items in one.compartment_entities.items():
        assert _sources(built["one"], items) == _sources(
            built["many"], many.compartment_entities[name]
        )


@pytest.mark.parametrize("grant", GRANTS, ids=["+".join(g) for g in GRANTS])
def test_masks_counts_and_selection_over_seven_shards_are_one_shards(built, grant):
    one, many = built["one"], built["many"]
    masks = {which: _mask(built[which], grant) for which in built}
    for which, bundle in built.items():
        terms = [bundle.term_id_of(name.encode()) for name in grant]
        postings = {int(i) for term in terms for i in bundle.postings(term)}
        assert masks[which] == postings
    assert _sources(one, masks["one"]) == _sources(many, masks["many"])

    for view_id in mv.ALL_VIEW_IDS:
        totals = {w: viewport.visible_total(built[w], masks[w], view_id) for w in built}
        assert totals["one"] == totals["many"]
        for depth in (3, 7):
            selection = {w: viewport.Selection(built[w], masks[w], view_id, depth) for w in built}
            tiles = selection["one"].rows_by_tile.keys()
            assert tiles == selection["many"].rows_by_tile.keys()
            assert selection["one"].counts_for(list(tiles)) == selection["many"].counts_for(
                list(tiles)
            )
            # θ: counted over the one shard's one segment, sketched over seven shards' seven.
            assert selection["one"].n_occ == occupancy.exact_ladder(tiles, depth)[depth]
            assert selection["many"].n_occ == occupancy.sketch_ladder(tiles, depth)[depth]
            # Saturated and uncapped, every visible point is served: the same positions, in an
            # order each build's identifiers decide.
            params = {"k_min": 1, "cap": mv.N_ITEMS, "v_total": totals["one"], "m_target": 1}
            for tile in tiles:
                served = {
                    w: sorted(selection[w].served_points(tile, filtered=True, **params))
                    for w in built
                }
                assert served["one"] == served["many"]


def test_filters_and_aggregates_over_seven_shards_are_one_shards(built):
    view_id = f"{mv.GROUP}:{mv.QUARTER_KEYS[1]}"
    expr = {"sentiment": {"range": {"gte": -0.25, "lt": 0.5}}}
    answers = {}
    for which, bundle in built.items():
        candidate = _mask(bundle, GRANTS[2]) & set(bundle.row_entity_ids(view_id))
        columns = {"sentiment": filt.NumericColumn(values=mv.sentiment_columns(bundle)[view_id])}
        selected = filt.evaluate(expr, columns, candidate)
        groups: dict[str, set[int]] = {}
        for item, mood in mv.mood_columns(bundle)[view_id].items():
            groups.setdefault(mood, set()).add(item)
        position = dict(zip(bundle.row_entity_ids(view_id), bundle.row_position_codes(view_id)))
        head, rows = agg.table(
            items=selected,
            reference=candidate,
            groups=groups,
            pick=("top", 3),
            depth=2,
            position=position,
        )
        assert selected and selected != candidate, "the filter must narrow the visible set"
        answers[which] = (_sources(bundle, selected), head, rows)
    assert answers["one"] == answers["many"]
