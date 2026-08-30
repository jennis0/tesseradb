# 0107 — A generating set with no survivors is not served

**Date:** 2026-08-30 · **Status:** Settled (owner ruling, 2026-08-30)

## The decision

**Where a permissive layer's fold empties a content's generating set, the fold withdraws the
content** rather than retaining it on the empty set. The artifact then follows the rule it already
has: a layer that declares supplied content withholds an artifact left with none
([0076](0076-an-artifact-is-served-whole-or-not-at-all.md)), so the artifact is absent until the
caller republishes — the same state a strict withdrawal produces, reached through the other arm.

Content that requires only inherited visibility is unaffected. It carries no generating set at all
(C28's other half, refused at publication), it names none of the entities a fold retires, and
the shrink never reaches it.

## The question it settles

**Permissive says a content survives its survivors. The design did not say what happens when there
are none.** `annotation-write-cycle.md` §2.1 and Appendix C's **C7** both describe the shrink as
serving *a principal satisfying the survivors*, and neither states a position on the set the fold
empties. A test-quality audit asked what the code did in that case.

**It served the content to everyone.** Containment is a subset test, and the empty set is a subset
of every mask: both of `satisfied_rank`'s guards — that the row form kept every member, and that the
viewer's mask holds every member — are trivially true at cardinality zero. So corpus-derived text
written from a document a viewer was never entitled to see would be served to every principal who
could see any member of the artifact. That is C7's own channel with its bound removed: the register
row is written on *a principal satisfying the survivors*, and with no survivors there is no
principal it excludes.

**The publish path already forbade the state, and said why.** An artifact whose layer requires every
member of its content visible is refused at publication if it declares an empty generating set —
*"such content is served only to a viewer who can see everything it was generated from, and an empty
set is satisfied by everyone"* — and the converse is refused too, so the two kinds of empty set are
already told apart where they enter. **The fold was the only other route to the state**, and it was
open. The gate's reasoning is the ruling's; what was missing was the ruling reaching the fold.

## Why this mechanism and not the alternatives

**Not a new containment outcome, and not a zero case inside the test.** Special-casing an empty
generating set at the point of the subset test would put the distinction in the one place that
cannot draw it: the serving path is handed a set and a mask, and an empty set there is either
withdrawn content or inherited content, which differ by a *declaration* the test does not hold. The
existing code already routes an artifact whose contents are gone to `Unsatisfied` when its layer
declares content, so withdrawing lands the case in a branch that exists, is exercised by the strict
arm's tests, and needs no new state.

**Not withdrawing the artifact directly.** The fold removes the content; the artifact's absence is a
consequence of its layer's declaration, decided where every other such absence is decided. An
artifact with a second, surviving content goes on serving from it, which is the ranked-contents
behaviour and not an exception to it.

**And no stored bit.** §2.1 already deleted a `content_withdrawn` bit for the reason that applies
again here: a bit that must be set correctly is a bit that can be set wrongly. Removing the content
needs no state at all.

## What it costs

**A caller on a permissive layer loses the artifact where the deletion takes its last source**,
rather than keeping it with text no longer generated from anything. That is the strict outcome, and
it is the one they would have got had they declared nothing. Permissive continues to mean exactly
what §2.1 says — the content survives a member leaving — for every case where a member remains.

The gap §2.1 already marks, that a permissive artifact is withheld from the ack until the fold, is
unchanged in length; what changes is that in this one case the fold does not end it.
