"""Known-answer tests for `oracle.viewport` — §7.2 evaluated by hand.

**Neither the engine nor the rest of the oracle is consulted here.** The differential in
`test_differential.py` compares this module against the implementation it exists to check, which
cannot exclude a defect the two share; these cases assert §7.2's arithmetic against answers worked
out from the design text and written into the docstrings, so a mirrored error has a third opinion
to disagree with. That third opinion is the specification, quoted at each assertion.

The fixture is ten rows built in this file, so nothing here needs the Phase 0 corpus, a bundle or
a server. `oracle.bundle.Segment` is a plain dataclass and is reused rather than re-declared; the
bundle around it is a stub, because `Selection` asks a bundle for exactly three things and reading
them off a built artefact would import the artefact's assumptions into a test of the definition.

**What these cases cannot pin, stated so the coverage is not over-read.** `m(T)`'s inner
`min(k_min, cap)` is arithmetically redundant — `min(cap, max(min(k_min, cap), C))` equals
`min(cap, max(k_min, C))` for every input — so no example can distinguish the two, and none below
claims to. `Selection`'s tiling (`code >> 32 >> shift`) is pinned only at depth 1 and depth 0.
"""

from __future__ import annotations

import numpy as np
import pytest

from oracle import viewport
from oracle.bundle import Segment


TWO_64 = 1 << 64

# The fixture, laid out as a table so a reader can check every expected answer against it.
#
#  row  tile  entity  tessera_id            below 2**63?
#    0     0     100   0x9000_0000_0000_0000   no
#    1     0     101   0x1000_0000_0000_0000   yes
#    2     0     102   0x3000_0000_0000_0000   yes
#    3     0     103   0xF000_0000_0000_0000   no
#    4     0     104   0x2000_0000_0000_0000   yes
#    5     0     105   0x8000_0000_0000_0000   no  -- exactly 2**63; the comparison is strict
#    6     0     106   0x0000_0000_0000_0001   yes -- outside DEFAULT_MASK
#    7     1     107   0x4000_0000_0000_0000   yes
#    8     1     108   0x5000_0000_0000_0000   yes
#    9     2     109   0x6000_0000_0000_0000   yes -- outside DEFAULT_MASK, so tile 2 is empty
#
# Rows are deliberately not in `tessera_id` order: §7.2's order is imposed at selection time, and a
# fixture already in that order could not tell an explicit sort from an inherited one.
ROWS = [
    (0, 100, 0x9000_0000_0000_0000),
    (0, 101, 0x1000_0000_0000_0000),
    (0, 102, 0x3000_0000_0000_0000),
    (0, 103, 0xF000_0000_0000_0000),
    (0, 104, 0x2000_0000_0000_0000),
    (0, 105, 0x8000_0000_0000_0000),
    (0, 106, 0x0000_0000_0000_0001),
    (1, 107, 0x4000_0000_0000_0000),
    (1, 108, 0x5000_0000_0000_0000),
    (2, 109, 0x6000_0000_0000_0000),
]

DEFAULT_MASK = {100, 101, 102, 103, 104, 105, 107, 108}
"""Entities 106 and 109 are authorised for nobody here — the masking half of I7."""

VIEW = "v"


def _position_code(tile: int, row: int) -> int:
    """A row's 64-bit position: a depth-1 tile in the top two bits of the cell half.

    At depth 1 `Selection` shifts the 32-bit cell code right by `32 - 2*1 = 30`, so the tile is
    the cell's top two bits; the row index in the low bits keeps every position distinct, and the
    residual half is arbitrary because no tiling decision reads it.
    """
    cell = (tile << 30) | row
    return (cell << 32) | 0xABCD


