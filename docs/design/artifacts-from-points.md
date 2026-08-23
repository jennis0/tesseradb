# Artifacts declared by the points that belong to them — design

**Date:** 2026-08-20
**Status:** Draft — owner-ruled in discussion, **and built in full at both entry points**. The
readers take an integer key, skip a noise one, and read a list column as the artifacts a point
belongs to plus the edges between them; `value_set` decides whether a member key may create an
artifact, and it now decides it at a build *and* at ingest. Points join an artifact that already
exists — a durable delta record, one store method, and a log pin the fold releases (§6.1) — an
ingest batch may carry a column named for a layer (§6.2), and a key that column carries which no
artifact holds **creates the artifact it names** (§6.3), lineage and all. That discharges
[decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md): there is nothing a
member table can say that the wire cannot. The reading is shared with the build rather than
reimplemented: which position carries which level, and which adjacencies are edges, live in
`tessera_types::layer`. The rulings in §5 are the owner's; the rest follows from them. Extends
[`configuration.md`](configuration.md) (normative for the surface) and
[`annotation-write-cycle.md`](annotation-write-cycle.md) §6.1 (normative for artifact semantics).

## 1. What this answers

A clusterer emits a label per point: one cluster id in a column, or a list of them for a
hierarchical run. The corpus that produced it is a point table, and the clusters exist only in the
sense that points reference them. Today that has to be turned into an artifact table plus a member
table before Tessera will read it, and any cluster the producer forgot to enumerate is a refusal
rather than a cluster.

The whole of this design is: **a cluster exists because points say it does**, and everything else
about it — its name, its parent, its content — is optional enrichment from wherever the caller
already keeps it.

Four cases, and the third is the one nothing expresses today:

| Roster | Enrichment | |
|---|---|---|
| a table | that table | the shape that exists |
| the points | none | a bare clustering |
| **the points** | **a table** | **a clusterer plus whatever is known about its clusters** |
| a table | elsewhere | not a case anyone has |

## 2. Membership needs no new surface

`[layer.members]` already means *one row per `(artifact, entity)`* and already reads `key`,
`entity`, and optionally `rank` and `level`, with `fields` naming which columns carry them. A point
table with a cluster column **is** that shape:

```toml
  [layer.members]
  source = "points"
  fields = { key = "cluster_id", entity = "id" }
```

The source is named, not inferred — a membership route that silently read some other block's file
would be the one thing about this that a reader could not see. A hierarchical run is the same
block over an exploded table carrying `level`.

Two readers change, and neither is about membership:

- **A key column may be an integer.** Cluster ids are integers, and requiring UTF-8 makes the
  common case both awkward and slow: converting per point costs ~54 s at 10⁹, against ~15 s for an
  integer hash and ~0.5 s where the value is already an interned code (measured, 10⁵ distinct ids,
  single-threaded). The roster is converted once; the per-point path never formats anything.
- **A null key, and `-1`, mean *this point is in no artifact*** — skip the row. Noise is a quarter
  of the points at each split of a condensed tree on the corpus measured here, and `-1` is the
  sentinel every clusterer emits. Exactly `-1`, not any negative: a negative id is otherwise
  unusual enough that swallowing `-7` would more likely be eating data than handling noise. For a
  string column, null only.

## 3. `value_set` decides whether a key may create an artifact

```toml
[[layer]]
artifacts  = "clusters"        # enrichment: names, parents, contents — optional
value_set  = "open"            # a key no artifact declares creates one
membership = "enumerated"
```

`value_set` is the word [`configuration.md`](configuration.md) already uses for exactly this
question on a vocabulary — *is an unknown key at ingest refused, or minted?* — applied to a layer's
artifacts. `closed` is the default and is today's behaviour: the roster is `artifacts`, and a key
not on it is refused because a mistyped id would otherwise publish a phantom artifact carrying the
members it stole from a real one, whose masked count then goes quietly short.

**It sits on `[[layer]]`, not on `[layer.members]`, because ingest has no member block.** A key in
a build-only acquisition block could not govern what the write path does with an unknown id, and
governing both is the point.

