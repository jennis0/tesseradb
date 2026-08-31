"""The multi-view differential — the oracle answers per view, and the mask does not.

`views.md` §11 names what the suite owes this design: "a two-view differential: the oracle answers
per view; the pinned-leaf cases, the gate-failed pin among them; the gate's work-indistinguishability".
This module is all three clauses: the first two over the four ungated views, and the gate's over a
second group whose `visibility` is a real access label.

The design's rule is a factoring (§1):

> Entity space is the invariant plane; a view owns everything downstream of the permutation and
> nothing upstream of it.

Every case here is one half of that sentence made falsifiable, over
`reference/oracle/multiview.py`'s corpus — one entity space of 6,144 items in six compartments,
a plain view holding all of them, a group of three views each holding a different subset with its
own frame and its own layout, and a second group of two behind a gate.

## What each case asserts

- **Counts per view** — I2's multi-view form. The masked tile counts a view serves equal the
  oracle's, computed through that view's own permutation, its own Morton order and its own frame,
  from the same entity-space mask. Two principals, four views, two viewports each.
- **Authorisation is view-invariant** — the epic's first clause. The oracle's mask is derived once
  from `terms/pairs.parquet`, in entity space, with no view parameter available to it even in
  principle (`oracle.mask.mask_of` takes term ids and a path). The assertion is the exact
  equality `served(view) == mask ∩ members(view)` in every view, with `members(view)` read off
  the view's permutation. One token answers every view, and no request re-authorises.
- **Identity is stable across views** — I9/I10's multi-view form, and §9's cross-view linkage
  note. An entity present in several views is served the **same** `tessera_id` in each and a
  **different** position; the second half is what stops the first passing on a corpus that reused
  one layout.
- **The cross-view leak check** — the epic's own sentence, and what it does and does not prove is
  set out at the case.
- **Pinned leaves** (§5) — a scoped filter under its own view, pinned from another view by key,
  and pinned across views of one group; per-view presence; and the two refusals the section
  names.
- **Ordering per view** — contracts §2.6's ascending `tessera_id` within each tile, in every view.
- **The gate** (§6) — a group, `sealed`, whose `visibility` is a real access label. Three separate
  claims: a principal who fails it finds the group on **no** surface — absent from `/v1/meta`'s
  roster whole, a 404 indistinguishable from a name nobody declared, and the unknown-column
  refusal for either spelling of a leaf over its scoped attribute; a principal who passes it is
  served exactly what those views would serve with no gate at all, compared against the ungated
  expectation rather than against itself; and the union-equals-mask equality holds per principal
  over the views that principal can actually reach.

## The oracle's independence, and where it is not independent

The comparison is engine-versus-oracle on every quantity a view owns. The oracle computes each
view's positions from **that view's own points file** — the build's input, not `morton.u32` — and
its tile membership by applying `tile_of` to every row, so a build that wrote a wrong Morton
column and then served consistently by its own wrong values fails here rather than agreeing.

What is shared is `viewport.py`'s one shared artefact, the stored `tessera_id` column, and this
module closes it the way `test_mask_catalogue.py` does: `verify_identity_cross_check` and
`derive_row_order` are run against **every** view before anything rests on the column.

## θ is saturated, deliberately

`multiview_server` runs with θ's target far above any mask here, so §7.2's selection reduces to
"serve every visible row up to the cap" and `served(T) == vis(T)`. That is what makes
`served(view) == mask ∩ members(view)` an equality rather than a subset relation — the epic's
clause is about *which entities a view may serve*, and a live threshold would thin the served set
for a reason that has nothing to do with views, leaving every assertion here a subset check that a
view-leaking engine would also pass. θ itself is covered per view by the count and point-set
comparisons in `test_i7_selection.py` and `reference/tests/test_differential.py`, over a corpus
built for it.

## What this module does not cover, stated rather than implied

- **A second gate semantics.** The gate here is one group's, over two views, with a label that is
  a single compartment term. A view's *own* label narrowing its group's — the conjunction §6
  specifies — is asserted at server level (`crates/tessera-server/tests/views_gate.rs`) and is not
  re-derived against an oracle here: the oracle's contribution is the served set behind a passed
  gate, which one label answers as well as two.
- **Ingest into a second view** (§4's join rule). The fixture is build-only, and a server that
  accepted writes would mutate the bundle under every other module in this suite;
  `crates/tessera-server/tests/views_write.rs` drives that path against a real flush.
- **A group's view created or dropped while the service runs** (§3.2, §3.4), for the same reason.
- **A scoped `text` family** (§5's remaining marker): the build writes no serving artefact a
  request can reach for it here, so there is nothing for an oracle to disagree with. The scoped
  **category** was in this list until its artefact existed, and so was the **rendered** family
  until `render` alone became an operand licence (§5 r26); both have their own cases above.
"""

from __future__ import annotations

import pytest

from oracle import morton
from oracle import multiview as mv
from oracle import viewport as vp
from oracle.bundle import Bundle
from oracle.mask import mask_of
from oracle.wire import decode_viewport, decode_viewport_points

#: The zoom every count comparison is made at, and one deeper for the partial viewport. Depth 2 is
#: sixteen tiles over a whole frame, which is enough for a per-tile disagreement to have somewhere
#: to hide and few enough for the literal oracle to answer in milliseconds.
ZOOM = 2
DEEP_ZOOM = 4

#: Well above every mask here, so a comparison is never truncated by a cap it did not choose.
K = 20_000

#: Two principals whose masks differ, plus a third that shares no compartment with either. Grants
#: are compartment terms, and a compartment is a contiguous entity range (`multiview`'s module
#: doc), so `wide` is not a superset of `disjoint` and neither contains the other's blocks — a
#: cross-principal assertion that held only for nested masks would say much less.
PRINCIPALS: dict[str, list[str]] = {
    "narrow": ["cc1"],
    "wide": ["cc1", "cc3", "cc4"],
    "disjoint": ["cc0", "cc5"],
}

