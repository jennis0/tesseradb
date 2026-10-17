"""**A deleted artifact is served on no route, and leaves the disc at the compaction**, black-box,
over the release binary and the mask catalogue.

Layer `erase` holds two artifacts with supplied content, `kept` and `erased`. `erased` is deleted:
from the acknowledgement it answers every artifact route as an identifier never issued would, while
its record and content stay on the disc, held. The compaction removes them, and a restart before
and after changes no answer.
"""

from __future__ import annotations

import pytest
from layer_fixture import VIEW, CatalogueServer, artifact_card, bulk_column, enumerated_layer, matched
from oracle import catalogue
from oracle import identity as identity_mod
from oracle.layout import live_prefix_dir, newest_side_manifest
from oracle.wire import decode_viewport_artifacts
from pyroaring import BitMap

LAYER = "erase"
#: The deleted artifact's key: bytes no other file of the bundle holds.
ERASED = "erased-cluster-7f3a9c"
KEPT = "kept-cluster"
#: The items each artifact holds, by catalogue item number; the first half of each generates its
#: content.
MEMBERS = {KEPT: list(range(0, 40)), ERASED: list(range(40, 80))}


def layer() -> dict:
    return {
        **enumerated_layer(LAYER, VIEW),
        "content": {
            "computed": [],
            "supplied": [{"name": "topic", "type": "text", "require_member_visibility": "all"}],
        },
    }


class Erasing(CatalogueServer):
    """The server over a private copy of the catalogue, and the identifiers the layer issued."""

    def __init__(self, tmp_path_factory, private_catalogue_bundle, catalogue_bundle):
        super().__init__("artifact-delete", tmp_path_factory, private_catalogue_bundle, catalogue_bundle)
        resp = self.server.register_layer(layer())
        assert resp.status_code == 201, resp.text
        ids = lambda items: {"mosaica_id": [str(self.bundle.mosaica_id_of(n)) for n in items]}  # noqa: E731
        body = [
            {
                "key": key,
                "members": ids(items),
                "content": [
                    {"values": [f"topic of {key}"], "generated_from": ids(items[: len(items) // 2])}
                ],
            }
            for key, items in MEMBERS.items()
        ]
        resp = self.server.publish_artifacts(LAYER, strict=True, level=0, artifacts=body)
        assert resp.status_code == 201, resp.text
        self.ids = {row["key"]: int(row["mosaica_id"]) for row in resp.json()["artifacts"]}
        self.space, self.erased_entity = identity_mod.invert_artifact(self.key, self.ids[ERASED])
        self.server.published()

    def routes(self) -> dict:
        """What the session holding every term is served on every artifact route: the keys each
        listing serves, and each artifact's card and `member_of` and `region` answers beside an
        identifier never issued."""
        server, token = self.server, self.token
        viewport = decode_viewport_artifacts(
            server.artifacts_viewport(token, VIEW, 0, catalogue.FULL_VIEWPORT)
        )
        browse = server.browse(token, view=VIEW, layer=LAYER)
        assert browse.status_code == 200, browse.text

        def member_of(mosaica_id: int) -> int:
            return matched(server, token, {"member_of": {"layer": LAYER, "artifact": str(mosaica_id)}})

        each = {**self.ids, "never issued": self.no_artifact}
        return {
            "viewport": sorted(a.key for a in viewport if a.layer == LAYER),
            "browse": sorted(row["key"] for row in browse.json()["artifacts"]),
            "bulk": sorted(bulk_column(server, token, LAYER, "key")),
            "card": {name: artifact_card(server, token, i) for name, i in each.items()},
            "member_of": {name: member_of(i) for name, i in each.items()},
            "region": {
                name: matched(server, token, {"region": {"artifact": str(i)}})
                for name, i in each.items()
            },
        }

    def stored(self) -> tuple[bool, bool, int]:
        """Whether any file of the live prefix holds the erased artifact's key, whether any of its
        space's content extents holds a row at its entity, and the deletions the status counts."""
        prefix = live_prefix_dir(self.root)
        holds_key = any(
            ERASED.encode() in path.read_bytes() for path in prefix.rglob("*") if path.is_file()
        )
        side_manifest = newest_side_manifest(self.root)
        extents = [
            extent
            for record in side_manifest["layer_spaces"]
            if record["space"] == self.space
            for extent in record["content_extents"]
        ]
        holds_content = any(
            self.erased_entity in BitMap.deserialize((prefix / extent["hasrow"]).read_bytes())
            for extent in extents
        )
        return holds_key, holds_content, self.server.status()["layer_spaces"]["retirable"]


@pytest.fixture(scope="module")
def erasing(tmp_path_factory, private_catalogue_bundle, catalogue_bundle):
    fixture = Erasing(tmp_path_factory, private_catalogue_bundle, catalogue_bundle)
    yield fixture
    fixture.stop()


def check_served_on_no_route(routes: dict) -> None:
    for listing in ("viewport", "browse", "bulk"):
        assert routes[listing] == [KEPT], listing
    for verdict in ("card", "member_of", "region"):
        assert routes[verdict][ERASED] == routes[verdict]["never issued"], verdict
    assert routes["card"][KEPT][0] == 200
    assert routes["member_of"][KEPT] == len(MEMBERS[KEPT])


def test_a_deleted_artifact_is_served_on_no_route_and_held_until_the_compaction(erasing):
    before = erasing.routes()
    assert ERASED in before["viewport"] and before["card"][ERASED][0] == 200
    assert erasing.stored() == (True, True, 0)

    resp = erasing.server.change(erasing.ids[ERASED], "delete", strict=True)
    assert resp.status_code == 200, resp.text
    erasing.server.published()
    erasing.deleted = erasing.routes()
    check_served_on_no_route(erasing.deleted)
    assert erasing.stored() == (True, True, 1), "held, and counted, until a compaction"

    erasing.restart()
    assert erasing.routes() == erasing.deleted
    assert erasing.stored() == (True, True, 1)


def test_the_compaction_removes_its_record_and_content(erasing):
    erasing.server.compact()
    assert erasing.routes() == erasing.deleted
    assert erasing.stored() == (False, False, 0)


def test_a_restart_after_the_compaction_brings_none_of_it_back(erasing):
    erasing.restart()
    assert erasing.routes() == erasing.deleted
    assert erasing.stored() == (False, False, 0)
