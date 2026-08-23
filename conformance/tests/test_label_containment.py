"""**I3: a label is served iff its generating set is a subset of `M_auth`** — both halves.

Conformance §4.4's I3 row, black-box, over `oracle.label_fixture`: the containment property across
principals and zoom tiers, and the cache half behaviourally rather than by hook.

**Why this row was uncovered and is not a "nothing to test" row.** Containment is built and
enforced on every route that serves an artifact — `|G ∩ M| == |G|`, all or nothing, against the
composed mask and never a filtered one — and the engine's own tests cover the property in
substance. What did not exist was a black-box assertion driving the real binary over HTTP, which is
this file. *We cannot test this* and *we have not tested this* are different claims and only the
first is an excuse.

**The whole difference between the two principals is one entity.** `oracle.label_fixture` plants
source id `SPLIT` carrying a term one principal holds and the other does not, and puts it inside
the widest generating set and nowhere else. So `CORE` is short of that set by exactly one member
and short of nothing else — asserted here from the masked counts, which differ by one, before any
containment claim is made. A fixture two principals a hundred entities apart would make every
absence explicable a dozen ways.

**Absence is checked against a control in the same response**, which is what stops this passing for
the wrong reason. `CORE` is served `l-core` — same layer, same membership, same request — and is
served `l-ranked`'s *narrower* content. So `l-whole`'s absence is containment and not a principal
who sees nothing, a layer gate, or an existence criterion: the layer is `public`, its artifact gate
is `inherited`, and its `require_member_visibility` is `none`, so containment is the only test in
the predicate that can fail here.

**And absence is absence, not a stripped artifact.** Decision 0076 forbids the in-between state —
identity and count served with the description missing — so the assertion is that no row of the
frame carries the key at all, never that its content list is empty.
"""

from __future__ import annotations

import base64
import shutil

import pytest

from oracle import label_fixture as lf
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_viewport_artifacts

#: The whole extent, so every artifact's members are inside every request's tiles and an absence is
#: never a viewport question.
WHOLE_MAP = [0.0, 0.0, lf.EXTENT_MAX, lf.EXTENT_MAX]

#: The zoom tiers the property is checked across. Depth 0 is one tile over the corpus; depth 6 is
#: 4,096, so the artifact pass resolves its membership against a viewport made of many ranges
#: rather than one.
TIERS = [0, 2, 4, 6]


@pytest.fixture(scope="session")
def label_bundle(tmp_path_factory):
    """The containment fixture's bundle, built once for the module."""
    return lf.build_label_bundle(tmp_path_factory.mktemp("label-fixture"))


@pytest.fixture(scope="session")
def label_server(tmp_path_factory, label_bundle):
    """A read-only server over it. Tests that mutate the overlay spawn their own against a private
    copy — a suppression is durable, and one leaking into another test's warm state is exactly the
    confusion this module is about."""
    server, proc = spawn_server(label_bundle, tmp_path_factory.mktemp("label-server"))
    yield server
    stop_server(proc)


def served(server, token: str, zoom: int = 0) -> dict[str, tuple[int, tuple[str, ...]]]:
    """The artifacts of one response, keyed by the publisher's own key.

    A dict is the right shape here precisely because the assertions are about **which keys are
    present**: a missing key is a `KeyError` at the point of use and an unexpected one shows up in
    the set comparison, where a list of rows would let either pass unnoticed.
    """
    body = server.viewport(token, lf.VIEW_ID, zoom, WHOLE_MAP, k=1000)
    return {
        artifact.key: (artifact.masked_count, tuple(artifact.content))
        for artifact in decode_viewport_artifacts(body)
        if artifact.layer == lf.LAYER
    }


WHOLE_SET_TEXT = "drawn from the whole set"
CORE_ALONE_TEXT = "drawn from the core alone"


def test_the_fixture_separates_the_two_principals_by_exactly_one_entity(label_server):
    """**The fixture's own claim, checked before anything rests on it.**

    Everything below reads "one member short" as the reason for an absence. That is only the reason
    if the two principals really are one member apart over the sets in question — so the masked
    count of the artifact both are served, taken over a membership neither generating set covers,
    must differ by exactly one, and the fixture's own arithmetic must agree with the engine's.
    """
    whole = served(label_server, label_server.authorise(lf.WHOLE_TERMS)["token"])
    core = served(label_server, label_server.authorise(lf.CORE_TERMS)["token"])

    assert whole["l-core"][0] - core["l-core"][0] == 1, (
        "the two principals must be one member apart over this artifact, or an absence below "
        "would be explicable by a difference nobody planted"
    )
    # And the difference is the entity the fixture says it is, computed from the planting rules
    # rather than from anything the service returned.
    visible_difference = lf.visible_to(lf.WHOLE_TERMS) - lf.visible_to(lf.CORE_TERMS)
    assert visible_difference == {lf.SPLIT}
    assert lf.SPLIT in lf.WIDE_SET and lf.SPLIT not in lf.NARROW_SET


