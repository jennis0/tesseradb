# What pass one holds, and why it was twice the thing it produces

**Date** 2026-09-10. **Branch** `perf/dictionary-memory`, against main at cc0977bc. **Box** WSL2,
AMD Ryzen 9 5900X, 12 cores, 47 GiB, local NVMe-backed VHDX, 12 GiB of swap. **Corpus** prefixes of
`data/ladder/gbif`'s `points.parquet` — 3,495,729,729 placed GBIF occurrences whose `entity_id`
ascends with the file, so `--limit L` is its first `L` rows and the row groups past `L` are pruned
by their own statistics.

Rung 6 was OOM-killed twice. Its second run reported `peak= 45943 MiB` at the end of `source_ids`,
the first stage, and never moved off that figure. This measures what pass one actually asks for,
removes the larger half of it, and moves the rest off the heap.

    export CARGO_TARGET_DIR=<a target dir of this worktree>
    git stash && cargo build --release --bin tessera && cp target/release/tessera /tmp/before
    git stash pop && cargo build --release --bin tessera && cp target/release/tessera /tmp/after
    BEFORE=/tmp/before AFTER=/tmp/after \
      PREFIX_CORPUS=data/ladder/gbif IDENTITY_CORPUS=data/ladder/gbif-64p \
      WORK=/tmp/sim LIMITS="20000000 200000000 1000000000" \
      bash probes/2026-09-10-source-ids-memory/run.sh

Every figure is **measured** unless it says otherwise.

## The result

`source_ids` asked the machine for **16 bytes a row** where the thing it produces is 8, and all of
it was anonymous memory. It now asks for a flat ~230 MiB whatever the row count, and the 8 bytes a
row are a file under `.build-tmp/`.

One alternating run, before/after at each row count, box at load 0.57 with 45.0 GiB available. "max
anonymous" is the largest `RssAnon` a 200 ms sampler saw, which is the figure that decides whether
a machine can hold the build; "the ids" is `8n`, the array the stage exists to produce.

| rows | | `source_ids` | peak `VmHWM` | max anonymous | the ids |
|---|---|---|---|---|---|
| 2×10⁷ | before | 1.24 s | 338 MiB | 293 MiB | 153 MiB |
| | after | 1.15 s | 346 MiB | **213 MiB** | 153 MiB |
| 2×10⁸ | before | 12.33 s | 3,108 MiB | 3,033 MiB | 1,526 MiB |
| | after | 10.58 s | **1,654 MiB** | **223 MiB** | 1,526 MiB |
| 10⁹ | before | 61.74 s | 15,303 MiB | 15,156 MiB | 7,629 MiB |
| | after | 54.72 s | **7,740 MiB** | **228 MiB** | 7,629 MiB |

The before column fits `16n + ~200 MiB` at every row count. The after column's anonymous figure is
flat across a fiftyfold range of `n`: 213, 223, 228 MiB. Its `VmHWM` is the mapped array's pages,
which are page cache the kernel may write back and evict.

**At 2×10⁷ the peak is not the ids at all.** 338 MiB against 153 MiB of array: the points decode
holds ~200 MiB whatever the corpus (below), and it is what both columns' first row is measuring.

An earlier run of the same six builds, on a box at load 2.8, agreed to within 2% on every figure
but the two `source_ids` wall times at 10⁹ (58.85 s and 55.86 s there).

The stage is not slower. Three pairs: 1.24 → 1.15 s, 12.33 → 10.58 s, 61.74 → 54.72 s. ⊘ No claim
is made for the difference beyond "not slower". The 8 GB `memcpy` and the `Vec::dedup` writes that
went away are the plausible source of it and were not isolated, and the box carried other sessions'
work throughout.

## What the 45,943 MiB was

`16n + ~200 MiB` at 3,495,729,729 rows is **53,340 MiB — 52.1 GiB — on a 47 GiB machine**
(modelled: the fit above, extrapolated). So the kill log's 45,943 MiB is what the box could give,
not what the build asked for: the rest went to swap and the process was killed when the next
allocation could not be met. The stage completed both times, which is why the peak appeared to
"arrive in the first stage and stay" — 26.0 GiB of it genuinely stayed, and the other 26.0 GiB was
the transient that reached the machine's ceiling on the way.

