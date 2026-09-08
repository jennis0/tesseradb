# Ingest at any scale — design

**Date:** 2026-09-07 · **Revised:** r3, 2026-09-07, after the three-lens review, the owner's
rulings and the re-review of r2
**Status:** **Normative — 2026-09-07.** Drafted under [decision 0134](../decisions/0134-anything-a-build-can-create-live-ingest-can-create-at-any-scale.md) and [decision 0135](../decisions/0135-a-generating-set-is-the-callers-claim-i8-withdrawn.md); reviewed under three lenses and re-reviewed once on the sections whose shape changed; the owner's rulings are [decision 0136](../decisions/0136-the-ingest-design-rulings.md). ⊘ Nothing in it is built except where a paragraph says so; §8 is the order of work.
**Reads against:** architecture §4, §11, Appendix C; write-path §1 to §4, §7; contracts §3.4;
annotation-write-cycle §1 to §6; artifacts-from-points §6; dag-hierarchies §4; views §3, §5, §7;
per-point-attributes §2.2, §5; records-and-search §3, §7; configuration §1, §2, §9; decisions
0047, 0048, 0077, 0081, 0091, 0127, 0128, 0129, 0133, 0134, 0135.
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **spec §n**.

**Owns:** the model of live ingest for every kind of data: what a client sends, what the server
commits, when a viewer sees it, how a payload larger than one request travels, what a client must
handle, and which caps exist and what each one is. It is the design decision 0134 asks for.

**Does not own:** the invariants and the leak register (architecture §4, Appendix C); the byte-level
wire (contracts §3.4, amended by spec §7 when this is promoted); the commit window, the WAL and the
flush (write-path, which this document reads and does not restate); artifact semantics
(annotation-write-cycle, whose sentences resting on the withdrawn I8 spec §7.4 lists for rewrite).

**Two preferences govern every choice below** (owner, 2026-09-07): simplicity and understandability
over cleverness; and an ingest-to-publish latency of minutes is acceptable for large data.

---

## 0. The rule, and the question it leaves

Decision 0134 makes size a pagination question and never a reach question: a cap on a request is
lawful only as a unit a client can discover and the server assembles, and a kind with no route at
ingest is a defect. Decision 0135 removes the one rule that made a part of an artifact unsayable in
pages. What the two leave open is the shape: how many wire forms, whether any kind needs a
multi-request upload committed once, and what the path of least surprise is for a client library.
The owner's framing, verbatim: *"How to ensure all objects can be ingested. How to do so
performantly. How to maximise the ease of use and utility by client and by API (so should we
discourage complex multi-part uploads? What do comparable systems do? What is the path of least
surprise?)"*

The answer, in one paragraph. Every kind is ingested as a batch of records, in JSON by default.
Applying a record is monotone: a subject that does not exist is created, an absent part is filled,
a set-valued part is joined or left, and a part already present must be identical. Every route
publishes two pagination units, a record count and a byte cap, and any size of data arrives over as
many requests as it takes. No kind needs a session-like upload. The acknowledgement means durable;
everything becomes visible at the next publication, the flush tick, and never before. Where a
served form is derived from a store that pages are changing, the executor publishes the form at
the tick from the deltas since the last one, so a request never builds a form and what it is served
may lag the store by up to one tick: stale, never unsafe.

## 1. One model of ingest for every kind

### 1.1 The unit of ingest

A **record** names a subject by the key the caller chose and carries parts of it. The subject is a
point (by external id, or by `tessera_id` with its idset), an artifact (by layer, level and key,
and on a group-scoped layer by its view as well), a layer, a view, a view group, a vocabulary or
one of its values, or an attribute column (each by name). A **batch** is one request carrying
records of one kind. The batch is the commit unit: it is applied on the executor in one durable
step, whole or without effect, exactly as `/control/ingest` and
`/control/layers/{name}/artifacts` already are (write-path §2.3; contracts §3.4). Nothing in a
batch is acknowledged before it is durable, and nothing is durable that was not acknowledged, so a
client's retry rule is the same for every kind: an unanswered request is resent with identical
bytes.

**Applying a record is monotone.** Four cases, and they are the whole of what a write may do on
this plane:

| The part is | The record | Result |
|---|---|---|
| absent | supplies it | filled |
| present and identical | supplies it | accepted, no effect |
| present and different | supplies it | `409 conflict` naming the part; the batch has no effect |
| a set (membership, generating set, vocabulary values) | names entries joining or leaving | the set changes by that delta; within one page joins are applied before leaves; an entry already in the state the record asks for is a no-op |

Removal of a subject is not on this plane. A point, an artifact or a layer leaves through
`/control/changes` and retires under write-path §5.4's two rules, as today. The one set that may
shrink is a generating set, because decision 0135 makes its contents the caller's claim and, since
its amendment, makes this plane the only place that claim can change: the service no longer edits
a set on the caller's behalf at the fold. A membership never shrinks (spec §10, R7), and a
vocabulary value is retired to `reserved`, never removed (per-point-attributes §2.2).

**A set carries its cardinality as a stored property**, moved by the same page that joins or
leaves it and durable in the same record. Containment (`and_cardinality(G, M_auth) == |G|`, I3)
reads **only the pair the executor published together at the tick**: the row-space operator and
the cardinality it was derived with. The store's current cardinality is not readable by the
serving path before the tick, so a test never mixes a cardinality from one version of the set with
an operator derived from another. An operator is derived from entity truth, member by member
(annotation-write-cycle §2): a delta of joins only is unioned into the last operator, which is the
same result; a delta containing any leave re-derives the `(artifact, view)` operator whole, since a
union cannot express a leave and a cardinality moved down against an operator that still holds the
leaver would pass containment for a principal who sees the leaver and not the rest; and the fold
re-derives every operator. Spec §4.1 prices the three arms.

**A content whose declared set is empty is not served**: an empty set is contained in every mask,
so it would serve to every principal reaching the artifact. This is decision 0107's rule re-made
for the caller's door. A page that leaves a set empty **withdraws the content**: the content record
is removed, and the page's acknowledgement reports it, naming the artifact's key and the rank. The
content does not return when the set refills; the caller re-supplies it. 0107's distinction
stands: an empty declared set under a content is a state that serves nothing, and a content with
no set does not exist.

**Built 2026-09-08 (T2b).** A `PATCH` row naming `rank` moves that content's generating set:
`members` join it, `leaving` leaves it, and the store applies joins before leaves within the page
(`ArtifactStore::grow_set`, the one method the live page and replay share). `leaving` with no rank
is refused — a membership never shrinks. The cardinality the delta produces is computed where the
page is prepared and travels in the growth record, and a replay whose recomputed set disagrees with
it refuses the delta rather than publishing a pair that was never derived together. A page that
empties a set removes the content record, moves the ranks above it down as the fold's withdrawal
does, and the acknowledgement names the rank beside the caller's key. The withdrawn content does
not come back: a later page at that rank names whichever content moved down into it, and one past
the last names no content and is refused, so the caller supplies the content again rather than
refilling its set. A batch naming one key more than once with a rank is refused for the same
reason — a withdrawal in the first row would move the rank the second names. The
containment test reads the operator and the cardinality the executor published together at the
tick, both taken from one read of the store.

**Creating an entity is not monotone; only filling is.** A point page allocates an entity for
every row it carries, so a page re-sent under a fresh batch id creates a second entity for every
row that carries no external id, and is refused `409` per duplicate id for every row that does.
Within the WAL retention window a re-sent page under its own batch id is answered as a replay
(write-path §2.4); past it the caller's own bookkeeping decides what has been sent, and a resumed
load carries external ids so that a page sent twice is refused rather than doubled.

This is the rule the build already follows without saying so. A build reads a corpus once, so
every part is supplied exactly once and nothing is ever present and different. Ingest sees the same
corpus in an order the caller chooses, and the monotone rule is what makes the order irrelevant to
the result: two clients supplying the same parts in any interleaving reach the same database, which
is decision 0091's test restated for pages.

### 1.2 The wire: JSON by default, Arrow by content type

**JSON is the default encoding for every kind**, as newline-delimited objects or an array of
objects, one object per record. A row-shaped record (a point, a values row) is an object whose
names are the declared column names, and every value is **coerced against the declared column
type**: a number to the declared width, a string to a keyword, text or category key, a list to the
declared list form, a label list to `access`. A value that does not coerce is a `422` naming the
row and the column, and the batch has no effect, which is the rule the Arrow decode already
applies per column (write-path §2.1 step 4) applied per cell. An integer is parsed exactly from
its digits, never through a double, so an identifier or a 64-bit value survives the door. A null or
empty `access` list at the JSON door is what it is at the other two (decision 0133): the view's
declared `point_visibility.default` where it declares one, and a `422` naming the count of such
rows and the view where it declares none. An object-shaped record (a
declaration, an artifact, a vocabulary value) is the JSON the routes already take.