With `open`, `artifacts` becomes enrichment rather than a roster. A cluster in the table that no
point references is an artifact with no members — subject to the layer's existence criterion, so
generally not served, and generally a sign the table is stale. A cluster the points reference that
the table omits exists with no title and whatever `content = { computed = … }` gives it. Neither is
an error.

**What `open` costs is that a typo stops being a refusal**, and the mitigation is that the number is
reported rather than bounded. A build prints what each member source created and carries the total
in its report; an ingest batch is told in its own 200 how many artifacts its keys created. Both are
*counts of what happened*, not thresholds: a bare clustering legitimately mints every artifact it
has, so there is no number that separates that from a wrong column — only the operator's knowledge
of which they meant. Whether a declared per-layer bound is worth a key of its own is open (§8).

**Two declarations make a layer unmintable, and both refuse the key rather than minting an artifact
that could not be served**: a layer declaring supplied content kinds, because an artifact served
without content its layer declares cannot be told apart from one whose content was withheld; and a
layer declaring `depends_on`, because a dependent with no attachment would be gated on nothing.
These are the refusals a publication has always made of an artifact carrying only a key — made
before the batch is admitted, so they cost one batch rather than the commit window it would have
joined.

## 4. A lineage list declares the edges

Where the column is a list, its shape is checked against the hierarchy kind the layer already
declares — the kind is declared as it is today, and the edges are read from the data the caller
supplied. **Every entry names an artifact the point is a member of**, exactly as a scalar key names
the one; what the kind supplies is what the *positions* mean:

| `hierarchy.kind` | Column | Edges |
|---|---|---|
| `flat` | scalar, or a list read as plain multi-membership | none |
| `stacked` | fixed-length list, one entry per level | none — independent analyses |
| `tiered` | fixed-length list, one entry per level | containment, between consecutive entries |
| `nested` | variable-length list | the lineage: entry *k* is the parent of entry *k+1* |

Fixed-length entries are nullable: a point may be noise at a fine resolution and clustered at a
coarse one. A variable-length list against `tiered`, or a fixed-length one against `nested`, is a
refusal rather than a guess.

**A child naming two different parents is refused.** The map is built during the pass that already
walks the column, and a conflict means the data is not the tree the layer declared — there is no
correct output, and choosing a parent would publish a hierarchy the caller did not write. A `parent`
column on an artifact row and a lineage column disagreeing about one artifact is the same conflict
and refuses with the same words: they are two spellings of one edge.

Three things the table does not settle, decided in the building:

- **A fixed length is the declared level count, however the file spells it.** An Arrow
  `FixedSizeList` states its arity in its type and is checked once against `[[layer.levels]]`; a
  plain list states it a row at a time, and a row of another length is the refusal. Reading the type
  alone would refuse every producer whose Arrow binding writes a plain list, which is most of them —
  the declaration and the data agree or they do not, and the type is only one of the two places the
  data can say so.
- **An entry naming nothing links nothing across itself.** The edge is between *consecutive*
  entries, and an entry that is null or `-1` is not one — so a point clustered at level 0 and level
  2 and noise at level 1 declares no edge at all, and an artifact only ever named that way is
  published as a root. Reading past the gap would state a containment no row makes, which the next
  point clustered at that resolution would contradict.
- **A row whose every entry is noise is one row in no artifact**, counted in §2's number rather than
  a second one: rows are the denominator that separates a noisy clustering from a wrong column.

A `level` column beside a list key column is **ignored, and said so**. The positions are what carry
the levels, so reading it too would place a point at a level its list did not name; refusing would
block a build over an input that discloses nothing and costs a rerun.

## 5. Identity, and what a suppression survives

Four rulings, owner-made:

**A key is a name; identity is what minting allocates.** Delete an artifact and reload a point
carrying its key and you get a **new artifact** — decision
[0047](../decisions/0047-edit-is-delete-plus-reingest.md) and
[0081](../decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md) applied, not a new
rule. Uniqueness is *at most one live artifact per key per level*; tombstoned ones do not count. The
build's existing refusal of one key on two rows is within a build and stands.

