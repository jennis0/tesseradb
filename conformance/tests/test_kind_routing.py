"""**An identifier's kind decides what it can name** (`docs/sharding.md` §2.2, §2.4), black-box,
over the release binary and the mask catalogue.

An artifact's and a layer's `mosaica_id` permute kind 1, the layer's space and the entity in it; an
item's permutes kind 0, its shard, its tenancy and its number. The catalogue holds item numbers 0
and 65,536. The layer registered here is the deployment's first, so it takes space 0, and it is
entity 0 there; its first artifact is entity 65,536. Each differs from an item's input by the kind
bit alone, which is the sharpest case for a route that reads one kind as the other.

What is asserted, and by which second reader:

- **Every identifier the server issued is the oracle's.** The oracle forwards the layer's space and
  its level's runs, read from the side-manifest the server published, under the bundle's key, and
  each receipt carries exactly those identifiers.
- **Each card answers its own kind**, and answers the other kind's identifier byte for byte as it
  answers one nobody was issued.
- **A deny on one kind leaves the other kind served**, at the same entity number.
- **A write naming an artifact or the layer names no item**: an ingest row and a member are refused
  as naming no item, as an identifier nobody was issued is.
- **An artifact-taking leaf given an item's identifier names nothing.**
- All of it holds after a restart.
"""

from __future__ import annotations

import io
import time

import pyarrow as pa
import pytest
import requests
from oracle import catalogue
from oracle import identity as identity_mod
from oracle.bundle import (
    artifact_mosaica_id,
    layer_mosaica_id,
    newest_side_manifest,
    registered_layer,
)
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport, split_items_frames
from pyarrow import ipc

VIEW = catalogue.VIEW_ID
LAYER = "routing"
KIND_ARTIFACT_HIGH = identity_mod.pack_artifact_high(0)
#: The members of each artifact, by item number. Artifact `i` is entity 65,536 + i of the layer's
#: space; the item at that number is a member of `a0` and `a2`, and not of `a1`.
MEMBERS = {"a0": [0, 65_536, 1, 2], "a1": [10, 11], "a2": [65_538, 12]}


def layer_declaration(name: str, levels: int = 0) -> dict:
    """An enumerated layer over the catalogue's view, with no gate and no content: `flat` with no
    levels, or `stacked` with `levels` of them."""
    return {
        "name": name,
        "title": name,
        "views": [VIEW],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": None,
        "artifact_visibility": {"field": None, "default": "inherited"},
        "require_member_visibility": None,
        "hierarchy": {"kind": "stacked" if levels else "flat", "prune_children": False},
        "content": {"computed": [], "supplied": []},
        "depends_on": [],
        "levels": [{"level": level} for level in range(levels)],
        "layout": None,
        "shape": None,
    }


def stored(root, holds, timeout: float = 30.0) -> dict:
    """The newest side-manifest of the bundle at `root` once `holds` is true of it. A running
    server publishes its side-manifest after it answers the write that changed it."""
    deadline = time.monotonic() + timeout
    while True:
        side_manifest = newest_side_manifest(root)
        if holds(side_manifest):
            return side_manifest
        assert time.monotonic() < deadline, "the server never published the state asked for"
        time.sleep(0.1)


def stored_layer(root, name: str) -> dict:
    """The record of the layer `name` once a side-manifest holds it."""
    side_manifest = stored(root, lambda m: registered_layer(m, name) is not None)
    return registered_layer(side_manifest, name)


def published(server) -> None:
    """Return once readers see every write the server answered before this call: a flush asked
    for with `wait=visible`, whose answer waits for that publication."""
    resp = requests.post(
        f"{server.control_base}/control/flush",
        params={"wait": "visible"},
        headers={"Authorization": f"Bearer {server.operator_credential}"},
        timeout=60,
    )
    resp.raise_for_status()
    assert resp.status_code == 202 and resp.json()["visible"], resp.text


def every_term() -> list[str]:
    """Every block's term: a principal holding them all sees every catalogue item."""
    return [block.descriptor for block in catalogue.BLOCKS.values()]


def item_card(server, token: str, mosaica_id: int) -> tuple[int, bytes]:
    resp = server.item(token, mosaica_id)
    return resp.status_code, resp.content


def artifact_card(server, token: str, mosaica_id: int) -> tuple[int, bytes]:
    resp = server.artifact_card(token, mosaica_id, VIEW)
    return resp.status_code, resp.content


def masked_count(server, token: str, mosaica_id: int) -> int:
    resp = server.artifact_card(token, mosaica_id, VIEW)
    assert resp.status_code == 200, resp.text
    return resp.json()["masked_count"]


def matched(server, token: str, filters: dict | None) -> int:
    resp = server.viewport_response(token, VIEW, 0, catalogue.FULL_VIEWPORT, k=0, filters=filters)
    tiles, _points = decode_viewport(resp.content)
    return sum(t[2] for t in tiles)


