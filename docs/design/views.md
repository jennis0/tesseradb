# Views — design

**Date:** 2026-08-30
**Status:** Provisional r5 — under review and **not approved**. The rest of the corpus governs
where they disagree. **To become normative:** one independent review on two lenses (security and
implementability), the rulings in §12 made by the owner, and the amendments in §11 folded into
`architecture.md`, `contracts.md` and `configuration.md`.
**Reads against:** architecture §5.1, §9, §11; contracts §2.1–§2.3, §2.6, §3.2, §3.4;
[`configuration.md`](configuration.md) §1, §8; [`projections.md`](projections.md);
[`filter-index.md`](filter-index.md) §7; [`per-point-attributes.md`](per-point-attributes.md)
§3.9; [`write-path.md`](write-path.md) §2, §4; [`compaction.md`](compaction.md).
**Citation convention:** unprefixed §n is the architecture design, per CLAUDE.md; this document's
own sections are cited as **spec §n**.

> **r5 is a rewrite, and it is narrower than r4.** r4 designed views and signature grouping
> together as one `(view, group, flush)` table addressing. The two are separated here: this
> document is views alone, and grouping — a performance layout with its own adoption gates and
> no dependency on anything below — returns to
> [`deferred-signature-major-layout.md`](deferred-signature-major-layout.md), which now points at
> r4 — then named `views-and-multi-table.md`, git `ead7e906` — for the multi-table elaboration.
> r4's identity-stability tiers and its roll-mode rotation are dropped from this document for the
> same reason: they are partition and rotation questions, recoverable from git, and nothing here
> depends on them.

---

## 1. Summary

A **view** is a named coordinate system over the shared entity space. The same point may sit in
several views with a different position in each; it has one identity, one label, one set of
attributes and one mask wherever it appears. A viewer switches between views; nothing about their
authorisation changes when they do.

A **view group** is a set of views that share every setting — projection, extent, point
visibility, gate — and differ only by a member key and per-member metadata. Its members need not
be enumerated when the corpus is built: a new member is created by the first ingest batch naming
it. Time slices are the motivating case: a corpus re-embedded each quarter, where each quarter is
its own layout and the next quarter arrives while the service is running. Groups complement
plain views; a corpus may carry both.

The rule the whole design rests on is §5.1's factoring, stated as a rule:

> **Entity space is the invariant plane; a view owns everything downstream of the permutation
> and nothing upstream of it.**

Shared, in entity space: identity, the term index and the mask, labels and generating sets,
attributes and their filter index, artifact membership. Per view, in row space: positions, the
Morton order, the permutation and its inverse, the segments, the tile ranges, the render columns
and θ. One token authorises across every view; membership of a view is the permutation's
non-sentinel, never a stored set.

> **⊘ Partially implemented.** Everything is keyed per view — the manifest's `views` registry,
> `partitions/<p>/views/<view>/` with its own permutation and segments, `WalRow.view`, one flush
> segment and one fold plan per view, `view` on every viewer verb, `(view, layer, level)` on every
> artifact extent, a per-view projection. What has never existed is a bundle with **two** views:
> `tessera build` materialises exactly one and refuses a declaration with several, so nothing
> downstream has been run against more than one. The build (spec §7), the second-view join
> (spec §4), the per-view extent (spec §2), groups and scoped attributes (spec §3, §5) and the
> gate (spec §6) are what remain, and each is marked where it is claimed.

## 2. A view

**Declaration.** `[[view]]` in the configuration surface (`configuration.md` §1): `name`,
`title`, `projection`, `extent`, `source` and `fields`, `point_visibility`, and `visibility` (spec
§6). A plain view is declared when the corpus is built and is constant for the life of the
deployment: adding one is a build, not an operation. That is deliberate — a view carries a frame
and a gate, and the design has one place where those are reviewed. Growth at ingest is what
groups are for (spec §3).

