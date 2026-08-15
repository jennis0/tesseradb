"""I2 canaries: a term nobody holds must never affect what any principal sees — and the comparator
that checks it must be able to fail.

Three fixture states are built from one synthetic corpus (`reference/oracle/canary_fixture.py`),
differing in exactly one item:

- **canary-free** — the corpus alone;
- **canary** — plus one item at the extreme corner of the quantisation extent (`(65535.9, 65535.9)`,
  contracts §2.5: `v == max` lands in the top cell) carrying a term id **no** grant set here
  authorises;
- **visible** — the same extra item at the same corner in the same commit window, carrying a term
  the grant sets **do** authorise.

One comparator runs over all of them. Against the canary state it must report no difference; against
the visible state it must report one. That second run is conformance §4.4's positive control, and
§4.4 calls its absence the sharpest single gap in the suite: a comparator broken in any way that
made it always agree would have reported green for ever, and nothing here would have noticed. The
other two differential suites in this directory carry their controls — the I7 differential disagrees
with a first-*k* storage-order stub, the overlay-journal differential rejects two deliberately
defective engines — so this was a specific omission rather than a habit.

**The control runs the comparator, not a copy of it.** `compare_states` is one function called
twice. An earlier version of this module compared inline, so a control written beside it would have
exercised a second code path and proved that path instead — which is the failure mode the control
exists to rule out, reproduced one level up.

## What is compared: canonicalised bytes, and why that is cheaper here than §4.2 assumed

§4.2 specifies canonicalise-then-compare, and specifies the canonicalisation as a rewrite of each
item handle to its planted `fx_key`, because raw comparison "is impossible, since per-session handle
and pin bytes never match". **The handle column that made it impossible no longer exists.** Decision
0006 retired the per-session `handle: u32` from the viewer plane and contracts r6 replaced it with
`tessera_id`, a keyed permutation of `(shard_id, entity_id)` — so the points batch is
`(tessera_id, x, y, declared scalars…)`, in which every column is a deterministic function of the
bundle and none is a function of the session. The per-session bytes that remain are the token and
`x-tessera-pin`, both of which live in headers and never enter the body this compares.

The two states' `tessera_id`s agree because the fixture builds every state with **one identity key**
and the allocation rules keep every base item's entity id identical across builds — so the join
§4.2 wanted `fx_key` for is already an equality on a column the wire carries. `fx_key` remains
planted-but-unserved and its strict xfail in `test_mask_catalogue.py` remains the marker for that.

The canonicalisation itself is `suite.canonical`'s (correctness-suite §12.2), of which this module
was the origin and onto which it is refactored — one implementation, deliberately, for the same
reason `compare_states` is one function: a control that exercises a copy proves the copy. That
module carries the design argument (points as served, tiles re-sorted, the underlay its own
surface, the chunk-boundary assumption); what is asserted *here* is the premise that lets the
comparison be of raw bytes with nothing session-dependent stripped:
`test_the_response_body_carries_nothing_session_dependent` reissues one request under a second,
independently-authorised session and requires the canonical forms to be identical. If a
session-dependent field is ever added to the body, that test fails and this paragraph is wrong,
which is the point of writing it as a test.

One strengthening arrived with the refactor: this module used to exclude the trailer wholesale as
"the one deliberately nondeterministic region", where §12.2 drops only its elapsed-time fields —
so the deterministic remainder (`points`, `flushes`) is now a fourth compared surface rather than
an unwatched one.

The labels batch §4.2 also names is not compared, because there is no label service and no labels
batch. That is a gap in the system, not in this comparison, and it is recorded in §4.6 rather than
papered over with a test that reads as covering it.

**Byte comparison rests on response determinism at a fixed thread count** — a documented
implementation detail rather than a guarantee (design §10.4, decision 0030). The suite is entitled
to lean on it because it pins its own configuration; `canary_servers` does the pinning explicitly,
and a run at a different thread count is outside what has been argued.
"""

from __future__ import annotations

import pytest

from oracle.canary_fixture import build_canary_states, verify_allocation_rules
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport
from suite.canonical import Streamed, canonicalise_viewport