def read_by_ids(server, token: str, ids: list[int]) -> list[int]:
    """The identifiers `POST /v1/artifacts` serves for `ids`, read whole."""
    resp = server.artifacts(
        token, view=VIEW, layer=LAYER, fields=["masked_count"], ids=[str(i) for i in ids]
    )
    assert resp.status_code == 200, resp.text
    served = []
    for records, _end in split_items_frames(resp.content).pages:
        with ipc.open_stream(io.BytesIO(records)) as reader:
            for batch in reader:
                served.extend(batch.column("mosaica_id").to_pylist())
    return served


def refusals(body: dict) -> list[tuple[int, str]]:
    return [(row["row"], row["reason"]) for row in body["refused"]]


class Routing:
    """The server, the identifiers it issued and those the oracle derives for the catalogue."""

    def __init__(self, tmp_path_factory, private_catalogue_bundle, catalogue_bundle):
        self.root = private_catalogue_bundle("kind-routing")
        self.tmp = tmp_path_factory.mktemp("kind-routing-server")
        self.key = catalogue_bundle.identity_key
        numbers = {1, 65_537, *(n for members in MEMBERS.values() for n in members)}
        self.item = {n: catalogue_bundle.mosaica_id_of(n) for n in numbers}
        #: Issued to nobody: item number `N_ITEMS`, and entity 65,536 of a space no layer holds.
        self.no_item = identity_mod.forward_item(self.key, 0, 0, catalogue.N_ITEMS)
        self.no_artifact = identity_mod.forward_artifact(self.key, 9_999, 65_536)
        #: Artifacts deleted on the way, which name nothing from then on.
        self.gone: list[int] = []
        self.start()
        self.token = self.server.authorise(every_term())["token"]

    def start(self) -> None:
        self.server, self.proc = spawn_server(self.root, self.tmp)

    def restart(self) -> None:
        stop_server(self.proc)
        self.start()
        self.token = self.server.authorise(every_term())["token"]

    def members(self, key: str) -> dict:
        return {"mosaica_id": [str(self.item[n]) for n in MEMBERS[key]]}


@pytest.fixture(scope="module")
def routing(tmp_path_factory, private_catalogue_bundle, catalogue_bundle):
    fixture = Routing(tmp_path_factory, private_catalogue_bundle, catalogue_bundle)
    server = fixture.server
    resp = server.register_layer(layer_declaration(LAYER))
    assert resp.status_code == 201, resp.text
    fixture.layer = int(resp.json()["mosaica_id"])
    resp = server.publish_artifacts(
        LAYER, strict=True, artifacts=[{"key": k, "members": fixture.members(k)} for k in MEMBERS]
    )
    assert resp.status_code == 201, resp.text
    fixture.artifacts = [int(row["mosaica_id"]) for row in resp.json()["artifacts"]]
    published(server)
    yield fixture
    stop_server(fixture.proc)


def test_every_identifier_the_server_issued_is_the_oracles(routing):
    record = stored_layer(routing.root, LAYER)
    assert record["space"] == 0, "the deployment's first layer takes space 0"
    assert routing.layer == layer_mosaica_id(routing.key, record)
    assert routing.artifacts == [
        artifact_mosaica_id(routing.key, record, 0, ordinal) for ordinal in range(len(MEMBERS))
    ]
    # The layer and item 0, and the first artifact and item 65,536, differ by the kind bit alone.
    assert identity_mod.invert(routing.key, routing.layer) == (KIND_ARTIFACT_HIGH, 0)
    assert identity_mod.invert(routing.key, routing.item[0]) == (0, 0)
    assert identity_mod.invert(routing.key, routing.artifacts[0]) == (KIND_ARTIFACT_HIGH, 65_536)
    assert identity_mod.invert(routing.key, routing.item[65_536]) == (0, 65_536)


def check_cards(routing) -> None:
    server, token = routing.server, routing.token
    no_item = item_card(server, token, routing.no_item)
    assert no_item[0] == 404
    for named in (routing.layer, *routing.artifacts):
        assert item_card(server, token, named) == no_item, named
    for number in (0, 65_536):
        assert item_card(server, token, routing.item[number])[0] == 200

    no_artifact = artifact_card(server, token, routing.no_artifact)
    assert no_artifact[0] == 404
    for named in (routing.item[0], routing.item[65_536], routing.layer, *routing.gone):
        assert artifact_card(server, token, named) == no_artifact, named
    for named in routing.artifacts:
        assert artifact_card(server, token, named)[0] == 200


def test_each_card_answers_its_own_kind_and_the_other_kind_as_naming_nothing(routing):
    check_cards(routing)