**The extent belongs to the view** (decision 0040): the frame every position in that view is
quantised against, immutable for the view's life, so a Morton prefix is a permanent address in
that view. An embedding and a map cannot share a frame without one of them wasting most of the
grid, which is why the extent is per view and not per bundle.

> **⊘ Specified, not implemented.** `Manifest.quantisation` and `/v1/meta`'s `quantisation` are
> bundle-wide, and every consumer reads them there. With one view per bundle the bundle's extent
> *is* the view's, so nothing is wrong today; two views with different extents cannot coexist
> until the extent moves onto `ViewDescriptor` and the `views` entries of `/v1/meta` — a
> `bundle_format` bump (contracts §2.2, §2.5) and a wire change (§3.2). A group's members all
> share one extent by construction, so groups do not wait on this move; a second plain view does.

**Addressing.** Every viewer verb names its view in the request body (contracts §3.2); an ingest
batch names it in `x-tessera-view`, optional only while the bundle has one view (write-path
§2.1). An unknown view is a 404 on both planes, and after spec §6 a gate-failed one is the same
404. There is no coordinate map on an ingest row: a batch belongs to one view, and a point that
belongs to several is several batches. r4's `{view → (x, y)}` map is withdrawn — it was free only
while no client existed, and two do.

**Layers declare the views they are drawn on** (`configuration.md` §1, `[[layer]].views`),
because an artifact's extents are per row space. A layer may name a group, meaning every member
present and future (spec §3.5).

**What a view does not do.** Positions are not updated in place — a re-placed corpus is a new
view or, for a group, a new member. Nothing removes one entity from one view short of deleting
the entity; dropping a member removes every entity from it at once (spec §3.4). Both are the
same class of rarity as a re-label, which is delete plus re-ingest (decision 0047).

## 3. View groups

### 3.1 Declaration

```toml
[[view_group]]
name              = "quarter"
projection        = "none"
extent            = { min = [-40.0, -40.0], max = [40.0, 40.0] }
point_visibility  = { field = "access", default = "public" }
visibility        = "public"
member_visibility = { field = "quarter_access", default = "inherited" }
source            = "embeddings"
fields            = { member = "quarter", label = "quarter_label" }
metadata          = ["label", "starts", "ends"]
```

A group takes every key a `[[view]]` takes, with the same meanings, plus three of its own:

| Key | | Value |
|---|---|---|
| `metadata` | O | the names of the per-member values a member carries; every member carries every name, as a string |
| `member_visibility` | R | `{ field, default }` — each member's own gate, and what a member declaring none gets; `default` may be `inherited`, meaning the group's `visibility` (spec §6) |
| `members` | O | another group's name: this group has that group's members rather than its own (spec §3.3) |

`source` and `fields` keep their meanings from `[[view]]` and from `configuration.md` §8: the
source is the file a build reads, and `fields` says where things are, never whether they exist.
On a group the located fields are the view's own (`entity_id`, the coordinates), plus `member`
— the column whose value is each row's member key — and one column per `metadata` name and for
`member_visibility.field`; all default to their own names. A group with a source has members
for every distinct `member` value the build reads; a group with no source is declared and empty,
exactly as a layer with no source is, and its first member arrives by ingest.

A group is not a view: it cannot be named on a viewer verb, has no row space and no permutation.
Its members are views in every respect below the declaration — each with its own Morton order,
permutation, segments, extents and θ — and they are what a request names.

### 3.2 Members

A member is identified as **`<group>@<key>`**, the key being a caller-chosen string under the
column-name charset (ASCII letters, digits, `_`, `-`, contracts §2.2). The joined form is a view
id wherever a view id goes: the request body, `x-tessera-view`, `/v1/meta`, the manifest and the
`views/<view>/` directory, which accepts it as one path component. `@` is what makes a member
unmistakable for a plain view, and it is reserved out of plain view names for that reason.

