# 0093 — Nothing is materialised per token over the artifact population

**Date:** 2026-08-21 · **Status:** Settled (owner ruling) — **amended the same day**, after the
campaign's adversarial review, with the row-major exception the owner named and with the evidence
corrections that review found ([the record](../evidence/memos/2026-08-21-artifact-serving-scale-review.md)).
**Its evidence was re-measured on the corrected probe and recorded here on 2026-08-22**; the ruling
is unchanged and its price is now a number.

## Context

[`architecture.md`](../design/architecture.md) §8.5 specifies a **servable-label set** held per
token: the labels a principal contains, computed once when their mask is materialised and reused for
every request in the session. It is specified and unbuilt, and the scale campaign's first draft
leaned on it — it is the obvious home for the two `O(artifacts)` questions that depend on the mask
but not on the request, containment (`G ⊆ M_auth`) and the masked count.

**There are a great many tokens** (owner, 2026-08-21). A principal is a token, principals do not
share masks, and the target scenario is many concurrent ones over a shared bundle. So a structure
sized by the artifact population is the wrong shape on that cadence however cheap one copy of it is:
at 10⁷ artifacts the per-token route costs 0.85–1.7 s of setup and ~40 MB of state, per token, per
generation (measured; see the evidence below).

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

**How many distinct expressions a real layer produces is now measured, and it does not change the
ruling — it changes what the partition is for.** The probe's expression census composes,
canonicalises and interns the expression for every artifact of a population and counts the distinct
ones (scale memo §4.2, `probes/2026-08-20-artifact-serving-scale/data/expressions.csv`). Under
[`annotations.md`](../design/annotations.md) §8.1's **per-term** authoring the count is 32 at every
generating-set size swept — the vocabulary's, not the layer's. Under generating sets drawn across the
demo corpus's real signature distribution — **54,794 distinct signatures over 2.4M items** — it is
**29,175 distinct expressions at `|G| = 1`, 329,080 at `|G| = 2`, 976,510 at `|G| = 4`, and ~10⁶ at
`|G| ≥ 8`: one expression per artifact over 10⁶ artifacts**, with interned expressions reaching
~306 MB at the widest set measured. **Outside per-term authoring the partition's sharing collapses
entirely.** What survives is the cadence this ruling rests on — one build-time copy naming no
principal, at any expression count — and what does not survive is the per-request *union* of the
satisfied expressions' bitmaps, which is a route only where the count is the vocabulary's. Where it
is the population's, containment is a per-candidate lookup through the identifier column, which the
viewport bounds: cheap at a narrow zoom, and the wide zoom's price is the build wave's to take. The
expression identifier is **at least a `u16`** either way.

**What the per-token route would have cost is now recorded rather than argued.** Measured at 10⁶
artifacts over 10⁸ points, whole map, full mask, every route carrying the masked candidacy probe the
design requires — *median of three runs;
`probes/2026-08-20-artifact-serving-scale/data/parity-r1e8-a1e6-medians.csv`*: the shipped
per-artifact loop is **1 362 ms** (4 443 ms at its worst cell), the build-time partition without
hoisting **379 ms**, the design's route **134 ms**, and the per-token route **12.4 ms per request —
plus 99–343 ms of setup per token per generation**, rising to **0.85–1.7 s at 10⁷ artifacts**, at
~4 B per artifact. So the per-token structure is genuinely the faster one per request, and the ruling
is a cadence judgement with a price attached: at 121 ms saved per request the setup breaks even after
one to three requests at 10⁶ artifacts and two to four at 10⁷ *(derived)*, never breaks even if the
generation moves first, and thousands of near-unique tokens are **4–40 GB resident plus minutes of
rebuild at every generation move** *(derived)*. Its setup is also dearest for the narrowest
principals — 99 ms at a full mask against 343 ms at 3.1% — which is the opposite of where a cache
would want its cost.

**The build-time partition costs one copy naming nobody**: 7.3 MB over 10⁶ `(artifact, rank)` pairs
and 73.3 MB over 2×10⁷ at the fixture's 32 expressions *(measured)*, ~306 MB where the census puts
one expression per artifact. A token costs what it always did — its mask fragment, and no artifact
structure at all.

## What this forecloses

The masked count under an **existence criterion** at a wide viewport is genuinely per-principal and
does not dissolve (memo §4.3). One of its three candidate answers was to hold the counts per token
after all — which is now the only thing §8.5's structure would have been for, and **this decision
closes it for an artifact-major layer**; the row-major exception above is the one place it does not.
Two candidates remain, both unbuilt and neither yet chosen:

- **Per-signature counts**, build-time and mask-independent, summed over the satisfied signatures
  per request. Its storage scales with the **signature** count rather than the artifact count, which
  is the thing to measure against a real corpus before building it — the fixture's thirty-two against
  the census's **54,794 distinct signatures** over the demo corpus's 2.4M items.
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
- **What the partition costs to build**, at the fixture's expression count: 0.8 s over 10⁶
  `(artifact, rank)` pairs and 5.9–12.0 s over 2×10⁷ *(measured)*. ⊘ That is thirty-two expressions
  composed and interned; at the census's real distribution it is a million distinct canonical forms
  instead, and **that build cost is not measured**.

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