def check_leaves(routing) -> None:
    server, token = routing.server, routing.token
    a0 = routing.artifacts[0]

    def leaf(mosaica_id: int) -> dict:
        return {"member_of": {"layer": LAYER, "artifact": str(mosaica_id)}}

    assert matched(server, token, leaf(a0)) == masked_count(server, token, a0) > 0
    for named in (routing.item[0], routing.item[65_536], routing.layer, routing.no_artifact):
        assert matched(server, token, leaf(named)) == 0, named

    assert read_by_ids(server, token, [a0]) == [a0]
    assert read_by_ids(server, token, [routing.item[0], routing.item[65_536], routing.layer]) == []

    def children(parent: int) -> list:
        resp = server.browse(token, view=VIEW, layer=LAYER, parent=str(parent))
        return [resp.status_code, resp.json()]

    for named in (routing.item[0], routing.item[65_536]):
        assert children(named) == children(routing.no_artifact), named


def test_an_artifact_taking_leaf_given_an_items_identifier_names_nothing(routing):
    check_leaves(routing)


def test_a_write_naming_an_artifact_or_the_layer_names_no_item(routing):
    server = routing.server
    a0, layer = routing.artifacts[0], routing.layer
    high_water = server.status()["entity_id_high_water"]

    def rows(ids: list[int]) -> bytes:
        table = pa.table(
            {
                "mosaica_id": pa.array([str(i) for i in ids], pa.string()),
                "x": pa.array([1.0] * len(ids), pa.float64()),
                "y": pa.array([1.0] * len(ids), pa.float64()),
                "access": pa.array([[every_term()[0]]] * len(ids), pa.list_(pa.string())),
            }
        )
        sink = io.BytesIO()
        with ipc.new_stream(sink, table.schema) as writer:
            writer.write_table(table)
        return sink.getvalue()

    resp = server.ingest(rows([a0, layer, routing.no_item]), "routing-rows")
    assert resp.status_code == 200, resp.text
    assert refusals(resp.json()) == [(row, "unknown_mosaica_id") for row in range(3)]
    unknown = server.ingest(rows([routing.no_item]), "routing-strict-unknown", strict=True)
    assert unknown.status_code == 409, unknown.text
    for batch, named in (("routing-strict-artifact", a0), ("routing-strict-layer", layer)):
        resp = server.ingest(rows([named]), batch, strict=True)
        assert resp.status_code == unknown.status_code, resp.text
    assert server.status()["entity_id_high_water"] == high_water

    members = {"mosaica_id": [str(a0), str(layer), str(routing.no_item), str(routing.item[1])]}
    resp = server.publish_artifacts(LAYER, artifacts=[{"key": "b", "members": members}])
    assert resp.status_code == 201, resp.text
    assert refusals(resp.json()) == [(row, "unknown_mosaica_id") for row in range(3)]
    (b,) = (int(row["mosaica_id"]) for row in resp.json()["artifacts"])
    record = stored_layer(routing.root, LAYER)
    assert b == artifact_mosaica_id(routing.key, record, 0, len(MEMBERS))
    routing.artifacts.append(b)
    published(server)
    assert masked_count(server, routing.token, b) == 1

    def strict(named: int) -> int:
        resp = server.publish_artifacts(
            LAYER, strict=True, artifacts=[{"key": "c", "members": {"mosaica_id": [str(named)]}}]
        )
        return resp.status_code

    assert strict(routing.no_item) == 404
    assert strict(a0) == strict(layer) == 404


def test_a_deny_on_one_kind_leaves_the_other_kind_served(routing):
    server, token = routing.server, routing.token
    a0, a1, a2 = routing.artifacts[:3]
    visible = matched(server, token, None)

    def layers() -> list[str]:
        return [layer["name"] for layer in server.meta(token)["layers"]]

    def change(named: int, op: str) -> None:
        resp = server.change(named, op, strict=True)
        assert resp.status_code == 200, resp.text

    # The layer, entity 0 of space 0, and item 0.
    change(routing.layer, "suppress")
    assert LAYER not in layers()
    assert item_card(server, token, routing.item[0])[0] == 200
    assert matched(server, token, None) == visible
    change(routing.layer, "unsuppress")
    assert LAYER in layers()

    before = masked_count(server, token, a0)
    change(routing.item[0], "suppress")
    assert LAYER in layers()
    assert masked_count(server, token, a0) == before - 1
    change(routing.item[0], "unsuppress")
    assert masked_count(server, token, a0) == before

    # Entity 65,537 of the space, and item 65,537; entity 65,538 and item 65,538.
    change(a1, "delete")
    assert item_card(server, token, routing.item[65_537])[0] == 200
    before = masked_count(server, token, a2)
    change(routing.item[65_538], "delete")
    assert masked_count(server, token, a2) == before - 1
    assert matched(server, token, None) == visible - 1
    routing.artifacts.remove(a1)
    routing.gone.append(a1)
    del routing.item[65_538]
    check_cards(routing)


def test_a_restart_and_a_compaction_change_none_of_it(routing):
    routing.server.compact()
    routing.restart()
    record = stored_layer(routing.root, LAYER)
    assert routing.layer == layer_mosaica_id(routing.key, record)
    assert routing.artifacts[0] == artifact_mosaica_id(routing.key, record, 0, 0)
    check_cards(routing)
    check_leaves(routing)
