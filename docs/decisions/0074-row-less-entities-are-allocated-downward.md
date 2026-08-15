# 0074 — Artifacts keep entity IDs, allocated downward from the top of the space

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The decision

**Artifacts are entities.** They keep entity IDs, so the deny lane, the overlay, `verdict`, the WAL,
both removal rules and `tessera_id` work on them unchanged — which is the whole reason the artifact
design could claim to add no new durability mechanism.

**One space, two regions, growing towards each other.** Points are allocated upward from 0 as they
are today. **Row-less entities — artifacts, and the entity a layer takes so that layer suppression
can ride the deny lane — are allocated downward from `u32::MAX`.** A layer publish takes its run
downward and gets a contiguous block, so an artifact's address stays `entity − entity_base`
arithmetic ([`annotation-representation.md`](../design/annotation-representation.md) §2.3).
Exhaustion is the two marks meeting.

This answers ruling 1 of [the Stage 0 review](../evidence/memos/2026-08-15-artifact-design-review.md).

## Why the region matters, which is not where a reader expects

The hazard the review found is that three structures allocate memory across entity **ranges**
rather than over entity counts:

- a flush or merge segment's row table is **dense over `[entity_lo, entity_hi]`**, resident in the
  served row space and rebuilt at every open (`tessera-store::permutation`);
- a merge allocates one slot per entity across its **whole merged window** (`execute_merge`);
- the fold's `permutation_bound` is the last extent's `entity_hi + 1`, `entity_bound` is the maximum
  of those across slices, and `memory_estimate` charges 4 bytes per entity against a pre-flight
  refusal that **declines the fold** when it exceeds what the host has (`tessera-engine::compact`);
- `ext-locator.u32` is likewise dense over a segment's own span (`tessera-store::flush`).

**Every one of those bounds is derived from a segment extent, never from the highest ID ever
issued** — verified against the code. An artifact has no row, so it appears in no segment, so it
enters none of those spans. **An artifact ID above every point therefore costs nothing anywhere.**

The cost exists only when an artifact run sits **between** two point segments that are later merged:
the merged window is dense across the gap, so a ten-million-wide run is ten million wasted slots in
a resident array and ten million entities' worth of budget in a fold that can refuse for it. Under a
single monotone allocator that interleaving is the *normal* case — a layer published between two
ingest windows — and the waste is permanent, because the bound never comes back down.

**Two regions make the interleaving impossible rather than unlikely.** It is a property of the
allocator, not a rule an operator has to observe.

## The precedent is in this repository

`tessera-lifecycle::buffer` already allocates term extension IDs **downward from `u32::MAX`**, for
the same reason and with the same argument: *"a downward-from-`u32::MAX` extension id can never
collide with"* one allocated upward from the dictionary's length. It even carries a static assertion
that the start sits far above the declared vocabulary bound. This decision is that pattern applied
to a second population.

## What it costs, stated rather than discovered

- **Two durable marks instead of one.** The upward mark is what exists today; the downward mark
  joins it, in the same places — the side manifest refreshed at every flush publication, and the
  allocator's boot seed.
- **Recovery is the part to get right.** `alloc::high_water_from` derives the restart seed from
  `IngestBatch` rows and `OverlaySnapshot` entries and from nothing else, so **an artifact
  allocation raises no mark today**. Left unaddressed, a rotation and restart can re-issue an
  artifact's ID to a point: two entities, one `tessera_id`. Whatever record carries an artifact
  allocation must raise the downward mark, and the mark must survive rotation the way the upward one
  does.
- **Exhaustion changes shape and gets more honest.** Today the allocator refuses at a fixed ceiling
  that knows nothing of a second population; under this it refuses when the marks meet, which is the
  true condition and accounts for both.

## What it does not fix

**The burn rate.** A 10⁷-artifact layer still spends 10⁷ IDs per regeneration; this makes the space
*usable*, not larger. Recovering spent IDs is
[decision 0072](0072-entity-ids-are-slots-and-are-reused-after-a-fold.md), which is **settled and
unbuilt** — the allocator is monotone with no free list and `tessera_id` carries no generation field.

**Its implementation is deliberately scheduled after this and is not a dependency** *(owner,
2026-08-15)*. Artifacts need identity, the deny lane and the opaque identifier; none of that needs
reuse. Deferring it also *removes* a hazard rather than carrying one: the review's fail-open in which
a recycled slot silently joins a stored membership set can only exist once reuse is built. **The
condition attached is that the membership-reconciliation clause ships with it** — a freed slot must
leave every artifact membership naming it, in the same fold that frees it, on the ordering rule 0072
already states for generating sets. Under two regions that reconciliation is also tidier: a freed
artifact slot returns to the artifact region rather than into the middle of the point sequence.

## The alternative, and why not

**Teach the merge and the fold to skip empty ranges.** It works, and it is a change to the hot path
that *produces row space* — the one place in the build where an off-by-one is a corrupted corpus
rather than a slow query — undertaken to solve a problem the allocator can make unrepresentable.
Declined on that asymmetry: prefer the construction that is obviously correct.

## Two consequences worth recording

**The ordinal scheme survives.** A downward allocator still returns a contiguous run, so
`ordinal = entity − entity_base` needs no lookup table, which is what the representation's addressing
rests on.

**Entity IDs stop being dense from zero**, which slightly narrows the plaintext prior that §4's
**I10** threat model names as the blinding's stated weakness — the same small improvement decision
0072 claimed for its generation field. Not load-bearing, and not a change in kind: the permutation
remains non-cryptographic.
