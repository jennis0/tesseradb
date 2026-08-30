# 0106 — The coordinate path is `f64`, from the input file to the quantiser

**Date:** 2026-08-30 · **Status:** Settled (owner ruling, 2026-08-30)

## The decision

**A coordinate is `f64` from where it is read to where it is quantised**, on both entry points: the
build's Parquet read, the ingest wire, the write-ahead log record, the buffered row and the flush.
The stored form does not change — a position is a 32-bit fixed-point pair against the frame,
interleaved into a Morton code, and no float reaches the bundle.

**Both widths are accepted on input and the narrower is widened.** A whole-world frame is well
served by `f32`, and a corpus emitting it should not have to double the size of its largest columns
to be read; a sub-square frame needs `f64` and the caller supplies it. Precision is a property of
the corpus rather than of the release. This is the rule the build already applied to attribute
columns, in the other direction.

## Why

**A tile-aligned sub-square cannot exist under `f32`.** Over the unit square an `f32` resolves about
2^24 steps per axis against a grid of 2^16 cells — 256 steps per cell at the world frame, but only
`2^(8−k)` at zoom offset *k*. Past roughly offset 8 there is less than one step per cell, so the
**cell** is wrong rather than the residual, and no report can see it.

**And the narrowing was a defect at any frame.** A coordinate of 1000.00002 against a `0..1000`
extent narrows to exactly 1000.0, which the frame's inclusive bound admits — so the wire acked a row
outside the declared extent as inside it. Separately, `tessera_spatial::shape` was already `f64`
while the point path was not, and the asymmetry was measurable: of 17,551 Overture divisions, three
metres-wide polygons held a place that was inside at its source coordinates and outside at its
stored position.

## What it costs — measured, not modelled

**Nothing in memory or on disk.** Input files already stored `double` and the build narrowed at
read, so the pipeline was paying for precision and discarding it. A coordinate exists as a float
only inside one 65,536-row decode batch, so the build's cost is per-worker buffers, not per entity;
the bundle is unchanged; the log grows 8 bytes per ingested row. For a corpus that would otherwise
emit `float32`, the measured input cost is +6.51 B/row for the pair as unit-square values and
+5.07 B/row as degrees.

**It moves stored positions, and that is the change being made.** Removing the narrowing moves about
one point in 330 by exactly one cell on one axis, bounded by the `f32` half-ULP — measured across
both geographic corpora at 13,463,857 and 73,631,092 points. The old path was modelled and confirmed
to reproduce both prior bundles exactly before that difference was measured.

The log version bumps and the log is recreated rather than migrated, per
[0048](0048-no-deployments-exist-so-delete-rather-than-support.md): a field's type changes what every
stored record means.
