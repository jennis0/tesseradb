# 0028 — What the postings build actually requires, and who needs the pair relation

**Date:** 2026-08-01 · **Status:** Settled

## Context

The specification stated: *"The postings are built from a pre-exploded `(entity_id, term_id)` pair
relation, joined as a semi-join. This is a requirement rather than an optimisation."*

No semi-join is performed anywhere. Postings are built by a streaming pipeline that spills banded
pair files, scatters through per-term cursors and encodes bitmaps in parallel. The exploded pair
file was described as an optional build input, off by default.

## Two decisions

### The requirement is the refusal, not the mechanism

What is actually required — and is measured — is that **the array-containment formulation is
refused**: holding terms as a list column per item and testing containment against the presented
set is roughly three orders of magnitude slower in every implementation measured, because
containment operators rebuild a probe structure per row and never hoist the loop-invariant grant
set out of the row loop. It is called out because it is the formulation anybody writes first.

The build works from a pair schema. **How that schema is joined is not specified**, and a semi-join
is one way rather than the way. The build's own hard requirement is different again and belongs
next to it: it must be memory-bounded. The linear in-memory build was killed at 10⁹ items on a
47 GiB machine and survives only as a byte-identity oracle for the streaming one.

### The pair relation is required for conformance, though not for serving

`pairs.parquet` is not merely an optional convenience. It is the **other side of the mask
differential**: the reference oracle derives a viewer's authorised set from the flat pair relation
by direct scan, while the engine derives the same set from compressed postings, and agreement
between the two is the test. Without the file, the independent second implementation has nothing to
work from and I1's coverage disappears.

So: **optional for a serving deployment, required for a conformance run.** Recorded because
"optional, off by default" invites a deployment to omit it and then discover its differential
cannot be run against the bundle it actually ships.

## Evidence

Register row S9. Architecture §6.3 and §15; `crates/tessera-build/src/pipeline.rs` for the eleven
streaming stages; `reference/oracle/mask.py` and `reference/tests/test_differential.py` for the
differential that consumes the pair relation.
