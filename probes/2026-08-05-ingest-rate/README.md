# Ingest rate — 2026-08-05

Raw output of `tessera-bench ingest-rate`, the arm that replaced `ingest-batch`'s throughput
headline. Findings and what they mean:
[`docs/evidence/memos/2026-08-05-ingest-rate.md`](../../docs/evidence/memos/2026-08-05-ingest-rate.md).

**The campaign is a bench arm in the tree, not a script here.** Every figure re-runs from the
current engine rather than from a probe binary that has drifted from it.

```bash
scripts/bench_build_fixtures.sh --scales 250000,2422486 --label-sets categories-subclass
cargo build --release -p tessera-bench          # bench-timing is default-on for this crate

BIN=./target/release/tessera-bench

# A — the rate table, at the server's own defaults (commit window ceiling == max batch rows)
$BIN --repeat 5 --scale 2422486 --run-dir runs/a ingest-rate \
  --density 1,3,8 --bw 1,4,12,24 --submitters 1,2,4,8 \
  --batch 10000 --window 10000 --min-steady-rows 240000

# B — the same shapes with the ceiling raised, so a commit window can actually gather
$BIN --repeat 5 --scale 2422486 --run-dir runs/b ingest-rate \
  --density 3 --bw 4,12,24 --submitters 1,2,4,8 \
  --batch 10000 --window 240000 --min-steady-rows 240000

# C — base-size control: one shape at 250,000 and at 2,422,486
$BIN --repeat 5 --scale 250000,2422486 --run-dir runs/c ingest-rate \
  --density 3 --bw 12 --submitters 1 --batch 10000 --window 10000 --min-steady-rows 240000

# D — batch size, so `record_batch`'s lap can be read as the per-batch cost it is
for bs in 1000 10000 40000; do
  $BIN --repeat 5 --scale 2422486 --run-dir runs/d-$bs ingest-rate \
    --density 3 --bw 12 --submitters 1 --batch $bs --window $bs --min-steady-rows 240000
done
```

The allocator-pressure A/B in the memo's §5 is the same binary built against
`crates/tessera-lifecycle/src/buffer.rs` as it stood before commit `78759b1` (the `Arc<BufferedItem>`
change), which is the only file that differs between the two arms:

```bash
git show 78759b1^:crates/tessera-lifecycle/src/buffer.rs > crates/tessera-lifecycle/src/buffer.rs
cargo build --release -p tessera-bench
# ... rerun campaign A's shapes ... then `git checkout crates/tessera-lifecycle/src/buffer.rs`
```

## Machine

WSL2 on Linux 6.18, 12 cores, 47 GB RAM, NVMe. **Not a latency-certification environment** — the
Phase 0 memo's standing caveat applies to every wall-clock number here. Read the ratios and the
shapes; the absolutes are indicative.

## Reading a cell

One JSONL record per `(density, B/W, submitters)`. The fields that carry the finding:

| field | meaning |
|---|---|
| `rows_per_sec` / `us_per_row` | steady state: min over `--repeat` complete steady regions, submission only |
| `rows_per_sec_with_flush` | the same rows including the publication a deployment also waits for |
| `cold_us_per_row` | cycle 0, at an empty buffer. **Not a throughput** — reported so the ramp is visible |
| `stage_us_per_row` | the 13 `WriteStage` laps, differenced across the measured region |
| `w_observed` | rows per commit-window close — **measured**, since a serial caller pins it at the batch size whatever the ceiling says |
| `bw_observed` | `B / w_observed`: the ratio the `B²/2W` clone term is actually keyed on |
| `entries_per_window` | `wal_appends / wal_closes`. `1.00` means group commit ran and collected nothing |
| `submitters_effective` | callers a cycle could keep busy — `min(submitters, B/W)`, since `B/W` is also batches per cycle |
| `buffered_after_flush` | rows the flush left behind. Non-zero means `B` is not what the cell claims |

## E — the case group commit is actually for

Campaigns A and B submit **maximal** batches, which `tessera-server`'s `config.rs` says outright
gain nothing from grouping: `commit_window_max_items` equals `ingest_max_batch_rows` deliberately,
so one maximal batch is one maximal window. E submits *small* batches, which is what the window is
there to collect.

```bash
$BIN --repeat 5 --scale 2422486 --run-dir runs/e ingest-rate \
  --density 3 --bw 40,240 --submitters 1,2,4,8 \
  --batch 1000 --window 10000 --min-steady-rows 240000
```

`--bw` is in units of `--batch`, so `40` and `240` are the same 40,000 and 240,000 rows buffered
between flushes that campaign A reaches at `--bw 4` and `--bw 24`.

## Files

| | |
|---|---|
| `runs-abcd.txt` | campaigns A–D's console output |
| `a.jsonl` … `e.jsonl` | the emitted cells, one JSON object per line |
| `arc-ab.txt`, `arc-prearc.jsonl`, `arc-postarc.jsonl` | the pre/post-`Arc<BufferedItem>` A/B behind the memo's §5 |

