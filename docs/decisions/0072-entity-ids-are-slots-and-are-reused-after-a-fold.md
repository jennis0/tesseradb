# 0072 — Entity IDs are slots and are reused after a fold; identity moves to `tessera_id`

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The rulings

Four, made together, and each one is what makes the next affordable.

**An entity ID is a slot, not an identity.** **I9**'s never-reuse rule is relaxed: a fold returns
every unused slot to the allocator. The rule it replaces was written on a narrower rationale than
it stated — *"masks and generating sets are sets of entity IDs held in caches with non-zero
lifetime"* — and a fold already **invalidates the term index and every mask fragment**
(`system-architecture.md` §6, decision 0050), so the cache half is discharged by the same event
that frees the slot.

**`tessera_id` becomes `(shard: 12, generation: 20, entity: 32)`.** The permutation's input gains a
discriminator taken from the fold generation current at allocation. The width comes out of the shard
half — 32 bits, reserved and valued 0 while sharding is premature (contracts §2.6) — so **the entity
half keeps its full `u32` and nothing on disk or on the wire changes width**. The Feistel is
untouched: it still splits a 64-bit block into two 32-bit halves, and only the interpretation of
`L₀` changes. Two occupants of one slot are always allocated at different generations, because a
slot is freed only at a fold and the counter is monotone, so **no `tessera_id` is ever reused** even
though every slot is.

**No `bundle_format` bump** *(owner, 2026-08-15)*. Existing copies of the data are deleted rather
than carried (decision 0048), so no stale artifact exists for a version number to refuse — and
independently, **the old encoding is this one with the generation pinned to 0**, so `L₀` is
byte-identical for a bundle that has never folded and there is nothing to misread. The guard a bump
would have supplied lands where it belongs instead: the manifest's generation counter is a
**required** field, so a bundle lacking one is a typed reader error rather than a silently defaulted
zero.

*The split, and why these numbers* **(owner, 2026-08-15)**: 12 bits carries 4 096 shards against a
forward target of ~1 000, so the smallest workable field (11 bits) is not taken without headroom; 20
bits carries 1 048 576 folds, which is ~2 900 years nightly and still ~120 years if a deployment
folds hourly. **The second margin is the one that matters**, because a fold's schedule is a gated
window rather than a timer (decision 0056) and its frequency is not a constant the format should
assume. **The counter is per shard**, not global: entity space is per shard (contracts §1's
4B-per-shard cap), so a slot is `(shard, entity)` and the discriminator only has to separate
occupants within one — which also means shards fold without coordinating. **It is monotone and
wrapping is a refusal, never a silent roll-over**, on the **idset**'s precedent (§10.6: monotone is
load-bearing there for the same reason).

**Validation is whole-identifier equality, and the discriminator is never decoded.** A presented
identifier is valid iff the entity it inverts to *currently carries that exact identifier*: invert
(a pure function), take the slot, read the `tessera_id` stored at its row (contracts §0.3
deviation 6 — `columns.arrow` carries `tessera_id`, not `entity_id`), compare all 64 bits. In the
owner's words: *"no need to actually decode — tessera id remains source of truth for them being
the same."*

