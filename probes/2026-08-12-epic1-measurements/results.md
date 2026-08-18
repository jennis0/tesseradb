# Epic 1's owed measurements: the record blob's ratio, and the built row route's constants

**Date:** 2026-08-12 · **Machine:** WSL2 on Linux 6.18, AMD Ryzen 9 5900X (12 cores, 32 MiB L3),
47 GB RAM · **Harness:** `crates/tessera-bench/src/bin/record_blob_ratio.rs`,
`crates/tessera-bench/src/bin/row_route_cost.rs` — both measure the **shipped** writer, reader and
route, not a transcription of them.

The memo reading against this is
[`2026-08-12-records-and-search-epic-1-measurements.md`](../../docs/evidence/memos/2026-08-12-records-and-search-epic-1-measurements.md).
It carries the conclusions and the re-markings `records-and-search.md` owes; this file is the raw
campaign — what was run, over what, and where the output is.

## Files

| | |
|---|---|
| [`schema-render.toml`](schema-render.toml) | `data/scaled/attrs/schema.toml` restated on the epic's `render` / `index` surface. The original still writes `used_for = [...]` and no longer parses |
| [`schema-render-and-index.toml`](schema-render-and-index.toml) | the same, plus `index = true` — the "affords both routes" case 0068's rule needs to have anything to choose between |
| [`build_scaled_attrs.py`](build_scaled_attrs.py) | carries each real paper's attributes to the replicas that are affine transforms of its geometry, so a scale above 2,422,486 can be built without inventing a value. **Timing fixtures only** — the distinct-value count stays the real corpus's, so no storage or vocabulary claim may be read off one |
| [`run-record-blob.txt`](run-record-blob.txt) | three row shapes through `RecordBlobWriter`, 2,400,000 real arXiv records |
| [`run-row-route-2422486.txt`](run-row-route-2422486.txt) | viewport / coarse / routes at the real corpus |
| [`run-row-route-25000000.txt`](run-row-route-25000000.txt) | viewport / coarse at 25M |
| [`run-row-route-100000000.txt`](run-row-route-100000000.txt) | viewport / coarse / routes at 10⁸ — the probe's own scale |

## Reproducing

```bash
cargo build --release -p tessera-bench -p tessera-cli

# The blob. Needs only the snapshot; writes to a temp dir and cleans up.
./target/release/record_blob_ratio \
  --snapshot ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json \
  --limit 2400000 --reads 2000

# The row route. The 2.4M fixtures build from the existing attributed points file.
./target/release/tessera build \
  --points data/scaled/attrs/points.parquet \
  --pairs data/scaled/pairs/categories-subclass.pairs.parquet \
  --schema probes/2026-08-12-epic1-measurements/schema-render.toml \
  --values archive=data/scaled/attrs/archive.parquet \
  --values primary_category=data/scaled/attrs/primary_category.parquet \
  --out /tmp/tessera-bench/fixtures/2422486/attrs-subclass \
  --extent 0,65536,0,65536 --view s0 --limit 2422486 \
  --mint-external-ids --id-key 000102030405060708090a0b0c0d0e0f --idset 1
# ... and again with schema-render-and-index.toml into .../attrs-both

# Above the real corpus, the points file has to be made first (~50 s at 25M, ~3 min at 10^8;
# the whole 10 GB geometry is walked either way, since it is sorted by morton, not entity id).
reference/.venv/bin/python probes/2026-08-12-epic1-measurements/build_scaled_attrs.py \
  --limit 100000000 --out /tmp/points-100m.parquet
# then `tessera build --points /tmp/points-100m.parquet ... --limit 100000000`

./target/release/row_route_cost \
  --fixture /tmp/tessera-bench/fixtures/100000000/attrs-subclass \
  --both    /tmp/tessera-bench/fixtures/100000000/attrs-both --repeat 3
```

Build times observed: 5.3 s at 2.4M, 74 s at 25M, 4m36 at 10⁸ render-only, ~9 min at 10⁸ with the
entity-space index as well.

## What is not here

- **No 10⁹ run.** Buildable on this machine — under two hours end to end, ~50 GB of the 96 GB free
  — and not built. Every 10⁹ figure in the memo is marked *modelled* with the 25M→10⁸ flatness as
  its basis.
