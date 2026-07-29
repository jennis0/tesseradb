"""Known-answer tests for the `tessera_id` bijection (Task 12, Step 1).

Every block in `reference/vectors/tessera_id.json` is asserted, and -- per the reviewer
flag recorded in the vectors file's own `notes.directions` (which only prose-scopes the
both-directions rule to the `vectors` block) -- **both directions** are asserted for
`inverse_only` and `secondary_key.vectors` too: forward(shard, entity) == tessera_id AND
invert(tessera_id) == (shard, entity). This is valid because the construction is a *total*
bijection over 2**64 (memo §1.7): `invert` never fails on a structurally valid u64, so
`forward(*invert(x)) == x` must hold for every entry, `inverse_only` included.

This module was written against `docs/design-memos/2026-07-30-tessera-id-construction.md`
and this vectors file only -- no Rust was read while writing `oracle/identity.py` or this
test file.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from oracle import identity as ident
from oracle import morton

VECTORS_PATH = (
    Path(__file__).resolve().parents[1] / "vectors" / "tessera_id.json"
)


@pytest.fixture(scope="module")
def vectors() -> dict:
    return json.loads(VECTORS_PATH.read_text())


@pytest.fixture(scope="module")
def canonical_key(vectors) -> ident.IdentityKey:
    return ident.IdentityKey.from_hex(vectors["key"])


def _u(hexstr: str) -> int:
    return int(hexstr, 16)


# --- File-level identity: catches the file being swapped to a different construction ----


def test_file_construction_and_rounds_match_the_module(vectors):
    """One-line check that catches the vectors file being swapped to a different
    construction (task-5 review minors): `construction` must be the string this module
    implements, and `rounds` must equal `ident.ROUNDS`, not merely be presumed to."""
    assert vectors["construction"] == "feistel-splitmix64-v1"
    assert vectors["rounds"] == ident.ROUNDS


# --- §1.2 key parsing: the worked example in the memo text itself -----------------------


def test_key_parsing_worked_example():
    key = ident.IdentityKey.from_hex("000102030405060708090a0b0c0d0e0f")
    assert key.k0 == 0x0706050403020100
    assert key.k1 == 0x0F0E0D0C0B0A0908


# --- splitmix64: 6 known answers ---------------------------------------------------------


def test_splitmix64_vectors(vectors):
    cases = vectors["splitmix64"]
    assert len(cases) == 6
    for case in cases:
        got = ident.splitmix64(_u(case["input"]))
        assert got == _u(case["output"]), f"splitmix64({case['input']}) mismatch"


# --- key schedule: k0, k1, 8 round keys --------------------------------------------------


def test_key_schedule(vectors, canonical_key):
    ks = vectors["key_schedule"]
    key = ident.IdentityKey.from_hex(ks["key"])
    assert key.k0 == _u(ks["k0"])
    assert key.k1 == _u(ks["k1"])
    assert key == canonical_key

    expected_round_keys = [_u(rk) for rk in ks["round_keys"]]
    assert len(expected_round_keys) == ident.ROUNDS
    got_round_keys = key.round_keys()
    assert got_round_keys == expected_round_keys


# --- vectors: 33 entries, asserted in BOTH directions ------------------------------------


def test_main_vectors_both_directions(vectors, canonical_key):
    cases = vectors["vectors"]
    assert len(cases) == 33
    for case in cases:
        shard_id = case["shard_id"]
        entity_id = case["entity_id"]
        tessera_id = _u(case["tessera_id"])

        forward_got = ident.forward(canonical_key, shard_id, entity_id)
        assert forward_got == tessera_id, (
            f"forward(shard={shard_id}, entity={entity_id}) = {forward_got:#x}, "
            f"expected {tessera_id:#x}"
        )

        invert_got = ident.invert(canonical_key, tessera_id)
        assert invert_got == (shard_id, entity_id), (
            f"invert({tessera_id:#x}) = {invert_got}, expected ({shard_id}, {entity_id})"
        )


# --- inverse_only: 6 entries, asserted in BOTH directions (the reviewer-flagged gap) -----


def test_inverse_only_both_directions(vectors, canonical_key):
    cases = vectors["inverse_only"]
    assert len(cases) == 6
    for case in cases:
        tessera_id = _u(case["tessera_id"])
        shard_id = case["shard_id"]
        entity_id = case["entity_id"]

        invert_got = ident.invert(canonical_key, tessera_id)
        assert invert_got == (shard_id, entity_id), (
            f"invert({tessera_id:#x}) = {invert_got}, expected ({shard_id}, {entity_id})"
        )

        # Both directions: the construction is a total bijection over 2**64 (memo §1.7), so
        # forward(*invert(x)) == x must hold even for an arbitrary tessera_id not drawn from
        # a forward-generated vector.
        forward_got = ident.forward(canonical_key, shard_id, entity_id)
        assert forward_got == tessera_id, (
            f"forward(shard={shard_id}, entity={entity_id}) = {forward_got:#x}, "
            f"expected {tessera_id:#x}"
        )


# --- secondary_key.vectors: 6 entries, asserted in BOTH directions (the reviewer-flagged gap)


def test_secondary_key_both_directions(vectors):
    sk = vectors["secondary_key"]
    key = ident.IdentityKey.from_hex(sk["key"])
    assert key.k0 == _u(sk["k0"])
    assert key.k1 == _u(sk["k1"])

    cases = sk["vectors"]
    assert len(cases) == 6
    for case in cases:
        shard_id = case["shard_id"]
        entity_id = case["entity_id"]
        tessera_id = _u(case["tessera_id"])

        forward_got = ident.forward(key, shard_id, entity_id)
        assert forward_got == tessera_id

        invert_got = ident.invert(key, tessera_id)
        assert invert_got == (shard_id, entity_id)


def test_secondary_key_disagrees_with_canonical_key(vectors, canonical_key):
    """A sanity cross-check: the same (shard, entity) under two different keys must produce
    different tessera_ids, or the key isn't being used at all."""
    sk = vectors["secondary_key"]
    key2 = ident.IdentityKey.from_hex(sk["key"])
    assert canonical_key != key2
    assert ident.forward(canonical_key, 0, 0) != ident.forward(key2, 0, 0)


