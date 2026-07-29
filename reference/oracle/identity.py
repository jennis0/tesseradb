"""The `tessera_id` keyed bijection — Reference Sheet independent re-derivation.

Written from `docs/design-memos/2026-07-30-tessera-id-construction.md` §1 alone, **without
reading `crates/tessera-types/src/identity.rs`**. The oracle's independence from the Rust
implementation is the only thing that makes their agreement over
`reference/vectors/tessera_id.json` evidence of anything (memo, "Why this document exists at
all"): if this module were a port, an agreement with the Rust would prove only that
copy-paste works.

`tessera_id = FPE_k(shard_id: u32 || entity_id: u32) -> u64`: a balanced Feistel network, 8
rounds, 32-bit halves, keyed by a 128-bit per-deployment key. It is a **blinding permutation**,
not encryption (memo, "Vocabulary, deliberately") — `splitmix64` is not a cryptographic PRF,
and the defended property is narrower than confidentiality (memo §3, §8).

`priority` is the high 16 bits of `tessera_id` — a *prefix* of the identity, not an
independent function (the 2026-07-30 fold recorded in
`docs/design-memos/2026-07-30-priority-as-identity-prefix.md` and landed in the plan at
commit `199a6b3`). The construction memo's own §6 still describes the pre-fold
`(morton, priority, entity_id)` tiebreak and an unkeyed `splitmix64(entity_id)` priority;
both are superseded by the fold for any bundle built after it, which is the only kind this
oracle now reads. Storage sort order is therefore `(morton, tessera_id)` ascending, with no
further tiebreak needed -- `tessera_id` is already unique.

Mind Python's unbounded integers: every intermediate the memo specifies as 64-bit is masked
with `& MASK64` after each add/xor/multiply, and every 32-bit half with `& MASK32` --
otherwise this silently diverges from the vectors on the first overflow, exactly as the memo
warns.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

MASK64 = 0xFFFF_FFFF_FFFF_FFFF
MASK32 = 0xFFFF_FFFF

ROUNDS = 8

_HEX_KEY_RE = re.compile(r"^[0-9a-f]{32}\Z")


class IdentityError(ValueError):
    """A typed error naming the reason a key or identity value was rejected (memo §1.2, §1.3,
    §1.8)."""


def splitmix64(x: int) -> int:
    """The round function's mixer -- contracts §2.6's `splitmix64`, memo §1.5.

    Shift amounts are 30, 27, 31 in that order; all arithmetic wraps at 64 bits.
    """
    x &= MASK64
    z = (x + 0x9E3779B97F4A7C15) & MASK64
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK64
    z = z ^ (z >> 31)
    return z & MASK64


def _describe_hex_rejection(key_hex: object) -> str:
    """Build a rejection message that names the length and the offending character/
    position, never the key text itself (finding 4: the key must appear in no log line,
    memo §3.2 -- a rejected key is still key material, and interpolating `{key_hex!r}`
    puts a byte-for-byte valid key into a log line the moment the only thing wrong with
    it is uppercase)."""
    if not isinstance(key_hex, str):
        return f"identity key must be a string; got {type(key_hex).__name__}"
    length = len(key_hex)
    if length != 32:
        return (
            "identity key must be exactly 32 lowercase hex characters (0-9a-f); got "
            f"length {length}"
        )
    for position, ch in enumerate(key_hex):
        if ch not in "0123456789abcdef":
            return (
                "identity key must be exactly 32 lowercase hex characters (0-9a-f); "
                f"invalid character {ch!r} at position {position}"
            )
    # _HEX_KEY_RE rejected the string but every character is individually a lowercase hex
    # digit and the length is 32 -- e.g. a trailing newline, which `\Z` (not `$`) now
    # catches. The loop above already scanned every character of `key_hex`, so falling
    # through here means the string matched length and character-set but not the anchored
    # regex; report that plainly without repeating the string.
    return "identity key does not match the required 32-lowercase-hex-character form"


def _reject_if_degenerate(k0: int, k1: int) -> None:
    """Degenerate keys are rejected (memo §1.3): `k1 == 0` collapses the schedule to a
    single repeated round key for all eight rounds. The all-zero key is a special case of
    `k1 == 0` and is named separately so the error says which one was hit. This is the one
    gate both `from_hex` and direct construction go through (finding 3) -- there is no
    second, bypassable copy of this check.
    """
    if k0 == 0 and k1 == 0:
        raise IdentityError("degenerate identity key: all-zero key (k1 == 0)")
    if k1 == 0:
        raise IdentityError("degenerate identity key: k1 == 0 (round schedule collapses)")


@dataclass(frozen=True, repr=False)
class IdentityKey:
    """A parsed, validated 128-bit per-deployment key (memo §1.2-§1.4).

    `repr=False` plus the explicit `__repr__` below keep key material out of any pytest
    assertion dump, `%r` log line or traceback that touches a value of this type (finding
    1) -- the generated dataclass `__repr__` would otherwise print `k0`/`k1` verbatim.
    """

    k0: int
    k1: int

    def __post_init__(self) -> None:
        # Runs for EVERY construction path, including `IdentityKey(k0=0, k1=0)` called
        # directly -- there is exactly one gate (finding 3), not one gate plus a
        # convention that `from_hex` is the only entry point.
        _reject_if_degenerate(self.k0, self.k1)

    def __repr__(self) -> str:
        return "IdentityKey(<redacted>)"

    @staticmethod
    def from_hex(key_hex: str) -> "IdentityKey":
        """Parse the MANIFEST/config-file key encoding.

        The hex-case rule is reject, not normalise (memo §1.2): exactly 32 lowercase
        `0-9a-f` characters, no `0x` prefix, no whitespace, no separator -- and, since
        `_HEX_KEY_RE` is anchored with `\\Z` rather than `$`, no trailing newline either
        (`$` matches immediately before a trailing `\\n`, which `bytes.fromhex` then
        silently tolerates -- exactly the shape a key read from `--id-key-file` arrives
        in). Uppercase is rejected outright, not case-folded. `k0`/`k1` are then read
        little-endian **over the decoded bytes**, not the text: `key_bytes[0]` is the
        first two characters of the string, `k0` covers bytes 0..8, `k1` covers bytes
        8..16.
        """
        if not isinstance(key_hex, str) or not _HEX_KEY_RE.match(key_hex):
            raise IdentityError(_describe_hex_rejection(key_hex))
        key_bytes = bytes.fromhex(key_hex)
        if len(key_bytes) != 16:
            raise IdentityError(f"identity key must decode to 16 bytes; got {len(key_bytes)}")
        k0 = int.from_bytes(key_bytes[0:8], byteorder="little", signed=False)
        k1 = int.from_bytes(key_bytes[8:16], byteorder="little", signed=False)
        return IdentityKey._validated(k0, k1)

    @staticmethod
    def _validated(k0: int, k1: int) -> "IdentityKey":
        """Mask to 64 bits and construct. Degenerate rejection happens in
        `__post_init__`, not here -- so it applies uniformly whether the key came from
        `from_hex` or was built directly."""
        k0 &= MASK64
        k1 &= MASK64
        return IdentityKey(k0=k0, k1=k1)

    def round_key(self, i: int) -> int:
        """round_key(i) = splitmix64(k0 ^ (k1 *64 (i + 1))) for i = 0..7 (memo §1.4)."""
        multiplier = (i + 1) & MASK64
        mixed = (self.k1 * multiplier) & MASK64
        return splitmix64(self.k0 ^ mixed)

    def round_keys(self) -> list[int]:
        return [self.round_key(i) for i in range(ROUNDS)]


def _round_function(key: IdentityKey, i: int, r: int) -> int:
    """F(i, r: u32) -> u32 = (splitmix64(r as u64 ^ round_key(i)) >> 32) as u32 (memo §1.5).

    `r` is widened to u64 and XORed against the *whole* 64-bit round key before mixing; the
    result's *high* 32 bits are taken -- the low half is a different, still-valid bijection
    that disagrees with every vector in the file.
    """
    r &= MASK32
    mixed = splitmix64(r ^ key.round_key(i))
    return (mixed >> 32) & MASK32


def forward(key: IdentityKey, shard_id: int, entity_id: int) -> int:
    """tessera_id = FPE_k(shard_id || entity_id) -- the forward Feistel rounds (memo §1.6).

    Both halves are u32; `forward` takes a checked entity id (memo §1.8) rather than a
    truncating cast -- an `entity_id` above `u32::MAX` raises rather than silently colliding
    with a different entity. The round index runs 0, 1, ..., 7 ascending, and each round's
    assignment is simultaneous: the new L is the old R, and the new R uses the *old* L and
    the *old* R together (overwriting L before computing the new R is a distinct, wrong
    function that disagrees with the vectors).
    """
    if not (0 <= shard_id <= MASK32):
        raise IdentityError(f"shard_id out of u32 range: {shard_id}")
    if not (0 <= entity_id <= MASK32):
        raise IdentityError(f"entity_id out of u32 range (checked conversion, memo §1.8): {entity_id}")

    left = shard_id & MASK32
    right = entity_id & MASK32
    for i in range(ROUNDS):  # 0, 1, 2, 3, 4, 5, 6, 7 -- ascending, longhand deliberately
        new_left = right
        new_right = (left ^ _round_function(key, i, right)) & MASK32
        left, right = new_left, new_right
    return ((left & MASK32) << 32) | (right & MASK32)


def invert(key: IdentityKey, tessera_id: int) -> tuple[int, int]:
    """(shard_id, entity_id) = FPE_k^-1(tessera_id) -- the inverse Feistel rounds (memo §1.7).

    The round index runs 7, 6, ..., 0 descending -- the same eight rounds in reverse order,
    again longhand deliberately. Inversion is total over 2**64: an arbitrary u64 inverts to
    *some* (shard_id, entity_id), and the caller is responsible for validating the result
    (shard match, entity existence) -- this function never raises for a structurally valid
    u64 input, matching the memo's "every tessera_id inverts".
    """
    if not (0 <= tessera_id <= MASK64):
        raise IdentityError(f"tessera_id out of u64 range: {tessera_id}")

    left = (tessera_id >> 32) & MASK32
    right = tessera_id & MASK32
    for i in reversed(range(ROUNDS)):  # 7, 6, 5, 4, 3, 2, 1, 0 -- descending, longhand
        new_right = left
        new_left = (right ^ _round_function(key, i, left)) & MASK32
        left, right = new_left, new_right
    shard_id = left & MASK32
    entity_id = right & MASK32
    return shard_id, entity_id


def priority_of(tessera_id: int) -> int:
    """priority = high 16 bits of tessera_id -- a *prefix* of the identity, not an
    independent function (the priority-as-identity-prefix fold). Contracts §2.6 post-fold;
    supersedes the pre-fold `morton.priority(entity_id)` (unkeyed splitmix64 over the entity
    id), which remains in `oracle/morton.py` only as a historical artefact of the pre-fold
    format and must not be used for any bundle built after the fold.
    """
    if not (0 <= tessera_id <= MASK64):
        raise IdentityError(f"tessera_id out of u64 range: {tessera_id}")
    return (tessera_id >> 48) & 0xFFFF


def row_sort_key(
    key: IdentityKey, shard_id: int, entity_id: int, morton_code: int
) -> tuple[int, int]:
    """The post-fold row order: `(morton, tessera_id)` ascending, no further tiebreak
    (`docs/design-memos/2026-07-30-priority-as-identity-prefix.md`, "The decision"). Row
    order is therefore key-dependent, where it previously was not -- this is what makes
    `identity.key`/`identity.shard_id` (read from MANIFEST) a dependency of row-order
    re-derivation rather than an artefact of it.
    """
    return (morton_code, forward(key, shard_id, entity_id))
