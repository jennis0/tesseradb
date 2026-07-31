"""The independent differential oracle's harness (Task 14).

Five things are proven, kept deliberately separate (task brief):

(a) morton.u32 byte-for-byte vs the oracle's own recomputation from columns.arrow's x/y
    (contracts §2.5's obligation).
(b) postings-derived server counts == the oracle's pairs-derived brute-force counts, for every
    tile a random viewport touches — the union-vs-semi-join differential, and the entity-space
    vs row-space-diff composition-equivalence check.
(c) point sets: server handles are opaque, so points are compared as (x, y) multisets against the
    oracle's own `served` (design §7.2's definition, implemented independently — see
    `oracle/viewport.py`'s doc).
(d) suppress over the control plane -> oracle told to drop it -> counts re-agree.
(e) a mixed-change composition stress (delete, suppress, predicate-widen onto an entity the grant
    set previously missed, predicate-narrow) -> all counts re-agree; the oracle composes in
    entity space per I1's formula, the server composes as row-space diffs.

20 random grant sets (mixed sizes, one empty, one huge) x 10 random viewports at zooms 3-8, per
the brief; the RNG is seeded for reproducibility.
"""

from __future__ import annotations

import base64
import os
import random
from collections import Counter

import pytest

from oracle import mask as mask_mod
from oracle import morton
from oracle import viewport as vp
from oracle.bundle import Bundle

from .wire import decode_viewport

SLICE = "s0"
SEED = 20260728
# Task 16, Step 4: the 10^9-bundle exit run uses a reduced sample count (the brief's "5 grants x
# 5 viewports" -- the independent Python oracle is slow by design, and re-deriving a mask from
# 1.72B pairs per grant set is not something to do 20x at that scale). Overridable via env so the
# default 250k invocation (CI, everyday `pytest`) is unaffected.
N_GRANT_SETS = int(os.environ.get("TESSERA_DIFFERENTIAL_N_GRANTS", "20"))
N_VIEWPORTS_PER_GRANT = int(os.environ.get("TESSERA_DIFFERENTIAL_N_VIEWPORTS", "10"))
ZOOM_RANGE = (3, 8)
GRID_MAX = 65536.0


@pytest.fixture(scope="session")
def oracle_bundle(bundle_root) -> Bundle:
    return Bundle(bundle_root)


def _descriptor_str(d: bytes) -> str:
    return d.decode("ascii")


def _random_grant_sets(descriptors: list[bytes], rng: random.Random) -> list[list[bytes]]:
    """20 grant sets: one empty, one everything, the rest random subsets of mixed size."""
    sets: list[list[bytes]] = [[]]
    sets.append(list(descriptors))
    while len(sets) < N_GRANT_SETS:
        size = rng.randint(1, len(descriptors))
        sets.append(rng.sample(descriptors, size))
    return sets


def _random_bbox(rng: random.Random) -> tuple[float, float, float, float]:
    x0 = rng.uniform(0, GRID_MAX)
    x1 = rng.uniform(0, GRID_MAX)
    y0 = rng.uniform(0, GRID_MAX)
    y1 = rng.uniform(0, GRID_MAX)
    return (min(x0, x1), min(y0, y1), max(x0, x1), max(y0, y1))


