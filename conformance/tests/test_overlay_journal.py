"""I1 and I7 under the overlay — the overlay-heavy catalogue state, driven through `AckedJournal`.

**I1: one effective mask, composed before use.** `M_auth = (token_mask \\ L) ∪ direct_eval(L)`,
composed at fetch time, before any consumer sees it. The oracle composes it in **entity space**,
item by item, from a journal of what the service acked; the engine composes it as **row-space
diffs** against a cached projection. Agreement between the two is the equivalence this module
proves, and it is the equivalence Phase 2's retirement rules will be judged against.

`overlay_heavy` is the catalogue member that cannot be built at build time: it is not a *shape* of
the mask but a *state* of the overlay, so it is a base case plus a history of accepted control
operations. `oracle/journal.py` is that history, and its one rule — **only a 200 is journalled** —
is what `test_a_refused_operation_...` exercises directly.

**Only the subtracting arm is reachable, and that is a property of the phase, not of this module.**
Decision 0047 withdrew the `predicate` op — an access edit is a delete plus a re-ingest — so the
composed mask can only shrink: `∪ direct_eval(L)` has no way in until a re-ingested item can reach
a viewport, which needs a flush Phase 1 does not have. What is asserted instead is the refusal
itself (`test_the_withdrawn_predicate_op_is_refused_...`), because an engine that started accepting
the op again would restore the novel-descriptor silent hide the withdrawal dissolved, and no other
test here would notice.

## Why the served *points* are compared here, and not only the counts

Counts are not the disclosure. Until this module gained
`test_served_points_follow_the_overlay_under_live_theta`, no test anywhere compared served points
after an overlay change, and the θ-live server was never combined with a journal — every
post-change assertion in this suite and in `reference/tests` was counts-only. Two engines passed
all of it, and CLAUDE.md names both:

1. one that composes `M_auth` for §7.1's counts but draws §7.2's sample from the **pre-overlay**
   mask. Every count is right; deleted and suppressed items keep being served as drawn marks with
   their coordinates on the wire. That is "keeps the Morton and Roaring machinery while quietly
   dropping I7" — a disclosure, not a wrong number, and invisible to any count comparison because
   the counts *are* right;
2. one that anchors θ on the **pre-overlay row projection**. §7.2 spells this leak out in its own
   text: the pre-overlay projection strictly contains `M_auth` after any accepted deny, so a
   viewer aggregates mark counts over a few hundred tiles, solves for the anchor, differences it
   against its own summed per-tile `visible` (which §7.1 discloses exactly), and recovers a
   running count of how many of its own items have been denied — which Appendix C admits nowhere.
   Only an engine-internal Rust test pinned this, and a defective reimplementation would not carry
   that test.

Both need the same three things at once, which is why neither was caught: a **live θ** (under
saturation the anchor is inert and both defects vanish), an **overlay** with accepted denies, and
a **point-level** comparison. None of the three needs an unshipped engine feature.
"""

from __future__ import annotations

import pytest

from oracle import catalogue as cat
from oracle import morton
from oracle import viewport as vp
from oracle.bundle import Bundle
from oracle.harness import spawn_server, stop_server
from oracle.journal import AckedJournal
from oracle.wire import decode_viewport

SLICE = cat.SLICE_ID
FULL_VIEWPORT = cat.FULL_VIEWPORT
DEPTH = 4

# The θ-live servers below are spawned with this. Against a case with a few thousand visible items
# it puts `P_0` low enough that the threshold clause — not the cap and not the floor — decides most
# tiles, which is what makes the anchor observable in the served *counts* per tile.
THETA_TARGET_MARKS = 16

# The point-level overlay test's case, depth and perturbation. Three constraints, and they pull
# against each other:
#
# * **θ must be live, not saturated.** θ_d = m_target·4^d / V_total, so the case and the depth must
#   satisfy `m_target·4^d < V_total` or the threshold clause admits everything and both defects
#   this module exists to catch become invisible. `cross_lo`'s 3,750 entities at depth 3 give
#   θ_3 ≈ 0.27 — about 16 marks in a tile of ~59 visible, which is the design's own operating
#   point. The same case at depth 4 saturates (16·256 > 3,750), which is why the depth is stated
#   here rather than inherited from `DEPTH` above.
# * **the anchor must move enough to be observable.** An anchor error is a proportional error in
#   every tile's mark count, so a 5% perturbation leaves most tiles serving identical sets and the
#   comparison is nearly blind to the pre-overlay anchor. Denying ~64% takes V_total from 3,750 to
#   1,600, a 2.3× move: ~16 marks per tile against a defective ~7.
# * **denies cost ~4 ms each** on this machine (one WAL fsync per item; the write-latency budget
#   accepts seconds for a deny). 2,400 of them is ~10 s, which is what this test is worth. Batched
#   at 400, comfortably inside the client's 10 s timeout with the measured 1.7 s per batch.
DENSITY_CASE = "crossover_below"
DENSITY_DEPTH = 3
N_DELETES = 800
N_SUPPRESSIONS = 1_600
BATCH = 400


