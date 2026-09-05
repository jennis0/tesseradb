# 0126 — The generation counts a slot's occupancies, and a freed slot is reused before a shard opens

**Date:** 2026-09-05 · **Status:** Settled (owner ruling)

## What this answers

Two limits in [sharding.md](../design/sharding.md) as first ruled. The identity's shard field was
12 bits, so a corpus could hold 4,096 shards: about 4.4T live rows at a seal size of 2³⁰. The
generation field was 20 bits and counted a shard's compactions, monotone, with exhaustion a refusal
([decision 0072](0072-entity-ids-are-slots-and-are-reused-after-a-fold.md)); at hourly compaction a
shard stopped accepting writes after about 120 years. Sharding ruling C, sealed by default, sent
every allocation that replaced a deleted item to the open shard, so shard numbers were consumed by
churn as well as by growth: at 10¹² rows and 1% daily churn, about 3,400 a year, and the 12-bit
field lasted about 1.2 years.

The owner asked whether the generation could be narrowed to widen the shard field, and whether its
exhaustion could be handled so that writes run indefinitely.

## The decision

Three rulings, made together.

**The generation is the slot's occupancy count.** Each shard's allocator holds a free pool: the
slots its compactions have freed, bucketed by the generation their next occupant is stamped with. A
compaction inverts the identifier stored at each row it removes, reads the generation, and returns
the slot to the bucket one above. A slot whose generation is the field's maximum is retired at that
compaction and is never issued again. Allocation from the pool stamps the bucket's generation;
allocation above the high water stamps 0. No shard-level counter exists.

**A freed slot is reused before a shard opens.** The allocation target for a kind is the shard with
the largest pool, if that pool holds at least `shard.reuse_min_free` slots; otherwise the open
shard, from its own pool and then its high water. A shard opens when the open shard's high water is
at the seal size and no pool meets the threshold. The reopened state and the reopen verb go: a
sealed shard issues slots from its pool without an operator's action.

**The identity input is `(shard: 20, generation: 12, entity: 32)`.** The Feistel is unchanged; `L₀`
is the shard number shifted over the generation.

⊘ Not built. The allocator in the tree is monotone with no pool, and the identity's `L₀` is a `u32`
shard number valued 0. Decision 0072's split was never written either, so this replaces a
specification and no code.

## Why an occupancy count

The generation exists so that two occupants of one slot carry different identifiers, and a stale
identifier answers 404 rather than naming the new occupant. Nothing else reads it (decision 0072).
Counting a shard's compactions does that at a cost: every compaction consumes a value whether or
not it freed a slot, and a sealed shard taking only deletes consumes them at its compaction rate.
Counting occupancies consumes a value only when a slot is reused, and the count is per slot, so
exhaustion is a property of one slot's history and not of the shard's age.

Retirement makes exhaustion cost a slot. A slot delivers 2¹² occupancies and is then lost, so the
shard's capacity falls by one slot per 4,096 reuses. Two alternatives were declined. Wrapping the
count is tolerable under contracts §2.2, which makes `tessera_id` a transport identifier valid for
one idset, but it reopens the case the discriminator closes for a client that ignores that
contract, and retirement removes the case for nothing. Advancing the idset at the cap invalidates
every identifier and session in the deployment to save one slot.

The compaction reading a generation is the one exception to decision 0072's rule that nothing
decodes the field. It is still not a plausibility check and still not a timestamp: the value is
read from the row being removed and used to place the slot, and no request path reads it. The
per-entity generation array decision 0072 declined is still declined; the pool is per free slot,
held in bitmap containers, and a never-freed slot has no entry.

## Why reuse before grow

A replacement has no affinity to a shard, and a shard whose items are being deleted is already
being compacted, so its freed slots are the cheapest place for the next allocation. Sending that
allocation to the open shard instead opened a shard per seal size of churn, and each shard costs
every session a fragment leaf and a projection, every tile a fan-out, and every level a set of
artifact structures. Under this ruling the shard count is the live count over the seal size,
whatever the churn, and at a steady live size no shard opens.

