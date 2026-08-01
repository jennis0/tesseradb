# Deny-ack latency: the first baseline, and two things the plan gets wrong about it

**Status:** measured 2026-08-01 at `6204c39`, 2,422,486 items / `categories-subclass`, release +
`bench-timing`, 12 cores / 47 GiB, every run under `scripts/bench-slot.sh` on a quiet box. Raw
cells: `probes/2026-08-01-deny-ack/`. New arm: `tessera-bench deny-ack`
(`crates/tessera-bench/src/arms/changes.rs`, `run_deny_ack`).

This is the baseline `2026-07-31-ingest-baseline-pre-task3a.md`'s closing section says does not
exist. It is in the same shape as that memo.

## Results

**1. Quiescent deny-ack is one fsync — ~3.0–3.5 ms — and buffered-item depth does not move it at
all.** Measured, `Suppress` and `Delete`, 30 timed denies per cell:

| op | buffered items | ack p50 | ack p99 | the deny's own apply, p50 | apply as share of ack |
|---|---:|---:|---:|---:|---:|
| delete | 0 | 3.240 ms | 3.566 ms | 1.47 µs | 0.050% |
| delete | 10,000 | 3.263 ms | 3.794 ms | 1.34 µs | 0.048% |
| delete | 100,000 | 2.931 ms | 3.656 ms | 1.31 µs | 0.045% |
| delete | 1,000,000 | 3.246 ms | 4.919 ms | 1.33 µs | 0.050% |
| suppress | 0 | 3.247 ms | 4.543 ms | 1.42 µs | 0.066% |
| suppress | 10,000 | 3.472 ms | 6.750 ms | 2.04 µs | 0.062% |
| suppress | 100,000 | 3.434 ms | 3.944 ms | 1.54 µs | 0.053% |
| suppress | 1,000,000 | 3.055 ms | 3.920 ms | 1.30 µs | 0.059% |

A hundred-thousand-fold change in buffer depth moves the deny's own apply by nothing measurable.
The ack is the fsync, consistent with the ingest baseline's ~3.2 ms floor and with the existing
`changes` arm's ~2.5 ms. `wal_fsyncs == wal_appends` in every cell: one fsync per submission, as
expected before Task 7a's window.

**2. `ExecutorHealth::apply_nanos_*`'s doc is wrong about the deny path, and plan 7b inherits the
error.** The doc (`crates/tessera-engine/src/write.rs:132-147`) says the counter is "**the deny-ack
latency floor**", that the clone "is O(total buffered items)", and that "plan 7b sizes it at
100–300 ms per clone at 1 M buffered items". **At 1,000,000 buffered items a deny's own apply
measures 1.33 µs** — five orders of magnitude below that prediction.

The reason is in the code, one function apart: `apply_ingest` clones the **buffer**
(`write.rs:1166`, `let mut buffer = (*generation.buffer).clone()`), while `apply_change` clones the
**overlay** (`write.rs:1213`, `let mut overlay: Overlay = (*generation.overlay).clone()`). A deny
never touches the buffer. The counter is a **sum over both lanes**, so `apply_nanos_total` at any
moment is dominated by ingest applies and says nothing about a deny's own cost.

Isolating the driver, 2,000 denies on an idle executor so the overlay grows 1 → 2,000 (**measured**):

| starting buffer | ack p50 | deny's own apply, p50 | its max |
|---:|---:|---:|---:|
| 0 | 2.913 ms | 4.29 µs | 118.55 µs |
| 1,000,000 | 3.018 ms | 4.69 µs | 44.47 µs |

Identical at both buffer depths, and up from 1.3 µs at overlay depth 30 to 4.3 µs at depth 2,000.
**The deny's apply is O(overlay), not O(buffer)** — which is what the code says and the opposite of
what the counter's doc says. *(Modelled, flagged as such: extrapolating that linearly to
`overlay_soft_limit = 500,000` puts a deny's own apply near ~1 ms — comparable to the fsync but
still not dominant. It has not been measured at that depth.)*

**3. The real deny latency is head-of-line blocking, and it is 20–50× the quiescent floor.** Six
background threads submitting 10,000-row ingest batches continuously, denies timed from a seventh
(**measured**):

| op | buffer at start | effective buffer under flood † | deny ack p50 | p99 / max | `apply_nanos_max` over the phase | max ÷ apply_max |
|---|---:|---:|---:|---:|---:|---:|
| delete | 0 | 380,000 | 72.31 ms | 476.62 ms | 369.86 ms | 1.29 |
| delete | 10,000 | 390,000 | 75.06 ms | 227.20 ms | 210.17 ms | 1.08 |
| delete | 100,000 | 470,000 | 80.17 ms | 235.04 ms | 279.58 ms | 0.84 |
| delete | 1,000,000 | 1,340,000 | **165.56 ms** | 314.07 ms | 230.24 ms | 1.36 |
| suppress | 0 | 370,000 | 66.79 ms | 219.80 ms | 205.23 ms | 1.07 |
| suppress | 10,000 | 380,000 | 80.85 ms | **666.40 ms** | 436.91 ms | 1.53 |
| suppress | 100,000 | 460,000 | 88.68 ms | 461.26 ms | 310.34 ms | 1.49 |
| suppress | 1,000,000 | 1,340,000 | **166.64 ms** | 345.84 ms | 234.06 ms | 1.48 |