#: Who passes the gated group's gate: exactly the principals granted its label (`views.md` §6 —
#: satisfaction is the intersection of the label's terms with the principal's). `wide` holds `cc4`
#: and the other two do not, which is what makes every case below have both sides.
def _passes_gate(principal: str) -> bool:
    return mv.GATE_LABEL in PRINCIPALS[principal]


def _visible_views(principal: str) -> tuple[str, ...]:
    """The views this principal may reach — every view, or every ungated one.

    The gate is the only thing that removes a view here, so this is the whole of the "visible-view
    set" as the corpus can express it, and it is derived from the grant rather than read back off
    `/v1/meta` — reading it back would compare the server against itself.
    """
    return mv.ALL_VIEW_IDS if _passes_gate(principal) else mv.VIEW_IDS

# ---------------------------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------------------------


@pytest.fixture(scope="module")
def masks(multiview_bundle: Bundle) -> dict[str, set[int]]:
    """Each principal's `M_auth`, in **entity space**, derived once.

    From `terms/pairs.parquet` — the flat `(entity_id, term_id)` relation — rather than from the
    postings the engine answers from, which is what makes the comparison a differential.

    **This is the case's own subject as much as its input.** `mask_of` takes a set of term ids and
    a path; there is no view parameter to pass it and no view-dependent branch inside it, so the
    mask a view is compared against is the same object for every view by construction rather than
    by an assertion. That is the shape §1's factoring demands of an implementation, and the oracle
    having it structurally is why a server that lost it fails rather than being agreed with.
    """
    out = {}
    for name, terms in PRINCIPALS.items():
        term_ids = set()
        for term in terms:
            term_id = multiview_bundle.term_id_of(term.encode())
            assert term_id is not None, f"the dictionary holds no term '{term}'"
            term_ids.add(term_id)
        out[name] = mask_of(term_ids, multiview_bundle.pairs_path())
    return out


@pytest.fixture(scope="module")
def tokens(multiview_server) -> dict[str, str]:
    """One token per principal, authorised once and used against **every** view.

    Naming a view is a property of a request, not of a session: a token that had to be reissued
    per view would make "a token's authorisation is unchanged by which view it names" untestable,
    because there would be no one token to hold fixed.
    """
    return {name: multiview_server.authorise(terms)["token"] for name, terms in PRINCIPALS.items()}


@pytest.fixture(scope="module")
def members(multiview_bundle: Bundle) -> dict[str, set[int]]:
    """`{view: the entities that view has a row for}` — read off each view's permutation.

    Membership of a view *is* the permutation's non-sentinel (§1); there is no stored member set,
    and a fixture that used its own generation function here would be comparing the corpus against
    itself rather than against what the build laid down.
    """
    return {view: set(multiview_bundle.row_entity_ids(view)) for view in mv.ALL_VIEW_IDS}


@pytest.fixture(scope="module")
def entity_of_fx(multiview_bundle: Bundle) -> dict[int, int]:
    return mv.entity_of_fx_key(multiview_bundle)


def _full_bbox(view_id: str) -> tuple[float, float, float, float]:
    """The whole of a view's frame as a request bbox, `(x0, y0, x1, y1)`.

    Per view, because the frame is (decision 0040) — the group's `[-40, 40]²` read against the
    plain view's `[0, 65536)²` is a degenerate box in the corner, which is precisely the mistake a
    single bundle-wide extent invites.
    """
    x_min, x_max, y_min, y_max = mv.extent_of(view_id)
    return (x_min, y_min, x_max, y_max)


def _inner_bbox(view_id: str) -> tuple[float, float, float, float]:
    """A quarter of a view's frame — a viewport that excludes tiles rather than spanning them."""
    x_min, x_max, y_min, y_max = mv.extent_of(view_id)
    return (
        x_min + (x_max - x_min) * 0.10,
        y_min + (y_max - y_min) * 0.10,
        x_min + (x_max - x_min) * 0.55,
        y_min + (y_max - y_min) * 0.60,
    )


def _served_entities(raw: bytes, entity_of_fx: dict[int, int]) -> set[int]:
    """The response's served set as **entity ids**, joined through the planted `fx_key`.

    The suite's one legitimate handle→item route: no reverse map, no extra endpoint, no entity id
    on the viewer plane (`catalogue.py`'s `fx_key` section). A response that served nothing carries
    no points frame at all, and the empty set is a legitimate answer here — a principal whose
    compartments miss a view's population entirely reaches it.
    """
    tiles, _points = decode_viewport(raw)
    if not any(served for _t, _v, _m, served in tiles):
        return set()
    table = decode_viewport_points(raw)
    return {entity_of_fx[key] for key in table.column("fx_key").to_pylist()}


def _served_identities(raw: bytes, entity_of_fx: dict[int, int]) -> dict[int, int]:
    """`{entity: tessera_id}` for every point the response served.

    Both columns off the same batch, in served order, so the pairing is the response's own rather
    than a re-join through anything the oracle holds.
    """
    tiles, _points = decode_viewport(raw)
    if not any(served for _t, _v, _m, served in tiles):
        return {}
    table = decode_viewport_points(raw)
    keys = table.column("fx_key").to_pylist()
    idents = table.column("tessera_id").to_pylist()
    return {entity_of_fx[key]: int(ident) for key, ident in zip(keys, idents)}


# ---------------------------------------------------------------------------------------------
# The fixture's own preconditions
# ---------------------------------------------------------------------------------------------


def test_the_corpus_is_the_shape_every_case_below_assumes(multiview_bundle: Bundle):
    """Four views over one entity space, the compartments as planted, and the decorrelation.

    `mv.verify` re-derives all of it from the built bundle. The decorrelation clause is the one
    worth naming here: if a compartment sat inside one quarter, then `served(view)` and
    `mask ∩ members(view)` would coincide for a reason that has nothing to do with the engine, and
    every equality below would pass vacuously.
    """
    report = mv.verify(multiview_bundle)
    assert report.views[mv.WORLD_VIEW] == mv.N_ITEMS
    assert set(report.views) == set(mv.ALL_VIEW_IDS)
    assert len(report.compartment_entities) == mv.COMPARTMENTS
    # Every (compartment, view) pair has members on both sides — `verify` refuses otherwise, so
    # this asserts the report was actually populated rather than re-checking the rule. The gated
    # group is in it: a gate that correlated with a view's membership would make the "a passed
    # gate serves what an ungated view would" equality pass for the wrong reason.
    assert len(report.decorrelation) == mv.COMPARTMENTS * (len(mv.ALL_VIEW_IDS) - 1)