What sealed by default bought was a byte-stable closing form for a sealed shard. A shard nobody
deletes from has no pool and stays stable under this ruling. A shard taking deletes was being
compacted in either case.

What reuse costs is locality. A slot from the pool sits wherever its previous occupant did, so
postings over reused slots hold fewer runs than postings over a contiguous allocation at the high
water. Draining the pool in slot order against a signature-sorted commit window hands out runs of
freed slots as runs, and a source's items are usually freed together. Decision 0072 accepts this
cost for reuse inside the open shard; this ruling applies it in every shard, and the bound is the
same, since bitmap cost is by containers touched. Not measured.

## Why 20 and 12

Once churn consumes no shard numbers, the shard field bounds live capacity and nothing else, so it
is the scarcer resource. Twelve generation bits give a slot 4,096 occupancies. Modelled as 2ᵏ times
the corpus turnover period, with the pool drained lowest bucket first so the population's counts
rise together:

| Generation bits | 1% daily churn | 10% daily churn |
|---|---|---|
| 8 | 70 years | 7 years |
| 12 | about 1,100 years | about 110 years |
| 16 | about 18,000 years | about 1,800 years |

Live capacity at a 2³⁰ seal size: 4.4T rows at 12 shard bits, 70T at 16, 2⁵⁰ (about 1,100T) at 20.
Twenty shard bits and twelve generation bits leave both margins past any deployment; sixteen and
sixteen spends generation bits nothing needs.

The blinding prior narrows. Decision 0072 counted a live generation range as an improvement to the
weakness I10's threat model names; under an occupancy count most slots carry 0 to 3. No claim rests
on that improvement ([decision 0014](0014-i10-weakened-to-construction.md)).

## What each scale looks like

| Corpus | Shards | Shards opened per year |
|---|---|---|
| 10⁷ rows, daily ingest | one point shard, one artifact shard | none |
| 10¹⁰ rows, hourly ingest | live over the seal size, about 10 at 2³⁰ | growth only |
| 10¹² rows and up | live over the seal size | growth only |

The artifact shard gains the same. Sharding ruling E rested a second artifact shard on artifact
churn alone consuming a `u32` in about 14 months; with reuse a deleted artifact's slot returns at
the artifact shard's compaction, so a second artifact shard opens only when live artifacts exceed
the entity ceiling. Ruling E's mechanism stays.

## What this supersedes in decision 0072

- The per-shard, monotone compaction counter, and "wrapping is a refusal, never a silent
  roll-over". Replaced by the occupancy count and retirement.
- "Nothing decodes it." One exception, at compaction, above.
- "Allocation ... needs no per-slot state either." The allocator holds the pool.
- Sharding ruling C's amendment, "slot return applies within the open shard". Slot return applies
  in every shard.
- The open question "whether the free pool must be handed out in runs" stays open; draining in slot
  order is the construction, and its gain is not measured.

Everything else in decision 0072 stands: slots are reused, validation is whole-identifier
equality, the buffered-slot and deleted-slot boundaries, the reconciliation order, and
verification by inversion.

## What this changes elsewhere

[sharding.md](../design/sharding.md) §1.1 (states and the artifact shard), §1.2 (widths), §1.3
(the manifest and side-manifest fields), §3.1 (the pool, the stamp, the target rule, retirement),
§3.2, §3.4, §3.5 (seal and drop, with no reopen), §5, §6 (fixtures (c) and (i)), §8, §10 and §11.
Contracts §2.6 takes the widths at stage S1, as sharding §12 lists. The `shards[]` manifest record
loses `entity_id_low_water` and `compaction_generation`; the per-shard side manifest gains the
pool, one bitmap extent per bucket, and the retired count. `WalRow` gains `generation` beside
`shard`.
