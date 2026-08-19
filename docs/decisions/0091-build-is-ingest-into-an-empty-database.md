# 0091 — Build is ingest into an empty database

**Date:** 2026-08-20 · **Status:** Settled (owner ruling)

## The decision

**There is no difference in functionality or user experience between a build and an ingest.** A
build is a more efficient form of ingesting into an empty database, and nothing more. Anything a
caller can express at one entry point they can express at the other, and the result is the same
database.

What may differ is *cost and acquisition*: a build reads files, batches, sorts and packs in one
pass because it knows the corpus is empty and nothing is being served. Those are properties of how
the work is scheduled, never of what can be said.

**The test is the one already in use.** Two spellings of one input must produce a byte-identical
bundle — the property inline artifacts, sourced artifacts, `excluding`, member tables and point
columns are each held to. This decision extends it across the two entry points: the same corpus
built, or ingested into an empty database, is the same bundle.

## What this rules

**An ingested point enters an enumerated membership.**
[`annotation-write-cycle.md`](../design/annotation-write-cycle.md) §3.4's timing table said
**never** — *"I8; the caller declared the set"* — while
[`artifacts-from-points.md`](../design/artifacts-from-points.md) §5 said it joins. §3.4 is wrong,
and this rule is why: a member table at build enters points into an enumerated membership, so a
build has always done the thing the row says never happens. The two documents were describing one
operation at two entry points and disagreeing about it.

**I8 is not at stake and never was.** It governs a **generating set** — the documents a supplied
description was drawn from — which is a different set from a membership, is never grown at either
entry point, and is unaffected here.

**Membership is service-maintained.** A point carrying an artifact's key joins that artifact's
membership, and the artifact then behaves exactly as though the point had been there all along.
There is no state in which a cluster holds some of its points because of how they arrived.

## What it costs

Growing a membership is machinery that does not exist. Every route into the artifact store writes a
whole record; the two mutating passes only remove. Three pieces are needed, and the third is the
one that decides the shape:

- **A durable record carrying a delta**, not a restated membership: rewriting a 10⁸-member cluster
  is ~12 MB on the fsync path for every batch that names it.
- **A store method that grows a membership** — a second way state enters a structure whose removal
  rules have been conflated twice in review. It is a growth path rather than a removal one, which
  is why it is admissible, but it is written once and shared, not per caller.
- **The packing bookkeeping**, which is where it bites. A level is packed only above its published
  high-water, so a grown record below that mark never reaches a manifest again: durable in the log,
  absent after a restart. That failure is silent and indistinguishable from an artifact failing its
  criterion, which makes it the one part of this that must be got right rather than got working.

**The fold is the natural home.** A membership is projected through base rows, so a point ingested
since the last structural pass contributes to no masked count, hull or criterion however its
membership was recorded. Nothing is observable before the fold, and the fold already rewrites every
level whole. What the interval needs is durability and a pin that holds — not a second packing rule.

## The asymmetries this rule does not yet close

Stated rather than implied, because a rule whose exceptions are unlisted is a rule nobody can
apply:

- **Entity id assignment differs by construction.** A build assigns ids in signature-sorted order
  across the whole corpus; ingest allocates above the high-water. The same corpus built and
  ingested therefore carries different permanent ids, and `tessera_id` derives from them. This is
  the one place the byte-identical test cannot hold, it is deliberate (§11.1), and it is the
  reason `--carry-id-key-from` exists — which itself does not yet carry the term dictionary.
- **Acquisition is build-only and always was.** `source`, `fields`, inline `artifacts`, `--file`,
  `--limit` and extent fitting say where rows come from, not what they mean, and a deployment that
  never builds simply omits them ([`configuration.md`](../design/configuration.md) §2). Not an
  exception to this rule: nothing a caller can *say* differs.
- ⊘ **The wire cannot yet say everything the build can.** A point may name its artifacts in a file
  and not on the wire, which is the gap `artifacts-from-points.md` §6 exists to close and the first
  thing this rule obliges.

## Consequences

- Every future input route is designed at both entry points or at neither. A feature that works at
  build and not at ingest is unfinished, not staged — and the reverse is equally unfinished.
- A build-only refusal is a bug unless it is about acquisition. If a build refuses something an
  ingest accepts, one of them is wrong about what the system means.
- The conformance suite gains a shape it did not have: any input spelling can be driven both ways
  and the two bundles compared.
