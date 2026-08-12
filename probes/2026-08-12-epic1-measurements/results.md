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
  --extent 0,65536,0,65536 --slice s0 --limit 2422486 \
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
- **Nothing but categories.** `scan_rows` accepts a `u8`/`u16`/`u32` code slice and refuses
  anything else, so the row route serves categories alone today (records §6.2 — a rendered number
  waits on 0064's render half). The per-row constants here are 1- and 2-byte columns' and must not
  be carried onto an `i64`.
- **One link, one alignment.** `2026-08-11-scan-constant-sensitivity` measured a 64–68% bimodal
  swing in this repo's scan constants driven by instruction-address alignment alone. No alignment
  flag is set in the workspace, so every constant here is one draw.
- **The coalesce and fold rewrite rates** (records §11 item 7), and **list timing on real skew**
  (item 6's fourth residual). Untouched.