**Arrow IPC is accepted on the same routes by content type** (`application/vnd.apache.arrow.stream`
against `application/json`), typed as today, with the same column rules and the same refusals.
Nothing about a route's semantics depends on which encoding carried the batch; the two are
decoded into one record form before the executor sees either.

Why JSON first: it is what every comparable system's client speaks first (spec §3.1), it needs no
serialisation library on the client, a page of it is readable in a log, and a client library author
gets one encoding for every kind rather than two. What Arrow buys, and why it stays: a typed
column needs no per-cell coercion, and a ten-thousand-row page decodes in a fraction of the
executor's per-row cost (spec §4.1); a bulk loader that already holds a table sends it as it is.
The ingest driver (`test_corpora/common/ingest_cycle.py`) keeps Arrow for that reason. A page of
base64 member ids costs about 15 bytes a member in either encoding (measured on rung 3,
2026-09-05), so no third form is needed for sets.

**Built 2026-09-07 (T1) for the routes that exist**: `POST /control/ingest` takes JSON and Arrow
by content type and decodes both into one record batch before the row rules run; `PATCH
/control/layers/{name}/artifacts` takes JSON and an Arrow form of one row per artifact with the
envelope in the schema's metadata; `PUT` stays JSON, an artifact record being object-shaped; a
request with no content type is JSON, and any other content type is refused naming the two. A
timestamp is integer microseconds since the epoch, the spelling the roster's `timestamp_us` takes.
A null or absent `access` at the JSON door and a null list cell at the Arrow door are each a row
with no label, as an empty list is, so the two doors agree; only a column absent from a whole
batch is refused at either. The routes
marked ⊘ in §1.3 take JSON when they are built.

### 1.3 The kinds

The table is the model. `⊘` marks a unit that does not exist yet; every route without the mark is
built and is described in contracts §3.4. **Visible** has one value for every row and it is stated
once here: the acknowledgement means durable, and every effect becomes visible at the **next
publication**, which is the flush tick (`flush_max_age_secs`, 90 s by default, or earlier on the
row trigger or `POST /control/flush`; write-path §4.1). A declaration is listed on `/v1/meta` at
the same moment. Nothing is visible at the ack. A subject **exists for resolution** at the ack:
an attribute declared in one request can be filled in the next, an artifact published in one grown
in the next, and only viewers wait for the tick.

| Kind | What a client sends (one record) | Route | Commit | Pagination units | Idempotency |
|---|---|---|---|---|---|
| **Point** | id, coordinates, labels, attribute values, one key or list per layer column | `POST /control/ingest` | the commit window's fsync | `max_batch_rows` 10,000; `max_batch_bytes` | batch id + body hash within the WAL retention window; a supplied external id refuses a duplicate (`409`) |
| **Attribute values for an existing entity** | id, attribute columns, layer columns; no coordinates; the view header where a group-scoped column is carried | `POST /control/values` (spec §1.4) | the commit window's fsync | `max_batch_rows`; `max_batch_bytes` | batch id + hash; the fill rule per cell |
| **Membership** | the point's layer column, or a page of `members` per artifact | ingest, values, or `PATCH /control/layers/{name}/artifacts` | on the ingest and values routes, the window's fsync (artifacts-from-points §6.2); on `PATCH`, its own append and fsync on the executor | `max_members_per_request` ⊘; `max_body_bytes` | set join: a retry adds nothing (`joined: 0`) |
| **Generating set of content *k*** ⊘ | a page of members at `rank: k`, `joining` and `leaving`; `leaving` resolves a deleted item, which is its purpose (spec §1.5) | the same `PATCH` | its own append and fsync | as membership; the only route by which a set changes | set join and leave; a retry is a no-op |
| **Artifact record** | key, and on a group-scoped layer `view`; `parent`, `attached_to`, shape, content values; a first page of members and generating sets | `PUT /control/layers/{name}/artifacts` creates; `PATCH` fills | its own append and fsync; ordinals claimed contiguously | `max_artifacts_per_request` ⊘; `max_body_bytes` | a held key with identical parts is accepted with no effect (spec §1.5) |
| **Membership by exclusion** | `excluding`: the entities the membership leaves out, on the artifact record | `PUT` | its own append and fsync; the complement is taken on the executor | one request, admissible only while the view's entity count is under `max_excluded_per_request` (spec §2.3) | a second `excluding` on a held key is `409` |
| **Layer** | the `[[layer]]` block minus acquisition keys | `PUT /control/layers` | its own append and fsync | one request; a declaration is kilobytes | identical redeclaration answers the existing identity ⊘ |
| **View of a group** | the roster record | `PUT /control/views/{group}/{key}` | its own append and fsync | one request | as above |
| **View group** ⊘ | the `[[view_group]]` block minus roster and source | `PUT /control/view_groups/{name}` | its own append and fsync | one request; its views follow one by one | as above |
| **Plain view** ⊘ | the `[[view]]` block minus source | `PUT /control/views/{name}` | its own append and fsync | one request | as above |
| **Vocabulary** ⊘ | the `[[vocabulary]]` block minus source; values inline where they fit | `PUT /control/vocabularies/{name}` | its own append and fsync | the declaration, then pages of values | as above |
| **Vocabulary values** ⊘ | a page of `{key, code?, title?, …}` | `PATCH /control/vocabularies/{name}/values` | one `VocabularyMint` per value, one fsync per page | `max_values_per_request` ⊘; `max_body_bytes` | append-only; an identical value is a no-op; a different property on a held key is an upsert of properties, never of key or code |
| **Attribute column** (built 2026-09-07, T4) | the `[[attribute]]` block minus acquisition keys | `PUT /control/attributes` | its own append and fsync | one request | as above |
| **Suppress, delete, unsuppress** | `{id, op}` | `POST /control/changes` | the deny window | about 10⁴ per request; `max_body_bytes` | each op idempotent |

A discovered category value is not a kind: it is minted at the window close of the batch that
carries it, by the view-first rule (per-point-attributes §5), and this document changes nothing
there. A declared category value is a vocabulary value above.

**Built 2026-09-08 (T2c), two rows of the table.** *Membership by exclusion*: `excluding` on a
`PUT` record carries the entities the membership leaves out, the executor complements the list
once against the view's entity set before the record is written, and a second `excluding` on a
held key is the `409` the idempotency column states; the row's route is `PUT` alone, a growth page
being track T2b's and a membership never shrinking (spec §10, R7). *The artifact record's `view`*:
required on a group-scoped layer and refused on an entity-scoped one, part of the key's uniqueness
scope, and carried in the publication record so a replay lands the artifact in the view it was
acked in, and in the packed extent's blob so a fold and a reopen land it there too
(`bundle_format` 8, spec §7.1). The growth route addresses no group-scoped layer, carrying no
view. A view the layer's own group has no key for is refused rather than acked and drawn
nowhere, in the words the build refuses the same row in; the roster read is the generation's,
which is the build's views plus every view created since, so a view created a moment ago passes.
**A group-scoped level is filtered by view wherever a row form is built** — the projection, the
flush's extension, the merge's rebase, and a spatial level's resolution, decomposition and
inverted column alike — so an artifact of one view has no membership and no count on another
view of its group.

**A level under continuous paging.** An artifact's served forms, its row-space membership operator
per view, its generating-set operators and its lineage, are derivatives of the store. Under a
paged load the store changes many times a minute, and a design that re-derived a form on each
request, or rolled a form forward under each page, would put the paging rate on the request path.
The simple design is chosen: **the executor publishes a level's row forms at the flush tick**,
applying the deltas accumulated since the last publication, together with the cardinalities those
deltas moved; a request never builds a form and is served the forms as last published. The served
form may therefore lag the store by up to one tick. That is stale and never unsafe: a member not
yet in the operator is not counted, so a count can only understate, and the mask is composed
against the request's own generation as today (I1, I11). This is the same one-moment rule as a
point's flush, and it is why the visible column above has one value.

**Built 2026-09-08 (T2b)** for memberships, generating sets and fills. The executor holds each
accepted write's delta against the level version it followed and applies the run of them to every
held row form at the tick, in every view the generation carries: a membership join and a page of
joins alone are unioned into the served operator, a page holding any leave and a fill re-derive
that artifact's operators from entity truth, and every ordinal a page or a fill touched has its
stored cardinalities read from the store in the same pass. A request builds a form only where none
is held; one held at an earlier level version is served as it stands. A level whose delta moved a
generating set gives up its containment partition and answers containment from the mask, which is
exact: the partition's expression was composed against the set as it was, and against a set that
has since grown it would answer about a smaller one. The flush and the merge arms are unchanged.

### 1.4 The values kind

