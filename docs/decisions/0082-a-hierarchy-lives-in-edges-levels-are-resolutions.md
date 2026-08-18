# 0082 — A hierarchy lives in its edges; levels are resolutions, not depths

**Date:** 2026-08-16 · **Status:** Settled (owner ruling)

## The decision

**A layer's tree is its edges. A layer's levels are declared resolutions. They are different
structures and neither stands in for the other.**

- **A nested clustering declares no levels.** Its artifacts are the nodes of one tree and its
  lineage is the parent/child edges. Every artifact sits at level 0, which is then an address
  component carrying no information — as it should be, since the structure is in the edges.
- **Levels remain for what they were for**: a resolution that is *semantic and balanced*. An
  administrative hierarchy is the case — a ward is a ward everywhere on the map — and so is a
  **stacked** layer, where each level is an independent analysis with no lineage between them.

## Why levels cannot carry a tree

**A condensed tree is unbalanced.** HDBSCAN splits a branch where the density says to, so one region
of the space splits at depth two and another at depth nine. Cutting such a tree at `min_cluster_size`
values produces levels whose members sit at many different depths, so *descend one level* is not
*descend one edge* and a level number tells a client nothing about position in the lineage.

[`annotation-representation.md`](../design/annotation-representation.md) §6.2 already says levels need
not align with tree depth — and the model then used levels as the ladder a frontier descends, which
is only sound if they do. This ruling resolves that by separating the two rather than by requiring
balanced trees, which no clustering algorithm produces.

## Rollup needs no walk, and its condition is the criterion's form

**Under an absolute criterion, rollup is automatic.** A child's members are a subset of its parent's,
so its masked count is never larger; if a child fails `min_visible` and its parent passes, the parent
is served — because it was tested on its own and passed. That *is* rollup, and per-artifact testing
([decision 0080](0080-the-frontier-is-a-per-artifact-test.md)) delivers it with no descent at all.

⊘ **Under a proportional criterion it is not, and this is the interaction to know.**
`min_fraction` divides by declared size, and a ratio does not shrink downward:

> A parent with 10 000 declared members and 500 visible is at **5%**. Its child with 200 declared and
> 100 visible is at **50%**. The child is a strict subset — 100 ≤ 500 — and it **passes** a 10% rule
> while its parent **fails** it.

So a passing child beneath a failing parent is possible in a perfectly nested tree, arriving through
the criterion rather than through the data. **Rollup is therefore guaranteed only for absolute
criteria**, and a layer declaring `min_fraction` must expect gaps in its lineage: a viewer may be
shown a fine cluster with nothing coarser above it. That is not a disclosure — each artifact passed
its own test — but it is a rendering consequence a caller should choose deliberately, and it is
recorded here because the proportional form is otherwise the better default (it scales, where a fixed
bar does not).

## Frontier selection is a display concern, and safe either way

**When a parent and a child both pass, something must choose**, or the map draws both and the same
points are counted twice at two sizes. Choosing the deepest passing artifact per branch is a tree
computation over the edges — for each passing artifact, does a descendant also pass — and it is what
`prune_children` names.

**It carries no disclosure argument in either direction.** Every artifact served has passed its own
test independently, so serving the frontier rather than every passer serves strictly *less*; and
serving every passer reveals nothing beyond what each artifact's own presence already does. **So it
is decided on rendering grounds**, which is unusual enough in this corpus to be worth stating: most
questions of the form *what do we withhold* are the register's, and this one is not.

## What this changes

| Where | What changes |
|---|---|
| The model's hierarchy declaration | `kind = "nested"` implies a tree in edges and **no levels**; `kind = "stacked"` implies levels and **no lineage**. *(The third shape §6.2 describes — levels **and** edges, the administrative case — had no value here until [decision 0087](0087-cross-level-edges-are-information-not-rollup.md) added `kind = "administrative"`. Its edges run between levels and are information rather than roll-up, so nothing in this ruling's separation of tree from levels is disturbed by it.)* |
| The zoom→level map | does not apply to a treed layer — there are no levels to map, and depth is not a resolution |
| `annotation-representation.md` §6.2 | *"levels need not align with tree depth"* becomes *"a tree has no levels"* |
| The artifact address | `(layer, level, ordinal)` is unchanged; a treed layer's level is always 0, and one reserved entity run serves it |
| [Decision 0080](0080-the-frontier-is-a-per-artifact-test.md) | its correction reopened whether the walk returns for nested layers. It does not need to: rollup falls out of per-artifact testing under an absolute criterion, and what remains is frontier *selection*, above |

## The alternative, and why not

**Require balanced hierarchies and keep levels as the ladder.** It would let a client descend by level
number and need no edge traversal. It is declined because no clustering algorithm produces a balanced
tree, so the requirement would fall on the caller to flatten their own structure into one — throwing
away the lineage the algorithm computed, which is the mistake this ruling exists to correct.
