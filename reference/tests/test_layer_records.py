"""The oracle reads a layer's identifiers off the side-manifest a running server last published: the
layer is entity 0 of its space, and a level's artifacts take its runs' entities in ordinal order.
"""

from __future__ import annotations

import json

import pytest
from oracle import identity
from oracle.bundle import (
    artifact_mosaica_id,
    layer_mosaica_id,
    newest_side_manifest,
    registered_layer,
)
from oracle.layout import partition_dir

KEY = identity.IdentityKey.from_hex("000102030405060708090a0b0c0d0e0f")


def _layer(space: int, *levels: list[tuple[int, int]]) -> dict:
    return {
        "declaration": {"name": "clusters"},
        "space": space,
        "runs": [{"runs": [{"start": s, "end": e} for s, e in runs]} for runs in levels],
    }


def test_the_newest_side_manifest_of_the_live_prefix_is_read(tmp_path):
    partition = partition_dir(tmp_path / "v00002")
    partition.mkdir(parents=True)
    for n in (2, 10, 9):
        published = {
            "n": n,
            "layers": [],
            "files": {},
            "shards": {"size": 7, "next_id": 0, "files": []},
            "layer_spaces": [],
        }
        (partition / f"SEGMENTS-{n}.json").write_text(json.dumps(published))
    (tmp_path / "CURRENT").write_text(json.dumps({"prefix": "v00002"}))
    side_manifest = newest_side_manifest(tmp_path)
    assert side_manifest["n"] == 10
    assert registered_layer(side_manifest, "clusters") is None


def test_a_layer_is_entity_zero_of_its_space():
    layer = _layer(3, [(65_536, 131_072)])
    assert identity.invert_artifact(KEY, layer_mosaica_id(KEY, layer)) == (3, 0)
    assert registered_layer({"layers": [layer]}, "clusters") is layer


def test_a_levels_ordinals_run_across_its_runs_in_the_order_given():
    layer = _layer(7, [(65_536, 131_072)], [(131_072, 196_608), (262_144, 327_680)])
    assert identity.invert_artifact(KEY, artifact_mosaica_id(KEY, layer, 0, 5)) == (7, 65_541)
    assert identity.invert_artifact(KEY, artifact_mosaica_id(KEY, layer, 1, 65_535)) == (7, 196_607)
    assert identity.invert_artifact(KEY, artifact_mosaica_id(KEY, layer, 1, 65_536)) == (7, 262_144)
    with pytest.raises(ValueError):
        artifact_mosaica_id(KEY, layer, 0, 65_536)
