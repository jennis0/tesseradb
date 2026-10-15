"""What the annotation-layer tests over the mask catalogue share: a server over a private copy of
the catalogue, an enumerated layer's declaration, and the side-manifest the server publishes."""

from __future__ import annotations

import time

from oracle import catalogue
from oracle import identity as identity_mod
from oracle.harness import kill_server, spawn_server, stop_server
from oracle.layout import newest_side_manifest
from oracle.wire import decode_viewport

VIEW = catalogue.VIEW_ID
#: Every block's term: a principal holding them all sees every catalogue item.
EVERY_TERM = list(next(c for c in catalogue.catalogue() if c.name == "full_100pct").grants)


def enumerated_layer(name: str, view: str, levels: int = 0) -> dict:
    """An enumerated layer over `view`, with no gate and no content: `flat` with no levels, or
    `stacked` with `levels` of them."""
    return {
        "name": name,
        "title": name,
        "views": [view],
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


def artifact_card(server, token: str, mosaica_id: int) -> tuple[int, bytes]:
    resp = server.artifact_card(token, mosaica_id, VIEW)
    return resp.status_code, resp.content


def matched(server, token: str, filters: dict | None) -> int:
    resp = server.viewport_response(token, VIEW, 0, catalogue.FULL_VIEWPORT, k=0, filters=filters)
    tiles, _points = decode_viewport(resp.content)
    return sum(t[2] for t in tiles)


class CatalogueServer:
    """A server over a private copy of the catalogue named `name`, and a session holding every
    term."""

    def __init__(self, name: str, tmp_path_factory, private_catalogue_bundle, catalogue_bundle):
        self.root = private_catalogue_bundle(name)
        self.tmp = tmp_path_factory.mktemp(f"{name}-server")
        self.bundle = catalogue_bundle
        self.key = catalogue_bundle.identity_key
        #: Issued to nobody: entity 65,536 of a space no layer holds.
        self.no_artifact = identity_mod.forward_artifact(self.key, 9_999, 65_536)
        self.start()

    def start(self) -> None:
        self.server, self.proc = spawn_server(self.root, self.tmp)
        self.token = self.server.authorise(EVERY_TERM)["token"]

    def restart(self, crash: bool = False) -> None:
        (kill_server if crash else stop_server)(self.proc)
        self.start()

    def stop(self) -> None:
        stop_server(self.proc)