@pytest.fixture(scope="module")
def overlay_server(tmp_path_factory, private_catalogue_bundle):
    """A server of its own, on a **private copy of the bundle**, not the session-scoped ones.

    Overlay changes are **irreversible in Phase 1** — a deletion deny retires only at the
    compaction fold, which does not run here — so a module that deletes items must not share a
    server with modules that assume the corpus intact. `reference/tests` learned this the expensive
    way and left the comment to prove it.

    A private *server* is not enough, which is what the copy is for: an accepted deny is published
    into the bundle prefix (contracts §2.3), so a server on the shared cached fixture denies items
    for every later reader of that fixture, this run and the next. See
    `conftest.private_catalogue_bundle`.
    """
    tmp_dir = tmp_path_factory.mktemp("catalogue-overlay")
    srv, proc = spawn_server(private_catalogue_bundle("overlay"), tmp_dir)
    yield srv
    stop_server(proc)


@pytest.fixture(scope="module")
def overlay_density_server(tmp_path_factory, private_catalogue_bundle):
    """θ **live** *and* an overlay — the combination nothing in this suite previously made.

    A third server rather than a reuse of either existing one, for two independent reasons. The
    session-scoped `catalogue_density_server` is shared with `test_i7_selection.py`, which asserts
    against the intact corpus, and Phase 1 denies do not retire. And `overlay_server` above is
    θ-saturated, which makes the anchor inert: both defects this module's point-level test exists
    to catch are invisible there whatever else is asserted.
    """
    tmp_dir = tmp_path_factory.mktemp("catalogue-overlay-density")
    srv, proc = spawn_server(
        private_catalogue_bundle("overlay-density"), tmp_dir,
        theta_target_marks=THETA_TARGET_MARKS,
    )
    yield srv
    stop_server(proc)


def _engine_counts(server, token: str) -> dict[int, int]:
    raw = server.viewport(token, SLICE, DEPTH, FULL_VIEWPORT, k=500)
    tiles, _points = decode_viewport(raw)
    return {tile: visible for tile, visible, _matched, _served in tiles}


def _oracle_counts(bundle: Bundle, mask: set[int]) -> dict[int, int]:
    selection = vp.Selection(bundle, mask, SLICE, DEPTH)
    return selection.counts_for(morton.tiles_for_bbox(FULL_VIEWPORT, DEPTH, bundle.extent))


def _engine_view(server, token: str, k: int, depth: int = DEPTH):
    """`({tile: (visible, served)}, {tile: [code, ...]}, {tile: [tessera_id, ...]})`.

    Points are split per tile by the tile batch's own `served` column and keep their arrival
    order — contracts §2.6 makes ascending `tessera_id` within a tile part of the payload, and
    §7.2's nesting argument depends on the served set being a prefix, so the order is contract.
    """
    raw = server.viewport(token, SLICE, depth, FULL_VIEWPORT, k=k)
    tiles, points = decode_viewport(raw)

    counts: dict[int, tuple[int, int]] = {}
    per_tile: dict[int, list[int]] = {}
    per_tile_ids: dict[int, list[int]] = {}
    cursor = 0
    for tile, visible, matched, served in tiles:
        assert visible == matched, "Phase 1 has no filters: matched must equal visible"
        window = points[cursor : cursor + served]
        counts[tile] = (visible, served)
        per_tile[tile] = [code for _ident, code in window]
        per_tile_ids[tile] = [ident for ident, _code in window]
        cursor += served
    assert cursor == len(points), "the points batch must be exactly consumed by the tile batch"
    return counts, per_tile, per_tile_ids


def _round(points) -> list[int]:
    """The served sequence itself.

    A pass-through since a position became a 64-bit integer code: the comparison is exact and
    there is nothing left to round. Kept as a named function because every call site below reads
    as a statement that the sequence is compared as it arrived — unsorted, undeduplicated — and
    because deleting it would touch a dozen assertions that are not what changed."""
    return list(points)


