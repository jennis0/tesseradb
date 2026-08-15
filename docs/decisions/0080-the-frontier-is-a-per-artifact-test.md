# 0080 — The frontier is a per-artifact test, not a tree walk

**Date:** 2026-08-15 · **Status:** Settled (owner ruling)

## The decision

**The descent is dropped.** §7.5 selects which cluster nodes to serve by walking down the tree from
the root, testing each node's masked count and stopping where one fails, so that insufficient
visibility becomes a vaguer ancestor rather than a hole. That algorithm is replaced by: **gather the
artifacts whose members intersect the viewport, test each independently against the layer's
existence criterion, and serve those that pass.** Where a child and its parent both pass, the client
prefers the child at depth; rollup emerges from independent decisions rather than being a property
of the walk.

This contradicts a normative document, which is why it was the owner's to rule. ⊘ **An amendment to
§7.5 is owed** at promotion.

## Why the walk cannot stay

**Correction, 2026-08-16** *(owner)*: this section first claimed *"HDBSCAN children hold points their
parent does not"*, and that is **false of a condensed tree**. HDBSCAN splits branches, so a child's
members are always a subset of its parent's and there is explicit parent/child lineage. What is true
is that children do not **exhaust** a parent — points fall out as noise at each split, 20–25% of them
on the real corpus — which is a different claim, and one that leaves counts monotone downward. The
two were merged, and the wrong half was the stated reason for this ruling.

**Where the walk genuinely fails is the stacked case**, which is what produces a child holding points
its parent does not: three *independent* runs at three `min_cluster_size` settings, where a point that
was noise in the coarse run joins a cluster in the fine one. That is what the measurement campaign
produced, and it is a real configuration — but it is not what a clustering with a hierarchy is.

**So the walk is well defined for a nested layer and ill defined for a stacked one**, and the
surviving reasons for testing per artifact are the two that do not depend on monotonicity: the
disclosure control becomes the per-cell case the census literature is about, and the whole-level leak
below cannot arise. ⊘ **Whether the walk should return for nested layers, as a display policy over
per-artifact outcomes, is reopened by this correction and is not settled here.**

**The repair that looks obvious is rejected and recorded so it is not proposed again.** Defining a
node's *reach* as its own members unioned with its descendants' restores monotonicity by
construction — and makes the engine assert a membership the caller never declared, so the count and
geometry describe an invented set while the content describes the declared one. Two inconsistent
statements about one object, one of them ours.

## What is gained

**Nothing is required of the data.** No covering, no nesting, no containment between levels. Every
artifact means what its creator said it means, which is what lets the model carry analyses that do
not cover in either direction.

**The disclosure control becomes the case the literature is about.** Small-cell suppression is a
decision about one cell, evaluated alone; the tree-walk form had no analogue anywhere in the survey,
which is why C1's outstanding review had no prior art whose failure modes it could borrow. A
per-artifact test can be reviewed in the field's own vocabulary.

**The whole-level leak cannot arise.** Holding a viewer at the last level they can see *entirely*
announces the suppression it is meant to conceal — a viewer whose own points obviously split knows
some other cluster exists below threshold, and can difference it by panning the region in and out of
view at one bit per level. With artifacts tested alone, a level is served **partially** and an absent
artifact is indistinguishable from one that never existed.

## What is lost, stated plainly

**The rollup promise weakens, and it was underwritten by covering.** *"Nobody gets a blank region;
they get a vaguer ancestor"* holds only where an ancestor exists that the viewer passes. Without the
walk, if nothing coarse enough passes in a region, the viewer gets their points and no artifact.

**That is the caller's to fix, by supplying a covering top level**, and it belongs in the labeller
guidance as a stated consequence rather than being manufactured on their behalf — manufacturing it
is the rejected repair above.

**The cost model changes shape.** The walk paid one test per node visited and pruned whole subtrees;
this pays one masked count per candidate on screen, bounded by the viewport rather than by the
hierarchy. ⊘ The constant is unmeasured.
