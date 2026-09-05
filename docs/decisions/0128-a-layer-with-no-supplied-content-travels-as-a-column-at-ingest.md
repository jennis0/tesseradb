# 0128 — A layer with no supplied content keeps its member source at the base build and travels as a column at ingest

**Date:** 2026-09-05 · **Status:** Settled (owner ruling) · ⊘ Not built in the driver

## What this answers

The owner ruled on 2026-09-03 that in the ingest cycle everything is ingested after the build: the
base bundle carries the points and the declarations, and every artifact, membership and supplied
content is published through `PUT /control/layers/{name}/artifacts` after the points it depends
on. The reason was layers with **supplied** content: a key naming no artifact yet is minted, and
`LayerRegistry::resolve_or_mint` refuses to mint on a layer that declares supplied content, so a
membership column at either entry point would always arrive first and always be refused.

Rung 5's `taxonomy/tree` has no roster to publish. It is an open value set with computed content
only, a seven-level tiered hierarchy minted at the build from a list column of 233,055,986 rows,
1,001,193 artifacts, on the order of 10⁸ members at its kingdom level. The publication step reads
rosters, so the layer has never been carried in the ingest cycle, and its census rows differ by
construction.

## The decision

**A layer that declares no supplied content keeps its `[layer.members]` source at the base build
and travels as the ingest batch's column named for the layer.** The column is what contracts §3.4
already specifies: a `tiered` list carries one entry per declared level, an unknown key under
`value_set = "open"` creates the artifact carrying its name and the computed content its points
give it, and a lineage naming artifacts that do not exist yet creates the chain parent before
child in one batch. Growth is per batch and never meets the publication route's cap.

The 2026-09-03 ruling stands for every layer with supplied content. This refines it for the case
its reason does not reach.

## Why

The two entry points already agree on this column (decision 0091); the driver was declining to use
the route the contract specifies. Synthesising a roster from the list column and publishing it
would need decision 0127's growth for the kingdoms and would exercise nothing the build does. The
column route exercises the mint-from-column path at 2.3×10⁸ rows, which nothing has yet.

## What this changes elsewhere

`test_corpora/common/ingest_cycle.py`: `base_declaration` removes `[layer.members]` only from
layers with supplied content; `encode_batch` carries the layer's list column for the others; the
module doc's "Everything is ingested after the build" section says which layers take which route.
The census for `taxonomy/tree` is then a real comparison.
