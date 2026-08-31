# Views — design

**Date:** 2026-08-30
**Status:** Normative (r13) — **a group-scoped attribute answers filters** (r13, 2026-08-31):
spec §5's evaluation rule is built end to end — the family is recorded in the manifest beside the
roster, `/v1/meta`'s `filter_operands` carries its scope, and a leaf resolves to one view's column
by the request's own view or by a pin, `name@key` or `name@#n`, with a `422` naming the group where
nothing decides and the unknown-view `404` for a pin naming no view of it (contracts §2.2, §3.2
r55). ⊘ Three named gaps remain at that claim: a category or text family, `render`, and the gate.
**the roster is served** (r12, 2026-08-31): `/v1/meta` publishes
every view of every group with its key, ordinal and typed metadata, in ordinal order, beside the
groups' own orderings; every one of them answers a viewer verb, by key or by ordinal
(contracts §3.2 r54). What is still unbuilt above the build is creation and drop while the service
runs, and the gate. **the permutation is paged** (r11, 2026-08-31): spec §8's
representation ruling is built, for every view, and its marker is discharged; contracts r53 carries
the encoding. **The build is complete for a declaration** (r10, 2026-08-31):
every plain view and every view of every group, whichever roster form declared it, over one entity
space unioned from their sources and ordered by the declared anchor (decision 0112); a group's
points selected out of a shared file by `fields.view`; one frame per group under `auto`; a
group-scoped attribute's column family on disc; and a layer drawn on a group or scoped to one.
What remains is above the build — the ingest join (spec §4), the gate (spec §6), and the serving
surfaces a scoped column and a scoped layer will need; the markers at spec §1, §5, §7 and §8 say
which.
Spec §2's per-view extent was built at r7 (contracts r52, decision 0040): the extent moved off the
bundle onto `ViewDescriptor` and into each `/v1/meta` `views` entry, with no fallback. No design
changes in either revision; the markers do. Promoted at r6 on 2026-08-30 after the
two-lens review (security, implementability); the findings and their dispositions are Appendix
R's r6 entry. §11's amendments to the wider corpus are scheduled work, and the ⊘ markers say what
exists meanwhile.
Every ruling is made; decision 0112 (the allocation anchor view) closed the last.
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
visibility, gate — and differ only by a key and per-view metadata. Its views need not be
enumerated when the corpus is built: a new one is created while the service runs and populated
by ingest. Time slices are the motivating case: a corpus re-embedded each quarter, where each
quarter is its own layout and the next quarter arrives while the service is running. Groups complement
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
> artifact extent, a per-view projection. The per-view extent (spec §2) is **built** — it was
> taken first, so the manifest shape was settled before anything depended on it — and so, since
> 2026-08-31, is **the whole build of a declaration** (spec §7): `tessera build` materialises
> every plain `[[view]]` and every view of every `[[view_group]]` — inline blocks or the rows of a
> roster table — unions entity space over their sources, refuses a label that disagrees between
> them, allocates against the declared anchor (decision 0112), and writes a row space each. A
> group's views may share one points file behind `fields.view`; a group's `auto` frame is fitted
> over every one of them; a group-scoped attribute is a column family on disc (spec §5); and a
> layer names a group or is scoped to one (spec §3.5). The roster is published in the manifest,
> validated at open, and — since 2026-08-31 — **served**: `/v1/meta` publishes every view with its
> roster record and the groups with their orderings, and every one of them answers a viewer verb,
> by key or by ordinal (spec §3.2, contracts §3.2 r53).
>
> What remains is above the build, and each refuses or is absent by name: a group whose views are
> **minted from a discriminator** with no roster at all (spec §3.1), the second-view join at
> ingest (spec §4), the gate (spec §6), and the serving surfaces a scoped **layer** would need. A
> scoped **attribute** is served: since 2026-08-31 a numeric or keyword family is a filter operand
> carrying its scope, and a leaf reads the view the request names or the one it pins (spec §5,
> contracts §3.2 r55) — what remains of it is a category or text family, and `render`, which spec
> §5's marker states.

## 2. A view

**Declaration.** `[[view]]` in the configuration surface (`configuration.md` §1): `name`,
`title`, `projection`, `extent`, `source` and `fields`, `point_visibility`, and `visibility` (spec
§6). A plain view is declared when the corpus is built and is constant for the life of the
deployment: adding one is a rebuild, not an operation. That is deliberate — a view carries a frame
and a gate, and the design has one place where those are reviewed. Growth at ingest is what
groups are for (spec §3).

**The extent belongs to the view** (decision 0040): the frame every position in that view is
quantised against, immutable for the view's life, so a Morton prefix is a permanent address in
that view. An embedding and a map cannot share a frame without one of them wasting most of the
grid, which is why the extent is per view and not per bundle.

> **Implemented 2026-08-30** (contracts r52). `ViewDescriptor.quantisation` and the `quantisation`
> object inside each `/v1/meta` `views` entry are where the extent lives; the bundle-level field is
> deleted, with no fallback and no default — a manifest whose view omits it refuses at open. This
> was taken first, ahead of any multi-view build (owner ruling 2026-08-30), so the manifest shape
> was settled before anything depended on it; the artifacts are recreated rather than migrated
> (decision 0048), and `bundle_format` did not move — the required field is the loud guard.
>
> **⊘ Three consumers still read one frame for a layer's several views, and a bundle carrying
> several now exists.** A shape publication canonicalises against one view's extent — the layer's
> *first* at `canonical_shapes` and at `/control/layers`' own resolution, the build's **anchor**
> view at the build's own read — where `polygon-membership.md` §4.3 wants it per view: a layer's
> views must share a projection, but need not share a frame. A group's views do share one by
> construction (spec §3.1), so a shape layer scoped to a group is exact; one spanning a plain view
> and a group is not, and nothing refuses it. No fixture carries a shape layer over several
> frames, so this is untested rather than known-wrong.

**Addressing.** Every viewer verb names its view in the request body (contracts §3.2); an ingest
batch names it in `x-tessera-view`, optional only while the bundle has one view (write-path
§2.1). An unknown view is a 404 on both planes, and after spec §6 a gate-failed one is the same
404. There is no coordinate map on an ingest row: a batch belongs to one view, and a point that
belongs to several is several batches. r4's `{view → (x, y)}` map is withdrawn — it was free only
while no client existed, and two do.