def _compare_served(
    bundle: Bundle,
    server,
    token: str,
    mask: set[int],
    constants,
    *,
    k: int,
    why: str,
    depth: int = DENSITY_DEPTH,
):
    """The point-level differential for one mask: counts, served sets, order, and θ's anchor.

    `v_total` is recomputed **from the composed mask** on every call rather than carried over —
    that is the anchor, and it moves the moment a deny is accepted.
    """
    v_total = vp.visible_total(bundle, mask, SLICE)
    selection = vp.Selection(bundle, mask, SLICE, depth)
    expected_counts = selection.counts_for(
        morton.tiles_for_bbox(FULL_VIEWPORT, depth, bundle.extent)
    )
    params = vp.params_from_meta(constants, k=k, v_total=v_total)

    counts, per_tile, per_tile_ids = _engine_view(server, token, k, depth)
    assert {t: v for t, (v, _s) in counts.items()} == expected_counts, (
        f"{why}: masked tile counts disagree. This is §7.1 — the selection comparison is "
        "meaningless until it passes."
    )

    truncating = 0
    for tile, (visible, served_n) in counts.items():
        expected = _round(selection.served_points(tile, **params))
        assert _round(per_tile[tile]) == expected, (
            f"{why}: the served SET disagrees for tile {tile} (engine {served_n} of {visible} "
            f"visible; the definition serves {len(expected)}, θ anchored at V_total={v_total}). "
            "Compared as an ordered list, so this is membership or order — and after an overlay "
            "change, membership means an item the engine had no right to draw."
        )
        assert per_tile_ids[tile] == sorted(per_tile_ids[tile]), (
            f"{why}: tile {tile}'s points are not ascending by tessera_id (contracts §2.6). A "
            "client truncating to its own budget would lose §7.2's nesting property."
        )
        assert served_n == len(expected), f"{why}: the `served` column disagrees for tile {tile}"
        if 0 < served_n < visible:
            truncating += 1

    assert truncating > 0, (
        f"{why}: no tile served strictly fewer points than it had visible, so θ never bound and "
        "this comparison degenerated to 'serve everything visible' — it cannot distinguish a "
        "correct anchor from a pre-overlay one. Check theta_target_marks and the case size."
    )
    return counts, per_tile, per_tile_ids, v_total, selection, params




def _apply(journal: AckedJournal, entities: list[int], op: str, term_ids=None) -> None:
    """Submit `entities` in batches, asserting each batch was acked.

    Batched rather than one call per item because the volume is what makes θ move — and because
    `AckedJournal.changes` is the all-or-nothing path, which is otherwise exercised only by the
    refusal test below.
    """
    for start in range(0, len(entities), BATCH):
        chunk = entities[start : start + BATCH]
        response = journal.changes([(e, op, term_ids) for e in chunk])
        assert response.status_code == 200, response.text


@pytest.fixture(scope="module")
def denied_overlay(catalogue_bundle: Bundle, overlay_density_server):
    """The θ-live server with several thousand acked denies on it, and everything to check it.

    A fixture rather than the tail of one test, so the two tests below are order-independent: Phase
    1 denies never retire, so a test that applied them itself would leave the *other* test reading
    whatever state the runner happened to produce — the ordering dependency this suite has already
    been bitten by once.

    The pre-change agreement is asserted here, as a precondition. Nothing below means anything if
    the engine and the definition already disagree on the intact corpus, and a failure at that
    point is a setup failure rather than a result.

    **Deletes and suppressions only.** The state used to carry a predicate-widen as well, for the
    `∪ direct_eval(L)` arm; the `predicate` op is withdrawn (decision 0047 — edit is delete +
    re-ingest) and `/control/changes` refuses it with a typed 422, so in Phase 1 no control
    operation can make a composed mask larger. The arm is not untested here by choice; it is not
    reachable. What replaces it as coverage is `test_the_withdrawn_predicate_op_is_refused_...`,
    which pins the refusal and that a refusal composes nothing.
    """
    case = next(c for c in cat.catalogue() if c.name == DENSITY_CASE)
    base_mask = set(case.entities)
    session_terms = {catalogue_bundle.term_id_of(g.encode("ascii")) for g in case.grants}
    server = overlay_density_server
    token = server.authorise(list(case.grants))["token"]
    constants = server.meta(token)["selection"]
    journal = AckedJournal(server, catalogue_bundle)
    k = 500

    _c, _p, _i, v_before, _s, _params = _compare_served(
        catalogue_bundle, server, token, base_mask, constants, k=k, why="before any change"
    )
    assert v_before == len(base_mask)

    ordered = sorted(base_mask)
    deletes = ordered[:N_DELETES]
    suppressions = ordered[N_DELETES : N_DELETES + N_SUPPRESSIONS]

    _apply(journal, deletes, "delete")
    _apply(journal, suppressions, "suppress")
    assert not journal.refused, journal.describe()
    assert journal.acked_count == len(deletes) + len(suppressions)

    composed = journal.resolve(base_mask, session_terms)
    assert len(composed) == len(base_mask) - N_DELETES - N_SUPPRESSIONS

    return {
        "server": server,
        "token": token,
        "constants": constants,
        "journal": journal,
        "base_mask": base_mask,
        "composed": composed,
        "denied": set(deletes) | set(suppressions),
        "v_before": v_before,
        "k": k,
    }


