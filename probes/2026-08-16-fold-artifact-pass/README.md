# The fold's artifact pass: which construction, and what it costs

**Date:** 2026-08-16 · **Harness:** `cargo run --release --bin fold_artifact_pass`
· **Host:** WSL2, 47 GB RAM, 12 cores

`annotation-representation.md` §5.0.3 poses the pass as a choice between **riding pass 1** with the
inverted entity→artifacts multimap resident and **per-artifact translation** through a table, and
marks the comparison unmeasured. `artifact-delivery.md` §6 calls it the largest unpriced item left;
§7 says it decides whether `plan_fold`'s ~9–10 GB anonymous peak moves, and that if the pass cannot
be fitted the choice becomes a first-toucher stall of tens of seconds per level or blank annotation
levels after every nightly fold. This is the measurement, and it was run before any Stage 4 code.

## The results

**1. The per-artifact arm needs no table and no scatter, because membership never lives in row
space.** §5.0.3's posing — a `old_row → new_row` table, scatter-built, a second 4 GB mapping beside
`permutation.bin`'s — describes a translation that does not arise. The durable membership form is
entity-canonical (rep §2.4, write-cycle §4.1) and the row form is *derived* from it, so the pass is
`Permutation::project` of an entity-space bitmap through the **new** `permutation.bin` — the file
pass 1 writes anyway. Old rows are never consulted. That is not a cheaper way of doing the posed
work; it is a smaller question, and it is the construction `ArtifactRows::build` already runs at
open.

**2. Projecting per artifact wins on every axis that matters, and threading makes it win on the one
it was losing.** At the design point — 10⁹ rows, 10⁷ artifacts, 100 members each in 4 entity-space
runs, 2% of rows retired:

| construction | threads | seconds | peak RSS | added over the population |
|---|---:|---:|---:|---|
| **project** | 1 | 198.8 | 18.01 GB | +3.5 GB — the output row forms |
| **project** | 8 | **32.8** | 18.05 GB | +3.5 GB — the output row forms |
| ride pass 1 | 1 | 101.3 | 23.76 GB | **+9.2 GB anonymous** — the CSR multimap and the open builders |

The population itself is 14.53 GB resident before either arm starts, and both arms must produce the
same 3.5 GB of row forms, so the whole difference between them is the ride arm's inverted relation:
**+9.2 GB of anonymous memory on top of `plan_fold`'s existing ~9–10 GB peak, which roughly doubles
it.** The project arm's extra bytes are the mapping pass 1 has just written — page cache, evictable,
already in the budget.

**Riding pass 1 is inherently sequential and projecting is not.** One stream, one consumer, in
new-row order; against 10⁷ independent reads of a shared read-only mapping. Eight threads take the
project arm to 32.8 s — **3× faster than the ride arm** at a third of its added memory. Twelve cores
were available; eight was not tuned.

**3. The pass costs minutes at the design point, not the 13–25 s §5.0.3 quotes.** That figure was
bitmap construction alone; the projection's reads are the larger term. Sequentially the pass is
~200 s at 10⁹ rows — inside a nightly window, and far too long to leave to a first toucher, which is
what §5.0.3 already concluded on other grounds. Threaded it is ~33 s.

**4. Cost is linear in rows across three decades**, so `plan_fold` can budget it from the corpus
size and the artifact population without a calibration table:

| entity order | arm | threads | 10⁶ rows | 10⁷ | 10⁸ | 10⁹ |
|---|---|---:|---:|---:|---:|---:|
| morton | project | 1 | 0.12 s | 1.23 s | 12.87 s | 198.8 s |
| morton | project | 8 | — | — | 2.11 s | 32.8 s |
| morton | ride | 1 | 0.04 s | 0.60 s | 7.61 s | 101.3 s |
| issue | project | 1 | 0.23 s | 8.20 s | 113.6 s | — |
| issue | project | 8 | — | — | 19.03 s | — |
| issue | ride | 1 | 0.10 s | 2.91 s | 41.25 s | — |

Artifacts are `rows / 100` throughout.

## The two entity orders bracket the answer, and the pessimistic one is mostly not about the pass

`morton` models the shipped `(signature, morton)` allocation within one signature group: entity rank
tracks Morton rank, so a spatial cluster is a few runs in entity space and its permutation slots are
a neighbourhood. `issue` models entity ids uncorrelated with Morton rank — `permutation.rs`'s own
stated assumption, and what a corpus spread over many signature groups approaches.

The `issue` arm is ~9× dearer, and **most of that is the output rather than the construction**: when
entity rank does not track row rank, a cluster contiguous in entity space projects to *scattered*
rows, and the resulting row forms carry ~100 containers each instead of ~4. Peak RSS in that arm is
8.1 GB against 1.8 GB for the same population — which is
[the residency probe](../2026-08-16-membership-residency/README.md)'s 78 B/container reappearing,
not a property of either construction. Both arms pay it equally.

A real clustering is contiguous in *row* space by construction, so the honest reading is that the
`morton` row is the one to design against and the `issue` row is what a badly-spread signature
population would cost.

## What was measured, and what was not

- **Measured:** wall time for a whole level's row forms, and `VmHWM` around the pass, at
  10⁶–10⁹ rows, both entity orders, both constructions, single- and eight-threaded.
- **Cross-checked:** the two constructions produce **identical bitmaps** for every artifact
  (`--verify`, asserted at 10⁶). Two constructions of one quantity that disagreed would make every
  timing above meaningless.
- **Not measured:** the pass under a concurrent serving load. Every figure here is an otherwise idle
  box, so the threaded row is an upper bound on what a nightly window would see, not a promise.
- **Not measured:** the write side — the entity-space membership extents the new prefix needs, and
  Rule F's retirement of deleted artifacts' files. Those are file copies and deletions against a
  population two orders smaller than the corpus, and they are not the term §7 was worried about.
- **Not a real clustering.** Memberships are synthetic, four runs each, on
  `membership_residency`'s realistic arm.

## Reproducing

```bash
cargo run --release --bin fold_artifact_pass -- --rows 100000000 --entity-order morton
cargo run --release --bin fold_artifact_pass -- --arm project --rows 1000000000 \
    --artifacts 10000000 --entity-order morton --threads 8
cargo run --release --bin fold_artifact_pass -- --rows 1000000 --verify
```

The 10⁹ configurations need ~24 GB of RAM and ~4 GB of scratch disk for `permutation.bin`.
