"""**A dropped layer leaves nothing behind** (`docs/sharding.md` §2.2), black-box, over the release
binary and the mask catalogue.

Layer `drop-a` holds two levels and `drop-b` one. `drop-a` takes space 0 and `drop-b` space 1, and
the oracle derives every identifier either issued from the side-manifest's record of the layer:
its space, and its levels' runs, which it first checks are the aligned blocks of 65,536 the
registration gives, one per level from entity 65,536. Then `drop-a` is dropped, with one artifact
suppressed and one deleted. What is asserted:

- **What is served**: `drop-a` is answered as a layer never registered, every identifier it issued
  as one nobody was issued, and `drop-b` and every point count are unchanged.
- **What is stored**: the next side-manifest names neither `drop-a` nor its space, in the layer
  spaces or in the shard's membership slices and row forms, and the next compaction's prefix holds
  no directory of the space.
- **The name registers again** as a new layer in a new space, higher than every space given
  before: the same keys at the same levels take the same entities there, and none of the new
  identifiers is an old one.
- **A restart brings none of it back**, and a drop acknowledged just before the process is killed
  is replayed.
"""

from __future__ import annotations

import json

import pytest
import requests
from oracle import identity as identity_mod
from oracle.bundle import (
    artifact_mosaica_id,
    layer_mosaica_id,
    newest_side_manifest,
    registered_layer,
)
from oracle.harness import kill_server, spawn_server, stop_server
from oracle.layout import layer_space_dir
from test_kind_routing import (
    VIEW,
    artifact_card,
    every_term,
    layer_declaration,
    matched,
    published,
    stored,
)

A, B, C = "drop-a", "drop-b", "drop-c"
#: The never-registered layer every route naming a dropped one is compared with.
NEVER = "drop-never"
#: The artifacts each layer publishes, by level: their keys and the item numbers of their members.
ARTIFACTS = {
    A: {0: {"k0": [0, 1, 2], "k1": [3, 4], "k2": [5]}, 1: {"m0": [0, 3], "m1": [6, 7]}},
    B: {0: {"b0": [0, 8]}},
    C: {0: {"c0": [9]}},
}
BLOCK = 65_536


class Dropping:
    """The server over a private copy of the catalogue, and what each layer issued."""

    def __init__(self, tmp_path_factory, private_catalogue_bundle, catalogue_bundle):
        self.root = private_catalogue_bundle("layer-drop")
        self.tmp = tmp_path_factory.mktemp("layer-drop-server")
        self.bundle = catalogue_bundle
        self.key = catalogue_bundle.identity_key
        self.no_artifact = identity_mod.forward_artifact(self.key, 9_999, BLOCK)
        #: Each layer's own identifier and its artifacts', by `(level, key)`, as last registered.
        self.layer: dict[str, int] = {}
        self.ids: dict[str, dict[tuple[int, str], int]] = {}
        self.start()

    def start(self) -> None:
        self.server, self.proc = spawn_server(self.root, self.tmp)
        self.token = self.server.authorise(every_term())["token"]

    def restart(self, crash: bool = False) -> None:
        (kill_server if crash else stop_server)(self.proc)
        self.start()

    def register(self, name: str) -> dict:
        """Register `name`, publish its artifacts level by level, and check every receipt against
        the identifiers the oracle derives from the layer's stored record."""
        levels = len(ARTIFACTS[name]) if name == A else 0
        resp = self.server.register_layer(layer_declaration(name, levels))
        assert resp.status_code == 201, resp.text
        self.layer[name] = int(resp.json()["mosaica_id"])
        self.ids[name] = {}
        for level, artifacts in ARTIFACTS[name].items():
            body = [
                {
                    "key": key,
                    "members": {"mosaica_id": [str(self.bundle.mosaica_id_of(n)) for n in members]},
                }
                for key, members in artifacts.items()
            ]
            resp = self.server.publish_artifacts(name, strict=True, level=level, artifacts=body)
            assert resp.status_code == 201, resp.text
            for row in resp.json()["artifacts"]:
                self.ids[name][(level, row["key"])] = int(row["mosaica_id"])
        side_manifest = stored(
            self.root,
            lambda m: (held := registered_layer(m, name)) is not None
            and layer_mosaica_id(self.key, held) == self.layer[name],
        )
        record = registered_layer(side_manifest, name)
        assert record["runs"] == [
            {"runs": [{"start": (1 + level) * BLOCK, "end": (2 + level) * BLOCK}]}
            for level in range(len(ARTIFACTS[name]))
        ]
        assert self.layer[name] == layer_mosaica_id(self.key, record)
        for level, artifacts in ARTIFACTS[name].items():
            for ordinal, key in enumerate(artifacts):
                derived = artifact_mosaica_id(self.key, record, level, ordinal)
                assert self.ids[name][(level, key)] == derived, (name, level, key)
        published(self.server)
        return record

    def every_id(self, name: str) -> list[int]:
        return [self.layer[name], *self.ids[name].values()]