GRID_MAX = 65536.0
SLICE = "s0"
# Zooms 0-6 of the grid's full 0-16 depth range (contracts §2.5) — deep enough that the corner
# tile the extra item occupies is a small, specific prefix distinct from its neighbours (checked
# below at the deepest zoom), shallow enough to keep the test fast. Not exhaustive over all 17
# depths.
ZOOM_RANGE = range(0, 7)
# K = 500 exceeds N_BASE_ITEMS (400), and the fixture below spawns servers with `max_k`,
# `k_max_marks` and a saturating `theta_target_marks` all far above the fixture size — so no tile's
# point set is truncated in any state. That matters: conformance §4.2 requires the canonicalised
# comparison to include the points batches, and under design §7.2's density rule a truncating cap or
# a live theta would silently degrade this from a full-membership comparison to a prefix comparison
# with no test failing. The three overrides keep the surface at full strength.
#
# Deliberately still NOT exercised here: cap truncation and theta's threshold clause themselves.
# Those are tested in `crates/tessera-engine/tests/selection.rs`, against fixtures built for them.
K = 500
# §3.3 underlay depth offset. 4^2 = 16 sub-cells per tile is enough to produce a populated third
# stream at every zoom without tripping the server's max_underlay_cells budget — the same value the
# byte-scan uses, and for the same reason: what matters is that the bytes exist, not that there are
# many of them. Non-zero is the load-bearing part: at 0 no underlay is emitted and the surface
# silently drops out of the comparison.
UNDERLAY_OFFSET = 2

# Grant sets that never include the canary's own term id — every principal tested here is exactly
# the population the canary claims never to affect. The empty set is deliberate (R5): a
# zero-visibility token must still agree between states.
GRANT_SETS: list[list[str]] = [
    [],
    [str(t) for t in range(3)],
    [str(t) for t in range(6)],
]


@pytest.fixture(scope="module")
def canary_bundles(tmp_path_factory):
    work_dir = tmp_path_factory.mktemp("canary-fixture")
    return build_canary_states(work_dir)


@pytest.fixture(scope="module")
def canary_servers(tmp_path_factory, canary_bundles):
    """One server per state, all pinned to the same untruncated selection configuration.

    The pinning is deliberate and is the whole of the argument that a byte comparison is legitimate
    here (decision 0030): what this depends on is determinism at *one* configuration, which it
    controls, not stability across configurations, which the engine does not promise.
    """
    # `max_underlay_cells` is raised for the same reason as the other three: the deployment default
    # (8192) refuses a wide bbox at a deep zoom outright, so at zoom 6 over the full extent the
    # underlay surface would arrive as a 422 rather than as bytes to compare.
    untruncated = {
        "max_k": 100_000,
        "k_max_marks": 100_000,
        "theta_target_marks": 1 << 40,
        "max_underlay_cells": 1 << 20,
    }
    servers = []
    procs = []
    for bundle, name in zip(canary_bundles, ("free", "canary", "visible")):
        tmp = tmp_path_factory.mktemp(f"canary-serve-{name}")
        srv, proc = spawn_server(bundle, tmp, **untruncated)
        servers.append(srv)
        procs.append(proc)
    yield tuple(servers)
    for proc in procs:
        stop_server(proc)


def _canonical_response(server, token, zoom, bbox) -> Streamed:
    """One viewport response, canonicalised by the shared module, **surfaces kept separate**.

    `suite.canonical` (correctness-suite §12.2) does the work and carries the argument for its
    steps; this wrapper adds the one check that is this fixture's own premise rather than the
    canonical form's. The `served == visible` assertion is the untruncated premise, checked rather
    than assumed: if a cap or a live theta ever truncated a tile, the comparison would quietly
    weaken from "the two states serve the same points" to "the two states serve the same prefix"
    and still pass.

    The chunk boundaries inside the canonical points surface are not contract, but at one pinned
    configuration they are deterministic, which is all decision 0030's argument requires of these
    bytes (`canary_servers` does the pinning). The underlay matters here specifically: it is a
    per-cell `mask.count_range(...)` — an exact masked cardinality, the one derived aggregate in
    the system besides tile counts — and it was absent from this comparison until an independent
    review pointed out that an I2 defect confined to the underlay path moves no tile count and no
    point set, so every canary comparison would have passed while §4.6 called I2 covered.
    """
    raw = server.viewport(token, SLICE, zoom, bbox, k=K, underlay_offset=UNDERLAY_OFFSET)

    tiles, _points = decode_viewport(raw)
    for tile, visible, _matched, served in tiles:
        assert served == visible, (
            f"tile {tile} at zoom {zoom} served {served} of {visible} visible — this comparison "
            f"must be untruncated to mean what §4.2 asks of it; see K's comment and the "
            f"spawn_server overrides"
        )

    return canonicalise_viewport(raw)


