# 0093 — Nothing is materialised per token over the artifact population

**Date:** 2026-08-21 · **Status:** Settled (owner ruling)

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
**build-time containment partition**: one byte per artifact naming the boolean expression over terms
that decides it, plus one bitmap per distinct expression. It names no principal, so one copy serves
every token that will ever exist.

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
to evaluation. What keeps the number of distinct expressions small is that a generating set is
usually drawn from inside one signature group, and composes to *holds that group's term* — so the
count of expressions is the vocabulary's rather than the layer's.

**It measures at parity with the per-token route and ahead of it where it matters.** At 10⁷
artifacts and a full mask the two are within noise across four viewports (140.0 ms against 141.9 at
whole map, 0.221 against 0.186 at a 0.024% viewport) — with the per-token structure's 151 ms of
setup deleted rather than amortised. At a **narrow** principal the build-time route wins outright,
**4.09 ms against 13.1** at whole-map zoom, because the groups a principal fails are never touched
where the per-token pass had to evaluate every artifact once to find that out.

**It costs 40 MB of state naming nobody**, against ~40 MB per live session, and a token then costs
what it always did: its mask fragment, and no artifact structure at all.

## What this forecloses

The masked count under an **existence criterion** at a wide viewport is genuinely per-principal and
does not dissolve (memo §4.3). One of its three candidate answers was to hold the counts per token
after all — which is now the only thing §8.5's structure would have been for, and **this decision
closes it**. Two candidates remain, both unbuilt and neither yet chosen:

- **Per-signature counts**, build-time and mask-independent, summed over the satisfied signatures
  per request. Its storage scales with the **signature** count rather than the artifact count, which
  is the thing to measure against a real corpus before building it.
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
- The partition is build-time state, so it is rebuilt where row forms are rebuilt — at a generation
  move and inside the fold — and it inherits whatever cadence
  [decision 0094](0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md) gives
  the rest of the per-generation structures.

⊘ **Two things the measurement does not model**, and both are corrections to the partition rather
than to this ruling. A **suppression** removes a member of `G` from `M_auth` whatever the terms say,
so the answer must be intersected with *no member suppressed* — an inverted index from entity to the
artifacts whose generating set holds it, refreshed when the **overlay** moves rather than per token.
And a generating set that lost members in projection can never be contained, which is per view and
mask-independent, so it folds into the group.

⊘ **The partition is not built.** Containment is computed live, per artifact, per request, against
the composed mask (delivery Stage 3), which is correct and is what the figures above are measured
against.