def test_served_points_follow_the_overlay_under_live_theta(
    catalogue_bundle: Bundle, denied_overlay
):
    """§7.2's served **points**, against a journal of accepted denies, with θ live.

    The three preconditions are all present here and were previously never present together (see
    the module doc): a live θ, an overlay carrying accepted deletes and suppressions, and a
    comparison of the served set rather than the count. What that buys, stated as
    the two engines it rejects:

    * an engine that samples from the **pre-overlay** mask serves items it has just been told to
      deny. Its counts are right, so nothing before this test could see it. The identity assertion
      at the end is that disclosure in its most direct form — no denied item's `tessera_id` appears
      anywhere in the payload — and it is exact rather than coordinate-approximate;
    * an engine that anchors θ on the **pre-overlay** projection serves the right items in the
      wrong quantity, everywhere, by the ratio of the two anchors. §7.2 records this as the I2 leak
      the composed anchor exists to prevent.

    `test_the_point_level_overlay_differential_rejects_both_defective_engines` builds both of them
    out of the oracle and shows this test's assertions reject them, so "would have caught" is a
    demonstration and not a claim.
    """
    journal = denied_overlay["journal"]
    composed = denied_overlay["composed"]
    v_before = denied_overlay["v_before"]

    _counts, _points, ids, v_after, _sel, _p2 = _compare_served(
        catalogue_bundle,
        denied_overlay["server"],
        denied_overlay["token"],
        composed,
        denied_overlay["constants"],
        k=denied_overlay["k"],
        why=f"after {journal.acked_count} acked changes",
    )

    # θ's anchor moved, and by exactly the composed difference. Asserted rather than assumed
    # because the whole second defect is an anchor that did not move, and a run where V_total
    # happened not to change would be blind to it.
    assert v_after == len(composed)
    assert v_after < v_before // 2, (
        "V_total barely moved, so an engine anchored on the pre-overlay projection would serve "
        "almost the right number of marks everywhere and this run could not tell it apart"
    )

    # The disclosure, stated directly: nothing denied is on the wire. By `tessera_id`, which is
    # exact — two entities can share rounded coordinates, so a coordinate answer to "was this item
    # served?" is approximate where an exact one is available. The identities are computed from the
    # fixture's own key (`identity.forward`), not read back from the bundle's stored column.
    denied = {catalogue_bundle.tessera_id_of(e) for e in denied_overlay["denied"]}
    served_ids = {ident for tile_ids in ids.values() for ident in tile_ids}
    leaked = served_ids & denied
    assert not leaked, (
        f"{len(leaked)} denied item(s) were served as drawn marks after their deny was acked — "
        f"e.g. tessera_id {sorted(leaked)[:3]}. The counts were correct throughout, which is "
        "exactly why no counts-only test could see this."
    )
    assert served_ids, "nothing was served at all; this test proved nothing"