**A slot returns to the pool only once every durable structure naming it has been reconciled in
that same fold.** Masks and the term index are handled by the invalidation above. What is not:
node memberships, artifact membership and generating sets, which are entity-space structures that
survive folds by design (`system-architecture.md` §6: *"node memberships and generating sets are
untouched"*).

## Why the discriminator carries no meaning

It is **a discriminator, not a timestamp**, and calling it one invites a reader to derive meaning
from it. Nothing decodes it: the read path compares whole identifiers, and the field exists only
so that two occupants of a slot differ. Three consequences follow, and the third is the one worth
keeping.

- **No per-entity generation array.** The obvious implementation — store each slot's generation
  and compare against the presented one — costs 4 B/entity, 4 GB at 10⁹, an array the size of
  `permutation.bin`. The equality check needs none of it, because the authoritative identifier is
  already stored at the row the request resolves anyway.
- **No plausibility check on the field.** `/control/changes`' admission currently range-checks the
  shard half against the manifest's `shard_id` (write-path §5.1). Repurposing those bits does not
  turn that into a generation check: the equality test subsumes it, and an identifier that inverts
  to an implausible generation simply fails to match.
- **Allocation is the only place the generation is read**, and it needs no per-slot state either:
  the allocator stamps the current fold generation, which is one monotone counter.

## What a stale identifier now does

Before this ruling, a client holding a `tessera_id` for a deleted item got a 404 (decision 0047's
own cost note). **That is preserved and is the whole point of the discriminator.** Under slot
reuse without one, the same identifier would have inverted to a live slot and silently named a
different item — a clean failure becoming a wrong answer, which is the failure mode §10.6 already
requires the **idset** to signal for repartitioning.

The two signals compose and answer different questions. **The idset** is global and coarse: the
identifier space moved, re-resolve everything. **The discriminator** is per-slot and exact: this
identifier named something that no longer exists. Neither replaces the other.

**A recycled identifier and a deleted one must be answered identically.** Distinguishing them
would tell a holder that the corpus recycled a slot, which is a fact about churn rather than about
anything they hold.

## The boundaries, stated because each is one slip from a hole

- **Durable entity-space structures hold slots, not identities**, so a recycled slot makes a
  generating set name the wrong document — the fail-open [`annotation-write-cycle.md`](../design/annotation-write-cycle.md)
  §2 exists to close, arriving from the other side. The fold's reconciliation is what forbids it,
  and the fold **already computes the list**: one `and_cardinality(G, D₀)` per generating set
  against the tombstone clone it holds in entity space (that document's §4.2). It must now *act*
  on that list rather than only report it.
- **A buffered slot has no row, so the equality check has no stored identifier to compare
  against.** The check must consult the buffer for entities `row_of` cannot answer, or it will
  refuse identifiers the ingest ack itself issued (write-path §5.1 returns them). Refusing them is
  fail-closed and wrong.
- **A deleted slot before its fold still holds its row and its original identifier**, so the
  equality check *passes*. That is correct and not a hole: the deny is enforced by `verdict`,
  checked live and first, and the two gates answer different questions.
- **The blinding is not strengthened in kind.** A live generation range widens the plaintext prior
  the §4 **I10** threat model names as its weakness — *"`shard_id` is always 0 and entity IDs are
  dense from zero"* — which is a real improvement to a stated weakness and **not** a move from
  blinding to encryption. Eight rounds of `splitmix64` remain non-cryptographic; the wording of
  I10's mechanical half stands unchanged.
- **`priority` is unaffected.** It is the leading 16 bits of the Feistel *output*
  (contracts §2.6), whose distribution does not depend on the input's field layout.
- **Identifier stability across rebuilds is unaffected.** A slot's generation is fixed at
  allocation and never revised, so carrying `identity.key` forward still reproduces every
  identifier exactly.

## The one contract this changes: verification inverts rather than reproduces

**The conformance oracle must today reproduce the stored `tessera_id` column byte-for-byte from
`(identity.key, identity.shard_id, entity_id)`, and `tessera verify` checks the whole column against
it** (contracts §2.6). A discriminator is a fourth input, and the oracle cannot supply it from
nothing.

**Within one allocation batch it is free**, because every entity allocated between two folds carries
the same generation — so it is a per-segment constant recorded beside the segment, not a per-entity
quantity. **The base segment after a fold is where that breaks**: it holds entities allocated across
many generations, and slot reuse scatters them, so no range table recovers it. Two ways out, and the
choice is not mine:

- ~~Store the generation per entity~~ — 4 B/entity, 4 GB at 10⁹, an array the size of
  `permutation.bin`, on the build and verify side only, whose sole reader would be the check itself.
  **Declined.**
- ✔ **Verification inverts instead of reproducing** *(owner ruling, 2026-08-15)*. `tessera verify`
  asserts that **every stored identifier inverts, under the deployment key, to the entity holding
  the row it sits at**. Still a total check over the whole column, still catching corruption, a
  mis-permutation and a wrong key — and it needs no fourth input, because inversion recovers the
  generation rather than requiring it.

**What the weakening gives up, stated exactly:** a systematically wrong generation is no longer
detectable. Nothing is lost by that, because there is nothing there to detect — the discriminator
has no external truth to be checked against. Its only requirement is that two occupants of one slot
differ, and monotone per-shard allocation delivers that at the allocator, without the column
attesting to anything. **The property that was actually load-bearing survives intact**: the column
is a bijection over entity space under the declared key, which is what I10's structural half and the
oracle's differential both rest on.

This narrows a contract a second reader depends on — the class CLAUDE.md holds out of the
pre-release demolition licence — so it is ruled explicitly rather than absorbed.

## What this does not settle

- **The reconciliation pass's cost** — the fold gains work proportional to the structures naming
  freed slots, and it is inside a pre-flight budget that refuses the fold when it exceeds memory
  (compaction §3). Unmeasured.
- **Whether the free pool must be handed out in runs.** Nothing above requires it; it was proposed
  when validation was assumed to need a derivable generation, and the equality check removed that
  need. Recorded so it is not reintroduced as a constraint without a reason.
- **The exhaustion answer contracts §1 defers to "§16"** is narrowed rather than closed: slots are
  now recoverable, so the `u32` space bounds *live* entities rather than cumulative allocations.
  Whether 2³² live entities is itself the right ceiling is untouched.

## Why the invariant was relaxed rather than the space widened

Widening past `u32` leaves the 32-bit Roaring substrate every measurement in the performance
campaign was taken on, and the arrays are `u32` in every fixed-width on-disk structure
(contracts §1). Against that, never-reuse was not free either: **an edit is delete plus re-ingest
(decision 0047), so every edit burns a slot**, and a 10⁹ corpus at 1% daily churn spends the whole
space in under a year. The invariant was not protecting against a stupid error; it was deferring a
bill, and the fold is where it can be paid.