**Layers declare the views they are drawn on** (`configuration.md` §1, `[[layer]].views`),
because an artifact's extents are per row space. A layer may name a group, meaning every view
of it present and future, and says whether its artifacts are shared or per view (spec §3.5).

**What a view does not do.** Positions are not updated in place — a re-placed corpus is a new
view or, for a group, a new view. Nothing removes one entity from one view short of deleting
the entity; dropping a group's view removes every entity from it at once (spec §3.4). Both are the
same class of rarity as a re-label, which is delete plus re-ingest (decision 0047).

## 3. View groups

### 3.1 Declaration

A group takes every key a `[[view]]` takes, with the same meanings, and adds the roster — which
views it has — in one of two forms.

```toml
[[view_group]]                      # form A: one view per block, one file per view
name             = "quarter"
title            = "By quarter"
extent           = { x = [-40.0, 40.0], y = [-40.0, 40.0] }
visibility       = "public"                        # the group's own gate
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us", ends = "timestamp_us" }

[[view_group.view]]
key        = "2026-Q2"
source     = "q2"                   # this view's points: entity_id, x, y, access, …
visibility = "public"              # this view's gate; absent = the group's
label      = "Q2 2026"
starts     = 2026-04-01T00:00:00Z
ends       = 2026-07-01T00:00:00Z

[[view_group.view]]
key    = "2026-Q3"
source = "q3"
label  = "Q3 2026"
starts = 2026-07-01T00:00:00Z
ends   = 2026-10-01T00:00:00Z
```

```toml
[[view_group]]                      # form B: the roster is a table, the points one file
name             = "quarter"
extent           = { x = [-40.0, 40.0], y = [-40.0, 40.0] }
source           = "quarter_papers" # one row per (entity, view): entity_id, quarter, x, y, access, …
fields           = { view = "quarter" }
visibility       = "public"
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us", ends = "timestamp_us" }

[view_group.views]
source = "quarters"                 # one row per view: quarter, access, label, starts, ends
fields = { key = "quarter" }
```

| Key | | Value |
|---|---|---|
| `visibility` | D `public` | the group's own gate: an access label, or `public` (spec §6). A plain `[[view]]` carries the same key with the same meaning |
| `metadata` | O | the per-view values a view carries, `name = type` over the `[[attribute]]` types; a category is `{ type = "category", vocabulary = … }` |
| `members` | O | another group's name: this group's views are that group's (spec §3.3) |
| `[[view_group.view]]` | O, repeatable | one view, inline — `key`, `source`, `visibility`, and one key per declared metadata name. **Form A.** |
| `[view_group.views]` | O | the roster as a table: `source` and `fields` (canonical `key`, `visibility`, and the metadata names). **Form B**, with the group's own `source` and `fields.view` carrying the points |

**The roster decides where the points come from.** In form A each view's points are that view's
`source`, and the group declares no `source` of its own — the file is the view, so there is no
discriminator, exactly as a layer's file is the layer. In form B the group's `source` holds every
view's points with `fields.view` saying which view each row lands in, and the roster table lists
the views; a row naming a key the table does not carry is refused, and a listed key with no rows
is an empty view. A group declaring neither has views minted from the discriminator's distinct
values and carries no metadata. Declaring both is refused, as `source` beside inline `artifacts`
is.

`source` and `fields` keep `configuration.md` §8's rule: the map says where, never whether.
Under form B the located fields are the view's own — `entity_id`, the coordinates,
`point_visibility.field` — plus `view`; under a roster table they are `key`, `visibility` and the
metadata names, all defaulting to their own names; each view's own gate is the roster's
`visibility`, and a view carrying none takes the group's.

A group is not a view: it cannot be named on a viewer verb, has no row space and no permutation.
Its views are views in every respect below the declaration — each with its own Morton order,
permutation, segments, extents and θ — and they are what a request names.

### 3.2 Keys and ordinals

A view of a group is addressed as **`<group>:<key>`**, or **`<group>:#<ordinal>`**. The key is
the caller's, **required at creation**, under the column-name charset (ASCII letters, digits,
`_`, `-`); the ordinal is assigned monotonically at creation, never reused, and is an alias —
`#` is what keeps a numeric-looking key from being read as one. Either form is a view id
wherever a view id goes: the request body, `x-tessera-view`, `/v1/meta`, the manifest. On disc
the view lives at `views/<group>/<key>/`, nested rather than the joined id because `:` is not a
path character everywhere; `SegmentDescriptor.view` and `WalRow.view` hold the joined
`group:key` form, one named function derives the two-component path from it, and the manifest's
`files` map is keyed by the derived path. `:`, `#` and `@` are reserved out of plain view names
and keys, refused at the configuration parser and again at manifest load.

Views are served in ordinal order: `/v1/meta` lists a group with its views, each
`{ key, ordinal, metadata }`, so a client can offer previous-and-next without interpreting keys.
The ordinal is creation order and nothing else; a caller ingesting quarters out of order gets
them in arrival order and sorts by `starts` if it wants time order.

**A view is created ahead of the rows that name it**, by `PUT /control/views/{group}/{key}`
carrying the roster record — `visibility` and the metadata — which is the inline
`[[view_group.view]]` block as a request. **A roster record is immutable**: a wrong gate or
wrong metadata is a drop and a recreate under a new key, never an update — the alternative is a
narrowed gate that does not bite live sessions, a staleness the deny lane is not allowed and
the roster is not either. A batch naming a view that does not exist is a 404, as for any
unknown view — with one exception: a group whose views carry nothing (no metadata, no roster
`visibility` field) has nothing to put in the record, and there the first batch naming a new
key creates it. That is the one place a view comes into being without a declaration, and it is
safe because the view has nothing of its own to declare: its frame, projection, visibility
default and gate are the group's, already reviewed.

**The roster's durable home is the segments manifest, not the WAL.** The create and drop
records are WAL entries for replay, and the served roster is the manifest's plus the WAL
overlay — but WAL rotation reclaims records, so the roster, the ordinal high-water and the
tombstoned keys are published into the segments manifest at every flush and carried forward
for ever, exactly as `entity_id_low_water` and `layer_tombstones` are and for the same reason:
a mark that lives only in the log is lost at the first rotation, and a reused ordinal or key
silently repoints every client cache keyed on the view (decision 0029).