A build reads attribute files by entity id and joins them to points that were read from another
file (artifacts-from-points §7: joins are reported, not refused). Ingest has no such join today: a
value travels only on the row that creates its entity, and a joining row's values are compared and
then discarded (contracts §3.4, r65). So a column declared over an existing corpus cannot be
filled, which is the third of the owner's three cases and a reach gap under decisions 0091 and
0134.

`POST /control/values` takes a batch of `(external_id | tessera_id, …attribute columns, …layer
columns)` over entities that exist. Per cell it applies the fill rule: an absent cell takes the
value; a cell holding the same value is a no-op; a cell holding a different value is a `409`
naming the column and the id, and the batch has no effect. That is the comparison the join arm
already makes on the executor (contracts §3.4 r68), with the write the join arm withholds. A batch
carrying a **group-scoped** column carries the view header, and the key-in-group check the ingest
route runs at the join (contracts §3.4 r68) runs at the fill. A layer column on a values row is a
membership join for an existing entity, through the same `ArtifactGrow` the point route uses.

**A blob-resident value on a flushed entity is read.** The record blob is a stack of layers, the
base and one extent per flush, and today an entity's record is read from the one layer that holds
its row. Under the values kind an entity can hold a row in more than one layer: the layer that
created it and the layer that filled a column later. The fill rule leaves **a column held by at
most one layer per entity**, and the per-column presence extents the flush already writes
(spec §6.3) say which. The record stack therefore locates, per column, the layer that claims it
and reads that block. The cost at drill-down is one block decode per column claimant, which for an
entity with no filled column is the one decode it pays today; the fold folds the layers into one
row again. A `text` cell that has flushed is compared against the block its claimant holds.

Why a route and not a mode of `/control/ingest` (spec §10, R2): a points batch allocates entities
and needs a view; a values batch allocates nothing and names a view only for a scoped column.
Elasticsearch makes the same distinction as an explicit `update` action beside `index` (spec §3).

**Built 2026-09-08 (T3).** `POST /control/values` takes a batch of rows in JSON by default and
Arrow by content type, each row naming its entity by `external_id` or by `tessera_id` with its
idset, resolved at the boundary so the executor and the log see entities alone. The fill rule runs
on the serial writer beside the join arm (`plan_fills`), against three sources in the order a cell
is claimed: the entity's own buffered row, the cells an earlier values batch filled and no flush
has written, and the flushed homes. A cell nothing holds is filled, one holding the identical value
is counted and dropped, and one holding a different value refuses the batch with a `409` naming the
column and, for a group-scoped cell, the key. A scoped column may be named only where the batch
carried the view header and the view's key is in the attribute's group's key set. A layer column is
a membership join through the growth route's own record, appended and fsynced with the values
record so no cell is durable without its membership; a key no artifact holds refuses the batch,
because a values batch mints nothing. The cells wait in the buffer's fill map, which the flush
writes into the family's entity-space extent, the text layer and the record blob; a tick whose only
work is fills publishes those and no segment, a fill having no row for one to hold. Two pagination
units are published under `limits.values`.

**A `render` column is refused rather than filled**, and this is narrower than spec §6.3's R10.
A fill acquires no row, so it never reaches the hot column — and the hot column is the only home a
tile and the drill-down read a rendered value from. Two shapes, refused for two different reasons.
A column declared `render` and not `index`, and not a `derived` category, has **no home at all**
for a fill: it owes no entity-space value column, and it is not blob-resident precisely because it
renders, so the value would be acknowledged and stored nowhere. A column declared `render` **and**
`index`, and a rendered `derived` category, do have an entity-space column, so a fill would be
stored and would answer a filter that took the entity route — and would still draw absent on every
tile and at every drill-down, and answer nothing to a filter whose request made the row route
cheaper, which is a per-request cost choice (decision 0068) and not a property of the column. One
column answering two ways depending on the shape of the request is why this is refused rather than
half-served.

What R10 says is true of the entity route: a back-filled `render` value where `index` was declared
is filterable. What it does not say is that the value is drawn nowhere until the fold. **The owner
has not ruled on the gap** and the refusal is what holds the two surfaces together in the
meantime; the alternatives are to serve the value to filters and not to tiles, or to let a fill
write the row tail, which would make a values row a row.

**A values batch under one view may fill a group-scoped cell on an entity that holds no row in
that view.** The address of a scoped value is `(attribute → its group, key)` and never the view
(decision 0116), so the cell exists as soon as the key does, whatever rows the entity has: the
fill is admissible and the value is served under every view of that key. What the entity does not
gain is a row — a values batch creates none — so the value is answered wherever a filter or a
drill-down reaches the entity through some other view, and is drawn on this view's map only if the
entity acquires a row in it by an ordinary ingest.

### 1.5 The artifact and its parts