def test_morton_matches_byte_for_byte(oracle_bundle: Bundle):
    """(a) morton.u32 vs the oracle's own recomputation from columns.arrow x/y."""
    seg = oracle_bundle.segment(SLICE)
    for i in range(0, seg.row_count, max(1, seg.row_count // 5000)):
        recomputed = morton.morton_of(float(seg.x[i]), float(seg.y[i]), oracle_bundle.extent)
        assert recomputed == int(seg.morton[i]), f"row {i}: morton mismatch"


def test_grid_differential(server, oracle_bundle: Bundle):
    """(b) and (c) with theta SATURATED: masking and counts, isolated from the density rule."""
    _grid_differential(server, oracle_bundle)


def test_grid_differential_with_theta_live(density_server, oracle_bundle: Bundle):
    """(b) and (c) with theta LIVE — the configuration §7.2's density rule actually ships in.

    **This is the case that makes the point-set half of the differential mean anything.** Under
    saturation both sides reduce to "serve everything visible", so the comparison cannot distinguish
    this engine from one that anchors theta on the pre-overlay projection (the I2 breach), gets the
    x4 depth progression wrong, or drops the threshold clause. Here the oracle computes a real cut
    from a real anchor and the engine must agree tile by tile.

    The assertion below (`saw_partial`) is what stops this test quietly reverting to the vacuous
    case if the fixture size, the zoom range or `theta_target_marks` ever drift.
    """
    _grid_differential(density_server, oracle_bundle, require_partial=True)


def _grid_differential(server, oracle_bundle: Bundle, *, require_partial: bool = False):
    """The 20 grant sets x 10 viewports matrix, against whichever server fixture is passed."""
    rng = random.Random(SEED)
    descriptors = oracle_bundle.dictionary
    grant_sets = _random_grant_sets(descriptors, rng)

    tiles_checked = 0
    points_checked = 0
    # Tiles where the threshold clause actually bound — i.e. the engine served STRICTLY FEWER than
    # were visible, and not because the cap truncated. Under saturated theta this stays 0, which is
    # exactly the degeneracy `require_partial` exists to catch.
    saw_partial = 0

    for grant in grant_sets:
        terms_str = [_descriptor_str(d) for d in grant]
        term_ids = {oracle_bundle.term_id_of(d) for d in grant}
        term_ids.discard(None)

        auth = server.authorise(terms_str)
        token = auth["token"]

        base_mask = mask_mod.mask_of(term_ids, oracle_bundle.pairs_path())

        # Cross-check (b)'s other half in miniature: postings-derived union for these terms must
        # equal the pairs-derived one.
        postings_union = set()
        for term_id in term_ids:
            postings_union |= set(oracle_bundle.postings(term_id).tolist())
        assert postings_union == base_mask, "postings union must equal the pairs-derived mask"

        # The deployment constants §7.2's definition needs. They are config, so the oracle cannot
        # know them; `GET /v1/meta` publishes them precisely so an independent implementation can
        # reproduce the served set. theta's anchor is computed independently below, not read back.
        constants = server.meta(token)["selection"]
        v_total = _oracle_visible_total(oracle_bundle, base_mask, SLICE)

        for _ in range(N_VIEWPORTS_PER_GRANT):
            zoom = rng.randint(*ZOOM_RANGE)
            bbox = _random_bbox(rng)
            k = 200  # max_k (Reference Sheet R1) — exercise the full per-tile sampling budget

            raw = server.viewport(token, SLICE, zoom, bbox, k=k)
            server_tiles, server_points = decode_viewport(raw)

            # ONE `Selection` per request, asked about each of its tiles — see
            # `oracle/viewport.py`'s doc. Both halves of this comparison, the counts and the point
            # sets, then provably read the same `vis(T)`: §7.1 discloses that set's size and §7.2
            # selects from it, so deriving them by two routes would let a disagreement between them
            # hide in the oracle. It is also what keeps the literal definition affordable — the
            # free `viewport.served()` makes a whole pass per tile.
            oracle = vp.Selection(oracle_bundle, base_mask, SLICE, zoom)
            oracle_tile_counts = oracle.counts_for(
                morton.tiles_for_bbox(bbox, zoom, oracle_bundle.extent)
            )

            server_tile_map = {t: v for t, v, m, _s in server_tiles}
            for t, v, m, _s in server_tiles:
                assert v == m, "Phase 1 has no filters: matched must equal visible"
            assert server_tile_map == oracle_tile_counts, (
                f"tile counts disagree for zoom={zoom} bbox={bbox} grant_size={len(grant)}: "
                f"server={server_tile_map} oracle={oracle_tile_counts}"
            )
            tiles_checked += len(oracle_tile_counts)

            # (c): split the flat points list back into per-tile groups. The split key is the tile
            # batch's own `served` column (contracts r7), NOT `min(k, visible)` — under §7.2's
            # density rule the per-tile count is min(cap, max(k_min, C_theta)) clamped to visible,
            # which cannot be recomputed from k and visible alone. That is the whole reason `served`
            # is on the wire.
            cursor = 0
            for t, _visible, _matched, served_n in server_tiles:
                tile_points = server_points[cursor : cursor + served_n]
                cursor += served_n
                # Counter, not set: two distinct entities can share rounded coordinates within a
                # tile, and a set would silently absorb a server bug that dropped one of them
                # while duplicating another (the brief calls for a multiset comparison here).
                server_xy = Counter((round(x, 4), round(y, 4)) for _h, x, y in tile_points)

                oracle_xy_list = oracle.served_points(
                    t, **vp.params_from_meta(constants, k=k, v_total=v_total)
                )
                oracle_xy = Counter((round(x, 4), round(y, 4)) for x, y in oracle_xy_list)

                assert server_xy == oracle_xy, (
                    f"point multiset disagrees for tile {t} zoom={zoom}: "
                    f"server={server_xy} oracle={oracle_xy}"
                )
                points_checked += len(tile_points)
                if 0 < served_n < _visible_of(server_tiles, t):
                    saw_partial += 1
            assert cursor == len(server_points), "points list must be exactly consumed by tiles"

    assert tiles_checked > 0
    assert points_checked > 0
    if require_partial:
        assert saw_partial > 0, (
            "no tile served strictly fewer points than it had visible, so theta never bound and "
            "this comparison degenerated to 'serve everything visible' — it can no longer "
            "distinguish a correct density rule from a missing one. Check the fixture size, "
            "ZOOM_RANGE, and the density_server fixture's theta_target_marks."
        )


def _visible_of(server_tiles, tile):
    for t, v, _m, _s in server_tiles:
        if t == tile:
            return v
    raise AssertionError(f"tile {tile} not in the tiles batch")


def _oracle_visible_total(bundle, mask, slice_id):
    return vp.visible_total(bundle, mask, slice_id)


def _oracle_counts(bundle, base_mask, slice_id, zoom, bbox):
    return vp.counts(bundle, base_mask, slice_id, zoom, bbox)


def test_suppress_over_control_plane_drops_the_count(server, oracle_bundle: Bundle):
    """(d) suppress over the control plane -> oracle told to drop it -> counts re-agree."""
    descriptor = oracle_bundle.dictionary[0]
    term_id = oracle_bundle.term_id_of(descriptor)
    base_mask = mask_mod.mask_of({term_id}, oracle_bundle.pairs_path())
    assert len(base_mask) > 1, "need at least two visible entities to suppress one meaningfully"

    auth = server.authorise([_descriptor_str(descriptor)])
    token = auth["token"]

    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
    zoom = 4

    raw_before = server.viewport(token, SLICE, zoom, bbox, k=200)
    tiles_before, _ = decode_viewport(raw_before)
    counts_before = {t: v for t, v, m, _s in tiles_before}
    oracle_before = _oracle_counts(oracle_bundle, base_mask, SLICE, zoom, bbox)
    assert counts_before == oracle_before

    target_entity = min(base_mask)
    external_id_b64 = base64.b64encode(oracle_bundle.external_id_of(target_entity)).decode()
    resp = server.change(external_id_b64, "suppress")
    assert resp.status_code == 200, resp.text

    changes = mask_mod.ChangeSet()
    changes.apply(target_entity, "suppress")

    raw_after = server.viewport(token, SLICE, zoom, bbox, k=200)
    tiles_after, _ = decode_viewport(raw_after)
    counts_after = {t: v for t, v, m, _s in tiles_after}
    resolved_mask = changes.resolve(base_mask, {term_id})
    oracle_after = _oracle_counts(oracle_bundle, resolved_mask, SLICE, zoom, bbox)

    assert counts_after == oracle_after
    assert sum(counts_after.values()) == sum(counts_before.values()) - 1

    # Clean up: unsuppress so later tests in this module see the original state.
    resp = server.change(external_id_b64, "unsuppress")
    assert resp.status_code == 200, resp.text


def test_mixed_change_composition_stress(server, oracle_bundle: Bundle):
    """(e) delete, suppress, predicate-widen onto an entity the grant set previously missed, and
    predicate-narrow, composed together — the oracle in entity space per I1, the server as
    row-space diffs. Agreement here is the equivalence proof for Task 10's retirement rules."""
    dictionary = oracle_bundle.dictionary
    term_a = 0
    term_b = 1 if len(dictionary) > 1 else 0
    session_terms = {term_a}

    base_mask = mask_mod.mask_of(session_terms, oracle_bundle.pairs_path())
    assert len(base_mask) >= 4, "fixture must have enough term-0 members for this stress test"

    auth = server.authorise([_descriptor_str(dictionary[term_a])])
    token = auth["token"]

    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
    zoom = 4

    changes = mask_mod.ChangeSet()

    ordered_mask = sorted(base_mask)
    delete_entity = ordered_mask[0]
    suppress_entity = ordered_mask[1]
    narrow_entity = ordered_mask[2]  # currently has term_a; predicate-change removes it

    # An entity NOT already in base_mask (does not carry term_a) — predicate-widen grants it
    # term_a directly, independent of pairs.parquet's original labelling.
    not_granted = mask_mod.mask_of({term_b}, oracle_bundle.pairs_path()) - base_mask
    assert not_granted, "fixture must have an entity with term_b but not term_a"
    widen_entity = min(not_granted)

    batch = [
        (delete_entity, "delete", None),
        (suppress_entity, "suppress", None),
        (widen_entity, "predicate", {term_a}),
        (narrow_entity, "predicate", set()),
    ]

    payload = []
    for entity_id, op, terms in batch:
        external_id_b64 = base64.b64encode(oracle_bundle.external_id_of(entity_id)).decode()
        item = {"external_id": external_id_b64, "op": op}
        if op == "predicate":
            # `builtin:passthrough`'s access label is the comma-joined decimal term ids (R6);
            # the dictionary's descriptor bytes for these ids are exactly those decimal strings.
            access = ",".join(dictionary[t].decode("ascii") for t in sorted(terms))
            item["access"] = access
        payload.append(item)
        changes.apply(entity_id, op, terms)

    import requests

    resp = requests.post(
        f"{server.control_base}/control/changes",
        headers={"Authorization": f"Bearer {server.operator_credential}"},
        json=payload,
        timeout=10,
    )
    assert resp.status_code == 200, resp.text

    resolved_mask = changes.resolve(base_mask, session_terms)
    assert delete_entity not in resolved_mask
    assert suppress_entity not in resolved_mask
    assert widen_entity in resolved_mask
    assert narrow_entity not in resolved_mask

    raw = server.viewport(token, SLICE, zoom, bbox, k=200)
    tiles, _ = decode_viewport(raw)
    server_counts = {t: v for t, v, m, _s in tiles}
    oracle_counts = _oracle_counts(oracle_bundle, resolved_mask, SLICE, zoom, bbox)

    assert server_counts == oracle_counts


def test_items_drilldown_returns_expected_external_id(server, oracle_bundle: Bundle):
    """`/v1/items` on a visible item returns the same `external_id` the oracle derives
    (Task 12 brief, Step 4). Requires a post-r6 bundle (`identity` in MANIFEST) so the
    oracle can compute `tessera_id_of` -- skipped against a pre-r6 fixture, which is what
    this repo's `tessera build` still produces as of this task (identity.rs is landing but
    tessera-build has not yet been repointed at it)."""
    if oracle_bundle.identity_key is None:
        pytest.skip("bundle has no `identity` object in MANIFEST (pre-r6 bundle)")

    descriptor = oracle_bundle.dictionary[0]
    term_id = oracle_bundle.term_id_of(descriptor)
    base_mask = mask_mod.mask_of({term_id}, oracle_bundle.pairs_path())
    assert base_mask, "need at least one visible entity"

    auth = server.authorise([_descriptor_str(descriptor)])
    token = auth["token"]

    # Take the LAST entity of the mask, not the first.
    #
    # `server` is session-scoped (a boot is expensive) and
    # `test_mixed_change_composition_stress` mutates `sorted(base_mask)[0..3]` of this same
    # mask — including a **delete**, which is terminal in Phase 1 by design: nothing clears
    # it, because deletion denies retire only via the epoch ledger and that does not exist
    # until compaction lands (lifecycle §3.1's three retirement rules). So `min(base_mask)`
    # is permanently invisible to every test that runs after the stress test, and this test
    # correctly got a 404 when it targeted it — the product was right and the fixture sharing
    # was wrong.
    #
    # Reserving disjoint ends of the mask is the cheap fix. Do not "fix" a future failure here
    # by unsuppressing or by re-ordering the file: a terminal delete cannot be undone, and
    # ordering dependence between tests sharing a mutable server is exactly what this comment
    # exists to stop someone reintroducing.
    target_entity = max(base_mask)
    tessera_id = oracle_bundle.tessera_id_of(target_entity)
    expected_external_id = oracle_bundle.external_id_of(target_entity)

    resp = server.item(token, tessera_id)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    got_external_id = base64.b64decode(body["external_id"])
    assert got_external_id == expected_external_id
