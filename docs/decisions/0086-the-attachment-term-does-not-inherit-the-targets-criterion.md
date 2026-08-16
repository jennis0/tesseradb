# 0086 — The attachment term does not inherit the target's criterion

**Date:** 2026-08-17 · **Status:** Settled

## What this answers

An artifact may be published as an attachment to another — the edge Stage 3 built, and what makes a
label a label rather than a set that happens to sit on top of one. The predicate withholds an
attached artifact when its target is **deleted**, **suppressed**, or in a **layer the viewer cannot
reach**: a label does not outlive the thing it labels
([decision 0076](0076-an-artifact-is-served-whole-or-not-at-all.md)'s companion rule, evaluated on
every route rather than only on traversal).

Review 2026-08-16 raised the fourth case the term does not cover. A target may be perfectly alive
and reachable and still not servable *to this viewer*, because its masked count fails its own
layer's **existence criterion** — the control that decides whether a grouping is announced at all
([decision 0075](0075-the-masked-count-is-an-existence-criterion.md)). Today the label is served
anyway.

## The decision

**The term stays as it is: disposition and layer reachability, not the target's criterion.**

## Why

**A label is an artifact, not a decoration on one.** It carries its own membership, its own masked
count and its own layer's declaration, and it is that declaration which decides whether *this*
grouping is announced. Inheriting a second layer's threshold would mean a layer's artifacts appear
and disappear by a number set on a layer its own declaration does not mention — the objection
[decision 0084](0084-an-undeclared-criterion-declares-no-test.md) sustains against defaults reached
from somewhere other than the declaration in front of the reader.

**The surface the extension appears to close is already governed elsewhere, and by the more
permissive declaration.** Where two layers cover the same points under different declarations, what
is recoverable about both is governed by the weaker of the two (C1, r43). A label layer declaring a
weak criterion over a cluster layer declaring a strong one is exactly that case, and the label's own
membership and count are the disclosure — not the edge. Extending the term makes the two layers
agree in one direction while leaving the general case untouched, which is mechanism bought at less
than face value.

**The target's identity does not cross the boundary.** A served artifact carries its layer, its
opaque identifier, its masked count, its derived shape and its content. It does not carry what it
hangs from, so a label reveals that *a* grouping was named here, on its own membership — never which
artifact on the target layer it was written about.

**The cost is on every route, not on traversal.** The existing term is two overlay lookups, one of
them the lookup the predicate's first branch already performs on the artifact's own entity.
Inheriting the criterion is a second masked count — the same work as serving the target — paid per
attached artifact per request, on the viewport, on search and on a held identifier alike.

## What this costs, stated plainly

A deployment can publish a cluster layer that withholds small groupings and a label layer that does
not, and a viewer too sparse to be shown the cluster will be shown the label written about it. That
is a real asymmetry and it is the operator's to avoid: **a label layer over a gated cluster layer
should declare a criterion at least as strong as its target's.** Nothing enforces that, and this
decision is why nothing does.

The build plane's containment verification is where an advisory check belongs if one is ever wanted
— it already reports violating edges rather than deciding anything (Stage 5), and a label layer
weaker than the layer it attaches to is the same shape of finding.

## Consequences

- The predicate is unchanged; this decision records why, so the case is not rediscovered as a gap.
- The register needs no new row: C1's r43 annotation already carries the multi-layer surface, and
  this is an instance of it rather than a second channel.
- Stage 4 closes its open ruling; the strict/permissive declaration beside it is unaffected, being
  about content rather than existence.