Measured at 10⁹ rows with temporary `eprintln`s reading `/proc/self/status` at seven points inside
`read_source_ids_union`, removed before the change was committed. MiB.

| point in the stage | RSS | anonymous | `VmHWM` |
|---|---|---|---|
| entry | 39 | 26 | 39 |
| the counting pass has run | 85 | 72 | **240** |
| the ids are read | 7,685 | 7,672 | 7,729 |
| `union.extend_from_slice(&ids)` | 15,315 | 15,302 | **15,315** |
| the view's own vector is dropped | 7,686 | 7,672 | 15,314 |
| sorted and deduplicated | 7,686 | 7,672 | 15,314 |

Three terms, and only one of them is the answer the stage was asked for.

1. **The array, 8 B/item.** 7,629 MiB at 10⁹, 26,670 MiB at rung 6. Held from pass one to the layer
   publication, because every later pass resolves a source id to an ordinal against it.
2. **A second copy of it, 8 B/item**, for the length of one `memcpy`. Each view was read into a
   vector of its own and then concatenated into the union, so the peak was `8n` for the union plus
   `8n` for the view whose ids were being copied into it — with one view, which is what rung 6 has,
   that is the whole corpus twice.
3. **The points decode, ~200 MiB, constant.** Six decode workers, each holding a 10⁶-row row group's
   projected columns plus a bounded channel of 65,536-row batches. It is a function of the file's
   row-group size and the worker count, not of `n`, and it is what sets the peak below about 10⁸
   rows. Nothing models it: `residency::SLACK` is 64 MiB and stands in for this and everything like
   it.

## What was changed

Two changes, both in `read_source_ids_union` (`crates/tessera-build/src/pipeline.rs`), and one
consequence in the model.

### 1. The union is one array, filled segment by segment

Every view is counted before any is read, the array is allocated once at the total, and each view's
ids are read straight into their own segment of it and sorted and duplicate-checked there. The
concatenation is gone and with it term 2 above.

`read_source_ids` split into `count_source_ids` and `read_source_ids_into`, which writes into a slot
of exactly the length the counting pass said. A scan that does not fill it exactly is now
`input_changed` rather than a short or a reallocated array: the count decides where the *next*
view's ids start, so a points file rewritten between the two passes would give every item after it
another item's ordinal. Nothing checked that before.

The per-view anchor (`rows` and the `mix64` sum) is accumulated during the fill rather than folded
over the finished vector afterwards, which is the same sum over the same values.

### 2. The array is a file under `.build-tmp/`

`SourceIds` is a `MappedArray<u64>` and a length, dereferencing to the slice, so every consumer
still takes `&[u64]` and none of them knows where the bytes are. `entity_of_ordinal`, the declared
columns, the member table and the text index's runs each went this way before it, and the argument
is `MappedArray`'s own: the same bytes as page cache mean a
smaller machine gets slower rather than OOM-killed, and a larger one is no worse off because the
pages stay resident anyway.

Every pass but one reads the ids sequentially: the join's merge sweep (`join_chunk`), the
external-id write, the ordinal walks. The exception is `layers::publish`'s binary search on the
sparse path, which is the random-access case the mapping was written for.

`TmpDir::create` moved from stage 3 to before stage 1, `.build-tmp/` being where the array lives.

**The disk cost is `8n`, exactly.** Measured at `gbif-64p`: `source-ids.u64` is 206,768,056 bytes
over 25,846,007 items. 26,670 MiB at rung 6 (modelled), against 473 GB free on this box. It is
unlinked at the layer publication, which is where the build already released the ids.

### 3. `residency.rs` charges the ids to the disk

The model's `dense` flag is gone with the term it qualified. It existed so that a contiguous id
range charged the larger of the id vector and the publication's Roaring rather than both, the two
being the largest anonymous terms and exclusive on that path; the ids are not an anonymous term any
more, so the exclusivity has nothing to arbitrate. `plan_build` lost the parameter and the
`(first, last, len)` test that computed it.

At rung 6 (modelled, the model being arithmetic over `n`, the schema and two footer figures):

| | before | after |
|---|---|---|
| charged against `--memory-budget` | 26,734 MiB | **13,399 MiB** |
| the largest charged term | the sorted source ids, 26,670 MiB | the publication's Roaring, 13,335 MiB |
| reported as disk | — | **+26,670 MiB** |