**Metadata names are bounded by the roster's own keys**: `key`, `source`, `visibility` and, on
a form B group, the discriminator's field name are refused as metadata names — the inline block
and the roster table would otherwise be ambiguous. The form A block mixes closed keys with the
declared metadata names, so it is parsed by the manual route the `extent` spellings already
take rather than by `deny_unknown_fields` alone; the discipline's guarantee — an unknown key is
refused — is preserved by checking against the declared set.

> **⊘ Partially implemented — the roster is built and served; creation is not** (2026-08-31).
> The group object, the roster and the ordinal exist: they are published in the segments manifest
> at a build, validated against the view registry at open, and served on `/v1/meta` — the plain
> views in manifest order, then each group's views in ordinal order, each carrying its `group`,
> `key`, `ordinal` and typed `metadata`, beside a `groups` array giving each group's view ids in
> ordinal order (contracts §3.2 r53). Both addressing forms resolve, `<group>:<key>` and
> `<group>:#<ordinal>`, in one place for both planes, so an unknown name, an absent key and an
> ordinal no view holds are one 404.
>
> **What is not built is creation while the service runs**: there is no `PUT
> /control/views/{group}/{key}`, no drop (spec §3.4), no WAL create or tombstone record, no
> ordinal high-water carried across a flush, and no first-batch-creates route for a group whose
> views carry nothing. Every view that exists was declared and built, so every ordinal is roster
> order and no key has ever been retired.

### 3.3 Sharing views

Two groups may be layouts over the same views — a quarterly embedding and a quarterly map — and
an attribute that varies by quarter (spec §5) should apply to both without being declared twice.
A group declares `members = "quarter"` to say that its views are another group's:

```toml
[[view_group]]
name             = "quarter_map"
members          = "quarter"
projection       = "web_mercator"
extent           = { lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }
source           = "quarter_places"
fields           = { view = "quarter" }
point_visibility = { field = "access", default = "public" }
```

Keys, ordinals, metadata and each view's own gate belong to the group that owns them, and a
group naming `members` declares none of those: `metadata` and both roster forms are refused on
it, and its own `visibility` is the one gate it may still declare, because a second layout may
be narrower than the first. Its points come from its own `source` in form B's shape — a
discriminator column against the owning group's keys. Creating `quarter:2026-Q3` creates `quarter_map:2026-Q3` at the same moment, empty, so
a request naming it is answered rather than 404ed; dropping the key drops both. Chains are
refused — `members` must name a group that declares none — so the owner of a key set is always
one hop away.

There is no separate object for the shared key set. One was considered and declined: with one
group the object is invisible, and with two it is a second name for the first group.

### 3.4 Drop

Dropping a view of a group is a control operation: a WAL'd tombstone on the key. The view leaves
`/v1/meta` on acknowledgement, a request naming it is a 404 from then on, and its row-space
artifacts are reclaimed at the next fold. Under `members` sharing the drop is of the key, and
takes the view out of every group on it. A dropped key is never reused, because a recreated
`2026-Q3` with different contents would silently repoint every bookmark, every cached θ and every
client cache keyed on the view (decision 0029).

**Dropping a view deletes no entity, and entity deletion drops no view.** An entity whose only
view was dropped still exists, with its label, its attributes and its artifact memberships, in
no view — and a later batch into a new view picks it up by `external_id` under spec §4's join
rule, which is the ordinary shape of a corpus whose items come and go between slices. The two
lifecycles are kept apart because they retire differently: a view is row space and its artifacts
are garbage the moment the tombstone is acknowledged, while an entity leaves only through the
deny lane and retires at the fold that executes it (Rule F, write-path §5.4).

The drop takes one option, `delete_dangling = true`, for the caller who does mean "and the
items that were only here". It is defined as sugar and nothing else: at acknowledgement the
service computes the entities of the dropped view that hold a row in no other view — the
buffer included — and submits them as ordinary deletions, which enter the overlay, are
acknowledged with the drop, and retire at the fold like any deletion. It is not a second
retirement route and must not become one; a drop that removed an entity any other way would be
the fail-open the two removal rules exist to prevent. The cost is one permutation probe per
other view per row of the view, paid once at the drop and reported in its acknowledgement
with the count.

### 3.5 Layers over a group

A layer naming a group in `views` is drawn on every view of it, present and future, and there
are two things such a layer can be, so it says which with the same `scope` key an attribute
takes (spec §5):

- **`scope = "entity"`** (the default): one artifact set, drawn on every view the layer names — a
  curated reading list shown on the whole-corpus map and on every quarter alike. The layer's
  file is what it is today.
- **`scope = { group = "quarter" }`**: a different artifact set per view — clusters recomputed
  each quarter. The artifact rows carry a `view` column (`fields.view`), an artifact belongs to
  one view, keys are unique per `(layer, view)`, edges (`parent`, `attached_key`) may not cross
  views, and `views` may name only that group and groups sharing its views. Membership storage
  is unchanged — an entity set per artifact — and its row-space projection was per view already.

A view created at ingest has no artifact extents for either kind of layer until the fold that
writes them, and the layer answers empty on the new view until then — the ordinary state of a
layer over a segment the fold has not seen.

**A shape layer over several views** follows `polygon-membership.md` §4.3 (decision 0111): a
`wgs84` shape spans any set of projected views, each through its own transform; `view`-space
geometry spans only views sharing projection and frame — which a group's views do by
construction, so a shape layer scoped to a group is the embedding case done safely; and a
layer's views are all projected or all `none`, never the mix.

## 4. One entity in several views

Identity is entity-space, so the same point in two views is the caller saying so at ingest: two
batches, two views, one `external_id`. The second batch is the operation contracts §3.4 currently
refuses as a duplicate, and its rule is amended:

- **Unknown `external_id`**: allocate an entity, as today.
- **Known, and not in the named view**: accept. The row's position lands in the named view's
  pending segment; the entity, its label and its entity-scoped attributes are untouched.
- **Known, and already in the named view**: 409. "In the view" is the view's permutation **and
  the commit window's buffer** — a row accepted but not yet flushed is in no permutation, and a
  check that misses it lets two batches in one window hand flush two rows for one entity in one
  view. Positions are not updated in place, and this arm must not become an update path by
  accident — the single-valued permutation cannot hold two rows for one entity in one view.
