# `Permutation::project` — where the seconds went, and what removed them

**Result: 8 267 ms → 1 277 ms** single-threaded at 10⁹ over a 25% grant (**6.5×**), and 795 → 131 ms
at 10⁸ (6.1×). The form this replaces was rayon-parallel and reached **3 024 ms on twelve threads**,
so the serial rewrite wins wall clock by 2.4× as well, on one core instead of twelve.

Re-run: `cargo run --release --example project_decomposition -p tessera-store -- [--entities N]
[--grant F] [--shape scattered|identity] [--reps N]`. Raw output in `1e9-scattered.txt` and
`1e8-scattered.txt`; the harness is `crates/tessera-store/examples/project_decomposition.rs`.

## Why the existing figures were not enough to act on

The corpus quoted **10.7 s** for a cold projection at 10⁹ (`2026-07-30-1e9-rebuild/`) and **4 550 ms**
for the primitive (`2026-08-04-refresh-ladder/`). Neither said which stage the time was in, and the
second was measured over a permutation `refresh_probe` builds as the **identity** map — entity `e` at
row `e`. A build orders rows by `(morton, tessera_id)`, which is uncorrelated with entity-issue
order, so a real slot array scatters; under the identity map the gathered rows come out already
sorted and `par_sort_unstable` is a run-detecting pdqsort that charges almost nothing for that. The
ladder probe's rebuild figure was therefore an underestimate of the one stage that mattered.

This probe builds a scattered bijection instead (a cycle-walked Feistel, so it needs no materialised
shuffle) and reproduces the corpus's 125.12 MB projection size exactly, which is what licenses
comparing its baseline against the 10.7 s figure.

## Everything is single-threaded work, not wall clock

Under concurrent sessions the machine is already saturated, so spreading one projection across cores
buys throughput nothing. Two candidates are recorded below **because they fail that test** — they
looked like wins on wall clock and remove no work at all.

## The four stages as they were, at 10⁹

| stage | | ms |
|---|---|---:|
| S1 | decode the mask to a `Vec<u32>` | 468 |
| S2 | look up each entity's row | 1 576 |
| S3 | **sort the row array** | **5 272** |
| S4 | build the bitmap (`add_many`) | 976 |
| | **total** | **8 267** |

The lookup is near its floor: entities arrive ascending, so it is a sequential scan of the 4 GB slot
array, not random access. The scatter is entirely in the *output*, and the sort is the price of it.

## Candidates

| | replaces | 10⁸ | 10⁹ |
|---|---|---:|---:|
| A — range-split the mask, seek each range | S1+S2 | 0.70–0.92× | 1.07× |
| C — build the bitmap across cores | S4 | 1.01× | 1.03× |
| B — partition on the container key | S3+S4 | 3.22× | 2.06× |
| E — fuse decode, lookup and partition | all four | 2.88× | 3.59× |
| F — E, emitting containers directly | all four | 6.21× | 6.73× |
| **landed `Permutation::project`** | all four | **6.09×** | **6.47×** |

**A and C are the negative results.** C is pure parallelism and removes nothing — 1.03× is the
measurement saying so. A replaces croaring's bulk `read_many` with per-element cursor stepping and is
never reliably better, while also giving up the sequential walk of the slot array.

**B gets worse with scale, and that is the finding.** Partitioning on the high 16 bits looks like the
natural unit because it *is* the Roaring container key — but at 10⁹ that is 15 259 live write
cursors, far more than the cache holds, and the advantage falls from 3.22× to 2.06× between the two
scales. E and F bucket by **row range** instead, 239 buckets at 10⁹, sized so the bit array each one
stamps stays in L2 while the cursors stay in L1. Measuring only at 10⁸ would have chosen wrong.

**The packer is the larger half.** E and F share a first pass and differ only in the tail: E expands
each bucket's stamped bits back to `u32` for `add_many`, F hands the words to
`tessera_roaring::Sink` as the container payloads they already are. That is 2 306 ms against 1 229 ms.

## Density: this is a wide-grant optimisation

At 10⁹, comparing the landed route against the old stages:

| grant | old total | landed |
|---|---:|---:|
| 25% | 8 267 ms | 6.5× |
| 1% | 354 ms | 2.2× |
| 0.1% | 35 ms | ~1× |

The gain tracks the cost: it is largest exactly where the operation is expensive, and vanishes where
the whole thing is already 35 ms. No fallback route is warranted for the sparse case — a second path
would be a second thing to audit for a quantity that does not matter.

## `madvise` on the slot mapping: measured, and not worth taking

`permutation.bin` was the only large mapping in the tree carrying no advice, which looked like an
oversight. Walking all 4 GB at 10⁹:

| | walk |
|---|---:|
| none (as built) | 25.2 ms |
| `MADV_WILLNEED` | 24.1 ms |
| `MADV_HUGEPAGE` | 23.6 ms |
| `MADV_SEQUENTIAL` | 23.7 ms |

An earlier reading of this showed 3.5×, which was the **first arm paying to pull the file into page
cache** — the arms are ordered, and re-run warm the spread is ~20% at 10⁸ and ~6% here. Nothing to
take. **The useful finding is the negative one:** `MADV_SEQUENTIAL`, which the rest of the tree uses
for morton codes, columns and dictionaries ([decision 0052](../../docs/decisions/)), would be
actively wrong here. Its drop-behind frees each page just after it is read, which is right for a
one-shot streaming pass and backwards for a table every session walks again.

## Two measurement traps, recorded because both nearly produced wrong conclusions

**A truncated fixture reads as a valid one.** `PermutationWriter` sizes the file up front and then
*scatters* into it, so a run interrupted mid-write leaves a full-length file whose unwritten slots
hold the row-absent sentinel — indistinguishable by any check on length or existence. One 10⁹ run
was measured against such a file: 43% of the grant projected to nothing, every stage downstream of
the gather was quietly sized against a smaller row set, and the ratios still looked plausible. The
harness now writes to a sibling and renames, and asserts the projection's cardinality equals the
grant's.

**Position in the run is worth ~25%.** An arm measured after several others has ~8 GB of allocator
churn behind it and lands consistently slower: the landed projection measured 6.5× second in the
sequence and 5.1× last, with no code change between. The harness now measures the production path
directly after the baseline, and the research arms afterwards. Any figure here compared against
another should come from the same position, or from separate runs.

## What landed

- `tessera-roaring` — a leaf crate holding the portable-format container assembler, lifted out of
  `tessera-filter` where it was `pub(crate)`. `check-layers.sh` denies filter → store, and store must
  not acquire a dependency on the query-side filter index to reach a byte encoder, so it sits below
  both. `Sink::push_block` needed `#[inline]`: both callers had it in-crate before, and the workspace
  builds release with neither LTO nor a single codegen unit.
- `Permutation::project` — the one-pass form. §10.4's "gather, radix sort, bulk-construct" is
  satisfied by a partition rather than a sort, and the bulk construction is the container emission.
- `permutation_project_parallel.rs` — the `--ignored` gate that decided whether to keep the parallel
  implementation is deleted, its question being settled. Added: a case spanning more than one bucket,
  which every existing case missed (they use a few thousand entities against a 2²²-row bucket, so all
  of them exercise bucket zero and never the bit array's reuse).