class _StubBundle:
    """The three things `Selection` asks a bundle for, and nothing else.

    Not a `Bundle`: a real one reads a built artefact, and a test of §7.2's arithmetic that needed
    a build would be testing the build. `Segment` itself is the oracle's own dataclass, holding no
    behaviour that these tests assert.
    """

    def __init__(self, rows):
        self._entities = [entity for _, entity, _ in rows]
        self._codes = [_position_code(tile, row) for row, (tile, _, _) in enumerate(rows)]
        self._segment = Segment(
            entity_id=np.array(self._entities, dtype=np.uint64),
            residual=np.zeros(len(rows), dtype=np.uint32),
            morton=np.zeros(len(rows), dtype=np.uint32),
            row_count=len(rows),
            tessera_id=np.array([tid for _, _, tid in rows], dtype=np.uint64),
        )

    def segment(self, view_id: str) -> Segment:
        assert view_id == VIEW
        return self._segment

    def row_position_codes(self, view_id: str) -> list[int]:
        assert view_id == VIEW
        return list(self._codes)

    def row_entity_ids(self, view_id: str) -> list[int]:
        assert view_id == VIEW
        return list(self._entities)


@pytest.fixture
def selection() -> viewport.Selection:
    """The ten-row fixture at depth 1 under [`DEFAULT_MASK`]."""
    return viewport.Selection(_StubBundle(ROWS), DEFAULT_MASK, VIEW, 1)


# -- theta_cut: §7.2's product, arithmetic done here --------------------------------------------


def test_theta_cut_floors_the_quotient():
    """`P_d = ⌊m_target · N_occ · 2⁶⁴ / V_total⌋` — §7.2 ("`P_d` **floors**", r24/r62).

    Working: 2⁶⁴ = 18446744073709551616, and 18446744073709551616 / 3 = 6148914691236517205.33…,
    so the floor is 6148914691236517205 and a ceiling would be …206. The two differ, which is the
    whole reason r24 had to state the rounding.

    Kills: rounding up; rounding to nearest; `round()` in place of `//`.
    """
    assert viewport.theta_cut(3, 1, 1) == 6148914691236517205
    assert TWO_64 // 3 == 6148914691236517205  # the same arithmetic, spelled out


def test_theta_cut_at_one_occupied_tile_is_the_depth_zero_anchor():
    """Depth 0 is one tile whatever the corpus does, so `N_occ(0) = 1` (§7.2 r62).

    Working: V_total = 8, m_target = 1, so P_0 = 1·1·2⁶⁴/8 = 2⁶¹ = 2305843009213693952. This is
    where the occupied-tile anchor and the `4^d` progression it replaced agree exactly, and the
    agreement is not a coincidence: both say "one tile, `m_target` marks".

    Kills: an anchor that multiplies by `N_occ` where the definition divides, or that treats the
    single depth-0 tile as `4^0 = 1` by a different route and gets a different number.
    """
    assert viewport.theta_cut(8, 1, 1) == 1 << 61
    assert (1 << 61) == 2305843009213693952


def test_theta_cut_tracks_the_occupied_tile_count_not_four_per_depth():
    """`θ_d = m_target · N_occ(d) / V_total` — the count of occupied tiles, and nothing else.

    Working: V_total = 1024, m_target = 1. A **space-filling** corpus has `N_occ(d) = 4^d`, and
    there the cut reproduces the progression this anchor replaced exactly: 2⁵⁴, 2⁵⁶, 2⁵⁸. A
    **clustered** one at the same depths does not — half the occupied tiles is half the cut, so a
    level whose points fell into 2 tiles rather than 4 is thresholded at 2⁵⁵.

    Kills: a cut that still multiplies by four per depth; one that ignores `N_occ` and reads the
    depth; one that scales by `N_occ` in the denominator.
    """
    assert viewport.theta_cut(1024, 1, 1) == 1 << 54
    assert viewport.theta_cut(1024, 1, 4) == 1 << 56
    assert viewport.theta_cut(1024, 1, 16) == 1 << 58
    assert viewport.theta_cut(1024, 1, 2) == 1 << 55


