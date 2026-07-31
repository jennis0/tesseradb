"""I1 — the overlay-heavy catalogue state, driven through `AckedJournal`.

**I1: one effective mask, composed before use.** `M_auth = (token_mask \\ L) ∪ direct_eval(L)`,
composed at fetch time, before any consumer sees it. The oracle composes it in **entity space**,
item by item, from a journal of what the service acked; the engine composes it as **row-space
diffs** against a cached projection. Agreement between the two is the equivalence this module
proves, and it is the equivalence Phase 2's retirement rules will be judged against.

`overlay_heavy` is the catalogue member that cannot be built at build time: it is not a *shape* of
the mask but a *state* of the overlay, so it is a base case plus a history of accepted control
operations. `oracle/journal.py` is that history, and its one rule — **only a 200 is journalled** —
is what the second test here exercises directly.
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
FULL_VIEWPORT = (0.0, 0.0, 65536.0, 65536.0)
DEPTH = 4


@pytest.fixture(scope="module")
def overlay_server(tmp_path_factory, catalogue_bundle_root):
    """A server of its own, not the session-scoped catalogue ones.

    Overlay changes are **irreversible in Phase 1** — a deletion deny retires only via the epoch
    ledger, which does not exist until compaction lands (lifecycle §3.1) — so a module that deletes
    items must not share a server with modules that assume the corpus intact. `reference/tests`
    learned this the expensive way and left the comment to prove it.
    """
    tmp_dir = tmp_path_factory.mktemp("catalogue-overlay")
    srv, proc = spawn_server(catalogue_bundle_root, tmp_dir)
    yield srv
    stop_server(proc)


def _engine_counts(server, token: str) -> dict[int, int]:
    raw = server.viewport(token, SLICE, DEPTH, FULL_VIEWPORT, k=500)
    tiles, _points = decode_viewport(raw)
    return {tile: visible for tile, visible, _matched, _served in tiles}


def _oracle_counts(bundle: Bundle, mask: set[int]) -> dict[int, int]:
    selection = vp.Selection(bundle, mask, SLICE, DEPTH)
    return selection.counts_for(morton.tiles_for_bbox(FULL_VIEWPORT, DEPTH, bundle.extent))


def test_overlay_heavy_state_composes_the_same_mask_both_ways(
    catalogue_bundle: Bundle, overlay_server
):
    """Several hundred acked changes on top of a catalogue case, composed both ways.

    The four operations are chosen to cover each arm of I1's formula rather than to be numerous:
    `delete` and `suppress` remove an entity outright by different retirement rules; a
    **predicate-widen** onto an entity the grant set previously missed adds one from *outside*
    `token_mask`, which is the `∪ direct_eval(L)` arm and the only one that can make the composed
    mask larger; a **predicate-narrow** removes one via `L` rather than via a deny, which is the
    `\\ L` arm. An engine that composed by subtraction alone would pass every test that used only
    deletes and suppressions.
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
    narrows = ordered[300:400]
    # Entities carrying `cross_hi` and not `cross_lo` — outside the token's mask entirely, so a
    # predicate-widen onto them is the `∪ direct_eval(L)` arm and nothing else.
    widen_block = cat.BLOCKS["cross_hi"]
    widens = list(range(widen_block.start, widen_block.start + 100))

    for entity in deletes:
        assert journal.change(entity, "delete").status_code == 200
    for entity in suppressions:
        assert journal.change(entity, "suppress").status_code == 200
    for entity in narrows:
        assert journal.change(entity, "predicate", term_ids=set()).status_code == 200
    for entity in widens:
        assert journal.change(entity, "predicate", term_ids=session_terms).status_code == 200

    assert journal.acked_count == 500
    assert not journal.refused

    composed = journal.resolve(base_mask, session_terms)
    assert len(composed) == len(base_mask) - 400 + 100, (
        "the oracle's own composition is not what this test set up; check I1's arms before "
        "blaming the engine"
    )
    for entity in deletes + suppressions + narrows:
        assert entity not in composed
    for entity in widens:
        assert entity in composed

    assert _engine_counts(overlay_server, token) == _oracle_counts(catalogue_bundle, composed), (
        "the engine's row-space diff composition and the oracle's entity-space composition "
        f"disagree after:\n{journal.describe()}"
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
