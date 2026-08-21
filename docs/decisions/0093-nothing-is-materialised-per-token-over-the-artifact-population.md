# 0093 — Nothing is materialised per token over the artifact population

**Date:** 2026-08-21 · **Status:** Settled (owner ruling) — **amended the same day**, after the
campaign's adversarial review, with the row-major exception the owner named and with the evidence
corrections that review found ([the record](../evidence/memos/2026-08-21-artifact-serving-scale-review.md)).

## Context

[`architecture.md`](../design/architecture.md) §8.5 specifies a **servable-label set** held per
token: the labels a principal contains, computed once when their mask is materialised and reused for
every request in the session. It is specified and unbuilt, and the scale campaign's first draft
leaned on it — it is the obvious home for the two `O(artifacts)` questions that depend on the mask
but not on the request, containment (`G ⊆ M_auth`) and the masked count.

**There are a great many tokens** (owner, 2026-08-21). A principal is a token, principals do not
share masks, and the target scenario is many concurrent ones over a shared bundle. So a structure
sized by the artifact population is the wrong shape on that cadence however cheap one copy of it is:
at 10⁷ artifacts the per-token route measured 151 ms of setup and ~40 MB of state, per session.

## The decision

**No structure sized by the artifact population is held per token.** Containment is answered from a
**build-time containment partition**: an interned identifier per `(artifact, rank)` naming the boolean
expression over terms that decides that rank, plus one bitmap per distinct expression. It names no
principal, so one copy serves every token that will ever exist.

**One exception, named rather than left to be argued from the general rule** (owner, 2026-08-21): a
**row-major** layer may hold a masked-count histogram per `(session, layer)` — about 4 bytes per
artifact, 4 MB at 10⁶ and 40 MB at 10⁷ — byte-budgeted exactly as the row-projection cache is, and
refreshed on the session-geometry cadence. The general rule stands everywhere else. What makes this
the one place it gives way is that a row-major layer has **no other route** to the quantity the
disclosure rule requires: an artifact-major layer answers `|membership ∩ M_auth|` one artifact at a
time, so a request's budget bounds the work, while a row-major layer has no per-artifact membership to
intersect and its only route is a histogram over the whole layer. Declining the exception would not
save the work; it would pay it on every request instead of once a session.

## Why

**Containment is a question about terms, not about the mask.** An entity is in `M_auth` exactly when
its own visibility expression holds for the principal's terms, so

```text
G ⊆ M_auth   ⟺   ( ⋀ vis(e) for e in G )( T )
```

— a boolean expression over terms with nothing about the mask in it. Compose it once at build,
canonicalise, intern: artifacts sharing an expression share an answer for every principal. This is
what [`annotations.md`](../design/annotations.md) §4 already says the tractability of the test rests
on — *what decides is which terms, never how many items* (**I5**) — applied to storage rather than
to evaluation.

⊘ **How many distinct expressions a real layer produces is unmeasured, and this ruling does not rest
on it.** The claim that the count stays small assumes generating sets drawn from inside one signature
group — which [`annotations.md`](../design/annotations.md) §8.1 shows as a **variant an author may
adopt** against label creep, not as a rule the service imposes. The measurement behind it planted 32
signature groups and drew every set from one, so 32 is what the fixture could produce; the demo corpus
holds **54,791 distinct permission signatures over 2.42M items** (`probes/phase0-memo.md` §2.3). Two
consequences: the expression identifier is **at least a `u16`**, and the distinct-expression count over
a real generating-set population is a queued measurement. What the ruling rests on is the *cadence* —
the structure names no principal — and that is true at any expression count.

**It measures at parity with the per-token route and ahead of it where it matters.** ⊘ *These cells
appear in no committed run log — unrecorded earlier revision, re-measurement queued, and both routes
compared here omit the masked candidacy test the design now requires (scale memo §4).* At 10⁷
artifacts and a full mask the two are within noise across four viewports (140.0 ms against 141.9 at
whole map, 0.221 against 0.186 at a 0.024% viewport) — with the per-token structure's 151 ms of
setup deleted rather than amortised, against a build-time interning that took 0.1 s at 32 expressions.
At a **narrow** principal the build-time route wins outright, **4.09 ms against 13.1** at whole-map
zoom, because the expressions a principal fails are never touched where the per-token pass had to
evaluate every artifact once to find that out.

