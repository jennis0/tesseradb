"""The fixture-reuse receipt, which is the fix for a defect that has now happened twice.

A fixture bundle is built once per machine at a fixed `/tmp` path and reused across sessions, so
"may this one be reused?" is answered on every run of both suites. Answering it by *inspecting the
bundle* is an allowlist — it has to be extended in step with every new build input, and the input
nobody adds is the one that then goes wrong silently. It failed that way on MANIFEST's `identity`
object, again on `--mint-external-ids`, and the catalogue builder had reintroduced it a third time
with a `CURRENT`-plus-`identity` predicate over inputs it shares none of.

The receipt inverts it: the builder stamps the whole input set beside the bundle, and reuse is
equality against the input set wanted now. Adding an input can then only fail in the safe
direction. **These tests are of the mechanism, not of a build** — no `tessera build` runs here, so
they cost milliseconds and can enumerate the cases a real build never would.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from oracle import catalogue as cat
from oracle import harness


@pytest.fixture
def work_dir(tmp_path: Path) -> Path:
    return tmp_path


def _stamped(bundle_root: Path, recipe: dict) -> None:
    """A bundle that looks built: a receipt, a CURRENT, and a MANIFEST with an identity."""
    prefix = "v00000"
    (bundle_root / prefix).mkdir(parents=True, exist_ok=True)
    (bundle_root / "CURRENT").write_text(json.dumps({"prefix": prefix}))
    (bundle_root / prefix / "MANIFEST.json").write_text(json.dumps({"identity": {"key": "00"}}))
    harness.write_recipe(bundle_root, recipe)


@pytest.mark.parametrize(
    "attribute,value",
    [
        ("SEED", 1),
        ("_FX_SEED", 1),
        ("_GEOMETRY_SEED", 1),
        ("ONE_TILE_TX", 3),
        ("ONE_TILE_TY", 3),
        ("ONE_TILE_DEPTH", 5),
        ("EXTENT_ARG", "0,1024,0,1024"),
        ("SLICE_ID", "s9"),
        ("CATALOGUE_ID_KEY_HEX", "0102030405060708090a0b0c0d0e0f10"),
        ("N_ITEMS", 1234),
        ("_LAYOUT", [("only", 10)]),
        ("POINTS_NAME", "other.parquet"),
    ],
)
def test_every_input_the_catalogue_is_a_function_of_changes_its_recipe(
    monkeypatch, work_dir: Path, attribute: str, value
):
    """The parametrisation **is** the assertion: this is the list of inputs, enumerated.

    Each of these silently changes what a rebuild would produce. The two that motivated the whole
    receipt are `SEED` and `ONE_TILE_TX`: change either, and `verify()` still passes — it re-derives
    geometry from the *bundle* — while every planted `fx_key` and every geometric claim in the
    suite is computed from the *new* corpus. `CATALOGUE_ID_KEY_HEX` is the one with the widest
    blast radius, because `tessera_id` is §7.2's entire served order.
    """
    bundle_root = work_dir / "bundle"
    before = cat.recipe(work_dir, bundle_root)
    monkeypatch.setattr(cat, attribute, value)
    assert cat.recipe(work_dir, bundle_root) != before, (
        f"changing {attribute} does not change the recipe, so a bundle built before the change "
        "would be reused after it — silently, because nothing else compares the two"
    )


def test_a_matching_receipt_is_reused_and_a_stale_one_is_not(work_dir: Path, monkeypatch):
    bundle_root = work_dir / "bundle"
    wanted = cat.recipe(work_dir, bundle_root)

    assert not cat._is_usable_bundle(bundle_root, wanted), "no bundle at all must not be reused"

    _stamped(bundle_root, wanted)
    assert cat._is_usable_bundle(bundle_root, wanted), "an exact receipt match must be reused"

    monkeypatch.setattr(cat, "SEED", cat.SEED + 1)
    assert not cat._is_usable_bundle(bundle_root, cat.recipe(work_dir, bundle_root))


def test_an_unstamped_or_damaged_bundle_is_never_reused(work_dir: Path):
    """Both halves of the gate, and both fail closed.

    An unstamped bundle is the state every machine is in the first time this lands, and every
    externally-built bundle is in forever: it must rebuild rather than be assumed compatible. A
    stamped bundle whose files were removed afterwards — a wiped `/tmp`, a half-deleted tree — is
    the case the receipt alone cannot see, which is why the structural check is still there.
    """
    bundle_root = work_dir / "bundle"
    wanted = cat.recipe(work_dir, bundle_root)

    _stamped(bundle_root, wanted)
    harness.recipe_path(bundle_root).unlink()
    assert not cat._is_usable_bundle(bundle_root, wanted), "an unstamped bundle must be rebuilt"

    _stamped(bundle_root, wanted)
    (bundle_root / "CURRENT").unlink()
    assert not cat._is_usable_bundle(bundle_root, wanted), "a damaged bundle must be rebuilt"


def test_the_receipt_lives_beside_the_bundle_and_not_inside_it(work_dir: Path):
    """The bundle is a `tessera build` output and the suite audits it byte by byte; a
    fixture-management file inside it would be the suite planting something in its own evidence."""
    bundle_root = work_dir / "bundle"
    assert harness.recipe_path(bundle_root).parent == bundle_root.parent
    assert bundle_root not in harness.recipe_path(bundle_root).parents


def test_the_250k_fixture_recipe_covers_every_build_argument(work_dir: Path):
    """`ensure_fixture_bundle`'s inputs are all `tessera build` arguments, so the argv is the
    recipe — with `--out` and the binary path dropped, since neither is a property of the fixture
    and both differ per worktree.

    The two flags in it are the two that broke this before: `--mint-id-key` (r6 refuses to build
    without a lineage decision) and `--mint-external-ids` (every `/control/changes` test needs one
    to address an item *by*). Under the old predicate, a bundle built before either flag existed
    was reused and the failure surfaced as a `KeyError` deep inside the oracle.
    """
    argv = harness._fixture_build_argv(
        work_dir / "bundle",
        points="p.parquet",
        pairs="q.parquet",
        limit=250_000,
        extent="0,65536,0,65536",
        slice_id="s0",
    )
    recipe = harness.fixture_recipe(argv)["build_argv"]

    assert "--out" not in recipe and str(harness.CLI_BIN) not in recipe
    for expected in ("p.parquet", "q.parquet", "250000", "--mint-id-key", "--mint-external-ids"):
        assert expected in recipe, f"{expected} is not in the recipe, so a change to it is silent"

    without_limit = harness.fixture_recipe(
        harness._fixture_build_argv(
            work_dir / "bundle",
            points="p.parquet",
            pairs="q.parquet",
            limit=None,
            extent="0,65536,0,65536",
            slice_id="s0",
        )
    )
    assert without_limit["build_argv"] != recipe, (
        "an unlimited build and a 250k build share a recipe, so the 10^9 fixture and the small "
        "one would be reused for each other"
    )