Each member carries an **ordinal**, assigned monotonically at creation and never reused, and the
group's `metadata` values. Members are served in ordinal order: `/v1/meta` lists a group with its
members, each `{ key, ordinal, metadata }`, so a client can offer previous-and-next without
interpreting keys. The ordinal is creation order and nothing else — a caller ingesting quarters
out of order gets them in arrival order and sorts by its own `starts` metadata if it wants time
order. Keys and ordinals are tombstoned on drop and never reused (spec §3.4).

**Creation is a side-effect of ingest.** The first batch naming `quarter@2026-Q3` creates the
member; its metadata and, where the group reads one, its own gate label travel in
`x-tessera-view-metadata`, a JSON object carrying exactly the declared names, required on the
creating batch and refused on any later one — a member's metadata and gate are set once. The creation is a WAL record ahead of the batch, so replay recreates the
member before the rows that need it, and the served member set is the manifest's registry plus
the WAL overlay, materialised at the next flush — the overlay-then-fold shape the write path has
everywhere.

This is the one place a view is created without an operator declaring it, and it is safe for a
reason r4 spelled out when refusing auto-creation for plain views: a member has nothing of its own
to review. Its frame, projection, visibility default and gate are the group's, already declared;
the only things the batch supplies are a key, metadata and at most a member gate that can
only narrow the group's (spec §6).

> **⊘ Specified, not implemented — the whole of this section.** There is no group object, no
> member, no ordinal, no metadata header and no creation record. Every view that exists was
> declared and built.

### 3.3 Sharing members

Two groups may be layouts over the same members — a quarterly embedding and a quarterly map —
and an attribute that varies by quarter (spec §5) should apply to both without being declared
twice. A group declares `members = "quarter"` to say that its members are another group's:

```toml
[[view_group]]
name     = "quarter_map"
members  = "quarter"
projection = "web_mercator"
extent   = "auto"
point_visibility  = { field = "access", default = "public" }
visibility        = "public"
member_visibility = { default = "inherited" }
```

Keys, ordinals, metadata and each member's own gate belong to the group that owns them, and a
group naming `members` declares none of those: `metadata` and `member_visibility.field` are
refused on it, and `member_visibility.default` is the one thing it may still say, because the
inherited gate is its own. Creating `quarter@2026-Q3` creates `quarter_map@2026-Q3` at the same
moment, empty, so a request naming it is answered rather than 404ed; dropping the key drops
both. Chains are refused — `members` must name a group that declares none — so the owner of a
key set is always one hop away.

There is no separate object for the shared key set. One was considered and declined: with one
group the object is invisible, and with two it is a second name for the first group.

### 3.4 Drop

Dropping a member is a control operation: a WAL'd tombstone on the key. The member leaves
`/v1/meta` on acknowledgement, a request naming it is a 404 from then on, and its row-space
artifacts are reclaimed at the next fold. Under `members` sharing the drop is of the key, and
takes the member out of every group on it. A dropped key is never reused, because a recreated
`2026-Q3` with different contents would silently repoint every bookmark, every cached θ and every
client cache keyed on the view (decision 0029).

**Dropping a view deletes no entity, and entity deletion drops no view.** An entity whose only
member was dropped still exists, with its label, its attributes and its artifact memberships, in
no view — and a later batch into a new member picks it up by `external_id` under spec §4's join
rule, which is the ordinary shape of a corpus whose items come and go between slices. The two
lifecycles are kept apart because they retire differently: a view is row space and its artifacts
are garbage the moment the tombstone is acknowledged, while an entity leaves only through the
deny lane and retires at the fold that executes it (Rule F, write-path §5.4).

The drop takes one option, `delete_dangling = true`, for the caller who does mean "and the
items that were only here". It is defined as sugar and nothing else: at acknowledgement the
service computes the entities of the dropped member that hold a row in no other view — the
buffer included — and submits them as ordinary deletions, which enter the overlay, are
acknowledged with the drop, and retire at the fold like any deletion. It is not a second
retirement route and must not become one; a drop that removed an entity any other way would be
the fail-open the two removal rules exist to prevent. The cost is one permutation probe per
other view per row of the member, paid once at the drop and reported in its acknowledgement
with the count.

