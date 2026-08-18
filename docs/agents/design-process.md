# How a design becomes normative

Design work here is deliberately slow at one specific point: nothing enters the corpus as
binding without an independent adversarial review it could have failed. That review has caught
four fail-open paths and two unimplementable mechanisms in this project's history, which is the
whole argument for keeping it.

It is slow at that point and nowhere else. This process is for documents that will bind the
implementation — the invariants, the contracts, a mechanism the rest of the corpus rests on.
Everything else is just work: make the change, run the gate, move on.

## The stages

**1. Brainstorm.** Establish what problem is actually being solved and what would count as
solving it. Most of the value is in discovering that the question was wrong.

**2. Draft into `docs/design/`, marked Provisional.** Not into a separate staging directory — a
second place to look is worse than an honest marker in the one place. The `Status:` line must
say `Provisional — under review`, and must name **what specifically remains before it becomes
normative**. "For review" alone is not enough; the reader needs to know what the gate is.

**3. Independent review, with no stake — once.** Dispatch a subagent that did not write the draft,
and brief it to review against the invariants and the existing corpus rather than against taste.
Where a design is invariant-bearing, use two or three reviewers with distinct lenses — security,
performance, implementability — rather than several with the same one. Redundant reviewers agree
with each other; diverse reviewers find different failures. The views design took three lenses
and all three found something the others did not.

The reviewer's job is to try to break the design. A review that returns approval without having
attempted a refutation has not happened.

One round is the norm. Re-review only when the disposition changed the design's shape — not to
confirm that edits were applied, and not because a reviewer offered improvements. A design that
has been through three rounds is usually accreting complexity rather than converging: each round
answers the last round's objections with more mechanism. If a review round adds machinery without
closing a fail-open path, that is the signal to stop and cut, not to run another round.

Reviewers report findings that would change the design. Taste, alternatives that are merely
different, and speculative hardening are noise at this stage — brief them to say so plainly and
briefly if they have nothing that bites.

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

Appendix R is a trail, not a changelog. One short entry per revision: what the review attacked
and what changed as a result. Anything a reader needs in order to understand the system belongs
in the body; anything they do not need belongs in git. Older entries collapse — once a revision
is several revisions back, a single line covering the range is enough, and Appendix R should
never approach the length of the design it trails.

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

Nor does anything outside the normative corpus: implementation, tests, probes, evidence memos,
tooling, and the provisional exploration that precedes a draft. Those go through the gate in
[`parallel-work.md`](parallel-work.md) and nothing else.
