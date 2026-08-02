# 0040 — The quantisation extent is slice-scoped index configuration, immutable at runtime

**Date:** 2026-08-02 · **Status:** Settled

## Context

Morton codes are computed against an extent: `fixed32(v, min, max)` is
`floor((v − min) / (max − min) × 2³²)`, so a stored code is not a position in world units but a
**fraction of the declared extent**, and the extent is the frame that fraction is measured against.
Read the other way, a tile prefix names a fraction-of-extent rectangle rather than a fixed region.

Two questions made that consequential at once.

**Flush gave a buffered item geometry**, so `/control/ingest` now refuses a coordinate outside the
declared extent (flush §6) — an out-of-extent point has no cell to occupy, and clamping one at the
boundary would be indistinguishable from a point that belongs there. The refusal's stated remedy was
a re-quantising compaction.

**The pin design asked whether a Morton prefix is a stable client-facing address**
(`geometry-pinning.md` §3, §5). It is, against any generation — *unless* the extent changes, at which
point the same prefix covers different ground and a client's held `/v1/meta` extents silently
mis-decode every position it draws. That was the one case in that document's identifier audit that
did not re-resolve, and it was the basis for keeping a client-visible refusal on a superseded prefix.

## Decision

**The quantisation extent is a property of a *slice*, set when the slice is created and immutable
for its life. It is server-side configuration of the same class as an index's configuration in any
other database — an Elasticsearch mapping, a Postgres operator class. Changing it is a reindex, not
an operation.**

Three things follow, and they are the operative content:

1. **A compaction carries each slice's quantisation forward byte-for-byte.** Compaction is a
   reorganisation — the fold, re-ranking, a batch-grid change — and re-quantisation is not among
   them. This is the enforceable form of the decision and the obligation the compaction spec
   inherits.
2. **A Morton prefix is a permanently stable address** for the life of the slice, not merely until
   the next compaction. Every identifier a client holds — `tessera_id`, tile prefix, sub-cell
   identifier, the `/v1/meta` extents it decodes positions with — re-resolves against any
   generation, without qualification.
3. **Data outside a slice's extent can never be ingested into that slice.** Not "until a
   re-quantisation"; at all. The remedy is to rebuild the slice under a corrected extent, which is a
   migration.

## Why slice-scoped rather than deployment-scoped

A slice is already a distinct row space, and the tile grid is already per slice: a viewport names one
slice, and no tile arithmetic anywhere crosses slices. The extent is therefore scoped to a slice in
everything except where it is stored.

It also stops being merely tidier once slices carry different kinds of space. A geographic projection
and an embedding sharing one extent either wastes most of the grid for one of them or is wrong for
both. `slices-and-multi-table.md` is where that lands, and this is a prerequisite for it.

## What this costs, stated rather than implied

An extent chosen too small cannot be widened in place, and the data that falls outside it is
unreachable until the slice is rebuilt. That is the trade an index configuration always makes, and it
is the reason the choice belongs at creation time where it is deliberate.

**The narrower remedy is expressible but not built.** Because the extent is slice-scoped, the
principled fix is to rebuild the one slice whose extent was wrong. No such path exists: `tessera build`
is initial-load only and whole-bundle (flush §11), so the practical remedy today is a new deployment.
What the slice scoping buys is that a per-slice reindex needs no model change when someone writes it.

## ⊘ Not implemented: the extent still lives on the bundle

`Manifest.quantisation` is a single bundle-level extent and `SliceDescriptor` carries only
`{ id, display_name }`. Every consumer reads `bundle.manifest.quantisation`. Moving it is a
contracts §2.2/§2.5 change and a `/v1/meta` wire change, tracked separately.

Nothing in this decision depends on that move having happened: with one slice per bundle, which is
what `tessera build` emits, the bundle-level extent *is* the slice's. The decision is what the extent
means and when it may change, and that is true at either location.

## What changes in the corpus

`flush-and-merge.md` §0 and §11 drop re-quantisation from compaction's list of reorganisations; §6's
"until it re-quantises, which is compaction's shape and is recorded as a compaction obligation"
inverts into the obligation above. `geometry-pinning.md` §5 loses its caveat and §12's
superseded-prefix obligation with it. The ingest refusal in `control.rs` and the quarantine alarm in
`flush.rs` stop naming re-quantisation as a remedy.