### 3.5 Layers over a group

A layer naming a group in `views` is drawn on every member, present and future. A member created
at ingest has no artifact extents for that layer until the fold that writes them — the same
window a new flush's artifacts have today (annotation-representation, the fold's artifact pass) —
and the layer answers empty on the new member until then, which is the ordinary state of a
layer over a segment the fold has not seen.

## 4. One entity in several views

Identity is entity-space, so the same point in two views is the caller saying so at ingest: two
batches, two views, one `external_id`. The second batch is the operation contracts §3.4 currently
refuses as a duplicate, and its rule is amended:

- **Unknown `external_id`**: allocate an entity, as today.
- **Known, and not in the named view**: accept. The row's position lands in the named view's
  pending segment; the entity, its label and its entity-scoped attributes are untouched.
- **Known, and already in the named view**: 409. Positions are not updated in place, and this arm
  must not become an update path by accident — the single-valued permutation cannot hold two rows
  for one entity in one view.
- **A different label**, on a known id: 409. A re-label is a delete plus a re-ingest (decision
  0047), never a field carried in on a second-view row, because the alternative is a widening
  with no overlay entry or a narrowing that bypasses the deny lanes.
- **An entity-scoped attribute** (spec §5) on a known id must byte-match the stored value or be
  absent from the batch; a differing value is a 409 naming the column. A group-scoped attribute
  is expected, because that is the value this member carries.
- **A deleted holder is not a duplicate**, as today: the re-ingest allocates fresh.

The identifier forms are r4's, kept: `external_id` is canonical; `tessera_id` is accepted with a
**mandatory** idset beside it, a retained idset translating exactly and a revoked or unknown one a
409. Contracts §2.2's argument for an optional idset on reads does not transfer to a write — a
stale identifier on a read misresolves one bounded answer; on a write it silently names another
entity.