def test_the_stored_identity_column_is_the_permutation_it_claims_in_every_view(
    multiview_bundle: Bundle,
):
    """`tessera_id` is `forward(key, shard, entity_of_row)` and the rows are stored in that order.

    `viewport.py`'s module doc names the stored identity column as the one artefact the selection
    differential shares with the engine, and names these two checks as what closes it. Run **per
    view**, because a bundle now has several row spaces and the ordering claim is a claim about
    each of them separately: a build that ordered `world` correctly and a group's view by anything
    else would pass a single-view check and serve one view's points in the wrong order.
    """
    for view_id in mv.VIEW_IDS:
        multiview_bundle.verify_identity_cross_check(view_id)
        multiview_bundle.derive_row_order(view_id)


# ---------------------------------------------------------------------------------------------
# Counts per view — I2's multi-view form
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("principal", sorted(PRINCIPALS))
@pytest.mark.parametrize("view_id", mv.VIEW_IDS)
def test_masked_counts_agree_per_view(
    multiview_bundle: Bundle, multiview_server, masks, tokens, principal, view_id
):
    """Every tile's masked count equals the oracle's, in each view, for each principal.

    The oracle answers through **that view's** permutation, Morton order and frame, from the same
    entity-space mask — which is the whole content of §1's factoring on the count path. A server
    that answered a group's view out of the plain view's row space, or decoded a tile prefix
    against the wrong frame, disagrees here on the first tile.

    Two viewports: the whole frame, so nothing is excluded by the bbox and the comparison is over
    the view's entire population; and an inner box at a deeper zoom, so tile *selection* is live
    and a server that returned every tile regardless of the bbox is caught by the same comparison.
    """
    token = tokens[principal]
    mask = masks[principal]
    checked = 0
    for zoom, bbox in ((ZOOM, _full_bbox(view_id)), (DEEP_ZOOM, _inner_bbox(view_id))):
        raw = multiview_server.viewport(token, view_id, zoom, bbox, k=K)
        server_tiles, _points = decode_viewport(raw)

        oracle = vp.Selection(multiview_bundle, mask, view_id, zoom)
        expected = oracle.counts_for(
            morton.tiles_for_bbox(bbox, zoom, multiview_bundle.extent_of(view_id))
        )

        served_counts = {t: v for t, v, _m, _s in server_tiles}
        assert served_counts == expected, (
            f"tile counts disagree for {principal} in view '{view_id}' at zoom {zoom}: "
            f"server={served_counts} oracle={expected}"
        )
        for _t, visible, matched, _served in server_tiles:
            assert visible == matched, "an unfiltered request has matched == visible"
        checked += len(expected)
    assert checked > 0, f"no tile carried a visible count for {principal} in '{view_id}'"


def test_a_views_counts_are_not_another_views(multiview_server, tokens):
    """The negative control for the case above: two views must **disagree**.

    A differential that compared every view against an oracle which happened to answer them all
    alike would pass against an engine that served one row space for every view. The corpus is
    built so that cannot be so — different populations, different frames, independently drawn
    layouts — and this is where that is checked rather than assumed.
    """
    token = tokens["wide"]
    per_view = {}
    for view_id in mv.VIEW_IDS:
        raw = multiview_server.viewport(token, view_id, ZOOM, _full_bbox(view_id), k=K)
        tiles, _points = decode_viewport(raw)
        per_view[view_id] = {t: v for t, v, _m, _s in tiles}
    distinct = {tuple(sorted(counts.items())) for counts in per_view.values()}
    assert len(distinct) == len(per_view), (
        "two views served identical per-tile counts, so this corpus cannot tell a per-view answer "
        f"from a shared one: {per_view}"
    )


# ---------------------------------------------------------------------------------------------
# Authorisation is view-invariant — the epic's first clause
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("principal", sorted(PRINCIPALS))
def test_the_served_set_is_the_mask_met_with_the_views_membership(
    multiview_server, masks, members, tokens, entity_of_fx, principal
):
    """`served(view) == mask ∩ members(view)`, exactly, in every view — one token throughout.

    This is the epic's clause in its strongest available form. The mask is entity-space and has no
    view in it; membership is the view's permutation; and what the view serves is the meet of the
    two and nothing else. Equality rather than containment is what makes it bite in both
    directions: a subset check passes an engine that serves too little, which is the shape a view
    filter applied in the wrong space takes.

    Holding the token fixed is the other half of the clause — the same authorisation answers every
    view, and nothing about the response's *authorisation* content is a function of which view was
    named. What differs is membership, which the equality names explicitly.
    """
    mask = masks[principal]
    for view_id in mv.VIEW_IDS:
        raw = multiview_server.viewport(
            tokens[principal], view_id, ZOOM, _full_bbox(view_id), k=K
        )
        served = _served_entities(raw, entity_of_fx)
        expected = mask & members[view_id]
        assert served == expected, (
            f"{principal} in view '{view_id}': served {len(served)} entities, "
            f"mask ∩ members is {len(expected)} "
            f"(served-only {len(served - expected)}, missing {len(expected - served)})"
        )
        assert served <= mask, "a view served an entity outside the mask"