# The three streamed surfaces of §12.2 plus the trailer's deterministic remainder — kept apart
# rather than concatenated, which is what lets the positive control assert *which* surface a
# difference landed on. When this module's comparator returned one concatenated blob, dropping the
# points batch from it left every canary test green, and so did dropping the tile batch: the
# control fired on whichever surface remained. Split, each is pinned — replacing any one with
# `b""` fails the control, measured in all three directions before the split was made.
SURFACES = ("tiles", "points", "underlay", "trailer")


def compare_states(server_a, server_b, bbox) -> list[tuple[str, str]]:
    """Every difference between two states, over the same logically identical query stream.

    Returns `(surface, description)` pairs; empty means the two states are indistinguishable on
    every surface this compares. Both the canary state (which must produce none) and the visible
    state (which must produce some) go through this function and no other.

    "Logically identical" rather than "byte identical" for the *requests*: the grant descriptors are
    the same strings in both states, but each state's session is authorised against its own server,
    so the tokens differ. That is the only asymmetry, and it lives in a header.
    """
    differences: list[tuple[str, str]] = []
    for terms in GRANT_SETS:
        auth_a = server_a.authorise(terms)
        auth_b = server_b.authorise(terms)
        for zoom in ZOOM_RANGE:
            a = _canonical_response(server_a, auth_a["token"], zoom, bbox).surfaces()
            b = _canonical_response(server_b, auth_b["token"], zoom, bbox).surfaces()
            for surface in SURFACES:
                if a[surface] != b[surface]:
                    differences.append(
                        (
                            surface,
                            f"terms={terms} zoom={zoom}: {surface} differ "
                            f"({len(a[surface])} vs {len(b[surface])} bytes)",
                        )
                    )
    return differences


def test_the_allocation_rules_hold_for_both_extra_item_states(canary_bundles):
    """The five allocation rules, checked before anything is read into the comparisons below.

    Order matters: without these rules the canary test measures **fixture perturbation, not
    disclosure** (conformance design §2), and it measures it while passing or failing for reasons
    that have nothing to do with I2. An extra item allocated in the middle of entity space changes
    every later item's `tessera_id`, which is §7.2's selection key — so the two states would
    legitimately draw different samples and the comparator would report a leak that is not there.

    Both the canary and the visible state are checked, because the positive control is only a
    control if its extra item displaces nothing either. See `verify_allocation_rules`.
    """
    free_bundle, canary_bundle, visible_bundle = canary_bundles
    for name, other in (("canary", canary_bundle), ("visible", visible_bundle)):
        failures = verify_allocation_rules(free_bundle, other)
        assert not failures, f"the {name} state's allocation rules are broken:\n  " + "\n  ".join(
            failures
        )


def test_an_ungranted_term_changes_no_canonicalised_response(canary_servers):
    """I2: an item nobody tested can see moves nothing, on any surface, by any amount."""
    free_srv, canary_srv, _visible_srv = canary_servers
    differences = compare_states(free_srv, canary_srv, (0.0, 0.0, GRID_MAX, GRID_MAX))
    assert not differences, (
        "the canary state's responses differ from the canary-free state's, so a term no tested "
        "principal holds influenced what one of them was served:\n  "
        + "\n  ".join(d for _surface, d in differences)
    )


