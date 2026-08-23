# 0089 — A dependency edge carries deletion and visibility

**Date:** 2026-08-19 · **Status:** Settled (owner ruling)

## Context

The member grain answers two questions explicitly, and each has its own key: what becomes of an
artifact when one of its members is deleted (`withdraw_on_member_deletion`), and how much of its
membership a viewer must already see for the artifact to appear
(`require_member_visibility`, [decision 0088](0088-visibility-is-two-axes-and-the-membership-test-is-one.md)).

`depends_on` asks the same two questions at the **dependency** grain and answered neither. It
declared that a layer's artifacts carry edges into another layer — enough to constrain write order
(rep §5.0.4) and to make the dangling-replacement refusal sound — and said nothing about what the
edge means once both ends exist. A label whose cluster is deleted, and a label whose cluster the
viewer cannot see, were both undefined.

The two grains are genuinely distinct. A label's *members* are the documents it was generated
from; its *dependency* is the cluster it describes. A viewer may be able to see most of the
documents and none of the cluster, or the reverse.

## The decision

**A dependency edge is not a bare ordering constraint. It carries both rules, and neither is
configurable.** Where a layer declares `depends_on`, its artifacts:

1. **Are deleted when the artifact they depend on is deleted.**
2. **Are visible only where the artifact they depend on is visible** — a prerequisite evaluated
   before, and in addition to, the dependent's own `visibility` and `require_member_visibility`.

**The prerequisite is per-artifact, not per-layer.** A label is gated on *the artifact it attaches
to*, not on something existing in the parent layer. The layer-level declaration is what makes the
check well-founded: a layer is declared after every layer it names, so a single pass in declaration
order resolves every prerequisite without search.

**An artifact in a layer that declares a dependency must declare one, into the declared layer.**
Refused at build and at ingest alike. An artifact with no dependency has nothing for rule 2 to gate
on, so admitting it would make the prerequisite silently optional — the caller's producer error
becoming a permission.

**Neither rule is configurable**, for now and deliberately. Four knobs across two grains is a
surface no author can hold, and the pair chosen here is the fail-closed one in both directions:
deletion removes rather than strands, and visibility narrows rather than widens. A configurable
form can be added later against a real requirement; it cannot be removed once callers depend on
it.

## Why cascade does not contradict the dangling-dependent refusal

The refusal in rep §5.0.4 governs **replacement**, and its harm is specific: a replacement mints
new identities, so silently repointing a label at the successor attaches it to a cluster it was not
generated from — I8's class. That argument is about *repointing*, and it is untouched here.

Deletion is the other operation. Nothing is left to point at, so the cascade cannot mispoint
anything; it removes the label whose subject is gone. **Replacement still refuses rather than
repointing, and deletion cascades rather than stranding.** The manual sequence rep §5.0.4 already
directs a caller to — drop the dependents first — is what rule 1 performs.

A cascade is a deletion and takes the deletion lane: it retires at the compaction fold that
executes it (Rule F, write-path §5.4), never by any other route. Two removal rules have been
conflated twice in review; a cascade beside the fold rather than inside it would be the third.
The notification obligation write-path §5.8 already anticipates for a nested chain covers it.

## What this supersedes

**[Decision 0086](0086-the-attachment-term-does-not-inherit-the-targets-criterion.md) is reversed
on its central point.** That decision asked whether an attached artifact should be withheld when
its target is alive and reachable but fails its *own* layer's existence criterion for this viewer,
and answered no: the term stays disposition and layer reachability. Rule 2 is that inheritance,
arrived at from the other direction — not as an extension of the attachment term, but as the
meaning of a dependency edge.

Its two arguments are answered rather than overlooked. **On a number set elsewhere:** 0086 objected
that a layer's artifacts would appear and disappear by a threshold its own declaration does not
mention. They do — and the declaration now mentions it, because `depends_on` is in the dependent's
own block and is what a reader follows. The objection held against inheritance arriving silently
through an edge; it does not hold against a declared dependency whose meaning is stated. **On the
cost:** 0086 priced the change at a second masked count per attached artifact per request, and that
price is now paid. It is accepted rather than disputed.

What 0086 called the operator's problem to avoid — *a label layer over a gated cluster layer should
declare a criterion at least as strong as its target's, and nothing enforces that* — is what rule 2
enforces. The asymmetry it documented and declined to close is closed.

Per `docs/decisions/README.md`, 0086 is not edited: it records why the case was decided the other
way in August 2026, which is exactly what a reader needs when they find this one.

## What this replaces

**The `public`-under-a-gated-parent refusal is deleted.** It was introduced with the
`[layer.labels]` expansion to catch a label layer declared wider than the layer it describes, and
it was the one place the sugar was *stricter* than the two `[[layer]]` blocks it expands to — an
author could escape it by writing the expansion out by hand. Rule 2 makes it unnecessary rather
than merely inconsistent: a `public` label layer under a gated parent discloses nothing, because
the viewer that cannot reach the cluster cannot reach its labels either. A check becomes a
property.

This also removes the surface's asymmetry argument against the sugar: with the refusal gone,
`[layer.labels]` supplies mechanism only, and its expansion is again exactly two hand-written
layers.

## Consequences

- Label visibility is conjunctive: the dependency's visibility **and** the label's own two axes.
  It can only narrow what a principal sees, so it introduces no new disclosure and no leak-register
  row.
- The visibility pass gains an ordering requirement it can satisfy in one sweep, since layers are
  already declared dependency-first and the declaration order is acyclic by construction.
- An artifact whose dependency is invisible is absent, not empty — it contributes to no count a
  viewer is shown, on the same rule every other invisible artifact follows (I2).