# --- rejected_keys: 6 malformed/degenerate cases -----------------------------------------


def test_rejected_keys(vectors):
    cases = vectors["rejected_keys"]
    assert len(cases) == 6
    for case in cases:
        with pytest.raises(ident.IdentityError):
            ident.IdentityKey.from_hex(case["key"])


def test_rejected_key_reasons_are_distinct_all_zero_vs_k1_zero(vectors):
    """§1.3: the all-zero key and a merely-k1-zero key are both refused, and are named
    separately (the error should say which one an operator hit)."""
    cases = {c["key"]: c["reason"] for c in vectors["rejected_keys"]}
    all_zero = "00000000000000000000000000000000"[:32]
    assert all(ch == "0" for ch in all_zero)
    with pytest.raises(ident.IdentityError, match="all-zero"):
        ident.IdentityKey.from_hex("0" * 32)
    with pytest.raises(ident.IdentityError, match="k1 == 0"):
        ident.IdentityKey.from_hex("0f0e0d0c0b0a09080000000000000000")


def test_trailing_newline_is_rejected_not_normalised(vectors):
    """Finding 2 (task-5 review): `$` matches immediately before a trailing `\\n`, and
    `bytes.fromhex` then silently tolerates ASCII whitespace, so the old
    `^[0-9a-f]{32}$` regex accepted a key with a trailing newline as the canonical key --
    exactly the shape a key read from `--id-key-file` arrives in. Memo §1.2's rule is
    reject, not normalise; `\\Z` (not `$`) is what makes that true."""
    canonical = "000102030405060708090a0b0c0d0e0f"
    assert ident.IdentityKey.from_hex(canonical) is not None  # sanity: valid on its own
    with pytest.raises(ident.IdentityError):
        ident.IdentityKey.from_hex(canonical + "\n")


