# Artifacts — the Stage 0 review

**Date:** 2026-08-15 · **Status:** Review record — evidence, not normative. **Findings await owner
disposition**; nothing here changes a design until it is ruled on.
**Reviewed:** [`annotations.md`](../../design/annotations.md) (the model) and
[`annotation-representation.md`](../../design/annotation-representation.md) (the representation),
both **drafts**. [`annotation-write-cycle.md`](../../design/annotation-write-cycle.md) was read as
context and was **not** under review — it has had its three reviews and its findings are
dispositioned.
**Method:** three independent reviewers, distinct lenses — disclosure, implementability against the
code, data modelling — none seeing another's report, each briefed to refute rather than approve and
told which questions were already open so the review could not return the owner's own list.
**Verification:** every finding recorded below was checked against the documents or the code by the
controller before being written down. Claims that did not survive that check are not here.

---

## The headline

**The model's core survives.** Three object kinds, three questions, one containment test plus one
threshold: all three reviewers attacked it and none broke it. What did not survive is the claim to
have **derived** the rules that hang off it. Four of them — who owns the count threshold, how a
label's existence is gated, what happens under a filter, and what governs a count — are currently
settled by whichever section the reader lands on.

**Four fail-opens, and a structural blocker upstream of the storage design.** All five are cheaper
to answer now than after the layer registry is built on them, which is why the stage gate exists.

## 1. Fail-opens

**Search reads withheld label text.** The model separates *may you know this exists* from *may you
see this content*, and the representation then specifies search as a token index over artifact text
with **no containment test on the route at all** — verified: its search section never mentions a
generating set. A viewer correctly refused a label's text can match a word in it, and repeating that
is a word-presence oracle over content derived from documents they cannot read. The corpus already
requires the adjacent surface — the filterable label vocabulary — to be containment-filtered under
C11; this route reaches the same data without citing it.

**A recycled entity slot silently joins a stored membership.**
[Decision 0072](../../decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md) frees a
slot at the fold only once every durable structure naming it has been reconciled, and names artifact
membership as one of those structures. The write cycle defines that reconciliation for generating
sets and says in terms that *"membership is not in scope and never was"*. So: a member of an
analyst's selection is deleted; every masked count falls correctly; the fold frees the slot; a later
ingest is allocated it; **the selection regains a member nobody added**, invisible to its owner. It
is the undeclared-member hazard the write cycle already refuses in its other form — range-wise row
translation is forbidden precisely because a Morton neighbour could lift an artifact across
`min_visible_members` — arriving through a different door.

**The build-time bounding box the representation deleted is still mandated by the model.** The
representation removes it and says why: serving wherever a full-membership box intersects discloses
the unmasked extent by panning. The model's §6.2 — not withdrawn with §7 — still requires it.
Whichever document an implementer opens decides whether the channel returns, and the model is the
one that owns the model.

**Substitutive layers serve exact counts down to one.** Declaring a layer substitutive switches
`min_visible_members` off entirely, and the representation's visibility predicate has no
count-threshold branch on that path. The model's own boundary example declares a substitutive layer
*and* instructs that the threshold be applied to its counts with rollup. Implemented as the
representation states it, every UK boundary is served to every principal with an exact masked count
— which the same example's trap paragraph identifies as worse than the threshold-of-one it warns
against.

## 2. The structural blocker: where artifact entity IDs come from

Artifacts take entity IDs so the deny lane, the overlay and `tessera_id` work on them unchanged, and
a level takes a **contiguous run** so its ordinals are arithmetic. Three point-side structures are
dense over entity *ranges* rather than counts — verified in the code: a flush or merge extent's row
table is dense over `[entity_lo, entity_hi]`; a merge allocates one slot per entity across its whole
window; and the fold's pre-flight estimate charges four bytes per entity up to the highest one, then
**refuses the fold** when the total exceeds the host.

So a ten-million-wide run allocated mid-stream punches a hole that every straddling merge pays for
in resident memory, that every fold pays for in its budget, and that never comes back down. Nightly
regeneration walks that budget upward until the fold refuses — and the fold is the only thing that
executes deletions and reclaims dropped layers. Fail-closed, and a slow stall of the whole
retirement path whose cause is nowhere near the symptom.

**This is upstream of the addressing scheme**, so it is ruled before the storage layout, not after.

## 3. Not an artifact finding: decision 0072 is settled and unbuilt