- **A suppressed holder** takes the same arms as a live one, and stays hidden: the new row lands
  on the *same* entity, suppression composes in entity space, and the entity is invisible in the
  new view as in every other from the moment the row exists. What write-path §2.1 refuses is a
  byte-identical *re-ingest past* a suppression — a second copy under a fresh entity — and
  attaching a view to the suppressed entity creates no copy. Stated because the two removal
  rules have been conflated twice, and this is the rule's edge.
- **A different label**, on a known id: 409. A re-label is a delete plus a re-ingest (decision
  0047), never a field carried in on a second-view row, because the alternative is a widening
  with no overlay entry or a narrowing that bypasses the deny lanes.
- **An entity-scoped attribute** (spec §5) on a known id must byte-match the stored value or be
  absent from the batch; a differing value is a 409 naming the column. A group-scoped attribute
  is expected, because that is the value this view carries.
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

The case that needs declaring is a value that differs by view — a sentiment score recomputed
each quarter. It is declared as a **scope**:

```toml
[[attribute]]
name  = "sentiment"
type  = "f32"
scope = { group = "quarter" }      # default: scope = "entity"
```

The group named is the one that owns the members; naming a group that declares `members` is
refused, pointing at the owner. The attribute then applies to every group sharing those views.

**Storage.** A group-scoped attribute is a family of entity-space columns, one per view of the group, each
with its own presence bitmap (decision 0064 — an absent number is presence beside the column)
and, for a category, its own postings. Nothing is materialised per row space, which is what keeps
the attribute inside I2's argument: every value is indexed by entity, every predicate answers a
bitmap in entity space, and the mask meets it there before any permutation is applied. The
family grows by one column when a view is created, empty; the fold's attribute pass
(`filter-index.md` §6.2) runs per column and needs no new case.

**Evaluation.** A filter leaf names the attribute, and the view whose column is read is decided
one of two ways:

- **Under a view of the group** (or of a group sharing its views), the request's own view
  decides: `sentiment` under `quarter:2026-Q3` reads that quarter's column. Nothing is added to
  the wire.
- **Under any other view** — a plain view, or a view of an unrelated group — the leaf must
  **pin** a view: `sentiment@2026-Q3`, or `sentiment@#3` by ordinal. That is an ordinary
  entity-space bitmap and it composes with everything else, so "the documents that were negative in Q3, on the whole-corpus map" is a
  filter like any other. An unpinned leaf there is a 422 naming the group, not an empty answer,
  because a leaf with no column to read is a malformed request rather than a constraint.

A pinned leaf under a view of the same group is allowed too — Q4's map filtered by Q3's
sentiment — and means what it says.

**The scoped surface is inside the gate** (review finding, accepted). A group-scoped attribute
exists, for a principal, only where the group's gate passes: `/v1/meta`'s `filter_operands`
omits it otherwise, a pin resolves the named view through the session's visible-view set — a
gate-failed pin is indistinguishable from an attribute that was never declared — and the
unpinned 422 names the group only where the principal can reach it. Without this the pinned
leaf is a route around spec §6: a principal failing `quarter`'s gate could filter their visible
entities by a Q3 value, which is per-entity membership of a gated view. Decision 0090's
argument — a gate at some surfaces and not others is fail-open — is the rule applied here.

**Ingest.** A batch into a group's view carries that view's values for every attribute scoped to
the group, under the attribute's plain name; the view is known from the header, so the column
is not qualified. At a build the attribute's own `source` carries the value and, unless that
source is a `[[view_group.view]]` file, `fields.view` says which view each row's value is for. A
batch into a plain view may not carry a group-scoped attribute at all: there is no view of the
group for the value to belong to.

**Render.** A `render = true` group-scoped attribute is rendered in the views of its group and
of any group sharing them, and in no other view — the rule `per-point-attributes.md` §3.9 already
has for `render_in`, with the view set decided by the scope instead of listed.

> **⊘ Specified, not implemented.** No scoped column reaches any row's hot tail, so a scoped
> attribute is rendered in **no** view whatever it declares; `render = true` on one is recorded by
> the declaration and printed at the build as buying nothing. A client reading a viewport response
> must not expect the value, and a reader must not count `render` here as an available means of
> getting it into a row.

**View metadata is not an attribute.** A view's `label` or `starts` is one value per view, lives
on the roster, filters nothing and is served typed on `/v1/meta`. A per-(entity, view) value is
an attribute. The two are kept apart so that neither grows the other's surface.

> **Implemented 2026-08-31 — the filter surface, end to end** (contracts §2.2, §3.2 r55).
> `scope` parses; a build writes one entity-space column per view of the group, each with its own
> presence bitmap, at `attrs/<column>/<group>/<key>/`, read from that view's own points — under the
> view's own selection where a group's views share one file — and every file is digested, so
> `tessera verify` walks them. The family's record is `MANIFEST.groups[..].scoped_scalars`, beside
> the ordinals a pin resolves against, and deliberately **not** `MANIFEST.declared_scalars`, which
> is one flat bundle-wide list with no slot for a family. From there the engine opens one column
> per view; `/v1/meta`'s `filter_operands` entry carries the scope; and a leaf resolves exactly as
> this section says — the request's own view under a view of the group or of a group sharing them,
> a pin by key or ordinal anywhere else, a `422` naming the group where nothing decides, and the
> unknown-view `404` for a pin naming no view of it. Evaluation is the family's ordinary one over
> the resolved column: a value scan under the candidate, the presence bitmap for absence, an entity
> bitmap the mask meets before any count.
>
> ⊘ **Three things remain unbuilt, each named here.** A **category** or **text** scoped family is stored
> and on no filter surface: the per-view postings each is answered from are not written, and a
> category's value list is `/v1/categories`' own surface besides — the build prints what the
> declaration did not buy. **`render` on a scoped attribute renders nothing**, in any view: the hot
> column is per row space and a scoped column is in none of them, so the paragraph below is
> specification and not behaviour; the build prints that too. And the **gate** is unbuilt (spec
> §6), so no pin is filtered against a visible-view set today — the check has one site when it
> lands, the view resolution a pin goes through, where a gate-failed pin becomes the same 404 an
> absent key already gets. The ingest rule is unimplemented with the rest of the write half.
>
> ⊘ **A scoped attribute declaring its own `source` is refused by name**: that file needs
> `fields.view` to say which view each row's value is for, and reading it as entity space would
> take one view's values as every view's. Declaring none — Appendix A's `sentiment`, and the
> fixture's — is the shape that builds.

## 6. The gate