def test_the_comparator_rejects_a_state_whose_extra_item_is_visible(canary_servers):
    """§4.4's positive control: the same comparator, over a state it must reject.

    This is the assertion that makes the test above mean something. It fails if the comparator is
    broken towards agreement in any way at all — a canonicalisation that discards the surface the
    extra item lands on, a decoder that yields nothing, a comparison of two values that are equal
    for reasons unrelated to the states.

    Note what is *not* asserted: that every grant set disagrees. The empty grant set cannot — a
    zero-visibility token sees no items in either state, and requiring it to differ would be
    requiring a leak. The claim is that a visible item is visible to the principals who hold its
    term, and the differences are reported per grant set so a run that disagreed for the *wrong*
    grant set is legible rather than merely green.
    """
    free_srv, _canary_srv, visible_srv = canary_servers
    differences = compare_states(free_srv, visible_srv, (0.0, 0.0, GRID_MAX, GRID_MAX))
    assert differences, (
        "the comparator found no difference between the canary-free state and a state carrying an "
        "extra item that tested principals CAN see — so it cannot fail, and the canary test above "
        "is proving nothing"
    )
    assert any("terms=['0', '1', '2']" in d for _s, d in differences), (
        "the comparator disagreed, but not for the grant sets that hold the visible item's term "
        f"— which is a difference arising somewhere other than visibility: {differences}"
    )

    # Every surface must carry the difference on its own. Without this, a canonicalisation that
    # silently stopped comparing one of them would keep both canary tests green: the control would
    # fire on whichever surface was left. That is not hypothetical — it was measured before
    # `_canonical_response` returned surfaces separately, in both directions. The trailer
    # participates on the same argument: its `points` count is the served cardinality, so a
    # visible extra item must move it, and a canonicalisation that dropped the remainder would
    # otherwise be indistinguishable from one that kept it.
    differing = {surface for surface, _d in differences}
    for surface in SURFACES:
        assert surface in differing, (
            f"the visible state's extra item moved every surface except `{surface}` — so nothing "
            f"here demonstrates that `{surface}` participates in the comparison at all, and a "
            f"canonicalisation that dropped it would report green for ever. §4.2 requires the "
            f"points batch explicitly; the underlay is the system's only other masked aggregate."
        )


def test_the_response_body_carries_nothing_session_dependent(canary_servers):
    """The premise that lets the served surfaces be compared as raw bytes: nothing in them is a
    function of the session.

    §4.2's canonicalisation exists because per-session handle bytes made raw comparison
    impossible. Decision 0006 retired that column; this asserts the consequence rather than
    assuming it. Two independently-authorised sessions on the **same** server with the **same**
    grants must canonicalise identically — the only thing the shared canonicalisation strips is
    elapsed time, which is issue-dependent, never session-dependent. If a session-dependent field
    is ever reintroduced into the body, this fails, and the module doc's claim that nothing else
    needs stripping is caught out at once.
    """
    free_srv, _canary_srv, _visible_srv = canary_servers
    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
    terms = [str(t) for t in range(3)]
    token_a = free_srv.authorise(terms)["token"]
    token_b = free_srv.authorise(terms)["token"]
    assert token_a != token_b, "two authorise calls must mint distinct sessions for this to test"
    for zoom in ZOOM_RANGE:
        assert _canonical_response(free_srv, token_a, zoom, bbox) == _canonical_response(
            free_srv, token_b, zoom, bbox
        ), (
            f"two sessions with identical visibility were served different bytes at zoom {zoom} — "
            f"something session-dependent has entered the response body, and the canary "
            f"comparison must strip it before comparing"
        )


def test_the_canary_occupies_a_tile_no_state_reports(canary_servers):
    """The corner tile carries no visible members in the canary-free or canary state.

    The comparison above already covers this — any visible content there would show up as a
    difference — but at the deepest zoom the corner tile is a specific, nameable prefix, and
    checking it by name is what distinguishes "the two states agree" from "the two states agree and
    the canary is genuinely where the fixture put it".
    """
    free_srv, canary_srv, _visible_srv = canary_servers
    bbox = (0.0, 0.0, GRID_MAX, GRID_MAX)
    zoom = max(ZOOM_RANGE)
    corner = (1 << (2 * zoom)) - 1
    for srv in (free_srv, canary_srv):
        token = srv.authorise([str(t) for t in range(6)])["token"]
        tiles, _points = decode_viewport(srv.viewport(token, SLICE, zoom, bbox, k=K))
        assert corner not in {t for t, _v, _m, _s in tiles}
