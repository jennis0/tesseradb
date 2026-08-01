# 0021 — Rust rather than the JVM, and what that costs

**Date:** 2026-07-28 · **Status:** Settled, and explicitly revisitable · **Related:** [0001](0001-rust-build-pipeline.md)

## Decision

The engine is Rust. Lucene is not adopted, and its segment lifecycle is reimplemented.

## What that costs, stated fairly

Lucene solves the segment lifecycle properly, and a subset of it is being rebuilt here:

- `TieredMergePolicy`'s parameter set, and its separation of natural, forced and deletes-driven
  merges.
- `BPReorderingMergePolicy`'s decorator shape, which turns a scheduled Morton re-rank into a
  continuous property of sufficiently large merges.
- `SearcherLifetimeManager` with `SnapshotDeletionPolicy` — which is precisely I11's pinning,
  already hardened.

Estimated cost: **3,000–5,000 lines of subtle concurrent code**, and the bugs will be in
merge-versus-snapshot races rather than anywhere interesting.

## Why the trade is still worth taking

The alternative is adopting a whole runtime to obtain a merge policy. Lucene's query layer, its
codecs and its scoring are all unused here. Against that, the JVM costs memory safety in the gather
loop and adds garbage collection to a sub-millisecond path.

It also forfeits process-level compartment isolation: partitions are child processes, and a
JVM-hosted design would put every compartment inside one address space.

## When to revisit

**This is a genuine trade, not a rout.** It should be revisited if the team turns out to be a JVM
shop with existing Lucene expertise, in which case the arithmetic changes.

## The standing instruction

**Read Lucene's implementations before writing ours.** The design is the right one; only the
language is different, and the failure modes it has already found are the ones this will hit.

## Evidence

Moved here from the retiring implementation plan §2.4, which is where this argument lived.
