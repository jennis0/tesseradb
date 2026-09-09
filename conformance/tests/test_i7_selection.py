"""I7 — the sampler differential over the adversarial mask catalogue, with a negative control.

**I7: sampling happens after masking, never before.** The sample of an authorised set is not the
authorised portion of a global sample. §7.2 gives the definition — floor ∪ threshold ∪ cap over
`tessera_id`, every rank and every count taken over `vis(T)` — and `oracle/viewport.py` is that
definition written out literally. This module puts the two side by side across the catalogue ×
depths × `k`.

Four tests, and the last is the one that makes the others mean anything:

1. **the differential**, over the catalogue × depths × `k`, against θ saturated and θ live;
2. **cross-zoom nesting** — an item drawn in a parent tile is still drawn in whichever child
   contains it (§7.2's nesting argument, which is what makes a zoom not flicker);
3. **the cap**, against a server whose `K_max` is small enough to bind — every other run in this
   suite leaves `cap = min(k, K_max)` reducing to `k`, so the clause is otherwise dead;
4. **the negative control** — a first-`k` stub that ignores `tessera_id` ordering, and proof that
   the differential *disagrees* with it. A differential that passes against a deliberately wrong
   implementation is not testing anything, and this is the cheapest possible check that it is live.

## Why the catalogue rather than random masks

`reference/tests/test_differential.py` already runs random grant sets over the 250k Phase 0
fixture, and random masks are the wrong instrument for I7: they cluster around whatever coverage
the corpus's term distribution happens to produce, and they never straddle a Roaring container
boundary or §7.2's ~5% crossover on purpose. The catalogue is designed backwards from the
properties — `oracle/catalogue.py` explains the construction — so each case is named for what it
attacks and a failure names it too.

## What "disagree" is allowed to mean

Exact equality, and **as ordered lists** rather than sets or multisets. A set comparison would
absorb an engine bug that dropped one of two entities sharing rounded coordinates in a tile while
duplicating the other; a multiset comparison catches that but still discards the order — and the
order is contract, not presentation. Contracts §2.6 makes the points batch ascending by
`tessera_id` within each tile, and §7.2's nesting argument depends on the served set being a
*prefix*, so a client that truncates to its own budget forfeits nesting the moment the payload
arrives unsorted. Both sides are already in ascending order, so list equality is strictly stronger
at no cost — an engine draining a bounded heap without a final sort passes a multiset comparison
and fails this one.
"""

from __future__ import annotations

from collections import Counter

import pytest

from oracle import catalogue as cat
from oracle import morton
from oracle import viewport as vp
from oracle.bundle import Bundle
from oracle.wire import decode_viewport

VIEW = cat.VIEW_ID

# `(x0, y0, x1, y1)`, the request's bbox order — NOT `Bundle.extent`'s order for the same four
# numbers. Defined in `oracle/catalogue.py` beside the extent it is derived from; imported rather
# than restated, because it existed here and in `test_overlay_journal.py` with the explanation
# attached to only one of them.
FULL_VIEWPORT = cat.FULL_VIEWPORT

# Depth 0 is one tile over the whole map, where the cap clause binds for every case with more
# visible items than `k`; depth 6 is 4,096 tiles, where the floor clause binds for the sparse
# cases and `all_in_one_tile` reduces to a single occupied tile. The four together take every case
# from "everything in one tile" to "at most a handful per tile".
DEPTHS = (0, 2, 4, 6)

# `k` sets `cap = min(k, K_max)`, so it is the cap clause's only lever from the client side. 2 is
# the deployment `k_min`, where floor and cap coincide and `m` is pinned; 30 is the historical
# request default; 500 is the measured operating point (perf campaign, k=500 ruling).
K_VALUES = (2, 30, 500)


