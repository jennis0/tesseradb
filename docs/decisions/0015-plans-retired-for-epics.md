# 0015 — Plans are retired; work is tracked as capability epics

**Date:** 2026-08-01 · **Status:** Settled

## Context

The repo accumulated twelve implementation plans, several thousand lines each. Every checkbox in
every one was unticked while the work was in fact complete, because completion was tracked in
ledgers that were gitignored and existed on one machine.

The plans were also being read as technical references, which they are bad at: each conflated
design rationale, work breakdown, and per-task agent instructions — three things with different
lifespans.

## Decision

**Split them by lifespan.**

- Design rationale → [`../design/`](../design/), or [`.`](.) where it is a point decision.
- Work status → GitHub issues. A **capability epic** is a tracking issue holding a goal, its
  gates, and links to the governing documents; its **sub-issues are the tasks**, and the issue is
  the sole authority on status.
- Per-task instructions → the gitignored SDD workspace, disposable by design.

Work is organised by capability, not by phase. A phase is a date; a capability is something the
system can do, and it is the unit that has a goal and a point at which it is finished.

No milestones — for one owner and a fleet of agents they add ceremony without adding a query you
cannot already run.

## Consequence

At epic close, decisions are extracted here, measurements to `../evidence/memos/`, and leftovers
to new issues. The plan is never maintained as a reference. The twelve existing plans are archived
with banners recording whether they ran.

## Evidence

[`../agents/epic-lifecycle.md`](../agents/epic-lifecycle.md).