def test_direct_construction_cannot_bypass_degenerate_key_rejection():
    """Finding 3 (task-5 review): `IdentityKey` used to be a bare `@dataclass(frozen=True)`
    with public `k0`/`k1`, so the generated `__init__` skipped `_validated`'s check
    entirely and `IdentityKey(k0=0, k1=0)` succeeded. `__post_init__` is now the one gate
    every construction path -- `from_hex` included -- runs through."""
    with pytest.raises(ident.IdentityError):
        ident.IdentityKey(k0=0, k1=0)
    with pytest.raises(ident.IdentityError):
        ident.IdentityKey(k0=123, k1=0)


def test_rejection_message_never_contains_the_key_text(vectors):
    """Finding 4 (task-5 review): interpolating `{key_hex!r}` into the error put a
    byte-for-byte valid key into a log line the moment the only defect was uppercase.
    The message must name the length and the offending character/position instead."""
    upper = "000102030405060708090A0B0C0D0E0F"
    with pytest.raises(ident.IdentityError) as excinfo:
        ident.IdentityKey.from_hex(upper)
    message = str(excinfo.value)
    assert upper not in message
    assert upper.lower() not in message
    assert "position" in message

    too_short = "000102030405060708090a0b0c0d0e"
    with pytest.raises(ident.IdentityError) as excinfo:
        ident.IdentityKey.from_hex(too_short)
    message = str(excinfo.value)
    assert too_short not in message
    assert "length" in message


def test_debug_repr_does_not_print_key_material(canonical_key):
    """Finding 1 (task-5 review), mirroring Rust's `debug_does_not_print_key_material`
    (`identity.rs:469`): `IdentityKey` was a public frozen dataclass with no `repr=False`,
    so the generated `__repr__` printed `k0`/`k1` verbatim -- any pytest assertion dump,
    `%r` log line or traceback touching a key leaked it."""
    printed = repr(canonical_key)
    assert "redacted" in printed
    assert format(canonical_key.k0, "x") not in printed
    assert format(canonical_key.k1, "x") not in printed


# --- Round-trip, over and above the vectors ----------------------------------------------


def test_round_trip_dense_range(canonical_key):
    for entity_id in range(0, 2000):
        tid = ident.forward(canonical_key, 0, entity_id)
        assert ident.invert(canonical_key, tid) == (0, entity_id)


def test_round_trip_random(canonical_key):
    import random

    rng = random.Random(42)
    for _ in range(2000):
        shard_id = rng.randrange(0, 2**32)
        entity_id = rng.randrange(0, 2**32)
        tid = ident.forward(canonical_key, shard_id, entity_id)
        assert ident.invert(canonical_key, tid) == (shard_id, entity_id)


def test_forward_rejects_entity_above_u32_max(canonical_key):
    """§1.8: forward's input is a checked conversion, not a truncating cast."""
    with pytest.raises(ident.IdentityError):
        ident.forward(canonical_key, 0, 2**32)


def test_priority_is_high_16_bits_of_tessera_id(vectors, canonical_key):
    case = vectors["vectors"][0]
    tid = _u(case["tessera_id"])
    assert ident.priority_of(tid) == (tid >> 48) & 0xFFFF


def test_no_collisions_over_a_dense_range(canonical_key):
    """Mirrors memo §1.9's empirical check at a scale suitable for a unit test (not
    16,777,216 entities): the bijection must not collide over a dense range."""
    seen = set()
    for entity_id in range(0, 20000):
        tid = ident.forward(canonical_key, 0, entity_id)
        assert tid not in seen
        seen.add(tid)