Which views an entity is in is not stored anywhere but the permutations, and is not served:
`/v1/items` answers for the view it was asked about, and a point's absence from a view is
indistinguishable from its invisibility there (C4's closure).

> **⊘ Specified, not implemented.** A known `external_id` is a 409 whatever view the batch
> names (`/control/ingest`'s duplicate check consults every run). With one view per bundle the
> two rules agree, so nothing is wrong today; the amendment is what makes a second view
> populatable at all.

## 5. Attribute scope

An attribute today is one value per entity, evaluated in entity space, and therefore visible under
every view with no declaration saying so (`filter-index.md` §7). That stays the default and needs
no key: a **constant** attribute is not declared against views, because there is nothing a
declaration could add.

The case that needs declaring is a value that differs by member — a sentiment score recomputed
each quarter. It is declared as a **scope**:

```toml
[[attribute]]
name  = "sentiment"
type  = "f32"
scope = { group = "quarter" }      # default: scope = "entity"
```

The group named is the one that owns the members; naming a group that declares `members` is
refused, pointing at the owner. The attribute then applies to every group sharing those members.

**Storage.** A group-scoped attribute is a family of entity-space columns, one per member, each
with its own presence bitmap (decision 0064 — an absent number is presence beside the column)
and, for a category, its own postings. Nothing is materialised per row space, which is what keeps
the attribute inside I2's argument: every value is indexed by entity, every predicate answers a
bitmap in entity space, and the mask meets it there before any permutation is applied. The
family grows by one column when a member is created, empty; the fold's attribute pass
(`filter-index.md` §6.2) runs per column and needs no new case.

**Evaluation.** A filter leaf names the attribute, and the member whose column is read is decided
one of two ways:

- **Under a member of the group** (or of a group sharing its members), the request's own view
  decides: `sentiment` under `quarter@2026-Q3` reads that quarter's column. Nothing is added to
  the wire.
- **Under any other view** — a plain view, or a member of an unrelated group — the leaf must
  **pin** a member: `sentiment@2026-Q3`. That is an ordinary entity-space bitmap and it composes
  with everything else, so "the documents that were negative in Q3, on the whole-corpus map" is a
  filter like any other. An unpinned leaf there is a 422 naming the group, not an empty answer,
  because a leaf with no column to read is a malformed request rather than a constraint.

A pinned leaf under a member of the same group is allowed too — Q4's map filtered by Q3's
sentiment — and means what it says.

**Ingest.** A batch into a member carries that member's values for every group-scoped attribute
on its members, under the attribute's plain name; the member is known from the header, so the
column is not qualified. A batch into a plain view may not carry a group-scoped attribute at
all: there is no member for the value to belong to.

**Render.** A `render = true` group-scoped attribute is rendered in the members of its group and
of any group sharing them, and in no other view — the rule `per-point-attributes.md` §3.9 already
has for `render_in`, with the view set decided by the scope instead of listed.

**Member metadata is not an attribute.** A member's `label` or `starts` is one value per member,
lives on the registry entry, filters nothing and is served on `/v1/meta`. A per-(entity, member)
value is an attribute. The two are kept apart so that neither grows the other's surface.

> **⊘ Specified, not implemented — scope, the column family, the pinned leaf and the ingest
> rule.** `scope` is not a key the parser knows and would be refused under `deny_unknown_fields`,
> which is the right behaviour meanwhile.

## 6. The gate

Who may reach a view follows the shape a layer already has: a gate on the kind, and a gate on
the individual.

- **`visibility`** on a `[[view]]` or a `[[view_group]]` — an access label, or `public`
  (decision 0088), required rather than defaulted, as it is on a layer: a disclosure control
  nobody wrote is a value nobody chose. A plain view has only this.
- **`member_visibility = { field, default }`** on a group — each member's own label, read from
  the source column at a build or carried in `x-tessera-view-metadata` on the creating batch,
  and `default` for a member supplying none. `default = inherited` means the group's own, which
  is the same spelling `artifact_visibility` uses.

A member is reachable only where its group is: the group's gate is the outer bound and the
member's is taken as written inside it, so a member gate can narrow and cannot widen — the
relation decision 0089 gives an artifact to its layer, and the I12 direction.

Satisfaction is the item-visibility predicate verbatim (§6.1): the label resolves to its term
set, and the gate is satisfied iff that set intersects the principal's satisfied set. Not the
conservative label join — under §12.2's required-set reading a disjunctive gate
(`finance | legal`) yields an empty required set and every principal passes, which is a fail-open
on exactly what the gate protects. Intersection gives a disjunctive gate its intended meaning.

- The principal's **visible-view set is resolved once at authorise**, every view and every member
  evaluated whatever the outcome, so the request-time check is one set-membership lookup and a
  gate-failed name costs the same work as a never-registered one — r23's
  work-indistinguishability standard, the closure C4 records for `/v1/items`.
- A gate-failed view or member is absent from `/v1/meta`; a request naming one is a 404
  indistinguishable from an unknown name. A gate-failed group takes its member list with it.
- The gate governs every view-valued surface, not only discovery: a layer's `views` list as
  served, a member list, anything else keyed by view omits gate-failed entries.
- The gate is conjunctive with item labels, never substitutive: an item inside a gated view is
  still governed by its own label.

> **⊘ Specified, not implemented.** `visibility` on a view is parsed and refused; no gate is
> evaluated and no visible-view set exists. Every declared view is reachable by every principal
> that authorises at all, and a reader must not count gating as an available means of
> restricting reachability. Making `visibility` required is a change to every declaration in the
> repository, which is the cost of not defaulting it.

## 7. Build and populate

**A build materialises every declared view and every member `member_field` names.** The
`--view` flag and the refusal of a declaration with several views are withdrawn (decision 0091:
a build is ingest into an empty database, and an ingest can populate any view). Per view the
build is what it is today — read the source, transform, quantise, Morton-sort, write the segment
and the permutation; for a group it is that once per distinct `member_field` value, with the
members created in the order their keys first appear and the metadata read from the source's
metadata columns, which must agree for every row of a member. Entity space is built once, from
every source's rows unioned by `external_id`; an entity appears in as many row spaces as sources
placed it in.

**Populate at ingest** is spec §2's addressing and spec §4's join rule, for a plain view and a
member alike; a member that does not exist yet is created (spec §3.2).

**Bulk backfill of a plain view over an existing corpus** — a new embedding over 10⁹ items —
is r4's `tessera build --attach-view`: a build-plane operation that reads `(external_id, x, y)`,
builds the one view's row-space artifacts, references every other view's artifacts from the new
manifest verbatim, and flips `CURRENT`. It is kept as the design's answer to "add a view without
rebuilding the others" and is not scheduled: it needs manifest references that may name an older
prefix, which contracts §2.1 forbids today.

> **⊘ Specified, not implemented — all three.** The build takes `--view` and materialises one;
> a second view cannot be populated (spec §4); there is no attach.

## 8. Cost

The costs a second view adds are the r4 figures, kept because they decide the representation
choices below.

- **The permutation is sized by the maximum entity id, not by the view's population**: a flat
  `u32` array per view, ~4 GB at 10⁹, sentinel-dominated when the view is sparse. A group of
  forty quarters over one entity space is forty of them. The contracts reader interface keeps the
  representation abstract for this reason, and a **paged permutation** — a directory over
  2¹⁶-entry pages, an absent page meaning all-sentinel — is the representation for a member,
  chosen at the view's creation. Whether it should be every view's default is spec §12's
  question.
- **The projected mask is per `(token, view, segments version)`**, so a session scrubbing through
  members holds one projection per member touched. The filter-result cache
  (`filter-result-cache.md`) is view-independent by construction and is unaffected.
- **A flush writes one pending segment per view touched**; a point in k views is k rows, k
  segments' worth of merge and fold debt. That is the price of independent coordinates and is
  visible at flush, never on the request path.
- **A group-scoped attribute costs one entity-space column per member**, each the size the
  attribute would cost alone.
- **Files**: views × columns × (segments + 1), plus the attribute families — thousands at the
  counts above, inside every limit that matters.

None of these is measured against a multi-view bundle, because none exists; every figure is the
single-view cost multiplied. The first two-view build is where they become measurements.

## 9. Leak analysis

Row-space layout is already a full-corpus function — Morton rank depends on every item's
position — and has never been a leak because row ids never cross the trust boundary. Views add
row spaces, not channels. What has to be checked is what a viewer learns *from* the set of views.

- **View and member existence** is governed by the gate (spec §6), and a gate-failed view is
  indistinguishable in outcome and in work from an absent one. An ungated group's member list —
  keys, ordinals, metadata — is public to every principal that authorises, by declaration; a
  deployment whose member keys are themselves sensitive gates the group.
- **Cross-view linkage.** `tessera_id` is the same for an entity in every view — the view is not
  an input to the keyed bijection — so a viewer can join a visible item to itself across views.
  That is the point, and C17's acceptance of the identifier as a stable handle covers it.
- **An item's presence in a view** is disclosed only through the mask: an entity the viewer
  cannot see is served in no view, and an entity absent from a view is indistinguishable from
  one invisible there.
- **Group-scoped filters** are entity-space bitmaps intersected with the mask before any count,
  so the I2 argument for filters (`filter-surface.md`) applies unchanged; a pinned leaf under
  another view is the same operand with the column chosen by the request rather than by the
  view, and discloses nothing a filter under the member would not.
- **`/v1/meta` becomes per-principal** in its `views` entry, under the gate — the second such
  field beside the C11-gated vocabulary, the same precedent.
- **Timing.** A member's row space is smaller than a plain view's, and a request against it is
  correspondingly faster; the size of a member is a fact about the corpus a viewer could estimate
  from response times. It is the same class as C15 (tile-level timing over the corpus) and is
  noted there rather than given a new row.

No new verb, no new leak-register row, one register note.

## 10. What this design deliberately does not do

- **Signature grouping.** A performance layout, off by default, with its own gates; see
  [`deferred-signature-major-layout.md`](deferred-signature-major-layout.md).
- **In-place position updates**, and removing one entity from one view.
- **Runtime creation of plain views.** A plain view is a build; a member is an ingest.
- **Time in the Morton code** (§9): views are discrete and a viewer looks at one at a time.
- **Historical authorisation.** Current credentials govern every view, historical ones included
  (§9, r17).

## 11. Corpus amendments on fold-in

| Document | Change |
|---|---|
| Architecture §5.1, §9 | View generalised from the temporal case to a named coordinate system; groups and shared members; the paged permutation admitted behind the reader interface |
| Contracts §2.1 | A bundle carries several views, each `views/<view>/`; the `@` id form; the group and member registry in the manifest |
| Contracts §2.2, §2.5 | The quantisation extent moves onto the view descriptor; `bundle_format` bump |
| Contracts §2.3 | Attribute `scope` in `declared_scalars`; group-scoped column families under `attrs/<column>@<key>/` |
| Contracts §3.2 | `/v1/meta`: per-view `extent`, groups with members and metadata, gate-filtered; `filter_operands` carries the scope; the pinned leaf `name@key` in the filter grammar |
| Contracts §3.4 | The duplicate rule amended per spec §4; `x-tessera-view-metadata`; member creation; member drop with `delete_dangling`; identifier forms with mandatory idset on the `tessera_id` form |
| Configuration §1 | `[[view_group]]` with `members`, `metadata` and `member_visibility`; `fields.member`; `scope` on `[[attribute]]`; `visibility` required on a view or group; `--view` withdrawn |
| Write-path §2, §4, §5 | Member creation record; the join rule at admission; one pending segment per view touched restated for several views; `delete_dangling` as submitted deletions |
| Compaction | Reclamation of a dropped member; the attribute pass over a family |
| Appendix C | C17 note (cross-view linkage), C15 note (member size via timing), the `views` field of `/v1/meta` under C11's precedent |
| Conformance | A two-view differential: the oracle answers per view and per member; the pinned-leaf and unpinned-leaf cases; the gate's work-indistinguishability |

## 12. Rulings sought

1. **Member id syntax.** `<group>@<key>` as one path component, `@` reserved from plain names.
2. **Ordinal is creation order.** Alternatively the key could be required to sort, which would
   let the service order time slices without metadata and would refuse an out-of-order arrival.
3. **The paged permutation as every view's default**, rather than the member's representation
   only — it costs a page-directory lookup per permutation read and saves the sentinel-dominated
   4 GB for every sparse view.
4. **Whether a plain view may be attached after the build at all** (`--attach-view`, spec §7),
   or whether "a plain view is a build" is the whole rule and a new embedding is a rebuild.
5. **The pinned leaf** under a view outside the group — kept (the recommendation) or refused.
7. **`visibility` becomes required** on every view, as on every layer — or stays optional with
   `public` as its default, which would be the surface's second defaulted disclosure control.
8. **`delete_dangling`** — offered on a member drop as sugar over deletion, or left out so that
   a caller who wants the items gone deletes them.
6. **The extent move** (spec §2) — taken with the first two-view build, or taken first as its own
   `bundle_format` bump so that the manifest shape is settled before members exist.

## Appendix R — review trail

- **r5 (2026-08-30)** — rewritten against the built system. Views separated from signature
  grouping; view groups, shared members and attribute scope added; the ingest map withdrawn in favour of
  per-batch addressing; runtime creation of plain views withdrawn; identity tiers and roll-mode
  rotation moved out. Not yet reviewed.
- **r1–r4 (2026-08-01 → 2026-08-18)** — three independent reviews (performance, security,
  maintainability) of the joint views-and-tables design; the accepted findings are carried where
  they survive (the gate's intersection semantics, the mandatory idset on writes, the permutation
  budget) and the rest is in git at `ead7e906`. r4 was the slice→view rename.