Who may reach a view follows the shape a layer already has: a gate on the kind, and a gate on
the individual.

- **`visibility`** on a `[[view]]` or a `[[view_group]]` — an access label, or `public`
  (decision 0088), defaulting to `public` (owner ruling 2026-08-30): a view is a coordinate
  system over items that carry their own labels, and the ordinary corpus gates none of them.
  One key, spelled as it is on a layer.
- **A group's view carries its own `visibility` on the roster** — the inline block's key, the
  roster table's column, the create operation's record — and a view carrying none takes the
  group's, as an artifact's `inherited` takes its layer's.
- **`point_visibility`** is the item's label and is unchanged.

A view of a group is reachable only where its group is: the group's gate is the outer bound and
the view's is taken as written inside it, so a view gate can narrow and cannot widen — the
relation decision 0089 gives an artifact to its layer, and the I12 direction.

Satisfaction is the item-visibility predicate verbatim (§6.1): the label resolves to its term
set, and the gate is satisfied iff that set intersects the principal's satisfied set. Not the
conservative label join — under §12.2's required-set reading a disjunctive gate
(`finance | legal`) yields an empty required set and every principal passes, which is a fail-open
on exactly what the gate protects. Intersection gives a disjunctive gate its intended meaning.

- The principal's **visible-view set is resolved once at authorise and is fixed for the
  session's life**, every view of every group evaluated whatever the outcome, so the
  request-time check is one set-membership lookup and a gate-failed name costs the same work as
  a never-registered one — r23's work-indistinguishability standard, the closure C4 records for
  `/v1/items`. **A view created after a session authorised is a 404 to that session until it
  re-authorises** (owner ruling 2026-08-30): creation is rare, tokens expire, and the
  alternatives — per-request gate evaluation, or a lazily-evaluated miss — cost the
  work-indistinguishability this bullet exists to hold. Roster immutability (spec §3.2) is the
  other half: a gate, once written, never changes, so a fixed set can never hold a stale
  *widening*.
- A gate-failed view is absent from `/v1/meta`; a request naming one is a 404
  indistinguishable from an unknown name. A gate-failed group takes its roster with it.
- The gate governs every view-valued surface, not only discovery: a layer's `views` list as
  served, a roster, anything else keyed by view omits gate-failed entries.
- The gate is conjunctive with item labels, never substitutive: an item inside a gated view is
  still governed by its own label.

> **⊘ Specified, not implemented.** `visibility` on a view is parsed and refused; no gate is
> evaluated and no visible-view set exists. Every declared view is reachable by every principal
> that authorises at all, and a reader must not count gating as an available means of
> restricting reachability.

## 7. Build and populate

**A build materialises every declared view and every view of every group.** The `--view` flag
and the refusal of a declaration with several views are withdrawn. Per view the build is what
it is today — read the source, transform, quantise, Morton-sort, write the segment and the
permutation — run once per view; for a group under form B, once per distinct discriminator
value.

What is not one-view-at-a-time is entity space. Today the build reads one source, allocates
entity ids in signature-sorted order as it goes, and never meets an entity twice. With several
point sources an entity may appear in each of them, and in a form B source once per view, so
the build becomes two passes: **pass one** collects `(external_id, label)` over every point
source — plain views, group sources, inline view files — unions by `external_id`, refuses a label
that disagrees between appearances (it is the entity's label, not the row's), and allocates;
**pass two** builds each view's row space against those ids. A row is unique per
`(external_id, view)`, and the same entity in two views is the ordinary case rather than a
duplicate.

**The allocation key over several sources** ([decision 0112](../decisions/0112-the-anchor-view-orders-a-signature-groups-ids.md),
extending 0073): within a signature group, ties order by the item's Morton code in the
**declared anchor view** — `[defaults].allocation_view`, required when more than one view is
declared, refused absent naming the candidates — then by `external_id` bytes. An item absent
from the anchor takes its Morton code in the first-declared view that holds it. Explicit rather
than positional, so reordering declaration blocks cannot silently re-key a rebuild; the ids are
permanent (I9), which is why the anchor is a declaration and not a default.

**Populate at ingest** is spec §2's addressing and spec §4's join rule, for a plain view and a
group's view alike, after the create operation of spec §3.2 where the view is new.

**No plain view is added after the build.** A new whole-corpus embedding is a rebuild; growth at
ingest is what groups are for. r4's `--attach-view` — a build-plane backfill of one view over an
existing corpus, the other views carried by manifest reference — is withdrawn with it; if the
need arrives the design is in git.

> **⊘ Partially implemented — the build is complete for a declaration; ingest is not.** `--view`
> is gone and a build materialises every view the declaration enumerates: pass one unions
> `(external_id, label)` over every view's source, refuses a disagreement naming the entity and
> the files, and allocates against `[defaults].allocation_view`, whose absence is a refusal
> listing the candidates; pass two transforms, quantises against each view's own frame,
> Morton-sorts and writes each row space, a view's permutation being sentinel wherever it does not
> hold the entity. A group's views come from inline blocks or from a roster table read before pass
> two; their points from a file each, or from one file selected by `fields.view` — a row naming a
> key the roster does not carry being a refusal naming both, and a listed key with no rows an
> empty view. A group's `auto` frame is surveyed over every view's source and fitted once, per
> spec §3.1's one-frame rule.
>
> **⊘ What a build still cannot enumerate is a group with no roster at all** — its keys minted
> from the discriminator's distinct values — which refuses by name. Populating a view at ingest is
> spec §4's join rule, still unimplemented.
>
> **The registry's order is the declaration's, block kind by block kind**: the plain `[[view]]`
> blocks in declaration order, then each `[[view_group]]` in declaration order with its views in
> roster order. `Config` holds the two block kinds in separate lists, so their interleaving in the
> document is not recoverable — which matters only to the fallback for an item absent from the
> anchor, and is stated here rather than left to be inferred.

## 8. Cost

The costs a second view adds are the r4 figures, kept because they decide the representation
choices below.

