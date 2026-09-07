# 0134 — Anything a build can create, live ingest can create, at any scale

**Date:** 2026-09-07 · **Status:** Settled (owner ruling) · ⊘ not yet built; the design pass is
[`ingest.md`](../design/ingest.md), normative 2026-09-07, its rulings decision 0136 (`docs/evidence/memos/2026-09-07-ingest-at-any-scale-review.md`
is the inventory it starts from)

## What this answers

The measurement campaign declined something at ingest on every rung — a layer whose column the
wire read another way (until decision 0125), artifacts over the 64 MiB publish body (3 of 256
clusters on rung 5, 11 of 4,798 topics on rung 4, 45 of 30,217 MeSH descriptors on rung 3,
measured 2026-09-05), memberships over a driver cap — and each decline was lawful, because
decision 0091 rules *expressibility* and carves out "cost and acquisition" and "scheduling and
packing". The review of 2026-09-07 found the residue: an artifact's member list has no ceiling
since decision 0127, but its content, generating set, parents, shape and attachment travel only on
one publish body; the attribute schema, declared vocabularies, view groups, membership by
exclusion and a group-scoped layer's per-view artifact sets have no ingest route at all; and no
document states a rule that would make any of that a defect.

## The decision

**Anything a user can create in a build can be created, and extended, through live ingest — every
kind of data, at any scale.** Points, attribute values, artifacts and every part of one, layers,
views and their groups, vocabularies, the attribute schema. Size is never a reason to decline: a
cap on a request body is lawful only as a **pagination unit** the client can discover and the
server assembles; as a **ceiling** on what can be said it is a defect. Decision 0091's carve-out
for cost, acquisition and packing is withdrawn to that extent — the *route* may differ between the
two entry points, the *reach* may not.

**Invariant I8 stands and is about identity, not size.** A generating set cannot change after the
content derived from it exists; that says nothing about how large a set may be supplied, and a
publication assembled over several requests and committed once satisfies it at any scale.

## Why

The rules the declines rested on were not requirements. They were engineering facts about a
buffered body and an unbounded connection count, written down as if they bounded what the system
can hold. The requirement is the one the project states first: one corpus, read the same way at
both doors, at billions of rows. A memory bound binds the wire and is met by streaming or staging;
it does not bind what a caller may say.

## Consequences

- A design pass over ingest under this rule, before any track: how every kind is ingested, how
  performantly, and what the client-facing shape should be — whether multi-part uploads are the
  path of least surprise or something simpler is, and what comparable systems do. Its draft goes
  to `docs/design/` marked provisional and through `docs/agents/design-process.md`.
- Every cap on the ingest path is reclassified as a pagination unit or removed; `PUBLISH_MAX_BODY_BYTES`
  becomes a published, configurable value on `/v1/meta`'s terms, as the other shape caps are.
- The no-route kinds get routes. The review's nine document contradictions are corrected as the
  design lands, `write-path.md`'s silence on artifacts first.
- Decision 0091's text gains a line pointing here; decision 0127's "Open" on very large
  memberships is answered by this rule and the design that follows.
