# 0117 — A child may name several parents, and a withheld artifact is not in the viewer's tree

**Date:** 2026-09-01 · **Status:** Settled (owner ruling)

## What this answers

Rung 3 of the ingest campaign keys its MeSH layer by descriptor, and a descriptor sits at several
positions of the NLM's tree — 30.0% of them under more than one parent. Every hierarchy kind refused
a child naming two parents, rightly for a tree. GeoNames' feature containment has the same shape.
The design is [`dag-hierarchies.md`](../design/dag-hierarchies.md); five questions were put and
ruled the same day.

## The decision

**(A)** A fifth hierarchy kind, **`dag`**: `nested` in every respect — no levels, edges within the
level, roll-up — except that a child may name several parents. A second parent is recorded, not
refused; a self-edge and a cycle refuse at both entry points. A kind value rather than a key on
`nested`, because the kind is what every reader switches on and a tree and a graph are different
shapes.

**(B)** Depth on a `dag` layer is the **longest path** from a root, so every edge descends. The
served count is then not monotone in depth, so a budget takes the deepest depth that fits by reading
every depth's count rather than bisecting.

**(C)** The artifacts frame's `parent_id` becomes **`parent_ids: list<uint64>`** for every kind —
the served parents in the same response, ascending by `tessera_id`. C29's control applies per
entry. **`api_version` does not move**: nothing has launched, and the client changes in the same
commit.

**(D)** Rung 3's membership is the **ancestor closure**, emitted as a set per article. MeSH indexes
the most specific heading only; without closure a parent does not contain its children and roll-up
substitutes a count of a different thing.

**(E)** **A withheld artifact is not in the viewer's tree.** The cut is taken over the artifacts
the principal passes, with an edge wherever one is the nearest passing ancestor of another, depth
counted in passing artifacts. The implementation walks the whole level today, on a tree as well,
so a budget's settling depth was a function of artifacts the viewer may not see. Corrected rather
than registered.

## What this does not change

No verdict and no count: every artifact is tested on its own masked count against its own
criterion ([0080](0080-the-frontier-is-a-per-artifact-test.md)), the number served is its own
declared membership, and roll-up is substitution and never a sum
([0087](0087-cross-level-edges-are-information-not-rollup.md)). `tiered` still refuses two coarser
parents. Keying MeSH by tree number, which needed no change, was declined because it duplicates a
concept and every descendant of it at each of its positions with nothing on the wire to say they
are one thing.