def test_the_point_level_overlay_differential_rejects_both_defective_engines(
    catalogue_bundle: Bundle, denied_overlay
):
    """The negative control for the test above: build both defective engines, watch it reject them.

    Neither defect is hypothetical — CLAUDE.md names the first ("keeps the Morton and Roaring
    machinery while quietly dropping I7") and §7.2 names the second in its own text. Both are
    modelled out of the oracle, on the same overlay state the test above left behind, and for each
    one this asserts the pair that matters:

    * the **counts** agree with the correct engine — so every counts-only assertion in this suite
      and in `reference/tests` passes against it, which is the audit answer to why neither defect
      was catchable before;
    * the **served points** disagree, on a large fraction of tiles, so the comparison above fails.

    A differential that could not fail is not a differential, and "would have caught it" is a claim
    until something shows the failure.
    """
    server = denied_overlay["server"]
    token = denied_overlay["token"]
    constants = denied_overlay["constants"]
    base_mask = denied_overlay["base_mask"]
    composed = denied_overlay["composed"]
    denied = denied_overlay["denied"]
    k = denied_overlay["k"]

    v_composed = vp.visible_total(catalogue_bundle, composed, SLICE)
    v_pre = vp.visible_total(catalogue_bundle, base_mask, SLICE)
    correct = vp.Selection(catalogue_bundle, composed, SLICE, DENSITY_DEPTH)
    pre_overlay = vp.Selection(catalogue_bundle, base_mask, SLICE, DENSITY_DEPTH)
    params = vp.params_from_meta(constants, k=k, v_total=v_composed)
    params_pre_anchor = vp.params_from_meta(constants, k=k, v_total=v_pre)

    counts, per_tile, _ids = _engine_view(server, token, k, DENSITY_DEPTH)
    assert counts, "the engine returned no tiles; the state this test assumes is not there"

    tiles = 0
    sampled_from_pre_overlay_differs = 0
    count_matched_differs = 0
    pre_overlay_count_agrees = 0
    pre_anchor_differs = 0
    served_a_denied_item = 0
    for tile, (visible, served_n) in counts.items():
        engine = _round(per_tile[tile])
        assert engine == _round(correct.served_points(tile, **params)), (
            f"the definition itself disagrees with the engine at tile {tile} — fix that before "
            "reading anything into a negative control"
        )
        tiles += 1

        # Defect 1: counts composed, sample drawn from the pre-overlay mask.
        defect_1 = _round(pre_overlay.served_points(tile, **params))
        if defect_1 != engine:
            sampled_from_pre_overlay_differs += 1
        if len(defect_1) == served_n:
            pre_overlay_count_agrees += 1

        # …and its count-matched variant, which is the sneakier engine: `m` taken from the composed
        # set, the *items* still drawn from the pre-overlay ordering. Its `served` column is right
        # as well as its `visible`, so every number on the wire is correct and only the membership
        # is a disclosure. Modelled explicitly so that "the point comparison is what catches this"
        # cannot be answered with "the served count would have caught it anyway".
        if defect_1[: len(engine)] != engine:
            count_matched_differs += 1
        if denied & {
            _entity_of(catalogue_bundle, ident)
            for ident in pre_overlay.served_identities(tile, **params)
        }:
            served_a_denied_item += 1

        # Defect 2: right items, θ anchored on the pre-overlay projection.
        defect_2 = _round(correct.served_points(tile, **params_pre_anchor))
        if defect_2 != engine:
            pre_anchor_differs += 1

    assert tiles > 40, f"only {tiles} tiles compared; too few to draw a conclusion from"

    # What no counts-only test could see. §7.1's own numbers are identical for both defects — they
    # come from the composed mask either way — so the tile batch's `visible` column is the same.
    # The first defect additionally serves denied items *as marks*, which is the disclosure.
    assert served_a_denied_item > tiles // 2, (
        f"the pre-overlay sampler drew a denied item in only {served_a_denied_item} of {tiles} "
        "tiles, so this run does not demonstrate the disclosure it is here to demonstrate"
    )
    assert sampled_from_pre_overlay_differs > tiles // 2, (
        f"the point comparison agreed with a pre-overlay sampler on "
        f"{tiles - sampled_from_pre_overlay_differs} of {tiles} tiles"
    )
    assert count_matched_differs > tiles // 2, (
        f"the point comparison agreed with a count-matched pre-overlay sampler on "
        f"{tiles - count_matched_differs} of {tiles} tiles — that engine puts every correct "
        "number on the wire and only the membership is wrong, so nothing but a point comparison "
        "can see it at all"
    )
    assert pre_anchor_differs > tiles // 2, (
        f"the point comparison agreed with a pre-overlay θ anchor on {tiles - pre_anchor_differs} "
        f"of {tiles} tiles (anchors {v_pre} vs {v_composed}); it is not testing the anchor"
    )