An artifact has one identity, `(layer, level, key)`, and on a group-scoped layer `(layer, level,
view, key)`: **`view` is part of the identity, required at publish, and never a fillable part**
(views §3.5: keys are unique per `(layer, view)`, and edges may not cross views). Its parts are of
three sorts. **Fixed parts** are filled once: `parent`, `attached_to`, the shape, and each
content's values. **Set parts** change by delta: the membership (rank null) and each content's
generating set (rank *k*), which is the member table's own grain at the build (`(key, entity,
rank)`; annotation-write-cycle §6.1), each carrying its stored cardinality (spec §1.1). **Computed
parts** are derived per viewer and are never sent.

`PUT` creates the record with whatever parts the caller has, including a first page of each set.
`PATCH` fills a fixed part that is absent (built 2026-09-07, below) and applies set deltas ⊘: per
artifact `{key, members?, leaving?, rank?, parent?, attached_to?, content?, shape?}`. The
extension is what decision 0091 requires for enrichment: a build's `artifacts` table is
enrichment over artifacts the points minted (artifacts-from-points §3), and at ingest an
artifact minted from a column receives its name, parent or content by `PATCH`.

**How a fill is recorded and compared.** A fill of a fixed part is its own WAL record,
`ArtifactFill{layer, level, ordinal, part}`, applied through its own store path
(`ArtifactStore::fill`), beside `ArtifactPublish` and `ArtifactGrow` and never by rewriting the
publication record. The comparison that makes a repeat safe reads a **digest stored in the
record**: each content value and each shape is stored with its digest at publish or fill, and a
later record carrying the part is compared digest to digest, so repeat-safety survives a repack of
the level. A `PUT` whose batch mixes keys the level holds with keys it does not is **partitioned
before any ordinal is claimed**: every held key is checked for identity of parts and resolved to
its existing ordinal, every new key is allocated, and a held sibling named as a parent by a new
artifact resolves to that existing ordinal (spec §10, R3).

Three consequences of the parts model, each a change to a rule that exists:

- **Lineage may be filled on an artifact that holds none** (spec §10, R4). The cycle check today
  is publication-scoped because a growth never adds an edge (dag-hierarchies §4). A fill of
  `parent` on a held key adds edges into an existing graph, so the check becomes a walk **over the
  layer by `(level, ordinal)`**, taking the held edges and the batch's own adjacency as one graph,
  refusing if a walk from a named parent reaches the child. The walk is over artifacts, not
  members; rung 3's level holds 30,954 nodes and 42,287 edges, so it is microseconds. It runs on
  the executor, serially, so no two requests can each pass and together close a cycle. The served
  lineage cache gains a **second version counter**: a growth moves the membership version and
  never the lineage version, so a growth never rebuilds the hierarchy and only a lineage fill does.
- **A generating set may precede its content.** Decision 0135 lets a set change at any time; what
  the order buys is that content is never served against a partial set. A caller supplying a set
  larger than one page sends the pages, then the content. Content supplied first is lawful and
  is served, from the next publication and while the pages arrive, to every principal who can see
  the part of the set declared so far, which is C12's shape and spec §6.2's subject.
- **A layer declaring supplied content publishes an artifact without it, and reports the count**
  (spec §10, R5). The refusal that stands today (artifacts-from-points §3) forces the content onto
  the `PUT`, and with it the first page of the set, which is exactly the widening window above.
  An artifact without its declared content is served without content, indistinguishable from one
  whose content is withheld, which discloses nothing; it is a fidelity signal, and the `201` carries
  `without_content: n` so a pipeline sees it.

**Built 2026-09-07 (T2a).** `ArtifactFill` is written and applied through `ArtifactStore::fill`,
the one method the live path and replay share; a content's values and a shape are compared by
their stored digest, parents and an attachment by their resolved references, and the log's copy
of a shape is rebuilt through the shape constructor and its digest compared before it is trusted.
`PATCH` carries `parent`, `attached_to`, `content` at a rank and the shape fields; `PUT`
partitions its batch before any ordinal is claimed, fills and joins on a held key, and answers
`created`, `without_content`, `filled` and `joined` beside the identifiers, `201` where anything
was created and `200` where every key was held. The cycle walk runs over the layer by
`(level, ordinal)` with the batch's edges, and the served lineage is keyed on the level's own
lineage version, which a growth does not move. A layer declaring supplied content publishes an
artifact without it and the count is reported; the artifact is withheld until a content is
filled. Two things wait on later tracks. A content fill carries values and no generating set,
since the set is a page at the rank (T2b), so on a layer whose content requires every member
visible a fill is refused, and a set supplied beside a content fill on any layer is refused; a
content and its set arrive together on a new artifact's publication until then. A key repeated
within one batch is refused at both routes where any of its rows carries a fixed part, since
two rows filling one part would each read it as absent; a `PATCH` repeating a key with members
alone is a join twice. And a content filled at a further rank on an artifact whose
content row is already in a durable extent is refused, because the record blob holds one row per
entity until the per-column read (§1.4, T3); a first content on an artifact that has none is
written beside the level's tail whichever side of the high-water the artifact sits. A fill pins
the log as a growth does, until the fold rewrites the level; a content's values pin it until the
content extent naming them is durable, since the fold carries content extents forward. A fill's
delta reaches the level's row forms at the tick as a growth's does (§1.3): the ordinal it changed
has its records entry and its operators read from the store again, the membership being untouched
by a fill.

**Deletion, and the repair the caller makes.** Under decision 0135's amendment there is one
behaviour when a member of a generating set is deleted, and the set page is the caller's only
tool. From the deletion's acknowledgement the deleted item is outside every mask, so containment
fails for every principal by arithmetic and the content is served to nobody. At the fold that
executes the deletion, a content whose set still holds the deleted item is **withdrawn** and
**reported**: the fold's report names the layer, the artifact's key and the content's rank
(write-path §5.8's notification obligation, ⊘ unbuilt; it carries no identifier, since the operator
who deleted the item holds its external id). The caller repairs through ingest, in either order:

- **before the fold**, a page at rank *k* with `leaving` naming the deleted item, which resolves
  while its binding stands (a deleted holder's binding survives; contracts §3.4, compaction §6),
  after which containment can pass again and the content is served from the next publication to
  whoever satisfies the set that remains, unless the set is now empty (spec §1.1);
- **after the fold**, the content is absent, so the caller fills it again, with a set that omits
  the item, in as many pages as it needs.

Those are the two repair routes, and there is no third: **fixed parts are write-once**. A content
is supplied once and replaced never; what changes under a served content is its set, by pages, and
what removes a content is the fold's withdrawal or an emptied set (spec §1.1), after which the
caller supplies it again. Editing a content in place is decision 0077's deferred pass and is not
opened here. `withdraw_on_member_deletion` is untouched: it withdraws the artifact, where this
withdraws a content.

### 1.6 Ordering a client must keep

The monotone rule makes most orderings free. What remains is that a record cannot name a subject
that does not exist, resolved once at the boundary and never later (I10):

1. an entity before any member list, generating set or values row naming it;
2. a vocabulary before a declared category value naming one of its keys; an attribute before a
   values column for it; a layer before its artifacts; a group before its views;
3. a parent before its child, or in the same batch (dag-hierarchies §4);
4. the pages of a generating set before the content served against it, if the caller wants no
   principal to read the content against a partial set; and joins before leaves across pages, if
   the caller wants no intermediate set narrower than both the old and the new (spec §2.2).

The 2026-09-03 ordering ruling (every artifact after every point it depends on) is the first line
above and nothing more: an artifact record may precede its points, and grows as they arrive.

## 2. Scale without ceilings

### 2.1 What a cap is, and where the memory bound sits

Every request body on the control plane is buffered whole before its handler runs, and the count
of authenticated connections is bounded by the deployment's proxy and by nothing in this process
(`config.rs`, `INGEST_MAX_BATCH_BYTES_CEILING`). The resident cost of ingest is therefore
`connections × body cap`, and the cap is what makes an unbounded count survivable rather than
fatal: at the 64 MiB ceiling a hundred concurrent uploads is 6.4 GB; at the 8 GiB the other
startup relations would admit, one is fatal. That argument is correct and this design keeps every
one of its caps. What it changes is the classification: each is a **pagination unit**, every route
publishes two, a record count and a byte cap, and a payload larger than one page is several pages,
each complete in itself.

| Route | Records per request | Bytes per request | Also |
|---|---|---|---|
| `/control/ingest` | `max_batch_rows` 10,000 (the commit window's size, so no client picks the sort scope) | `max_batch_bytes` 16 MiB, ceiling 64 MiB | |
| `/control/values` | `max_batch_rows` | `max_batch_bytes` | |
| `/control/layers/{name}/artifacts` `PUT` | `max_artifacts_per_request` 10,000 | `max_body_bytes` (`publish_max_body_bytes`, 64 MiB) | `max_shape_vertices` 10⁶; `max_excluded_per_request` 10⁶, enforced per artifact's list (spec §2.3) |
| the same, `PATCH` | `max_members_per_request` 5,000,000, the sum over the page's artifacts | `max_body_bytes` | |
| `/control/vocabularies/{name}/values` ⊘ | `max_values_per_request` | `max_body_bytes` | |
| `/control/changes` | 10,000 | 2 MiB | |
| declarations | one | 2 MiB | |

All of it is one `limits` block on `/control/status` (spec §10, R1). A client reads the block once
and sizes every page from both units; a page over either is a `422` naming the limit and the
field, never a truncation. The record counts are of one shape and one reason: a page's cost on the
executor is linear in its records, and a count bounds what one executor step does where bytes
bound what one connection holds.

**Built 2026-09-07 (T1)**: the block, keyed `ingest`, `publish`, `grow`, `changes` and
`declarations`, each entry naming its route; the four `[ingest]` keys `publish_max_body_bytes`,
`max_artifacts_per_request`, `max_members_per_request` and `max_excluded_per_request`
(configuration §1); every count and byte cap of the routes that exist enforced at the published
value as a `422` naming the unit; and the driver reads every limit it sizes a request by from the
block. The `/control/values` row was added at T3; the vocabulary values route waits on its own.

### 2.2 Why no kind needs a multi-part upload

A multi-part upload exists to commit, in one moment, something that could not be sent in one
request. Three candidates were examined and none survives.

**Memberships and generating sets** are sets, and a set assembled by pages is the set, whatever
the order or the timing of the pages. Decision 0127 settled this for memberships; decision 0135
extends it to generating sets by removing the rule that made their assembly need a single moment.
Under decision 0135 a set is also *replaceable* without a session, **when paged joins-first**: the
caller sends the entries joining, then the entries leaving. Within one page the server applies
joins before leaves (spec §1.1); across pages the order is the caller's. Paged that way, every
intermediate set is a superset of both the old and the new, so at no point is the content served
to a principal who could see neither, which is the one property a one-shot replace would have
bought. A caller who pages leaves first can empty the set mid-replace; the page that empties it
withdraws the content and says so in its acknowledgement (spec §1.1), the content does not return
when the set refills, and the caller supplies it again after the last page.

**Artifact records** are one request each because ordinals are claimed contiguously at a level's
cursor and a refusal must spend nothing. Nothing in a record is large except its sets, which page.
The fixed parts together are kilobytes; a shape is the one exception and is bounded by
`max_shape_vertices` (spec §2.3).

**Points and values** are rows, and rows page.

What a staged upload would have cost is recorded so it is not rediscovered: a record kind for a
part and one for a commit, a staging index rebuilt at replay, a WAL pin per open upload on a log
that has no runtime ceiling, an abandonment rule with a clock, and a first object in the system
that is durable and deliberately never visible. S3 carries all of that (spec §3) because an
object has no parts a client can address once it is complete; every subject here does.

Chunked transfer of one request was declined for a different reason: a proxy buffers it
(nginx buffers request bodies by default), a dropped connection loses the whole upload rather
than one page, and the server would need the same part-and-commit records to make the partial
body durable before the end of the body. Pagination is what S3, Elasticsearch and every vector
store converged on for the same reasons.

### 2.3 The bounds that remain, and why each is not a ceiling on reach

Three per-object bounds survive, published in the `limits` block, and the argument for each is
that the object has a smaller spelling or is already bounded elsewhere (spec §10, R8).

- **An exclusion list is one request, bounded by `max_excluded_per_request`**, a record count
  of the same shape as `max_members_per_request` and a proposed default of 1,000,000 (about
  15 MB of base64 ids, under the byte cap). What must fit one request is the list, because the
  complement is taken on the executor against the view's entity set as of that step (every entity
  holding a row in the view or buffered for it, deleted entities excluded), and it cannot be taken
  until the whole list is in. The complement itself is not bounded and need not be: it is one
  `andnot` over the view's entity bitmap, in spec §4.1's units one bitmap operation costing by
  containers touched, producing a membership of order the view's size, about 12 MB at 10⁸
  entities (modelled from the per-artifact figure in spec §2.4), which is less than one membership
  page. A list over the bound is spelled as an inclusion, which pages. The build's rule that the two spellings are byte-identical in the bundle
  (annotation-write-cycle §6.1) holds at ingest **in a quiescent database**: the complement is
  materialised before the record is written, and the record is the inclusion's. Two things
  diverge under concurrent writes, and both are stated rather than closed: an entity ingested
  after the complement was taken is in the inclusion's membership if the caller listed it and not
  in the exclusion's, because the exclusion was evaluated over the entities that then existed; and
  a suppressed entity is in both (suppression is not deletion), where a build has no suppressions.
  **Built 2026-09-08 (T2c)**: the list is refused over the bound with a `422` naming the limit and
  the inclusion spelling; the complement is one `andnot` on the executor over the view's entities
  — every entity holding a row in the view or buffered for it, deleted entities excluded — and its
  size is logged. `members` and `excluding` on one row is a `422`, and so is a record carrying
  neither: `members` is optional only where `excluding` is given, an empty list being the
  membership that holds nobody. **Which view**, since an
  entity-scoped layer is drawn on several: the artifact's own on a group-scoped layer, where it
  belongs to one; the union of the layer's declared views otherwise, that being the corpus the
  layer is drawn over. On a single-view corpus the union is the whole entity space, which is what
  the build complements against (`0..high_water`), so the two doors agree where the build has an
  opinion.
- **A single value is one batch.** A text cell, a keyword, a number arrive on one row, and a row
  cannot page. The bound is `max_batch_bytes`, 64 MiB at the ceiling. What such a value costs, so
  the bound is understood as a bound on cost and not a ceiling on reach: the analyser runs at
  116.5 µs per abstract of about 1,500 characters (measured, PaperSeek 92M), which is about
  78 ns a character and so **about 5 s of the pool's one flush thread for a 64 MiB value**
  (modelled); at drill-down the value is its own record-blob block and decompresses at 1.5 to
  1.7 GB/s (measured), about 40 ms, before it is served whole (C26's channel, unchanged in kind).
  The blob's block target is 256 KiB, so a value near the bound is three orders past what the
  storage was shaped for. Accepted and published.
- **A shape is `max_shape_vertices`.** A polygon has a lossless smaller spelling only in the sense
  that `ST_Simplify` keeps what a map at any served zoom can show (artifact-shapes: the vertex
  budget is a wire guard, not a fidelity control), and the served hull is itself vertex-budgeted.
  Accepted and published.

### 2.4 The WAL pin, with numbers

A growth record's pin is per record, and the rule is the one the store's own comment describes
(spec §10, ruling 5): **a growth of an artifact above the level's published high-water is released
at the next tail pack**, the flush that appends that artifact's record to the level's extents with
its membership as it then stands; **only a growth of an artifact below the high-water waits for the
fold**, because the append-only route never rewrites a packed record. Under a paged load the
pinned bytes are therefore: for artifacts published in the current interval, the pages since the
last tick, released at the tick; for artifacts packed in an earlier interval, every page since,
until the fold. A portable Roaring bitmap over a 10⁸-member artifact is about 12 MB
(artifacts-from-points §6.1, modelled). A hierarchy loaded roster-first and then grown, which is
the driver's order, pins each artifact's pages only until the tick after its publication **when the
artifact's growth completes within the interval it was published in**; the log then holds at most
one interval's pages, of the order of the interval's members at 0.1 to 1 byte each. Growth that
spans ticks lands its later pages on a packed record: every page after the first tick pins until
the fold, up to about 12 MB per 10⁸-member artifact, so a 10⁸-member artifact grown over several
intervals pins nearly all of its bitmap until the fold, and a hierarchy grown long after its
publication, rung 3's 1.66×10⁹ closure entries onto packed records, pins on the order of 10⁸ to
10⁹ bytes until the fold (all modelled from the per-member figure; not measured). The log has no runtime ceiling (write-path §1.3) and replays every pinned
record at restart, so the operator's rule is the fold's own: fold after a large growth onto packed
records. Packing growth at the flush for records below the high-water is the second packing rule
artifacts-from-points §6.1 declined, and this design does not reopen it.

## 3. Comparable systems, and the three cases as a client sees them

### 3.1 What each does for exactly this problem

| System | Unit | Size | Atomicity | Idempotency | Large object | Schema growth |
|---|---|---|---|---|---|---|
| Elasticsearch `_bulk` | NDJSON of actions | `http.max_content_length` 100 MB; clients chunk at 500 docs | per item, never per request; partial acceptance with a per-item status | `_id` with `create` (409 on duplicate); `if_seq_no` | none; a document is one line | `PUT _mapping` adds fields; a type change is a reindex behind an alias |
| PostgreSQL `COPY` | a stream of rows | unbounded; streamed to WAL and heap | one transaction, all or nothing | none in `COPY`; `ON CONFLICT` on `INSERT` | a large value is one row (TOAST) | `ALTER TABLE ADD COLUMN` is metadata; a back-fill is `UPDATE` in batches |
| S3 multipart | parts of 5 MiB to 5 GiB, up to 10,000 | 5 TiB | `CompleteMultipartUpload` is atomic; parts are invisible until it | a part is idempotent by number; the complete call by upload id | this is the mechanism | none |
| Kafka | producer batches per partition | `max.request.size` 1 MB default | per partition; transactions across partitions with `read_committed` | producer id + sequence | none; a record is bounded | none |
| Pinecone / Qdrant / Weaviate | upsert of vectors by id, JSON | 2 MB / 1000 vectors (Pinecone); 32 MB (Qdrant's default payload) | per request (Qdrant `wait`), per object (Weaviate batch errors) | upsert by id | none | fields are free-form |
| Snowflake, BigQuery | a staged file, then a load job | files in a durable stage | one job | job id | staging is the mechanism | `ADD COLUMN` |

Two families. Row stores and search engines page and upsert by id in JSON, and a client library
hides the page size behind an iterator (`helpers.bulk`, `copy.write_row`, `index.upsert(batch)`).
Object stores and warehouses stage and commit, because their object has no addressable parts
after commit. Tessera's subjects all have addressable parts, so it belongs to the first family,
and the one place it differs from Elasticsearch is deliberate: **a request is whole or without
effect**, never partially accepted, because entity ids and ordinals are allocated contiguously and
a refusal spends nothing (write-path §2.3). A client author therefore needs one retry rule and no
per-item status parsing.

What a client library author expects, from that evidence: a page size they can read rather than
guess, in records and in bytes; an iterator that splits a table into pages; `429` with
`Retry-After` and a bounded retry; an idempotency key per page so a lost acknowledgement is safe to
resend; fills and set pages that are no-ops on a re-run, and a stated resume rule where creation
cannot be (points: the batch id within retention, the caller's bookkeeping past it, spec §1.1); a
schema call that is safe to repeat; and JSON. Every one of those is in spec §1 and spec §4.

### 3.2 The three hardest cases, as SDK calls

Python, against the SDK in `clients/py`. `paginate` reads `limits` once and yields pages under both
the record and the byte unit; every call retries a `429` after its `Retry-After` and resends
identical bytes on a lost acknowledgement. Pages go as JSON unless the caller hands the SDK an
Arrow table, in which case they go as Arrow.

**A 10⁹-row corpus streamed in.**

```python
w = tessera.Writer(url, credential)                 # reads /control/status limits once
for i, page in enumerate(w.paginate(rows, kind="points")):
    w.ingest(view="s0", page, batch_id=f"{run}-{i}")   # 200 {accepted, tessera_ids, minted, …}