- **Nothing but categories.** `scan_rows` accepts a `u8`/`u16`/`u32` code view and refuses
  anything else, so the row route serves categories alone today (records §6.2 — a rendered number
  waits on 0064's render half). The per-row constants here are 1- and 2-byte columns' and must not
  be carried onto an `i64`.
- **One link, one alignment.** `2026-08-11-scan-constant-sensitivity` measured a 64–68% bimodal
  swing in this repo's scan constants driven by instruction-address alignment alone. No alignment
  flag is set in the workspace, so every constant here is one draw.
- **The coalesce and fold rewrite rates** (records §11 item 7), and **list timing on real skew**
  (item 6's fourth residual). Untouched.

---

## Follow-up, same day: hoisting the dispatch out of the row loop

The campaign above found the built route 4.8× above the probe and named no cause. The cause was
`scan_rows`' inner loop, which resolved the segment, matched the `CodeView` width and matched the
`RowPredicate` **per row** — roughly four branches and two bounds checks per row, none hoistable,
and no vectorisable compare anywhere in it. The tell was in this campaign's own data: the constant
was *insensitive to the code width*, which a loop bound by moving one or two bytes per row could
not be.

Deciding the width and the predicate once per contiguous run inside one segment, leaving a
monomorphic compare over a slice (`scan_run`/`run_matching`):

| shape, single-threaded | before | after | ratio |
|---|---|---|---|
| 343,391-row viewport, 10⁸ | 2.462 ns/row (0.845 ms) | 0.376 ns/row (0.129 ms) | **6.5×** |
| whole view, 10⁸, selective | 2.390 ns/row (238.95 ms) | 0.220 ns/row (22.02 ms) | **10.9×** |
| whole view, 10⁸, broad | 2.532 ns/row (253.15 ms) | 0.299 ns/row (29.87 ms) | **8.5×** |
| whole view, 10⁸, 12 threads | 33–38 ms | **3.2–5.3 ms** | 7.2–10.1× |

*A/B in one session on one binary pair, `git stash` between them, same fixtures, `--repeat 3`. Raw
for the "after" column: `run-row-route-100000000-hoisted.txt`, and `-2422486-`/`-25000000-` for the
other scales — the last row is the coarse block's four cells (both columns, both values), paired
against the same four before, and none is dropped; the viewport block's own whole-view cells at
12 threads run 3.55–5.67 ms. **⊘ The "before" run's output was not saved**, so only that row's is
reproducible (`run-row-route-100000000.txt`'s coarse block, 32.9–38.5 ms). The other three cells are
this session's own: the earlier campaign measures the same pre-hoist code on the same fixtures at
2.70 ns/row (0.927 ms), 2.804 (280.4 ms) and 2.993 (299.3 ms), which would make the ratios 7.2×,
13.1× and 10.5×. The order is the same either way; treat the first three "before" cells as this
session's and not as citable figures.*

**The constant is now 0.22–0.46 ns per row for domains of ~3×10⁵ rows and up**, across 2.4M, 25M
and 10⁸ — at or below the standalone probe's own 0.48–0.73, which is what the diagnosis predicted:
the probe measured a monomorphic compare and the engine had been running a polymorphic one. The
invariance in corpus and viewport size is unchanged **above that bound and not below it**: the
2.4M fixture's 139,920-row viewport measures 0.62–0.66 ns/row and its 3,504-row one 15.3–15.9,
which is the fixed floor below, not a different constant.

Two consequences worth stating. The **coarse-zoom whole-view cell now costs 21–29 ms
single-threaded at 10⁸** against 280–299 ms before, so it is inside the 100 ms interaction target
*without* the tile sweep's parallelism rather than only with it. And the **fixed floor is
unchanged** — a 3,504-row viewport still measures 15.3–15.9 ns/row, bitmap setup amortised over too
few rows — so the small-viewport observation in this campaign's §2 stands as written.

Not measured here: whether the compare vectorises (the diagnosis says the loop can now be
vectorised; whether LLVM does it under a data-dependent push is unchecked), and the `i64` case,
which no column reaches until 0064's render half lands.