def _entity_of(bundle: Bundle, tessera_id: int) -> int:
    """`tessera_id -> entity_id`, for the negative control only.

    The oracle holds the fixture's identity key, so this is a lookup rather than an inversion: the
    map is built once from the segment's own rows. It exists to name *which* items a defective
    engine would have served, and never appears in the differential itself — nothing on the viewer
    plane may translate an identity (I10), and a test that did so routinely would be modelling a
    capability the client does not have.
    """
    global _ENTITY_BY_IDENTITY
    if _ENTITY_BY_IDENTITY is None:
        seg = bundle.segment(SLICE)
        _ENTITY_BY_IDENTITY = {
            int(seg.tessera_id[row]): int(seg.entity_id[row]) for row in range(seg.row_count)
        }
    return _ENTITY_BY_IDENTITY.get(tessera_id, -1)


_ENTITY_BY_IDENTITY: dict[int, int] | None = None


def test_overlay_heavy_state_composes_the_same_mask_both_ways(
    catalogue_bundle: Bundle, overlay_server
):
    """Several hundred acked changes on top of a catalogue case, composed both ways.

    `delete` and `suppress` remove an entity outright by different retirement rules — Rule F at the
    compaction fold and Rule S on unsuppress — and they are the whole of what a Phase 1 overlay can
    hold. The two `predicate` arms this test used to carry, a **widen** onto an entity outside
    `token_mask` (I1's `∪ direct_eval(L)`, the only arm that can make a composed mask larger) and a
    **narrow** via `L`, are gone because the op is: decision 0047 withdrew it — an edit is a delete
    plus a re-ingest — and `/control/changes` answers it with a typed 422.

    So the caveat that used to justify those arms now stands unanswered by construction: **an
    engine that composed by subtraction alone would pass this test**, because subtraction is all
    Phase 1's control plane can ask for. The arm returns when a re-ingested item can reach a
    viewport, which needs the flush this phase does not have.
    """
    case = cat.overlay_heavy_base()
    base_mask = set(case.entities)
    session_terms = {catalogue_bundle.term_id_of(g.encode("ascii")) for g in case.grants}

    token = overlay_server.authorise(list(case.grants))["token"]
    journal = AckedJournal(overlay_server, catalogue_bundle)

    assert _engine_counts(overlay_server, token) == _oracle_counts(catalogue_bundle, base_mask), (
        "engine and oracle disagree before any change was applied — nothing below is meaningful"
    )

    ordered = sorted(base_mask)
    deletes = ordered[:100]
    suppressions = ordered[100:300]

    for entity in deletes:
        assert journal.change(entity, "delete").status_code == 200
    for entity in suppressions:
        assert journal.change(entity, "suppress").status_code == 200

    assert journal.acked_count == 300
    assert not journal.refused

    composed = journal.resolve(base_mask, session_terms)
    assert len(composed) == len(base_mask) - 300, (
        "the oracle's own composition is not what this test set up; check I1's arms before "
        "blaming the engine"
    )
    for entity in deletes + suppressions:
        assert entity not in composed

    assert _engine_counts(overlay_server, token) == _oracle_counts(catalogue_bundle, composed), (
        "the engine's row-space diff composition and the oracle's entity-space composition "
        f"disagree after:\n{journal.describe()}"
    )


@pytest.fixture(scope="module")
def ingest_server(tmp_path_factory, private_catalogue_bundle):
    """Its own server: an ingest allocates entity ids and moves the watermark, which every other
    module here asserts is where the bundle left it."""
    tmp_dir = tmp_path_factory.mktemp("catalogue-ingest")
    srv, proc = spawn_server(private_catalogue_bundle("ingest"), tmp_dir)
    yield srv
    stop_server(proc)


