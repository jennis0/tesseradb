# Annotations — the write cycle

**Date:** 2026-08-15 · **Promoted:** 2026-08-16
**Status:** **Normative for the annotation write cycle** — how artifacts, levels and layers behave
under write: their own operations (spec §5–§6), and what a point-side event obliges artifact-side
(spec §2–§4). Reviewed under three lenses (2026-08-15: disclosure, write-path integration, data
modelling) and ruled by decisions
[0074](../decisions/0074-row-less-entities-are-allocated-downward.md)–[0083](../decisions/0083-the-frontier-is-a-request-time-budget.md).
Companion to [`annotations.md`](annotations.md) (the model) and
[`annotation-representation.md`](annotation-representation.md) (the representation).
**Supersedes in scope** the sections listed in spec §10 — where a sibling document disagrees with
this one on the write cycle, this one wins. [`write-path.md`](write-path.md) is normative for the
point-side write path and this defers to it; `architecture.md` remains the specification.
**Review found four fail-opens**, all closed in the text: the fold shrinking a generating set
through its row form; a missing merge arm; retirement retiring the entry that hid a deleted artifact
before dropping what still served it; and — in the sibling representation document — a suppressed
cluster's labels still serving on routes that do not traverse the edge.
**⊘ Open inside a normative document, allocated to stages rather than held against promotion:** spec
§8's costs are unmeasured, of which the three maintenance arms (spec §2) are the ones that could
refute the shape — **Stage 4's first measurement, not its last** — and spec §11's residue, plus the
deferred edit pass (Stage 7) and the proportional criterion's denominator for predicate membership
(Stage 6).
**Reads against:** design §4 (I1, I2, I3, I7, I8, I9, I10, I12), §7.6–§7.8, §11.2, Appendix C;
[`write-path.md`](write-path.md) §2–§5 (**normative** for the write path); [`compaction.md`](compaction.md)
§3–§5, §9; [`filter-index.md`](filter-index.md) §6; [`views-and-multi-table.md`](views-and-multi-table.md) §3;
decisions [0047](../decisions/0047-edit-is-delete-plus-reingest.md),
[0048](../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md),
[0043](../decisions/0043-geometry-maintenance-never-blocks-a-request.md),
[0044](../decisions/0044-invisible-means-stale-serve-plus-background-refresh.md).
**Citation convention:** unprefixed §n is the architecture design; `model §n` is `annotations.md`;
`rep §n` is `annotation-representation.md`; this document's own sections are **spec §n**.

> **Most of this is built** *(r10; it was none of it when this document was written)*. Layers,
> artifacts, generating sets, containment, the dependency edge and the fold's artifact pass all
> exist and are enforced, through Stage 5 of
> [`artifact-delivery.md`](../artifact-delivery.md). What is not built is named at its own site:
> the **edit pass** (deferred to Stage 7), **membership by predicate** (Stage 6), content
> reclamation, and the notification *feed* — the fold writes its report, and nothing subscribes.
> Figures are marked *measured*, *modelled* or *assumed* at each site; most of this document's own
> quantities are still assumed, and spec §8 names them.

---

## 1. Two doors, and no third

A point event — ingest, delete, suppress, unsuppress; there is no update (spec §2.1) — reaches an artifact through exactly
two structures, and the design's safety argument is that there is no third:

- **The mask.** Every count, hull, centroid, criterion and candidacy test is a function of
  `membership ∩ M_auth`, composed live per request (I2, rep §2.1). A denied point leaves `M_auth`
  at the moment its deny is acknowledged (write-path §5.8), so every masked quantity is correct from
  the ack onward with **no artifact-side work at all**.
- **The generating set.** Supplied corpus-derived content — a label, a summary, an authored hull —
  is served on containment: `and_cardinality(G, M_auth) == |G|` (model §4). `G` is the one place an
  artifact stores a reference to *specific* points, so it is the one place a point event can leave a
  stored artifact-side fact wrong.

Nothing else artifact-side holds any point-derived quantity. No unmasked count, no build-time box,
no corpus-wide anything (rep §2.4) — so nothing else can go stale, and that is why most cells of
spec §3's cross-product are honestly *nothing*.

The failure this document was commissioned for lived in the second door. The previous draft
(rep §5.0.3) stored `G` in row space and had the fold **remap it** when a delete executed: `G` lost
the member, `|G|` shrank to match, containment passed again, and content derived from a deleted
item was served to everyone — deletion ending up weaker than suppression, flipping at a nightly
window. The repair is not a patch on the fold; it is that `G` was in the wrong space.

## 2. A generating set is an identity, and it lives in entity space

**Entity space is where truth is stored; row space is where every comparison happens.** A generating
set is a set of entity IDs on disk, never grown (I8) and shrunk only by the fold on a **permissive**
layer (spec §2.1, §7.6), mmapped only when specifically touched,
and the operator the containment test actually runs against is its row-space image, derived from that
truth and re-derived on the events that change row space *(owner ruling, 2026-08-15)*. Membership
takes the same shape (spec §4.1); this is one rule, not two.

**Row space extends at flush, so a derived operator has no lifecycle of its own.** Row space is the
base permutation plus an ordered extent list ([`write-path.md`](write-path.md) §4.4), and a flush
*extends* it — a projection over the new generation is the previous bitmap unioned with the new
extents' rows, *equal to* rather than an approximation of a projection over the whole space
([`concurrency-lifecycle.md`](concurrency-lifecycle.md) §2.2). So the maintenance an artifact operator
needs is the maintenance the mask projection already has, and it has **three** arms, not two:
**union in the new extents at flush, rebase over the merged span at a merge, rebuild at the fold**
(spec §4.1).

**The merge arm is the one a reader will leave out, and leaving it out is fail-open.** A merge is
row-count preserving in that no later extent's `row_base` moves, but *inside the merged span* it is a
merge-sort producing globally sorted output, so **a row id there names a different entity afterwards**
([`concurrency-lifecycle.md`](concurrency-lifecycle.md) §2.1). An operator holding extent rows across
a merge therefore names entities the caller never declared — spec §3.1's forbidden interloper, reached
by omission rather than by range translation. The rebase is bounded by the span, not by the corpus,
and it is the same rebase the projection already performs. ⊘ **Unbuilt and unmeasured**, like the
other two arms.

*An earlier revision of this document said the operator was "untouched by flush and by merge" and
covered base rows only. Both halves are gone: the base-rows-only rule was wrong on its premise
(flushed entities have rows), and "untouched by merge" was only true because of it.*

**The only entities without rows are buffered ones, and I4 already governs them**: *"a buffered entity
has no row and contributes to no row-space verb"*. That is not a gap this design opens — it is the
system-wide rule, and it delivers exactly what the ruling asks for. **An artifact's members follow the
same lifecycle as the documents themselves**: a document is invisible until its flush, and an artifact
declared over it is incomplete until the same flush, by the same mechanism, for the same one tick.
*(An earlier draft of this section had the operator cover base rows only, deferring recent members to
the fold. That was wrong on the premise — flushed entities have rows — and it invented a day-long
staleness where the system has a tick.)*

**Two properties do the safety work:**

- **Every operator is derived from the entity truth, member by member, never from its own
  predecessor.** Deriving each generation from the last is precisely what let the fold shrink `G`.
  Range-wise translation is equally forbidden, and for a second reason: it would sweep in a Morton
  neighbour the caller never declared (spec §3.1).
- **Containment compares against the declared cardinality, read from the immutable disk form.** A
  deleted member has no row, so it is absent from the derived operator; comparing that operator
  against its own size would pass on the survivors, which is the shrink wearing a different hat.
  Against `|G|` as declared, the test is unsatisfiable for ever — strict mode's behaviour, and the
  point. *(**Not** the count-freezing rejected at spec §2.2: there the count stood in for a set that
  was not retained, so an interloper could re-satisfy it. Here the set is on disk and the count is a
  fact read from it.)*

What entity space buys is that the containment test needs no guard, because both of its operands
already tell the truth:

- **`G` changes on exactly one event, and never by growing.** I8 forbids additions outright, so no
  later arrival is ever a member and the left operand cannot be widened. The single exception is a
  **permissive** layer losing a member, where the fold rewrites `G` without it (spec §2.1) — the
  branch I8's own body routes to §7.6 rather than prohibits. On a strict layer the fold does not
  touch `G` at all.