def test_the_mask_the_oracle_derives_carries_no_view(masks, members):
    """The oracle's mask construction is view-independent **by construction**, and this says so.

    `oracle.mask.mask_of` takes term ids and a path to `terms/pairs.parquet`; a view is not among
    its arguments and `pairs.parquet` is entity-space, so there is no view-dependent branch for
    one to hide in. What is left to assert is the consequence: one mask meets four different
    memberships and produces four different served sets, none of which is the mask itself except
    in the view that holds every entity.

    Stated as a case rather than left in a docstring because it is the premise every equality above
    rests on. If `mask_of` ever grew a view parameter, this is the test that would have to be
    edited to accommodate it — which is the point at which somebody would have to argue for it.
    """
    for principal, mask in masks.items():
        seen = {view: mask & members[view] for view in mv.VIEW_IDS}
        assert seen[mv.WORLD_VIEW] == mask, (
            f"'{mv.WORLD_VIEW}' holds every entity, so the mask meets it whole"
        )
        for key in mv.QUARTER_KEYS:
            view = f"{mv.GROUP}:{key}"
            assert seen[view] < mask, (
                f"{principal}: view '{view}' holds a strict subset of the corpus, so the meet must "
                "be a strict subset of the mask — a corpus where it is not cannot test this"
            )


# ---------------------------------------------------------------------------------------------
# Identity is stable across views — I9/I10's multi-view form
# ---------------------------------------------------------------------------------------------


def test_one_entity_serves_one_identity_and_several_positions(
    multiview_bundle: Bundle, multiview_server, tokens, entity_of_fx
):
    """The same entity carries the same `tessera_id` in every view, and a different position.

    The view is not an input to the keyed bijection (§9's cross-view linkage note, C17), so a
    viewer joins a visible item to itself across views — that is the design, and this is what
    holds an implementation to it.

    **Both halves matter, and the second is the control.** Identity agreement alone is satisfied by
    an engine that serves one row space under four names; requiring the positions to differ is what
    says the four answers came from four permutations. The corpus draws each view's layout
    independently, so an entity sharing a position code across two views would be a coincidence at
    the level of a 64-bit code rather than a legitimate outcome.
    """
    token = tokens["wide"]
    per_view = {}
    for view_id in mv.VIEW_IDS:
        raw = multiview_server.viewport(token, view_id, ZOOM, _full_bbox(view_id), k=K)
        per_view[view_id] = _served_identities(raw, entity_of_fx)

    shared = set(per_view[mv.VIEW_IDS[0]])
    for view_id in mv.VIEW_IDS[1:]:
        shared &= set(per_view[view_id])
    assert len(shared) > 100, (
        f"only {len(shared)} entities are served in every view, which is too few to be checking "
        "cross-view identity on"
    )

    for entity in shared:
        idents = {view: per_view[view][entity] for view in mv.VIEW_IDS}
        assert len(set(idents.values())) == 1, (
            f"entity {entity} was served different identities per view: {idents}"
        )

    # The control: positions must move. Compared through the bundle's own per-view position codes
    # rather than the wire, because the wire's codes are what the count case already compared and
    # the question here is whether the *views* differ at all.
    codes = {}
    for view_id in mv.VIEW_IDS:
        entities = multiview_bundle.row_entity_ids(view_id)
        positions = multiview_bundle.row_position_codes(view_id)
        codes[view_id] = dict(zip(entities, positions))
    moved = sum(
        1
        for entity in shared
        if len({codes[view][entity] for view in mv.VIEW_IDS}) == len(mv.VIEW_IDS)
    )
    assert moved == len(shared), (
        f"{len(shared) - moved} of {len(shared)} entities share a position code between two "
        "views, so this corpus does not distinguish four permutations from one"
    )


# ---------------------------------------------------------------------------------------------
# The cross-view leak check — the epic's own sentence
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("principal", sorted(PRINCIPALS))
def test_every_view_together_yields_exactly_what_the_mask_licenses(
    multiview_server, masks, tokens, entity_of_fx, principal
):
    """A viewer cannot learn from one view what another would not give them.

    Everything served across **every view this principal may reach**, unioned in entity space, is
    exactly the mask: no view's response contains an entity outside it, and the combination of
    every view yields no identifier the mask does not already explain. The identity half rides on
    the same union — `tessera_id` is per entity and not per view, so a distinct identifier
    appearing across the views that the mask cannot account for would show up here as an entity
    that is not in it.

    **The gate moves the domain of the union and not its value** (§6). A principal who fails the
    gated group's gate unions four views and a principal who passes it unions six, and both equal
    the same mask — which is the shape a gate has to have: it withholds *coordinate systems*, and
    the entities behind it are the ones the principal's own label already admitted elsewhere. A
    gate that withheld entities would show up here as a union short of the mask.

    **What this proves and what it does not.** It is a differential over one corpus, three
    principals and six views at two zooms — evidence that the composition of these views discloses
    nothing beyond the mask, not a proof that no combination of views can. It does not reach:

    - **timing**, which §9 accepts as a C15-class channel and which no equality can observe.

    The union being *equal* to the mask, rather than merely contained in it, is a property of this
    corpus: `world` holds every entity and θ is saturated, so every visible entity is served
    somewhere. On a corpus where no view held the whole entity space, containment is all that
    could be asserted — and containment is the direction the invariant is about.
    """
    mask = masks[principal]
    union: set[int] = set()
    for view_id in _visible_views(principal):
        for zoom, bbox in ((ZOOM, _full_bbox(view_id)), (DEEP_ZOOM, _full_bbox(view_id))):
            raw = multiview_server.viewport(tokens[principal], view_id, zoom, bbox, k=K)
            served = _served_entities(raw, entity_of_fx)
            outside = served - mask
            assert not outside, (
                f"view '{view_id}' served {len(outside)} entities outside {principal}'s mask"
            )
            union |= served
    assert union == mask, (
        f"{principal}: every view together served {len(union)} entities against a mask of "
        f"{len(mask)} — {len(mask - union)} visible entities reached no view"
    )


# ---------------------------------------------------------------------------------------------
# Pinned leaves — views.md §5
# ---------------------------------------------------------------------------------------------


@pytest.fixture(scope="module")
def sentiment(multiview_bundle: Bundle):
    """The scoped family as the fixture planted it, `{view: NumericColumn}`.

    From the generation function, never from `attrs/` — the engine serves what the build stored and
    the oracle knows what the corpus was given, which is what makes the comparison a differential.
    """
    from oracle.filters import NumericColumn  # noqa: PLC0415 — one case needs the type

    return {
        view: NumericColumn(values=values)
        for view, values in mv.sentiment_columns(multiview_bundle).items()
    }


