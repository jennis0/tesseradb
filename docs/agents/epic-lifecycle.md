# How work is tracked

Work is organised as **capability blocks**, not phases. A phase is a date; a capability is a
thing the system can do, and it is the unit that actually has a goal, a gate, and a point at
which it is finished.

## The shape

An **epic** is a GitHub issue labelled `epic`. It holds:

- the capability, in a sentence — what the system will be able to do that it cannot do now;
- its **gates**: what must be true to call it done, stated so that it can be checked rather than
  asserted;
- links to the governing documents in `docs/design/`;
- its tasks, as **native sub-issues**.

Sub-issues are the tasks. **The issue is the sole authority on status.** No checkbox in a
markdown file, no ledger, no plan. This repo learned that the hard way: it accumulated a dozen
plans whose checkboxes were entirely unticked while the work was in fact complete, because
completion was tracked somewhere else that was never committed.

There are no milestones. For one owner and a fleet of agents they add ceremony without adding a
query you cannot already run.

## Labels

- `epic` — the tracking issue.
- `area/*` — engine, authz, store, lifecycle, build, server, client, conformance, docs, bench.
- `kind/*` — design, impl, measurement, debt, bug, decision.
- `invariant/I*` — this work bears on that invariant. Worth applying generously: it is how you
  find everything that touches I7 before changing what I7 means.
- `status/blocked` — blocked on a decision or on other work. Say which in the issue.

`kind/decision` deserves a note: it marks work that **cannot proceed until the owner rules**.
Do not guess and proceed on one of these. Write the escalation in the issue in the form
[`design-process.md`](design-process.md) requires — rulable without reading the code — and wait.

## Where the detailed instructions go

Not in the issue, and not in a committed plan. Per-task briefs live in the SDD workspace under
`.superpowers/sdd/`, which is gitignored and **disposable by design**. A brief is scaffolding
for one execution; it has no value once the work has landed, and keeping it invites someone to
read it later as though it were a specification.

What the issue holds is what a person needs to understand and check the work. What the brief
holds is what an agent needs to do it. These are different documents and conflating them is what
produced the three-thousand-line plans now sitting in `docs/archive/plans/`.

## Closing an epic

The disposable parts get thrown away; the durable parts get extracted first. Before an epic
closes:

- **Decisions** made along the way → [`../decisions/`](../decisions/), one file each. Owner
  rulings and controller decisions especially — these are the things that exist nowhere else and
  that the next agent will otherwise re-litigate from scratch.
- **Measurements** taken → [`../evidence/memos/`](../evidence/memos/) or `probes/`, dated, and
  not edited afterwards. Record negative results too; "we measured this and it was not true" is
  more valuable than silence, and this repo has several.
- **Design changes** → the corpus, through [`design-process.md`](design-process.md). An epic that
  changed how the system works and did not change its specification has left the specification
  wrong.
- **Leftovers** → new issues. Not a "deferred" note in a document nobody will read again. If it
  is real work, it is an issue; if it is not real work, delete it.

Then close it. An epic that stays open because 10% of it might one day matter is no longer
tracking anything.
