# 0125 — A `dag` list column is membership, not lineage

**Date:** 2026-09-03 · **Status:** Settled (owner ruling)

## What this answers

Rung 3 of the ingest campaign publishes a MeSH article's ancestor-closed descriptor set, about
forty-six entries per article. A build took them as member rows, one per entry, and read plain
membership; an ingest batch has one row per point, so the same set travels as a list column. The ingest route read the same column as a lineage — entry *k* the parent of entry *k+1*
— which on a set of about ten unrelated descriptors asserted about nine edges per row the tree does
not hold; the driver declined the layer, and carrying it anyway cost 31k items/s against 157k. The
two entry points disagreed about what one column meant, which
[0091](0091-build-is-ingest-into-an-empty-database.md) forbids. The problem statement is
[the memo](../evidence/memos/2026-09-03-dag-membership-at-ingest.md).

## The decision

Under hierarchy kind `dag`, **a list of member keys on a member row is plain multi-membership — a
set in no order**, read exactly as `flat` reads one. It is not a lineage and declares no edge.
Under `dag`, an edge is spelled **only on the artifact row's `parent` column**, which may be a
list, at both entry points: a build's artifact rows, and the roster published on `/control/layers`
at ingest. `nested` is unchanged: its list is still a lineage.

## Why

A tree node's ancestor closure is a chain, so a `nested` lineage list states memberships and edges
at once and the two never disagree. A DAG node's closure is a set with no linear order, so the
adjacency of a list of it carries nothing anyone could have meant — reading it as a lineage does
not recover the graph, it invents one.

## What this changes

- The sentence of 0117 that read *a second parent is
  recorded, not refused* as applying to a lineage list is withdrawn; `dag-hierarchies.md` §4's
  second bullet, which spelled that out, is rewritten. A second parent is still recorded when it
  arrives on the artifact row's `parent` list.
- `ListMeaning::of(Dag, _)` is `Unordered`. The machinery that turned a lineage-list conflict into
  an insertion under `dag` — in the build's `record_lineage` and `apply_lineage`, the ingest
  route's batch and window checks, and the registry's `check_edge` — is deleted; what remains is
  the tree refusal, at every kind, for the kinds whose lists declare edges.

## What this does not change

`nested` and `tiered` lists; depth as the longest path; roll-up; cycle refusal at both entry
points; `parent_ids` on the wire; the cut over the viewer's passing artifacts; and 0117's other
rulings — the kind value, the ancestor closure as rung 3's membership, and a withheld artifact not
being in the viewer's tree.
