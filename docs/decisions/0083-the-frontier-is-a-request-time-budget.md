# 0083 — The frontier is a request-time budget, not a declared depth

**Date:** 2026-08-16 · **Status:** Settled (owner ruling)

## What this answers

With a nested layer's hierarchy in its edges and no levels
([decision 0082](0082-a-hierarchy-lives-in-edges-levels-are-resolutions.md)), something that levels
had been doing quietly stops happening: **bounding the response**. Asking for level 1 used to bound
what came back. A viewport now intersects a root cluster *and* every descendant of it that passes, so
the natural response is a whole passing subtree — overlapping shapes at every depth, the same points
counted several times over.

## The decision

**The depth of the cut is a request parameter, and the layer declares only the default.** A viewport
request carries an artifact budget in the same shape as the mark budget it already carries; the
server honours it by cutting the tree, and `prune_children` becomes the layer's default rather than
its only setting.

**The budget is honoured structurally, never by sampling.** Artifacts cannot be sampled — dropping
half the boundaries gives a wrong map rather than half a map, and no ordering over artifacts makes
the retained half stand for the discarded half. So a budget that cannot be met by serving everything
is met by **serving ancestors instead of their descendants**, which is reduction by the layer's own
structure: the one route the representation leaves open beside *serve them all* and *refuse*.

**That is what rollup is for.** It was described as a disclosure behaviour — insufficient visibility
becomes a vaguer ancestor — and under per-artifact testing it is no longer needed for that, because a
failing child simply leaves its passing parent served. What it is *now* is the mechanism that makes a
tree renderable, and it earns its place there.

## Why depth is free, where the old frontier's depth was not

**Both directions are safe.** Cutting shallower serves fewer and coarser artifacts, which is strictly
less. Cutting deeper serves more, and every one of them passed its own existence test independently
against `M_auth`. So no depth a caller can ask for reveals anything that per-artifact testing did not
already permit.

**This is the sharp contrast with what it replaces.** §8.4 fixed maximum depth against `M_auth` and
never against `M_sel` — the operational form of **I12**, because there the depth *was* a disclosure
control and a filter that deepened it would have differenced a suppressed node into view. Under
per-artifact testing the control is the criterion, evaluated per artifact against `M_auth`, and depth
carries none of it. **A budget is not a control**, and the two should not be confused because they
occupy the same place in the request.

**It stays inside the client/server boundary.** A client asking for less detail is selecting a
request, not filtering a response — the same reading that lets a client toggle a layer off. What it
must still never do is discard artifacts it was served and redraw the map as though they were absent,
because the counts beside them are the server's answer.

## What it costs

⊘ **A budget that resolves to different depths in different branches is the honest general case**, and
it is unspecified here: a dense region may need cutting shallower than a sparse one to fit the same
budget. The simple form — one depth for the whole tree — is what a first implementation should do, and
it will be visibly wrong on an unbalanced tree, which is every real clustering.

⊘ **Nothing here is measured.** The cut is a walk over the edges of the passing set within the
viewport; its cost is bounded by that set rather than by the tree, and the constant is unknown.

## The alternative, and why not

**Fix the depth per layer in its declaration.** It removes the request parameter and makes responses
predictable — and it makes the layer undrawable at some zooms, because the right depth is a property
of what the client is trying to show and how much of it it can draw, which the declaration cannot
know. A deployment would end up declaring several layers over one clustering to get several depths,
which is the level system again, wearing the costume the hierarchy ruling just took off it.
