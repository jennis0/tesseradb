# How a design becomes normative

Design work here is deliberately slow at one specific point: nothing enters the corpus as
binding without an independent adversarial review it could have failed. That review has caught
four fail-open paths and two unimplementable mechanisms in this project's history, which is the
whole argument for keeping it.

## The stages

**1. Brainstorm.** Establish what problem is actually being solved and what would count as
solving it. Most of the value is in discovering that the question was wrong.

**2. Draft into `docs/design/`, marked Provisional.** Not into a separate staging directory — a
second place to look is worse than an honest marker in the one place. The `Status:` line must
say `Provisional — under review`, and must name **what specifically remains before it becomes
normative**. "For review" alone is not enough; the reader needs to know what the gate is.

A design document in this repo argues. It states what it rejected and why, distinguishes
measured from modelled, and carries a section on what it deliberately does not do wherever its
scope is contestable. See [`writing.md`](writing.md).

**3. Independent review, with no stake.** Dispatch a subagent that did not write the draft, and
brief it to review against the invariants and the existing corpus rather than against taste.
Where a design is invariant-bearing, use several reviewers with **distinct lenses** — security,
performance, maintainability — rather than several with the same one. Redundant reviewers agree
with each other; diverse reviewers find different failures. The slices design took three lenses
and all three found something the others did not.

The reviewer's job is to try to break the design. A review that returns approval without having
attempted a refutation has not happened.

**4. Owner ruling.** Findings are dispositioned by the owner, not by the drafter and not by the
reviewer. Anything touching an invariant, the leak register, or an assumption the rest of the
corpus rests on is an owner decision by definition.

Escalations must be **rulable without reading the code**: state what the corpus says now, what
the change would make it say, what the code actually does in one sentence, the options with
their consequences, and a recommendation with what it costs if wrong. An escalation that
requires the owner to open a source file to answer it has failed and goes back for rework.

**5. Promotion.** On approval: the `Status:` line becomes `Normative (rN)`, the revision is
bumped, an Appendix R entry records what the review found and what changed, and every
cross-reference to the document is updated. Rulings are written to
[`../decisions/`](../decisions/) — a decision that exists only in a review thread does not
exist.

## Changing a normative document

Same review, no exceptions for small changes. Additionally:

- Bump `rN` and add the Appendix R entry. A revision that changes content without a trail entry
  is the failure this convention exists to prevent.
- If the change touches the invariants or the leak register, expect it to show up as a one-line
  diff in `docs/design/inventory.md`. That is deliberate — those two things are too important to
  change invisibly inside a thousand-line document.
- Update the code comments that cite the section you changed. They are prose cross-references
  with only partial mechanical checking.

## What does not need this process

Fixing a stale cross-reference, a revision pointer, a typo, or a figure that the probes contradict.
Those are corrections, not design. If you find yourself arguing for one, it is not one.