**It costs ~40 MB of state naming nobody** at the 32-expression fixture — 30 MB of bitmaps and 10 MB
of identifiers — against ~40 MB per live session, and a token then costs what it always did: its mask
fragment, and no artifact structure at all. The storage figure moves with the expression count and the
identifier width; the property that it names nobody does not.

## What this forecloses

The masked count under an **existence criterion** at a wide viewport is genuinely per-principal and
does not dissolve (memo §4.3). One of its three candidate answers was to hold the counts per token
after all — which is now the only thing §8.5's structure would have been for, and **this decision
closes it for an artifact-major layer**; the row-major exception above is the one place it does not.
Two candidates remain, both unbuilt and neither yet chosen:

- **Per-signature counts**, build-time and mask-independent, summed over the satisfied signatures
  per request. Its storage scales with the **signature** count rather than the artifact count, which
  is the thing to measure against a real corpus before building it — the fixture's thirty-two against
  the demo corpus's 54,791.
- **Bounding evaluation to the levels the request can serve from**, which changes what is evaluated
  rather than what is served, and reaches the request contract.

## What this does not rule

**§8.5 is not deleted.** This decision removes the *artifact population* from that cadence; whether
anything else belongs on it is an architecture question and is not answered here. §8.5's own
incomplete cache key stands as a live defect wherever it is implemented (memo §8.3): the specified
key is *(auth-data hash, auth-plugin version, overlay version)* and none of those moves when an
**artifact** does, so a grown generating set — which makes containment *harder* — would keep being
answered permissively from a stale entry. It needs the artifact store's version beside the
overlay's.

## Consequences

- A session's cost is its mask fragment. Adding artifacts to a bundle does not make a token dearer
  to open, which is the property that matters at a large token count.
- The partition is build-time state, keyed `(prefix, view, store_version)` — the same identity
  `ProjectionKey` carries — so it is rebuilt where row forms are rebuilt: in the fold's artifact pass
  and at a store-version bump. It inherits whatever cadence
  [decision 0094](0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md) gives
  the rest of the per-generation structures, and it inherits the invalidation defect with it (memo
  §8.1): any artifact write moves the store version, so the partition is rebuilt for every layer in
  every view.
- ⊘ **What the partition costs to build is unmeasured.** The probe interned 32 expressions in 0.1 s
  over ten million artifacts, which prices the fixture's expression count and not a corpus's.

⊘ **The deny correction is the partition's acceptance test, not a refinement of it.** A **deletion**
and a **suppression** each remove a member of `G` from `M_auth` whatever the terms say, so the answer
must be intersected with *no member denied*, where `denied = deleted ∪ suppressed` — both, and an
earlier drafting of this paragraph named only suppression. It is evaluated **live against the overlay
per request**, or applied synchronously with the deny's acknowledgement: a refresh on any other
cadence is fail-open for the length of its window, and `annotation-write-cycle.md` §3.4 puts
containment's response to a deny at **accept**. An **unsuppress re-derives** rather than subtracts,
because delete → suppress → unsuppress must leave the entity deleted. The structure this needs is an
inverted index from entity to the artifacts whose generating set holds it, sized by `Σ|G|` — which is
in tension with `annotation-write-cycle.md` §4.5, whose whole content is that the deny lane does no
artifact work. Recorded here, reconciled in the build wave; a partition consulted without this
correction is fail-open, and that is what makes it an acceptance test.

⊘ **One thing the measurement does not model, and it is a correction to the partition rather than to
this ruling.** A generating set that lost members in projection can never be contained, which is per
view and mask-independent, so it folds into the build rather than being asked per request.

⊘ **The partition is not built.** Containment is computed live, per artifact, per request, against
the composed mask (delivery Stage 3), which is correct and is what the figures above are measured
against.