w.flush()                                            # optional; the tick flushes anyway
```

Each page is one request, one commit-window entry, one durability receipt. Visibility is the
flush. A page re-sent under its batch id within the WAL retention window is a replay; a load
resumed past it relies on the caller's own record of which pages were acknowledged, and on the
external ids, which make a page sent twice a `409` rather than a second copy. At the measured rates (spec §4.1) the loop is bounded by the pool's flush of the widest
column family, not by the client.

**An artifact with 10⁸ members, content, and a generating set of 10⁷.**

```python
a = w.layer("topics/openalex").artifact(key="3", parent=["root"])
a.publish(members=first_page)                        # PUT: the record, ordinal claimed
for page in w.paginate(members, kind="ids"):
    a.join(page)                                     # PATCH members: 23 pages at 4.4M each
for page in w.paginate(generating, kind="ids"):
    a.join(page, rank=0)                             # PATCH rank 0: the set, before the content
a.fill(content=[{"rank": 0, "values": ["Machine learning"]}])   # PATCH: content
```

Five calls in the SDK's own vocabulary; 27 requests on the wire; no token, no session, no
commit call. A re-run of the loop is a no-op at every step. The artifact is served from the first
tick after the `PUT` with the members published by then, its content from the first tick after
the last call, contained against the whole set.

**A new attribute column declared and back-filled over an existing corpus.**

```python
w.declare_attribute({"name": "sentiment", "type": "float32", "index": True, "render": True})
for i, page in enumerate(w.paginate(scores, kind="values")):    # rows: {external_id, sentiment}
    w.values(page, batch_id=f"sentiment-{i}")