The allocator is monotone with no free list and refuses at `u32::MAX`; `tessera_id` carries no
generation field. **Verified.** Both reviewers who touched exhaustion assumed the decision was in
force — the representation cites it in the present tense — and one of the fail-opens above only
exists once it is. As things stand a 10⁷-artifact layer regenerated daily consumes the identifier
space in about fourteen months, after which every write refuses, points included. This belongs to
the corpus rather than to this design: mark the citation specified-not-implemented
(decision 0013) and decide separately when
0072 is built.

## 4. Where the model is undecided rather than wrong

Each of these is one contradiction with two live readings, and each has a worked example depending
on the reading the other example refutes.

| | The contradiction | Why it matters |
|---|---|---|
| **The threshold's owner** | declared *independently of gate mode* in one section, *switched off by* substitutive in three others | the boundary example needs the first, the selections example needs the second, and the register row aimed at the gate mode may be aimed at the wrong field |
| **Label existence** | *"a viewer satisfying no version receives nothing"* against the general rule that an artifact keeps its identity and count when content is withheld | the flagship label layer's gate mode is written as *"derived — containment"*, which is not one of the three modes; a fourth — exists iff some version's containment passes — is what normative §7.6 actually does |
| **The filter axis** | the model deletes the descent and never mentions `M_sel` again; the representation restates the two-threshold frontier as live | under a filter nothing says what number sits beside an artifact, or what prunes an emptied cluster |
| **The count rule** | recast as *falling out of* the corpus-independent/derived matrix, but density cells and administrative boundaries occupy the same cell with opposite count rules | and a public polygon decomposes into Morton cells whose exact masked counts are served with no threshold, so a suppressed boundary count is recoverable by summing the underlay |

**Multi-version membership is undefined**, and it reintroduces the defect the model used to reject
`reach`: a label's membership is *"the sample it was generated from"*, versions carry different
samples, one membership may be declared — so the count served beside a resolved version describes a
set that version was not generated from.

## 5. What the code will not support as written

- **One membership file per artifact** at 10⁷ artifacts means 10⁷ manifest entries, each digested at
  write and parsed at open. The membership itself is affordable (794 MB measured); the packaging is
  not. The record blob's block-directory shape already solves this.
- **Editing content in place has no route.** Supplied content is placed in the record blob, whose
  layers are disjoint by construction and probed base-first — there is no shadowing and no update. An
  implementer writing an edit as a new layer gets the **pre-edit text served silently**, which breaks
  the model's emergency path (suppress, edit the leaking version out, unsuppress) on the one route
  that exists for withdrawing a leak.
- **Runtime-created artifacts have nowhere durable to live.** The only holding pen between a WAL
  append and a build is the ingest buffer, which is drained by acquiring geometry — and an artifact
  never acquires a row. Placing one there pins WAL rotation permanently.
- **The layer registry's template is neither built nor approved.** It is specified as the slice
  lifecycle's shape *"verbatim, cited rather than re-derived"*; that lifecycle exists only as a
  provisional, explicitly-unapproved design with no code behind it.
- **The fold's cheaper artifact-pass construction rests on a stream the fold does not emit** — it
  assumes old-row order, and the fold emits in new-row order. The comparison the representation owes
  a measurement for is currently between the wrong two things.

## 6. The rulings this asks for, in dependency order

1. **Where artifact entity IDs come from** — the same monotone space with holes, a separate region,
   or a cap on merge-window width. Blocks addressing, storage layout and the fold. (§2)
2. **Is the count threshold independent of the gate mode?** Independence is the smaller model and the
   only reading under which both worked examples survive. Settles a fail-open and a contradiction.
3. **Does a label's existence follow its content's containment?** — the fourth mode, which is what
   the normative label rule already does. Settles the C3-for-labels claim.
4. **Where does supplied content live** — the record blob, or the level's own file? Decides whether
   in-place edits and the emergency path exist at all.
5. **Search gates on containment**, and what the artifact population's authoritative candidate set is,
   given it is never `M_auth`.

Below those, and cheaper: delete the model's build-time box; give the fold's Rule F arm a membership
clause; state the filter axis; pick a packaging for membership.

## 7. Checked, and does not bite

`reach` (never served, masked at use, pruning only removes); predicate membership's disclosure
(masked throughout — its open item is cost); the ordinal-gap channel (closed by the wire ruling);
labels of a suppressed cluster on non-traversal routes (closed by the representation's amendment); a
stale bookmark resolving to a recycled slot (closed by 0072's discriminator, *once built*); the
record blob's addressing and the 28–118× row-space result (nothing in the code contradicts either).

Two reviewers noted that partitions appear nowhere in either document, so containment, the threshold
and edge traversal each acquire an unwritten I13b obligation at the first second partition. It does
not bite while one partition exists.