- **A deleted member never returns to the right operand.** While the delete is pending, the member
  is outside every composed mask by the overlay entry. At the fold that executes it, its postings
  are blanked (compaction pass 2) and its row is dropped, so it is in no `token_mask`, no buffer and
  no mask ever again. What guarantees no future point occupies its ID is
  [decision 0072](../decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md)'s
  reconciliation rule: a slot returns to the allocator only once every durable entity-space
  structure naming it has been dealt with in the same fold, and `G` is one of those structures —
  the fold already computes the list (spec §4.2) and must act on it. ⊘ Reuse is unbuilt; until it
  is, the allocator is monotone and the dead ID simply stays burned, which upholds the same
  property for free. Containment fails
  for every principal, permanently, until the caller republishes the content. The old design made
  the fold rewrite the test's left operand; this one leaves `G` alone and lets the fold finish
  moving the member out of the right operand. The test keeps failing, for the right reason, across
  the retirement handoff — the overlay entry hides the member up to the flip, the blanked postings
  hide it after, and the flip is one atomic publication (compaction §4).
- **A suppressed member round-trips.** Rule S touches no postings, so the member stays in `G` and
  leaves only the composed mask: containment fails for everyone while the suppression stands and
  resumes on unsuppress, when every source is again visible to whoever satisfies it. Suppression is
  temporary withholding; deletion is permanent withdrawal. The stronger act is now the stronger
  outcome.
- **`delete → suppress → unsuppress` on a member cannot resurrect the content**, inherited from the
  two-store overlay (write-path §5.3): the unsuppress mutates a store that does not hold the
  deletion.
- **Ingest changes nothing.** New entities are allocated above the high-water and are never in `G`
  (I8: later arrivals are not members). The Morton-neighbour interloper that defeated count-freezing
  (spec §2.2) does not exist in entity space, because entity identity is not positional.

### 2.1 There is no edit, so there is no edit question

**A caller cannot update a document.** `/control/changes` refuses the op with a typed 422, and what
exists is delete plus re-ingest, which mints a fresh entity and gives the caller no control that would
make it behave like an update (decision 0047). So the service has no *edited member* to reason about.
It has a deleted member and an unrelated new one, and treating those as a continuity problem — an
earlier draft bound `G` to caller keys to preserve one — solves a case the write path does not pose.

**What the caller may declare is narrower than it first appears: whether a generating set is
allowed to shrink** *(owner ruling, 2026-08-15)*. Containment is all-or-nothing, so a generating set
that loses a member fails for every principal, for ever — that is not a policy, it is what the test
does. The only question is whether a caller may say *this content survives that*, and it is declared
per layer:

| Mode | A member is deleted | The kind of object it is for |
|---|---|---|
| **Strict** *(default)* | the fold **drops the supplied content and its generating set**; the artifact — no longer declaring that content — thereafter serves whole with its derived content: existence, masked count, recomputed geometry | anything whose text was written from material including the deleted document: a summary, an authored description, a label over a curated set |
| **Permissive** | the fold **removes the member from the generating set** and the content goes on serving — unless the removal empties the set, in which case the content is withdrawn as under strict (§2.1's limit case, [decision 0107](../decisions/0107-a-generating-set-with-no-survivors-is-not-served.md)) | a toponymy label over a clustering — the sample is statistical, and one document leaving changes nothing the label asserted |

**Membership is not in scope and never was.** A deleted point simply leaves the artifact's
membership, the masked count falls, and derived content recomputes without it — spec §3.1's delete
row, unchanged and unaffected by the declaration. The artifact's identity is untouched, and its
existence changes only where the falling count crosses a declared criterion (spec §3.3). **Only
supplied corpus-derived content is at stake in this declaration**,
because only that carries a generating set.

**The shrink is performed by the fold, and I8 does not forbid it** *(owner ruling, 2026-08-15:
shrinking is explicitly what permissive is)*. I8's headline says a generating set is immutable, but
its body forbids only *growth* — *"items arriving later are not part of it and must not be added"* —
and routes the opposite case elsewhere: *"members leaving is an availability problem, addressed in
§7.6."* §7.6 requires explicit sign-off and a register entry before any shrink, and design r42 gives
both. So permissive is not an exception to the invariant; it is the branch the invariant already
pointed at.

**This is also what frees the slot.** [Decision 0072](../decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md)
requires every durable structure naming a slot to be dealt with before the allocator may reissue it,
and left "dealt with" undefined. For a permissive layer it is defined here: the slot is dropped from
`G` by the same fold that frees it. Without that ordering the rebuilt operator would resolve the
slot to its **new** occupant and re-satisfy containment — §2.2's re-satisfaction unsoundness, reached
by reuse instead of by Morton adjacency.

**Permissive is not available until the register says so, and that is the whole of its cost.**
Shrinking a generating set under deletion is **C7**, disposition **Not adopted** — *"a label
reflecting content the principal may never have been entitled to"* — and §7.6 requires explicit
sign-off and a register entry before any such shrink. The reason is exact: the label was written from
material that included the removed member, so a viewer who can see the survivors may receive content
drawn from a document they were never entitled to. Offering permissive as a declared mode **is**
adopting C7, narrowed from a service behaviour to a caller's declaration. ✔ **The register carries
it** — C7 is *Accepted — caller's declaration, strict by default*, and §7.6's closing requirement of
explicit sign-off is discharged at design r42 (owner ruling, 2026-08-15). Strict remaining the
default is what the narrowing rests on: an undeclared layer never shrinks, and the service shrinks
nothing on its own initiative in either mode.

**One action, two outcomes, and neither destroys the artifact.** The fold removes the deleted entity
from every generating set naming it — that much is uniform — and the layer's declaration decides what
happens to the content that set generated: **strict** drops it along with the set, **permissive**
keeps it and serves it from the smaller set. In both cases the artifact's identity is untouched and
it serves whole again from the fold, and in both cases nothing durable still names the deleted
entity.

**Permissive says a content survives its survivors, and with none it does not survive**
*(owner ruling, 2026-08-30; [decision 0107](../decisions/0107-a-generating-set-with-no-survivors-is-not-served.md))*.
Where the deletion takes a content's **last** source, the fold withdraws the content rather than
leaving it on the empty set — and the artifact then follows §2.1's own rule, absent until the caller
republishes if its layer declares supplied content and this was the last of it. The reason is the
one the publication gate already states: containment is a subset test, and **the empty set is
contained in every mask**, so a content retained on it would be served to every principal who can
see any member of the artifact — corpus-derived text drawn from a document they were never entitled
to, which is exactly what C7's bound excludes and exactly what an empty set removes the bound from.
Publication refuses such content on the way in for that reason; the fold was the only other route to
the state. The outcome is identical to a strict withdrawal, so nothing new is expressible: what
permissive changes remains what it says above, for every case where a member remains.

**Content requiring only inherited visibility is untouched by any of this.** It carries no
generating set at all — publication refuses one, C28 — so it names none of the entities a fold
retires and neither the shrink nor the withdrawal reaches it.

**That second property is the whole of [decision 0072](../decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md)'s
reconciliation.** A freed slot is dangerous only while something durable names it and could resolve
it to the next occupant. Strict drops the set; permissive shrinks it. **The ordering is the safety
property** — reconcile, then reclaim. An earlier revision reached the same end through a stored
`content_withdrawn` bit; it is deleted, because a bit that must be set correctly is a bit that can be
set wrongly, and removing the member needs no state at all.

**Where the fold does it, and why not earlier.** §4.2's sweep already computes
`and_cardinality(G, D₀)` per `G`-bearing artifact to produce the caller's report; it now also
performs the drop or the shrink, in the same publication. The deny lane is not the place: finding
which generating sets name an entity is the inverted lookup §4.5 exists to avoid, and doing it at
accept would cost the lane its O(window) bound.

**So the content vanishes at the ack and, under permissive, returns at the fold.** Between the two
the deleted member is outside every mask and containment fails for everyone — fail-closed, and
identical under both declarations. They diverge only at the fold, where strict makes the withholding
permanent by dropping the content and permissive ends it. **And what the interim withholds is the
artifact's service, not merely its text**
([decision 0076](../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)): the content
whose generating set lost the member is unservable, so the **artifact** is absent to any viewer no
other entry of the ranked contents covers. Fail-closed and deliberate — the artifact's *identity* is never destroyed
by a point event; its service resumes at the fold under either declaration. ⊘ **A caller reading
"permissive" as "nothing changes" will be surprised by that gap**, which is up to one fold long.

**Strict is the default, and it is also what containment does unaided.** A layer that declares
nothing gets the behaviour the test already has: the content stays withheld, and the fold merely
makes that permanent rather than leaving a set that could later be re-satisfied. Permissive is the
only declaration that changes an outcome, which is why it is the one the register carries.

**Longer term, if a real edit is ever adopted, it needs no artifact machinery** *(owner direction,
2026-08-15)*: editing a document has no effect on artifact visibility unless it changes the
document's required terms, and when it does the artifact behaves exactly as if it had been built with
a member carrying those terms from the start. That falls out rather than being built — containment is
evaluated live against current masks, so a member whose terms changed is simply tested under its new
terms. The one thing such an edit must supply is identity continuity, which delete-plus-re-ingest
does not have; that is the edit design's problem when it arrives, and nothing here should anticipate
it.

### 2.2 Why counting cannot work — rejected, recorded

Freezing `|G|` at declaration — making containment permanently unsatisfiable once the row-space set
shrank — was proposed and is **rejected** (owner, 2026-08-15): a cardinality is not an identity.
Row space renumbers at every fold and rows are Morton-ordered, so a newly ingested point in the
cluster's spatial neighbourhood is assigned a row inside the set's row range; any translation or
representation that is not exactly member-wise can regain a member, and the frozen count is then
*re-satisfied by a different point*. A scheme that proves "this set is still the declared set" by
counting is unsound. It is recorded because it is the obvious repair and will be proposed again.

### 2.3 What entity space does not catch

Entity identity proves the set still names what was declared. It proves nothing about whether the
**content** is honest when every member survives — a caller who declares an optimistic `G`, or
re-declares old text over a shrunken one, is C12's row (caller's control), unverifiable by the
service in any representation. The mechanical guard this document does add: **content and generating
set are declared, and edited, together** (rep §5.0.1's rule, kept; ⊘ the edit verb itself is
deferred to the edit pass, which inherits the rule) — a `G` edit
that does not accompany a content edit is refused, so re-basing a declaration is at least a
deliberate act.
Automatic shrinking of `G` by the service remains **not adopted** (C7, §7.6: the content was
generated from material including the removed member, and shrinking needs explicit caller sign-off).

