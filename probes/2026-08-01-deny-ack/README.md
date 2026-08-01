# Deny-ack raw cells

Findings, recommendations and the corrections that follow are in
[`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`](../../docs/evidence/memos/2026-08-01-deny-ack-baseline.md).
This directory is the data.

## Files

| file | what |
|---|---|
| `deny_ack.jsonl` | the main matrix: `{suppress,delete}` × buffered `{0, 10 000, 100 000, 1 000 000}`, 30 timed denies quiescent + 30 contended per cell |
| `deny_ack_overlay_depth.jsonl` | the driver-isolation cell: 2,000 denies (overlay 1 → 2,000) at buffered 0 and 1,000,000, no flood |
| `ledger.jsonl` | the resume ledger for the main run |

## Reproduce

```sh
cargo build --release -p tessera-bench
mkdir -p /tmp/tessera-bench/fixtures/2422486
ln -sfn "$PWD/data/bench-fixtures/2m4" /tmp/tessera-bench/fixtures/2422486/categories-subclass

scripts/bench-slot.sh ./target/release/tessera-bench deny-ack \
  --scale 2422486 --buffered 0,10000,100000,1000000 --denies 30 \
  --ingest-batch 10000 --flood-workers 6 --queue-bound 2 --repeat 1 \
  --run-dir /tmp/tessera-bench/runs/denyack

# the driver-isolation cell (no flood, so the contended phase is inert and the
# WORK_QUEUE_NEVER_SATURATED flag is expected)
scripts/bench-slot.sh ./target/release/tessera-bench deny-ack \
  --scale 2422486 --buffered 0,1000000 --denies 2000 --flood-workers 0 \
  --op suppress --repeat 1 --run-dir /tmp/tessera-bench/runs/denyack-overlay
```

## Reading a cell

Every figure is measured. The fields that carry the findings:

| field | meaning |
|---|---|
| `quiet_ack_p50_ns` | deny ack on an idle executor — the fsync floor |
| `quiet_apply_p50_ns` | **the deny's own apply step, exactly**: the delta in `apply_nanos_total` across one deny, read either side of it while nothing else is executing |
| `busy_ack_p50_ns` / `busy_ack_max_ns` | deny ack while `flood_workers` threads submit ingest continuously — the head-of-line term lifecycle §1.3 bounds |
| `apply_nanos_max_after_flood` | `ExecutorHealth::apply_nanos_max` over the contended phase; dominated by ingest applies, i.e. the quantity Task 7b is told to size the floor from |
| `ingest_queue_full_count` | `SubmitError::QueueFull` on the bounded work lane — **proof the queue was genuinely saturated**, not an assumption |
| `denies_refused_for_load` | must be 0. Anything else is fail-open (SA §4.2, contracts §3.1) and the arm flags `DENY_REFUSED_FOR_LOAD` |
| `never_shed_exercised` | false ⇒ the queue never filled, so `never_shed_holds` is vacuous; flagged `WORK_QUEUE_NEVER_SATURATED` |

`ingest_accepted_count × ingest_batch` is how many rows the flood added during the phase — add it
to `buffered_items` for the *effective* buffer depth, which is what the contended numbers are
actually a function of. Below 100,000 the flood dominates the starting depth, so only the 1 M cells
separate cleanly on that axis.

## Environment

`git_sha` `6204c39` (dirty: the arm itself was uncommitted at run time), release + `bench-timing`,
12 cores / 47 GiB, load average 1.20 at start, bundle resident. The contended phase deliberately
saturates the box; its absolute numbers are loaded-box figures by construction and are not
comparable with the quiescent column as if the two were one experiment.