def _oracle_state(bundle: Bundle, case, depth: int):
    """The oracle's side for one (case, depth): the composed mask, θ's anchor, and the Selection.

    θ's anchor is computed here from the bundle and the case's own entity set, never read back from
    the service — see `oracle/viewport.py`'s module doc for why taking it from the thing under test
    makes every density assertion circular.
    """
    mask = set(case.entities)
    v_total = vp.visible_total(bundle, mask, VIEW)
    return mask, v_total, vp.Selection(bundle, mask, VIEW, depth)


def _server_view(server, token: str, depth: int, k: int):
    """One viewport request, decoded into `({tile: (visible, served)}, {tile: [code, ...]})`.

    The points batch is split per tile by the tile batch's own `served` column (contracts r7), not
    by `min(k, visible)`: under §7.2's density rule the per-tile count is `min(cap, max(k_min,
    C_θ))` clamped to visible, which cannot be recomputed from `k` and `visible` alone. That is the
    whole reason `served` is on the wire.

    Per-tile points keep the order they arrived in **and their exact positions**. The order is
    part of what is under test (see the module doc), so this must not sort, canonicalise or
    deduplicate anything. Positions are 64-bit codes, which removes a hazard the old `(x, y)` pairs
    carried: the nesting test recomputes a point's containing tile from its position, and a
    *rounded* coordinate could — rarely, but not never — fall the other side of a tile edge. A
    tile is now a prefix of the code, so it cannot.
    """
    raw = server.viewport(token, VIEW, depth, FULL_VIEWPORT, k=k)
    tiles, points = decode_viewport(raw)

    per_tile: dict[int, list[tuple[float, float]]] = {}
    counts: dict[int, tuple[int, int]] = {}
    cursor = 0
    for tile, visible, matched, served, _highlighted in tiles:
        assert visible == matched, "Phase 1 has no filters: matched must equal visible"
        per_tile[tile] = [code for _ident, code in points[cursor : cursor + served]]
        counts[tile] = (visible, served)
        cursor += served
    assert cursor == len(points), "the points batch must be exactly consumed by the tile batch"
    return counts, per_tile


def _ordered(points) -> list[int]:
    """The served sequence itself, **order preserved**.

    A pass-through since positions became integer codes — kept as a named function because the
    call sites read as a statement about what is and is not canonicalised here, and because its
    counterpart `_multiset` is the one that discards order.
    """
    return list(points)


def _multiset(points) -> Counter:
    """Order discarded. Used only where the question is genuinely about *membership* — the
    negative control, which asks whether the engine drew the same items as a wrong-order stub, and
    the nesting test, where the child's own order is not what is being asserted."""
    return Counter(_ordered(points))


