"""**The drill-down serves the satisfied labels and no others** — a differential against the flat
`(entity_id, term_id)` relation ([decision 0114](../../docs/decisions/0114-the-drill-down-serves-the-satisfied-labels-only.md),
contracts §3.2, design §7.4; Appendix C's C30).

The rule is an intersection, so a differential over it is worth writing exactly because both sides
of the intersection are already in the fixture: `terms/pairs.parquet` says what each entity carries
and the case's `grants` say what the principal holds. The oracle computes the intersection from the
flat relation — the same file `oracle.mask` derives a mask from, and never the transpose the engine
reads — so the two arrive at the same answer by different artefacts, which is the whole point of a
differential over a second copy of one relation.

**The assertion that matters is the negative one, and this suite cannot make it. Stated as a
negative result rather than left to be discovered.** A drill-down that served the item's *full*
label set would pass everything below: the mask catalogue plants **exactly one term per entity**
(measured: 150,000 entities, all of cardinality one — the disjoint contiguous blocks are what make
its masks exact sets, which is the fixture's whole design), and `oracle.multiview` likewise gives
each item one compartment. Over a corpus where no item carries a term its viewer lacks, "the
intersection" and "the full set" are the same answer, so no fixture here can tell them apart.

What the cases below therefore prove is that the served array is *exactly* the intersection where
the intersection is the whole set — which catches an engine serving nothing, serving the wrong
descriptor, or serving them unsorted, and does not catch the disclosure. The **withholding** case is
covered at the server level instead, in `crates/tessera-server/tests/item_labels.rs`, whose fixture
gives every third item a second label: there a principal holding one of the two is served one, and
a principal holding the other is served the other. Closing it here needs a conformance fixture with
multi-compartment items, which is a fixture regeneration rather than a test to write.

**Why the join runs through `fx_key` and not through an inverted identity.** A served point carries
its planted key and its opaque `tessera_id`; `oracle.catalogue.entity_of_fx_key` is the suite's one
legitimate handle→item join, and using it here keeps this module on the same side of **I10** as
every other differential — no entity id crosses the boundary, in either direction.
"""

from __future__ import annotations

import pyarrow.parquet as pq
import pytest

from oracle import catalogue as cat
from oracle.wire import decode_viewport, decode_viewport_points

VIEW = cat.VIEW_ID

#: Enough drill-downs to cover both shapes of item in every case (carrying a granted term only, and
#: carrying a granted term beside one the principal lacks) without turning a differential into a
#: load test. Each is one HTTP round trip against a live server.
ITEM_SAMPLE = 40

def _terms_by_entity(bundle) -> dict[int, set[int]]:
    """`entity -> {term id}` from `terms/pairs.parquet`.

    The flat relation, not the engine's transpose: `entities/terms/` is what the service reads, and
    an oracle that read it too would be checking a file against itself.
    """
    table = pq.read_table(bundle.pairs_path(), columns=["entity_id", "term_id"])
    out: dict[int, set[int]] = {}
    for entity, term in zip(
        table.column("entity_id").to_pylist(), table.column("term_id").to_pylist()
    ):
        out.setdefault(int(entity), set()).add(int(term))
    return out


#: The label every principal holds, added inside the trust boundary at `authorise` and therefore in
#: every session's satisfied set whatever the credential said (design §6.1). A drill-down on an item
#: carrying it names it for everyone, so the oracle's satisfied set is the grants **plus this** —
#: modelled even though the mask catalogue plants it on no item, because a fixture that later does
#: would otherwise fail this differential against a correct engine.
PUBLIC_LABEL = "public"


def _granted_descriptors(bundle, grants) -> dict[int, str]:
    """`term id -> descriptor` for what this session satisfies: the credential's own descriptors,
    plus `public`.

    Resolved through the bundle's dictionary rather than computed, exactly as
    `MaskCase.dict_term_id` is and for the same reason: the interning rule is checked in one place
    and used everywhere else. `public` is resolved the same way and is **not** asserted present —
    every build interns it first, but a bundle whose dictionary lacks it grants nobody anything
    through it, which is the narrow direction and the one the engine takes too.
    """
    resolved = {}
    for descriptor in grants:
        term_id = bundle.term_id_of(descriptor.encode("ascii"))
        assert term_id is not None, (
            f"descriptor {descriptor!r} is not in the bundle's dictionary — the corpus and the "
            "bundle disagree about what was interned"
        )
        resolved[term_id] = descriptor
    public = bundle.term_id_of(PUBLIC_LABEL.encode("ascii"))
    if public is not None:
        resolved[public] = PUBLIC_LABEL
    return resolved


def _served_sample(server, token: str):
    """`[(tessera_id, fx_key)]` from one whole-map viewport — the only route a client has to an
    identifier it may drill down on."""
    body = server.viewport(token, VIEW, 0, cat.FULL_VIEWPORT, k=500)
    # A zero-visibility principal's response carries no points frame at all, so the schema-bearing
    # decode has nothing to read — the tuple decode answers the empty list for it.
    _tiles, rows = decode_viewport(body)
    if not rows:
        return []
    points = decode_viewport_points(body)
    pairs = list(
        zip(
            points.column("tessera_id").to_pylist(),
            points.column("fx_key").to_pylist(),
        )
    )
    return pairs[:ITEM_SAMPLE]


@pytest.mark.parametrize("case", cat.catalogue(), ids=lambda c: c.name)
def test_the_served_labels_are_the_oracle_s_intersection(
    catalogue_bundle, catalogue_server, case
):
    """`labels` equals the item's terms intersected with the principal's, as descriptors, sorted.

    Exact equality both ways: a subset assertion would pass an engine that served nothing at all,
    and a superset one would pass the very disclosure the ruling forbids.
    """
    token = catalogue_server.authorise(list(case.grants))["token"]
    sample = _served_sample(catalogue_server, token)
    if not sample:
        pytest.skip(f"{case.name} has no visible item to drill down on")

    entity_of_key = cat.entity_of_fx_key(catalogue_bundle)
    terms = _terms_by_entity(catalogue_bundle)
    granted = _granted_descriptors(catalogue_bundle, case.grants)

    for tessera_id, fx_key in sample:
        entity = entity_of_key[fx_key]
        expected = sorted(
            granted[term] for term in terms.get(entity, set()) if term in granted
        )
        response = catalogue_server.item(token, tessera_id)
        assert response.status_code == 200, (
            f"{case.name}: an item this principal's own viewport served must drill down"
        )
        assert response.json()["labels"] == expected, (
            f"{case.name}: the item at fx_key {fx_key} carries "
            f"{sorted(terms.get(entity, set()))} and this principal holds {sorted(granted)}"
        )


def test_a_zero_visibility_principal_drills_down_on_nothing(catalogue_server):
    """The `empty` case's own shape: a principal who satisfies no term has no item to ask about,
    so there is no response for a label to appear in. Stated because the intersection with an
    empty satisfied set is the one case where "serve the full set" and "serve the intersection"
    differ by everything."""
    token = catalogue_server.authorise([])["token"]
    body = catalogue_server.viewport(token, VIEW, 0, cat.FULL_VIEWPORT, k=500)
    _tiles, points = decode_viewport(body)
    assert points == [], "a zero-visibility principal is served no point"
