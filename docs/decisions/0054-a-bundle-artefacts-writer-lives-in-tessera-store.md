# 0054 — A bundle artefact's writer lives in `tessera-store`

**Date:** 2026-08-06 · **Status:** Settled (owner ruling)

## Context

Compaction's pass 2 re-emits `terms/pairs.parquet` — the `(entity_id, term_id)` relation the I1
mask differential runs against — as a side output of the term sweep. It cannot be carried forward
from the old prefix: it would then disagree with the new base postings about every folded deletion,
which is the one disagreement the differential exists to catch (`compaction.md` §3, pass 2).

The sweep's postings half must live in `tessera-authz`, which owns postings, tiers and the
dictionary. **It cannot reach the Parquet writer.** `PairsParquetWriter` was in `tessera-build`,
and `tessera-build` already depends on `tessera-authz`, so the reverse edge is a cycle cargo
refuses. The sweep therefore hands each term's final entity set to a callback and a caller on the
legal side of the boundary writes the file.

That left the question of *who that caller is*, and the answer was nobody: `compaction.md` §10 puts
the fold's driver in `tessera-engine`, and that crate does not depend on `tessera-build` either.
**As the design stood, nothing in the system could write `pairs.parquet` after a fold.**

## The decision

**A writer for a file `contracts.md` §2 defines lives in `tessera-store`.**

Three moved to make that true: `PairsParquetWriter` from `tessera-build`; `MANIFEST.json` and
`CURRENT`'s writers from `tessera-build` (as `write_manifest_json` / `write_current`); and
`write_segments_manifest` from `tessera-engine`.

## Why

It is where the others already were. `SegmentWriter` (`columns.arrow`, `morton.u32`),
`PermutationWriter`, `RunWriter` and `LocatorWriter` are all in `tessera-store` because they write
bundle bytes, and that is the crate's stated charter. The three that were not each got there by an
accident of history rather than a decision: a build was `pairs.parquet`'s and `MANIFEST.json`'s only
producer until a fold became the second, and `write_segments_manifest` grew where flush, merge,
coalesce and the deny lane call it. None had a reason to be outside.

`tessera-store` is also the crate every producer already depends on — `tessera-build`,
`tessera-engine` and the fold's driver alike — so the rule is the one placement that needs no new
edge in the crate graph.

## What was considered and declined

- **`tessera-engine` depends on `tessera-build`.** Permitted by `scripts/check-layers.sh`, which
  denies neither direction. Declined: it inverts the layering, since `tessera-build` is the offline
  producer that sits *above* the serving core, and it links the whole build pipeline — source
  Parquet ingestion, the tiler, the rayon fan-out, `statvfs` — into the serving binary to reach one
  writer. It also invites a cycle the moment `tessera-build` wants anything from the engine.
- **Drive pass 2 from `tessera-cli`**, which links both. Declined: the CLI is not on the path a
  fold takes. `POST /control/compact` and the automatic trigger at the flush tick are both in the
  server, so the CLI could only regenerate the file in a *separate* pass over the corpus — doubling
  the fold's read cost and reopening the disagreement window that makes carrying it forward unsafe.
- **Skip `pairs.parquet` in a serving deployment.** contracts §2.4 makes it optional to *read*, and
  that was the tempting reading. Declined: pass 2's own argument is that a fold which omits it
  leaves a compacted bundle the conformance suite cannot run against, and the suite is the
  deliverable.

## What it obliges

- `tessera-store` gains a `parquet` dependency. It already had `arrow`; nothing excluded `parquet`
  deliberately.
- The sweep keeps its callback shape. `tessera-authz` may not depend on `tessera-store` either
  (`check-layers.sh` denies it), so the sweep yields `(term_id, &Bitmap)` and a caller drives the
  writer — via `PairsParquetWriter::push_iter`, which takes an iterator so the sweep never
  materialises a term's entities as a `Vec<u32>` (a measured 125.12 MB as portable Roaring against
  2 GB as `u32`s at 10⁹, per term).
- Whoever adds the next bundle artefact follows the rule rather than rediscovering it. The
  discrepancy that produced this ruling was found because a doc claimed six of seven writers were
  already in `tessera-store` when it was five, and the claim was made without checking.
