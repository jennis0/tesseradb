# 0075 — The masked count is an existence criterion, declared absolute or proportional

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The correction this starts from

`min_visible_members` was being described — in the review, in the delivery plan and in places in the
designs — as a *count threshold* or *count suppression*. **It is neither.** It does not suppress a
number: it uses the masked count as the criterion deciding whether the artifact is served at all
*(owner, 2026-08-15)*. The number beside a served artifact is the masked count, unmodified, as it
always was.

**Correction, same day** *(owner, 2026-08-15)*: this section first claimed a **second mechanism**
existed beside the criterion — *"count suppression / rollup: serve the artifact, withhold or coarsen
its number"* — citing [`annotations.md`](../design/annotations.md) §8.2's boundaries. **There is no
such mechanism, and inventing one contradicted
[decision 0076](0076-an-artifact-is-served-whole-or-not-at-all.md)**, under which an artifact with a
withheld number is a partial artifact and forbidden.

**A ward and a district are two artifacts, on two levels of one layer, joined by an edge — not one
artifact whose number is drawn two ways.** So "rolling a suppressed ward count up to its district" is
not an operation on a number at all: the ward fails its criterion and is **absent, whole**; the
district contains more of the viewer's visible members, clears the criterion on its own count, and is
**served whole, with its own exact number**. What the viewer sees is a coarser object, which is what
rollup always was.

That is [decision 0080](0080-the-frontier-is-a-per-artifact-test.md)'s statement — *rollup emerges
from independent per-artifact decisions rather than being a property of the walk* — arriving in the
geographic case, and the two should be read together. The count beside any served artifact is its own
masked count, unmodified. **Nothing suppresses a number.**

## The decision

**The criterion is configurable, and takes either form:**

- **absolute** — serve iff the masked count ≥ *N* visible members;
- **proportional** — serve iff the masked count ≥ *p* of the artifact's **declared** membership.

The proportional form is the one that scales: a fixed bar of fifty protects a cluster of a hundred
and does nothing for a cluster of ten thousand, where fifty visible members is half a percent of it.

**It is declared per layer, and it is a disclosure control, so it has no default** (CLAUDE.md: a
config file doubles as a disclosure-review checklist). A layer that wants no criterion says so.

**It is independent of the gate mode**, which answers ruling 2 of
[the Stage 0 review](../evidence/memos/2026-08-15-artifact-design-review.md) by dissolving it: the
gate modes and this criterion are separate conjuncts of one existence test, not one field with two
meanings. A substitutive layer may carry a criterion; a derived layer may decline one.

## What the proportional form costs, and the guard it needs

⊘ **It requires the artifact's declared, unmasked membership size to be available server-side**, and
[`annotation-representation.md`](../design/annotation-representation.md) §2.4 deliberately stores no
such quantity: *"An artifact's unmasked own-count is deliberately not stored. It is the obvious field
to add and it is C8"* — a corpus-wide count over items a principal may not see, one careless line
from being served beside a masked one.

That position is narrowed rather than reversed. The unmasked size becomes an **input to a
predicate**, never a field: it is read to evaluate the criterion and has no path to a response. The
representation's argument was never that the quantity is unknowable — the build computes it — but
that storing it beside the served ones invites its serving. The guard that replaces "do not store
it" is that no wire shape carries it, which the register already requires of C8 and which
`/v1/meta`'s refusal to serve artifact cardinality already exercises for a sibling quantity.

## Why the criterion exists at all, recorded because it was challenged

It does not protect counts. The density underlay already serves exact masked per-tile counts at any
depth with no criterion, accepted at **C18** because those counts are over the viewer's own visible
data. What this protects is the existence of a **corpus-derived grouping**.

The case that justifies it: a corpus where the topic map is open to everyone and documents are
restricted per project. Without a criterion, a principal holding one project is shown every cluster
in which they hold a single document, each one asserting *a topic exists here and the clustering
found members in it* — and across hundreds of such clusters they reconstruct the corpus's topic
structure from a grant that covers a thousandth of it. With one, they are shown the clusters they
can already see a substantial part of, so an artifact's existence is approximately their own
information rather than news about material they cannot read.

**It is per-artifact and not per-layer** because the same principal legitimately keeps the clusters
they are deep in. The blunt instrument — *you may not see this analysis at all* — is the layer gate,
which is a different control and stays one.

**What it does not achieve, stated so it is not oversold**: concealment. C1 already accepts that a
viewer can reason *"my data is dense here and nothing covers it, so something is hidden"*, and a
suppressed grouping's size is partly recoverable by subtracting covered points from the tile counts
the underlay serves anyway. This bounds precision; it does not close the channel.