@pytest.mark.parametrize("theta", ["saturated", "live"])
@pytest.mark.parametrize("case", cat.catalogue(), ids=lambda c: c.name)
def test_i7_selection_differential(
    request, catalogue_bundle: Bundle, case, theta: str
):
    """§7.2's served set, engine against definition, over depths × `k`.

    Parametrised on θ because the two configurations test disjoint halves of the definition. Under
    **saturation** `C_θ = |vis(T)|` for every tile, so `m` collapses to `min(cap, |vis(T)|)` and
    what is under test is the floor, the cap and — crucially — the *ordering*: which items, not how
    many. Under **live θ** the threshold clause does real work and the comparison additionally
    covers both anchors — the composed visible total and the occupied-tile count `N_occ(d)` — and
    saturation as a distinct state.

    Running only the saturated configuration was a real gap in the 250k suite's history: it cannot
    distinguish a correct engine from one that anchors θ on the pre-overlay projection (the I2
    breach §7.2 exists to prevent), counts occupied tiles over the wrong mask, or omits the
    threshold clause outright.
    """
    server = request.getfixturevalue(
        "catalogue_server" if theta == "saturated" else "catalogue_density_server"
    )
    token = server.authorise(list(case.grants))["token"]
    constants = server.meta(token)["selection"]

    tiles_compared = 0
    points_compared = 0
    truncating_tiles = 0

    for depth in DEPTHS:
        mask, v_total, selection = _oracle_state(catalogue_bundle, case, depth)
        expected_counts = selection.counts_for(
            morton.tiles_for_bbox(FULL_VIEWPORT, depth, catalogue_bundle.extent)
        )

        for k in K_VALUES:
            counts, per_tile = _server_view(server, token, depth, k)

            assert {t: v for t, (v, _s) in counts.items()} == expected_counts, (
                f"{case.name} ({case.attacks}) at depth {depth}: masked tile counts disagree. "
                f"This is §7.1, not §7.2 — the selection comparison below is meaningless until it "
                f"passes.\n  engine={ {t: v for t, (v, _s) in counts.items()} }\n"
                f"  oracle={expected_counts}"
            )

            params = vp.params_from_meta(constants, k=k, v_total=v_total)
            for tile, (visible, served_n) in counts.items():
                expected = _ordered(selection.served_points(tile, **params))
                assert _ordered(per_tile[tile]) == expected, (
                    f"{case.name} ({case.attacks}): served set disagrees for tile {tile} at "
                    f"depth {depth}, k={k}, θ {theta}. Engine served {served_n} of {visible} "
                    f"visible; the definition serves {len(expected)}. Compared as ordered lists: "
                    f"a disagreement here can be membership OR order, and contracts §2.6 makes "
                    f"the order (ascending tessera_id within a tile) part of the payload."
                )
                assert served_n == len(expected), (
                    f"{case.name}: the tile batch's `served` column says {served_n} but the "
                    f"definition says {len(expected)} for tile {tile} at depth {depth}, k={k}"
                )
                tiles_compared += 1
                points_compared += len(expected)
                if 0 < served_n < visible:
                    truncating_tiles += 1

    if case.name == "empty":
        # The zero-visibility principal, whose whole content is that there is none. V_total is 0,
        # which §7.2's closed form divides by, so this case is also the θ arithmetic's edge.
        assert tiles_compared == 0 and points_compared == 0
        return

    assert tiles_compared > 0, f"{case.name} produced no tiles to compare — the case is vacuous"
    assert points_compared > 0, f"{case.name} produced no points to compare"

    # Every non-empty case must truncate *somewhere*, or its point-set comparison degenerated to
    # "serve everything visible" and could not distinguish a correct selection from one that
    # ignores `tessera_id` entirely. `single_item` is the honest exception: one visible entity can
    # never exceed a cap of 2.
    if case.name != "single_item":
        assert truncating_tiles > 0, (
            f"{case.name}: no tile served strictly fewer points than it had visible across "
            f"depths {DEPTHS} and k {K_VALUES}, so this comparison never exercised the selection "
            f"at all — only the mask. Check the case's size against K_VALUES."
        )


# The catalogue minus `empty`, which has no marks in any parent tile and therefore nothing to
# nest. Excluded here rather than skipped inside the test: a skip is a line the report prints on
# every run, and a reader has to go and re-derive an answer that cannot change.
NESTING_CASES = [c for c in cat.catalogue() if c.entities]


