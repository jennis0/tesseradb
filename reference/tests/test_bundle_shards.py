"""The oracle opens a bundle of one shard numbered 0 and refuses every other shape, as the server's
open does.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest

from oracle.bundle import Bundle
from oracle.identity import ROUNDS


def _bundle(root: Path, records: list[dict], identity_extra: dict | None = None) -> Path:
    """A bundle naming no file, with `records` as its side-manifest's shard records."""
    prefix = root / "v00000"
    partition = prefix / "partitions" / "default"
    partition.mkdir(parents=True)
    identity = {
        "construction": "feistel-splitmix64-v1",
        "rounds": ROUNDS,
        "key": "000102030405060708090a0b0c0d0e0f",
        **(identity_extra or {}),
    }
    manifest = json.dumps({"files": {}, "views": [], "identity": identity}).encode()
    (prefix / "MANIFEST.json").write_bytes(manifest)
    (root / "CURRENT").write_text(
        json.dumps({"prefix": "v00000", "manifest_digest": hashlib.sha256(manifest).hexdigest()})
    )
    side = {"files": {}, "dict_extents": [], "shards": {"size": 7, "records": records}}
    (partition / "SEGMENTS-0.json").write_text(json.dumps(side))
    return root


def _record(shard: int) -> dict:
    return {"id": shard, "high_water": 12, "segments": []}


def test_one_shard_numbered_zero_opens_and_its_record_holds_the_high_water(tmp_path):
    bundle = Bundle(_bundle(tmp_path, [_record(0)]))
    assert bundle.shard_record()["high_water"] == 12
    assert bundle.partition_manifest()["shards"]["size"] == 7


@pytest.mark.parametrize(
    "records",
    [
        pytest.param([], id="no shard"),
        pytest.param([_record(0), _record(1)], id="two shards"),
        pytest.param([_record(1)], id="one shard numbered 1"),
    ],
)
def test_any_other_shards_are_refused(tmp_path, records):
    with pytest.raises(ValueError):
        Bundle(_bundle(tmp_path, records))


def test_a_shard_named_in_the_identity_is_refused(tmp_path):
    with pytest.raises(ValueError):
        Bundle(_bundle(tmp_path, [_record(0)], {"shard_id": 0}))
