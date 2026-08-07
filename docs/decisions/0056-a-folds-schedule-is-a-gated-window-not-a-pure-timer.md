# 0056 — A fold's schedule is a gated window, not a pure timer

**Date:** 2026-08-07 · **Status:** Settled (owner ruling)

> **Extended the same day, before anything was built on the first form.** As first ruled, the
> segment gauge fired *only* inside the window. The owner's correction is that segment growth is
> deferrable up to a point and not indefinitely, so it gets a second, higher threshold that fires at
> any hour. The window and its argument are unchanged; what changed is that the segment gauge now
> has a floor **and** a ceiling. Recorded in place rather than as a superseding decision because it
> completes this ruling rather than reversing it — the reader who wants the difference has one
> commit to look at.

## Context

`compaction.md` §9 specifies the fold's automatic trigger as an **OR over four work gauges** with
`compaction_min_interval_secs` as a floor beneath them, and declines a timer outright:

> **The minimum interval is a floor, not a trigger, and there is deliberately no maximum age.** A
> pure timer was considered and is declined: it schedules the most expensive operation in the system
> against a bundle that may have nothing to reclaim.

That reasoning is about a timer that fires *whatever the state of the bundle*. It does not reach a
timer that fires **only when there is work**, and the difference matters operationally: a fold is
minutes to hours of IO that costs a concurrent viewport up to a measured 2.03× (P3) and every
resident session a measured 10.7 s inline projection rebuild at the flip. A gauge-only trigger
starts that at whatever hour the gauge happens to cross — which, for a deployment whose churn tracks
its traffic, is reliably the busiest one.

## The decision

**Three trigger routes over two work gauges, and only one of them is windowed.**

1. **A daily window, over the segment gauge's floor.** `compaction_window_start` (UTC `HH:MM`,
   default `00:00`) opens a window of `compaction_window_secs` (default 4 h) in which a fold is
   dispatched **if** any live slice holds at least `compaction_window_min_segments` segments
   (default 8). Outside the window this route never fires.
2. **The segment gauge's ceiling, unwindowed.** A fold is dispatched at any hour once any live
   slice reaches `compaction_max_segments` (default 64) — §9's own gauge at §9's own default.
3. **Retirable depth, unwindowed.** A fold is dispatched at any hour once `|deleted|` reaches
   `compaction_after_deletions`, which defaults to `overlay_soft_limit` — the action that alarm was
   always supposed to prompt.

`compaction_min_interval_secs` (86,400) remains the floor under all three, and the ceiling must sit
strictly above the floor or the window is unreachable — a configuration the loader refuses rather
than ships.

**The window is not a maximum age and does not become one after a missed window.** A node that is
down at 00:00 and starts at 09:00 does **not** fold: the window has closed, and the next one is
tonight. That is what `compaction_window_secs` is for, and it is the whole difference between "a
start time" and "any time after a restart".

## Why

**A gated timer is not the mechanism §9 declined.** §9's objection is *"a bundle that may have
nothing to reclaim"*, and the gate is exactly the answer to it: an idle deployment, or one whose
merge is keeping the segment axis bounded on its own, crosses no threshold and folds nothing. §9's
own precedent — the growth-gated tick rotation, where an idle node rotates nothing — is the same
shape. What the window adds is *when*, not *whether*.

**Deferring a cost is not the same as ignoring it, which is why the segment gauge has two
thresholds and not one.** Segment count is a read cost that degrades a viewport gradually, so at
eight segments nothing breaks if it is paid down tonight — that is the window's floor. But
"gradually" is a rate, not a ceiling: decision 0049 measured ~73 ms on a 300-tile viewport at ~152
segments against a 135–164 ms baseline, a ~50% regression, and a deployment ingesting fast enough to
add segments through the night gets there long before the next window. Telling it to wait is
choosing a worse hour for the read path over a worse hour for the write path, on behalf of every
viewer. So the same gauge fires at any hour once it reaches `compaction_max_segments`.

**Retirable depth has no window at all**, because the cost it measures is unbounded rather than
merely growing: the overlay grows monotonically under deletion churn, every deny acceptance clones
it, and depth is a term in I1's composition cost. There is no threshold below which waiting is
free, so there is nothing for a window to protect.

**UTC, not local time, and this is a correctness argument rather than a convenience one.** A
local-time window shifts by an hour twice a year, and on the transition day it fires either twice or
not at all — for the most expensive operation in the system, against a 24 h floor that would then
either block the second firing or let it through depending on which direction the clock moved. A
deployment that wants its window at local midnight sets the offset itself, once, and it stays put.

**`compaction_after_deletions` defaults to `overlay_soft_limit` rather than to a number of its
own.** §9 already says the alarm *"gains here the action its alarm was always supposed to prompt"*,
and two independently-set thresholds is how an operator ends up with an alarm that never has a
consequence. It is separately settable for the deployment that wants to be told earlier than it
wants to act.

## What was rejected

**Keying the unwindowed route on "changes received" rather than on retirable deletions.** That was
the shape first proposed, and it is r3's memory finding F5 arriving by a new route: a *change* is a
delete, a suppress or an unsuppress, and Rule S says a suppression never retires. A deployment
holding 500,000 standing suppressions would be permanently over the threshold and would dispatch a
**full no-op fold every interval, for ever** — rewriting the whole corpus to retire nothing. The
trigger keys on what a fold can actually reduce; the *alarm* stays on total depth, which is the
right thing for an operator to see.

**A window with no work gate.** That is the pure timer §9 declined, and the argument is unchanged: a
deployment that takes three deletions a year has no reason to rewrite 47 GB at midnight.

**A windowed-only segment gauge**, which is how this decision first read. It leaves a deployment
that reaches a viewport-degrading segment count at 09:00 waiting until midnight while every tile
pays a binary search per segment — deferring a cost past the point where deferring is the cheaper
option, which is the opposite of what the window is for.

**Making the window a deadline that survives its own end** (fold as soon as possible after a missed
window). It converts the one knob whose purpose is *"not during the day"* into a guarantee that a
fold will eventually run during the day.

**Durable last-fold state, so the floor survives a restart.** Unnecessary, because the work gate
already covers what it would buy: a fold leaves one segment per partition-slice and an overlay with
the executed deletions gone, so a node restarting inside its own window re-evaluates both gauges
against the bundle it just produced and dispatches nothing. The floor being process-local is
therefore only reachable by a restart *between* a fold's completion and the gauges recovering, which
is a state a fold does not leave behind.

## Consequences

- `compaction.md` §9's trigger table gains the window and its two knobs, and the paragraph declining
  a pure timer is narrowed to say which timer it declines.
- Two of §9's four gauges stay **unbuilt**: dead bytes and the tombstoned-row fraction, which are
  the two thresholds §14 already marks as *assumed* and which need a disc walk nothing performs.
- Two new defaults — `compaction_window_secs` (4 h) and `compaction_window_min_segments` (8) — are
  **assumed**, on the same footing as §9's other two uncalibrated numbers, and probe **P1** is what
  turns them into evidence.
- `POST /control/compact` (contracts §3.4) is still unbuilt. `Engine::request_fold` is the trigger
  both it and this schedule call.