@pytest.mark.parametrize("case", NESTING_CASES, ids=lambda c: c.name)
def test_i7_selection_nests_across_zoom(catalogue_bundle: Bundle, catalogue_density_server, case):
    """An item drawn in a parent tile is still drawn in whichever child contains it (§7.2).

    This is the property that makes zooming in *reveal* rather than reshuffle. §7.2 proves it from
    three facts — ranks fall under a subset, θ is monotone in depth, and each clause is a
    `tessera_id`-order prefix — and it holds **for a fixed `cap`**, which is why `k` is held
    constant across the two depths here. A client that reduced `k` while zooming in would forfeit
    nesting; that is a client obligation the engine cannot enforce, so the test does not ask it to.

    Checked against the θ-live server deliberately: under saturation the threshold clause is inert
    and monotonicity in depth is trivially satisfied by a clause that does nothing.

    **"Whichever child contains it" is computed, not searched for.** An earlier version flattened
    every child tile and asserted the point was drawn *somewhere*, which a bug that drew the item
    into the wrong child passes — and drawing into the wrong child is a tiling bug, exactly the
    class `Bundle.row_morton_codes` recomputes geometry to catch. The containing child is a
    *prefix* of the point's own position code: take the cell half, shift to the child depth. One
    line, and the assertion becomes the property the docstring claims.
    """
    server = catalogue_density_server
    token = server.authorise(list(case.grants))["token"]
    k = 30

    checked = 0
    for parent_depth in (2, 4):
        child_depth = parent_depth + 1
        shift = 32 - 2 * child_depth
        parent_counts, parent_points = _server_view(server, token, parent_depth, k)
        _child_counts, child_points = _server_view(server, token, child_depth, k)

        for tile in parent_counts:
            # Group the parent's marks by the child that contains them, then require that child to
            # carry at least as many copies of each. Points sharing a position share a child by
            # construction — the child is a prefix of the position — so counting per child is
            # exact.
            wanted: dict[int, Counter] = {}
            for code in parent_points[tile]:
                # The containing child is a *prefix* of the position, not a requantisation of it:
                # the cell is the code's high 32 bits, and the tile the top `2·depth` of those.
                child = (code >> 32) >> shift
                wanted.setdefault(child, Counter()).update([code])

            for child, needed in wanted.items():
                drawn = _multiset(child_points.get(child, []))
                for point, count in needed.items():
                    assert drawn[point] >= count, (
                        f"{case.name}: a point drawn in depth-{parent_depth} tile {tile} is not "
                        f"drawn in depth-{child_depth} tile {child}, the child that contains it, "
                        f"at the same k={k}. §7.2's nesting argument is violated — the mark pops "
                        f"out on zoom-in (or lands in the wrong child, which is a tiling bug and "
                        f"would pass a test that only looked for it somewhere)."
                    )
                    checked += 1

    assert checked > 0, f"{case.name}: nesting was not exercised at all"


K_MAX_UNDER_TEST = 128  # §7.2's own K_max; `catalogue_capped_server` is spawned with it


def test_k_max_binds_and_is_not_the_max_k_knob(catalogue_bundle: Bundle, catalogue_capped_server):
    """`cap = min(k, K_max)`, against a server where `K_max` is small enough to bind.

    Every other run in this suite has `k_max_marks` defaulted to 1,000,000, so `cap` reduces to `k`
    and the cap clause never does anything. Two engines pass that and fail this: one that ignores
    `k_max_marks` at selection, and one that reads `max_k` as the cap — the conflation §7.2's
    "deliberately not the same knob" paragraph exists to warn about, with `max_k` left at its
    1,000,000 default here so the two are distinguishable.

    `K_VALUES` straddles the cap deliberately: at `k = 2` and `k = 30` the *request* is the binding
    term and an engine that capped at `K_max` unconditionally serves too many; at `k = 500` the
    deployment's `K_max` binds and an engine that ignored it serves too many the other way.

    The published constant is asserted before it is used, because the oracle takes `K_max` from
    `/v1/meta` (it cannot know deployment config any other way) — so an engine that both ignored
    the cap *and* misreported it would otherwise be agreed with.
    """
    server = catalogue_capped_server
    case = next(c for c in cat.catalogue() if c.name == "full_100pct")
    token = server.authorise(list(case.grants))["token"]
    constants = server.meta(token)["selection"]

    assert constants["k_max_marks"] == K_MAX_UNDER_TEST, (
        "/v1/meta does not publish the k_max_marks this server was configured with, so the oracle "
        "would compute `cap` from the engine's own claim and agree with it whatever it did"
    )

    depth = 4
    mask, v_total, selection = _oracle_state(catalogue_bundle, case, depth)
    capped_tiles = 0
    for k in K_VALUES:
        counts, per_tile = _server_view(server, token, depth, k)
        params = vp.params_from_meta(constants, k=k, v_total=v_total)
        assert params["cap"] == min(k, K_MAX_UNDER_TEST)

        for tile, (visible, served_n) in counts.items():
            expected = _ordered(selection.served_points(tile, **params))
            assert _ordered(per_tile[tile]) == expected, (
                f"served set disagrees for tile {tile} at k={k} against K_max="
                f"{K_MAX_UNDER_TEST}: engine served {served_n} of {visible} visible, the "
                f"definition serves {len(expected)} (cap={params['cap']})"
            )
            assert served_n <= min(k, K_MAX_UNDER_TEST), (
                f"tile {tile} served {served_n} marks at k={k}, above cap = min(k, K_max) = "
                f"{min(k, K_MAX_UNDER_TEST)}"
            )
            if served_n == K_MAX_UNDER_TEST and visible > K_MAX_UNDER_TEST:
                capped_tiles += 1

    assert capped_tiles > 0, (
        f"no tile was truncated at K_max={K_MAX_UNDER_TEST} across k {K_VALUES}, so this run did "
        "not exercise the cap clause it exists for — check the case's size against the depth"
    )


