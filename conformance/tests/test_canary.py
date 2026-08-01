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

Canonicalisation is therefore:

- **the points batch: raw bytes, in served order.** Contracts §2.6 makes selection order contract,
  so comparing it as served is strictly stronger than sorting it first — a reordering is a defect,
  not noise, and sorting would hide it.
- **the tile batch: sorted by tile id, then re-serialised.** Emission order under a parallel gather
  is *not* contract (§4.2), and a byte comparison that flaked on it would get "fixed" by weakening.
- **nothing stripped from the body**, because there is nothing session-dependent left in it. This is
  asserted rather than assumed: `test_the_response_body_carries_nothing_session_dependent` reissues
  one request under a second, independently-authorised session and requires the bytes to be
  identical. If a session-dependent field is ever added to the body, that test fails and this
  paragraph is wrong, which is the point of writing it as a test.

The labels batch §4.2 also names is not compared, because there is no label service and no labels
batch. That is a gap in the system, not in this comparison, and it is recorded in §4.6 rather than
papered over with a test that reads as covering it.

**Byte comparison rests on response determinism at a fixed thread count** — a documented
implementation detail rather than a guarantee (design §10.4, decision 0030). The suite is entitled
to lean on it because it pins its own configuration; `canary_servers` does the pinning explicitly,
and a run at a different thread count is outside what has been argued.
"""

from __future__ import annotations

import io

import pyarrow as pa
import pyarrow.ipc as ipc
import pytest

from oracle.canary_fixture import build_canary_states, verify_allocation_rules
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport, split_frames

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
    untruncated = {"max_k": 100_000, "k_max_marks": 100_000, "theta_target_marks": 1 << 40}
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


def _canonical_response(server, token, zoom, bbox) -> bytes:
    """One viewport response, canonicalised: the tile batch sorted by tile id and re-serialised,
    then the points batch's own bytes, unaltered and in served order.

    The `served == visible` assertion is the untruncated premise, checked rather than assumed. If a
    cap or a live theta ever truncated a tile, the comparison would quietly weaken from "the two
    states serve the same points" to "the two states serve the same prefix" and still pass.
    """
    raw = server.viewport(token, SLICE, zoom, bbox, k=K)
    tile_bytes, points_bytes = split_frames(raw)

    tiles, _points = decode_viewport(raw)
    for tile, visible, _matched, served in tiles:
        assert served == visible, (
            f"tile {tile} at zoom {zoom} served {served} of {visible} visible — this comparison "
            f"must be untruncated to mean what §4.2 asks of it; see K's comment and the "
            f"spawn_server overrides"
        )

    with ipc.open_stream(io.BytesIO(tile_bytes)) as reader:
        table = pa.Table.from_batches(list(reader), reader.schema)
    sorted_tiles = table.sort_by([("tile", "ascending")])
    sink = io.BytesIO()
    with ipc.new_stream(sink, sorted_tiles.schema) as writer:
        for batch in sorted_tiles.to_batches():
            writer.write_batch(batch)
    return sink.getvalue() + points_bytes


def compare_states(server_a, server_b, bbox) -> list[str]:
    """Every difference between two states, over the same logically identical query stream.

    Returns a list of human-readable differences; empty means the two states are indistinguishable
    on every surface this compares. Both the canary state (which must produce none) and the visible
    state (which must produce some) go through this function and no other.

    "Logically identical" rather than "byte identical" for the *requests*: the grant descriptors are
    the same strings in both states, but each state's session is authorised against its own server,
    so the tokens differ. That is the only asymmetry, and it lives in a header.
    """
    differences: list[str] = []
    for terms in GRANT_SETS:
        auth_a = server_a.authorise(terms)
        auth_b = server_b.authorise(terms)
        for zoom in ZOOM_RANGE:
            body_a = _canonical_response(server_a, auth_a["token"], zoom, bbox)
            body_b = _canonical_response(server_b, auth_b["token"], zoom, bbox)
            if body_a != body_b:
                differences.append(
                    f"terms={terms} zoom={zoom}: canonicalised bodies differ "
                    f"({len(body_a)} vs {len(body_b)} bytes)"
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
        "principal holds influenced what one of them was served:\n  " + "\n  ".join(differences)
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
    assert any("terms=['0', '1', '2']" in d for d in differences), (
        "the comparator disagreed, but not for the grant sets that hold the visible item's term "
        f"— which is a difference arising somewhere other than visibility: {differences}"
    )


def test_the_response_body_carries_nothing_session_dependent(canary_servers):
    """The premise that lets the comparison be of raw bytes with nothing stripped.

    §4.2's canonicalisation exists because per-session handle bytes made raw comparison impossible.
    Decision 0006 retired that column; this asserts the consequence rather than assuming it. Two
    independently-authorised sessions on the **same** server with the **same** grants must produce
    byte-identical bodies — if a session-dependent field is ever reintroduced into the body, this
    fails, and the module doc's claim that nothing needs stripping is caught out at once.
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
