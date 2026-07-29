"""I2 canary scaffold (plan §10.2, brief step 3): a term nobody holds must never affect what any
tested principal sees.

Builds a small synthetic dataset twice (`reference/oracle/canary_fixture.py`): once as-is
("canary-free"), and once with one extra item at an extreme corner of the quantisation extent
(`(65535.9, 65535.9)`, contracts §2.5: `v == max` lands in the top cell) carrying a term id no
grant set this test ever authorises holds. Two server instances are then run — one per bundle —
and, across every zoom level, every response is checked identical between the two: tile counts,
the tile *list*, and every visible point's `(x, y)`.

Because the canary item's term is held by nobody tested, admitting or denying it changes nothing
about any *other* item's visibility (masks are per-entity set membership; adding one brand-new,
ungranted term does not touch any other entity's grant), so the two bundles' responses should be
byte-for-byte equivalent at the decoded level for every session used here — the canary's own tile
should never appear in either bundle's response (0 visible members there, and a 0-count tile is
never emitted per the existing engine convention the differential suite already relies on). This
is the scaffold Phase 2 extends to centroids/hulls (module doc says so in the brief); Phase 1 has
no derived aggregates beyond raw tile counts and point sets to check.

**What is stripped before comparison, and why raw bytes cannot be compared directly.** Two
independent server instances mint independent session tokens, `token_id`s, and (per-response)
`x-tessera-pin` values — none of which are a function of anything semantically comparable between
the two bundles, so comparing HTTP bytes byte-for-byte would fail on token/pin churn alone, not on
anything meaningful. This test therefore never compares raw response bytes or headers at all: it
decodes each response via `oracle.wire.decode_viewport` and compares only the *tile* list (as a
`{tile: (visible, matched)}` mapping) and the *point* set (as a `(round(x, 4), round(y, 4))`
multiset, `Handle` values are per-session opaque and never compared — see I10's discussion in
`test_byte_scan.py`).
"""

from __future__ import annotations

from collections import Counter
from pathlib import Path

import pytest

from oracle.canary_fixture import CANARY_TERM_ID, N_BASE_ITEMS, build_canary_pair
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport

GRID_MAX = 65536.0
SLICE = "s0"
# Zooms 0-6 of the grid's full 0-16 depth range (contracts §2.5) — deep enough that the corner
# tile the canary occupies is a small, specific prefix distinct from its neighbours (checked
# below at the deepest zoom), shallow enough to keep the test fast. Not exhaustive over all 17
# depths; a scaffold, not an exhaustive per-depth sweep (see module doc: this is what Phase 2
# extends).
ZOOM_RANGE = range(0, 7)
# K = 500 is comfortably >= N_BASE_ITEMS (400), so no tile's point set is ever truncated by the
# `k` cap for either bundle — deliberately: this test is about comparing full membership between
# two bundles, not about exercising the first-k selector/priority-tiebreak machinery itself (that
# is Task 7/9's job). Worth stating explicitly: `k`-truncation and its interaction with the canary
# item's priority ordering is therefore NOT exercised by this test.
K = 500


@pytest.fixture(scope="module")
def canary_bundles(tmp_path_factory):
    work_dir = tmp_path_factory.mktemp("canary-fixture")
    free_bundle, canary_bundle = build_canary_pair(work_dir)
    return free_bundle, canary_bundle


@pytest.fixture(scope="module")
def canary_servers(tmp_path_factory, canary_bundles):
    free_bundle, canary_bundle = canary_bundles
    free_tmp = tmp_path_factory.mktemp("canary-free-serve")
    canary_tmp = tmp_path_factory.mktemp("canary-serve")

    free_srv, free_proc = spawn_server(free_bundle, free_tmp)
    canary_srv, canary_proc = spawn_server(canary_bundle, canary_tmp)
    yield free_srv, canary_srv
    stop_server(free_proc)
    stop_server(canary_proc)


def _decoded_viewport(server, token, zoom, bbox):
    raw = server.viewport(token, SLICE, zoom, bbox, k=K)
    tiles, points = decode_viewport(raw)
    tile_map = {t: (v, m) for t, v, m in tiles}
    point_multiset = Counter((round(x, 4), round(y, 4)) for _h, x, y in points)
    return tile_map, point_multiset


def test_ungranted_canary_term_never_changes_any_other_response(canary_bundles, canary_servers):
    free_bundle, canary_bundle = canary_bundles
    free_srv, canary_srv = canary_servers

    # Grant sets that never include the canary's own term id — every principal tested here is
    # exactly the population the canary claims never to affect.
    grant_sets = [
        [],  # zero-visibility token: deliberate (R5) — must still agree between the two bundles
        [str(t) for t in range(3)],
        [str(t) for t in range(6)],  # every base term, still not the canary's
    ]

    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)

    for terms in grant_sets:
        free_auth = free_srv.authorise(terms)
        canary_auth = canary_srv.authorise(terms)

        for zoom in ZOOM_RANGE:
            free_tiles, free_points = _decoded_viewport(free_srv, free_auth["token"], zoom, bbox)
            canary_tiles, canary_points = _decoded_viewport(
                canary_srv, canary_auth["token"], zoom, bbox
            )

            assert free_tiles == canary_tiles, (
                f"tile map disagreed for terms={terms} zoom={zoom}: "
                f"canary-free={free_tiles} canary={canary_tiles}"
            )
            assert free_points == canary_points, (
                f"point multiset disagreed for terms={terms} zoom={zoom}: "
                f"canary-free={free_points} canary={canary_points}"
            )

            # `free_tiles == canary_tiles` above already covers the corner tile the canary would
            # occupy (any additional visible content there would show up as a disagreement) — at
            # shallow zoom that tile spans a broad quadrant shared with plenty of base points, so
            # there is no zoom-independent single tile id to check in isolation; the map-level
            # equality is the actual scaffold assertion. At the deepest zoom tested, additionally
            # confirm the corner tile itself carries no visible members in either bundle (the
            # canary's sole membership is an ungranted term, and no base point was placed there).
            if zoom == max(ZOOM_RANGE):
                canary_tile_prefix = _tile_of(zoom)
                assert canary_tile_prefix not in free_tiles
                assert canary_tile_prefix not in canary_tiles


def _tile_of(zoom: int) -> int:
    """The depth-`zoom` tile the canary's extreme corner falls into: `cell(65535.9) == 65535`
    for both x and y (contracts §2.5, top cell), so its Morton code is the maximum representable
    code and its tile prefix at depth `zoom` is `(1 << (2*zoom)) - 1` (all-ones)."""
    return (1 << (2 * zoom)) - 1