#: The predicate every pinned case uses. A bound at an exact multiple of 1/1024 so `f32` holds it,
#: and one that splits each column into two non-empty halves.
RANGE = {"range": {"gte": 0.0}}


@pytest.mark.parametrize(
    ("request_view", "leaf", "column_view"),
    [
        # Under a view of the group, the request's own view decides and nothing is pinned.
        (f"{mv.GROUP}:2026-Q2", "sentiment", f"{mv.GROUP}:2026-Q2"),
        # Under a plain view the leaf must pin, by key — a view's only address (decision 0113).
        # Q3 is the third of three, neither the first nor the last, so an off-by-one shows up.
        (mv.WORLD_VIEW, "sentiment@2026-Q3", f"{mv.GROUP}:2026-Q3"),
        # A pinned leaf under a view of the same group — Q3's map filtered by Q1's sentiment,
        # which §5 allows and says means what it says.
        (f"{mv.GROUP}:2026-Q3", "sentiment@2026-Q1", f"{mv.GROUP}:2026-Q1"),
    ],
)
@pytest.mark.parametrize("principal", ["narrow", "wide"])
def test_a_pinned_leaf_reads_the_pinned_views_column_in_entity_space(
    multiview_server,
    masks,
    members,
    tokens,
    entity_of_fx,
    sentiment,
    principal,
    request_view,
    leaf,
    column_view,
):
    """A scoped leaf's column is the **pinned** view's; its projection is the **request** view's.

    §5's evaluation rule in one equality. The oracle evaluates the family's column for
    `column_view` over the principal's mask — an ordinary entity-space bitmap, which is what keeps
    the scoped surface inside I2's argument — and then projects the result through the *request*
    view's membership. Two different views appear in one answer, and an implementation that used
    either one for both is caught: reading the request view's column gives a different value set,
    and projecting through the pinned view's membership gives a different row set.
    """
    mask = masks[principal]
    raw = multiview_server.viewport(
        tokens[principal],
        request_view,
        ZOOM,
        _full_bbox(request_view),
        k=K,
        filters={leaf: RANGE},
    )
    served = _served_entities(raw, entity_of_fx)

    column = sentiment[column_view]
    matched = {e for e in mask if column.matches(e, "range", RANGE["range"])}
    expected = matched & members[request_view]

    assert served == expected, (
        f"{principal}: '{leaf}' under '{request_view}' served {len(served)} entities, the oracle's "
        f"column for '{column_view}' projected into '{request_view}' gives {len(expected)}"
    )
    assert expected, "the predicate matched nothing, so this case checked no membership"


@pytest.fixture(scope="module")
def glow(multiview_bundle: Bundle):
    """The **render-only** family as the fixture planted it, `{view: NumericColumn}`."""
    from oracle.filters import NumericColumn  # noqa: PLC0415 — one case needs the type

    return {
        view: NumericColumn(values=values)
        for view, values in mv.glow_columns(multiview_bundle).items()
    }


@pytest.mark.parametrize(
    ("request_view", "leaf", "column_view"),
    [
        (f"{mv.GROUP}:2026-Q2", "glow", f"{mv.GROUP}:2026-Q2"),
        (mv.WORLD_VIEW, "glow@2026-Q3", f"{mv.GROUP}:2026-Q3"),
        (f"{mv.GROUP}:2026-Q3", "glow@2026-Q1", f"{mv.GROUP}:2026-Q1"),
    ],
)
@pytest.mark.parametrize("principal", ["narrow", "wide"])
def test_a_render_only_familys_leaf_reads_the_pinned_views_column_in_entity_space(
    multiview_server,
    masks,
    members,
    tokens,
    entity_of_fx,
    glow,
    principal,
    request_view,
    leaf,
    column_view,
):
    """`render` alone is the operand licence, and the column it reads is the pinned view's (§5 r26).

    The case above, asked of a family declared `render = true` and `index = false`. Two things
    could go wrong that the indexed family cannot show: the leaf could be refused as an unknown
    column, and — because a rendered family also occupies each view's **row tail** — it could be
    answered from the rows in front of the request rather than from the pinned view's entity-space
    column. The second is what the pinned cases discriminate: the row lane under `request_view`
    holds that view's values, and the expectation here is `column_view`'s.
    """
    mask = masks[principal]
    raw = multiview_server.viewport(
        tokens[principal],
        request_view,
        ZOOM,
        _full_bbox(request_view),
        k=K,
        filters={leaf: RANGE},
    )
    served = _served_entities(raw, entity_of_fx)

    column = glow[column_view]
    matched = {e for e in mask if column.matches(e, "range", RANGE["range"])}
    expected = matched & members[request_view]

    assert served == expected, (
        f"{principal}: '{leaf}' under '{request_view}' served {len(served)} entities, the oracle's "
        f"render-only column for '{column_view}' projected into '{request_view}' gives "
        f"{len(expected)}"
    )
    assert expected, "the predicate matched nothing, so this case checked no membership"


@pytest.fixture(scope="module")
def mood(multiview_bundle: Bundle):
    """The scoped **category** family as the fixture planted it, `{view: CategoryColumn}`."""
    from oracle.filters import CategoryColumn  # noqa: PLC0415 — one case needs the type

    return {
        view: CategoryColumn(values=values, codes=dict(mv.MOOD_CODES))
        for view, values in mv.mood_columns(multiview_bundle).items()
    }