def drop(server, name: str, wait: bool = True) -> requests.Response:
    resp = requests.delete(
        f"{server.control_base}/control/layers/{name}",
        params={"wait": "visible"} if wait else {},
        headers={"Authorization": f"Bearer {server.operator_credential}"},
        timeout=60,
    )
    resp.raise_for_status()
    return resp


def spaces_named(side_manifest: dict) -> set[int]:
    """Every layer space a side-manifest names: its layers' and layer spaces' records, and the
    shard's membership slices and row forms."""
    shard = side_manifest["shards"]["records"][0]
    return {
        record["space"]
        for record in (
            *side_manifest["layers"],
            *side_manifest["layer_spaces"],
            *shard["membership_extents"],
            *shard["derived_extents"],
        )
    }


def live_prefix(root):
    return root / json.loads((root / "CURRENT").read_text())["prefix"]


@pytest.fixture(scope="module")
def dropping(tmp_path_factory, private_catalogue_bundle, catalogue_bundle):
    fixture = Dropping(tmp_path_factory, private_catalogue_bundle, catalogue_bundle)
    fixture.spaces = {A: fixture.register(A)["space"], B: fixture.register(B)["space"]}
    yield fixture
    stop_server(fixture.proc)


def test_each_layer_takes_the_next_space_and_the_oracles_identifiers(dropping):
    assert dropping.spaces == {A: 0, B: 1}
    for name in (A, B):
        for mosaica_id in dropping.every_id(name):
            space, _entity = identity_mod.invert_artifact(dropping.key, mosaica_id)
            assert space == dropping.spaces[name]


def check_dropped(fx, old: list[int], names_gone: bool) -> None:
    """`old` names nothing on any route, and where `names_gone` the dropped name is answered as a
    layer never registered. `drop-b` is served as it was."""
    server, token = fx.server, fx.token
    layers = [layer["name"] for layer in server.meta(token)["layers"]]
    assert B in layers
    no_artifact = artifact_card(server, token, fx.no_artifact)
    for mosaica_id in old:
        assert artifact_card(server, token, mosaica_id) == no_artifact, mosaica_id
        leaf = {"member_of": {"layer": B, "artifact": str(mosaica_id)}}
        assert matched(server, token, leaf) == 0
    changes = [{"op": "suppress", "match": {"mosaica_id": str(i)}} for i in old]
    resp = server.changes(changes)
    assert resp.status_code == 200, resp.text
    assert resp.json()["accepted"] == 0
    assert {row["reason"] for row in resp.json()["refused"]} == {"unknown_mosaica_id"}
    assert len(resp.json()["refused"]) == len(old)
    assert server.changes(changes[:1], strict=True).status_code == 404
    b0 = fx.ids[B][(0, "b0")]
    assert artifact_card(server, token, b0)[0] == 200
    if names_gone:
        assert A not in layers

        def asked(layer: str) -> tuple:
            """Each refusal's status and code; the detail names the layer asked for."""
            artifacts = server.artifacts(token, view=VIEW, layer=layer, fields=["masked_count"])
            browse = server.browse(token, view=VIEW, layer=layer)
            return tuple((r.status_code, r.json()["error"]) for r in (artifacts, browse))

        assert asked(A) == asked(NEVER)
        assert asked(A)[0][0] == 422


