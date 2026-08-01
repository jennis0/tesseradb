# Working in this repo

You are almost certainly an agent. This directory tells you how work is done here; it does not
tell you what the system is. For that, start at [`../design/README.md`](../design/README.md).

## Where truth lives

| Directory | What it is | How to treat it |
|---|---|---|
| [`../design/`](../design/) | **The specification.** Architecture, invariants, contracts, mechanisms | Normative. If your code disagrees with it, one of you is wrong and it is usually not the document |
| [`../decisions/`](../decisions/) | Settled decisions, one per file, dated, immutable | Binding. Read before re-litigating anything |
| [`../evidence/`](../evidence/) | Measurements, investigations, prior art | Never normative. Evidence for decisions, not decisions |
| [`../../probes/`](../../probes/) | Raw measurement campaigns | Same. Re-run before trusting a quoted figure |
| [`../archive/`](../archive/) | Frozen and superseded | Never cite as authority. Do not execute |
| GitHub issues | What is being worked on now | The only authority on status |

Two rules follow from that table and are worth stating plainly.

**A document's location does not tell you its standing — its `Status:` line does.** Read it
before you trust the document. `docs/design/` contains both normative documents and provisional
ones that code is already written against; the provisional ones say so in their first five lines
and name what remains before they become normative.

**Precedence, where documents conflict:** `architecture.md` is the specification and wins.
`system-architecture.md` and the mechanism documents implement it and defer to it. Where
`contracts.md` and `system-architecture.md` differ, the eleven recorded deviations in contracts
§0.3 govern. Provisional documents lose to normative ones until an owner ruling promotes them.

Every corpus document carries a review trail in its Appendix R. Read it before re-opening a
decision — most obvious objections have already been raised and answered there, and the trail
records which ones were wrong.

## Which document for which task

- **"How does X work?"** → `../design/architecture.md` §2.6 walks a request end to end and points
  at the section governing each step.
- **"Am I allowed to do this?"** → the thirteen invariants, and Appendix C's leak register. The
  register is exhaustive by construction: a disclosure not in that table is a bug, not an
  omission.
- **"What does this byte mean?"** → `../design/contracts.md`.
- **"Why is this concurrent thing shaped like this?"** → `../design/concurrency-lifecycle.md`.
  Note the **three** retirement rules; conflating them is fail-open and has been caught twice.
- **"What is the client allowed to assume?"** → `../design/client-interaction.md`.
- **"Has this been measured?"** → `../evidence/memos/` and `../../probes/`. Do not assume; the
  repo distinguishes measured from modelled deliberately, and several plausible optimisations
  here were refuted by measurement.
- **"Was this already decided?"** → `../decisions/`, then the relevant Appendix R.

## The four procedures

- [`design-process.md`](design-process.md) — how a design becomes normative.
- [`epic-lifecycle.md`](epic-lifecycle.md) — how work is tracked, from capability to closed issue.
- [`parallel-work.md`](parallel-work.md) — how several agents work this repo at once without
  colliding.
- [`writing.md`](writing.md) — the house style, for prose and for code comments.

## The one thing to internalise

The conformance suite is the deliverable, not the performance architecture. An implementation
that keeps every clever thing about this system — Morton order, Roaring masks, tiered decode —
while quietly dropping I2, I7 or I13 passes every functional test and leaks. Optimisations are
welcome; they are not welcome at the cost of being able to see that the guarantees hold.
