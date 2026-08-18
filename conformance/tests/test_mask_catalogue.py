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

import numpy as np
import pytest

from oracle import catalogue as cat
from oracle import mask as mask_mod
from oracle.bundle import Bundle
from oracle.wire import decode_viewport_points


def test_the_catalogue_bundle_identity_column_is_the_key_the_fixture_supplied(
    catalogue_bundle: Bundle,
):
    """The one artefact the §7.2 oracle shares with the engine, checked rather than trusted.

    `viewport.Selection` sorts by the segment's stored `tessera_id` column, and so does the engine.
    That is a shared artefact on the exact quantity the I7 differential exists to referee: a build
    writing a wrong-but-self-consistent identity column — say one still correlated with
    term-signature order, the r21 disclosure the negative control's docstring invokes — would be
    agreed with rather than caught, because both sides would read the same wrong values.

    Three checks close it, and the fixture supplies the key, so nothing is left on trust:

    1. the stored column **is** `forward(key, shard_id, entity_id)` for a sample of rows, where the
       entity id comes from the permutation (key-independent) and the identity from the column;
    2. the rows are **stored in the order that key implies** — re-derived from `(x, y)` and the
       permutation, never from the stored `morton`/`tessera_id` columns — so `derive_row_order` is
       the identity permutation;
    3. MANIFEST's identity key is the one `oracle/catalogue.py` put in the build's environment,
       not one the build minted for itself. Without this, 1 and 2 would hold against *any* self-consistent
       key, including one the fixture never chose.

    `verify_identity_cross_check` and `derive_row_order` already existed but ran only against the
    250k `--mint-id-key` fixture, whose key is a build output rather than a fixture input — so the
    catalogue, which is the corpus every §7.2 assertion is made over, was never covered.
    """
    catalogue_bundle.verify_identity_cross_check(cat.VIEW_ID)

    order = catalogue_bundle.derive_row_order(cat.VIEW_ID)
    assert np.array_equal(order, np.arange(len(order))), (
        "the catalogue's rows are not stored in (morton, tessera_id) order re-derived from "
        "geometry and the identity key — so the stored order is not the order §7.2 selects in"
    )

    declared = catalogue_bundle.manifest["identity"]["key"]
    assert declared.lower() == cat.CATALOGUE_ID_KEY_HEX.lower(), (
        f"MANIFEST's identity key is {declared!r}, not the {cat.CATALOGUE_ID_KEY_HEX!r} the "
        "fixture states through the environment. The bundle was built by something other than "
        "`build_catalogue_bundle`, or the build ignored the variable and minted its own key — "
        "either "
        "way every tessera_id in it is a value nobody chose."
    )


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

    **The list is written out here rather than imported from `oracle.catalogue`, and that is the
    point of the test.** Exporting a `CATALOGUE_NAMES` constant and comparing the catalogue against
    it would make this tautological: both sides would come from the same edit, and deleting a case
    would delete it from both. The expected list has to live somewhere that is *not* the
    definition, and conformance design §2 is that somewhere — this is its transcription. Adding a
    catalogue member therefore costs an edit here, which is correct: the design's list grew, and
    this test asserts the catalogue matches the design's list.
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

    Reads the keys from `cat.fx_keys()`, a pure function of the seed, rather than by calling
    `write_corpus`: this test used to *write the corpus parquet* as a side effect of asking what
    the keys were, which made it order-dependent (alone on a fresh machine it raised a pyarrow
    error rather than failing) and made two concurrent sessions race over the same `/tmp` path.
    """
    fx = cat.fx_keys()
    assert len(fx) == cat.N_ITEMS
    assert len(set(fx)) == cat.N_ITEMS, "fx_key must be unique per item — it is the join key"

    # No affine relationship: an entity-ID-derived key would make the differences constant.
    differences = {fx[i + 1] - fx[i] for i in range(0, 500)}
    assert len(differences) > 400, (
        "consecutive fx_keys differ by an almost-constant amount, which means the key is derived "
        "from the entity id — see this test's docstring for why that is an I10 breach, not a "
        "shortcut"
    )


def test_fx_key_is_served_in_the_points_batch(catalogue_bundle: Bundle, catalogue_server):
    """The join `AckedJournal` and the I2 canonicalisation both need: a served point names its
    fixture item, with no reverse map, no extra endpoint, and no external ID on the viewer plane.

    **The assertion is on the served column, not on MANIFEST.** An earlier version of this body
    checked only that MANIFEST declared a scalar named `fx_key`, and carried a strict xfail while
    the build could not emit one. Had it stayed manifest-only, the day the build gained support it
    would have XPASSed, the marker would have been deleted, and the wire path would have stayed
    untested while "fx_key works" read green. The manifest check survives as the first line because
    it is the precondition; the property this test is named for is the last three — request a
    viewport, decode the points batch, and check each served point's `fx_key` against the value the
    fixture planted for that item.

    The xfail was removed on 2026-08-07, when `tessera build` gained `--schema` and the catalogue
    fixture began declaring `fx_key`. That the marker was **strict** is why this became a test that
    flipped rather than a gap somebody had to remember.
    """
    assert catalogue_bundle.manifest["declared_scalars"], (
        "MANIFEST declares no scalars, so no points batch can carry fx_key"
    )
    assert any(d["name"] == "fx_key" for d in catalogue_bundle.manifest["declared_scalars"])

    case = next(c for c in cat.catalogue() if c.name == "full_100pct")
    token = catalogue_server.authorise(list(case.grants))["token"]
    raw = catalogue_server.viewport(token, cat.VIEW_ID, 4, cat.FULL_VIEWPORT, k=30)
    points = decode_viewport_points(raw)

    assert "fx_key" in points.schema.names, (
        f"the points batch carries {points.schema.names}, with no fx_key column — MANIFEST "
        "declares the scalar but the gather does not serve it"
    )
    assert points.num_rows > 0, "no points were served, so nothing was checked"

    # The fixture planted `source_id -> fx_key`, and `entity_id == source_id` for this corpus
    # (`verify()` proves that from the bundle's own postings). The identity->entity map is built
    # from the segment because only the fixture may make that translation: on the viewer plane an
    # identity is opaque (I10), and this test is the fixture, not a viewer.
    planted = cat.fx_keys()
    seg = catalogue_bundle.segment(cat.VIEW_ID)
    entity_of = {int(seg.tessera_id[row]): int(seg.entity_id[row]) for row in range(seg.row_count)}
    for ident, key in zip(points.column("tessera_id").to_pylist(), points.column("fx_key").to_pylist()):
        assert key == planted[entity_of[ident]], (
            f"served fx_key {key} for tessera_id {ident} is not the planted key "
            f"{planted[entity_of[ident]]} — the join the whole catalogue depends on is wrong"
        )
