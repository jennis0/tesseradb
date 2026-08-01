# 0014 — I10 is weakened to what the construction defends

**Date:** 2026-08-01 · **Status:** Settled

## Context

Invariant I10 stated that `tessera_id` is "a keyed permutation of `(shard_id, entity_id)`,
**invertible only inside the trust boundary**".

The construction is an 8-round Feistel network whose round function is `splitmix64` — a
**non-cryptographic** mixer. The source calls it "a keyed blinding permutation, not encryption",
and the key is not secret against anyone holding the bundle, who obtains it by construction.

The qualification existed in a source file and a memo. It did not exist in the specification.

## Decision

**Weaken the claim to match the build.** I10 now states what is actually defended:

- a blinding permutation that prevents a viewer-plane client from correlating or enumerating
  entity IDs;
- explicitly **not** a cryptographic guarantee;
- explicitly **not** a defence against a bundle-holder.

The three-sentence threat model is promoted from `crates/tessera-types/src/identity.rs` into the
specification. No code changes.

## Why not strengthen the code instead

Replacing the round function would change a permanent, identity-bearing construction under I9.
Every existing `tessera_id` would change, and the corpus is explicit that identity assignment is
not retrofittable.

More importantly, the viewer-plane property is the one the system actually needs. The stronger
claim was never relied on by anything; it was simply overstated.

## What does not change

I10's structural half is true and load-bearing, and is untouched: no request-path artifact stores
an entity ID, so the gather cannot produce one. That is enforced by
`scripts/check-layers.sh` and by a byte-scanner in both languages.

## Evidence

Register row S6. `crates/tessera-types/src/identity.rs`;
[`../evidence/memos/2026-07-30-tessera-id-construction.md`](../evidence/memos/2026-07-30-tessera-id-construction.md).