**A suppressed artifact still exists, and ingesting a point carrying its key changes nothing.** The
point joins the membership; the artifact stays suppressed. That a point may join at all is
[decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md): a build is ingest
into an empty database, so an operation a member table performs at one entry point is available at
the other.

**Minting therefore tests against live artifacts, not servable ones.** Written the natural way —
*is this key unknown?* — against what is currently served, a suppressed artifact reads as absent, a
second artifact is minted under its key, and the new one is not suppressed. A suppression would be
defeated by ingesting a point. This is the only fail-open in the design and it is closed by
construction rather than by a check — three times over, as §6.3 sets out, the innermost being
publication's own refusal of a key its level already holds.

**Deleting an artifact deletes its suppressions**, and the removal rides the deletion's own entry —
hidden at the same ack, retired at the same compaction fold. Not a separate sweep: a sweep is a
second retirement route for suppressions, and if it ever ran ahead of the deletion becoming visible
the artifact would serve unsuppressed in the gap. One removal rule, as write-path §5.4 requires.

## 6. Build and ingest are one rule at two entry points

Everything above holds identically whichever way a point arrives. What that requires of the write
path is one thing that did not exist: **a membership that can grow**, which is what a member table
at a build has always done and what no route into the artifact store could do — every one of them
wrote a whole record, and the two mutating passes only removed. That is built (§6.1).

**The wire says what a file says** (§6.2): a column named for a layer carries a point's artifacts,
read by the same rules and joined in the same commit as the rows. **And a key it carries that no
artifact holds creates one** (§6.3): a point carrying a lineage that names clusters which do not
exist yet mints the chain and links it in one batch, parent before child — the ordering constraint
edges already carry (`annotation-representation.md` §5.0.4), applied to a batch rather than to a
build. Where the artifact *does* exist, a lineage only checks: a growth adds members and never
lineage, so a contradiction refuses and an edge the layer holds no parent for is reported and the
memberships still land.

Computed content needs nothing extra: centroid, box and hull are recomputed per viewer from current
membership, so they follow new points without invalidation.

### 6.1 How a membership grows — built 2026-08-20

Three pieces, and the third is the one that decides whether it is correct rather than merely
working.

**A durable record carrying a delta.** `ArtifactGrow` names a layer, a level and, per artifact, an
ordinal and the entities joining it as a portable Roaring bitmap. Restating the whole record at its
own ordinal would have replayed correctly and needed no new shape — it is what a publication already
does — but it costs `O(|membership|)` bytes on the fsync path for every batch naming the artifact,
~12 MB for a 10⁸-member cluster, which is not a write path. The ordinal is **resolved on the
executor and recorded**, exactly as a publication records the ordinals it claimed: replay applies
what was decided rather than re-resolving a key whose index has since moved.

**One store method that grows a membership**, shared by the live path and replay because both go
through the record. It is a second way state enters a structure whose *removal* rules have been
conflated twice, so it says at the site how it stands to them: it adds bits and never removes one,
and a member that joined has no separate provenance once it is in the set — the fold's rewrite and
its retirement cannot tell it from a declared member, which is what stops growth becoming a third
removal rule by the back door. An ordinal naming no record adds nothing, so a growth cannot
resurrect an artifact a fold retired.

**The packing rule, which is where it bites.** A level is packed only above its published
high-water, so a grown record below that mark never reaches a manifest by the append-only route; the
log holds the only copy of the join until the fold rewrites every level whole. So the log is
**pinned at the growth**, and the mark that releases a packed tail does *not* release it —
`mark_published` and `mark_growth_packed` are separate calls with separate preconditions, the second
made only by the fold and only after its flip. Releasing at the first is the silent failure this
whole piece exists against: the record is reclaimed, the extent still holds the pre-growth
membership, and the artifact comes back from a restart **without** the point — acked, quiet, and
indistinguishable from an artifact that failed its existence criterion. `artifact_growth.rs` asserts
it at the one place it bites: a rotation may not reclaim the member holding a growth, and the same
membership comes back whole after the fold with the log deleted outright.