- **The permutation is sized by the maximum entity id, not by the view's population**: a flat
  `u32` array per view, ~4 GB at 10⁹, sentinel-dominated when the view is sparse. A group of
  forty quarters over one entity space is forty of them. The contracts reader interface keeps the
  representation abstract for this reason, and a **paged permutation** — a directory over
  2¹⁶-entry pages, an absent page meaning all-sentinel — is every view's representation (owner
  ruling 2026-08-30), a flat array being the degenerate case with every page present.

  **Implemented at r11** (2026-08-31; contracts §2.6 carries the encoding). Every view is written
  paged, dense and sparse alike, and the file version moves to 2 so a flat one is refused at open
  rather than read as a directory. What a sparse view stores is its pages: measured at 3.9× smaller
  on a four-page build with one view holding 5 000 of 262 144 entities. **Below 2¹⁶ entities the
  saving is zero** and a view costs one whole page — the entity space is one page wide, so there is
  nothing to leave out. The cost being removed is the one at 10⁹, and it is removed per view of the
  group. What the paging does **not** shrink is the compaction fold's scatter, which still lays out
  every page before compacting them down (contracts Appendix R, r53).
- **The projected mask is per `(token, view, segments version)`**, so a session scrubbing through
  a group's views holds one projection per view touched. The filter-result cache
  (`filter-result-cache.md`) is view-independent by construction and is unaffected.
- **A flush writes one pending segment per view touched**; a point in k views is k rows, k
  segments' worth of merge and fold debt. That is the price of independent coordinates and is
  visible at flush, never on the request path.
- **A group-scoped attribute costs one entity-space column per view of the group**, each the size the
  attribute would cost alone.
- **Files**: views × columns × (segments + 1), plus the attribute families — thousands at the
  counts above, inside every limit that matters.

None of these is measured against a multi-view bundle, because none exists; every figure is the
single-view cost multiplied. The first two-view build is where they become measurements.

## 9. Leak analysis

Row-space layout is already a full-corpus function — Morton rank depends on every item's
position — and has never been a leak because row ids never cross the trust boundary. Views add
row spaces, not channels. What has to be checked is what a viewer learns *from* the set of views.

- **View existence** is governed by the gate (spec §6), and a gate-failed view is
  indistinguishable in outcome and in work from an absent one — by name. **Ordinals are the
  exception, and it is accepted** (owner ruling 2026-08-30, a new Appendix C row): ordinals are
  monotone per group and a gate-failed view is omitted from the roster, so a principal seeing
  ordinals 0, 1, 3 learns *a* view exists at #2, and a moving high-water counts hidden
  creations. The row's argument is C15's — knowing something was created is not knowing whose
  or what; a gap and a dropped key are indistinguishable; and a deployment whose roster shape
  is itself sensitive gates the *group*, which hides the whole roster, gaps included. The
  alternatives — per-principal-dense ordinals, or none — re-open the per-session handle
  machinery decision 0006 retired, for a channel of one bit per creation.
- An ungated group's roster — keys, ordinals, metadata — is public to every principal that
  authorises, by declaration.
- **Cross-view linkage.** `tessera_id` is the same for an entity in every view — the view is not
  an input to the keyed bijection — so a viewer can join a visible item to itself across views.
  That is the point, and C17's acceptance of the identifier as a stable handle covers it.
- **An item's presence in a view** is disclosed only through the mask: an entity the viewer
  cannot see is served in no view, and an entity absent from a view is indistinguishable from
  one invisible there.
- **Group-scoped filters** are entity-space bitmaps intersected with the mask before any count,
  so the I2 argument for filters (`filter-surface.md`) applies unchanged; a pinned leaf under
  another view is the same operand with the column chosen by the request rather than by the
  view, and — *for a principal who passes the group's gate, which spec §5 requires* — discloses
  nothing a filter under the view would not.
- **`/v1/meta` becomes per-principal** in its `views` entry, under the gate — the second such
  field beside the C11-gated vocabulary, the same precedent.
- **Timing.** A group's view is smaller than a plain view, and a request against it is
  correspondingly faster; the size of a group's view is a fact about the corpus a viewer could estimate
  from response times. It is the same class as C15 (tile-level timing over the corpus) and is
  noted there rather than given a new row.

No new verb; one new accepted register row (the ordinal gap), and the C15/C17 notes.

## 10. What this design deliberately does not do

- **Signature grouping.** A performance layout, off by default, with its own gates; see
  [`deferred-signature-major-layout.md`](deferred-signature-major-layout.md).
- **In-place position updates**, and removing one entity from one view.
- **Plain views after the build.** A plain view is a rebuild; a group's view is a create and an ingest.
- **Time in the Morton code** (§9): views are discrete and a viewer looks at one at a time.
- **Historical authorisation.** Current credentials govern every view, historical ones included
  (§9, r17).

## 11. Corpus amendments on fold-in

| Document | Change |
|---|---|
| Architecture §5.1, §9 | View generalised from the temporal case to a named coordinate system; groups and shared views; the paged permutation as the representation |
| Contracts §2.1 | A bundle carries several views, `views/<view>/` and `views/<group>/<key>/`; the `group:key` and `group:#n` id forms; the roster, ordinal high-water and key tombstones in the segments manifest, carried for ever |
| Contracts §2.2, §2.5 | The quantisation extent moves onto the view descriptor — first, ahead of any multi-view build |
| Contracts §2.2 | **Done at contracts r55** for the attribute: a `groups` row carrying the roster and each group's `scoped_scalars`, with the column families under `attrs/<column>/<group>/<key>/`. ⊘ A layer's `scope` still has no manifest field — it is compiled beside the declaration and never written |
| Contracts §3.2 | `/v1/meta`: per-view `extent` (r52), groups with their rosters and typed metadata (r53–r54), `filter_operands` carrying the scope and the pinned leaf `name@key` / `name@#n` in the filter grammar (r55) — all done, ⊘ none of it gate-filtered, the gate being unbuilt |
| Contracts §3.4 | The duplicate rule amended per spec §4; `PUT /control/views/{group}/{key}` and its drop with `delete_dangling`; identifier forms with mandatory idset on the `tessera_id` form; `--view` withdrawn |
| Configuration §1, §8 | `[[view_group]]` with `[[view_group.view]]`, `[view_group.views]`, `members`, `metadata` and per-view `visibility` on the roster; `fields.view` on a group source, a scoped attribute and a scoped layer; `scope` on `[[attribute]]` and `[[layer]]` |
| Write-path §2, §4, §5 | The create record; the join rule at admission; one pending segment per view touched restated for several views; `delete_dangling` as submitted deletions |
| Compaction | Reclamation of a dropped view; the attribute pass over a family |
| Appendix C | **New accepted row: the ordinal gap** (spec §9); C17 note (cross-view linkage), C15 note (a group's view's size via timing); the `views` field and the scoped `filter_operands` entries of `/v1/meta` gate-filtered under C11's precedent |
| Conformance | A two-view differential: the oracle answers per view; the pinned-leaf cases, the gate-failed pin among them; the gate's work-indistinguishability |
| Decisions | ~~The allocation key~~ — ruled, decision 0112 |