@pytest.mark.parametrize(
    ("request_view", "leaf", "column_view"),
    [
        (f"{mv.GROUP}:2026-Q2", "mood", f"{mv.GROUP}:2026-Q2"),
        (mv.WORLD_VIEW, "mood@2026-Q3", f"{mv.GROUP}:2026-Q3"),
        (f"{mv.GROUP}:2026-Q3", "mood@2026-Q1", f"{mv.GROUP}:2026-Q1"),
    ],
)
@pytest.mark.parametrize("principal", ["narrow", "wide"])
def test_a_scoped_category_leaf_reads_the_pinned_views_postings(
    multiview_server,
    masks,
    members,
    tokens,
    entity_of_fx,
    mood,
    principal,
    request_view,
    leaf,
    column_view,
):
    """The numeric family's rule, over the family whose per-view artefact is **postings**.

    Same equality, same two views in one answer, and a different route beneath it: a category's
    `eq` is answered by intersecting one view's postings rather than by scanning a value column, so
    an implementation that resolved the column correctly for a value scan and read the family's
    first postings file here is caught by exactly this case and by no case above it.
    """
    mask = masks[principal]
    raw = multiview_server.viewport(
        tokens[principal],
        request_view,
        ZOOM,
        _full_bbox(request_view),
        k=K,
        filters={leaf: {"eq": "calm"}},
    )
    served = _served_entities(raw, entity_of_fx)

    column = mood[column_view]
    matched = {e for e in mask if column.matches(e, "calm")}
    expected = matched & members[request_view]

    assert served == expected, (
        f"{principal}: '{leaf}' under '{request_view}' served {len(served)} entities, the oracle's "
        f"column for '{column_view}' projected into '{request_view}' gives {len(expected)}"
    )
    assert expected, "the predicate matched nothing, so this case checked no membership"


def test_an_entity_with_no_value_in_the_pinned_view_never_matches(
    multiview_server, masks, members, tokens, entity_of_fx, sentiment
):
    """Per-view presence (decision 0064): absence is not a value, and it matches no predicate.

    About a fifth of each view's entities carry no `sentiment` there, and an entity's presence
    differs *by view* — one it has a value in and another it does not. So this is not the ordinary
    absent-value case restated: it is the case where the entity is present, visible, in the request
    view, and carries a value in a **different** view of the same group.

    Both directions are asserted, because only the pair is informative: the holes are absent from
    the pinned answer, and the same entities are present in an answer pinned at a view that does
    give them a value.
    """
    mask = masks["wide"]
    pinned, other = f"{mv.GROUP}:2026-Q3", f"{mv.GROUP}:2026-Q1"
    holes = {
        e
        for e in mask & members[pinned]
        if e not in sentiment[pinned].values and e in sentiment[other].values
    }
    assert len(holes) > 20, (
        f"only {len(holes)} entities have a hole in '{pinned}' and a value in '{other}', which is "
        "too few for this case to be checking presence"
    )

    raw = multiview_server.viewport(
        tokens["wide"],
        mv.WORLD_VIEW,
        ZOOM,
        _full_bbox(mv.WORLD_VIEW),
        k=K,
        filters={"sentiment@2026-Q3": RANGE},
    )
    assert not (_served_entities(raw, entity_of_fx) & holes), (
        "an entity with no value in the pinned view was served by a predicate over that view"
    )

    raw = multiview_server.viewport(
        tokens["wide"],
        mv.WORLD_VIEW,
        ZOOM,
        _full_bbox(mv.WORLD_VIEW),
        k=K,
        filters={"sentiment@2026-Q1": {"range": {"gte": -1.0}}},
    )
    served = _served_entities(raw, entity_of_fx)
    assert holes <= served, (
        "entities that hold a Q1 value were not served by a predicate every Q1 value satisfies, "
        "so the absence above was not about the pinned view"
    )


def test_an_unpinned_scoped_leaf_under_a_plain_view_is_refused_naming_the_group(
    multiview_server, tokens
):
    """§5: a leaf with no column to read is a malformed request, not an empty answer.

    A 422 rather than a 404 or a zero-count response, and the message names the group so a client
    knows what to pin. An empty answer here would be a filter that silently constrained nothing.
    """
    resp = multiview_server.viewport_request(
        tokens["wide"],
        mv.WORLD_VIEW,
        ZOOM,
        _full_bbox(mv.WORLD_VIEW),
        k=K,
        filters={"sentiment": RANGE},
    )
    assert resp.status_code == 422, resp.text
    assert mv.GROUP in resp.text


def test_a_pin_naming_no_view_of_the_group_is_the_unknown_view_refusal(
    multiview_server, tokens
):
    """A pin resolves through the same view resolution every verb uses, so an unknown key is its
    404 — the same answer an absent key and a misspelt id get. The retired `#<ordinal>` form is
    one of the misspellings now (decision 0113): it addresses nothing and is refused as any other
    unknown key is."""
    for leaf in ("sentiment@2099-Q9", "sentiment@#2"):
        resp = multiview_server.viewport_request(
            tokens["wide"],
            mv.WORLD_VIEW,
            ZOOM,
            _full_bbox(mv.WORLD_VIEW),
            k=K,
            filters={leaf: RANGE},
        )
        assert resp.status_code == 404, f"{leaf}: {resp.status_code} {resp.text}"


def test_a_filter_narrows_the_matched_count_and_never_the_visible_one_in_any_view(
    multiview_server, tokens
):
    """I12's frontier direction, per view: a pinned leaf moves `matched` and leaves `visible`.

    The multi-view form of the property `test_filter_differential.py` asserts against the
    catalogue. It is worth repeating here and nowhere else in this module, because a scoped column
    is the one filter operand whose *evaluation* depends on a view at all — if a view could reach
    the visible count through it, this is where it would show.
    """
    token = tokens["wide"]
    for view_id, leaf in (
        (mv.WORLD_VIEW, "sentiment@2026-Q2"),
        (f"{mv.GROUP}:2026-Q1", "sentiment"),
    ):
        bbox = _full_bbox(view_id)
        plain, _ = decode_viewport(multiview_server.viewport(token, view_id, ZOOM, bbox, k=K))
        filtered, _ = decode_viewport(
            multiview_server.viewport(token, view_id, ZOOM, bbox, k=K, filters={leaf: RANGE})
        )
        visible = {t: v for t, v, _m, _s in plain}
        assert {t: v for t, v, _m, _s in filtered} == visible, (
            f"'{leaf}' moved the visible count in view '{view_id}'"
        )
        assert all(m <= visible[t] for t, _v, m, _s in filtered)
        assert any(m < visible[t] for t, _v, m, _s in filtered), (
            f"'{leaf}' matched everything visible in '{view_id}', so nothing was narrowed"
        )


