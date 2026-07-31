"""The adversarial mask catalogue's own claims, checked against the built bundle.

A fixture that has quietly stopped being adversarial is worse than no fixture: every test built on
it goes on passing while testing a shape nobody chose. `oracle.catalogue.verify()` re-derives every
structural claim from the bundle's own postings and geometry, and this module turns each into an
assertion.

Two of the claims are not decoration. **Container-boundary**: Roaring's cost model is O(containers
touched), so a mask that never crosses a multiple of 65,536 exercises no container arithmetic at
all — and at 10⁴ dense entity IDs, no mask ever does, which is why the catalogue's corpus is
150,000 items with a block placed astride the boundary deliberately. **The crossover pair**: §7.2
r18 puts the direct-evaluation/candidate-list crossover at roughly 5% coverage, so a catalogue that
does not straddle it cannot tell whether both routes compute the same definition.
"""

from __future__ import annotations

import pytest

from oracle import catalogue as cat
from oracle import mask as mask_mod
from oracle.bundle import Bundle


def test_the_catalogue_corpus_is_the_shape_it_claims(catalogue_bundle: Bundle):
    """Every structural claim `oracle/catalogue.py` makes, re-derived from the bundle.

    Reported all at once rather than one at a time: when the build's entity-ID assignment changes,
    every block moves, and seeing the whole list is what tells a reader that it is the assignment
    that moved and not one block's arithmetic.
    """
    report = cat.verify(catalogue_bundle)
    assert not report.failures, "the mask catalogue has drifted:\n  " + "\n  ".join(report.failures)
    assert report.row_count == cat.N_ITEMS


@pytest.mark.parametrize("case", cat.catalogue(), ids=lambda c: c.name)
def test_each_case_reaches_exactly_the_entity_set_it_names(catalogue_bundle: Bundle, case):
    """A case's grant set must produce its declared entity set — by a route independent of the one
    `verify()` uses.

    `verify()` reads `postings.arrow`, the per-term arrays the engine actually serves queries from.
    This reads `pairs.parquet`, the flat `(entity_id, term_id)` relation, and scans it. Agreement
    between a union of posting lists and a semi-join over the flat relation is the same
    union-vs-semi-join differential `reference/tests` runs at 250k, applied here to the shapes that
    were designed to be awkward.
    """
    term_ids = {catalogue_bundle.term_id_of(g.encode("ascii")) for g in case.grants}
    assert None not in term_ids, f"{case.name}: a grant descriptor is not in the dictionary"

    from_pairs = mask_mod.mask_of(term_ids, catalogue_bundle.pairs_path())
    assert from_pairs == set(case.entities), (
        f"{case.name} ({case.attacks}): the pairs-derived mask is not the entity set the case "
        f"names — got {len(from_pairs)} entities, expected {len(case.entities)}"
    )


def test_the_catalogue_covers_the_properties_the_design_names():
    """The catalogue is complete against conformance design §2's build-time list.

    Guards against the failure this whole module exists for, one level up: a case being *deleted*
    rather than drifting. The two states conformance design §2 also names — watermark-straddling
    and post-deletion at every ledger stage — are runtime states driven through `AckedJournal`, not
    members of this list, and are named here so their absence is a decision rather than an
    oversight.
    """
    names = {c.name for c in cat.catalogue()}
    assert names == {
        "empty",
        "single_item",
        "sparse_0_01pct",
        "full_100pct",
        "crossover_below",
        "crossover_above",
        "container_boundary",
        "all_in_one_tile",
    }
    assert cat.overlay_heavy_base().name == "overlay_heavy_base"


def test_fx_keys_are_unique_and_not_derived_from_the_entity_id():
    """`fx_key` must be a *planted* join key, not an encoding of the entity ID.

    Conformance design decision 4 makes `fx_key` the handle→item join precisely so the suite needs
    no reverse map and creates no I10 tension. That argument only holds if the key carries no
    entity-ID information: a key that were `f(entity_id)` for any invertible `f` would, the moment
    it is served in the points batch, *be* an entity ID crossing the trust boundary — the exact
    disclosure I10 forbids and `test_byte_scan.py` sweeps for.

    Checked here rather than asserted in prose because the tempting implementation (`fx_key =
    entity_id`, or `BASE + entity_id`) is one line and looks harmless.
    """
    _points, _pairs, fx = cat.write_corpus(cat.DEFAULT_WORK_DIR)
    assert len(fx) == cat.N_ITEMS
    assert len(set(fx)) == cat.N_ITEMS, "fx_key must be unique per item — it is the join key"

    # No affine relationship: an entity-ID-derived key would make the differences constant.
    differences = {fx[i + 1] - fx[i] for i in range(0, 500)}
    assert len(differences) > 400, (
        "consecutive fx_keys differ by an almost-constant amount, which means the key is derived "
        "from the entity id — see this test's docstring for why that is an I10 breach, not a "
        "shortcut"
    )


@pytest.mark.xfail(
    strict=True,
    reason=(
        "BLOCKED ON A RUST CHANGE (Track T does not own any crate — reported to the controller). "
        "`fx_key` is planted in the catalogue's points parquet, but no built bundle can carry it: "
        "tessera-build writes `declared_scalars: Vec::new()` into MANIFEST and `scalars: "
        "Vec::new()` onto every tiler item, so the engine has no declared-scalar column to serve. "
        "The plumbing exists on both sides of the gap — tessera-store::write_segment takes a "
        "scalar schema, tessera-wire::viewport_ipc emits scalar columns, and /control/ingest "
        "parses them — only the build does not connect them. STRICT xfail on purpose: this test "
        "must fail the day the build gains support, so the gap cannot be forgotten."
    ),
)
def test_fx_key_is_served_in_the_points_batch(catalogue_bundle: Bundle):
    """The join `AckedJournal` and the I2 canonicalisation both need: a served point names its
    fixture item, with no reverse map, no extra endpoint, and no external ID on the viewer plane.
    """
    assert catalogue_bundle.manifest["declared_scalars"], (
        "MANIFEST declares no scalars, so no points batch can carry fx_key"
    )
    assert any(d["name"] == "fx_key" for d in catalogue_bundle.manifest["declared_scalars"])