def test_a_dropped_layer_is_served_on_no_route(dropping):
    server, token = dropping.server, dropping.token
    resp = server.changes(
        [
            {"op": "suppress", "match": {"mosaica_id": str(dropping.ids[A][(0, "k1")])}},
            {"op": "delete", "match": {"mosaica_id": str(dropping.ids[A][(0, "k2")])}},
        ],
        strict=True,
    )
    assert resp.status_code == 200, resp.text
    visible = matched(server, token, None)
    assert layer_space_dir(live_prefix(dropping.root), dropping.spaces[A]).is_dir()

    resp = drop(server, A)
    assert resp.status_code == 200, resp.text
    assert resp.json()["visible"]
    dropping.old = dropping.every_id(A)
    check_dropped(dropping, dropping.old, names_gone=True)
    assert matched(server, token, None) == visible


def test_nothing_of_a_dropped_layer_is_stored(dropping):
    gone, kept = dropping.spaces[A], dropping.spaces[B]
    side_manifest = stored(dropping.root, lambda m: gone not in spaces_named(m))
    assert spaces_named(side_manifest) == {kept}
    assert not [path for path in side_manifest["files"] if f"/layers/{gone}/" in path]

    dropping.server.compact()
    prefix = live_prefix(dropping.root)
    assert not layer_space_dir(prefix, gone).exists()
    assert layer_space_dir(prefix, kept).is_dir()
    assert spaces_named(newest_side_manifest(dropping.root)) == {kept}


def test_the_name_registers_again_as_a_new_layer(dropping):
    old_ids = dict(dropping.ids[A])
    record = dropping.register(A)
    assert record["space"] == 2, "a space is never given twice"
    assert not set(dropping.every_id(A)) & set(dropping.old)
    for (level, key), mosaica_id in dropping.ids[A].items():
        new_space, new_entity = identity_mod.invert_artifact(dropping.key, mosaica_id)
        _, old_entity = identity_mod.invert_artifact(dropping.key, old_ids[(level, key)])
        assert (new_space, new_entity) == (2, old_entity), (level, key)
        assert artifact_card(dropping.server, dropping.token, mosaica_id)[0] == 200
    check_dropped(dropping, dropping.old, names_gone=False)


def test_a_restart_brings_none_of_it_back(dropping):
    dropping.restart()
    check_dropped(dropping, dropping.old, names_gone=False)
    for mosaica_id in dropping.ids[A].values():
        assert artifact_card(dropping.server, dropping.token, mosaica_id)[0] == 200
    side_manifest = newest_side_manifest(dropping.root)
    assert dropping.spaces[A] not in spaces_named(side_manifest)
    assert side_manifest["next_layer_space"] == 3
    assert dropping.register(C)["space"] == 3


def test_a_drop_acknowledged_before_a_crash_is_replayed(dropping):
    server = dropping.server
    space = 3
    c0 = dropping.ids[C][(0, "c0")]
    assert artifact_card(server, dropping.token, c0)[0] == 200
    resp = drop(server, C, wait=False)
    assert resp.status_code == 200, resp.text
    dropping.restart(crash=True)

    server, token = dropping.server, dropping.token
    assert C not in [layer["name"] for layer in server.meta(token)["layers"]]
    no_artifact = artifact_card(server, token, dropping.no_artifact)
    for mosaica_id in dropping.every_id(C):
        assert artifact_card(server, token, mosaica_id) == no_artifact
    # The drop's own publication may not have reached the disc before the kill. The compaction
    # publishes what the replay left, which holds nothing of the layer.
    server.compact()
    assert space not in spaces_named(newest_side_manifest(dropping.root))
    assert not layer_space_dir(live_prefix(dropping.root), space).exists()
