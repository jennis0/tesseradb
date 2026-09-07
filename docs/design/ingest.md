# Ingest at any scale — design

**Date:** 2026-09-07
**Status:** **Provisional — under review.** Drafted under [decision 0134](../decisions/0134-anything-a-build-can-create-live-ingest-can-create-at-any-scale.md)
(every kind a build can create, live ingest can create and extend, at any scale) and
[decision 0135](../decisions/0135-a-generating-set-is-the-callers-claim-i8-withdrawn.md) (I8
withdrawn; a generating set is the caller's claim). Before it becomes normative: an independent
review under three lenses (security against §4 and Appendix C, performance against §4 of this
document, implementability against the write path as built), and the owner's ruling on §10's ten
questions, R1 to R10. Nothing in it is built except where a paragraph says so; the whole of §1's
model is specified here and marked ⊘ where it differs from the routes that exist.
**Reads against:** architecture §4, §11, Appendix C; write-path §1 to §4, §7; contracts §3.4;
annotation-write-cycle §1 to §6; artifacts-from-points §6; dag-hierarchies §4; views §3, §5, §7;
per-point-attributes §2.2, §5; configuration §1, §2, §9; decisions 0047, 0048, 0081, 0091, 0127,
0128, 0129, 0133, 0134, 0135.
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **spec §n**.

**Owns:** the model of live ingest for every kind of data: what a client sends, what the server
commits, when a viewer sees it, how a payload larger than one request travels, what a client must
handle, and which caps exist and what each one is. It is the design decision 0134 asks for.

**Does not own:** the invariants and the leak register (architecture §4, Appendix C); the byte-level
wire (contracts §3.4, amended by spec §7 when this is promoted); the commit window, the WAL and the
flush (write-path, which this document reads and does not restate); artifact semantics
(annotation-write-cycle, whose sentences resting on the withdrawn I8 spec §7.4 lists for rewrite).

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

The answer this document gives, in one paragraph. Every kind is ingested as a batch of records.
Applying a record is monotone: a subject that does not exist is created, an absent part is filled,
a set-valued part is joined or left, and a part already present must be identical. No kind needs a
session-like upload, because the only object whose parts had to be committed together was a
generating set with its content, and decision 0135 makes the set a part like any other. Two wire
encodings carry all of it: Arrow for row-shaped kinds and JSON for object-shaped kinds. Every cap
is a page size published beside the route that enforces it.

## 1. One model of ingest for every kind

### 1.1 The unit of ingest

A **record** names a subject by the key the caller chose and carries parts of it. The subject is a
point (by external id, or by `tessera_id` with its idset), an artifact (by layer, level and key), a
layer, a view, a view group, a vocabulary or one of its values, or an attribute column (each by
name). A **batch** is one request carrying records of one kind. The batch is the commit unit: it is
applied at one commit-window close with one fsync, whole or without effect, exactly as
`/control/ingest` and `/control/layers/{name}/artifacts` already are (write-path §2.3; contracts
§3.4). Nothing in a batch is acknowledged before it is durable, and nothing is durable that was
not acknowledged, so a client's retry rule is the same for every kind: an unanswered request is
resent with identical bytes.

**Applying a record is monotone.** Four cases, and they are the whole of what a write may do on
this plane:

| The part is | The record | Result |
|---|---|---|
| absent | supplies it | filled |
| present and identical | supplies it | accepted, no effect |
| present and different | supplies it | `409 conflict` naming the part; the batch has no effect |
| a set (membership, generating set, vocabulary values) | names entries joining or leaving | the set changes by that delta; an entry already in the state the record asks for is a no-op |

Removal of a subject is not on this plane. A point, an artifact or a layer leaves through
`/control/changes` and retires under write-path §5.4's two rules, as today. The one set that may
shrink is a generating set, because decision 0135 makes its contents the caller's claim and, since
its amendment, makes this plane the only place that claim can change: the service no longer edits
a set on the caller's behalf at the fold. A membership does not shrink (spec §10, R7), and a
vocabulary value is retired to `reserved`, never removed (per-point-attributes §2.2).

This is the rule the build already follows without saying so. A build reads a corpus once, so
every part is supplied exactly once and nothing is ever present and different. Ingest sees the same
corpus in an order the caller chooses, and the monotone rule is what makes the order irrelevant to
the result: two clients supplying the same parts in any interleaving reach the same database, which
is decision 0091's test restated for pages.

### 1.2 Two encodings, argued

| Encoding | Kinds | What it buys |
|---|---|---|
| **Arrow IPC** | points; attribute values for existing entities | typed columns checked against the manifest by name and type; ten-thousand-row batches decoded without per-value parsing; the shape the build's readers already take, so one column rule serves both doors (decisions 0091, 0129) |
| **JSON** | declarations (layer, view, view group, vocabulary, attribute); artifact records and their parts; vocabulary values; changes | nested records with optional parts; the shape `tessera check --payloads` already emits for a layer; readable in a log |

A third form was considered for membership pages, an Arrow column of ids, and declined: a page of
base64 external ids in JSON is about 15 bytes per member (measured on rung 3, 2026-09-05), so a
64 MiB page carries some 4.4 million, and a 10⁸-member artifact is 23 requests. The saving is not
worth a third decoder.

### 1.3 The kinds

The table is the model. `⊘` marks a unit that does not exist yet; every route without the mark is
built and is described in contracts §3.4.

| Kind | What a client sends (one record) | Route | Commit | Scale route | Visible | Idempotency |
|---|---|---|---|---|---|---|
| **Point** | id, coordinates, labels, attribute values, one key or list per layer column | `POST /control/ingest`, Arrow | window close | pages of ≤ `max_batch_rows` and ≤ `max_batch_bytes` | at its flush | batch id + body hash within the WAL retention window; a supplied external id refuses a duplicate (`409`) |
| **Attribute values for an existing entity** ⊘ | id, attribute columns, layer columns; no coordinates | `POST /control/values`, Arrow (spec §1.4) | window close | pages, as points | index and filter at the flush; `render` at the fold (spec §6.3) | batch id + hash; the fill rule per cell |
| **Membership** | the point's layer column, or a page of `members` per artifact | ingest, values, or `PATCH /control/layers/{name}/artifacts` | window close; an `ArtifactGrow` record in the window's fsync | pages; each a delta | at the ack for an entity that has a row; a buffered entity contributes at its flush | set join: a retry adds nothing (`joined: 0`) |
| **Generating set of content *k*** ⊘ | a page of members at `rank: k`, `joining` and `leaving`; `leaving` resolves a deleted item, which is its purpose (spec §1.5) | the same `PATCH` | window close | pages; the only route by which a set changes | at the ack: containment is evaluated live (I3) | set join and leave; a retry is a no-op |
| **Artifact record** | key, `parent`, `attached_to`, shape, content values; a first page of members and generating sets; `view` on a group-scoped layer ⊘ | `PUT /control/layers/{name}/artifacts` creates; `PATCH` fills ⊘ | window close; ordinals claimed contiguously | pages over artifacts; each fixed part is filled once | at the ack; content when its declared set is contained | a held key with identical parts is accepted with no effect ⊘ (spec §10, R3) |
| **Membership by exclusion** ⊘ | `excluding`: the entities the membership leaves out | `PUT` or `PATCH`, on the artifact record | window close: the complement is taken at the close against the view's entity set | one request; over the cap, the inclusion spelling (spec §2.3) | at the ack | a second `excluding` on a held key is `409` |
| **Layer** | the `[[layer]]` block minus acquisition keys | `PUT /control/layers` | one fsync | one request; a declaration is kilobytes | at the ack | identical redeclaration answers the existing identity ⊘ (R3) |
| **View of a group** | the roster record | `PUT /control/views/{group}/{key}` | one fsync | one request | in `/v1/meta` at the ack; a row space at its first flush | as above |
| **View group** ⊘ | the `[[view_group]]` block minus roster and source | `PUT /control/view_groups/{name}` | one fsync | one request; its views follow one by one | at the ack | as above |
| **Plain view** ⊘ | the `[[view]]` block minus source | `PUT /control/views/{name}` (spec §10, R9) | one fsync | one request | at the ack | as above |
| **Vocabulary** ⊘ | the `[[vocabulary]]` block minus source; values inline where they fit | `PUT /control/vocabularies/{name}` | one fsync | the declaration, then pages of values | at the ack; a value usable by the next batch | as above |
| **Vocabulary values** ⊘ | a page of `{key, code?, title?, …}` | `PATCH /control/vocabularies/{name}/values` | window close; one `VocabularyMint` per value | pages | at the ack | append-only; an identical value is a no-op; a different property on a held key is an upsert of properties, never of key or code |
| **Attribute column** ⊘ | the `[[attribute]]` block minus acquisition keys | `PUT /control/attributes` | one fsync | one request | ingestable at the ack; absent for every existing entity until filled | as above |
| **Suppress, delete, unsuppress** | `{id, op}` | `POST /control/changes` | deny window | pages of about 10⁴ | at the ack | each op idempotent |

A discovered category value is not a kind: it is minted at the window close of the batch that
carries it, by the view-first rule (per-point-attributes §5), and this document changes nothing
there. A declared category value is a vocabulary value above.

### 1.4 The values kind

A build reads attribute files by entity id and joins them to points that were read from another
file (artifacts-from-points §7: joins are reported, not refused). Ingest has no such join today: a
value travels only on the row that creates its entity, and a joining row's values are compared and
then discarded (contracts §3.4, r65). So a column declared over an existing corpus cannot be
filled, which is the third of the owner's three cases and a reach gap under decisions 0091 and
0134.

`POST /control/values` ⊘ takes an Arrow batch of `(external_id | tessera_id, …attribute columns,
…layer columns)` over entities that exist. Per cell it applies the fill rule: an absent cell takes
the value; a cell holding the same value is a no-op; a cell holding a different value is a `409`
naming the column and the id, and the batch has no effect. That is the comparison the join arm
already makes on the executor (contracts §3.4 r68), with the write the join arm withholds. A layer
column on a values row is a membership join for an existing entity, through the same
`ArtifactGrow` the point route uses. A `text` cell that has flushed is compared against the record
blob, where the prose is stored whole; ⊘ the r68 refusal of any second string stays until that
comparison is built, and a client retrying a text back-fill past the WAL retention window meets
`409` for rows that landed and should read it as done.

Why a route and not a mode of `/control/ingest`: a points batch allocates entities and needs a
view; a values batch allocates nothing and names no view unless it carries group-scoped columns.
Making the difference a header would put the decision inside the one handler whose memory
argument is the tightest (spec §2.1). Elasticsearch makes the same distinction as an explicit
`update` action beside `index` (spec §3).

### 1.5 The artifact and its parts

An artifact has one identity, `(layer, level, key)`, and parts of three sorts. **Fixed parts** are
filled once: `parent`, `attached_to`, the shape, each content's values, and `view` on a
group-scoped layer. **Set parts** change by delta: the membership (rank null) and each content's
generating set (rank *k*), which is the member table's own grain at the build (`(key, entity,
rank)`; annotation-write-cycle §6.1). **Computed parts** are derived per viewer and are never sent.

`PUT` creates the record with whatever parts the caller has, including a first page of each set.
`PATCH` ⊘ fills a fixed part that is absent and applies set deltas: per artifact `{key, members?,
leaving?, rank?, parent?, attached_to?, content?, shape?}`. Today `PATCH` carries members and
nothing else (decision 0127; `GrowBody` refuses every other field). The extension is what
decision 0091 requires for enrichment: a build's `artifacts` table is enrichment over artifacts the
points minted (artifacts-from-points §3), and at ingest an artifact minted from a column can
receive no name, parent or content, because `PUT` refuses a held key and `PATCH` carries no part.

Three consequences of the parts model, each a change to a rule that exists:

- **Lineage may be filled on an artifact that holds none** (spec §10, R4). The cycle check today is
  publication-scoped because a growth never adds an edge (dag-hierarchies §4). A fill of `parent`
  on a held key adds edges into an existing graph, so the check becomes a walk from each named
  parent through the level's held parent lists, refusing if it reaches the child. The walk is over
  artifacts, not members; rung 3's level holds 30,954 nodes and 42,287 edges, so it is
  microseconds. It runs on the executor at the close, serially, so no two requests can each pass
  and together close a cycle.
- **A generating set may precede its content.** Decision 0135 lets a set change at any time; what
  the order buys is that content is never served against a partial set. A caller supplying a set
  larger than one page sends the pages, then the content. Content supplied first is lawful and
  is served, while the pages arrive, to every principal who can see the part of the set declared so
  far, which is C12's shape and spec §6.2's subject.
- **A layer declaring supplied content publishes an artifact without it, and reports the count**
  (spec §10, R5). The refusal that stands today (artifacts-from-points §3) forces the content onto
  the `PUT`, and with it the first page of the set, which is exactly the widening window above.
  An artifact without its declared content is served without content, indistinguishable from one
  whose content is withheld, which discloses nothing; it is a fidelity signal, and the `201` carries
  `without_content: n` so a pipeline sees it.

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
  after which containment can pass again and the content is served at the page's ack to whoever
  satisfies the set that remains;
- **after the fold**, the content is absent, so the caller fills it again, with a set that omits
  the item, in as many pages as it needs.

A withdrawn content is **absent for the fill rule**: between the deletion's acknowledgement and the
fold it is served to nobody, and a `PATCH` carrying content *k* with a new set replaces both in one
request. That is the one place a fixed part may be supplied twice, and it is narrow by
construction: nothing it replaces was being served. Replacing a served content stays out of this
design (decision 0077's deferred edit pass). `withdraw_on_member_deletion` is untouched: it
withdraws the artifact, where this withdraws a content.

### 1.6 Ordering a client must keep

The monotone rule makes most orderings free. What remains is that a record cannot name a subject
that does not exist, resolved once at the boundary and never later (I10):

1. an entity before any member list, generating set or values row naming it;
2. a vocabulary before a declared category value naming one of its keys; an attribute before a
   values column for it; a layer before its artifacts; a group before its views;
3. a parent before its child, or in the same batch (dag-hierarchies §4);
4. the pages of a generating set before the content served against it, if the caller wants no
   principal to read the content against a partial set.

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
one of its caps. What it changes is the classification: each is a **page size**, and a payload
larger than one page is several pages, each complete in itself.

| Cap | Default | Page of | Published as |
|---|---|---|---|
| `ingest_max_batch_rows` | 10,000 | points, values | `limits.ingest.max_batch_rows` |
| `ingest_max_batch_bytes` | 16 MiB, ceiling 64 MiB | points, values | `limits.ingest.max_batch_bytes` |
| `publish_max_body_bytes` ⊘ (today the constant `PUBLISH_MAX_BODY_BYTES`) | 64 MiB, ceiling 64 MiB | artifact records, set pages, vocabulary values | `limits.publish.max_body_bytes` |
| `CHANGES_MAX_BODY_BYTES` | 2 MiB | changes | `limits.changes.max_body_bytes` |
| declarations | 2 MiB (axum's default) | one declaration | `limits.declaration.max_body_bytes` |
| `max_shape_vertices` | 10⁶ | one shape | `limits.publish.max_shape_vertices` |

Where the block is published is R1: on `/control/status`, which the driver already reads
`ingest.max_batch_bytes` from, or on `/v1/meta` as decision 0134's text says. The recommendation
is the control plane: every writer holds the operator credential, a viewer has no use for a page
size, and the ingest handler's own rule is that an unauthenticated caller learns nothing about the
configured cap from a status code. A client reads the block once and sizes every page from it; a
page over the cap is a `422` naming the limit and the field, never a truncation.

### 2.2 Why no kind needs a multi-part upload

A multi-part upload exists to commit, in one moment, something that could not be sent in one
request. Three candidates were examined and none survives.

**Memberships and generating sets** are sets, and a set assembled by pages is the set, whatever
the order or the timing of the pages. Decision 0127 settled this for memberships; decision 0135
extends it to generating sets by removing the rule that made their assembly need a single moment.
Under decision 0135 a set is also *replaceable* without a session: send the entries joining first,
then the entries leaving. Every intermediate set is a superset of both the old and the new, so at
no point is the content served to a principal who could see neither, which is the one property a
one-shot replace would have bought.

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

Three per-object bounds survive, and the argument for each is that the object has a smaller
spelling or is already bounded elsewhere.

- **An exclusion list is one request.** The complement is taken at the window close against the
  view's entity set as of that close (every entity holding a row in the view or buffered for it,
  deleted entities excluded), so the list cannot arrive in pages. An exclusion list over the cap is
  spelled as an inclusion and grown; the build's rule that the two spellings are byte-identical in
  the bundle (annotation-write-cycle §6.1) holds at ingest by construction, since the complement
  is materialised before the record is written. R8.
- **A single value is one batch.** A text cell, a keyword, a number arrive on one row, and a row
  cannot page. The bound is `max_batch_bytes`, 64 MiB at the ceiling. The analyser and the record
  blob each hold a value whole, and the blob's block target is 256 KiB, so a value near the bound
  is three orders past what the storage was shaped for. Accepted and published. R8.
- **A shape is `max_shape_vertices`.** A polygon has a lossless smaller spelling only in the sense
  that `ST_Simplify` keeps what a map at any served zoom can show (artifact-shapes: the vertex
  budget is a wire guard, not a fidelity control), and the served hull is itself vertex-budgeted.
  Accepted and published. R8.

### 2.4 The WAL pin, with numbers

A growth record is pinned in the log until the fold that rewrites the level whole
(artifacts-from-points §6.1). Under this design every set page is such a record. A portable
Roaring bitmap over a 10⁸-member artifact is about 12 MB (artifacts-from-points §6.1, modelled);
rung 3's closed hierarchy is 1.66×10⁹ membership entries over 30,217 artifacts, so loading it
through pages pins on the order of 10⁸ to 10⁹ bytes of bitmaps until the fold (modelled from the
per-member figure; not measured). The log has no runtime ceiling (write-path §1.3) and replays
every pinned record at restart. That is the existing cost of decision 0127 at the scale decision
0134 asks for, and the mitigation is the one the fold already offers: an operator loading a
hierarchy folds after it. Packing growth at the flush instead is the second packing rule
artifacts-from-points §6.1 declined, and this design does not reopen it; if the pinned volume
ever surprises, that is where the fix lives.

## 3. Comparable systems, and the three cases as a client sees them

### 3.1 What each does for exactly this problem

| System | Unit | Size | Atomicity | Idempotency | Large object | Schema growth |
|---|---|---|---|---|---|---|
| Elasticsearch `_bulk` | NDJSON of actions | `http.max_content_length` 100 MB; clients chunk at 500 docs | per item, never per request; partial acceptance with a per-item status | `_id` with `create` (409 on duplicate); `if_seq_no` | none; a document is one line | `PUT _mapping` adds fields; a type change is a reindex behind an alias |
| PostgreSQL `COPY` | a stream of rows | unbounded; streamed to WAL and heap | one transaction, all or nothing | none in `COPY`; `ON CONFLICT` on `INSERT` | a large value is one row (TOAST) | `ALTER TABLE ADD COLUMN` is metadata; a back-fill is `UPDATE` in batches |
| S3 multipart | parts of 5 MiB to 5 GiB, up to 10,000 | 5 TiB | `CompleteMultipartUpload` is atomic; parts are invisible until it | a part is idempotent by number; the complete call by upload id | this is the mechanism | none |
| Kafka | producer batches per partition | `max.request.size` 1 MB default | per partition; transactions across partitions with `read_committed` | producer id + sequence | none; a record is bounded | none |
| Pinecone / Qdrant / Weaviate | upsert of vectors by id | 2 MB / 1000 vectors (Pinecone); 32 MB (Qdrant's default payload) | per request (Qdrant `wait`), per object (Weaviate batch errors) | upsert by id | none | fields are free-form |
| Snowflake, BigQuery | a staged file, then a load job | files in a durable stage | one job | job id | staging is the mechanism | `ADD COLUMN` |

Two families. Row stores and search engines page and upsert by id, and a client library hides the
page size behind an iterator (`helpers.bulk`, `copy.write_row`, `index.upsert(batch)`). Object
stores and warehouses stage and commit, because their object has no addressable parts after
commit. Tessera's subjects all have addressable parts, so it belongs to the first family, and the
one place it differs from Elasticsearch is deliberate: **a request is whole or without effect**,
never partially accepted, because entity ids and ordinals are allocated contiguously and a
refusal spends nothing (write-path §2.3). A client author therefore needs one retry rule and no
per-item status parsing.

What a client library author expects, from that evidence: a page size they can read rather than
guess; an iterator that splits a table into pages; `429` with `Retry-After` and a bounded retry;
an idempotency key per page so a lost acknowledgement is safe to resend; upsert-by-id semantics
so a re-run of a pipeline is a no-op; and a schema call that is safe to repeat. Every one of those
is in spec §1 and spec §4.

### 3.2 The three hardest cases, as SDK calls

Python, against the SDK in `clients/py`. `paginate` reads `limits` once and yields pages under both
the row and the byte unit; every call retries a `429` after its `Retry-After` and resends identical
bytes on a lost acknowledgement.

**A 10⁹-row corpus streamed in.**

```python
w = tessera.Writer(url, credential)                 # reads /control/status limits once
for i, page in enumerate(w.paginate(rows, kind="points")):
    w.ingest(view="s0", page, batch_id=f"{run}-{i}")   # 200 {accepted, tessera_ids, minted, …}
