# 0077 — Artifact supplied content lives in the record blob

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The decision

**An artifact's supplied content — label text, descriptions, authored names — lives in the record
blob, the same store points use, addressed at the artifact's own entity.** Not a second store
beside it, and not the level's own file.

**How that content is edited is deferred to its own design pass**
*(owner, 2026-08-15)* and does not constrain this: publishing and republishing artifacts needs no
edit route, so nothing before Stage 7 waits on it.

This answers the storage half of ruling 4 of
[the Stage 0 review](../evidence/memos/2026-08-15-artifact-design-review.md).

## Why it fits, verified rather than assumed

The blob addresses by **rank in its own has-row bitmap** — the bitmap of entities that carry a
record — indexing a compacted offset array, with a block directory locating the block by binary
search (`tessera-filter::record`). **That bitmap is the blob's own and has nothing to do with row
space.** An entity that carries no record is absent from it and *"occupies nothing anywhere"*.

So an artifact having no row in row space — the fact that shapes almost everything else about the
artifact population — is simply irrelevant here. It carries a blob record or it does not. This is
the one piece of the withdrawn reuse claim that survived review intact, and it is why artifacts
carry entity IDs at all.

**Artifacts get their own record extents by construction, not by choice.** An extent holds the blob
rows of the entities that produced it, and layers are disjoint in entity space under I9; artifacts
are created by a build-plane publish rather than by a flush, so their records land in extents of
their own. That is also what will keep a future republish artifact-local rather than rewriting a
file the point path depends on.

## The guard that must travel with it

**Living in the blob is a storage fact and carries no authorisation consequence.**
[`annotation-representation.md`](../design/annotation-representation.md) §2.4 describes the blob as
*"entity-addressed and has no `M_auth` involvement"*, which is true of the store and reads, wrongly,
as though no mask governs the content in it. **The blob is not a permission structure.** Containment
is evaluated by the serving route against the composed mask, every request, cached nowhere; the
store's job is to hold bytes and hand them back. A reader who takes that sentence as licence to serve
what the blob returns has skipped the only test that governs supplied content.

The corollary is the trap recorded at
[`annotations.md`](../design/annotations.md) §2.3 and worth repeating here, because it is where an
implementer will meet this decision: an edit written as an **additional record layer** serves the
**pre-edit text, silently**. The stack takes the first matching layer and rests on layers being
disjoint, so two layers naming one entity is outside its model. Whatever the edit design chooses, it
is not that.

## What it buys, and what it declines

One store rather than two: the compression, the block directory, the fail-closed reader checks and
drill-down's one-block-read assembly all come for free, and there is no second set of format
invariants to keep aligned with the first.

Declined: the level's own file, which would have kept every artifact concern inside the artifact
machinery. It is the better answer only if artifact content turns out to need a write pattern the
blob cannot express — and the edit pass, when it runs, is where that would surface. If it does, this
decision is the one to revisit, and the cost of having chosen the blob first is a migration of a
store nothing yet writes.