**The fold is the home, and the interval needed durability rather than a second packing rule.** A
membership is projected through **base** rows (`annotation-write-cycle.md` §4.1), so a point
ingested since the last fold contributes to no masked count however its membership was recorded —
but that is a statement about *newly ingested* points, and it is not what makes the fold sufficient
here. A point that already holds a base row joins and is counted at the ack, projection and all;
what waits for the fold is only where the join is *stored*. Growth is therefore observable
immediately in exactly the case the fold's timing argument does not cover, and correct in both.

**Two refusals and one no-op.** A key the level does not hold refuses the whole batch rather than
growing the artifacts beside it; a member that is deleted refuses it too, on the rule a publication
naming one already follows (§3.1) — a suppressed member joins, being live and merely outside every
mask. A batch with nothing joining appends no record and acks: nothing joining is a thing a caller
can honestly say, and pinning the log at a growth that changed nothing would be the cost of refusing
it twice over.

### 6.2 A column named for a layer — built 2026-08-20

**The acceptance rule is reserved, or a declared attribute's name, or a declared layer's name.** The
layer's own `name`, exactly as an attribute column is named for the attribute's `name` and not its
`field`: `fields` on `[layer.members]` maps a *file's* column onto the canonical meaning and is
build-only for the reason `source` is — it says where rows come from rather than what they mean
([`configuration.md`](configuration.md) §2), and a name a running node had to be told about could be
checked against nothing.

**The value is the member table's, and the rule is shared rather than reimplemented.** Text or an
integer; `null` and `-1` meaning the point is in no artifact of that layer; a list whose positions
mean what the declared `hierarchy.kind` says (§4). The Arrow decode is *not* shared and should not
be: `tessera-types` and `tessera-lifecycle` deliberately carry no `arrow` dependency, and forcing
one to make a reader common would be a layering change bought for a decode either side writes in
twenty lines. What is shared is the part that would drift — which position carries which level,
which adjacencies are parent edges, and what an integer key spells — as `ListMeaning`, `parent_edges`
and `integer_key` in `tessera_types::layer`, read by the build's member pass and by the ingest
handler alike.

**A column names a layer, and the level is the list's own.** A scalar names the artifact at level 0,
which is what a member table with no `level` column means; a levelled list's entry *k* names level
*k*. There is no level column on the wire and there is not going to be one: the positions carry it.

**Where it lands.** The handler reads the column into `(layer, level, key) → row positions` and the
edges the adjacency declared, and hands both to the write executor with the batch. Keys resolve at
**admission**, through `ArtifactStore::ordinal_of_key` — the same lookup the growth command makes, so
§5's third ruling holds here for the same reason — and on a **closed** layer a key naming no artifact
refuses *that batch* alone, before the window it would have joined is closed; on an open one it
travels unresolved to the close, where it mints (§6.3). The entities do not exist until the window
allocates, so the ordinals are carried from admission to the close and the joins become one
`ArtifactGrow` per `(layer, level)` appended inside the window's own fsync: the membership is durable
in the same commit as the point, and there is no state in which a point is ingested and its
membership is not.

**An edge into an artifact that exists is checked, never created.** A growth adds members; lineage
is settled where the artifact is published, which for a key that mints is §6.3's publication and for
a key that already exists is somebody else's, earlier. A lineage contradicting the parent the layer
holds is refused in the words the build uses for two points disagreeing — two spellings of one edge,
and no correct output. An edge whose child holds *no* parent is reported and the memberships still
land: the membership half of the same entry is unambiguous, and refusing it would block a batch over
a roster published without its edges, which discloses nothing and costs a republication. **Minting
narrows that case but does not close it**: a chain whose artifacts are all new arrives parent before
child and is linked, but a child that already exists and holds no parent is still an edge this route
cannot create, whoever its parent is.

**A column naming a layer whose membership is *evaluated* is refused**, naming it. There is no
stored membership for a point to join, and a stored answer beside a live predicate is what
`prepare_publish` already refuses a publication for; this is that refusal one step earlier, where the
batch can still be rejected without effect.

### 6.3 A key that names nothing creates it — built 2026-08-20

