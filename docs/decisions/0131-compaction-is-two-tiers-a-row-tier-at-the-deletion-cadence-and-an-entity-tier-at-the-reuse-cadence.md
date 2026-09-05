# 0131 — Compaction is two tiers: a row tier at the deletion cadence and an entity tier at the reuse cadence

**Date:** 2026-09-05 · **Status:** Settled (ruled from the independent review under the owner's delegation)

## What this answers

Under epoch shards a compaction is per shard, and one real compaction measured 330 s at 36M rows
(`probes/2026-09-04-epoch-shard-fold-decomposition/`), 50.5% of it proportional to the shard's
rows. Scaled linearly to a shard at the entity ceiling that half is about 5.5 hours, and the
deletion gauge that schedules a compaction, 500,000 un-retired deletions, unwindowed
([compaction.md](../design/compaction.md) §9), fires per shard continuously at any churn once a
shard holds 10⁹ rows. One shard at a time, at that cost and that cadence, does not fit a day at
tens of shards. The independent review of 2026-09-05
([memo](../evidence/memos/2026-09-05-sharding-design-review.md), finding 2) found the two cheap
passes and the two expensive ones serve different purposes.

## The decision

A compaction plan names one point shard and one tier.

**The row tier** merges the shard's row space to one base per view, rewrites postings, external
ids and the key filter, rebuilds the shard's membership slices and row-major label columns, and
writes digests. It removes dead rows and retires deletions under Rule F. It returns no slot. It
runs on the dead-rows fraction, the segment count, and un-retired deletions as a fraction of the
shard's live rows. Modelled at about 36 minutes per 2³² shard plus the artifact structures,
proportional to rows.

**The entity tier** rewrites the attribute extents and the entity-to-term transpose, returns the
slots the row tier freed to the pool or retires them at the cap (decision 0126), and reconciles
every generating set in every incarnation that names an entity of this shard, in decision 0072's
order. It runs on dead entity bytes as a fraction of the shard's and on pool demand. Modelled at
about 4.9 hours per 2³² shard.

Row tiers run concurrently, up to `compaction.concurrent`, admitted by the pre-flight budget
(compaction §3) summed over those in flight. Entity tiers run one at a time. The absolute
deletion gauge becomes a ratio of the shard's live rows.

⊘ Not built. Compaction today is one plan over one prefix, all passes, and the deletion gauge is
absolute.

## Why the tiers are safe apart

An entity leaves the shard's postings at the row tier, so from that publication no session
fragment names it, and every filter meets the mask before any extent (architecture §8.2). An
attribute value left in the extents between the tiers is addressed by nothing a request can
reach. The slot is returned only at the entity tier, after the reconciliation that decision 0072
requires before reuse, so the reconcile-before-reclaim order becomes a cadence rather than a pass
boundary inside one plan.

## What it costs

The entity tier's reconciliation walks every generating set naming the shard, in every
incarnation: class C of the decomposition, 1.9 s for 30,473 records' lineage, so about ten
minutes at 10⁷ records by the review's estimate, once per entity tier rather than once per
compaction. Between tiers a shard holds dead attribute bytes its row space no longer references,
bounded by the entity gauge. A shard whose pool is never drawn on runs the entity tier only on
dead bytes.

## What this changes elsewhere

Compaction §3 (the budget admits several row tiers), §9 (the gauges, the ratio); decision 0056
(the gated window applies per tier); decision 0072 (the order becomes a cadence);
[sharding.md](../design/sharding.md) §3.4 and ruling M.