def test_the_differential_disagrees_with_a_first_k_stub(catalogue_bundle: Bundle, catalogue_server):
    """The negative control: prove the differential can fail.

    `Selection.first_k_rows` serves §7.2's *count* in **storage order** instead of `tessera_id`
    order — the pre-2026-07-30 placeholder sampler. Every tile count, every `served` column and
    every response length is identical; only the membership differs. If the differential above
    could not tell the two apart it would be checking arithmetic and calling it I7.

    The stub is worth the eight lines it costs for a second reason: storage order is `(morton,
    tessera_id)`, and `tessera_id` is a keyed permutation of `(shard_id, entity_id)` where
    `entity_id` is assigned in **term-signature order** (§11.1, permanent under I9). So "first `m`
    in storage order" is, within a tile, ordered by permission signature — which is exactly the
    disclosure §7.2 r21 records having found and removed. The stub is not an arbitrary wrong
    answer; it is the wrong answer this design already made once.
    """
    server = catalogue_server
    case = next(c for c in cat.catalogue() if c.name == "full_100pct")
    token = server.authorise(list(case.grants))["token"]
    constants = server.meta(token)["selection"]

    depth, k = 4, 3  # a cap of 3 against ~600 visible items per occupied tile: everything truncates
    mask, v_total, selection = _oracle_state(catalogue_bundle, case, depth)
    params = vp.params_from_meta(constants, k=k, v_total=v_total)
    counts, per_tile = _server_view(server, token, depth, k)

    agreed_with_definition = 0
    disagreed_with_stub = 0
    for tile, (visible, served_n) in counts.items():
        engine = _multiset(per_tile[tile])
        assert engine == _multiset(selection.served_points(tile, **params)), (
            f"the definition itself disagrees with the engine at tile {tile} — fix that before "
            "reading anything into the negative control"
        )
        agreed_with_definition += 1

        stub = _multiset(selection.first_k_points(tile, **params))
        assert sum(stub.values()) == served_n, (
            "the stub must serve the same COUNT as the definition, or the comparison below is "
            "testing arithmetic rather than membership"
        )
        if engine != stub:
            disagreed_with_stub += 1

    assert agreed_with_definition > 100, (
        "too few tiles to draw a conclusion from; check the fixture and the depth"
    )
    # Not "at least one": a differential that caught the stub on a single tile out of a thousand
    # would be a coincidence, not a live check. Storage order and `tessera_id` order are unrelated
    # permutations, so on a truncating tile they agree only by chance.
    assert disagreed_with_stub > agreed_with_definition // 2, (
        f"the differential agreed with a deliberately wrong first-k stub on "
        f"{agreed_with_definition - disagreed_with_stub} of {agreed_with_definition} tiles. It is "
        "not distinguishing §7.2's ordering from storage order, so a green run above proves "
        "nothing about I7."
    )