† The flood adds ~370,000 rows during the phase regardless of the starting depth, so the buffered
axis is compressed below 100,000 and only the 1 M cells are cleanly separated. The 1 M cells are
**2.1× the p50 of the others**, which is the O(buffer) ingest clone showing up in the *deny's* wait
exactly as lifecycle §1.3 says it can: "a deny's wait is bounded by the work item currently
executing", and that item's apply is O(total buffered items).

**So the deny-ack floor is ~3.2 ms of fsync, and the deny-ack *bound* is 0.2–0.7 s under sustained
ingest at buffer depths already reachable today.** Those are different numbers by two orders of
magnitude, and only the first has ever been quoted.

**4. `apply_nanos_max` predicts the dominant term but is not a bound on it — short by up to 1.53×.**
Last column above: the worst deny wait exceeds the largest apply in the same phase by 1.07–1.53× in
six of eight cells, and falls below it in one. Two reasons, both structural rather than noise:
a deny waits for the **whole** in-flight work item — its `append`, its `fsync`, its apply, its ack —
not for the apply alone; and then pays its own append and fsync on top. And `apply_nanos_max` is a
population maximum over every apply whether or not a deny ever queued behind that one, so it can
over-state as easily as under-state (the 0.84 cell).

**Recommendation for Task 7b: size the deny-ack floor from `apply_nanos_max + wal fsync latency`,
not from `apply_nanos_max` alone, and treat the result as an estimator rather than a bound.** A
tighter instrument would be a counter on the *work item's whole execution*, which is what a deny
actually waits for; `apply_nanos_*` is the only lane-mixed counter available today and Task 7b is
currently told to use it unqualified.

**5. Never-shed holds, under genuine saturation, in all eight cells.** With `queue_bound = 2` and
six submitters, the bounded work lane refused **7,366–17,427** times per cell
(`SubmitError::QueueFull`) while **zero** denies were refused. The arm records
`never_shed_exercised` from the observed `QueueFull` count rather than assuming the queue filled —
a "no deny was refused" result taken while the queue was never full is not evidence, and the flag
exists so no reader mistakes one for the other.

**Scope limit, stated because the property is only half-tested.** This exercises the **engine's**
lane split (`write.rs:899-931`: `Command::Change` → unbounded `Sender`, `Command::Ingest` →
bounded `SyncSender`). **Task 6 has not landed**, so `/control/ingest`'s 429 mapping, the startup
headroom arithmetic, and the HTTP-level `changes_never_429s` assertion do not exist yet and are
**not** exercised here. What is measured is that the lane split beneath them is correct and that
saturating the work lane does not touch the deny lane.

## What contradicts the current documents

1. **`crates/tessera-engine/src/write.rs:141-146`** — "This is the deny-ack latency floor ... a
   clone that is O(total buffered items) — plan 7b sizes it at 100–300 ms per clone at 1 M buffered
   items". True of the *ingest* apply; false of the deny's. Result 2 above. The counter's own
   rename note ("named for what it measures") fixed the clone-vs-apply confusion and left the
   lane confusion in place.
2. **`crates/tessera-bench/src/arms/ingest.rs:80-84`** and
   **`docs/evidence/memos/2026-07-31-ingest-baseline-pre-task3a.md:99-102`** — "`accept_change` is
   not benchmarked at all" / "no baseline, before or after". **Stale**:
   `crates/tessera-bench/src/arms/changes.rs` has existed since 2026-07-30 with measured ack, tail
   and visibility-arithmetic cells in its own module doc. What was genuinely missing is what this
   memo adds — the buffered-depth axis, the contended bound, and never-shed — because the existing
   arm grows the overlay and never ingests, so its buffer is empty in every cell. Both notes are
   corrected in place by this change.

## Recommendations

1. **Quote the contended number, not the floor, whenever "deny visibility latency" is an SLO.**
   Lifecycle §1.3's bound is *(queue-front + fsync)*, and queue-front is the whole in-flight work
   item. At 1 M buffered items that is ~165 ms p50 and ~350 ms max, measured — against a ~3.2 ms
   floor. A revoked viewer keeps seeing for the larger number.
2. **This is an argument for `flush_max_items` becoming live at stage 2.2, on latency grounds.**
   The deny-ack bound is proportional to the ingest clone, which is proportional to buffer depth,
   which nothing caps until the flush exists. `overlay_soft_limit` alarms on the wrong structure
   for this particular risk: the *buffer* is what lengthens a deny's wait, and the overlay is what
   lengthens its own apply.
3. **Re-run this arm immediately after Task 7a.** Group commit changes the numerator directly —
   one fsync per window instead of per submission moves the ~3.2 ms floor, and draining work into
   a window changes what "the item currently executing" means, which is the entire result 3.
4. **Add a work-item-duration counter** if Task 7b's floor sizing is to be a bound rather than an
   estimate (result 4).

## What this does not measure

The 1e8/1e9 scales — the fixtures exist (`data/bench-fixtures/`) but the deny path's cost is
dominated by fsync and by buffer depth, neither of which is a function of corpus size; the
visibility *check* would be, and the existing `changes` arm owns that. Unsuppress is not measured
(it is not a deny). Replay after a crash at depth is not measured. And the arm's contended phase
saturates a 12-core box with six ingest threads, so its absolute numbers are a loaded-box figure by
construction — that is the point of the phase, but it means they are not comparable with the
quiescent column as if both were the same experiment.