# ---------------------------------------------------------------------------------------------
# Ordering per view — contracts §2.6
# ---------------------------------------------------------------------------------------------


@pytest.mark.parametrize("view_id", mv.VIEW_IDS)
def test_points_are_served_ascending_by_identity_within_each_tile(
    multiview_server, tokens, view_id
):
    """Contracts §2.6's within-tile order, in every view.

    Order is contract rather than presentation: §7.2's nesting argument has the served set be a
    prefix of the tile's visible set by ascending `tessera_id`, and a client truncating a response
    relies on it. A view owns the permutation and therefore the storage order, so this is a claim
    about each row space separately — asserting it in the plain view alone would leave three
    unchecked.

    The flat points list is split back into tiles by the tile batch's own `served` column, which is
    what that column is on the wire for: under §7.2 the per-tile count cannot be recomputed from
    `k` and `visible`.
    """
    raw = multiview_server.viewport(
        tokens["wide"], view_id, ZOOM, _full_bbox(view_id), k=K
    )
    tiles, points = decode_viewport(raw)
    cursor = 0
    tiles_checked = 0
    for tile, _visible, _matched, served in tiles:
        idents = [ident for ident, _code in points[cursor : cursor + served]]
        cursor += served
        assert idents == sorted(idents), (
            f"view '{view_id}' tile {tile} served identities out of ascending order"
        )
        if served > 1:
            tiles_checked += 1
    assert cursor == len(points), "the points list must be exactly consumed by the tile batch"
    assert tiles_checked > 0, f"no tile in '{view_id}' served more than one point"


# ---------------------------------------------------------------------------------------------
# The gate — views.md §6
# ---------------------------------------------------------------------------------------------
#
# `sealed` is gated by a real access label, `cc4`. `wide` holds it; `narrow` and `disjoint` do not.
# The three cases below are three different claims and are kept apart deliberately: what a failing
# principal cannot find, what a passing principal is served, and that the two are the same gate.


@pytest.mark.parametrize("principal", sorted(PRINCIPALS))
def test_the_gated_groups_roster_is_absent_for_a_principal_who_fails_its_gate(
    multiview_server, tokens, principal
):
    """`/v1/meta` publishes the gated group to the principals that hold its label and to no other.

    A gate-failed **group** takes its whole roster with it, so this is one assertion over two
    surfaces of the same document: the `groups` entry and every one of its view ids in `views`. A
    roster that carried the group with an empty view list, or a view whose group had gone, would
    be the existence oracle by subtraction that filtering exists to prevent.

    The passing side is asserted with the same strictness, because a filter that removed the group
    from everybody would satisfy the failing half alone.
    """
    document = multiview_server.meta(tokens[principal])
    groups = {g["name"] for g in document["groups"]}
    served_views = {v["id"] for v in document["views"]}

    assert mv.GROUP in groups, "the ungated group is on every principal's roster"
    assert set(mv.VIEW_IDS) <= served_views

    if _passes_gate(principal):
        assert mv.SEALED_GROUP in groups, f"{principal} holds '{mv.GATE_LABEL}' and must see it"
        assert set(mv.SEALED_VIEW_IDS) <= served_views
        entry = next(g for g in document["groups"] if g["name"] == mv.SEALED_GROUP)
        assert entry["views"] == list(mv.SEALED_VIEW_IDS), (
            "a passed gate serves the whole roster in creation order"
        )
    else:
        assert mv.SEALED_GROUP not in groups, (
            f"{principal} does not hold '{mv.GATE_LABEL}', so the group is on no surface"
        )
        assert not (served_views & set(mv.SEALED_VIEW_IDS)), (
            "a gate-failed group takes its whole roster with it"
        )


@pytest.mark.parametrize("principal", ["narrow", "disjoint"])
def test_a_gate_failed_view_answers_exactly_as_a_name_nobody_declared(
    multiview_server, tokens, principal
):
    """Same status and same detail shape as an undeclared name (§6, contracts §3.2 r57).

    The comparison is against a name of the same *shape* — a key of the gated group that was never
    declared, and a group name that does not exist — with the name itself substituted out of both
    bodies, which is the only difference a client may legitimately see. A different code or a
    different sentence between the two would tell a principal that the name they guessed is a view
    somebody else can reach, which is the roster leaking one key at a time.
    """
    token = tokens[principal]
    cases = [
        (mv.SEALED_VIEW_IDS[0], f"{mv.SEALED_GROUP}:2099-H9"),
        (mv.SEALED_VIEW_IDS[1], "nosuchgroup:2026-H2"),
    ]
    for gated, absent in cases:
        gated_resp = multiview_server.viewport_request(
            token, gated, ZOOM, _full_bbox(gated), k=K
        )
        absent_resp = multiview_server.viewport_request(
            token, absent, ZOOM, _full_bbox(gated), k=K
        )
        assert gated_resp.status_code == 404, f"{gated}: {gated_resp.text}"
        assert absent_resp.status_code == 404, f"{absent}: {absent_resp.text}"
        assert gated_resp.text.replace(gated, "<view>") == absent_resp.text.replace(
            absent, "<view>"
        ), f"'{gated}' and '{absent}' must answer with one code and one detail shape"


