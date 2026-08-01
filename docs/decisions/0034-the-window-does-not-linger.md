# 0034 — A commit window closes when its queue drains; it does not linger

**Date:** 2026-08-01 · **Status:** Settled, with one measurement outstanding

## Context

A commit window closes on two triggers: a row bound, and the work queue being observed empty. A
third was specified — an age bound, `commit_window_max_age_ms` — on the reasoning that without one
a single submission on an idle server would wait the full window age for company that is not
coming.

That reasoning describes a **linger**: having drained the queue, hold the window open in the hope
of gathering more. An age bound is the safety cap on a linger, not a trigger in its own right.

## Decision

**There is no linger, so there is no age bound.** `commit_window_max_age_ms` is parsed, validated
and inert, and a test fails the moment anything outside the configuration module reads it.

A window is a local of the drain that every exit disposes of; none survives the executor's wait for
new work. The interval an age bound would end therefore does not exist, and the risk it was
specified to prevent — a submission waiting for company that never arrives — is already prevented
by closing when the queue empties, which is strictly tighter.

## Why the obvious counter-argument does not hold as stated

A window closing on an empty queue can close *too early*: work that was genuinely imminent arrives
just after. That interval is real. But an age bound only ever closes a window **earlier** than the
trigger it supplements, so it is the wrong sign for that problem. The mechanism that addresses it
is a linger, and a linger has a cost the empty-queue close does not: it makes a submission wait for
a submission that may never come.

**A linger cannot help a single sequential client at all.** A client blocks on its acknowledgement,
so it cannot submit again until the window it is waiting on has closed. Lingering means waiting for
the one party who is waiting for us.

## What this leaves open

How much a window holds sets how much posting compression its allocation run collects, and rows per
window is the product of submission size and in-flight request count. A linger could raise it for
**many independent clients arriving in bursts** — a case the reasoning above does not cover, since
it concerns one drain rather than many arrivals.

Whether that is worth building depends on which of the two inputs dominates, which is
[measured in one arm](../evidence/memos/2026-08-01-deny-batching-and-window-compression.md) for
concurrency and **assumed, not measured**, for submission size. The assumption is that submission
size dominates: the arm held submissions two orders of magnitude below the batch cap, so a single
client sending maximal batches would fill a window alone. If that holds, the remedy is guidance to
loader authors rather than a mechanism, and this decision stands unchanged. The knob remains parsed
so that making it live is a small change rather than a new configuration surface.