```

The declaration is one request and is safe to repeat. The column is absent for every entity until
its page lands; a filter on it matches the filled rows at their flush; the rendered value reaches
the row tail at the fold (spec §6.3).

## 4. Performance

### 4.1 The throughput model, per kind

The executor is one thread per partition and the flush runs one plan at a time on the pool
(write-path §1.1, §4.2). Sustained ingest is therefore bounded by the slower of the executor's
step and the flush, per record, and the flush is the term at every scale measured:

| Kind | Per-unit cost | Class | Bound |
|---|---|---|---|
| points, titles | window close 11.6 µs/row; flush 14.8 µs/row, 69% the text index | measured, MedCPT 36M, 2026-09-05 | 50,090 to 65,194 rows/s measured; 10⁹ rows in about 4.5 h, modelled from the per-row figure with the base held constant |
| points, abstracts | flush 133.5 µs/row, 90% the analyser | measured, PaperSeek 92M | 7,427 rows/s measured against a 7,490 ceiling; 10⁹ rows in about 37 h, modelled |
| values | the flush cost of the columns carried, without the segment, permutation and allocation stages; plus one blob layer per fill at the fold's rewrite | modelled from the stage table (`segment`, `rows`, `promote` are 1.5 to 1.9 µs/row) | between the two rows above, by family |
| membership and generating-set pages | per member: one base64 decode and one resolution against the external-id sidecar runs on the pool, then a bitmap insert on the executor; per page, one append and one fsync | modelled; the driver records `members_per_s` per layer and the figure is read there | the resolution's binary search over the runs |
| a level's row forms at the tick, joins only | per `(artifact, view)` with a delta of joins: the delta's rows through the permutation into a **shared projection scratch**, one union into the served operator, and the cardinality it was derived with written beside it; nothing for an artifact with no delta | modelled; the same union the growth path performs today, moved to the tick | linear in the interval's delta members, once per tick, on the executor |
| the same, any leave in the delta | the `(artifact, view)` operator re-derived whole from entity truth: every member through the permutation into the scratch, then written with its cardinality | modelled; the projection the operator was first built with | linear in the artifact's size, once per tick per artifact with a leave; a generating set is small beside a membership, and memberships never leave |
| the fold | every operator re-derived whole | as the fold's artifact pass today | the fold's own budget |
| artifact records and fills | per artifact: key and parent resolution, the cycle walk, a content digest | modelled; kilobytes each | negligible against the pages |
| declarations | one WAL append and one fsync | modelled by analogy with the deny ack's 3.2 ms quiescent | one per declaration |

**Built 2026-09-08 (T2b):** the three row-form arms above run at the tick and the line the
executor logs prices them — the deltas taken, the sets unioned, the operators re-derived, the
ordinals published, the rows added, and the copy a concurrent reader forces.

Two things the model says that a loader should know. **The base grows under a long load**, and the
stages that scale with it (`plan`, `compose`, `drop_superseded`) rose 3× to 10× per row between
36M and 92M; the hour figures above hold the base constant and are lower bounds. **The analyser is
the term**, and the two answers to it, a faster analyser or the text index off the flush's critical
path, are the probe's open question and not this document's; nothing here adds threading
(owner direction, 2026-09-02). The tick's row-form publication is the one new term, and it is
priced per artifact touched in the interval, not per artifact in the level: a level of 10⁶
artifacts of which a page touched ten costs ten unions.

### 4.2 Where backpressure lands

| Producer | Kinds | Answer |
|---|---|---|
| admission semaphore, `ingest_admission` 64 | points, values, artifact records, set pages, vocabulary values | `429`, `Retry-After` from the observed service rate, 1 to 300 s |
| command queue, `ingest_queue_bound` 32 | every kind on this plane | `429`, drain-derived `Retry-After` |
| buffer occupancy, `ingest_buffer_max_items` 10⁶ rows | points, values | `429`, `Retry-After` derived from the observed drain rate, as the other two are: the time to the next tick plus the buffered rows at the per-row cost the last flushes took, clamped to 1 to 300 s (built 2026-09-07, T1) |
| the deny lane | changes | never refused for load |

**A publish or growth page takes an admission permit and resolves its addresses on the pool, not
the reactor** ⊘: a page is a buffered body like any other and its resolution is the same
per-member work an ingest page does, so it sits under the same bound and off the same thread.
Pages hold no rows and never meet the buffer bound.

### 4.3 What a client must handle

Every kind, one table:

| Answer | Meaning | Client action |
|---|---|---|
| `200` / `201` | durable; visible at the next publication | next page |
| `409 conflict`, a point page | a duplicate external id, or a batch id replayed with different bytes | past the WAL retention window a re-sent page meets this per id: the page was already taken, move on; otherwise the request is wrong |
| `409 conflict`, any other kind | a part present and different | stop; the request is wrong, not the timing |
| `422 contract` | malformed body, a cell that does not coerce to its column, an unknown column, a page over a limit (naming it), an unresolvable subject named at position *p* | fix and resend; a page over a limit is re-split |
| `429 backpressure` | one of the producers above | wait `Retry-After`, resend identical bytes |
| `500 fail-closed` | durability failed and nothing applied, or the receipt was lost after the swap | resend identical bytes; idempotency resolves which |
| `503 not-ready` | executor not running or a stepped-down partition | retry later |

There is no partial acceptance and no per-item status: a page is whole or without effect.

### 4.4 Idempotency, per kind

Batch id and body hash answer a replay of an ingest or values page within the WAL retention
window (write-path §2.4); past it, a supplied external id still refuses a duplicate point, and a
values page is answered by the fill rule. For every other kind the record itself is the key and
the monotone rule is the idempotency: a set join adds nothing the second time, a fixed part is
compared by its stored digest and accepted, a declaration is identical and answers the existing
identity. A client therefore needs a batch id only where the wire allocates, which is points, and
may carry one on values for the receipt.

## 5. What the build keeps, and what ingest has that the build does not

Decision 0134 lets a build be faster and never reach further. The differences that remain:

| Build keeps | Why it is lawful |
|---|---|
| acquisition: `source`, `fields`, inline data, `--file`, `--limit` | where rows come from, not what they mean (configuration §2); a deployment writing through the service omits them |
| `extent = "auto"` and margin fitting | reads the data to choose the frame; the frame is index configuration and immutable for the view's life (decision 0040), so a deployment that ingests declares its frame |
| signature-sorted allocation over the whole corpus | posting compression 8.9 to 36.7× against runs of order 10¹ at window scope (measured ceiling; modelled window figure, §11.1); latency and footprint, never reach |
| one pass, sorted and packed: 20,400 rows/s against 7,427 at ingest on abstracts | measured, PaperSeek 92M; faster, not further |
| external-id minting where a file carries none | a row without one is addressable by its `tessera_id` at both doors |
| refusing a whole corpus for one bad row | the build has one input and one operator present; ingest refuses the page |

The reverse, which decision 0091 calls equally unfinished:

- **A roster key minted at runtime and a view created at runtime** have no build counterpart, and
  need none: a build materialises what the declaration enumerates, and both are then the growth
  decision 0091 says is what ingest is for.
- **Late lineage** (spec §1.5) fills an edge on an artifact the points minted. A build does this in
  one pass by reading the enrichment table beside the column. The reach is the same; the order is
  free at ingest and fixed at the build.
- **A generating set that shrinks** (decision 0135) has no build spelling, because a build supplies
  each set once. A rebuild is the build's replace.

## 6. Visibility, the invariants and the register

### 6.1 The one visibility moment

Every effect on this plane becomes visible at the next publication, the flush tick, and never at
the acknowledgement (spec §1.3). What a principal can then observe, per event, and which register
row already covers it:

| Event | At the next publication a principal can observe | Row |
|---|---|---|
| a page of points | the rows, inside the mask; before the tick, `x-tessera-stale: 1` on the next response (C15) | none new |
| a page of values | the filled rows match a filter on the column, inside the mask | none new: an ingest in progress |
| a membership page | the artifact's masked count over the members whose rows the operator holds | none new (decision 0127) |
| a generating-set page joining, under a served content | the content stops being served to a principal who satisfied the smaller set and not the larger; they learn an item outside their mask joined a set they could read against | C7, annotated (spec §6.2) |
| a generating-set page leaving | the content is served to principals who satisfy the smaller set; content derived from an item a principal cannot see reaches them | C7 as amended by decision 0135: the declared set, whatever the caller did to it |
| a page that empties a set | the content is withdrawn (spec §1.1) | none: the served state narrows |
| a member of a generating set deleted | the content vanishes for everyone from the deny's ack, by arithmetic; a principal who satisfied the set learns a member was denied | C17's annotation: bounded to principals who could see every member |
| the fold withdraws a content, or the caller repairs the set before it | the content is absent, or reappears to whoever satisfies the repaired set | C7: the reappearance is the caller's re-declaration; C7's second channel (reappearance by the service's own removal) no longer exists |
| content filled after its set | served to principals who satisfy the whole declared set | none new |
| content filled before its set is complete | served, while the pages arrive, to principals who satisfy the partial set | C12: the caller's optimistic set, trusted as declared |
| an artifact on a supplied-content layer without content | served without content, indistinguishable from content withheld | none: withholding is already the served state |
| a level's row forms lagging the store by a tick | counts understate by the members not yet in the operator; the cardinality published beside the operator is the one it was derived with, so containment is never tested against a mismatched pair | none: stale in the safe direction |
| an attribute declared, unfilled | absent everywhere; `/v1/meta` lists the column | none |
| a vocabulary value declared, unused | listed under a `public` vocabulary; under `derived`, invisible until a visible member carries it (C11) | none |

### 6.2 The register

**No row is added for pagination.** Every intermediate state is a state some order of arrivals
could have produced under the routes that exist, and Appendix C's inclusion test asks what a viewer
learns about data they were not served: a page adds nothing to that.

**One annotation on C7** (spec §10, R6). Growth of a generating set under a served content
withdraws the content from a principal who could read it, and the withdrawal says that an item
outside their mask joined the set. It is bounded to content the principal was served, it is the
caller's action, and it is the same class as C17's delete signal on items the principal already
sees. The annotation: *a change to the declared set in either direction is observable by a
principal on one side of the containment boundary, as a served content appearing or vanishing at
a publication; the channel is one bit per artifact per page, at the caller's rate, and it says that
the caller changed their claim. The fold no longer gates it: under decision 0135's amendment a set
changes only by the caller's page, and C7's second channel, a content reappearing at a fold
because the service removed a deleted member, is gone.*

**I2, I3, I13.** Every count and containment test is computed inside the composed mask as today.
A generating set's row-space operator is published by the executor at the tick together with the
cardinality it was derived with, and the containment test reads that pair and nothing else: the
store's current cardinality is not on the serving path before the tick (spec §1.1, §1.3), so I3's
test is against the declared set as last published and never mixes versions, and an operator that
saw a leave was re-derived from entity truth rather than unioned. No cache holds a containment verdict above the test
(I3). A page refused yields no partial state, and a page accepted is whole (I13a). Nothing here
touches I12 or the filter mask.

### 6.3 A runtime attribute's three homes

A column's home is the hot column in the row tail, its family's entity-space structure, or the
record blob (records-and-search §3). A column declared at runtime has no base in any of them.
Entity-space extents are written per flush over the flushed entities' presence, so a values page
lands as an extent whose presence covers the filled entities, disjoint from every other extent for
that column because the fill rule leaves one claimant per cell. The record blob gains a layer, and a column is
read from the one layer that claims it (spec §1.4). The row tail is in the segments, in row space, and a segment
written before the declaration does not carry the column: **absence for a runtime column is
answered from the segment's schema**, the reader taking null for a `render` column the schema lacks
and never opening a blob to find out, and the fold, which rewrites every segment, writes it. Until
the fold a back-filled `render` value is filterable where `index` was declared and not drawn
(spec §10, R10); `/v1/meta` says which state the column is in.

**`POST /control/values` refuses a `render` column rather than leaving it in that state**
(spec §1.4). A fill reaches no hot column, so the value is drawn nowhere; where `index` is not
declared either, the column owes no entity-space value column and is not blob-resident, so the
value is stored nowhere at all. R10 as written accepts filterable-and-undrawn, and whether that is
the state it intends is **owed a ruling**: the alternatives to the refusal are to serve such a
value to filters and not to tiles, or to let a fill write the row tail, which would make a values
row a row.

**Built 2026-09-07 (T4).** The declaration route, the manifest home and its replay, the tail
append with padding at the window close and the flush, the schema-answered absence on the render
tail, the row-route filter, the drill-down and the categories vocabulary, and the fold's
materialisation of the column's base.

**Built 2026-09-08 (T3), the record blob's per-column claimant read** (spec §1.4, spec §10 ruling
7). `RecordStack` reads a column from every layer that holds a row for the entity and unions their
fields, first tag winning where two name one column — which the fill rule makes unreachable and
which is there so a damaged pair answers one value rather than two. The cost is one block decode
per column claimant: one for an entity whose columns the creating layer wrote, and one more per
flush that filled a column on it. The many-row read takes the entities that lie in more than one
layer out of its per-layer walk and answers them through the merged read, so each is visited once
and a stack no page has filled pays what it paid before. The layers are therefore **disjoint per
column and not per entity**: an entity holds a row in the layer that created it and another in
each layer that filled a column on it, and the fill rule leaves one claimant per cell.

## 7. Migration and contracts

### 7.1 On the wire

| Change | Kind | Where |
|---|---|---|
| JSON the default encoding on every route; Arrow by content type | new rule | contracts §3.4, every row; §5 |
| `limits` block: a record count and a byte cap per route, plus `max_shape_vertices` and `max_excluded_per_request`; `publish_max_body_bytes` a config key | new field; new keys | `/control/status` (R1); configuration §9's table gains the `[ingest]` keys it defers today |
| `PATCH /control/layers/{name}/artifacts` carries `rank`, `leaving`, `parent`, `attached_to`, `content`, `shape` | widened body | contracts §3.4, the r77 row |
| `PUT` accepts a held key with identical parts (partitioned before allocation); a different part is `409`; `without_content` on the `201`; `view` required on a group-scoped layer; `excluding` | changed rule; new fields | contracts §3.4, the `PUT` row; the sentence "published with as many members as fit" is replaced by the page rule |
| `POST /control/values`, with the view header for a scoped column | new route | contracts §3.4 |
| `PUT /control/attributes`, `PUT /control/vocabularies/{name}`, `PATCH …/values`, `PUT /control/view_groups/{name}`, `PUT /control/views/{name}` | new routes | contracts §3.4; `CONTROL_PLANE_ROUTES` |
| redeclaration answers the existing identity | changed rule | the layer and view rows |
| the buffer-occupancy `429` carries a drain-derived `Retry-After` | changed value | contracts §3.4, the ingest row |

`api_version` stays at 1: no client outside this repository exists and decision 0048 rules the
change made rather than versioned. `bundle_format` moves from 6 to 7 at T0 and to **8 at T2c**,
the blob gaining the artifact's view beside its key (spec §1.5), because the manifest is the
durable home of every declaration that survives WAL rotation (contracts §2.2's `groups` row makes
the argument for rosters) and gains runtime-declared attributes, vocabularies and groups, and each
stored content and shape gains its digest. `WAL_VERSION` moves from 20 to 21 in **one bump-and-recreate
commit before any track** (spec §8): `ArtifactGrow` gains a rank, a leaving set and the moved
cardinality, `ArtifactPublish` a view, and the log gains `ArtifactFill`, `ValuesBatch`,
`AttributeDeclare`, `VocabularyDeclare`, `ViewGroupCreate` and `PlainViewCreate` (the `[[view]]`
block minus its source, for R9's route; its own record because a plain view carries its frame,
projection and gate itself and belongs to no roster). Postcard is positional, so a stale log is
refused rather than read.

**Built 2026-09-07, the format half (T0)**, and `bundle_format` moved again to 8 at T2c
(2026-09-08) for the record blob's view: `WAL_VERSION` 21, with every
record variant and field above present and serialised. A generating set's cardinality and a
content's digest travel in the `ArtifactPublish` record and the record blob, a shape's digest with
the shape, and the growth record carries its rank, leaving set and moved cardinality as one
`set` discriminant. The runtime-declared lists live in the segments manifest (`attributes`,
`scoped_attributes`, `vocabularies`, `groups`), empty until their tracks. A record whose meaning
is not built refuses the open naming its track; nothing in the rest of this section is built.

**An attribute declared mid-ingest appends at the end of the scalar tail**, and the rows already
buffered for the next flush are padded with absence for it, so one flush writes one schema; a
batch arriving after the declaration carries the column or omits it, either being lawful (T4).

### 7.2 Documents this amends when promoted

- **write-path.md** gains the artifact records in §1.3's list, the set page and its own fsync in
  §2.3, the per-record pin rule in §4.5, the tick's row-form publication in §4.4, and the publish
  limits in §10 (the memo's contradiction 4).
- **contracts.md §3.4** as spec §7.1; the truncation sentence corrected (contradiction 3).
- **artifacts-from-points.md §9**: predicate membership is built and served (contradiction 1);
  §8's asymmetry (a growth never mints) stands.
- **per-point-attributes.md §2.2, §5, §6**: the vocabulary upsert exists once spec §8's T5 lands
  (contradiction 2); until then §6's "still owed" is the true line.
- **dag-hierarchies.md §4**: the cycle check becomes layer-scoped by `(level, ordinal)` with late
  lineage (contradiction 5).
- **configuration.md §9**: names the `[ingest]` keys (contradiction 6).
- **`scripts/campaign_report.py`**: the layer column exists (contradiction 7).
- **decision 0091's ⊘** and **annotation-write-cycle §3.4**'s closure sentence: closed by this
  design for every kind, and not before (contradiction 8).
- **`config.rs`'s "belongs with flush"** (contradiction 9): streaming the upload is declined here
  (spec §2.2) and the comment should say so.

### 7.3 The two durability paths

A membership join carried on an ingest or values row rides the commit window's own fsync as an
`ArtifactGrow` beside the `IngestBatch` records (artifacts-from-points §6.2; settled), so there is
no state in which a point is durable and its membership is not. A page on the growth route is its
own append and its own fsync on the executor, as a publication is. Both are durable at the
acknowledgement; neither is visible before the tick.

### 7.4 The rewrite decision 0135 leaves in annotation-write-cycle.md

Marked at its head; rewritten with this design. The sentences: §2's "never grown (I8)", "`G`
changes on exactly one event, and never by growing" and "shrunk only by the fold on a permissive
layer"; §2.1's strict and permissive modes; §3.1's and §3.4's rows for a generating set under
deletion; §5's edit row where it says a `G` edit needs a content edit; §11's I8 items. What
replaces them is spec §1.5 and spec §6.1: a generating set is a set part changed only by the
caller's pages and carrying its stored cardinality, its row-space operator is published at the
tick, containment is live against the declared set as last published, an empty set serves nothing,
a deletion inside a set withholds the content from the deny's ack and the fold withdraws and
reports it, and the caller repairs by a page or a refill.

## 8. Order of work

Each track is one implementer's, in a worktree, with a referee before merge. The order is by
dependency and by how much of the campaign each unblocks. **T0 comes first and is one commit**: the
WAL bump with every new record and field of spec §7.1, `bundle_format` 7, and the artifacts
recreated, so that every later track lands against one format and none waits on another's bump.

| Track | Delivers | Depends on |
|---|---|---|
| **T0 formats** | `WAL_VERSION` bump; `bundle_format` 7; the record variants and the stored digest and cardinality fields, unread until their tracks; artifacts recreated | — |
| **T1 caps and wire** | JSON on every route with Arrow by content type; the `limits` block with record counts; `publish_max_body_bytes`; the `PUT` row corrected; the driver reads every limit it uses from the block | T0 |
| **T2a artifact record and fill** (built 2026-09-07) | `ArtifactFill` and `ArtifactStore::fill`; the digest comparison; the partitioned `PUT`; late lineage with the layer-scoped walk and the second lineage version; `without_content` | T1 |
| **T2b generating-set pages** | `rank`, `joining` and `leaving` on `PATCH`; the stored cardinality moved by the page and published only with its operator; the whole re-derivation of an operator whose delta holds a leave; the empty-set floor and the withdrawal it reports; the tick's row-form publication for memberships and generating sets with the shared scratch; the per-record pin; permits and pool resolution for pages; the driver publishes over-cap artifacts and sets as pages and `declined` is empty on every rung | T2a |
| **T2c exclusion and view identity** (built 2026-09-08) | `excluding` under `max_excluded_per_request`; `view` in the identity on a group-scoped layer | T2a |
| **T4 attributes** | `PUT /control/attributes`; the manifest home; the schema-answered absence for a `render` column; the tail append and padding for a mid-ingest declaration; the fold's materialisation | T1 |
| **T3 values** (built 2026-09-08) | `POST /control/values`; the fill on the executor beside the join arm; the view header and key-in-group check for scoped columns; layer columns on existing entities; the record stack's per-column claimant read and its fold | T4 |
| **T5 vocabularies** | `PUT /control/vocabularies/{name}`; value pages; the property upsert (`/control/categories`'s debt) | T4's manifest home |
| **T6 groups and views** | `PUT /control/view_groups/{name}`; `PUT /control/views/{name}` | T4's manifest home |
| **T7 conformance** | the 0091 equivalence driver over every kind, **defined over served answers** (masked counts, artifact frames, drill-downs per principal), never over bundle bytes; write-path and annotation-write-cycle rewritten | alongside T2a onward |

The three no-route kinds are T4, T5 and T6; exclusion membership is T2c. T1 and T2b are what the
measurement campaign is waiting on (decision 0127's `declined` column); the rest close decision 0091.

## 9. Evidence

| Figure | Class | Source |
|---|---|---|
| ~15 B per base64 member; 4.4M members per 64 MiB page | measured | decision 0127, the driver's `_grow_slices` |
| flush 14.8 µs/row at 36M titles, 133.5 at 92M abstracts; window close 11.6 µs/row; analyser 116.5 µs per ~1,500-character abstract | measured | `probes/2026-09-05-flush-attribution/` |
| ~5 s of analyser for a 64 MiB text value | modelled from the per-character figure | spec §2.3 |
| zstd decompression 1.5 to 1.7 GB/s; ~40 ms for a 64 MiB block | measured rate; modelled time | records-and-search §7 |
| ingest 65,194 rows/s (MedCPT 10%), 11,060 (TreeOfLife 50%), 7,427 (PaperSeek) | measured | `docs/ingest-campaign.md`, the probe |
| build 20,400 rows/s on PaperSeek 92M | measured (75 min for 91,905,609 rows) | the probe |
| hours to 10⁹ rows per family | modelled from the per-row figures, base held constant | spec §4.1 |
| 12 MB bitmap per 10⁸-member artifact; one interval's pages pinned under roster-first loading that completes within the interval; up to the whole bitmap per artifact for growth spanning ticks; 10⁸ to 10⁹ B until the fold for a hierarchy grown onto packed records | modelled | artifacts-from-points §6.1; spec §2.4 |
| the complement of an exclusion list: one `andnot`, about 12 MB at 10⁸ entities | modelled | spec §2.3 |
| a re-derived operator after a leave: linear in the artifact's size | modelled | spec §4.1 |
| the tick's row-form cost: one union per `(artifact, view)` with a delta of joins; a whole re-derivation where a delta holds a leave | modelled from the existing growth path | spec §4.1 |
| cycle walk over 30,954 nodes and 42,287 edges | modelled from rung 3's counts | dag-hierarchies §8 |
| declaration cost ≈ one fsync, 3.2 ms quiescent | modelled by analogy | the deny ack baseline |
| values throughput between the two point figures | modelled | spec §4.1 |

## 10. Rulings made

Ruled by the owner on 2026-09-07 after the first review round; each is applied above and wants a
decision record at promotion. The two standing preferences govern all of them: simplicity over
cleverness, and minutes of ingest-to-publish latency accepted for large data.

| | Ruling | Applied at |
|---|---|---|
| 1 | JSON is the default encoding for every kind, coerced against the declared column types; Arrow optional by content type on the same routes; the driver keeps Arrow | spec §1.2 |
| 2 | every route publishes a record count beside its byte cap; points keep 10,000; memberships, generating sets and values take a count of the same shape | spec §2.1 |
| 3 | content with an empty declared set is not served (0107's rule re-made at the caller's door); joins before leaves within a page; the superset ordering across pages is the caller's | spec §1.1, §2.2 |
| 4 | exclusion memberships only under a published bound on the list; above it, the inclusion spelling; byte-identity claimed in a quiescent database with the two divergences named | spec §2.3 |
| 5 | the pin is per record: a growth above the high-water is released at the next tail pack, one below it at the fold | spec §2.4 |
| 6 | a level's row forms are published by the executor at the tick from the accumulated deltas; a request never builds one; a served form may lag the store by a tick | spec §1.3, §4.1, §6.1 |
| 7 | a fill on a flushed entity is read: the record stack reads a column from the one layer that claims it; T3 owns the change | spec §1.4, §6.3 |
| R1 | page sizes on `/control/status` | spec §2.1 |
| R2 | values is its own route | spec §1.4 |
| R3 | the fill rule replaces the held-key refusal, with the digest comparison and the partitioned `PUT` | spec §1.5 |
| R4 | lineage may be filled; the walk is over the layer by `(level, ordinal)`, held edges and the batch's adjacency as one graph | spec §1.5 |
| R5 | a supplied-content layer publishes without content and reports | spec §1.5 |
| R6 | an annotation on C7 stating the bound | spec §6.2 |
| R7 | memberships never shrink | spec §1.1 |
| R8 | the three per-object bounds stand and are published | spec §2.3 |
| R9 | a plain view has a create route, last in order | spec §1.3, §8 |
| R10 | a runtime `render` column reads absent until the fold, answered from the segment schema | spec §6.3 |
