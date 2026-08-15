# 0076 — An artifact is served whole or not at all

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The decision

**A label is its text.** There is no state in which a viewer may know a label exists but may not read
it, and the rule generalises past labels *(owner, 2026-08-15)*:

> **Beyond ranked versions, there are no levels of restriction *within* a single artifact.**

An artifact is visible to a principal, entire, or it is absent — indistinguishable from one that
never existed. Versions remain the one exception and are not one in substance: they are the same
artifact described at different clearances, resolved first-satisfied, and a viewer who satisfies no
version sees no artifact.

This answers ruling 3 of [the Stage 0 review](../evidence/memos/2026-08-15-artifact-design-review.md),
and it answers it more widely than the ruling asked.

## What it removes

**The model's three questions collapse to two.** *May you know it exists* and *may you see this
content* stop being independent: an artifact exists for a principal exactly when its gate passes,
its existence criterion passes ([decision 0075](0075-the-masked-count-is-an-existence-criterion.md)),
and **every** corpus-derived content it carries is contained in that principal's mask. One
conjunction, evaluated once, rather than a per-content test whose failures had to be represented in
a response.

**C3's open question evaporates.** The register asked whether withheld content is distinguishable
from content that was never declared. Under this rule nothing is withheld — a failed containment
removes the artifact — so there is no shell to be distinguishable and no mechanism owed. C3 holds as
written, and it holds because the model now matches it rather than because a claim was made about it.

**Degrade-to-derived is withdrawn.** `annotations.md` §8.6 had a viewer who fails containment on a
supplied centre and radius still receiving the artifact, its masked count and a recomputed centroid —
*"the artifact degrades to its derived content rather than disappearing"*. That behaviour is deleted.
Under this rule the fitted geometry is corpus-derived, so failing its containment means failing the
artifact. §4.1's observation that an artifact may carry both supplied and derived content survives;
what does not survive is serving one when the other was refused.

**The three routes get simpler.** No response shape has to express *artifact present, content
absent*, so the viewport, drill-down and edge traversal carry one predicate and one answer.

## What it costs

**Availability, and it should be stated rather than discovered.** One sensitive description withholds
the whole artifact — its identity, its count and its geometry — from every principal who fails that
description's generating set. Where an artifact carries several contents of differing sensitivity,
the artifact is only as available as its most restricted part.

**The caller's remedies are the two the model already has**, and this rule is what makes the choice
between them consequential rather than stylistic: ranked **versions**, where one artifact is
described at several clearances and each viewer gets the first they satisfy; or **separate
artifacts**, where the descriptions are different statements and each stands or falls alone. A caller
who wants the coarse thing visible to everyone and the detailed thing visible to a few writes two
versions, not one artifact with two contents.

**Corpus-independent content is unaffected**, since its generating set is empty and containment is
vacuous. `annotations.md` §8.5's programme — a name and an authored extent, visible to every
principal, over documents most of them cannot read, with a masked count of zero — behaves exactly as
written.

## Why this is the right way round

The alternative was already in the corpus and was already refused for labels: §7.6's normative rule
is that a principal never learns of the existence of a label they cannot see, and the response omits
every unsatisfied candidate. The model had drifted from that to *omit the content, keep the artifact*,
which reversed a **Closed** register row on the way. This restores the corpus's own position and
extends it to the general object, so the artifact population and the label population stop having
different answers to the same question.
