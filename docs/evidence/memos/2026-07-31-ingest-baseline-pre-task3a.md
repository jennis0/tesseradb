# Ingest baseline before Task 3a — the fsync floor, measured

**Status:** measured 2026-07-31 at `df84423`, before Phase 2 stage 2.1 Task 3a moves the WAL behind
a single writer thread. Captured because Task 3a changes what the ingest arms measure — the inline
path becomes the executor path — so figures taken after it are not comparable with figures taken
before. Raw cells: `probes/2026-07-31-ingest-baseline/{ingest_batch,ingest_continuous}.jsonl`.

**Corpus:** 2,422,486 items, `categories-subclass`, 3 repeats per cell, release + `bench-timing`,
12 cores / 47 GiB, bundle resident.

---

> ## ⚠ Do not compare post-Task-3a figures against this file *(added 2026-08-01)*
>
> **The warning three lines above was written, and then ignored — by the Task 3a worker, and by me.**
> Both of us produced a post-change table against these numbers and read a batch-10,000 "regression"
> out of it (+6.0% and +16.6% respectively). Track B's performance lens established that most of that
> figure is a **moved timer boundary, not moved work**, and the record is corrected here so the next
> reader inherits the correction rather than the table.
>
> **What moved.** At `df84423` the bench arm's `synth_rows` called `engine.allocate_sorted` and built
> the `WalRow`s *before* `Instant::now()`, so the timed region was append → fsync → apply → swap
> alone. At `c0bbd6b` `synth_rows` builds `UnallocatedRow`s and the timed region additionally covers
> the duplicate scan, `to_pending`, `assign_sorted` (one sort-key `Vec<u32>` per item, then a sort of
> all of them) and `into_wal_row` framing. **Per-row heap work across the system is unchanged** — the
> lens counted the clones on both sides and they are equal, and `c0bbd6b` removed two the executor had
> introduced.
>
> **The new arm is the more honest one.** Phase 1's handler always did the sort inside the request, so
> the *old* arm under-measured the real ack path. This is a re-baseline in the direction of accuracy.
>
> **Corrections that follow.** Task 3a shows **no evidence of real ack-path regression above ~1%**; the
> deltas in both post-change tables are timer boundary plus session noise (two runs of the same commit
> disagree by 10–12%, in opposite directions, and this file's own 10k cell spans 7.762–12.135 ms at
> n=3). And the Task 3a report's claim that the residual is "exactly what 7a amortises" is **false**:
> a commit window amortises per-*submission* costs — the fsync, the buffer clone, the swap — while
> `to_pending` and `assign_sorted` are per-*row* and a window contains the same rows. Whatever residual
> is real is permanent, and 7a's gate must not expect the 10k cell to move.
>
> **The baseline for Task 7a is `/tmp/tessera-bench/runs/b-verify/`, not this file** — measured at
> `c0bbd6b` on a quiet box under `scripts/bench-slot.sh`. This file remains valid for what it always
> was: the fsync floor and the compose curve, both of which are boundary-independent.

## Results

**1. Below ~1,000 items per batch, ingest cost is one fsync and nothing else.**

| batch | median ack | items/s | ns/item |
|---:|---:|---:|---:|
| 1 | 3.244 ms | 319 | 3,133,335 |
| 10 | 3.379 ms | 3,080 | 324,649 |
| 100 | 3.331 ms | 30,617 | 32,662 |
| 1,000 | 3.462 ms | 289,617 | 3,453 |
| 10,000 | 9.760 ms | 1,288,392 | 776 |

A thousand-fold change in batch size moves the ack by **7%**. Per-item cost falls **4,000×** across
that range while total time is flat, which is the signature of a fixed cost being amortised rather
than of work being done: ~3.2 ms of fsync, consistent with the figure already recorded in
`tessera-bench`'s own `ingest-continuous` doc. Only at 10,000 does real work overtake the barrier.

**2. This is the group-commit thesis, measured on the current engine.** Design §11.1 and lifecycle
§5.1 argue the commit window on compression grounds; row 1 says it also buys latency outright.
Four separate 25-row submissions today pay four fsyncs — **~13 ms** — for 100 rows that one window
would commit in **~3.2 ms**. The plan's `one_fsync_per_window` and
`the_sort_scope_is_the_window_not_the_request` tests are therefore measuring a ~4× effect at that
shape, not a marginal one, and the effect grows with the number of small submissions a window
absorbs.

**3. Compose cost against buffer depth — the F2 curve, and it bites earlier than the soft limit
suggests.**

| buffered | compose | share of request | ack at depth |
|---:|---:|---:|---:|
| 501 | 6.9 µs | 0.71% | 3.497 ms |
| 2,001 | 21.4 µs | 2.65% | 3.180 ms |
| 5,001 | 59.8 µs | 6.09% | 3.328 ms |
| 10,001 | 154.3 µs | 16.05% | 4.754 ms |
| 25,001 | 242.7 µs | 24.02% | 4.613 ms |

Compose is roughly linear in depth (50× depth → 35× compose) but its **share** of the request goes
0.7% → 24%, because the rest of the request does not grow. `overlay_soft_limit` defaults to
500,000 — twenty times the deepest point measured here — so a deployment reaching even a fraction
of that limit has compose dominating every viewport it serves. Stage 2.1 alarms on the limit but
cannot fold (there is no fold until 2.3); this table is the argument for why the alarm threshold
should be sized from compose share rather than from memory.

## Recommendations

1. **Re-run both arms immediately after Task 3a** and compare against this file, not against
   memory. The comparison is the stage's ingest result.
2. **Size `commit_window_max_items` against row 1, not against the compression ceiling alone.** The
   latency win saturates around 1,000 rows per window; the compression win keeps growing with
   window size. They are different curves and the knob has to serve both.
3. **Revisit `overlay_soft_limit`'s default** when 2.3's fold lands. 500,000 was chosen as a memory
   bound; row 3 says the serving cost arrives long before the memory cost does.

## What this does not measure

The deny path.

> **Corrected and closed 2026-08-01.** This section read: *"`accept_change` is not benchmarked at
> all — a gap `tessera-bench`'s own `arms/ingest.rs` doc already records — so the deny-ack latency
> that lifecycle §4 and stage 2.1's never-shed asymmetry both turn on has no baseline, before or
> after."* **Two corrections.** First, "not benchmarked at all" was already wrong when written:
> `crates/tessera-bench/src/arms/changes.rs` has measured `accept_change` ack, tail and visibility
> arithmetic against *overlay* depth since 2026-07-30. Second, the real gap was narrower and more
> interesting than the sentence claimed — that arm never ingests, so its buffer is empty in every
> cell, which left the two things that actually move deny-ack unmeasured: buffered-item depth, and
> the head-of-line wait behind an in-flight ingest apply.
>
> Both are now measured, along with never-shed:
> **[`2026-08-01-deny-ack-baseline.md`](2026-08-01-deny-ack-baseline.md)**. Headline: the quiescent
> floor is one fsync (~3.2 ms) and is **flat across 0 → 1,000,000 buffered items**, because a deny
> clones the overlay and not the buffer — but under sustained ingest the same deny takes **67–167 ms
> p50 and up to 666 ms**, because it waits behind a work item whose apply *is* O(buffer). That memo
> also corrects `ExecutorHealth::apply_nanos_*`'s doc, which names itself the deny-ack floor and is
> off by five orders of magnitude for that use.
