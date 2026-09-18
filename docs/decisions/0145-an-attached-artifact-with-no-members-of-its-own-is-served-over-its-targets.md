# 0145 — An attached artifact with no members of its own is served over its target's

**Date:** 2026-09-18 · **Status:** Settled (owner ruling, in conversation on the Python SDK's
label insert; [`python-sdk.md`](../design/python-sdk.md) §11.1)

## Context

A label is an artifact attached to the cluster it names, and an artifact is placed in tiles,
counted and tested for existence over its own member rows. A label published with no member rows
was therefore in no tile and served to nobody, with nothing refusing it: the build reported "0
artifacts with rows" and the map showed no label. The SDK's mapping form, a cluster key to a
line of text, produced exactly that, and every corpus's label set carried a members table to work
round it.

## The decision

A label with no member rows is the label of its cluster. An attached artifact that declares no
members of its own is served over its target's membership: placed where the target is placed,
counted over the target's visible members inside the viewer's own mask, tested by its own
criterion against that number, and served to whoever is served the target. That is the default
and no declaration key asks for it. The membership is read at the target's current membership,
so a target that grows grows its labels and a fold that re-derives the target re-derives them. A
chain of attachments is followed to a bound of eight; a hole in the chain leaves the record's own
empty set, which serves nothing.

An attached artifact that declares members keeps them. They are the generating set the caller
claimed (decision 0135), and a content requirement of `all` reads them unchanged. The predicate
is a state, not a flag: a label whose declared members were all deleted and retired at a fold has
none of its own and is served over its target's from the next form build.

The build, the fold, the flush and the request path resolve the membership through one store
function (decision 0139), so what a bundle's tile index and column describe and what a request
counts cannot come apart, and a build's report counts such a label's rows.

## Why it discloses nothing

The borrowed thing is a set of rows, and every count over it is taken against the borrower's own
composed mask (I2). Existence is the target's whole predicate one conjunct earlier (annotations
§5's attachment prerequisite, decision 0089), so a principal not served the cluster is not served
its label, filtered or not (I3, I12). No leak-register row is added.

## What it amends

annotations.md §2.2's "a label carries its own membership" now reads "its own membership where it
declares one, and its target's where it does not"; §3's per-artifact test against "its own
declared membership" reads the membership the artifact is served over. The viewport row of a label
carries that count. The rule that gave a dependent's row its target's masked count is withdrawn (owner ruling 2026-09-18; contracts
§3.2, r96), and the row names its target by `tessera_id`, so the viewport and the drill-down agree
for every label.