### 2.4 Alternatives considered

| Alternative | What it costs | What it fails to catch |
|---|---|---|
| Frozen `\|G\|` (spec §2.2) | 4 bytes | re-satisfaction by an interloper — unsound, rejected |
| Digest over member identities | a hash beside `G`, plus the members anyway (containment still needs them) | nothing the entity set does not already catch; strictly dominated — a digest of an immutable entity set is a checksum of a thing that cannot drift |
| Declaration epoch, invalidated when a fold executes any delete | one counter | far too coarse: any deletion anywhere withdraws every supplied content in the corpus |
| Per-artifact degraded flag, set by the fold | a flag plus the sweep that maintains it | safety, if it is the mechanism: the flag arrives at the fold, up to a day after the delete — the interim would be open. Kept only as the **report** (spec §4.2), where the interim is already closed by the mask |
| Withdrawal + notification, no stored state | nothing | this is the design: withdrawal is emergent from containment, notification is the fold's report |

## 3. The cross-product

Axes: point events (ingest; delete; suppress;
unsuppress) × membership source (rep §2.0: enumerated, spatial predicate, attribute predicate) ×
content kind (model §4.1: derived; supplied corpus-derived with `G`; supplied corpus-independent) ×
the two gate controls (model §5: the own-terms flag and the existence criterion, independent
conjuncts — [decision 0079](../decisions/0079-the-gate-is-one-flag-not-three-modes.md)).
**Ranked contents are a fifth axis and
they change exactly one cell** (§3.2's delete row): each entry carries its own generating set, so
a deletion consumes the entries it touches and a viewer falls through the ranking — to the next
entry they satisfy, or to no artifact at all
([decision 0076](../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)). Content kind is
orthogonal to
membership source: `G` is always an enumerated entity set, whatever the membership is.

The vocabulary of answers: **nothing** (and why nothing is safe), **recompute** (per request, by
construction), **withdraw the artifact from viewers the surviving contents do not cover**,
**refuse the write**, **notify the caller**. No cell destroys an artifact's identity: a point event
may withhold an artifact's service, never end the object.

### 3.1 Point event × membership source — what happens to membership, counts and the criterion

| Event | Enumerated | Spatial predicate | Attribute predicate |
|---|---|---|---|
| **Ingest** | **nothing** — membership is frozen at declaration (I8's shape); the point is in no artifact until the layer is refreshed. Safe: counts only ever *understate* what a refresh would show — fail-closed, stale-not-unsafe (rep §2.2.1) | **nothing stored; the point is a member at its flush** — membership is derived per request from the geometry, so the row's arrival is the whole event (rep §2.0). Earlier than flush is impossible: counts are row-space and the point has no row | as spatial: the value column's flush extents carry it; the filter machinery answers per request ([`filter-index.md`](filter-index.md)) |
| **Delete** | at accept: the entity leaves every composed mask, so every masked count, hull and criterion test is correct with no artifact work. At the fold: the row form drops the member's bit in translation (spec §4.1) | at accept: the row leaves `denied[view]`'s complement; `range_cardinality` over the mask is correct immediately. At the fold: rows renumber and the ranges re-derive per request — nothing stored, nothing stale | at accept: composed verdict excludes it (filter-index §6.1). At the fold: the attribute pass blanks the slot (filter-index §6.2). All existing machinery |
| **Suppress** | **nothing artifact-side** — the member leaves the composed mask at accept; counts fall, hulls recompute without it, an artifact may drop below its criterion and vanish. Rule S: no stored structure changes, postings and membership untouched | same — the mask is the only operand that moves | same |
| **Unsuppress** | **nothing** — the member returns to the composed mask at accept; counts restore. Reappearance above a *cached* criterion decision lags fail-closed (spec §4.4) | same | same |
| **Update** (delete + re-ingest) | the union of the two rows: the old life leaves masks at accept and leaves the row form at the fold; the new life is a new entity, in no enumerated membership until the layer is refreshed. **A caller must expect enumerated counts to drift down under churn** — a refresh is the repair (⊘ by replacement today, by edit once that pass lands) | the old row leaves, the new life's row is inside or outside the shape on its own coordinates from its flush — a point that moved across a boundary changes membership correctly, no artifact work | same, via the new value |

Two rules the enumerated column rests on, stated because each is one slip from a hole:

- **Publish-time member validation.** A declared membership or generating set naming a **deleted**
  entity is refused (422 naming the ids): the member can never contribute to a count and, in `G`,
  makes the content unservable from birth — better a loud refusal than discovered silence. Naming a
  **suppressed** entity is accepted: it is a live member, temporarily outside every mask, and both
  structures behave fail-closed until the unsuppress. One `verdict` lookup per declared member, on
  the build plane.
- **Row-form translation is member-wise, never range-wise** (spec §4.1). A range translation of a
  run container would capture newly ingested Morton neighbours — undeclared members whose visible
  count could lift an artifact across the existence criterion, weakening C1's control. The
  per-artifact construction rep §5.0.3 costs is member-wise by construction; that property, not its
  memory shape, is why it is required.

### 3.2 Point event × content kind

| Event | Derived (count, centroid, hull, extractive terms) | Supplied, corpus-derived (`G` declared) | Supplied, corpus-independent (`G` empty) |
|---|---|---|---|
| **Ingest** | **recompute per request** — nothing stored, so nothing to do; the new member (predicate sources) or non-member (enumerated) is simply in or out of `membership ∩ M_auth` | **nothing** — later arrivals are never in `G` (I8); the content is stale, not unsafe (§7.6) | **nothing** — the content asserts nothing about the corpus |
| **Delete** of a member | correct at accept via the mask; nothing stored | withheld at accept — emergent from containment, no stored state. Then the layer's declaration decides (spec §2.1): **strict** *(default)* drops that content and its set at the fold, **permissive** removes the member and resumes serving. **Where the artifact carries ranked contents** (model §2.3), the withholding applies to the entry whose set lost the member and the viewer **falls through to the next entry they satisfy**; a viewer no surviving entry covers sees **no artifact** until the fold ([decision 0076](../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)). **Notify the caller at the fold** either way (spec §4.2) | **nothing** — an empty `G` intersects nothing |
| **Suppress** of a member | correct at accept | **withhold content while the suppression stands** — containment fails for every principal; resumes on unsuppress. No stored change (Rule S) | **nothing** |
| **Unsuppress** | correct at accept | content serves again, to exactly those satisfying `G` — every source visible again | **nothing** |
| **Update** of a member — *not an operation; delete + an unrelated ingest* | correct at accept / at the new life's flush | **exactly the delete row**, because that is all the write path performs (spec §2.1). Under **strict** the content is dropped at the fold; under **permissive** the member leaves `G`. Notified at the fold | **nothing** |

The corpus-independent column is all *nothing*, and nothing is the correct answer — the standing
hazard in that column is a mis-declared independence (a fitted centre declared corpus-independent),
which is C12's class and a declaration-time problem (model §4.2), not a point-event one.

### 3.3 Point event × the two gate controls — what happens to existence

The own-terms flag and the existence criterion are independent conjuncts
([decision 0079](../decisions/0079-the-gate-is-one-flag-not-three-modes.md)); a layer declaring
both takes both columns.

| Event | Existence criterion, where declared | Own terms, where flagged |
|---|---|---|
| **Ingest** | enumerated: no movement (count frozen until the layer is refreshed). Predicate: the count may cross the criterion at the point's flush and the artifact appears — the criterion meaning what it says about a corpus that genuinely grew | **nothing** — own terms never derive from the corpus; the count beside the artifact moves with the mask and may be 0 (model §8.5) |
| **Delete / suppress** of members | the live count falls at accept; an artifact below the criterion is **omitted at the next request** (spec §4.4's live-count rule) | **nothing** — the terms alone decide this conjunct; the count beside the artifact falls at accept |
| **Unsuppress** | count recovers at accept; reappearance above a cached candidacy lags fail-closed (spec §4.4) | nothing |

### 3.4 Timing — what happens when, and why that is the earliest sound point

| Consequence | When | Why this point and not another |
|---|---|---|
| A denied point leaves every count, hull, criterion and containment test | **accept** | the ack asserts the disposition is in force (write-path §5.2); any later point serves a hidden item inside an aggregate — fail-open |
| A **permissive** layer's `G` loses the deleted member, and its content serves again | **the fold** (spec §2.1) | the interim is fail-closed — containment fails for everyone while the member is denied — so the only cost of waiting is availability. Earlier is the deny lane, where finding the affected sets is the inverted lookup §4.5 exists to avoid; the fold already computes the list for §4.2's report |
| An ingested point enters predicate membership | **its flush** | counts are row-space questions and the point has no row before flush (§11.2); earlier is impossible, later is a gratuitous staleness |
| An ingested point enters enumerated membership | **its flush**, as a predicate membership does — the point joins and the artifact then behaves as though it had been there all along ([decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md)) | a build reading a member table has always entered points into an enumerated membership, so a build is the same operation at the other entry point. This row read **never** and cited I8; I8 governs a *generating set*, which is a different set, is never grown either way, and was not at stake. The growth mechanism is built (`artifacts-from-points.md` §6.1): a delta record, one store method taken by both the live path and replay, and a log pin that only the fold's whole rewrite releases — the packing rule is what keeps a grown record reachable after a restart, since a level is packed only above its published high-water. **The wire carries it too** (§6.2, contracts r36): an ingest batch may name a point's artifacts in a column named for the layer, and the join is appended inside the batch's own commit window, so the membership is durable in the same fsync as the row. **And a key naming no artifact creates one** where the layer's `value_set` is open (§6.3, contracts r37) — at the window's close, as a publication carrying the points that named it, its lineage linked parent before child in the same batch. Nothing a member table can say is now unsayable on the wire, which is what this row waited on |
| An artifact's row operator is rebased over the merged span | **the merge that publishes it** | a merge permutes row space inside its span, so a row id there names a different entity afterwards (lifecycle §2.1); an operator holding extent rows is wrong from the publication until it is rebased. Entity space is untouched, so the ground truth needs nothing |
| Membership row forms reconcile with executed deletes | **the fold** (rep §5.0.3's pass, minus `G`) | the interim is already enforced by the overlay — the fold changes what is *stored*, never what is *served*, so its timing is an efficiency, not a safety property |
| A deleted `G` member's exclusion becomes structural (postings blanked) | **the fold** | the deny entry enforces it until the flip; Rule F retires the entry in the same publication that blanks the postings — no gap (compaction §4) |
| Rule F's artifact arm — the deleted artifact's record leaves its level, its ordinal left as a hole | **the fold, in the publication that retires the overlay entry** | **not reclamation — retirement's precondition.** A deleted artifact has no rows and no postings, so compaction's derivation would otherwise place it in `executed` *vacuously* at the first fold, retiring the overlay entry that is the only thing hiding it while its slot still serves. A hole rather than a closed gap because an ordinal is identity. **Edges into it are answered by the predicate rather than rewritten** — an attachment must resolve, and a hole resolves to nothing; dropping the edge instead would leave the label *unattached*, which serves it (rep §5.0.3) |
| The degraded-content report | **the fold's publication**, before retirement | write-path §5.8's obligation; the interim is fail-closed, so report latency is operability, never safety |

The acceptance test for the whole table: **no correctness-bearing consequence waits for the fold.**
The fold reconciles structure and reports; everything a viewer must not see is already unreachable
at the ack that hid it. The superseded draft failed exactly this test — the safety of supplied
content depended on what the fold did to `G`.

## 4. Rules the tables rest on

### 4.1 Membership: entity-canonical, base-row accelerated

The disk-canonical membership is entity space (rep §2.4); the resident row form is derived (rep
§2.1) and is rebuilt inside the fold (rep §5.0.3). This document adds the boundary condition the
representation left implicit: **the row form covers members holding base rows; a member whose row
is still in a flush extent contributes nothing until the fold folds it.** That keeps the row form
untouched by flush (appends move no bits) and by merge (only extent rows renumber, and the form
references none), at the price of understating a count for members ingested since the last fold —
fail-closed, the same posture as a buffered point being invisible until its flush, and typically
zero for clusterings, whose members predate the layer. A deployment whose selections need same-day
counts over fresh ingests can direct-evaluate the residue in entity space — I1's `direct_eval(L)`
shape, one level up — recorded as the refinement, not the default. ⊘ The refinement is not built.

**This is what removes the flush-union and merge-rebase arms rather than deferring them.** An
earlier plan gave the row operator three arms — union the new extents at a flush, rebase over the
merged span at a merge, rebuild at the fold — and named the merge arm as the one a reader leaves
out, fail-open when left out because a merged span's row ids name different entities afterwards. A
form that references no extent row has no such state: a flush appends rows it does not hold and a
merge renumbers rows it does not hold, so **the fold is the only operation that invalidates it**,
and the fold rebuilds it inline (rep §5.0.3). The projection is therefore keyed by prefix, view and
store version and *not* by the segments version — keying on the version a flush moves would rebuild
every level on every flush, tens of seconds per level at 10⁷ artifacts, for a set of bits that did
not move.

### 4.2 The fold's report is the notification mechanism

The fold already holds `D₀` (the plan's tombstone clone) in entity space, and `G` is now entity
space, so *which supplied content lost a member* is one small `and_cardinality(G, D₀)` per
`G`-bearing artifact — no traversal coupling, no inverted index (superseding rep §5.0.3's by-product
construction, which needed the artifact pass to visit every pair). The report — artifact key
or address, per lost-member content — is written as part of the fold's own publication, which is
what discharges write-path §5.8's rule that a deletion is not retired before the notification
obligation is; it is the first concrete content of §2.5's label-invalidation feed (⊘ the feed
itself is not built — the report is a file and an operator-plane accessor, not a subscription). The
report is control-plane, behind the operator credential, and out of the leak register's viewer scope
(decision 0024). The same sweep annotates enumerated *membership* losses for the caller's refresh
planning.

Two things the sweep's placement decides, both of which read as detail and are not:

- **The set swept against is what this fold *executes*, not every tombstone it holds.** A deletion
  whose entity is carried forward has not retired, so its notice is not yet owed; reporting it here
  would tell a caller their content was degraded by a deletion the fold left in force, and the next
  fold would tell them again.
- **The report is written before the flip, and a report that cannot be written discards the fold.**
  That is the whole content of "retirement and report in one publication" (§7): the alternative
  orderings either retire an unreported deletion or report one that did not retire. Nothing is lost
  by discarding — the deletions are in force from their own ack, and the next fold reports them.
  It lands **outside** the prefix, because a fold reclaims the prefix it supersedes and a notice
  written inside one would be deleted by the fold after next, taking with it exactly the notice a
  caller had not yet read.

### 4.3 Containment composes the verdict, and caches nothing

The containment test's right operand is the **composed** mask — the fragment adjusted by the
overlay — never a raw fragment. A raw fragment always contains a suppressed member (Rule S folds
nothing, filter-index §6.1), so testing against it serves content derived from a hidden item: the
exact fail-open, one operand away. And no route caches a containment result across requests — I3
verbatim ("nothing caching a label decision above the check"), restated here because a
version-keyed artifact cache is exactly where a plausible implementation would put one, and a
cached *pass* survives the member's suppression. The test is one `and_cardinality` over a small set
per supplied content per request; spec §8 sizes it.

### 4.4 The criterion is enforced on the live count; caches are candidates, never authorities

The session's resolved visibility set (rep §8, *measured* 883 ms at 10⁷) is computed from masked
counts that a later point deny changes without moving any layer version — so a cached "visible"
must never be served as the decision. The rule, in the shape write-path §5.8 already uses for
points ("the mask is applied after every cache"):

> A serving route takes candidates from the resolved set, computes the masked count it was going to
> serve anyway, and enforces the existence criterion on **that** count. `verdict` on the artifact's
> own entity is checked live on every route, first (rep §4) — a cache may bake in *counts*; it must
> never bake in *verdicts*.

The viewport pays nothing for this: it already computes the masked count per served artifact,
because the count is the answer. Staleness then runs only fail-closed: an artifact whose count
*recovered* (unsuppress) or whose pass a candidacy snapshot missed stays absent until the
resolved set refreshes — bounded by the set's invalidation events and the session's life, stated
rather than hidden. One route does not fit this shape: drill-down by identifier consumes the
resolved set precisely to avoid per-identifier count work (rep §8's C4 closure), and re-checking
the count there reopens the timing channel that route exists to close. Unsettled; carried in spec
§11 with the options.

### 4.5 The deny lane does no artifact work

A point deny's accept path gains nothing from this design: no sweep, no `G` intersection, no
artifact lookup. Everything at accept is emergent from mask composition the deny already performs,
and detection-for-notification is deferred to the fold (spec §4.2). The lane stays O(window)
(write-path §5.2), which is what "never refused for load" costs.

## 5. Operations on artifacts

Artifacts have entity IDs (rep §4), so the deny lane, the WAL, the overlay snapshot, recovery and
both removal rules already work on them **unchanged** — an artifact suppression survives rotation
and restart by the same snapshot machinery a point's does, and no new durability mechanism exists
in this document.

| Operation | Route | Atomic unit | Durable record | Caller told | Refused when |
|---|---|---|---|---|---|
| **Create** (runtime — a selection, a correction) | its own control verb (rep §5.1; ⊘ contracts work): `(layer, membership, gate, content, key?)`, members named by `external_id` or `tessera_id`+idset, resolved to entities at admission exactly as `/control/changes` resolves (write-path §5.1) | one artifact | WAL record carrying the resolved entity forms | 200 with the artifact's `tessera_id` — a durability receipt that is also **eligibility at ack**: an artifact has no geometry of its own, so no flush stands between it and visibility; when a given session first surfaces it is bounded by the resolved set's refresh (spec §4.4), fail-closed | member validation (spec §3.1); layer gate unevaluable; corpus-derived supplied content without a `G`; corpus-independent content declaring one; edge target absent (rep §5.0.4); the level's reserved entity run exhausted — extended by appending the next block **downward**, which cannot interleave with point segments ([decision 0074](../decisions/0074-row-less-entities-are-allocated-downward.md)), refused only if allocation fails |
| **Edit** — ⊘ **deferred to its own design pass** ([decision 0077](../decisions/0077-supplied-content-lives-in-the-record-blob.md)); until it lands the routes here are create, suppress, delete and layer replacement, and the shape below is what the pass inherits | rep §5.0.1's table: content in place (content and `G` together — spec §2.3); membership in place + version bump; gate widening in place + bump; gate narrowing = suppress, re-grant, unsuppress | one artifact | WAL record; **the version bump rides the same record** — a replayed edit without its bump would leave sessions on stale resolutions, so the two are one durable fact | 200 after fsync | a `G` edit without a content edit; membership edits fail spec §3.1's validation |
| **Suppress / unsuppress** | `/control/changes`, by the artifact's entity — no new API | the deny window | `ChangeByEntity`, snapshot at rotation — all existing | write-path §5.7's table verbatim | never for load (the lane's rule) |
| **Delete** | `/control/changes` | the deny window | as above; **Rule F's artifact arm** at the fold: membership file, `artifacts.arrow` slot, and every edge naming it are dropped (rep §5.0.3) | as above | — |

Consequences already ruled elsewhere, honoured here: a suppressed or deleted artifact is absent on
every route from the ack, `verdict` being checked first and live (rep §4); an edge into it is not
traversable (model §5's conjunctive rule), so labels on a suppressed cluster stop serving at the
same ack and resume at its unsuppress; and `delete → suppress → unsuppress` cannot resurrect it
(two stores, write-path §5.3).

**Wire identity: an artifact crosses the trust boundary as its `tessera_id` and nothing else.**
The model addresses artifacts as `(layer, level, ordinal)`, and the ordinal is
`entity − entity_base` (rep §2.3) — an affine image of the entity ID. On the wire it is an entity
ID with the base subtracted: dense, declaration-ordered, so the gap between two visible ordinals is
a count of artifacts the viewer cannot see (C6's channel, exactly), and the largest visible ordinal
bounds the level's size, which rep §9 already refuses to serve as a field. The address stays
internal — edges, files, the fold all use it — and the wire carries the keyed identity that already
exists for every entity (I10). Contradicts the model's addressing as read; ruling in spec §9.

## 6. Operations on levels and layers

| Operation | Route | What is durable | Refused when |
|---|---|---|---|
| **Layer create** | control verb; the view lifecycle's shape verbatim ([`views-and-multi-table.md`](views-and-multi-table.md) §3): WAL'd registry entry, served registry = manifest + WAL overlay. **Or a build input** — a `[[layer]]` block in the corpus declaration `tessera build` reads writes the registry section directly, running the same registry and allocator so both routes refuse and place identically; a build has no WAL, its manifest being the durable output | the registry record; **one entity ID is allocated to the layer itself** (below) | name in use **or tombstoned**; gate unevaluable; declaration refused at parse (empty term list, missing gate — rep §10) |
| **Level publish** (bulk) | build plane, `--attach-view`'s shape: `tessera build` reading a layer's `source` and its `[layer.members]` source, members named by source id and resolved through the build's own assignment, packed into the same membership and record extents a control-plane publication writes | the files, digested; the publication record | per-artifact validation (spec §3.1, §5); a level is **not atomic** — publishing artifacts is monotone and a partial level is coherent, merely incomplete (rep §5.0) |
| **Layer suppress / unsuppress** | `/control/changes` **on the layer's own entity** — which is why it has one. Rule S applies; the reachability check gains one live `verdict` lookup ahead of the session's resolved set | the existing deny machinery, end to end | never for load |
| **Layer drop** | WAL'd registry tombstone; vanishes from discovery at ack; artifacts reclaimed at the fold; **the name stays tombstoned for ever** | the tombstone | — |
| **Replace** (wholesale — changing the analysis, not refreshing it; [decision 0081](../decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md)) | create successor, then drop predecessor — never 10⁷ denies through a lane sized for trickle (rep §5). **Nothing carries across**: new identities are new objects, so suppressions, edges and bookmarks end with the predecessor, correctly — a replacement that strands live suppressions is **reported**, not refused (rep §5.0.2; ⊘ the report is unbuilt). **Not atomic, deliberately**: if the drop fails after the create, both generations serve — each individually gated and sound, so the intermediate state is duplication, never disclosure, and the caller retries the drop. The reverse order has an outage window and is not used. ⊘ Until the edit pass lands this is the only refresh, and callers should be told what it loses | the two registry records | the dangling-dependent refusal (rep §5.0.4), which binds replacement only |
| **Layer gate edit** | widening: in place, version bump. **Narrowing: in place, version bump — sound only under the rule below** — with layer suppression first when the narrowing is an emergency | WAL'd registry edit | — |
| **Level lifecycle ops** | **none exist.** Lifecycle is a layer property (model §2.1); a level is replaced by replacing its layer | — | a per-level suppress or drop is refused as an unknown operation |
| **Criterion or own-terms change on a populated layer** ([decision 0079](../decisions/0079-the-gate-is-one-flag-not-three-modes.md)) | by direction, with the criterion and the flag as the subjects. **Narrowing** — declaring or raising the criterion, setting the own-terms flag: in place, version bump. **Widening** — removing or lowering the criterion, clearing the flag: **the suppress–edit–unsuppress path, layer-level** — the same decomposition as an artifact gate narrowing, because the fail-open direction here is the widening | WAL'd registry edit | in-place widening is refused; the register row for the own-terms flag (model §4.2) accompanies the mechanism |

### 6.1 The build-plane inputs

**The full configuration surface is enumerated once**, in
[`configuration.md`](configuration.md) §1 — every block, every key and every
enumerated value word, including this section's layer keys. It is a closed set, and adding a key
without an entry there is adding a control nobody has reasoned about.

**One config file and one source per object.** The declaration `tessera build` reads declares the corpus,
its views, its vocabularies, its attributes and its layers together; every object that has data
names its own source, as a path relative to that document. [`configuration.md`](configuration.md) owns the declaration surface; what this section owns is the artifact and member grains and the rules
peculiar to them.

**A layer names its own source, and one file holds one layer.** That is what removes the
discriminator: there is no `layer` column to select on, no filter to configure, and no way for a
layer to ingest another's rows.

**A layer's `fields` map moves a field and the readers take the name it moved it to**, as every
other object's does. The two exceptions are `level` and `attached_level`, read under their own
names because [`configuration.md`](configuration.md) §1's tables do not name them — a level is an
address rather than a value, and the map's key set is that closed one.

```toml
[[layer]]
source     = "hdbscan"
fields     = { members = "members", parent = "parent_id" }
name       = "clusters/hdbscan"
views      = ["s0"]
membership = "enumerated"
hierarchy  = { kind = "nested", prune_children = true }

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = { fraction = 0.05 }
```

**A layer names one artifact source, and — where its memberships are too large to carry as a list
field — a second source for members** (`[layer.members]`, `configuration.md` §1). Two grains, and
the asymmetry is cardinality. An artifact source is **one row per artifact**,
carrying `contents` as a list ordered best first — the fallback chain, of which the viewer is
served the first entry whose sources they can see entirely, or nothing. A member source is **one
row per `(artifact, entity)`**: `key`, `entity`, and an optional `rank`, where a null rank is the
artifact's own membership and *k* is the generating set of `contents[k]`. Contents fold into the
artifact row because a fallback chain is two or three entries; members do not, because a condensed
tree's root holds the whole corpus and one cell carrying it can neither stream nor be materialised
by a producer.

Collapsing the artifact source to one row each is what retires the agreement refusal the old
`(artifact, rank)` grain needed: `key`, `parent` and the attachment were repeated on every row of
one artifact so that a single column could differ, and the build had to check the copies matched.
The refusal is *retired* rather than moved — with one row per artifact there are no copies to
disagree — and what the grain still admits, one key on two rows, is refused as two artifacts under
one name.
The member source's entity column is `entity` rather than `member`, a column named `member` on a
long source reading as though it should hold the whole membership.

**Membership by exclusion is an input spelling.** `fields = { excluding = … }` names the entities a
membership leaves out; the build complements once against the view's entity set and materialises
exactly the membership the included form would have produced, so the segment, the manifest and
every read path are byte-identical and never learn which way the source was written. It exists for
the producer: a tree's root is empty as an exclusion and the whole corpus as an inclusion, and the
clusters with large exclusion sets are the ones with short member lists. **Not** a complement taken
at request time, which would be a fourth membership source with the *"never stale"* character
`spatial` and `attribute` have — an artifact gaining members with nobody publishing to it.

**A layer may declare that a member deletion withdraws the whole artifact**
(`configuration.md` §1, `withdraw_on_member_deletion` on `[[layer]]`, default `false`). It is the
stronger form of the content-level rule this document's §3.1 already carries: where that one drops
supplied content and keeps the artifact, this drops the artifact at the same fold. It is a caller's
semantic declaration rather than a disclosure control — an artifact carries no residue of a deleted
member, its computed properties being recomputed per viewer — so the default is to keep it, and
`true` is for the artifact whose exact membership *is* the object. Withdrawal is a removal like any
other: what is attached to the artifact is governed by the dependency rules below — a replacement
refused rather than repaired, a deletion cascaded.

**Labels are declarable where they are used.** `[layer.labels]` expands to a layer of its own —
same views, flat, `depends_on` the parent, the content wrapper — because a label is a first-class
artifact with its own visibility and its own suppression, a synthesis being able to outrank its
sources in sensitivity. The expansion happens **before anything compiles**, so the sugared
declaration and the same layer written out produce a **byte-identical bundle** and nothing below
the parser has a label layer to treat differently. What the sugar supplies is mechanical; what it
never supplies is the gate, the membership requirement, or the existence of membership data, all of
which are written out. The label layer's `visibility` defaults to its parent's — a default on a
disclosure control, admissible because it is the parent's value rather than the widest one — and a
gate declared there is taken as written, nothing comparing it against the parent's. It does not
need to: a label is served only where the cluster it attaches to is served
([decision 0089](../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)), so a
gate declared here narrows what a principal sees and cannot widen it, `public` included
(`configuration.md` §1).

**Members are source ids**, resolved through the build's own assignment exactly as the access
relation's are; an id the build did not assign refuses the build. **Ordinals are assigned in
`(layer, level, key)` order**, so identity does not depend on how a source happened to be written,
and a key is required — it is what an edge into the layer names.

**Small layers need no data file at all**: `artifacts = [{ key = …, contents = [ … ] }]` inline, for
what a person authors rather than what a pipeline produces. It is a spelling and never a second kind
of layer, which the build asserts the hard way: an inline layer and the same layer read from a file
produce a **byte-identical bundle**, as do `excluding` and the inclusion it complements to, and a
membership on the artifact row and the same one in `[layer.members]`.

The layer entity exists for one reason: an operator discovering a leaking layer needs immediate,
reversible, fail-closed hiding, and a gate re-evaluated only at authorise cannot give it — a layer
suppressed mid-session would keep serving to every open session, the withdrawn model §7's fail-open
shape arriving at the layer. Riding the deny lane buys the ack semantics, the WAL, the snapshot,
recovery and Rule S for one allocated ID and one live bitmap check per request naming the layer.

The rule the two gate-edit rows depend on, found by this document's own fail-open pass:
**a session's resolved layer reachability is keyed on the layer version, exactly as its resolved
visibility set is (rep §5.0.1), and a gate edit bumps it** — the session re-evaluates the gate
lazily at its next touch, so a narrowing takes effect within one in-flight request rather than at
token expiry. Without this key, reachability resolved once at authorise would hold every open
session on the pre-edit gate for the session's remaining life — the same fail-open the layer entity
exists to close, arriving through the cache instead of the gate. Layer suppression remains the
emergency path: it acts at the ack, ahead of any resolution, and holds while the gate is edited
underneath it.

**Why a dangling dependent is refused rather than repaired, and why a deleted one is not:** the two
operations are different, and the boundary is the whole of the rule. **A replacement** mints new
identities, so repointing its dependents automatically fails open exactly when the caller reshaped
the layer — a renamed artifact, a split cluster — and each such failure attaches a label to a
cluster it was not generated from (I8's class). It is refused, and the operator performs the
reconciliation the service cannot verify. **A deletion** leaves nothing to point at, so nothing can
be mispointed: it cascades, deleting the artifacts that depend on the one deleted
([decision 0089](../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md), rule 1) —
which is the manual sequence the refusal already directs a caller to, performed by the service. A
cascaded deletion is a deletion in every respect: its own record, applied in the same window as the
deletion that caused it, retiring at the compaction fold that executes it (Rule F, write-path §5.4)
and by no other route. Suppressions take a third posture: a replacement ends the entities they
address, so they end too — the meaning of the operation, not an accident — and the operator is told
by the report rather than blocked. (Rep §5.0.2 and §5.0.4 carry the full arguments; this table only
inherits them.)

**Recovery**, stated once for the whole surface: registry ops and artifact records replay from the
WAL under the durable-prefix rules (write-path §1.3); artifact deny state rides the overlay
snapshot; a bulk publish that crashed before its manifest write leaves orphans and is re-run, and
one that crashed after is complete — the flush's own commit-point argument (write-path §4.4),
inherited by construction because the same publication seam is used.

## 7. What a point deny promises now, restated

Write-path §5.8's deletion-label bullet ("one deletion can dark-ship a node's whole nested chain…
the service owes the caller a notification… the fold must not retire a deletion before the
notification obligation is discharged") is implemented by this design as: withdrawal at the deny's
own ack (emergent, spec §2); the report at the fold's publication (spec §4.2); retirement and
report in one atomic publication. Nothing new is promised to the *writer* — the deny ack means what
it meant. What is newly stated for the **viewer**: an artifact vanishing this way is a signal that a
member was denied, and it is bounded to viewers who satisfied `G` — principals who could already see
every member — so it is C17's accepted shape (a delete/suppress signal on items the principal
already sees), not a new channel. Recommended as an annotation on C17 rather than a row; register deltas
below.

**Register deltas this document creates:** ⊘ **C7's disposition moves** if permissive mode is
offered (spec §2.1) — generating-set shrinking under deletion is currently *Not adopted*, and
offering it as a per-layer declaration adopts it, narrowed from a service behaviour to a caller's
choice; §7.6's requirement of explicit sign-off and a register entry is discharged by design r42.
✔ **Both are written** — C7's disposition and C17's annotation, r42, 2026-08-15. The ordinal finding (spec §5)
— resolved by never serving ordinals, else it is a C6-shaped row; and spec §4.4's drill-down
staleness, if the resolved-set route ships as drafted (a below-criterion artifact remains
inspectable by held identifier until the set refreshes — low, bounded to artifacts the principal
legitimately saw, but a row if accepted). The C3 question — withheld content distinguishable from
undeclared content — is **closed**
([decision 0076](../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)): nothing is
withheld from a served artifact, a failed containment absents the artifact whole, so there is no
shell to be distinguishable and C3 holds as written.

## 8. Costs

Nothing here is measured; each figure names what would refute it.

- **`G` storage** *(assumed)*: entity-space small sets, stored as sorted arrays beside the content
  (~4 B/member — scattered entity sets are the 28–118× loser as bitmaps, rep §2.1, so arrays, not
  Roaring), mmapped and touched only when the truth is needed — a refresh, the fold, an audit —
  so it is never resident in bulk. The derived row operator is the resident form and is sized with
  membership's, which is what rep §11.3's unpriced residency item now has to cover for both. The driver is Σ|G| over **supplied-content-bearing** artifacts, not artifact count: a
  label layer at 10⁵ labels × ~10²-member samples is ~40–400 MB; a deployment declaring per-term
  sets (§7.8) multiplies by the ladder width. Refuted if a real deployment's Σ|G| approaches
  membership's own order, at which point `G` wants the postings treatment (decision 0028).
- **Containment per request** *(assumed)*: O(|G|) array-vs-bitmap intersection per supplied content
  per served artifact; work identical for pass and fail (no early exit on the miss path — the
  timing must not distinguish which member failed). Refuted if profiling shows label-dense
  viewports paying materially; the fallback is per-session memoisation keyed on
  `(overlay_version, …)`, which spec §4.3 currently forbids and would need its own review.
- **Publish validation** *(modelled trivial)*: one `verdict` per declared member, build plane.
- **The fold's report sweep** *(modelled trivial)*: `and_cardinality(G, D₀)` per `G`-bearing
  artifact; `D₀` is sparse and each `G` small. Refuted with `G` storage.
- **The membership row-form pass** is rep §5.0.3's, unchanged by this document and still its
  largest unpriced item; removing `G` from it removes work.

## 9. Rulings needed

- ✔ **Entity space stores the truth; row space runs every comparison** (spec §2, §4.1) — **ruled**
  *(owner, 2026-08-15)*. Generating sets and membership take one rule, not two: immutable entity-space
  truth on disk, mmapped on touch; a derived row operator unioned forward at flush and rebuilt at the
  fold, on the mask projection's own cadence. Deleting a source point withdraws the content derived
  from it, for everyone, permanently, from the deny's ack. The frozen-`|G|` rule rep §12 requested a
  ruling on is withdrawn as unsound (spec §2.2), and the two soundness conditions at spec §2 are not
  optional: derive member-wise from truth, and compare against truth's cardinality.
- ~~**An edit of a source document withdraws supplied content derived from it**~~ — **withdrawn as
  a question** *(owner, 2026-08-15)*. There is no edit: the write path offers delete plus re-ingest
  and no control that would make it behave like one, so the service has no continuity to preserve or
  break (spec §2.1). What replaces it is a **caller declaration**: strict, where a deleted member
  withdraws the artifact, or permissive, where the member simply leaves the set. **Strict is the
  default.** ✔ **C7's disposition is changed and the register carries it** (design r42, 2026-08-15),
  which discharges §7.6's requirement of explicit sign-off; nothing further is needed to offer
  permissive.
- ✔ **Artifacts cross the wire as `tessera_id` only; ordinals never leave the server** (spec §5) —
  **ruled** *(owner, 2026-08-15)*. Clients bookmark and address artifacts exactly as they do points;
  `(layer, level, ordinal)` becomes internal addressing. **Split by audience is permitted**: the
  control plane may carry the structured address, being outside the register's viewer scope
  (decision 0024), and that is where an operator naming an artifact should read it.
- ✔ **Layers take an entity ID at create so that layer suppression rides the deny lane** (spec §6) —
  **ruled, accepted** *(owner, 2026-08-15)*. An operator can hide a leaking layer immediately and
  reversibly, with deny-grade durability, for one ID and one bitmap check per request.

  The ruling raised a wider question, **deliberately not answered here**: whether *every* addressable
  object — views, levels, edges — should carry an entity ID, so that one deny lane, one WAL and one
  removal rule serve all of them, and so that mixed object types can share a single bitmap. It is
  attractive and it is not this document's to settle, because the binding constraint is neither
  layout nor uniformity but **budget**: entity IDs are `u32`, reuse is settled and unbuilt
  ([decision 0072](../decisions/0072-entity-ids-are-slots-and-are-reused-after-a-fold.md) — the
  allocator today is monotone with no free list), and the burn is a property of **replacement**: a
  wholesale-replaced 10⁷-artifact layer mints 10⁷ fresh IDs per replacement (rep §5.2), while an
  edit, once that pass lands, spends nothing
  ([decision 0081](../decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md)). Daily
  replacement spends the space in about a
  year *before* levels, edges and views are added to it, and widening past `u32` leaves the 32-bit
  Roaring substrate every figure in the campaign was measured on. ⊘ **Needs its own design pass, and
  the question to put to it is what the entity budget is, not where type bits go.**

## 10. What this supersedes and contradicts

For mechanical integration; neither sibling document is edited here.

| Where | Disposition |
|---|---|
| rep §5, lifecycle table — the point-event rows ("nothing for derived content; supplied content degrades (§5.0.3)") | **superseded** by spec §3's tables: supplied content is *withdrawn at the deny's accept*, and the fold's role is report and reclamation only |
| rep §5.0.3, from "Supplied content is the half that does not hold" to the end of the freeze-`\|G\|` and by-product paragraphs | **superseded**: the re-base cannot occur because `G` has no row form; the freeze rule is withdrawn (spec §2.2); the degraded-artifact report is spec §4.2's construction. The pass's membership half, its two constructions and its Rule F artifact arm all **stand**, minus `G` |
| rep §12's freeze-`\|G\|` ruling request | **withdrawn**, replaced by spec §9's first ruling |
| rep §8, route 2 (the resolved visibility set) | **qualified** by spec §4.4: the set is candidacy, the live count decides; the drill-down interaction is reopened and carried in spec §11 |
| rep §2.4 (`members/<ordinal>.roaring`, entity space on disk) | **confirmed and extended**: `G` joins the entity-space disk plane as ground truth, mmapped only on touch, and takes membership's shape — a derived row operator, unioned forward at flush and rebuilt at the fold (spec §2, §4.1) |
| rep §2.1 / §2.2.1's "merge and flush cadence never touch it" | **qualified** by spec §4.1: true because the row form is bounded at base rows, which is now a stated rule rather than an assumption about who declares members |
| model §2 / §2.1 / rep §2.3, `(layer, level, ordinal)` addressing | **contradicted at the wire only** (spec §5): the address is internal; the wire identity is `tessera_id`. **Ruled** (spec §9), with the control plane permitted the structured address |
| model §5 / rep §9, layer reachability "resolved once at authorise" | **superseded** by spec §6: reachability is keyed on layer version, a gate edit bumps it, and a live `verdict` on the layer entity runs ahead of it. Resolving once at authorise held every open session on the pre-edit gate for its remaining life |
| rep §4, "this predicate and no other" | **extended**, not contradicted: an attached artifact is additionally tested on its target's `verdict` and gate, or a suppressed cluster's labels serve on every route that does not traverse the edge (rep §4, amended) |
| model §2.3's emergency path (suppress, edit, unsuppress) | **confirmed**, and extended to the layer (spec §6). ⊘ The edit step is deferred with the edit pass; the path that exists today is suppress, then republish |
| write-path §5.8, the deletion-label bullet | **implemented, not contradicted** (spec §7); its "labels are Phase 3" marker now points at this design |
| §7.6's availability-under-deletion paragraph | **confirmed and made permanent**: with the fold no longer able to shrink containment's operands, "fails for every principal" holds after the fold too; the notification obligation lands at spec §4.2 |

## 11. What was not settled

- **Drill-down by identifier under the live-count rule** (spec §4.4): the resolved-set lookup
  restores C4's closure and can serve a stale *pass*; a live re-check restores exactness and
  reopens per-identifier work proportional to the artifact's membership. Options: accept the
  bounded staleness with a register row; re-check live and re-open rep §8's escalation; or
  invalidate resolved sets on overlay movement, whose cost under deny churn is unmeasured. Needs
  its own small design; it is the only route where the two goods conflict.
- **The resolved set's invalidation events** — layer version and generation key are settled (rep
  §5.0.1); whether `overlay_version` participates, and at what granularity, is the same question as
  above from the cache side.
- **The three arms' cost** (spec §2, §4.1): flush-union, merge-rebase and fold-rebuild are each
  specified and none is measured. The merge arm is the one whose bound is least obvious, since it is
  proportional to the merged span rather than to the artifact population.
- **The runtime create/edit verb's contract shape** (rep §5.1) — carried, still contracts work.
- **Bulk suppression of a caller-defined *subset* of a layer** (every label whose `G` touches a
  compromised source, say): expressible today as N artifact suppressions; whether a set-valued
  control verb is wanted is unexamined. The fold's report (spec §4.2) supplies the N.

## Appendix R

**2026-08-30 — §2.1's shrink gains its limit case, and the fold no longer produces a set every
principal satisfies** ([decision 0107](../decisions/0107-a-generating-set-with-no-survivors-is-not-served.md),
owner ruling). §2.1 described permissive as serving *a principal satisfying the survivors* and said
nothing about there being none; the code shrank the set and kept the content, and an empty
generating set is contained in every mask, so the corpus-derived text served to everyone who could
see any member. Found by a test-quality audit rather than by review. The publication gate already
refused the same state on the way in, with the same reasoning, which is what made this a gap in the
fold rather than a question about the mode: the fold now withdraws such content, the artifact
follows the rule it already has, and no new outcome or stored bit is introduced. Appendix C's C7
carries the same sentence.

**2026-08-20 — the growth the ruled row promised is built, and §3.4's unbuilt marker moves to the wire.** What
an ingested point needed in order to join an enumerated membership was machinery that did not exist:
a durable record carrying a **delta** rather than a restated set (a restatement costs ~12 MB per
batch naming a 10⁸-member cluster, on the fsync path), one store method that grows a membership, and
the packing bookkeeping that keeps a grown record reachable. The third is the one that decides it:
a level is packed only above its published high-water, so releasing the log at the mark that covers
a packed *tail* would leave the join durable nowhere and the artifact back at its pre-growth size
after a restart — acked, silent, indistinguishable from an artifact below its criterion. The fold's
whole rewrite is what reaches it, so the pin it releases is a separate call with a separate
precondition. `artifacts-from-points.md` §6.1 is the mechanism.

**2026-08-20, later still — the row's last unbuilt half is built.** A key naming no artifact creates one where the
layer's `value_set` is open (`artifacts-from-points.md` §6.3): at the commit window's close, on the
write executor, as a publication carrying the points that named it — and a lineage naming clusters
that do not exist yet mints the chain and links it parent before child in the same batch. That was
[decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md)'s last obligation, so
§3.4's row now states a built rule with nothing marked absent, and the timing it states — the flush,
because a masked count is a row-space question — is unchanged by any of it.

**2026-08-20, later the same day — the wire carries it, and the row's ⊘ narrows to minting.**
`/control/ingest` accepts a column named for a declared layer (`artifacts-from-points.md` §6.2,
contracts r36), so a point names its artifacts beside its coordinates and the join is appended inside
the batch's own commit window rather than arriving through a separate call. What the row still marks
unbuilt is minting: a key naming no artifact is refused, at both entry points bar the build.

Two things this document said that the building qualified. **§4.1's base-row rule is about the
member, not about the membership**: a point that already holds a base row joins and is counted at
the ack, because the row-space projection is keyed on the artifact store's version — so the interval
before the fold is observable and correct, not merely unobservable, and the fold's role is where the
membership is *stored*. And growth is a second way state enters the artifact store, which is why the
one method says at the site how it stands to write-path §5.4's two removal rules: it adds bits and
removes none, and a member that joined has no separate provenance once it is in the set.

**2026-08-20 — the contested membership row is ruled, and the rule is larger than the row**
([decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md)). §3.4 said an
ingested point enters an enumerated membership *never*, citing I8 and *the caller declared the
set*; `artifacts-from-points.md` §5 said it joins. §3.4 was wrong, and the argument that settles it
is that a build reading a member table has always entered points into an enumerated membership — so
the two documents were describing one operation at two entry points and disagreeing about it. I8
governs a generating set, which is a different set and is never grown either way. The growth
mechanism is marked ⊘ at the row.

**r11 — 2026-08-20. §3.4's enumerated-membership row is marked contested, and nothing else moves.**
[`artifacts-from-points.md`](artifacts-from-points.md) §5 rules that a point carrying an artifact's
key joins that artifact's membership at ingest; this document's timing table rules the same
consequence **never**, citing I8. Both are normative for artifact semantics. The marker records the
disagreement where the next reader meets it rather than resolving it — the resolution is a ruling,
and the stage that ran into it stopped there (§6.1 of that document carries what the attempt
found). Worth stating plainly because the row's reason does not quite reach its claim: I8 governs a
**generating set**, and a membership is a different object with a different owner.

**r10 — 2026-08-19. The banner said none of this is built.** A correction, not a design change: no
claim in the body moves. Through Stage 5 the spine of this document is built — layers, artifacts,
generating sets, containment, the dependency edge and the fold's artifact pass with its report — so
a reader who took the banner at its word would have read a built mechanism as a proposal. What is
genuinely unbuilt is now named rather than covered by a blanket ⊘.

**r9 — 2026-08-19. A deletion cascades where a replacement refuses.** `depends_on` said what an
edge constrains and nothing about what it means, leaving a label whose cluster is deleted, and a
label whose cluster the viewer cannot see, both undefined. Both are answered
([decision 0089](../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)): a
dependent is deleted with its dependency and served only where its dependency is served. The
boundary against §5.0.4 is the operation, and it is now stated at the site rather than left to be
inferred — replacement mints identities and so refuses rather than repointing; deletion leaves
nothing to point at and so cascades rather than stranding, taking the deletion lane and retiring at
the fold that executes it. The label sugar's `public`-under-a-gated-parent refusal goes with it: a
gate declared on a label layer can no longer widen anything, so there is nothing left to check.

**r8 — 2026-08-19. The label sugar builds, and its one default is bounded.** `[layer.labels]`
expands to a `[[layer]]` block before anything compiles, and the two spellings are asserted to
produce a byte-identical bundle — so this section's claim that a label is a first-class artifact is
now structural rather than aspirational: there is no label layer below the parser to treat
differently. The sugar needs a member source of its own (`[layer.labels.members]`), a ranked
content's generating set being a `(artifact, rank, entity)` row and nothing else. **Its
`visibility` default is the parent's actual value**, and the *never wider* half is enforced only at
`public`, marked as unbuilt above: access labels are opaque terms and the build has no ordering over two of
them.

**r7 — 2026-08-19. §6.1's build inputs are built.** One source per layer, one row per artifact with
`contents` as a ranked list, inline `artifacts`, membership by exclusion, and a layer's `fields` map
reaching its readers — all of which this section already specified and none of which the build did.
Three consequences worth stating. The **cross-row agreement refusal is retired, not moved**: with
one row per artifact the disagreement it detected cannot be written, and what the grain still admits
— one key on two rows — is refused as two artifacts under one name. The **complement happens once,
at the build**, and no type below it carries the spelling, which is what makes *no request-time
complement* structural; an excluded id the build did not assign refuses it, an exclusion resolving
to nothing being a silent widening. And each pair of spellings is asserted to produce a
**byte-identical bundle**. No rule of the write cycle moved.

**r6 — 2026-08-19. Vocabulary only.** The renames §6.1 announced are performed workspace-wide:
`rank` and `entity` are what the artifact and member readers spell, and an artifact's caller-supplied
name is `key` rather than `stable_key`. Prose that called a ranked content a *variation* names it as
what it is — an entry of the artifact's `contents`. No operation table row changed.

**r5 — 2026-08-18.** §6.1 is rebuilt on
[decision 0088](../decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md)'s two
axes and the per-object sources that come with them
([`../evidence/memos/2026-08-18-configuration-surface.md`](../evidence/memos/2026-08-18-configuration-surface.md)).
No rule of the write cycle moved. What moved is the shape of what a build reads: one config file
rather than three flags, one source per layer rather than a shared file with a discriminator, an
artifact source of one row per artifact carrying `contents` as a ranked list — which retires the
cross-row agreement refusal the `(artifact, variation)` grain needed — and `variation`/`member`
renamed to `rank`/`entity`. Membership by exclusion is added as an input spelling with the
request-time reading explicitly excluded. `[layer.labels]` is added as sugar over a layer that is
still a layer.

**r4 — 2026-08-16.** §6 gains the build plane, which the operations table had allocated to layer
creation's control verb alone: `tessera build` now takes a declaration file and two artifact files
(§6.1). No rule moved — the build runs the registry, the allocator and the publication the control
plane runs, so the routes cannot disagree about what a layer is — and the bulk-publication note in
`annotation-representation.md` §5.0 loses its unbuilt marker with it.

**r3 — 2026-08-16. Promoted to normative.** No mechanism changed. Decisions 0082 and 0083 fall
entirely on the read path — a layer's lineage lives in its edges, and a response is bounded by a
request-time budget — and neither touches an operation table here: publishing an edge was already the
ordering constraint spec §5.0.4 names, and a treed layer's level component is always 0, which the
`(layer, level, ordinal)` address already admits. Recorded because *nothing changed* is a finding
when two rulings land on a sibling document, and the alternative is a reader assuming this one is
stale.

**r2 — 2026-08-15.** The owner rulings (decisions 0074–0081) applied. The operation tables are the
substance: the artifact edit row is marked deferred with the edit pass, the layer table's
"regenerate" became **replace** with nothing carried and a report where the suppression-carry
refusal stood (0081), gate changes restate by direction with the criterion and the own-terms flag
as their subjects (0079, 0075), and the delete rows state that the interim withholds the artifact's
service — not merely its text — for viewers no surviving variation covers (0076). The stale I9
claim in spec §9 is corrected to decision 0072's settled-and-unbuilt state, and C3 is recorded
closed.

**r1 — 2026-08-15.** Drafted; three independent reviews the same day, findings dispositioned in
place (four fail-opens closed — the status header names them).