@pytest.mark.parametrize("principal", ["narrow", "disjoint"])
def test_the_gated_groups_scoped_attribute_is_undeclared_for_a_principal_who_fails_it(
    multiview_server, tokens, principal
):
    """§5's collapse: for a principal who cannot reach the group, the whole family is not there.

    Both spellings — bare, and pinned at one of the group's keys — take the **ordinary
    unknown-column `422`**, the one a column nobody declared gets, and the detail names neither the
    group nor its keys. The pinned spelling is the one that matters: the pin carries a real key, so
    a refusal that resolved the view first would answer `404` here and confirm the key's existence
    by the code it chose.

    `filter_operands` is the same collapse on the discovery surface, and is asserted beside it
    because a client that trusted the document and never sent the leaf would learn the group from
    the document alone.
    """
    token = tokens[principal]
    document = multiview_server.meta(token)
    operands = {entry["column"] for entry in document.get("filter_operands", [])}
    assert "sentiment" in operands, "the ungated group's family is declared to everybody"
    assert "heat" not in operands, (
        f"{principal} cannot reach '{mv.SEALED_GROUP}', so its family is undeclared: {operands}"
    )

    bogus = multiview_server.viewport_request(
        token, mv.WORLD_VIEW, ZOOM, _full_bbox(mv.WORLD_VIEW), k=K, filters={"no_such_column": RANGE}
    )
    assert bogus.status_code == 422, bogus.text
    for leaf in ("heat", f"heat@{mv.SEALED_KEYS[0]}"):
        resp = multiview_server.viewport_request(
            token, mv.WORLD_VIEW, ZOOM, _full_bbox(mv.WORLD_VIEW), k=K, filters={leaf: RANGE}
        )
        assert resp.status_code == bogus.status_code, f"{leaf}: {resp.status_code} {resp.text}"
        assert mv.SEALED_GROUP not in resp.text, f"{leaf}: {resp.text}"
        # **The whole body, not the detail alone** — the 404 case's comparison, for its reason: a
        # refusal that carried the same sentence under a different `error` code would still tell a
        # principal that this column is one somebody else can filter on. The caller's own spelling
        # is echoed and the key inside it with it, which discloses nothing the caller did not
        # write, so it is substituted out of both sides exactly as the view name is there.
        assert resp.text.replace(leaf, "<column>") == bogus.text.replace(
            "no_such_column", "<column>"
        ), f"{leaf}: {resp.text}"


@pytest.mark.parametrize("view_id", mv.SEALED_VIEW_IDS)
def test_a_passed_gate_serves_exactly_what_an_ungated_view_would(
    multiview_bundle: Bundle, multiview_server, masks, members, tokens, entity_of_fx, view_id
):
    """The gate decides which views exist, and nothing about which items they hold.

    For the principal who passes it, the two equalities the ungated views are held to are asserted
    unchanged — the per-tile masked counts against the oracle answering through this view's own
    permutation and frame, and `served(view) == mask ∩ members(view)`. Neither expectation knows
    the gate exists: they are computed exactly as the public views' are, which is what makes this
    an equality against the *ungated* answer rather than against whatever the gate produced.

    The control is in the corpus. The gate's label is one compartment and this principal holds
    three, so an implementation that read the gate as a second row filter would serve `cc4`'s
    entities alone and disagree here — while agreeing with every count that the group is reachable
    at all.
    """
    principal = "wide"
    token, mask = tokens[principal], masks[principal]

    raw = multiview_server.viewport(token, view_id, ZOOM, _full_bbox(view_id), k=K)
    server_tiles, _points = decode_viewport(raw)
    oracle = vp.Selection(multiview_bundle, mask, view_id, ZOOM)
    expected = oracle.counts_for(
        morton.tiles_for_bbox(
            _full_bbox(view_id), ZOOM, multiview_bundle.extent_of(view_id)
        )
    )
    assert {t: v for t, v, _m, _s in server_tiles} == expected, (
        f"a gated view's counts disagree with the oracle's for {principal} in '{view_id}'"
    )

    served = _served_entities(raw, entity_of_fx)
    assert served == mask & members[view_id], (
        f"'{view_id}' behind a passed gate served {len(served)} entities against "
        f"{len(mask & members[view_id])} the mask and the membership license"
    )
    # The control the docstring names, and it is a claim about `cc1` rather than about the gate's
    # own `cc4`: an engine treating the gate as a row filter would serve `cc4`'s entities and no
    # others, so the presence of entities this principal holds through a *different* compartment —
    # `narrow`'s mask is `cc1` alone — is what separates the two readings. A strict subset, because
    # the view must also serve more than that one compartment.
    through_another_compartment = {e for e in served if e in masks["narrow"]}
    assert through_another_compartment and through_another_compartment < served, (
        f"'{view_id}' served {len(through_another_compartment)} of `cc1`'s entities out of "
        f"{len(served)}: a gate read as a row filter over its own label would serve none of them, "
        "and a corpus where they are the whole answer cannot tell the two readings apart"
    )


@pytest.mark.parametrize("leaf_view", mv.SEALED_KEYS)
def test_a_pinned_leaf_behind_a_passed_gate_reads_that_views_column(
    multiview_server, masks, members, tokens, entity_of_fx, multiview_bundle: Bundle, leaf_view
):
    """§5's pinned leaf, over the gated group, for the principal who may reach it.

    The same equality the ungated family is held to — the pinned view's column over the mask,
    projected through the request view's membership — asserted here because a gate that admitted
    the *view* and left the family resolving against the wrong group's column would pass every
    case above it: the counts are unfiltered and the refusal cases are the other principal's.
    """
    from oracle.filters import NumericColumn  # noqa: PLC0415 — one case needs the type

    principal = "wide"
    mask = masks[principal]
    column_view = f"{mv.SEALED_GROUP}:{leaf_view}"
    column = NumericColumn(values=mv.heat_columns(multiview_bundle)[column_view])

    raw = multiview_server.viewport(
        tokens[principal],
        mv.WORLD_VIEW,
        ZOOM,
        _full_bbox(mv.WORLD_VIEW),
        k=K,
        filters={f"heat@{leaf_view}": RANGE},
    )
    served = _served_entities(raw, entity_of_fx)
    expected = {
        e for e in mask if column.matches(e, "range", RANGE["range"])
    } & members[mv.WORLD_VIEW]
    assert served == expected, (
        f"'heat@{leaf_view}' under '{mv.WORLD_VIEW}' served {len(served)} entities, the oracle's "
        f"column for '{column_view}' gives {len(expected)}"
    )
    assert expected, "the predicate matched nothing, so this case checked no membership"