w.flush()                                            # optional; the tick flushes anyway
```

Each page is one request, one commit-window entry, one durability receipt. Visibility is the
flush. At the measured rates (spec §4.1) the loop is bounded by the pool's flush of the widest
column family, not by the client.

**An artifact with 10⁸ members, content, and a generating set of 10⁷.**

```python
a = w.layer("topics/openalex").artifact(key="3", parent=["root"])
a.publish(members=first_page)                        # PUT: the record, ordinal claimed
for page in w.paginate(members, kind="ids"):
    a.join(page)                                     # PATCH members: 23 pages at 4.4M each
for page in w.paginate(generating, kind="ids"):
    a.join(page, rank=0)                             # PATCH rank 0: the set, before the content
a.fill(content=[{"rank": 0, "values": ["Machine learning"]}])   # PATCH: content, served from here
```

Five calls in the SDK's own vocabulary; 27 requests on the wire; no token, no session, no
commit call. A re-run of the loop is a no-op at every step. The artifact is served from the `PUT`
with the members it has; its content from the last call, contained against the whole set.

**A new attribute column declared and back-filled over an existing corpus.**

```python
w.declare_attribute({"name": "sentiment", "type": "float32", "index": True, "render": True})
for i, page in enumerate(w.paginate(scores, kind="values")):    # columns: external_id, sentiment
    w.values(page, batch_id=f"sentiment-{i}")
