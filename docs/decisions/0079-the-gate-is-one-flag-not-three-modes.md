# 0079 — The gate is one flag: does the artifact carry its own terms

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The decision

The three gate modes are **not three modes**. With the existence criterion made independent by
[decision 0075](0075-the-masked-count-is-an-existence-criterion.md), the enumeration was describing
a two-by-two in three names:

| The old name | What it actually was |
|---|---|
| **derived** | no own terms — visibility is what the criterion says |
| **substitutive** | own terms, no criterion declared |
| **conjunctive** | own terms, criterion declared |

So a layer declares **one flag and one independent control**: *does an artifact carry its own access
terms*, and *is an existence criterion declared*. Both are conjuncts of one test; composition is
conjunction and never disjunction, on the slice gate's precedent — an empty required set under a
disjunctive reading admits everyone, which is an error this corpus has already made once and caught
in review.

**The enum had no name for the fourth cell**, which is a real configuration: no terms and no
criterion, an artifact whose existence discloses nothing and whose count is masked. That is what a
density level is, and the recast in
[`annotation-representation.md`](../design/annotation-representation.md) §10 reaches it without a
special case.

## The safety property this gains, which is the reason to prefer it

Under the enumeration, **one schema word disabled a disclosure control**: declaring a layer
substitutive switched off `min_visible_members` entirely, so a corpus-derived clustering
mis-declared substitutive served the existence and count of every cluster down to one member. The
model flagged that as needing a register row of C12's class — a caller assertion the service cannot
verify.

Under the flag it cannot happen that way. The criterion is declared or it is absent, and its absence
is its own statement rather than a side effect of a word chosen for a different reason. A layer that
serves everything says so in the field that means it. **The mis-declaration hazard does not
disappear — a caller can still omit the criterion — but it stops being reachable by accident from an
unrelated choice**, which is the difference between a control with a trap and a control with a
switch.

The register row still belongs to the *terms* half: whether an artifact's own terms are the right
ones is a caller assertion, unverifiable here, and sits beside C12 with the corpus-independence
declaration.