@pytest.mark.parametrize("zoom", TIERS)
def test_a_generating_set_one_member_short_serves_no_artifact_at_all(label_server, zoom):
    """**The headline, at every tier.** A viewer holding all but one member of a content's
    generating set is served **nothing** for that artifact — not its identity, not its masked
    count, not the artifact with its description missing (decision 0076).

    The control is in the same response: the same principal, over the same layer and the same
    membership, is served `l-core`, whose generating set they do contain. So the absent one is
    absent for containment and not for anything else in the predicate.
    """
    whole = served(label_server, label_server.authorise(lf.WHOLE_TERMS)["token"], zoom)
    core = served(label_server, label_server.authorise(lf.CORE_TERMS)["token"], zoom)

    assert set(whole) == {"l-whole", "l-ranked", "l-core"}, (
        "the wider principal contains every generating set here, so nothing is withheld from it — "
        "without this the test could pass against a service serving no artifacts at all"
    )
    assert "l-whole" not in core, (
        "one member short of its only content's generating set, so the artifact is absent whole"
    )
    assert set(core) == {"l-ranked", "l-core"}


@pytest.mark.parametrize("zoom", TIERS)
def test_the_first_contained_content_is_the_one_served(label_server, zoom):
    """**Ranked contents are a fallback, not an all-or-nothing.** `l-ranked` carries two: a wide
    one and a narrow one. The wider principal reads the wide one; the narrower principal, one
    member short of it, reads the narrow one — the same artifact, the same identity, a different
    description.

    This is the other half of what makes the absence above meaningful: containment picks the first
    set the viewer contains **entirely**, and only where there is no such set does the artifact go.
    """
    whole = served(label_server, label_server.authorise(lf.WHOLE_TERMS)["token"], zoom)
    core = served(label_server, label_server.authorise(lf.CORE_TERMS)["token"], zoom)

    assert whole["l-ranked"][1] == (WHOLE_SET_TEXT,)
    assert core["l-ranked"][1] == (CORE_ALONE_TEXT,)
    assert whole["l-core"][1] == core["l-core"][1] == (CORE_ALONE_TEXT,)


@pytest.mark.parametrize("zoom", TIERS)
def test_a_principal_containing_no_generating_set_is_served_no_artifact(label_server, zoom):
    """A viewer holding a term none of the generating sets touches is served the layer's artifacts
    not at all — and reaches the layer, which is what makes this containment rather than the gate:
    `visibility` is `public`, so the layer resolves for them exactly as it does for the other two.
    """
    other = served(label_server, label_server.authorise(lf.OTHER_TERMS)["token"], zoom)
    assert other == {}


# ---------------------------------------------------------------------------------------------
# The cache half — behavioural, not by hook
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize(
    "warming",
    [
        pytest.param([0], id="one-tier"),
        pytest.param([0, 2, 4, 6, 0], id="ascending-then-back"),
        pytest.param([6, 4, 2, 0], id="descending"),
    ],
)
def test_no_cache_above_the_containment_check_outlives_an_overlay_change(
    tmp_path, tmp_path_factory, label_bundle, warming
):
    """**Warm every tier a request touches, remove one generating-set member, ask again on the same
    token — and the label must be gone.**

    Conformance §4.4 asks for this behaviourally rather than by hook, and the reason is that the
    hook version tests the caches somebody remembered to name. What a warming order actually
    exercises here is the mask fragments, the session's resolved visibility and the row-space
    artifact projections; the servable-label tier design §8.5 anticipated is not built, containment
    being evaluated per request. A green run says no cache above the check outlived the change,
    without enumerating internals.

    **The same token, deliberately.** A fresh session would resolve visibility after the change and
    prove nothing about what the old one was holding. The stamp is *not* re-presented: since
    [decision 0041](../../docs/decisions/0041-pins-become-a-staleness-stamp.md) a geometry stamp is
    advisory and never authorisation, so presenting one is an ordinary request with an ordinary
    answer and could not keep a suppression out either way. The row was written when the pin was
    the presentable proxy; the token is what carries session state now.

    **A suppression, not a deletion**, because the two retire by different rules (write-path §5.4)
    and only one of them is about this. What is being asserted is that the *serving* answer moves
    at the ack; a deletion would assert that and a fold's arithmetic at once.
    """
    private = tmp_path / "bundle"
    shutil.copytree(label_bundle, private)
    server, proc = spawn_server(private, tmp_path_factory.mktemp("label-overlay"))
    try:
        token = server.authorise(lf.WHOLE_TERMS)["token"]
        for zoom in warming:
            warm = served(server, token, zoom)
            assert warm["l-whole"][1] == (WHOLE_SET_TEXT,), "warming must serve the label"

        # The external id is the source id's eight little-endian bytes (contracts §2.4), which is
        # what `--mint-external-ids` wrote — so the entity this addresses is named by the fixture
        # rather than looked up through a bundle read.
        external_id = base64.b64encode(lf.SPLIT.to_bytes(8, "little")).decode()
        assert server.change(external_id, "suppress").status_code == 200

        after = served(server, token, warming[-1])
        assert "l-whole" not in after, (
            "one member of its generating set left M_auth at the ack, so the label is withheld — "
            "a cache above the containment check would still be serving it"
        )
        assert after["l-ranked"][1] == (CORE_ALONE_TEXT,), (
            "and the ranked artifact falls back to the content it still contains"
        )
        assert after["l-core"][0] == 60 - 1, "the count moves with the mask too"

        # **The strongest form of the claim.** Removing the entity from `M_auth` by suppression and
        # never granting it in the first place are two routes to one mask, and containment is a
        # function of the mask alone — so this principal's answer is now exactly the narrower
        # principal's, key for key and value for value.
        narrower = served(server, server.authorise(lf.CORE_TERMS)["token"], warming[-1])
        assert after == narrower
    finally:
        stop_server(proc)
