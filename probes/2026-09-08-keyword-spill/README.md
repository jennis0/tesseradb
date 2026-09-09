# What an indexed keyword column's dictionary costs when it is built externally

**Date** 2026-09-08. **Branch** `fix/keyword-ordinal-sort`, against main at 7cd8c8c8. **Box** WSL2,
12 cores, 47 GB, local NVMe-backed VHDX. **Corpora** synthetic, written by `make_corpus.py`: one
view, one indexed `keyword` column, and nothing else declared, so the build's `filter_postings`
stage is the keyword emit and nothing shares it.

Measures the change from building a keyword column's dictionary whole in memory — a `&str` per
present row, cloned, sorted, deduplicated, then a binary search per row — to the chunk, spill,
merge and scatter the text index already uses. Every figure below is **measured** — three runs of
each shape at 2×10⁷ rows, and one run of the largest shape at 6×10⁷ — and the two binaries are the
same commit apart from that pass.

    export CARGO_TARGET_DIR=<a target dir of this worktree>
    git stash && cargo build --release --bin tessera && cp target/release/tessera /tmp/before
    git stash pop && cargo build --release --bin tessera && cp target/release/tessera /tmp/after
    BEFORE=/tmp/before AFTER=/tmp/after WORK=/tmp/kw ROWS=20000000 REPEATS=3 \
        bash probes/2026-09-08-keyword-spill/run.sh distinct many few

## The three shapes

Cardinality is what the superseded binary search's cost followed, so the shapes are the cardinality
extremes rung 5 (TreeOfLife, 2.33×10⁸ rows) actually carries:

| shape | keys over 2×10⁷ rows | the rung 5 column it stands for |
|---|---|---|
| `distinct` | 20,000,000, every row its own 36-character uuid | `uuid` |
| `many` | 218,380 binomials, Zipf-distributed | `scientific_name` (2.4×10⁵ keys) |
| `few` | 24 short codes | a code column of a few dozen values |

## Wall time

`filter_postings`, the stage that writes the value column, its presence bitmap and its dictionary.
Three runs each; the range is across the three.

| shape | before | after | |
|---|---|---|---|
| `distinct` | 63.66, 69.35, 72.24 s — 2.8×10⁵ rows/s | 9.69, 10.14, 10.28 s — 2.0×10⁶ rows/s | **6.8× faster** |
| `many` | 7.80, 7.92, 7.94 s — 2.5×10⁶ rows/s | 4.81, 4.82, 4.90 s — 4.1×10⁶ rows/s | **1.6× faster** |
| `few` | 2.75, 2.81, 2.83 s — 7.1×10⁶ rows/s | 2.91, 2.92, 2.93 s — 6.9×10⁶ rows/s | ⊘ **4–6% slower** |

**The `few` shape is a regression, and it is outside the run-to-run spread**: 0.08 s across the
three `before` runs, 0.02 s across the three `after` runs, and 0.10 to 0.18 s between them. It is
0.13 s over 2×10⁷ rows, which scales to about 1.5 s over rung 5.

What it buys is the spill and the merge on a column that did not need them: 24 keys is a key set
that stays in cache, so the search this pass replaced was five probes into it, and what replaces
them is 2×10⁷ rows written to a run file as varints and read back through a merge that has one run
to merge. The remedy, not taken here, is to skip the run file where the chunk pass produced exactly
one chunk: the dictionary and the ordinals would then be read straight out of the sorted buffer,
through the same guards and the same writer. That is the common case for any column whose rows fit
the plan — 5.9×10<sup>7</sup> rows at the 2 GiB allowance an auto-derived budget on this box gives
— so it would also take the merge off the `many` and `distinct` shapes below that size.

## Memory, and the same shape at 6×10⁷ rows

⊘ **Not distinguishable at 2×10⁷ rows.** The stage record's `peak_rss_kib` is `VmHWM`, the whole
process's high-water mark, and in both binaries an earlier stage sets it: 1.99 GiB for `distinct`,
1.69 for `many`, 1.44 for `few`, before and after, to within 0.01 GiB. The superseded path held
32 B per present row — 640 MB here — which is under that mark.

**At 6×10⁷ rows it is, and the pass owns the build's peak in both.** One run each of the `distinct`
shape, `ROWS=60000000`, same box, same corpus, output byte-identical:

| | `filter_postings` | build's peak at that stage | high-water set before it |
|---|---|---|---|
| before | 250.59 s — 2.4×10⁵ rows/s | 5.89 GiB | 3.61 GiB |
| after | 35.83 s — 1.7×10⁶ rows/s | 4.74 GiB | 3.61 GiB |

7.0× faster, and the pass adds 1.13 GiB to the build's high-water where it added 2.28 GiB. Of that
2.28, 1.79 GiB is the 32 B per row the superseded path held and the rest is the emit's own chunk
buffer and allocator slack; all of it grows with the corpus. The 1.13 is the plan's chunk plus the
mapped ordinals, and stops growing where the chunk does.

What changes at any size is the bound rather than the figure:

| | resident, anonymous | file-backed |
|---|---|---|
| before | 32 B × present rows, unbounded | — |
| after | 36 B × min(present rows, the plan's chunk) | 4 B × present rows |

The plan is `KeywordDictPlan` (`crates/tessera-build/src/pipeline.rs`): a sixteenth of the build's
memory budget, clamped to 128 MiB and 2 GiB, which is the share and the clamp the text pass takes. The file-backed 4 B is the
`MappedArray` the merge scatters ordinals into, which is page cache the kernel may evict rather
than memory the machine must have. A column smaller than one chunk therefore holds slightly *more*
than it used to; a column larger than one chunk holds a constant where it used to hold the corpus.

## The output is the same bundle

Byte-identical, both binaries, all three shapes at 2×10⁷ rows and the `distinct` shape at 6×10⁷:
15 files per bundle, and the only two that differ are `MANIFEST.json` — in `created_at` alone,
checked field by field — and `CURRENT`. This is the comparison `docs/ingest-campaign.md` §4c uses.

## One construction tried and rejected

Holding the key in a side array the sorted elements index into, so that a swap moves 12 bytes
rather than 28. Measured on the `distinct` shape at 2×10⁷ rows: **13.45 s against 10.12 s**. The
sorted array's order bears no relation to the side array's, so every key read after the sort — the
group scan and the row gather, one per row — is a random access into 320 MB of references, and that
costs more than the halved swap saves.

## What is not measured here

- **Rung 5 itself.** The 2.33×10⁸-row figures in the pass's own doc comment (42 MiB/min of output,
  1.75×10⁵ rows/s, an 8.0 GB arena, a 7.6 GB dictionary) are from the campaign's build and are not
  re-measured here; the shapes above are that column's cardinality at a row count that runs in
  minutes.
- **A cascade.** 2×10⁷ rows is four chunks at most under an auto-derived budget, well inside the
  128-run fan-in. The cascade is exercised by
  `pipeline::tests::chunking_the_keyword_column_does_not_change_its_bytes`, at a chunk of one row.
- **The box was not empty.** A rung 5 build belonging to another session ran throughout. The three
  runs of each shape were taken back to back and alternate between the two binaries, so the
  contention is shared; the `few` shape's 4–6% is the figure that is closest to that noise, which
  is why its spread is reported beside it.