def _ingest_batch(rows: int, access: str) -> bytes:
    """A minimal `/control/ingest` body: `(external_id, x, y, access, fx_key)`, Arrow IPC stream.

    `fx_key` is present because the catalogue declares it, and a declared column must be in every
    batch (contracts §2.2): the scalar tail is read back positionally, so an omitted column shifts
    every later scalar rather than defaulting to absent. The values come from the fixture, which
    owns the join key.
    """
    import io  # noqa: PLC0415

    import pyarrow as pa  # noqa: PLC0415
    import pyarrow.ipc as ipc  # noqa: PLC0415

    schema = pa.schema(
        [
            pa.field("external_id", pa.binary()),
            pa.field("x", pa.float32()),
            pa.field("y", pa.float32()),
            pa.field("access", pa.utf8()),
            pa.field("fx_key", pa.uint64()),
            # The catalogue's filter columns, present for the same contracts §2.2 reason as
            # fx_key. A category value must be a declared key ("alpha" is), and the values are
            # inert here: nothing in this module filters, and attribute ingest writes no
            # artefact today (filter-index §5 ⊘).
            pa.field("department", pa.utf8()),
            pa.field("archive", pa.utf8()),
            pa.field("title", pa.utf8()),
            # The keyword column, supplied as its value: the ordinal is the flush's to assign
            # against its own extent's dictionary (records §4.3). Inert here for the same reason
            # as the rest of this tail.
            pa.field("submitter", pa.utf8()),
            # The render-only category (a key, like the others) and the blob-resident pair,
            # declared by the catalogue since 2026-08-12 and therefore required here too. Inert
            # for the same reason — and for the blob pair doubly so: the flush-side blob extent
            # is unbuilt (records §7 ⊘), so these values reach the WAL and no artefact yet.
            pa.field("shelf", pa.utf8()),
            pa.field("note", pa.utf8()),
            pa.field("pages", pa.uint32()),
        ]
    )
    batch = pa.record_batch(
        [
            pa.array([(950_000_000 + i).to_bytes(8, "little") for i in range(rows)], pa.binary()),
            pa.array([1000.0 + i for i in range(rows)], pa.float32()),
            pa.array([2000.0 + i for i in range(rows)], pa.float32()),
            pa.array([access] * rows, pa.utf8()),
            pa.array(cat.ingest_fx_keys(rows), pa.uint64()),
            pa.array(["alpha"] * rows, pa.utf8()),
            pa.array(["red"] * rows, pa.utf8()),
            pa.array([f"ingested-{i}" for i in range(rows)], pa.utf8()),
            pa.array([f"relay-overlay-{i}" for i in range(rows)], pa.utf8()),
            pa.array(["north"] * rows, pa.utf8()),
            pa.array([f"ingested-note-{i}" for i in range(rows)], pa.utf8()),
            pa.array([100 + i for i in range(rows)], pa.uint32()),
        ],
        schema=schema,
    )
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue()


def test_an_acked_ingest_moves_the_watermark_and_nobody_s_mask(
    catalogue_bundle: Bundle, ingest_server
):
    """`AckedJournal.ingest` and `barrier` against a real `/control/ingest`.

    The half of the journal that models **acked ≠ applied**, exercised end to end: a 200 is
    recorded as durable-and-not-yet-applied, `barrier` polls `/control/status` until the watermark
    reflects the rows the service said it accepted, and only then is the batch marked applied. The
    rule-level cases — a barrier that times out must not mark anything applied, a refused ingest
    must journal nothing — are in `reference/tests/test_journal.py` against a stub, where the
    states can be produced on demand.

    What is deliberately **not** asserted here is the interesting half: that an acked, unflushed
    batch is visible to nobody *and then becomes visible on flush*. Phase 1 has no commit window
    to hold one open and `/control/status` exposes no per-window field, so the barrier is a proxy
    over `entity_id_high_water` and says so at its definition. Stage 2.2's Task 8 is what makes
    that state constructible; this test is the scaffolding it lands on, not a substitute for it.

    The batch is labelled with a descriptor the session never granted, so the assertion that the
    viewer's counts do not move holds whether or not the rows are flushed — it is a statement
    about the mask, not a bet on the buffering.
    """
    case = cat.overlay_heavy_base()
    base_mask = set(case.entities)
    token = ingest_server.authorise(list(case.grants))["token"]
    journal = AckedJournal(ingest_server, catalogue_bundle)

    before_counts = _engine_counts(ingest_server, token)
    assert before_counts == _oracle_counts(catalogue_bundle, base_mask)
    before_high_water = int(ingest_server.status()["entity_id_high_water"])

    response = journal.ingest(_ingest_batch(3, "999001"), "conformance-ingest-1")
    assert response.status_code == 200, response.text

    assert len(journal.ingests) == 1
    op = journal.ingests[0]
    assert op.accepted == 3
    assert op.applied is False, "an ingest is acked on WAL fsync, which is not a visibility promise"
    assert op.required_high_water == before_high_water + 3

    journal.barrier(timeout=20.0)
    assert journal.ingests[0].applied is True
    assert int(ingest_server.status()["entity_id_high_water"]) >= op.required_high_water

    assert journal.resolve(base_mask, set()) == base_mask, (
        "an acked ingest contributed to the composed mask; its entities postdate the bundle and "
        "nothing has said they are visible"
    )
    assert _engine_counts(ingest_server, token) == before_counts, (
        "a batch the session has no grant for moved the session's counts"
    )


