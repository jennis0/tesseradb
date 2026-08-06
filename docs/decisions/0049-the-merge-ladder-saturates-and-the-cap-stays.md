# 0049 — The merge ladder saturates at the cap, and the cap stays until the merge streams

**Date:** 2026-08-05 · **Status:** Settled (owner ruling)

## What was found

`MergePolicy::select` requires `tier_width` adjacent segments of one power-of-two size class whose
**total** is within `max_merged_segment_bytes`. A merge is row-count preserving, so the ladder
climbs in ×`tier_width` steps from `segment_floor_bytes` and stops at the last step that does not
exceed the cap. At the shipped defaults — width 4, floor 16 MiB, cap 256 MiB — that is 16 → 64 →
256 MiB, and four 256 MiB segments total 1 GiB, so **no further merge ever qualifies**.

Live segment count therefore settles at **corpus bytes ÷ the saturation size** and grows linearly
with the corpus. At 10⁹ rows and the measured ~39 B/row that is ~152 segments. Pinned by test
(`merge_selection.rs`,
`the_size_ladder_saturates_at_the_cap_and_segment_count_then_tracks_the_corpus`).

**This is not a defect in the policy.** It is the price of §11.3's *"a maximum merged size, so no
merge becomes an unbounded rewrite"*, and the count is bounded rather than unbounded. What was
wrong was the corpus: §11.3 said *"The operation that bounds it is a merge"* without saying **to
what**, and the 10⁷ soak that settles at 6 segments is too small to show the constant.

**It is a read-path constant.** A viewport pays a measured 1.4–1.6 µs per (tile × segment)
(`2026-08-05-write-path-at-scale.md` §3), so ~152 segments is ~73 ms on a realistic 300-tile
viewport against a measured 135–164 ms baseline at 10⁹. It is immaterial at 10⁷, becomes visible
around 10⁸, and is a ~50% regression by 10⁹.

**`tier_width` moves the fixpoint, in the direction nobody expects.** At width 8 the ladder reaches
128 MiB and the next rung overshoots, leaving **twice** as many segments over the same corpus.
Anyone widening the tier to reduce merge frequency is also raising a read-path constant.

## The ruling

**The cap is not raised.** Raising it lowers segment count nearly linearly, and would be the
obvious move — but `max_merged_segment_bytes` bounds *selection-time file bytes*, and merge's peak
memory is a **measured 4.4–4.9× those bytes** (`probes/2026-08-04-maintenance-memory/`). Reaching
~20 segments at 10⁹ needs a ~2 GiB cap, which models to a 9–10 GB pool transient — and nothing
bounds the sum when a flush, a merge and a coalesce overlap. Trading a 50% read regression for an
OOM risk is the wrong direction, and the memory multiplier, not the cap, is the binding constraint.

**The structural fix is to make merge stream**, and it is already designed for elsewhere: the
k-way merge over sorted inputs that compaction's row-space pass needs (`compaction.md` §3, pass 1)
is the same primitive. `execute_merge` today decodes every input at once and doubles at the sort,
which is what produces the multiplier; every input is mmapped, uncompressed and already sorted on
`(morton, tessera_id)`, so a merge of sorted runs costs O(inputs) cursor state instead. Once merge
streams, the cap stops being a memory bound and becomes a write-amplification knob, and it can go
to gigabytes for nothing. **Sequence it with compaction, and build the primitive once.**

**Until then the fold is the other backstop** — a compaction returns the slice to one segment, so
segment count is reset rather than merely bounded. Segment count joins the fold's trigger gauges
(`compaction.md` §9).

## What this obliges

- **A live segment-count gauge per slice on `/control/status`.** ⊘ **Not built** — there is no
  engine accessor for it today and nothing publishes it, so the constant this decision is about is
  currently unobservable in a running deployment. It is the smallest piece of this and it should
  land first.
- **write-path §7 and §10** state the saturation and the `tier_width` warning; architecture §11.3
  needs no edit, since r33 moved the numbers to write-path.
- Any future change to `tier_width` or the floor is a change to a read-path constant and says so at
  the site.

## What was considered and declined

- **Raise the cap now, accept the transient.** Declined above: the multiplier is measured and the
  overlap is unbounded.
- **Exempt the top tier from the cap.** That is an unbounded rewrite under another name — §11.3
  forbids it, and it would arrive without any of compaction's publication machinery.
- **Widen `tier_width`.** Actively counterproductive, per the test.
- **Treat it as compaction's problem alone.** Compaction resets the count, but a deployment between
  folds still pays it, and the fold is unbuilt.