The refusal's breakdown, at `--limit 20000000 --memory-budget 1g` against the rung-6 member file, is
the same total on both binaries — 13,399 MiB, the ids being the smaller of the two terms at that
`n` — and prints the ids as `152 MiB (mapped) the sorted source ids, 8 B/item, in .build-tmp/`
where it printed them as the charged half of a "larger of" term before.

The disk pre-flight's spill and band phases gained `8n`; its column phase gets the same figure
through `Residency::mapped()`, the ids being alive from pass one to the layer publication and that
publication being inside the column window.

## The bundle is the same bundle

Byte-identical on four corpora, each built with both binaries and compared file by file
(`docs/ingest-campaign.md` §4c). In every case the only files that differ are `MANIFEST.json`, in
`created_at` alone and checked field by field, and `CURRENT`.

| corpus | items | files | what it covers |
|---|---|---|---|
| `gbif-64p` | 25,846,007 | 32 | one view, contiguous ids, a tiered layer of three levels, `pairs.parquet` written |
| `geonames` | 13,463,857 | 70 | two tiered layers over two member files, eight vocabularies, thirteen declared columns |
| `treeoflife-1m` | 1,000,000 | 61 | two views, the second of them sparse, a tiered taxonomy |
| `multiview` | 21,300 | 111 | **ten views over one entity space with partial overlap, ids sparse over a 13.5×10⁶ span** — the union's deduplication and `layers::publish`'s binary search into the mapped array |

`multiview` is the case the change is riskiest for and the one no larger corpus on the ladder
exercises: every other points file on the ladder numbers its rows from zero.

`cargo test --workspace --no-fail-fast`: 2,825 passed, 0 failed, 23 ignored.

## What is left, and what is not measured

⊘ **Rung 6 was not run.** Nothing here was measured above 10⁹ rows, and every rung-6 figure in this
document is the fit or the model extrapolated. What the fit says is that pass one's anonymous
demand at 3,495,729,729 rows falls from 53,340 MiB to about 230 MiB and its disk demand rises by
26,670 MiB.

⊘ **The writeback was not measured.** The mapped array is fully dirtied by the sort, so 26,670 MiB
of it has to reach the disk at rung 6. The prefix runs above were stopped at the end of the stage,
before any of that was forced, so what the writeback costs a whole build — which has half an hour
of later stages to absorb it in the background — is unknown. The four whole-corpus builds in the
identity table are far too small to show it.

⊘ **Nothing was measured under memory pressure.** Every run had 44-45 GiB available, so the mapped
pages stayed resident and the eviction the change is *for* never happened. That a smaller machine
gets slower rather than killed is the argument `MappedArray`'s doc comment makes, not a measurement
taken here.

⊘ **`distinct_of_ordinal` is now the build's largest anonymous structure**, at 4 B/item — 13,335 MiB
at rung 6, allocated in the pairs pass and held to the assignment. Unlike the source ids it *is*
already charged: `plan_build`'s `loop_fixed` carries `4 * n`, so a budget it does not fit under
refuses rather than dies. Mapping it as well is a five-line change and was not taken here, because
`loop_fixed` feeds `auto_batch` and the batch size is identity-bearing (I9, §11.1 r23) — removing
`4 * n` from it would give a budget-constrained corpus a different permanent entity-id assignment.
That is an owner's call, not a performance one.

⊘ **The points decode window is unmodelled.** ~200 MiB at this corpus's 10⁶-row row groups and six
workers, measured at every row count above; `residency::SLACK` is 64 MiB. It is a constant rather
than a term that scales, so it is a floor the model reads low by about 140 MiB and not a reason a
build dies.

⊘ **The contiguous case still pays for the array.** A points file numbered from zero — which is
every corpus on the ladder except `multiview` — has ids that are exactly `first + i`, and the
resolver on that path already reads only the range's first id and its length. Such a build could
carry `(first, len)` and no array at all: no 26,670 MiB of disk, no page cache, and no sort. What
stands in the way is that contiguity is not known until the ids are sorted, so establishing it
without the array means a distinctness test of its own — a presence bitmap over the id range at
`n/8` bytes is exact and cheap where the range is bounded, and the mixed anchor already computed is
not, being a hash. Left for the owner: it removes a disk cost, and the memory cost it would remove
is the one that has already gone.