## 12. Rulings

Made 2026-08-30 (owner), first pass: the `group:key` and `group:#ordinal` forms; the paged
permutation as every view's representation; no plain view after the build; `visibility` as the
one gate key, defaulting to `public`, the per-view gate on the roster; `delete_dangling` kept;
typed metadata; the create operation ahead of the first batch; the two roster forms; the extent
move taken first.

Made 2026-08-30 (owner), dispositioning the review: the scoped-attribute surface is inside the
gate (spec §5); the ordinal gap is accepted as an Appendix C row (spec §9); the visible-view
set is fixed for the session's life and a new view waits for re-authorisation (spec §6); the
key is required and the ordinal is an alias; roster records are immutable.

Made 2026-08-30 (owner): the allocation key — a declared anchor view's Morton code then
`external_id` bytes, within the signature group (decision 0112). Nothing is open.

## Appendix A — a declaration, written out

The whole surface for one build, as a data pipeline would emit it: a whole-corpus embedding, a
quarterly re-embedding, a quarterly map sharing the quarters, one constant and one quarterly
attribute, and three layers — clusters of the whole corpus, clusters per quarter, and curated
lists drawn everywhere. `configuration.md` §1's keys throughout; what this document adds is
marked.

```toml
[sources]
papers         = "papers.parquet"             # doc_id, x, y, access, year, venue
q2             = "papers-2026-Q2.parquet"     # doc_id, x, y, access, sentiment — one file per quarter
q3             = "papers-2026-Q3.parquet"
quarter_places = "places-by-quarter.parquet"  # doc_id, quarter, lon, lat, access
venues         = "venues.parquet"
clusters_all   = "clusters-all.parquet"       # key, contents, members
clusters_q     = "clusters-by-quarter.parquet"# key, quarter, contents, members
collections    = "collections.parquet"        # key, contents, members, access

[defaults]
source          = "papers"
entity_id_field = "doc_id"

[[view]]
name             = "all"
title            = "All papers"
extent           = "auto"
visibility       = "public"                                   # new; default
point_visibility = { field = "access", default = "public" }

[[view_group]]                                                 # new block
name             = "quarter"
title            = "By quarter"
extent           = { x = [-40.0, 40.0], y = [-40.0, 40.0] }
visibility       = "public"
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us", ends = "timestamp_us" }

[[view_group.view]]                                            # form A: one file per view
key = "2026-Q2"
source = "q2"
label = "Q2 2026"
starts = 2026-04-01T00:00:00Z
ends = 2026-07-01T00:00:00Z

[[view_group.view]]
key = "2026-Q3"
source = "q3"
label = "Q3 2026"
starts = 2026-07-01T00:00:00Z
ends = 2026-10-01T00:00:00Z

[[view_group]]
name             = "quarter_map"
title            = "Affiliations by quarter"
members          = "quarter"                                   # the same views as quarter's
projection       = "web_mercator"
extent           = { lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }
source           = "quarter_places"                            # form B's shape for the points
fields           = { view = "quarter" }
point_visibility = { field = "access", default = "public" }

[[vocabulary]]
name = "venue"
width = "u16"
value_set = "closed"
visibility = "public"
source = "venues"

[[attribute]]                                                  # constant: every view
name = "year"
type = "u16"
index = true

[[attribute]]
name = "venue"
type = "category"
vocabulary = "venue"
index = true

[[attribute]]                                                  # one value per (paper, quarter)
name   = "sentiment"
type   = "f32"
scope  = { group = "quarter" }                                 # new
index  = true
render = true
# no source: read from each quarter's own file, the group's views being form A

[[layer]]
name       = "clusters"
views      = ["all"]
source     = "clusters_all"
membership = "enumerated"
hierarchy  = { kind = "flat" }
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "any"

[[layer]]                                                      # a different set per quarter
name       = "quarter_clusters"
views      = ["quarter"]
scope      = { group = "quarter" }                             # new
source     = "clusters_q"
fields     = { view = "quarter" }
membership = "enumerated"
hierarchy  = { kind = "flat" }
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "any"

[[layer]]                                                      # one set, drawn on every view named
name       = "collections"
views      = ["all", "quarter", "quarter_map"]
source     = "collections"
membership = "enumerated"
hierarchy  = { kind = "flat" }
visibility = "public"
artifact_visibility = { field = "access", default = "inherited" }
require_member_visibility = "all"
```

What the build does with it: pass one reads `papers`, `q2`, `q3` and `quarter_places`, unions
by `doc_id`, checks `access` agrees wherever a paper appears, and allocates; pass two builds
four row spaces for `all`, `quarter:2026-Q2`, `quarter:2026-Q3` and their two `quarter_map`
counterparts, and `sentiment` as two entity-space columns. A paper in `q3` and not in `papers`
is an entity in one row space; a paper in both is one entity in two. The file cannot build
until the extent lives on the view (spec §2): `all` and `quarter` declare different frames.

At ingest the next quarter is `PUT /control/views/quarter/2026-Q4` with
`{ "label": "Q4 2026", "starts": …, "ends": … }`, which also creates `quarter_map:2026-Q4`; then
batches under `x-tessera-view: quarter:2026-Q4` carrying `doc_id, x, y, access, sentiment`, and
under `quarter_map:2026-Q4` carrying `doc_id, lon, lat, access`. A paper already known joins
each view under spec §4's rule.

## Appendix R — review trail