```

The declaration is one request and is safe to repeat. The column is absent for every entity until
its page lands; a filter on it matches the filled rows at their flush; the rendered value reaches
the row tail at the fold (spec §6.3).

## 4. Performance

### 4.1 The throughput model, per kind

The executor is one thread per partition and the flush runs one plan at a time on the pool
(write-path §1.1, §4.2). Sustained ingest is therefore bounded by the slower of the window close and
the flush, per row, and the flush is the term at every scale measured:

| Kind | Per-unit cost | Class | Bound |
|---|---|---|---|
| points, titles | window close 11.6 µs/row; flush 14.8 µs/row, 69% the text index | measured, MedCPT 36M, 2026-09-05 | 50,090 to 65,194 rows/s measured; 10⁹ rows in about 4.5 h, modelled from the per-row figure with the base held constant |
| points, abstracts | flush 133.5 µs/row, 90% the analyser | measured, PaperSeek 92M | 7,427 rows/s measured against a 7,490 ceiling; 10⁹ rows in about 37 h, modelled |
| values | the flush cost of the columns carried, without the segment, permutation and allocation stages | modelled from the stage table (`segment`, `rows`, `promote` are 1.5 to 1.9 µs/row) | between the two rows above, by family |
| membership and generating-set pages | per member: one base64 decode and one resolution against the external-id sidecar runs, then a bitmap insert on the executor | modelled; the driver records `members_per_s` per layer and the figure is read there | the resolution's binary search over the runs; the record is one bitmap per `(layer, level)` |
| artifact records | per artifact: key and parent resolution, the cycle walk, a content string | modelled; kilobytes each | negligible against the pages |
| declarations | one WAL append and one fsync | modelled by analogy with the deny ack's 3.2 ms quiescent | one per declaration |

Two things the model says that a loader should know. **The base grows under a long load**, and the
stages that scale with it (`plan`, `compose`, `drop_superseded`) rose 3× to 10× per row between
36M and 92M; the hour figures above hold the base constant and are lower bounds. **The analyser is
the term**, and the two answers to it, a faster analyser or the text index off the flush's critical
path, are the probe's open question and not this document's; nothing here adds threading
(owner direction, 2026-09-02).

### 4.2 Where backpressure lands

| Producer | Kinds | Answer |
|---|---|---|
| admission semaphore, `ingest_admission` 64 | points, values | `429`, `Retry-After` from the observed service rate, 1 to 300 s |
| command queue, `ingest_queue_bound` 32 | points, values, artifact records, pages, declarations | `429`, drain-derived `Retry-After` |
| buffer occupancy, `ingest_buffer_max_items` 10⁶ rows | points, values | `429`, `Retry-After: 90`, the tick period |
| the deny lane | changes | never refused for load |

Artifact records and set pages reach the executor as commands and meet the queue bound; they hold
no rows and never meet the buffer bound. ⊘ Whether they should share the admission semaphore,
which today bounds ingest handlers only, is an implementation question for spec §8's T2; the
memory argument (spec §2.1) says they should, since a page is a buffered body like any other.

### 4.3 What a client must handle

Every kind, one table:

| Answer | Meaning | Client action |
|---|---|---|
| `200` / `201` | durable; visible per spec §1.3's column | next page |
| `409 conflict` | a duplicate external id, a batch id replayed with different bytes, or a part present and different | stop; the request is wrong, not the timing |
| `422 contract` | malformed body, an unknown or mistyped column, a page over a limit (naming it), an unresolvable subject named at position *p* | fix and resend; a page over a limit is re-split |
| `429 backpressure` | one of the producers above | wait `Retry-After`, resend identical bytes |
| `500 fail-closed` | durability failed and nothing applied, or the receipt was lost after the swap | resend identical bytes; idempotency resolves which |
| `503 not-ready` | executor not running or a stepped-down partition | retry later |

There is no partial acceptance and no per-item status: a page is whole or without effect.

### 4.4 Idempotency, per kind

Batch id and body hash answer a replay of an ingest or values page within the WAL retention
window (write-path §2.4); past it, a supplied external id still refuses a duplicate point, and a
values page is answered by the fill rule. For every other kind the record itself is the key and
the monotone rule is the idempotency: a set join adds nothing the second time, a fixed part is
identical and accepted, a declaration is identical and answers the existing identity. A client
therefore needs a batch id only where the wire allocates, which is points, and may carry one on
values for the receipt. The one place a retry reads as a conflict is a flushed text cell before
spec §1.4's blob comparison is built, and the client rule there is stated beside it.

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

### 6.1 The visibility moments

| Moment | What a principal can observe | Row |
|---|---|---|
| a page of points acknowledged, not flushed | nothing in any map verb; `x-tessera-stale: 1` on the next response (C15) | none new |
| a page of values acknowledged | nothing until the flush; then the filled rows match a filter on the column, inside the mask | none new: an ingest in progress |
| a membership page acknowledged | the artifact's masked count over the members that have rows, from the ack (decision 0127) | none new |
| a generating-set page joining, under a served content | the content stops being served to a principal who satisfied the smaller set and does not satisfy the larger; they learn an item outside their mask was added to a set they could read against | **new, R6** |
| a generating-set page leaving | the content is served to principals who satisfy the smaller set; content derived from an item a principal cannot see reaches them | C7, as amended by decision 0135: the declared set, whatever the caller did to it |
| a member of a generating set deleted | the content vanishes for everyone at the deny's ack; a principal who satisfied the set learns a member was denied | C17's annotation: bounded to principals who could see every member |
| the fold withdraws a content, or the caller repairs the set before it | the content is absent, or reappears at the repair page's ack to whoever satisfies the repaired set | C7: the declared set; the reappearance is the caller's re-declaration, and C7's second channel (reappearance by the service's own removal under a permissive layer) no longer exists |
| content filled after its set | served at the ack, to principals who satisfy the whole declared set | none new |
| content filled before its set is complete | served, while the pages arrive, to principals who satisfy the partial set | C12: the caller's optimistic set, trusted as declared |
| an artifact on a supplied-content layer without content | served without content, indistinguishable from content withheld | none: withholding is already the served state |
| an attribute declared, unfilled | absent everywhere; `/v1/meta` lists the column | none |
| an attribute half filled | as an ingest in progress on that column | none |
| a vocabulary value declared, unused | listed under a `public` vocabulary; under `derived`, invisible until a visible member carries it (C11) | none |

### 6.2 The register

**No row is added for pagination.** Every intermediate state is a state some order of arrivals
could have produced under the routes that exist, and Appendix C's inclusion test asks what a viewer
learns about data they were not served: a page adds nothing to that.

**One annotation is proposed, R6.** Growth of a generating set under a served content withdraws
the content from a principal who could read it, and the withdrawal says that an item outside their
mask joined the set. It is bounded to content the principal was served, it is the caller's action,
and it is the same class as C17's delete signal on items the principal already sees. Under
decision 0135 it is what C7 now says read in the other direction, so the recommendation is an
annotation on C7 rather than a row: *a change to the declared set in either direction is
observable by a principal on one side of the containment boundary, as a served content appearing
or vanishing; what it says is that the caller changed their claim.* C7's second channel, a
content reappearing at a fold because the service removed a deleted member from the set, is gone
with the permissive mode (decision 0135's amendment); a reappearance is now always a page the
caller sent.

**I2, I3, I13.** Every count and containment test is computed inside the composed mask as today;
a generating set's row-space operator is re-derived from the entity truth at every change, which
is the maintenance the membership operator already has (annotation-write-cycle §2's three arms,
with a fourth: the delta at the ack). No cache holds a containment verdict above the test (I3). A
page refused yields no partial state, and a page accepted is whole (I13a). Nothing here touches
I12 or the filter mask.

### 6.3 A runtime attribute's three homes

A column's home is the hot column in the row tail, its family's entity-space structure, or the
record blob (records-and-search §3). A column declared at runtime has no base in any of them.
Entity-space extents are written per flush over the flushed entities' presence, so a values page
lands as an extent whose presence covers the filled entities, disjoint from every other extent for
that column because the fill rule leaves one claimant per cell. The record blob likewise gains an
extent. The row tail is in the segments, in row space, and a segment written before the
declaration does not carry the column: ⊘ the reader takes absence for a `render` column a
segment's schema lacks, and the fold, which rewrites every segment, writes it. Until the fold a
back-filled `render` value is filterable where `index` was declared and not drawn. R10 rules
whether that interval is acceptable or a `render` column is declarable only for the fold to
materialise.

## 7. Migration and contracts

### 7.1 On the wire

| Change | Kind | Where |
|---|---|---|
| `limits` block; `publish_max_body_bytes` a config key | new field; new key | `/control/status` (R1); configuration §9's table gains the `[ingest]` keys it defers today |
| `PATCH /control/layers/{name}/artifacts` carries `rank`, `leaving`, `parent`, `attached_to`, `content`, `shape` | widened body | contracts §3.4, the r77 row |
| `PUT` accepts a held key with identical parts; a different part is `409`; `without_content` on the `201` | changed rule | contracts §3.4, the `PUT` row; the sentence "published with as many members as fit" is replaced by the page rule |
| `view` and `excluding` on an artifact record | new fields | contracts §3.4 |
| `POST /control/values` | new route | contracts §3.4 |
| `PUT /control/attributes`, `PUT /control/vocabularies/{name}`, `PATCH …/values`, `PUT /control/view_groups/{name}`, `PUT /control/views/{name}` | new routes | contracts §3.4; `CONTROL_PLANE_ROUTES` |
| redeclaration answers the existing identity | changed rule | the layer and view rows |

`api_version` stays at 1: no client outside this repository exists and decision 0048 rules the
change made rather than versioned. `bundle_format` moves from 6 to 7, because the manifest is the
durable home of every declaration that survives WAL rotation (contracts §2.2's `groups` row makes
the argument for rosters) and gains runtime-declared attributes, vocabularies and groups.
`WAL_VERSION` moves from 19: `ArtifactGrow` gains a rank and a leaving set, `ArtifactPublish` a
view and a fill form, and the log gains `ValuesBatch`, `AttributeDeclare`, `VocabularyDeclare` and
`ViewGroupCreate`. Postcard is positional, so each is a bump and a stale log is refused rather
than read.

### 7.2 Documents this amends when promoted

- **write-path.md** gains the artifact records in §1.3's list, the set page in §2.3's close, the
  pin in §4.5, and the publish cap in §10 (the memo's contradiction 4).
- **contracts.md §3.4** as spec §7.1; the truncation sentence corrected (contradiction 3).
- **artifacts-from-points.md §9**: predicate membership is built and served (contradiction 1);
  §8's asymmetry (a growth never mints) stands.
- **per-point-attributes.md §2.2, §5, §6**: the vocabulary upsert exists once spec §8's T5 lands
  (contradiction 2); until then §6's "still owed" is the true line.
- **dag-hierarchies.md §4**: the cycle check becomes graph-scoped with late lineage
  (contradiction 5).
- **configuration.md §9**: names the `[ingest]` keys (contradiction 6).
- **`scripts/campaign_report.py`**: the layer column exists (contradiction 7).
- **decision 0091's ⊘** and **annotation-write-cycle §3.4**'s closure sentence: closed by this
  design for every kind, and not before (contradiction 8).
- **`config.rs`'s "belongs with flush"** (contradiction 9): streaming the upload is declined here
  (spec §2.2) and the comment should say so.

### 7.3 Durability claim this rests on

`ArtifactGrow` records ride the window's own fsync beside the `IngestBatch` records
(artifacts-from-points §6.2). The memo could not confirm it in `write-path.md`, which does not
mention the record. The design assumes it; the review should check it, since spec §1.3's "at the
ack" column for memberships depends on it.

### 7.4 The rewrite decision 0135 leaves in annotation-write-cycle.md

Marked at its head; rewritten with this design. The sentences: §2's "never grown (I8)", "`G`
changes on exactly one event, and never by growing" and "shrunk only by the fold on a permissive
layer"; §2.1's strict and permissive modes; §3.1's and §3.4's rows for a generating set under
deletion; §5's edit row where it says a `G` edit needs a content edit; §11's I8 items; every
citation of decision 0107. What replaces them is spec §1.5 and spec §6.1: a generating set is a
set part changed only by the caller's pages, its row-space operator is maintained by the fourth
arm, containment is live against the declared set, a deletion inside a set withholds the content
from the deny's ack and the fold withdraws and reports it, and the caller repairs by a page or a
refill. Contracts §3.4's `PUT` row loses "one requiring every member visible refuses an empty
generating set (decision 0107)" with the decision.

## 8. Order of work

Each track is one implementer's, in a worktree, with a referee before merge. The order is by
dependency and by how much of the campaign each unblocks.

| Track | Delivers | Depends on |
|---|---|---|
| **T1 caps** | `publish_max_body_bytes` as a key; the `limits` block; the `PUT` row corrected; the driver reads every limit it uses from the block | R1 |
| **T2 artifact parts** | `PATCH` with rank, leaving and the fixed parts; the fill rule and dedupe on `PUT`; late lineage with the graph-scoped cycle walk; `without_content`; `view` per record; `excluding`; the WAL bump; the driver publishes over-cap artifacts and generating sets as pages and `declined` is empty on every rung | T1; R3, R4, R5, R8 |
| **T3 values** | `POST /control/values`; the fill on the executor beside the join arm; layer columns on existing entities; the text comparison against the blob | T2's WAL bump; R2 |
| **T4 attributes** | `PUT /control/attributes`; the manifest home; the reader's absence rule for a `render` column; the fold's materialisation; `bundle_format` 7 | T3; R10 |
| **T5 vocabularies** | `PUT /control/vocabularies/{name}`; value pages; the property upsert (`/control/categories`'s debt) | T4's manifest home |
| **T6 groups and views** | `PUT /control/view_groups/{name}`; a plain view if R9 says so | T4's manifest home; R9 |
| **T7 conformance** | the 0091 equivalence driver over every kind: each spelling driven both ways and the two bundles compared; write-path and annotation-write-cycle rewritten | alongside T2 onward |

The three no-route kinds are T4, T5 and T6; exclusion membership is in T2. T1 and T2 are what the
measurement campaign is waiting on (decision 0127's `declined` column); the rest close decision 0091.

## 9. Evidence

| Figure | Class | Source |
|---|---|---|
| ~15 B per base64 member; 4.4M members per 64 MiB page | measured | decision 0127, the driver's `_grow_slices` |
| flush 14.8 µs/row at 36M titles, 133.5 at 92M abstracts; window close 11.6 µs/row | measured | `probes/2026-09-05-flush-attribution/` |
| ingest 65,194 rows/s (MedCPT 10%), 11,060 (TreeOfLife 50%), 7,427 (PaperSeek) | measured | `docs/ingest-campaign.md`, the probe |
| build 20,400 rows/s on PaperSeek 92M | measured (75 min for 91,905,609 rows) | the probe |
| hours to 10⁹ rows per family | modelled from the per-row figures, base held constant | spec §4.1 |
| 12 MB bitmap per 10⁸-member artifact; 10⁸ to 10⁹ B pinned for rung 3's closure | modelled | artifacts-from-points §6.1; spec §2.4 |
| cycle walk over 30,954 nodes and 42,287 edges | modelled from rung 3's counts | dag-hierarchies §8 |
| declaration cost ≈ one fsync, 3.2 ms quiescent | modelled by analogy | the deny ack baseline |
| values throughput between the two point figures | modelled | spec §4.1 |
| a text value near 64 MiB is three orders past the blob's block target | modelled | records-and-search §3 |

## 10. What needs ruling

Each is rulable from this document. The recommendation is the drafter's.

**R1. Where the page sizes are published.** (a) A `limits` block on `/control/status`, read by
every writer, which holds the operator credential. (b) On `/v1/meta`, as decision 0134's text says,
beside the three shape caps already there. Recommend (a): a viewer has no use for a page size, and
the ingest handler's rule that an unauthenticated caller learns nothing of the cap is kept without
an exception. Costs if wrong: one field moves.

**R2. The values kind is its own route.** (a) `POST /control/values`, Arrow, over existing entities,
with the fill rule. (b) A mode of `/control/ingest` selected by the absence of coordinate columns.
Recommend (a): allocation and the view are the ingest handler's whole contract, and Elasticsearch's
`update` beside `index` is the precedent a client author knows. Costs if wrong: one route folds into
another.

**R3. The fill rule replaces the held-key refusal.** A `PUT` naming a key the level holds is
accepted when every part it carries is identical to the part held, and is `409` naming the first
part that differs; a `PATCH` fills an absent fixed part and refuses a present one that differs; a
declaration re-sent identically answers the existing identity. Recommend yes: it is what makes
every client call safe to repeat, and no second artifact is ever minted, so the guard that closed
the suppression-by-republication path (artifacts-from-points §6.3) is untouched. Costs if wrong: a
pipeline's re-run refuses instead of no-op.

**R4. Lineage may be filled on an artifact that holds none.** The cycle check walks the level's
held parent lists from each named parent and refuses if it reaches the child, on the executor at
the close. Recommend yes: decision 0091 requires it, since a build enriches minted artifacts with
parents from the table, and the walk is over tens of thousands of nodes. Costs if wrong: an
artifact minted from a column can never join a hierarchy at ingest.

**R5. A layer declaring supplied content publishes an artifact without it and reports the count.**
(a) Relax the refusal to `without_content: n` on the `201`. (b) Keep the refusal; a caller with a
set larger than a page accepts that the content is served against the partial set while the pages
arrive. (c) A `hidden` flag on the record, the artifact served from the request that clears it.
Recommend (a): it discloses nothing, it lets the set precede the content, and it needs no state.
(c) is S3's complete call under another name. Costs if wrong: a viewer sees an artifact whose label
has not arrived, for the interval between two requests.

**R6. The register.** Growth of a generating set under a served content withdraws the content from
a principal who could read it, and the withdrawal signals that an item outside their mask joined
the set. (a) An annotation on C7, worded as spec §6.2 gives it. (b) A new row, Low, accepted as the
caller's action. Recommend (a): decision 0135 already makes C7 say the declared set is what is
served against; this is the same fact read from the other side. Costs if wrong: a row is added
later.

**R7. Memberships do not shrink.** A member leaves an artifact only when the artifact is replaced
(decision 0081) or the member is deleted. Decision 0135 lets a generating set shrink because it is
a derivation claim and the caller's page is now the only way a set is ever repaired; a membership
is what an artifact is, and its count is what its existence criterion tests. Recommend: ruled out explicitly, here, so the `leaving` field is refused at rank
null. Costs if wrong: a clustering re-run that moves points between clusters is a replacement, as
today.

**R8. The three per-object bounds stand and are published.** An exclusion list is one request
(spell a large one as an inclusion); a single value is one batch; a shape is `max_shape_vertices`.
Recommend yes, with the arguments in spec §2.3. Costs if wrong: a value-continuation form or a
staged complement, both of which are the multipart machinery spec §2.2 declines.

**R9. A plain view at runtime.** views §7 rules that no plain view is added after the build. (a)
Keep it: a deployment that ingests everything builds an empty database from a declaration that
names its views, and a new whole-corpus embedding is a rebuild. (b) `PUT /control/views/{name}`,
the `[[view]]` block minus source, an empty row space until its first flush, which is what a
group's view already gets. Recommend (b), last in the order of work: decision 0134 names views
among the kinds, and the create is the group's create with no group. Costs if wrong: one route.

**R10. A `render` column declared at runtime reads absent until the fold.** (a) Accept the
interval: filterable at the flush where indexed, drawn after the fold. (b) A runtime declaration
may not set `render`; the fold-time rewrite is a separate operation. Recommend (a): per-point
attributes §2.2 already prices `render` as a rewrite of every segment, and the fold is that
rewrite. Costs if wrong: an operator sees a column that filters and does not colour for up to one
fold interval, and `/v1/meta` should say which state the column is in.