**Minting is a publication, and it happens at the window close.** An ordinal is claimed from the
level's own cursor and is durable only in the record that claims it, so a claim made at *admission*
would sit unappended across everything the executor does before the window closes — including a
`PublishArtifacts` command, which reads the same cursor and would take the same ordinal. At the close
there is nothing to interleave with: the window is closed, the allocation is made, and the record
rides the window's own fsync. That is §5's first ruling — ordinals are claimed serially on the write
executor, as a publication claims them — read as a placement rather than a slogan.

**What is decided at admission is everything that can still refuse one batch on its own**: whether
the layer exists, is enumerated and has that level; whether its declaration admits an artifact
carrying nothing but a name (§3's two unmintable shapes); and whether the batch's own column named
one child under two parents. A closed layer's unknown key is refused there as before. An open
layer's travels as a key with no ordinal.

**A minted artifact is published carrying its members**, not published empty and then grown: the
entities exist by the close, so one record says the whole of what happened and the join needs no
second record and no log pin of its own. A key that resolves *at* the close — a publication having
landed since the batch was admitted — becomes an ordinary growth instead.

**Lineage is what the growth record could not carry, and it did not have to.** A growth is a delta of
members; it has no field for an edge and gains none. What creates the edge is the publication that
creates the artifact, which has carried `parent_key` since artifacts existed — so a minted chain
needs no new record shape, only the right order:

- a **nested** lineage is one level and one record. `prepare_publish` resolves a parent that is a
  sibling of its own batch, so parent and child land together whatever order they sit in.
- a **tiered** chain is one record per level, coarse first, and the parent's ordinal was fixed by the
  record before. Nothing has been applied at that point, so the resolution takes a `pending` answer
  beside the store's — one level up and no further, because entry *k* of a list is the parent of
  entry *k+1* and searching past a gap would invent an edge the reader deliberately does not read.

**At most one live artifact per key per level, by construction three times over** (§5's second and
third rulings). The keys are gathered into one map before anything is prepared, so two points in one
batch — or two batches in one window — mint once. The map is then resolved against
`ArtifactStore::ordinal_of_key` a second time, so a key that acquired an artifact since admission
grows instead. And `prepare_publish` refuses a key its level already holds, which it has always done:
so even if both resolutions were written against the served view, the second artifact would be
*refused* rather than created. **That is what closes the design's one fail-open** — a suppression
defeated by ingesting a point — and the innermost of the three guards is the append-only rule rather
than anything added here.

`ArtifactStore::ordinal_of_key` is suppression-blind because there is nothing in it that could see a
suppression: the index loses a key at exactly one event, the fold retiring the artifact's own entity,
which is a *deletion* (Rule S touches no stored structure). It needs only not to be replaced by a
`verdict` call.

**What a batch minted is reported to the batch that minted it.** `/control/ingest`'s 200 carries
`minted`, and the executor logs the count with a sample of the keys. A replayed batch reports zero,
which is the honest reading: the count is what *this submission* created.

## 7. Joins are reported, not refused

Any file carrying entity ids joins to entity space by id — geometry, members, attributes alike — and
a row naming an entity this build did not load is **ignored**. That is the ordinary case: an
attribute file covering two million documents against a fifty-thousand-document build is a superset,
not an error.

Ignoring silently is the failure this system has already shipped once, in the extent that folded a
corpus into a corner while the build said nothing. So the numbers print, against the denominator
that means something — **entities covered**, not rows dropped, because a legitimate superset and a
broken join drop the same overwhelming fraction:

```
attribute 'sentiment': 49,812 of 50,000 entities have a value
        1,950,188 source rows named entities this build did not load
```

Zero of fifty thousand is a wrong column or a wrong file, and warns loudly. It does not refuse: the
operator is present, the loop is fast, and a bundle with one unpopulated attribute is a rerun rather
than a mission.

## 8. Open

- Whether a minted artifact that loses its last member should withdraw by default. The object *was*
  its membership, unlike a curated set, so the layer default may want to differ — undecided.
- How the roster appears in `reports/disclosure.json`. A diff reading *417 clusters, unchanged*
  while every identity churned would mislead in exactly the case that matters.
- The notification owed when a pipeline rerun deletes a cluster someone had suppressed. The
  suppression is gone by design (§5) and the operator who applied it is not the one who ran the
  pipeline. This rides write-path §5.8's existing obligation for deletions that dark-ship, rather
  than needing machinery of its own; ⊘ that report is unbuilt.
- A per-layer bound on minted artifacts, declared rather than enforced, so a corrupt column warns
  loudly instead of minting millions. Undecided whether it is worth the key. What exists instead is
  the count, at both entry points (§3): the build's report and the batch's own 200 say how many
  artifacts were created, and nothing refuses on the number.
- Whether a growth naming a key nothing holds should mint. It does not: `/control/layers/{layer}`'s
  growth route refuses an unknown key whatever the value set says, because it names an artifact to
  add members to rather than a point declaring the artifact it belongs to. The asymmetry is
  deliberate and small, and it is not [0091](../decisions/0091-build-is-ingest-into-an-empty-database.md)'s
  concern — that route has no build counterpart to differ from.

## 9. Not in scope

**`membership = { attribute = … }` stays declared and unbuilt**, and is a different feature: it
makes membership a *predicate evaluated per request*, where this design materialises at build and
maintains at ingest. The two look alike from the declaration and differ in where the work happens —
a predicate over a `derived` vocabulary is answered by a masked scan, which a viewport showing 263
clusters would pay 263 times.

## Appendix R — review trail

**2026-08-20 — r7. A key that names nothing creates it, at both entry points, and the fail-open is
closed three times.** §6.3 is built:
[decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md)'s last obligation, and
with it the last difference between what can be *said* at a build and at ingest. An unknown key on an
open layer no longer refuses a batch; it creates the artifact it names, carrying nothing but the
name and the points that named it.

**Where minting happens was the first decision, and admission was wrong for it.** An ordinal is
claimed from the level's cursor and is durable only in the record that claims it, so a claim made at
admission would be held unappended across everything the executor does before the window closes — and
a `PublishArtifacts` command executes in exactly that interval, reads the same cursor, and would take
the same ordinal. So the mint is at the **close**, where the window is shut, the allocation is made,
and the record rides the window's own fsync. What stays at admission is every refusal that can be
made about one batch alone, which is what keeps one caller's typo off another caller's rows.

**The growth record carried no lineage and did not need to.** A growth is a delta of members and has
no field for an edge; what creates an edge is the publication that creates the artifact, which has
carried `parent_key` since artifacts existed. So a minted chain needed no new record shape — only the
order: one record for a nested lineage, where a sibling's ordinal is resolved inside its own batch,
and one record per level coarse-first for a tiered chain, where the parent's ordinal was fixed by the
record before and is answered by a `pending` resolver one level up. That is §5.0.4's constraint
applied to a batch, and it is the whole of what minting a lineage required.

**A minted artifact is published carrying its members** rather than published empty and grown: the
entities exist by the close, so one record says the whole of what happened and no log pin is owed.

**§5's third ruling has three guards and the innermost is not one anybody added.** The resolution at
admission and the re-resolution at the close both read `ArtifactStore::ordinal_of_key`, which no
suppression touches; and `prepare_publish` refuses a key its level already holds. Breaking both
resolutions together turns the suppression test red on that third refusal — a 422 rather than a
second artifact — which is the fail-closed direction and is how the guards are known to be
load-bearing.

**One thing the design had wrong, and it cuts against the earlier claim.** §6.2 said the
edge-with-no-parent warning "stops being reachable when minting lands". It does not: a chain whose
artifacts are all new is linked, but a child that *already exists* and holds no parent is still an
edge a growth cannot create, whoever the parent is. §6.2 now says so.

**2026-08-20 — r6. The wire says what a file says, and the shared piece is the rule rather than the
reader.** §6.2 is built: `/control/ingest` accepts a column named for a declared layer, reads it by
the same rules a member table is read by, and the joins ride the batch's own commit window — one
`ArtifactGrow` per `(layer, level)`, inside the fsync that makes the rows durable. That closes the ⊘
[decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md) marked as the one
genuine breach of its own rule.

**Two things were decided in the building.** *Where the keys resolve*: at **admission**, not at the
close, because a commit window holds several callers' batches and one caller's typo may not refuse
another's rows — after the allocation there is no per-entry refusal left to make. The ordinal is then
carried rather than re-derived, which is safe for a reason worth writing down: between admission and
the close nothing can move an existing ordinal (a publication appends, and both operations that
remove an artifact close the open window before they run), and an ordinal whose record has gone by
then adds nothing, which is what stops a growth resurrecting an artifact a fold retired. *What a
lineage may do at ingest*: **check, never create**. A growth adds members and never edges, so a
contradiction is the build's own two-parents refusal and an edge the layer holds no parent for is a
warning with the memberships still applied — a refusal there would block a batch over a roster
published without its edges, which is outside the disclosure surface entirely.

**The sharing is deliberately partial, and the split is where the drift would be.** `tessera-types`
carries no `arrow` dependency and does not acquire one: the Arrow decode stays with each reader,
and `ListMeaning`, `parent_edges` and `integer_key` move into `tessera_types::layer` so that the
*meaning* — which position is which level, which adjacencies are edges, what `-1` says — has one
definition. The build's own reader now takes its list meaning, its noise sentinel and its adjacency
from there, so the two cannot read one file two ways.

**The test 0091 names is `tessera-server`'s `membership_column.rs`, and it is not the test that was
first planned.** *Ingest into an empty database* cannot be reached through this system's own tools:
`tessera build` refuses a bundle with no items, so the honest form is one corpus arriving two ways —
every point built on one side, a seed built and the rest ingested on the other — compared at what a
client sees, for three principals, at a scalar key and at a lineage. The built side is additionally
pinned to an oracle computed from the fixture, because two sides that both dropped the membership
column would otherwise agree perfectly. Removing the column from the handler's submission turns both
comparisons red, which is how it is known to be load-bearing.

**2026-08-20 — r5. The growth mechanism is built, and the packing rule is the part that decides it
is correct rather than working.**
[Decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md) ruled r4's
contradiction — an ingested point *does* enter an enumerated membership, because a build
reading a member table has always done exactly that — and what r4 investigated was then built as it
described: a delta record (`ArtifactGrow`), one store method taken by both the live path and replay,
and a log pin that only the fold's whole rewrite releases. §6.1 is now what exists rather than what
is needed, and the three refusals it names are the whole of what growth declines to do.

**The failure r4 named as the one to get right is the one the tests are built around.** A grown
record sits below its level's published high-water and no tail pack reaches it, so releasing the log
at `mark_published` would leave the join durable nowhere: the artifact comes back from a restart at
its pre-growth size, acked and silent. It is asserted where it bites — a rotation may not reclaim
the member holding a growth (removing the pin fails that case and nothing else, which is why the
case is written against rotation rather than against a restart alone) — and the fold's rewrite is
asserted by deleting the log outright and reading the membership back from the prefix.

**One thing §6.1 had slightly wrong, and it is worth stating because it cuts the other way.** The
argument that a growth has no observable effect before the fold is about points *ingested* since the
last structural pass, which hold no base row; a point that already holds one joins and is counted at
the **ack**, because the row-space projection is keyed on the store's version and growth moves it.
So the fold is the right home for where a membership is *stored*, and the interval is not
unobservable — it is observable and correct. That is a stronger position than the one the
investigation described, not a weaker one.

**2026-08-20 — r4. §6 was attempted and stopped, and §6.1 is what the attempt found.** The stage
was to be the first half of the ingest route — a column named for a layer, resolved against
artifacts that already exist, joined — with minting left for later. It stopped before the wire
column, on the third of its three parts: **there is no way to grow an existing artifact's
membership, and adding one is a second publication path.** Every route into the store writes a
whole record; publication refuses a key the level holds; the fold's two passes only remove. The
three pieces a growth needs are named in §6.1, and the one that decides the shape is the packing
bookkeeping rather than the record: a grown record below `published_through` is never repacked, the
rotation pin releases at the next flush, and the acked join is then lost on restart with nothing
saying so.

**Found in the same pass, and the more consequential of the two:
`annotation-write-cycle.md` §3.4 rules that an ingested point enters an enumerated membership
*never*.** §5 here rules that it joins. That contradiction predates this stage — §5's rulings were
taken in discussion against a table nobody re-read — and it is not resolvable from inside either
document, so §6 is blocked on a ruling and not only on machinery.

**Nothing was built, deliberately.** The wire column and the key resolution are real work and were
not landed: a column accepted at `/control/ingest` whose effect is unbuilt either drops the caller's
membership silently — the failure §7 exists to prevent, arriving on the request path — or answers
422 for every batch that carries one, which is what an undeclared column already does. Building the
reader ahead of the ruling would also be building on the contradiction. The one thing the attempt
*did* establish and is worth keeping is the timing observation now in §6.1: the row form covers base
rows, so an ingest-time join has no observable effect until the fold whatever route carries it,
which is what makes the fold the candidate home rather than a place the growth eventually reaches.

**2026-08-19 — r3. §4 is built, and the byte-identical assertion is what says it reads structure
rather than inventing it.** A `nested` lineage column and the same clustering written out as an
artifact table with a `parent` column and a member table build the same bundle down to the byte,
and so do a `tiered` fixed-length column and a member table carrying a `level`. Every entry is a
membership and the kind supplies the positions' meaning; minting composes with §3, so an interior
parent that only ever appears inside somebody's lineage is minted like any other key. Three points
the section did not settle were decided in the building and are now written into it: what counts as
a fixed length (the declared level count, from the Arrow type where the file gives one and from the
row otherwise), that a null entry breaks the adjacency rather than being read past, and that a row
of nothing but noise is one unclustered row. One refusal was added beyond the ones §4 states — a
lineage column contradicting a `parent` column, which is §4's own conflict arriving by two routes —
and one thing that could have been a second is a warning instead: a `level` column beside a list is
ignored and printed, on the rule that only a disclosure or an irreversibility earns a refusal.

**A list under `flat` was briefly refused and is not.** The reasoning was that a flat layer has no
positions for a list to index, which is true and does not follow: the entries are still
memberships, and a point in several artifacts of one flat layer is what several rows of a member
table have always meant. Refusing the list spelling would have made two spellings of one
membership disagree — the property every other input route here is held to — and would have
foreclosed overlapping groupings, a document under three topics being ordinary rather than a
mistake. `a_list_on_a_flat_layer_is_multi_membership_and_matches_a_member_table` pins it byte for
byte.

**2026-08-19 — r2. §2 and §3 are built at build time, and §2's first claim was already true.** The
membership route needed no code: a `[layer.members]` block pointed at the points file, with
`fields = { key = "cluster_id", entity = "id" }`, already built the same bundle as the same layer
declared with a member table — asserted byte for byte, which is how the other input spellings are
held. What the stage added is the two readers (an integer key canonicalised to its decimal string,
converted once per artifact and never per point; a null key and exactly `-1` skipped and counted)
and `value_set` on `[[layer]]`, carried on the layer declaration so the write path has the same key
to read when §6 is built. One refusal was lifted rather than added: a member source with no
artifacts source is legal under `open`, which is the bare clustering of §1's table. The count of
skipped rows is printed and reaches the build report; the join-coverage report of §7 remains
unbuilt.

**2026-08-19 — r1.** Written from a discussion that reversed several of its own conclusions, and
the reversals are worth recording because each was a wrong instinct with a recognisable shape.
*Attribute membership* was reached for first, because `membership = { attribute = … }` fits the
data; it was the wrong mechanism, being a live predicate whose per-request cost nobody asked for
(§9). *A vocabulary as the roster* followed, and fell to the observation that if the column is not
an attribute it does not want a vocabulary either — which removed the multi-valued-attribute
dependency that had made §4 look blocked. *A `column` key* reading implicitly from the corpus was
proposed and rejected: the source is named. And the roster was twice conflated with enrichment,
first in one `artifacts` key and then in a reserved word, before §3 separated them.

The residue is small: one capability (`value_set` on a layer), one reading of a list column, two
reader fixes, and four rulings. Everything else the requirement needed was already in
`[layer.members]`.