- **r13 (2026-08-31)** — a group-scoped attribute answers filters, and spec §5's marker moves
  from *on no serving surface* to *served, with three named gaps*. Built since r12: the family's
  record in `MANIFEST.groups[..].scoped_scalars` — the group is where it belongs, beside the
  ordinals a pin resolves against, `declared_scalars` being positional and having no slot for a
  family; one column opened per view; `filter_operands` carrying the scope; and the resolution
  this section specifies, in one place, over the same view namespace a request's own `view` goes
  through. Evaluation is unchanged and that is the point: the scope decides which column file, and
  a scoped leaf is answered by the family's ordinary scan under the candidate, its presence bitmap
  for absence, and an entity bitmap the mask meets before any count — I2's argument untouched.
  **Two refusals, deliberately different codes** (contracts §3.1's closed list): a bare leaf where
  nothing decides is a `422` naming the group, since a leaf with no column to read is malformed
  rather than a constraint; a pin naming no view of the group is the `404` an unknown view gets,
  in the same detail shape, because that is the answer a gate-failed pin must take when spec §6
  lands and the two must already be indistinguishable. Not moved: a **category** or **text** scoped
  family, whose per-view postings nothing writes; **`render`**, which reaches no row's hot tail in
  any view; and the **gate**, so no pin is filtered against a visible-view set — the site it will
  be is named at the claim. No design content changed in this revision; the wire shape is contracts
  §2.2/§3.2 r55.
- **r12 (2026-08-31)** — the roster is served, and spec §1's and §3.2's markers record it. Built
  since r10: `/v1/meta` publishes every view in roster order with its `group`, `key`, `ordinal` and
  typed `metadata`, and a `groups` array giving each group's view ids in ordinal order; every view
  of every group answers a viewer verb; and `<group>:#<ordinal>` resolves wherever a view id goes,
  in one place both planes take, so an unknown name, an absent key and an ordinal no view holds are
  one 404. The wire shape is contracts §3.2 r54. Not moved: creation and drop while the service
  runs — no control route, no WAL create or tombstone record, no ordinal high-water across a flush
  — and no gate (spec §6), so every declared view is reachable by every principal, which the served
  roster says rather than pretends otherwise. **A group's `title` is on no surface**: no title
  survives the build for any view either, so publishing a group's would be the only one on the
  document. No design content changed in this revision.
- **r11 (2026-08-31)** — the paged permutation is built, and spec §8's marker is discharged.
  Every view's `permutation.bin` is a directory over 2¹⁶-entry pages with the payload holding only
  the pages the view occupies; the representation stayed behind `Permutation`, so no consumer
  outside `tessera-store` moved. The version field carries the refusal a `bundle_format` bump would
  have, by owner direction. Two things are recorded rather than assumed: the saving is zero below
  2¹⁶ entities, where one page covers the whole entity space, and the fold's transient scatter is
  unchanged. No design content changed in this revision — contracts r53 carries the encoding.
- **r10 (2026-08-31)** — the build is complete for a declaration, and the markers at spec §1, §5
  and §7 record it. Built since r9: form B's selection (a view's rows picked out of a shared
  points file by `fields.view`, with a stray key refused naming the roster); a roster read from a
  `[view_group.views]` table, under the inline block's own rules; `extent = "auto"` on a group,
  surveyed over every view's source and fitted once; a group-scoped attribute's column family at
  `attrs/<column>/<group>/<key>/`, storage only; and layers over a group — a group name in `views`
  expanded to its views, and a scoped layer's artifacts drawn each in its own view. Not moved, and
  each refusing or absent by name: a group whose views are minted from a discriminator, a scoped
  attribute with its own `source`, a scoped layer reusing one key in two views (the store keys an
  artifact per `(layer, key)`), every serving surface for a scoped column, and the paged
  permutation (spec §8), which r11 then built. No design content changed in this revision.
- **r9 (2026-08-30)** — spec §7's two passes are implemented for the point half, and the markers
  at spec §1, §7 and §8 record what did and did not move. `tessera build` materialises every
  plain view and every inline-declared view of a group; `[defaults].allocation_view` is read and
  refused absent; the label-agreement rule is a refusal naming the entity and the files, made
  exact by a count identity rather than a hash; the roster is published in the manifest and
  validated at open; `views/<group>/<key>/` is derived from the joined id by one function every
  path consumer goes through. Not moved, and each refusing by name: a roster table, a
  discriminator group, a `fields.view` selection, a group-scoped attribute's column family, a
  layer over a group, and the paged permutation (spec §8) — the flat array is what a sparse view
  is written as.
- **r7 (2026-08-30)** — spec §2's extent move is implemented, and the marker inverted: the
  bundle-level `Manifest.quantisation` is deleted, `ViewDescriptor` and each `/v1/meta` `views`
  entry carry the frame, and a manifest whose view omits it refuses at open with no fallback
  (contracts r52; decision 0040's ruling, unchanged since 2026-08-02). `bundle_format` did not
  move, by owner direction. What the marker now says is what did *not* move: a shape publication
  still canonicalises a layer's views against the first one's frame, which
  `polygon-membership.md` §4.3 wants per view and which waits on a bundle carrying two. No
  design content changed in this revision.
- **r8 (2026-08-30)** — decision 0112 recorded (the anchor view orders a signature group's
  ids); the extent examples corrected to the surface's own spelling and the `members`-group
  roster contradiction settled the parser's way, both found by the build-surface implementation.
- **r6 (2026-08-30)** — the two-lens review. Security found one fail-open (the pinned leaf and
  `filter_operands` escaping the gate — closed, spec §5), one disclosure (the ordinal gap —
  accepted as a register row, spec §9), and the session/creation contradiction (ruled:
  re-authorisation, spec §6, with roster immutability); it confirmed the intersection-semantics
  gate, the join rule's byte-match arms, I7/0008 and I10 under attack. Implementability found
  the join rule missing the commit window (fixed, spec §4), the allocation key ungrounded over
  several sources (now a boxed proposal, spec §7), the roster with no durable home (fixed —
  segments manifest, spec §3.2), the id↔path mapping unspecified (fixed, spec §3.2), the
  keyless view (removed: key required), the metadata-name collision (reserved names, spec §3.2)
  and the unassigned charset refusal (assigned, spec §3.2). Promoted to Normative on
  disposition.
- **r5 (2026-08-30)** — rewritten against the built system. Views separated from signature
  grouping; view groups with two roster forms, shared views, attribute and layer scope added; the ingest map withdrawn in favour of
  per-batch addressing; runtime creation of plain views withdrawn; identity tiers and roll-mode
  rotation moved out. Not yet reviewed.
- **r1–r4 (2026-08-01 → 2026-08-18)** — three independent reviews (performance, security,
  maintainability) of the joint views-and-tables design; the accepted findings are carried where
  they survive (the gate's intersection semantics, the mandatory idset on writes, the permutation
  budget) and the rest is in git at `ead7e906`. r4 was the slice→view rename.