# --- Row-order re-derivation, on synthetic data ------------------------------------------
#
# No post-r6 fixture bundle exists in this checkout to test `Bundle.derive_row_order`
# end-to-end against (tessera-build/tessera-store have not yet been repointed at the
# tessera_id column or the identity object -- only tessera-types/identity.rs is landing, as
# of this task). These tests exercise the same logic -- `_entity_of_rows`'s permutation
# inversion and the `(morton, tessera_id)` sort -- against synthetic data built in-process,
# which is available without rebuilding anything.


def test_entity_of_rows_inverts_permutation_for_touched_rows(canonical_key):
    from oracle.bundle import PERMUTATION_ABSENT, Permutation, _entity_of_rows

    # entity -> row: entity 0 has no row (absent), entities 1..5 map to rows 4,3,2,1,0.
    slots = [PERMUTATION_ABSENT, 4, 3, 2, 1, 0]
    perm = Permutation(bound=len(slots), slots=__import__("numpy").array(slots, dtype="<u4"))

    rows = __import__("numpy").array([0, 1, 2, 3, 4], dtype=__import__("numpy").uint32)
    got = _entity_of_rows(perm, rows)
    assert got == {4: 1, 3: 2, 2: 3, 1: 4, 0: 5}


def test_row_order_is_morton_then_tessera_id_ascending(canonical_key):
    """The post-fold storage sort order (`docs/design-memos/2026-07-30-priority-as-identity-
    prefix.md`, "The decision"): `(morton, tessera_id)` ascending, no further tiebreak.
    `tessera_id` is already globally unique, so this needs no third key.

    Finding 5 (task-5 review): the shipped re-derivation -- `bundle.row_order_from_geometry`,
    which `Bundle.derive_row_order` also calls -- is exercised here directly, computing
    `tessera_id` from the permutation-derived `entity_id` via `forward` and `morton` from
    `(x, y)` via `morton_of`, rather than re-implementing the lexsort inline against
    pre-picked morton codes. Swapping `row_order_from_geometry`'s two `np.lexsort`
    arguments now breaks this test directly, instead of the test silently re-deriving the
    same (possibly also swapped) order alongside it."""
    import numpy as np

    from oracle.bundle import row_order_from_geometry

    entities = list(range(50))
    shard_id = 0
    extent = (0.0, 100.0, 0.0, 100.0)

    # A handful of distinct Morton codes with deliberate repeats, so the tiebreak is
    # exercised -- built from real (x, y) geometry, not by pre-picking morton codes
    # directly, so this exercises `morton_of` too rather than assuming its output.
    def xy_for_cell(cell: int) -> tuple[float, float]:
        frac = (cell + 0.5) / 65536.0
        return frac * 100.0, 0.0

    xs = []
    ys = []
    for i in range(len(entities)):
        x, y = xy_for_cell(i % 7)
        xs.append(x)
        ys.append(y)
    x_arr = np.array(xs, dtype=np.float32)
    y_arr = np.array(ys, dtype=np.float32)
    entity_arr = np.array(entities, dtype=np.uint64)

    morton_codes = [morton.morton_of(x, y, extent) for x, y in zip(xs, ys)]
    tessera_ids = [ident.forward(canonical_key, shard_id, e) for e in entities]

    # Reference: Python's stable sort by the exact tuple the memo specifies, computed
    # independently of `row_order_from_geometry`.
    expected_order = sorted(
        range(len(entities)), key=lambda i: (morton_codes[i], tessera_ids[i])
    )

    got_order = row_order_from_geometry(canonical_key, shard_id, entity_arr, x_arr, y_arr, extent)
    assert list(got_order) == expected_order

    # And re-sorting entirely by tessera_id alone, once morton codes are fixed, agrees with
    # sorting by (morton, priority, tessera_id-fallthrough) -- i.e. priority is genuinely a
    # prefix and does not change the outcome once ties are broken by the full id.
    priorities = [ident.priority_of(t) for t in tessera_ids]
    expected_with_priority = sorted(
        range(len(entities)),
        key=lambda i: (morton_codes[i], priorities[i], tessera_ids[i]),
    )
    assert expected_with_priority == expected_order