def test_a_refused_operation_is_not_journalled_and_changes_nothing(
    catalogue_bundle: Bundle, overlay_server
):
    """The whole point of `AckedJournal`: submitted is not acked.

    A `/control/changes` item naming an external ID the deployment has never seen is refused
    (`ApiError::Unknown`). Three things must then be true, and only the first is obvious: the
    refusal is recorded so a test can assert on it; it contributes **nothing** to the composed
    mask; and the engine's own view has not moved either. A journal that recorded intent would
    make the oracle expect a change the service never promised, and the red test that followed
    would be blamed on the engine.
    """
    case = cat.overlay_heavy_base()
    base_mask = set(case.entities)
    session_terms = {catalogue_bundle.term_id_of(g.encode("ascii")) for g in case.grants}
    token = overlay_server.authorise(list(case.grants))["token"]

    journal = AckedJournal(overlay_server, catalogue_bundle)
    before_counts = _engine_counts(overlay_server, token)
    before_mask = journal.resolve(base_mask, session_terms)

    # `external_id_b64` names an id the deployment has never seen — base64("not-an-id"). It goes
    # through the journal's own submission path, so what is under test is the journal's rule and
    # not a hand-built record of it.
    response = journal.change(-1, "suppress", external_id_b64="bm90LWFuLWlk")
    assert response.status_code != 200, "this call was supposed to be refused"

    assert journal.ops == [], "a refused operation must not enter the journal"
    assert len(journal.refused) == 1
    assert journal.resolve(base_mask, session_terms) == before_mask
    assert _engine_counts(overlay_server, token) == before_counts, (
        "a refused control operation moved the engine's view — the refusal was not a refusal"
    )


def test_the_withdrawn_predicate_op_is_refused_and_composes_nothing(
    catalogue_bundle: Bundle, overlay_server
):
    """`op: "predicate"` is refused with a typed 422, and refusing it changes nothing.

    Decision 0047 withdrew the op: an access edit is a delete plus a re-ingest under the same
    `external_id`. This is the conformance-side pin on that, and it is a **fail-closed** check
    rather than a tidiness one. The withdrawal is what dissolves the novel-descriptor silent hide
    (the invariants review's F4): a predicate change naming a descriptor the dictionary never held
    minted an unsatisfiable extension id that nothing ever promoted, leaving the item invisible to
    everyone behind a 200. An engine that quietly began accepting the op again would restore that
    hole while every other test here stayed green, because a widen the oracle also applies is
    indistinguishable from a widen that worked.

    Both directions are submitted — a narrow (empty term set) and a widen onto an entity outside
    the token's mask — because they travel the same handler but only the widen could enlarge a
    mask, and a refusal that covered one and not the other is the interesting half-fix.
    """
    case = cat.overlay_heavy_base()
    base_mask = set(case.entities)
    session_terms = {catalogue_bundle.term_id_of(g.encode("ascii")) for g in case.grants}
    token = overlay_server.authorise(list(case.grants))["token"]

    journal = AckedJournal(overlay_server, catalogue_bundle)
    before_counts = _engine_counts(overlay_server, token)
    before_mask = journal.resolve(base_mask, session_terms)

    narrow = sorted(base_mask)[-1]
    widen = cat.BLOCKS["cross_hi"].start
    assert widen not in base_mask, "the widen arm must start outside the token's mask"

    for entity, term_ids in ((narrow, set()), (widen, session_terms)):
        response = journal.change(entity, "predicate", term_ids=term_ids)
        assert response.status_code == 422, (
            f"the withdrawn predicate op was answered {response.status_code}, not 422: "
            f"{response.text}"
        )
        assert "predicate" in response.text and "delete" in response.text, (
            "the 422 must name the op and the flow that replaces it, or a caller cannot act on "
            f"it: {response.text}"
        )

    assert journal.ops == [], "a refused operation must not enter the journal"
    assert len(journal.refused) == 2
    assert journal.resolve(base_mask, session_terms) == before_mask
    assert _engine_counts(overlay_server, token) == before_counts, (
        "the refused predicate changes moved the engine's view — they were not refused"
    )