def test_theta_cut_floors_once_over_the_whole_product():
    """The floor is taken over `m_target · N_occ · 2⁶⁴ / V_total`, not over `m_target · 2⁶⁴ /
    V_total` and then scaled (§7.2 r62).

    Working: at V_total = 1000003, m_target = 16, N_occ = 97 the two orders differ by 59 — small,
    and the differential demands exact equality, so it is a failure rather than a rounding
    difference.

    Kills: an implementation that keeps a depth-0 anchor and multiplies it up, which is the natural
    transcription of the progression this replaced.
    """
    assert viewport.theta_cut(1000003, 16, 97) == (16 * 97 * TWO_64) // 1000003
    assert viewport.theta_cut(1000003, 16, 97) != ((16 * TWO_64) // 1000003) * 97


def test_theta_cut_saturates_rather_than_clamping():
    """θ ≥ 1 is a *state*, not the largest representable cut (§7.2; `viewport.SATURATED`).

    Working: V_total = 4, m_target = 1 and one occupied tile gives 2⁶². Four occupied tiles would
    form 4·2⁶² = 2⁶⁴, which is outside the identity space, so that case is saturated. A clamp to
    2⁶⁴ − 1 would wrongly exclude the single identity 2⁶⁴ − 1, and a clamp to 2⁶⁴ would compare as
    a value where the definition has no value — hence `None` and not a number.

    Kills: returning `2**64 - 1`; returning `2**64`; a saturation test that admits one tile too
    many.
    """
    assert viewport.theta_cut(4, 1, 1) == 1 << 62
    assert viewport.theta_cut(4, 1, 4) is viewport.SATURATED
    assert viewport.theta_cut(4, 1, 9) is viewport.SATURATED


def test_theta_cut_saturates_when_the_target_reaches_the_total():
    """`P_d ≥ 2⁶⁴` is saturated (§7.2's "saturating").

    Working: m_target = 16 marks wanted in each of one occupied tile, out of V_total = 16 visible,
    is 16·2⁶⁴/16 = 2⁶⁴ exactly — the first anchor that admits everything. At V_total = 17 the same
    target does not saturate.

    Kills: a saturation test written `>` rather than `>=`.
    """
    assert viewport.theta_cut(16, 16, 1) is viewport.SATURATED
    assert viewport.theta_cut(17, 16, 1) == (16 * TWO_64) // 17


def test_theta_cut_is_saturated_when_nothing_is_visible():
    """`P_d` is **saturated** when `V_total = 0` (§7.2, r24 — normative, not implementation detail).

    A negative total cannot arise; §7.2 does not specify one, and the module folds it here rather
    than inventing a fourth behaviour, which this case records as the behaviour under test.

    Kills: a `ZeroDivisionError`; returning 0, which is the *strictest* possible cut and would
    serve the floor only.
    """
    assert viewport.theta_cut(0, 16, 1) is viewport.SATURATED
    assert viewport.theta_cut(0, 16, 0) is viewport.SATURATED
    assert viewport.theta_cut(-1, 16, 1) is viewport.SATURATED


def test_theta_cut_with_no_occupied_tile_is_a_zero_cut():
    """`N_occ = 0` is a view with nothing visible in it, where the product is zero.

    A zero cut admits no identity, which is the strictest threshold there is — and it thins
    nothing, because a view with no occupied tile emits no tile. The saturated answer belongs to
    `V_total = 0`, which is the state that cannot be divided by, and the two must not be confused.

    Kills: a saturation test that treats a zero product as "admits everything".
    """
    assert viewport.theta_cut(1000, 16, 0) == 0


def test_n_occ_is_the_number_of_occupied_tiles_at_this_depth():
    """`N_occ(d)` is θ's second anchor, taken over the buckets §7.1's counts come from (§7.2 r62).

    The ten-row fixture at depth 1 under the default mask: whichever tiles its visible rows fall
    in, the number of distinct ones is what θ scales by, and it is a property of `(mask, view,
    depth)` alone — never of a request's bbox, which is what keeps θ from moving on a pan.

    **The anchor is a sketch estimate, and at this size the estimate is the count.** A
    HyperLogLog's linear-counting branch is exact for a handful of distinct values against 2¹⁴
    registers, so the equality below is a real assertion about the definition rather than a
    tolerance dressed as one — and it would fail immediately if the sketch were fed rows rather
    than tiles, or a request's tiles rather than the view's. `oracle/occupancy.py` carries the
    estimate's accuracy at scale.

    Kills: counting rows rather than tiles; counting a request's tiles; counting tiles that hold
    only rows the mask excludes.
    """
    selection = viewport.Selection(_StubBundle(ROWS), DEFAULT_MASK, VIEW, 1)
    assert selection.n_occ == len(selection.rows_by_tile)
    assert selection.n_occ == len({t for t, rows in selection.rows_by_tile.items() if rows})
    assert all(selection.visible_count(t) > 0 for t in selection.rows_by_tile)
    # Deeper cannot be coarser: every occupied tile has an occupied child, and the ladder's
    # running maximum is what carries that through an estimate.
    deeper = viewport.Selection(_StubBundle(ROWS), DEFAULT_MASK, VIEW, 2)
    assert deeper.n_occ >= selection.n_occ


# -- §7.1: the tile's exact masked count ---------------------------------------------------------


def test_visible_counts_are_the_masked_ones(selection):
    """`|vis(T)|` counts mask members only (§7.1; I2 — an aggregate computed inside `M_auth`).

    Working: tile 0 holds rows 0–6, of which row 6 (entity 106) is outside the mask, so six.
    Tile 1 holds rows 7 and 8, both visible, so two. Tile 2 holds only entity 109, which is
    outside the mask, so zero.

    Kills: counting rows rather than visible rows; a mask test applied after bucketing.
    """
    assert selection.visible_count(0) == 6
    assert selection.visible_count(1) == 2
    assert selection.visible_count(2) == 0


def test_counts_for_omits_the_empty_tile(selection):
    """A zero-count tile is omitted, never emitted as a zero (`counts_for`; Appendix C).

    Kills: `{t: len(...)}` over every requested tile, which would publish tile 2's zero and tile
    3's — a masked count disclosed by the shape of the response rather than by its numbers.
    """
    assert selection.counts_for([0, 1, 2, 3]) == {0: 6, 1: 2}


def test_visible_total_is_the_row_space_count(selection):
    """`V_total = |M_auth ∩ rows(view)|`, counted in **row space** (§7.2, r24).

    Working: eight of the ten rows carry an entity in the mask, so the total is eight. Entity 900
    is authorised and has no row — the acked-but-unflushed state group-commit allocation makes
    routine — and adding it to the mask leaves the total at eight, because an entity with no row
    contributes to no count whatever `L` says (§11.2). The two unauthorised rows are excluded.

    Kills: `len(mask)`, which answers nine for the widened mask and would move θ, and with it the
    mark count in every tile of every viewer's map, on the acceptance of items nobody can draw.
    """
    assert viewport.visible_total(_StubBundle(ROWS), DEFAULT_MASK, VIEW) == 8
    assert viewport.visible_total(_StubBundle(ROWS), DEFAULT_MASK | {900}, VIEW) == 8


# -- §7.2: served(T) -----------------------------------------------------------------------------


LIVE = {"k_min": 2, "cap": 4, "v_total": 8, "m_target": 1}
"""A live threshold at depth 1: P_0 = 2⁶⁴/8 = 2⁶¹, so P_1 = 4·2⁶¹ = 2⁶³.

Every expected `C_θ` below is therefore "how many of the tile's visible identities have their top
bit clear", which is readable straight off the fixture table.
"""


def test_served_is_the_theta_count_when_it_sits_between_floor_and_cap(selection):
    """`m(T) = min(cap, max(min(k_min, cap), C_θ(T)))`, and `served` takes the *m* smallest (§7.2).

    Working: at P_1 = 2⁶³ the visible identities below the cut are 0x1000…, 0x3000… and 0x2000…
    — rows 1, 2 and 4 — so C_θ = 3. 0x8000… is exactly 2⁶³ and the comparison is strict, so row 5
    is above. m = min(4, max(2, 3)) = 3, and the three smallest by `tessera_id` are 0x1000… (row
    1), 0x2000… (row 4), 0x3000… (row 2), in that order.

    Kills: `<=` for the threshold comparison (row 5 would join, giving four); selecting in storage
    order (rows 0, 1, 2); returning a set or an unordered list.
    """
    assert selection.served_rows(0, **LIVE) == [1, 4, 2]


def test_the_cap_binds_above_the_theta_count(selection):
    """The cap clause: `m = min(cap, …)` (§7.2 — "a **cap**, which bounds work, wire and overplot").

    Working: the same C_θ = 3 against cap = 2 gives m = min(2, max(2, 3)) = 2, so the served set
    is the prefix [row 1, row 4] of the previous case — a prefix, which is what the nesting
    argument's client-truncation clause needs.

    Kills: dropping the outer `min(cap, …)`; applying the cap to `|vis(T)|` instead of to *m*.
    """
    assert selection.served_rows(0, **{**LIVE, "cap": 2}) == [1, 4]


def test_the_floor_serves_k_min_when_nothing_is_below_the_cut(selection):
    """The floor clause — the **I7** guarantee, which "may not be removed as an optimisation" (§7.2).

    Working: V_total = 2⁶³ with m_target = 1 gives P_0 = 2⁶⁴/2⁶³ = 2, and P_1 = 8. Every visible
    identity in tile 0 is at least 0x1000_0000_0000_0000, so C_θ = 0 — the sparsest-principal case
    the floor exists for. m = min(4, max(2, 0)) = 2, and the two smallest are rows 1 and 4.

    Kills: `m = min(cap, C_θ)`, which blanks the tile; a floor of 1; a floor applied only when
    `C_θ` is zero *and* the tile is non-empty by some other test.
    """
    assert selection.served_rows(0, **{**LIVE, "v_total": 1 << 63}) == [1, 4]


def test_masking_precedes_selection(selection):
    """Sampling happens **after** masking (**I7**) — the unauthorised row never displaces one.

    Working: row 6 carries `tessera_id` 1, the smallest identity in the fixture, and its entity is
    outside the mask. Under the mask it is absent from `vis(T)` entirely, so C_θ = 3 and the served
    set is unchanged. Widen the mask to admit it and the answer changes in both quantities —
    C_θ = 4, m = min(4, max(2, 4)) = 4 — and row 6 leads the served set. Same tile, same
    parameters: the difference is the mask alone.

    Kills: selecting from all rows and intersecting the result with the mask afterwards, which
    would serve two rows here rather than three and is the sample-then-filter defect I7 names.
    """
    assert selection.served_rows(0, **LIVE) == [1, 4, 2]

    widened = viewport.Selection(_StubBundle(ROWS), DEFAULT_MASK | {106}, VIEW, 1)
    assert widened.served_rows(0, **LIVE) == [6, 1, 4, 2]


def test_served_is_truncated_to_the_visible_set(selection):
    """`served(T)` is the `min(m(T), |vis(T)|)` smallest (§7.2) — the floor cannot invent a row.

    Working: tile 1 holds two visible rows, 7 and 8. With k_min = 5 and cap = 6 the floor gives
    m = min(6, max(5, 2)) = 5, which exceeds the two rows that exist.

    Kills: `rows[:m]` guarded by nothing would in fact be safe in Python, so what this pins is the
    *count*: an implementation padding to *m*, or raising, fails here.
    """
    assert selection.served_rows(1, **{**LIVE, "k_min": 5, "cap": 6}) == [7, 8]
    assert selection.served_rows(3, **LIVE) == []


def test_a_zero_total_serves_the_saturated_selection(selection):
    """`V_total = 0` saturates, and a saturated threshold makes `C_θ = |vis(T)|` (§7.2).

    Working: C_θ = 6, so m = min(4, max(2, 6)) = 4. Ascending by identity the tile is
    0x1000…(1), 0x2000…(4), 0x3000…(2), 0x8000…(5), 0x9000…(0), 0xF000…(3); the first four are
    rows 1, 4, 2, 5.

    Kills: treating saturation as `cut = 2**64` and then comparing (identical here, but see
    `test_theta_cut_saturates_rather_than_clamping`); treating it as `C_θ = 0`, which would fall
    back to the floor and serve two.
    """
    assert selection.served_rows(0, **{**LIVE, "v_total": 0}) == [1, 4, 2, 5]


def test_a_filtered_request_evaluates_with_the_threshold_saturated(selection):
    """§8.5's match-layer rule: a filtered request serves every match up to the cap.

    Working: the same tile and the same live anchor. Unfiltered, C_θ = 3 and three rows are
    served; `filtered=True` ignores the cut altogether, so C_θ = |vis| = 6 and m = min(4, 6) = 4.
    The anchor is untouched — it is simply not consulted.

    Kills: re-anchoring θ on the filtered set (filter-surface §5.2 forbids a threshold that moves
    as the viewer types); ignoring `filtered` and returning the live selection.
    """
    assert selection.served_rows(0, **LIVE) == [1, 4, 2]
    assert selection.served_rows(0, **LIVE, filtered=True) == [1, 4, 2, 5]


def test_served_points_and_identities_report_the_selected_rows(selection):
    """The two projections of `served(T)` carry the served rows' own position and identity.

    Working: the served rows are 1, 4 and 2 (above), whose fixture identities are 0x1000…,
    0x2000… and 0x3000…, and whose positions are the codes this file's `_position_code` built for
    tiles and row indices 1, 4 and 2.

    Kills: indexing the position or identity array by served *order* rather than by row id — an
    off-by-permutation that a set comparison could not see.
    """
    assert selection.served_identities(0, **LIVE) == [
        0x1000_0000_0000_0000,
        0x2000_0000_0000_0000,
        0x3000_0000_0000_0000,
    ]
    assert selection.served_points(0, **LIVE) == [
        _position_code(0, 1),
        _position_code(0, 4),
        _position_code(0, 2),
    ]


def test_the_negative_control_keeps_the_count_and_loses_the_membership(selection):
    """`first_k_rows` must serve §7.2's *count* in storage order — the I7 differential's control.

    Working: m = 3 by the real definition, and the first three rows tile 0 holds in storage order
    are 0, 1 and 2, where the definition serves 1, 4 and 2. Equal length, different membership,
    which is what forces the differential's point-set comparison to do the work: a stub that also
    got the count wrong would be caught by the tile batch's `served` column instead.

    Kills: a control that has drifted into computing the definition (the differential would then
    pass against a wrong engine); a control whose count no longer matches.
    """
    served = selection.served_rows(0, **LIVE)
    control = selection.first_k_rows(0, **LIVE)
    assert control == [0, 1, 2]
    assert len(control) == len(served)
    assert set(control) != set(served)


def test_a_deeper_tiling_partitions_the_shallower_one(selection):
    """Tiles come from the position's cell half shifted by `32 - 2d` (§7.2's depth-`d` tile).

    Working: at depth 0 the shift is 32, so every row falls in tile 0 and the count is the whole
    visible set — eight rows. At depth 1 the shift is 30 and the same eight split 6 / 2 / 0 across
    tiles 0, 1 and 2, which sums back to eight.

    Kills: a shift of `2*d` or `32 - d`; reading the residual half as the cell.
    """
    depth0 = viewport.Selection(_StubBundle(ROWS), DEFAULT_MASK, VIEW, 0)
    assert depth0.counts_for([0]) == {0: 8}
    assert sum(selection.counts_for([0, 1, 2, 3]).values()) == 8


def test_a_pre_r6_segment_is_refused_rather_than_selected_from():
    """A segment with no stored `tessera_id` cannot be ordered, so `Selection` refuses.

    §7.2's order is over `tessera_id`; without the column there is no order, and an oracle that
    fell back to storage order would silently become its own negative control.

    Kills: a fallback to `entity_id` order; a `None` reaching the sort as an opaque key.
    """
    bundle = _StubBundle(ROWS)
    bundle.segment(VIEW).tessera_id = None
    with pytest.raises(ValueError, match="tessera_id"):
        viewport.Selection(bundle, DEFAULT_MASK, VIEW, 1)


def test_params_from_meta_caps_k_at_the_deployment_maximum():
    """`cap = min(k, K_max)` (§7.2), with the other three read from `/v1/meta` (contracts §3.2).

    Working: k = 100 against `k_max_marks` = 500 caps at 100 — the design's own note that a
    request `k` below `K_max` narrows the proportionality window to `min(k, K_max)/k_min`. k =
    5000 against the same maximum caps at 500.

    Kills: `cap = k`; `cap = K_max`; `v_total` read back from the response rather than passed in.
    """
    meta = {"k_min": 12, "k_max_marks": 500, "theta_target_marks": 16}
    assert viewport.params_from_meta(meta, k=100, v_total=7) == {
        "k_min": 12,
        "cap": 100,
        "m_target": 16,
        "v_total": 7,
    }
    assert viewport.params_from_meta(meta, k=5000, v_total=7)["cap"] == 500
