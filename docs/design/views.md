# Views — design

**Status:** Normative (r28) — **the drill-down is the whole permitted picture of one point**
(r28, 2026-09-01, owner ruling; `contracts.md` §3.2 r69). `POST /v1/items/{tessera_id}` names the
views the item is in **that the principal may reach**, each with the position that view places it
at, and carries the group-scoped attribute values keyed by the **group's key** — the key being a
view's only address (decision 0113), so two views sharing a key through a `members` group are one
entry. Both are gate-filtered against the session's own visible-view set (§6), so a gate-failed
view is named nowhere and a point held only in such views serves exactly what it served before.
§5's declaration with neither `index` nor `render` acquires its meaning here — stored, served at
the drill-down, on no filter surface and in no row tail — and only a `text` family is absent,
having no per-entity value slot. §9 gains the membership row's second half; no new register row,
C30 widening to cover it. The paragraph in §4 that said which views an entity is in "is not
served" is superseded.
**Date:** 2026-09-01
**Status:** Normative (r27) — **a scoped value's address is `(attribute → its group, key)`, and the
join rule is decided on the serial writer** (r27, 2026-09-01, owner rulings;
[decision 0116](../decisions/0116-a-scoped-values-address-is-the-attribute-and-the-key.md);
`contracts.md` §3.1 r68). Two changes, one section each. §5's write half admitted a scoped family's
columns only on a batch whose view the **owning** group holds; that one-door rule is withdrawn, and
with it the "two extents claiming one entity" argument that justified it. A batch whose view's key
belongs to the attribute's group's key set — through the owner or through any group declaring
`members` of it — may carry the family's columns, and the value lands in the one
`(entity, attribute, key)` cell whichever door it came through. A second row naming a cell that
already holds the same value is deduped, so there is still exactly one claimant; a differing value
is a 409 naming the column and the key. An unrelated view refuses exactly as before. §4's label and
attribute arms **move out of the request handler and onto the serial writer**, where
`established_collisions` settles which rows are joins: one authoritative site instead of two, and
the race a row promoted to a join between the two sites used to win — skipping both arms — is
closed. The caller still receives the 409 synchronously, before the WAL append, so a refused batch
leaves no record.

*(Revision numbering: concurrent branches may renumber this at merge.)*

**Status:** Normative (r26) — **`render` alone makes a group-scoped family a filter operand**
(r26, 2026-08-31, owner ruling; `contracts.md` §3.2 r67). §5's last standing restriction goes: the
licence is `index` **or** `render`, which is what an entity-scoped column has, and the asymmetry
between the two was a hole rather than a rule. `render` licences the family's **entity-space**
per-view column — written by every build whatever the flags — and not the row lane, so a pin reads
the named view's column exactly as an indexed family's does and no route answers a second question
under one spelling. A rendered **category** is on `/v1/categories` by the same admission and owes
its per-view postings; a rendered `text` family does not exist, `render` on one being refused at
the declaration. The collapse stays one site, evaluation and the masking are unchanged, and no
artefact format moves — what a rendered family now writes is what an indexed one already wrote.
**Status:** Normative (r25) — **the write half of §5 is built: ingest carries a scoped value,
flush writes the column, and merge and fold keep the lane** (r25, 2026-08-31, owner ruling;
`contracts.md` §2.2/§3.1 r66). §5's three ⊘ markers are discharged. A batch into a view of a group
may carry that group's scoped columns under their plain names; the flush writes the family's
per-view extents beside the build's, and the base a view created since the build has none of; and
a merge or a fold takes the **view's** writer schema rather than the bundle's, which was a defect
rather than an absence — a rewritten segment of a group's view dropped the family's lane, and
values served correctly before the rewrite came back as zeros. A view created while the service
runs therefore gains its scoped columns at its first flush, with no rebuild. One thing stays
marked, narrow: a batch into a view of a group that only *shares* another's views may not
carry the family — the column is the owner's, and a second writer for one `(entity, view)` column
is two layers claiming one entity. (§4's attribute arm went at r24, merged beside this.) No
design changes.
**Status:** Normative (r24) — **§4's join rule refuses a changed entity-scoped attribute value past
the entity's own flush** (r24, 2026-08-31; `contracts.md` §3.4 r65). §4's last ⊘ is discharged, and
with it the section's last one: the value is read back from the home its declaration gives it — the
entity-space value column, the record blob, or the hot column — and a joining batch carrying a
different one is the `409` naming the column that the rule always specified. Two things the marker
had wrong are recorded at the claim: the read is one per column rather than an oracle per family,
and the accepted row was *not* inert for a rendered column, whose value travels in the joining
row's own tail. That second point carries a **new rule** (owner ruling): a join that omits a
`render` column's value takes the entity's stored one rather than writing an absence into the
joined view's tail, so an entity-scoped attribute reads identically in every view that shows it.
One refusal is withdrawn as well — a joining batch that leaves a **category** null is a join,
absence being the reserved code rather than a null cell. No other design content moves.
**Status:** Normative (r23) — **§4's join rule refuses a re-label past the entity's own flush**
(r23, 2026-08-31; `contracts.md` §2.4 r63,
[decision 0114](../decisions/0114-the-drill-down-serves-the-satisfied-labels-only.md)). The label
arm's ⊘ is discharged: the bundle now carries the entity→term transpose the marker said did not
exist, so the arm compares against the buffer and then against the transpose, and a second view's
row naming a different label is the `409` the rule always specified rather than an accepted, inert
row. The attribute half stays marked, with the cost of closing it stated. Nothing else moves.
**Status:** Normative (r22) — **`render` on a group-scoped attribute reaches the row tail of each
view of its group** (r22, 2026-08-31, owner ruling; contracts r62): the value is carried in the
points batch of every view of the group, and of any group sharing those views via `members`, and
of no other view — the rule `per-point-attributes.md` §3.9 has for `render_in`, with the view set
decided by the scope. `/v1/meta` gains `scoped_scalars`, a family's counterpart to
`declared_scalars`, and the vocabulary and analyser move onto it from the operand entry. §5's
`render` marker is discharged; the **ingest** half stays marked, and a view created while the
service runs has no column of any family until a rebuild. No design changes.
**Status:** Normative (r21) — **a fold after a drop is tested, and it did not work** (r21,
2026-08-31): §3.4's last ⊘ is discharged, and discharging it found the mechanism it described to be
half-built. The drop retained the view out of the bundle, so the fold planned no base for it — and
the fold's *publication* then carried its segments forward from the live side-manifest, found no
base for them and discarded the fold, every time, so a bundle a view had ever been dropped from
compacted no further and retired nothing. The carry-forward now omits a segment whose view the
bundle no longer declares. No design content changed: §3.4 said reclamation is by omission, and it
now is. **Status:** Normative (r20) — **every family of a group-scoped attribute answers, and such an
attribute may declare its own `source`** (r20, 2026-08-31, owner ruling; contracts r60): §5's two
⊘ markers are discharged. A scoped **category** carries per-view keyed postings — the filter route,
and what `/v1/categories` derives a per-view value list from — and a scoped **text** column carries
a per-view token dictionary and positional postings and no value column at all; a scoped `text`
column now requires `index = true`, the record blob having no slot for a family. An attribute's own
`source` carries one row per `(entity, view)`, routed by `fields.view`. No design changes: the
markers do, and `render` and the ingest half stay marked. **Status:** Normative (r19) — **first-batch-creates is withdrawn** (r19, 2026-08-31, owner
ruling): a batch naming an unknown key is a 404 for **every** group, record-less ones included;
creation is `PUT /control/views/{group}/{key}` and nothing else. **a group with no roster mints
its views** (r18, 2026-08-31, owner
ruling: implement it): a `[[view_group]]` declaring neither roster form takes the distinct values
of its discriminator column as its keys, one view per value, sorted by key bytes. The minted
records are ordinary roster records, so nothing above the mint can tell them from written ones;
§3.1 carries the four rules the declaration does not settle. **a shape layer canonicalises per
view** (r17, 2026-08-31; spec §2's marker,
[decision 0111](../decisions/0111-a-shape-spans-projected-views-through-wgs84.md)): the
three consumers that read one frame for a layer's several views each take a frame per view, the two
spans 0111 refuses are refused, and `test_corpora/multiview` carries a layer over two frames.
**Ordinals are removed** (r16, 2026-08-31, owner ruling): a view of a
group is addressed `<group>:<key>` and by nothing else. There is no ordinal on the wire, in a
roster record, in a WAL record or in the manifest, and the `#` form addresses nothing; a caller
wanting a numeric ordering mints numeric keys. Views of a group are served in **creation order**,
which is roster-record order, so the ordering a client walks survives without a stored number. Key
tombstones are unchanged — a dropped key is refused for ever. Appendix C's C27 (the ordinal gap) is
deleted and [decision 0110](../decisions/0110-the-ordinal-gap-is-accepted.md) is superseded by
[0113](../decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md): with no ordinal
served there is no gap to observe. **The gate is built** (r15, 2026-08-31): spec §6 is end to end.
`visibility` is accepted on a view, a group and a roster record, checked at acceptance against the
plugin that will evaluate it, and stored on the manifest; a session's **visible-view set** is
resolved once at authorise — every view of every group, whatever the outcome, by intersection of
the label's term set with the principal's, the group's gate conjunctive with the view's own — and
is fixed for the session's life. Every view-valued serving surface is filtered by it: `/v1/meta`'s
`views` and `groups`, a gate-failed group taking its whole roster; each served layer's `views`;
`filter_operands`' scoped entries and the filter leaf that names one, bare or pinned, which
collapses to the unknown-column `422` (spec §5); and both viewer verbs that name a view, where a
gate-failed name is the *same* 404 a never-declared one gets, from the same site, at the same cost
— one set-membership lookup, made on both outcomes. The control plane is not gated. Two things
the gate deliberately does not do, both ruled: a view created after a session authorised is a 404
to it until re-authorisation, and a filtered roster is a shorter list with nothing to count the
withheld views by (spec §9, decision 0113). **a group grows while the service runs** (r14, 2026-08-31):
`PUT /control/views/{group}/{key}` creates a view of a declared group and `DELETE` drops it,
burning the key; the roster's durable home is the segments manifest, carried
forward for ever as `layer_tombstones` is; a created view answers a viewer verb empty and takes
its first row space at the next flush; and a known `external_id` naming a view the entity is not
in is a **join** (spec §4), the row landing in that view with the entity, its label and its
attributes untouched. `delete_dangling` is built, as sugar over the deny lane. Spec §3.2's and
§4's markers say what remains inside what is built. **a group-scoped attribute answers filters** (r13, 2026-08-31):
spec §5's evaluation rule is built end to end — the family is recorded in the manifest beside the
roster, `/v1/meta`'s `filter_operands` carries its scope, and a leaf resolves to one view's column
by the request's own view or by a pin, `name@key`, with a `422` naming the group where
nothing decides and the unknown-view `404` for a pin naming no view of it (contracts §2.2, §3.2
r55). **All four families answer, and a scoped attribute may declare its own `source`** (r20,
2026-08-31, owner ruling; contracts r60): a category's per-view postings and a text column's
per-view dictionary and postings are written and opened, `/v1/categories` is view-addressed for a
scoped category, and a source of the attribute's own is routed per view by `fields.view`.
**`render` reaches the row tail of each view of the group** (r22, 2026-08-31, owner ruling;
contracts r62), and `/v1/meta`'s `scoped_scalars` publishes the placement. ⊘ What remains at that
claim is the ingest half.
**the roster is served** (r12, 2026-08-31): `/v1/meta` publishes
every view of every group with its key and typed metadata, in creation order, beside the
groups' own orderings; every one of them answers a viewer verb, by its key
(contracts §3.2 r54). What is still unbuilt above the build is creation and drop while the service
runs, and the gate. **the permutation is paged** (r11, 2026-08-31): spec §8's
representation ruling is built, for every view, and its marker is discharged; contracts r53 carries
the encoding. **The build is complete for a declaration** (r10, 2026-08-31):
every plain view and every view of every group, whichever roster form declared it, over one entity
space unioned from their sources and ordered by the declared anchor (decision 0112); a group's
points selected out of a shared file by `fields.view`; one frame per group under `auto`; a
group-scoped attribute's column family on disc; and a layer drawn on a group or scoped to one.
What remained above the build at r10 was the ingest join (spec §4, built at r13), the gate
(spec §6) and the serving surfaces a scoped column and a scoped layer will need; the markers at
spec §1, §5, §7 and §8 say which.
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
> by its key (spec §3.2, contracts §3.2 r53).
>
> **The write half is built too** (2026-08-31): a view of a group is created and dropped while
> the service runs (spec §3.2, §3.4), the roster's durable home is the segments manifest, a
> created view serves empty and takes its first row space at the next flush, and the second-view
> join at ingest is the rule spec §4 states — its arms refusing, its joining row carrying geometry
> and nothing else.
>
> **A group with no roster at all builds too** (2026-08-31): its keys are minted from the distinct
> values of the discriminator, and everything above the mint sees the roster a table would have
> given it (spec §3.1, §7). What remains is the serving surfaces a scoped **layer** would need. A
> scoped **attribute** is served: since 2026-08-31 a numeric or keyword family is a filter operand
> carrying its scope, and a leaf reads the view the request names or the one it pins (spec §5,
> contracts §3.2 r55) — what remains of it is a category or text family, and `render`, which spec
> §5's marker states. Each refuses or is absent by name.

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
> **Implemented 2026-08-31 — a shape layer canonicalises per view, in each view's own frame**
> ([decision 0111](../decisions/0111-a-shape-spans-projected-views-through-wgs84.md)). The three
> consumers that read one frame for a layer's several views now each take a frame per view:
> `canonical_shapes` takes a `ViewFrame` — view id, projection, extent — per view rather than one
> projection and one extent for all of them; `/control/layers` resolves one per view of the layer;
> and the build's layer read passes every view it materialises rather than the anchor's frame. A
> layer whose views share a frame — every view of a group (spec §3.1) — produces identical bytes
> under each name and pays only the repeated canonicalisation; one spanning frames produces a
> genuinely different canonical form per view, which is the semantics `polygon-membership.md` §4.3
> states. Two spans are refused: a layer mixing a projected view with a `projection = "none"` one,
> at the declaration on both entry points (the build's config parse and `PUT /control/layers`),
> naming the layer and both sides; and a `space = "view"` row over frames that are not identical,
> at the row, since the space is a fact about the submission. A shape wholly outside a view's
> extent warns with the count for **that view** and is published. `test_corpora/multiview` now
> carries the case: `regions`, three `wgs84` polygons over `world` (Web Mercator) and `world_flat`
> (equirectangular), decomposing differently in each.

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

**A group declaring neither form mints its views from the data**: the keys are the distinct values
of `fields.view` in the group's own source, one view per value. A group whose views carry no
metadata and no gate of their own is exactly the group with nothing to write a roster about. Four
rules the declaration does not settle are architect's choices (2026-08-31), and they are
**recoverable defaults** rather than invariants — each may be reruled without a migration, the
artifacts being rebuilt:

- **The order is the keys' own bytes**, sorted, rather than the order the values appear in the
  source. Roster order is served order (§3.2, decision 0113), so appearance order would make what a
  client walks a property of how the source's row groups happen to be arranged, and two builds of
  one corpus could serve one group's views in two orders.
- **A distinct value outside the key charset refuses the build**, naming the value and the column.
  Not skipped: a value no view was minted for is one whose rows belong to no view, which is the
  refusal a stray key already earns under form B. Not rewritten into a legal key either, which
  would serve a view under a name nobody wrote.
- **`metadata` is refused at parse**, because there is no roster record for a per-view value to sit
  on and nothing else could supply one. A group wanting metadata writes a roster.
- **Every minted view takes the group's own `visibility`.** A narrower per-view gate is a roster
  record's field, and there are no roster records to carry one.

**A source with no rows mints no views and is refused, naming the group** — what an empty roster
table earns and for the same reason: a group with no views is a declaration promising coordinate
systems the bundle would not carry. A null in the discriminator is refused on the same read, a row
that names no view being in no view.

`source` and `fields` keep `configuration.md` §8's rule: the map says where, never whether.
Under form B the located fields are the view's own — `entity_id`, the coordinates,
`point_visibility.field` — plus `view`; under a roster table they are `key`, `visibility` and the
metadata names, all defaulting to their own names; each view's own gate is the roster's
`visibility`, and a view carrying none takes the group's.

A group is not a view: it cannot be named on a viewer verb, has no row space and no permutation.
Its views are views in every respect below the declaration — each with its own Morton order,
permutation, segments, extents and θ — and they are what a request names.

### 3.2 Keys

A view of a group is addressed as **`<group>:<key>`**, and by nothing else (owner ruling
2026-08-31, [decision 0113](../decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md)).
The key is the caller's, **required at creation**, under the column-name charset (ASCII letters,
digits, `_`, `-`); a caller who wants a numeric ordering mints numeric keys. That form is the view
id wherever a view id goes: the request body, `x-tessera-view`, `/v1/meta`, the manifest. On disc
the view lives at `views/<group>/<key>/`, nested rather than the joined id because `:` is not a
path character everywhere; `SegmentDescriptor.view` and `WalRow.view` hold the joined
`group:key` form, one named function derives the two-component path from it, and the manifest's
`files` map is keyed by the derived path. `:` and `@` are reserved out of plain view names and
keys, refused at the configuration parser and again at manifest load.

Views are served in **creation order**: `/v1/meta` lists a group with its views, each
`{ key, metadata }`, in the order the roster records are in, so a client can offer
previous-and-next without interpreting keys and without a number to sort by. Roster records are
append-ordered and never rewritten, so the order is the records' own and nothing stores it twice.
Creation order is arrival order and nothing else; a caller ingesting quarters out of order gets
them in arrival order and sorts by `starts` if it wants time order.

**A view is created ahead of the rows that name it**, by `PUT /control/views/{group}/{key}`
carrying the roster record — `visibility` and the metadata — which is the inline
`[[view_group.view]]` block as a request. **A roster record is immutable**: a wrong gate or
wrong metadata is a drop and a recreate under a new key, never an update — the alternative is a
narrowed gate that does not bite live sessions, a staleness the deny lane is not allowed and
the roster is not either. A batch naming a view that does not exist is a 404, as for any
unknown view, **for every group — a record-less one included** (owner ruling 2026-08-31, r19).
An earlier revision excepted the group whose views carry nothing, letting the first batch naming
a new key create it; that route is withdrawn. Creation stays explicit because the cost runs one
way: a typo in a key must be a refusal, never a freshly minted view collecting the mistyped rows.

**The roster's durable home is the segments manifest, not the WAL.** The create and drop
records are WAL entries for replay, and the served roster is the manifest's plus the WAL
overlay — but WAL rotation reclaims records, so the roster and the tombstoned keys are published
into the segments manifest at every flush and carried forward for ever, exactly as
`entity_id_low_water` and `layer_tombstones` are and for the same reason: a mark that lives only
in the log is lost at the first rotation, and a reused key silently repoints every client cache
keyed on the view (decision 0029).

**Metadata names are bounded by the roster's own keys**: `key`, `source`, `visibility` and, on
a form B group, the discriminator's field name are refused as metadata names — the inline block
and the roster table would otherwise be ambiguous. The form A block mixes closed keys with the
declared metadata names, so it is parsed by the manual route the `extent` spellings already
take rather than by `deny_unknown_fields` alone; the discipline's guarantee — an unknown key is
refused — is preserved by checking against the declared set.

> **Implemented 2026-08-31** (contracts §3.4 r55). `PUT /control/views/{group}/{key}` takes the
> roster record — `visibility` and the declared metadata, typed — and creates the view: the key's
> charset is checked, an existing or tombstoned key is a `409`, an unknown group and a group that
> takes another's views are a `404` and a `422`, and a `visibility` that is not `public` is
> refused for spec §6's reason. The record is a WAL entry (`ViewCreate`) replayed before
> any row referencing it, and **the roster's durable home is the segments manifest**:
> `SegmentsManifest.views` and `view_tombstones` are published at every flush and every deny
> publication and carried forward for ever, exactly as `layer_tombstones` is; the served roster is
> the build's plus those, with the WAL's own records replayed on top at open. Creating a key on
> the owner creates it, empty, on every group sharing its views (spec §3.3).
>
> A created view **is a view from the acknowledgement**: it is in `/v1/meta` with its record, it
> answers a viewer verb with an empty result — a view with no files now has an empty row space
> rather than being absent from the bundle, which is what a 404 and a missing deny-mask entry used
> to be made of — and its first flush gives it a row space, every segment of it an extent over
> that empty base.
>
> ⊘ **One thing this does not do.** A group declaring a **category**-typed metadata name has
> no create that can satisfy it: the wire carries a key and nothing resolves it to a code here, so
> such a create is refused by the type check rather than accepted with an unresolved value — no
> declaration in the corpus uses one.

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

Keys, metadata and each view's own gate belong to the group that owns them, and a
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

> **Implemented 2026-08-31** (contracts §3.4 r55). `DELETE /control/views/{group}/{key}` appends a
> `ViewDrop` record carrying the key, and the view leaves `/v1/meta` and every group sharing it at
> the acknowledgement; a request naming it is the same 404 as one that never existed, and the key
> is refused for ever. Any view of a group may be dropped, a declared one included.
> The rows the buffer held for it are discarded with it: they name a coordinate system that no
> longer exists, so nothing would ever give them geometry, and a row left in the buffer for a view
> no flush will plan pins the WAL's reclaim bound for the life of the process. Their **entities**
> are untouched, which is what this section says a drop produces.
>
> `delete_dangling` is built as the sugar defined above: the probe walks the dropped view's rows —
> inverting its row space where it can, and asking entity space where a built view published no
> `row-entity.u32` — unions the buffer's rows for that view, and submits the entities that hold a
> row nowhere else as ordinary `Delete` records on the deny lane. The probe and the submission are
> one step on the write executor, so no acked batch can interleave between them; the count is in
> the acknowledgement.
>
> **Row-space reclamation is the fold's, and a fold after a drop is tested** (2026-08-31,
> `tests/views_write.rs`). The dropped view is absent from the bundle the fold plans over, so it
> has no base in the plan and no segment in the new prefix, and the superseded prefix — files and
> all — is reclaimed when its last reader lets go, the startup sweep taking any that stands:
> reclamation by omission rather than by a sweep. Until a fold runs its files stay on disc, named
> by a side-manifest and reachable by nothing: the view is not in the manifest a request resolves
> against, and a reopened bundle re-applies the tombstone before it serves anything.
>
> **The test found the omission incomplete, and it is fixed.** The drop retains the view out of the
> *bundle* but leaves its `SegmentDescriptor`s in the live side-manifest, so the fold's publication
> carried them forward, found no base for a view its plan did not have, and **discarded the whole
> fold** — every fold after a drop of a view that held rows, for the life of the bundle, so
> nothing retired either. The carry-forward now omits a segment whose view the bundle no longer
> declares, which is the same omission the plan already made.

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
  is expected, because that is the value this view carries — subject to §5's cell rule, which is
  the same rule asked of the `(entity, attribute, key)` cell rather than of the entity.
- **A deleted holder is not a duplicate**, as today: the re-ingest allocates fresh.

**Every arm above is decided on the serial writer** (2026-09-01, decision 0116). The duplicate
answer — the one refusal that may *name* the caller's own ids — is taken in the request handler and
is advisory; whether a row **is** a join, and what a join may carry, is settled once, beside the
live map the apply will clone from. The two sites this replaces disagreed by a whole queue drain,
and a row promoted to a join in between met no arm at all. The refusal reaches the caller
synchronously and before the WAL append, so a refused batch leaves no record, spends no entity id
and moves nothing.

The identifier forms are r4's, kept: `external_id` is canonical; `tessera_id` is accepted with a
**mandatory** idset beside it, a retained idset translating exactly and a revoked or unknown one a
409. Contracts §2.2's argument for an optional idset on reads does not transfer to a write — a
stale identifier on a read misresolves one bounded answer; on a write it silently names another
entity.

Which views an entity is in is stored nowhere but the permutations, and **is served for the views
the asking principal may reach** *(r27, owner ruling 2026-09-01; contracts §3.2 r68)*:
`POST /v1/items/{tessera_id}` names them, each with that view's own position for the item. It
discloses nothing the viewport does not, because membership in a reachable view *is*
`mask ∩ members(view)` — the equality the multi-view differential asserts — and every view outside
the gate is absent from the array exactly as a view nobody declared is. What stays closed is the
other direction: a point's absence from a view a principal *can* reach is still indistinguishable
from its invisibility there, and a gate-failed view is named on no surface at all.

> **Implemented 2026-08-31** (contracts §3.4 r55). `/control/ingest`'s duplicate check is this
> rule: a known `external_id` naming a view the entity is not in is accepted, and the row lands in
> that view's pending segment carrying the entity it joins. "In the view" is the view's
> permutation **union** the commit window's buffer, at the handler and again at the executor's
> apply-adjacent backstop, which reads the live map beside the generation its apply will clone
> from. Each other arm refuses loudly: already in the named view is a 409 naming the ids, a
> different label on a known id is a 409, and an entity-scoped value that neither matches nor is
> null is a 409 naming the column. A suppressed holder takes the same arms and stays hidden; a
> deleted one allocates fresh, as decision 0047 requires.
>
> **A joining row carries geometry and nothing else, structurally.** It arrives with its entity
> already decided, so the allocation is sized by the rows that need an id; it carries no
> descriptors, so it promotes nothing into the dictionary and contributes no postings; and the
> flush's entity-space passes — postings, attribute columns, prose, record fields — skip it, so it
> writes no value for any of them. That is what makes a label supplied on a second view's row
> *inert* rather than a widening with no overlay entry.
>
> **The label arm is exact past the flush** (2026-08-31, `contracts.md` §2.4 r61,
> [decision 0114](../decisions/0114-the-drill-down-serves-the-satisfied-labels-only.md)). The
> entity→label oracle this marker said did not exist now does: the bundle carries the postings'
> transpose, `entities/terms/`, written by the build and by every flush. So the arm compares
> against the buffer while the entity's own row is still there and against the transpose
> afterwards, and a join naming an already-flushed entity with a *different* label is the `409`
> above rather than an accepted, inert row. A novel descriptor resolves to a process-local
> extension id, which no stored ordinal can equal — so a batch naming a label this deployment has
> never interned is a mismatch, which is right: the flushed entity cannot be carrying it. The read
> is the **full** set and is server-side; the drill-down's `labels` array over the same artefact
> serves the *intersection* with the asking session, which is a different question with a different
> answer.
>
> **The attribute arm is exact past the flush** (2026-08-31, `contracts.md` §3.4 r65). This marker
> said the read-back would be a second value oracle across every declared family; it is one read
> per column, because a declared column's value already has exactly three homes and the
> declaration says which (records §3, decision 0068) — the entity-space **value column** where the
> column owes one, the **record blob** where it is blob-resident, and the **hot column** where
> `render = true` is the value's only store. `session::flushed_scalar_of` reads the one that applies
> and normalises it to the shape a batch carries, so the flushed arm and the buffered arm make the
> same comparison and produce the same refusal, byte for byte.
>
> **And the third home was never inert.** A joining row is geometry-only in *entity* space — no
> postings, no attribute column, no record field — but its scalars still travel in its own row
> tail, so a rendered column's differing value was being written into the joined view's hot column
> and read back there. For that home the marker was losing the rule, not only the report.
>
> **An omitted value is backfilled into the joined view's tail, not written there as an absence**
> (owner ruling, 2026-08-31). A joining row is geometry-only in *entity* space, and its scalars
> still travel in its own row tail — so a batch that lawfully omitted a `render` column's value
> would leave the joined view rendering nothing for a point every other view renders a value for,
> and an entity-scoped attribute that reads differently under two views is not the one value per
> entity §5 says it is. The row takes the entity's stored value instead, read by the same oracle
> the comparison uses (the buffer where the entity's own row is still there, the stored homes
> after). A column the entity genuinely holds **nothing** for is untouched, and its absence stays
> an absence in every view.
>
> It is applied on the **write executor**, where join-ness is settled — the apply-adjacent backstop
> is what finally decides which rows join and which allocate fresh, and a row that stops being a
> join must not carry a value taken from an entity it turned out not to be joining. That is also
> before the log append, so the value the log carries is the value the flush writes and replay
> reproduces it rather than re-deriving it against whatever the bundle holds by then.
>
> **Absence is one question, asked of both sides.** A joining row that omits a value never
> disagrees with a stored one, and a stored *absence* is never something a supplied value can
> contradict — the arm is one-directional, guarding against a *change*. It is asked in two
> spellings because a category's absence is in band: `null` for every other family, and the
> reserved code `0` for a category (per-point-attributes §3.4), which is what a null category cell
> has already become by the time this arm runs. Reading only the `null` spelling made a batch that
> left a category null a `409` against an entity holding a value — a refusal the rule does not ask
> for, and one the buffered arm was making before this revision.
>
> **A store that cannot be read loses the refusal, not the batch.** An unreadable transpose,
> record blob or segment set is warned — naming the artefact and never the entity (**I10**) — and
> the join is then accepted exactly as it was before either oracle existed. That is the
> recoverable-and-discloses-nothing side of the line: a corrupt artefact must not turn a caller's
> write into a server error. A view whose segment set will not resolve is *skipped*, not taken as
> an answer: the hot-column read scans every view the entity has a row in and returns the first
> **present** value it finds, absence never pre-empting a value. One lawful disagreement survives
> the backfill — an entity holding nothing for a column in the view it was ingested into, joined
> into a second view *with* a value, there having been nothing to backfill from — and reading the
> first view's absence as the answer made the third view's join accept or refuse by hash order.
>
> A **group-scoped** attribute is refused on every batch by construction rather than by this rule:
> a scoped column is deliberately absent from `MANIFEST.declared_scalars` (spec §5), so a column
> named for one is an undeclared column, which ingest already refuses naming it.

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
  **pin** a view: `sentiment@2026-Q3`, by key. That is an ordinary
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
source is a `[[view_group.view]]` file, `fields.view` says which view each row's value is for
(built at r17; `fields.view` defaults to `view`). A
batch into a plain view may not carry a group-scoped attribute at all: there is no view of the
group for the value to belong to.

A batch into a view of a group that declares `members` **may** carry one (2026-09-01, decision
0116): the address of a scoped value is `(attribute → its group, key)` and never the view, so the
key a sharing group's view holds — the owner's by construction (§3.3) — is the same cell the
owner's own view addresses, and either door writes it. What decides admission is therefore the key,
not the spelling: a view whose key is in the attribute's group's key set may name the family's
columns, and one whose key is not takes the undeclared-column refusal whatever it is called.

The rule that keeps one cell single-valued is a **comparison, not a door**. A row naming a cell the
deployment already holds a value for — put there through either door, in this window or an earlier
one — is deduped where the value agrees and refused with a 409 naming the column and the key where
it does not. One claimant per cell, so the extents stay disjoint in entity space; the "two layers
claiming one entity" argument that carried the old one-door rule is dissolved rather than
overridden.

**A `text` family past a flush is refused rather than compared.** Its column stores a token
dictionary, positional postings and a presence bitmap and no value per entity, so there is nothing
to compare a supplied string against once the cell has flushed — and admitting it would write a
second text layer stamped with the same view, which `match` unions across with no symptom. The arm
therefore asks occupancy: a cell that already holds prose takes no second value, equal or not,
equality being what cannot be established. Omitting the column passes; within one window the buffer
holds the value and text compares exactly. ⊘ The build's **base** writes no presence file
([#123](https://github.com/jennis0/tessera-index/issues/123)), so a cell whose only prose came from
the build reads as unoccupied and is not covered by this refusal.

**Render.** A `render = true` group-scoped attribute is rendered in the views of its group and
of any group sharing them, and in no other view — the rule `per-point-attributes.md` §3.9 already
has for `render_in`, with the view set decided by the scope instead of listed.

> **Implemented 2026-08-31** (contracts §3.2 r61). The build permutes each view's column into that
> view's row space beside the entity-scoped render columns, with its own presence bitmap, so the
> value arrives in the points batch under a view of the group and the column is not in the schema
> anywhere else — a view outside every scope writes the bytes it wrote before the family was
> declared. A request's render list is therefore per view, and `/v1/meta`'s `scoped_scalars` entry
> publishes the family's `render` flag and **every view id whose rows carry the column and that
> this principal may reach** — the owning group's and every sharing group's alike, since a client
> under a shared view receives the column under an id the family's own list does not name. The
> gate is inside this as it is inside the filter surface: a family whose group this principal
> cannot reach is named in no response they receive, and a view they cannot reach is on no list.
>
> **`tessera verify --deep` checks the lane per (build segment, view)**, because its absence is the
> one defect here that is silent: a segment with no column of the family is served as a row with no
> value, which is exactly what a flushed segment legitimately is. Only the build's own segments are
> checked — a segment named in `MANIFEST.files` — so a bundle that has ingested is not refused for
> the write half's deliberate absence.
>
> **A rendered family is a filter operand, built 2026-08-31** (owner ruling; contracts §3.2 r67).
> `index` **or** `render` puts a family on the filter surface, which is the licence
> `filter-index.md` gives an entity-scoped column and the asymmetry between the two is closed. What
> `render` licences here is the **entity-space column**, not the lane: a family's per-view column is
> written whatever its flags — the build has always written it — so a rendered family is answered by
> the ordinary scan over the resolved column, and a **pin** therefore works, reading another view's
> column where it lives rather than from rows the request does not hold. The lane answers no filter
> at all; the operators are the family's own, since the route the value column takes is not a
> surface. The one exclusion is `text`, which owes no value column and whose `render` is refused at
> the declaration anyway. What a rendered family gains on disc is what an indexed one already pays:
> the flush's per-view extents and the empty base of a view created since the build, and for a
> **category** the keyed postings both an `eq` and `/v1/categories` are answered from — one
> admission decides both surfaces, so a rendered category earns its value list too.
>
> **A merge and a fold keep the lane, built 2026-08-31 — and this was a defect, not an absence.**
> Both took their writer schema from the bundle-wide render list, a family having no row in it, so
> a rewritten segment of a group's view carried the entity-scoped tail alone: values served
> correctly before the rewrite came back as the type's zero afterwards, indistinguishable from
> absence and with no error anywhere. The schema is now the **view's** — the bundle-wide render
> tail, then that view's scoped render lanes, one derivation shared with the flush — and the
> fold's attribute pass folds each family's per-view column exactly as it folds an entity-scoped
> one, which it previously did not do at all: a fold wrote a prefix the families' directories were
> simply not in.

**View metadata is not an attribute.** A view's `label` or `starts` is one value per view, lives
on the roster, filters nothing and is served typed on `/v1/meta`. A per-(entity, view) value is
an attribute. The two are kept apart so that neither grows the other's surface.

> **Implemented 2026-08-31 — the filter surface, end to end** (contracts §2.2, §3.2 r55).
> `scope` parses; a build writes one entity-space column per view of the group, each with its own
> presence bitmap, at `attrs/<column>/<group>/<key>/`, read from that view's own points — under the
> view's own selection where a group's views share one file — and every file is digested, so
> `tessera verify` walks them. The family's record is `MANIFEST.groups[..].scoped_scalars`, beside
> the keys a pin resolves against, and deliberately **not** `MANIFEST.declared_scalars`, which
> is one flat bundle-wide list with no slot for a family. From there the engine opens one column
> per view; `/v1/meta`'s `filter_operands` entry carries the scope; and a leaf resolves exactly as
> this section says — the request's own view under a view of the group or of a group sharing them,
> a pin by key anywhere else, a `422` naming the group where nothing decides, and the
> unknown-view `404` for a pin naming no view of it. Evaluation is the family's ordinary one over
> the resolved column: a value scan under the candidate, the presence bitmap for absence, an entity
> bitmap the mask meets before any count.
>
> **The scoped surface is inside the gate, built 2026-08-31** (spec §6). A family whose group this
> principal's gate fails is **undeclared**: `filter_operands` omits its entry — the only place the
> document names a group — and a leaf naming it, bare or pinned, takes the ordinary unknown-column
> `422`, never the `422` that names the group nor the `404` that would confirm the key space. The
> check is one site, ahead of the pin/bare split, so the two spellings cannot diverge; a pin under
> a group that *does* pass resolves through the session's visible-view set, so a pin naming a view
> this principal may not reach is the same `404` a key no view holds gets.
>
> **Every family is served, built 2026-08-31** (contracts §2.2, §3.2 r59). What a view's directory
> holds is what the family's entity-scoped counterpart holds bundle-wide: values and presence for a
> number, those and a dictionary for a keyword, those and the keyed `postings.arrow` an `eq` or an
> `in` is answered from for a **category**, and for **text** a token dictionary and positional
> postings with no value column at all. Each is written by the writer the entity-scoped pass calls,
> pointed at `attrs/<column>/<group>/<key>/`, and opened by the opener that reads it. A scoped
> `text` column is **refused without `index = true`**: the record blob is bundle-wide and addressed
> by a column's position in `declared_scalars`, which a family has none of, so the token index is
> the only home its prose has — and by the same absence a scoped text value is returned by no
> drill-down, which is the one thing its entity-scoped counterpart does that this one cannot. Every
> **other** family is returned by the drill-down, keyed by the group's key *(r27, owner ruling
> 2026-09-01; contracts §3.2 r68)*, and that is what a declaration with neither `index` nor
> `render` means: stored, served there, on no filter surface and in no row tail. A flush writes
> per-view extents for **every family with a value column** (r27's write half, widened at this
> merge), so such a family is served live at the drill-down exactly as its searchable siblings
> are — the build is its first writer, not its only one.
>
> **A category's value list is `/v1/categories`' own surface, and it is view-addressed**
> (contracts §3.2). One column per view is one value set per view, so the route takes the view the
> same two ways a leaf does — `?view=<group>:<key>` for the request's own, `{column}@{key}` in the
> path for a pin — resolved at the same site, with the same `422` naming the group where nothing
> decides, the same unknown-view `404`, and the same collapse to the route's own unknown-column
> answer for a principal whose group gate fails. `?view=` is resolved *before* the column is and
> whatever the column's scope, so the gate is the route's first act rather than a check on one
> branch of it; an entity-scoped category's list is unaffected by which view asked, but a `view`
> naming nothing still refuses. Note the one asymmetry with the filter surface, which is the
> vocabulary's rather than the scope's: a category has a value list whether or not it is an
> operand, so a blob-resident one — neither `render` nor `index` — is answered here and appears in
> no `filter_operands` entry. A `derived` vocabulary's list is then derived from
> that view's postings inside `M_auth`: two views of one group offer two lists, and each is that
> view's.
>
> **A scoped attribute may declare its own `source`, built 2026-08-31.** That file carries one row
> per `(entity, view)` and its `fields.view` — defaulting to `view`, the same key and default a
> scoped layer's artifacts source takes — says which view each row's value is for. Each view's
> column is the rows whose discriminator is that view's key, selected through the same
> `ViewSelector` form B's roster is read through: a row naming a key the roster does not carry is a
> refusal naming the key and the roster, and a view with no rows in the file simply has no values.
> Declaring no source — Appendix A's `sentiment`, and the fixture's — reads each view's own points
> file and remains the shape most declarations want.
>
> **`render` reaches the row tail, built 2026-08-31** (contracts §3.2 r61). Each view's column is
> permuted into that view's row space at the build, beside the entity-scoped render columns and
> with the same presence bitmap (decision 0064), so a viewport response under a view of the group
> carries the value and one under any other view does not carry the column at all. Serving narrows
> the render list per view and per principal at one site — the same site the head's column names
> come from, so the names and the buffers cannot diverge — and a family whose group's gate this
> principal fails is absent from it, as it is absent from `filter_operands`. `/v1/meta`'s
> `scoped_scalars` is where a client reads the placement: the family's `render` and `index` flags,
> its type, its vocabulary or analyser, and the view ids that have a column.
>
> **The write half is built, 2026-08-31** (contracts §2.2, §3.1 r64). A row carries the group's
> scoped values in a **second positional list** beside its declared scalars — `WalRow::scoped`,
> positional against the owning group's `scoped_scalars` — rather than in slots of the first,
> because the two are indexed against different declarations: `declared_scalars` is one flat
> bundle-wide list a family has no slot in, and a family's columns are the group's. The boundary
> admits a column named for a family of the group that owns the batch's view and nothing else, so
> the entity-space refusal is byte-identical where it always applied; nullability, the wire type
> and a category's key-not-code rule are the entity-scoped ones, asked of the family's own
> declaration. A scoped category's novel key is minted where an entity-scoped one is, at the
> commit-window close.
>
> The flush writes the family's extent for its view under `attrs/<column>/<group>/<key>/extents/`,
> beside the build's base and composed at publication exactly as an entity-scoped extent is — the
> only thing the scope changes is the directory. A view the family has no column for acquires an
> **empty base** at the same flush, so what is on disc is what a build would have written for an
> empty view, and the pair enters `scoped_scalars[..].views` — the durable record being
> `SEGMENTS-<n>.json`'s `scoped_columns` — durable rather than derived because it predates r26,
> under which a render-only family wrote a row lane and no extent for a derivation to find; r26
> gives every operand family its extents, and the record stays the authority regardless. A rendered family's lane is written from the row's own value,
> which is why a **join** row carries the scoped values and nothing else: the value belongs to the
> `(entity, view)` pair the join is creating rather than to the entity.
>
> **A view of a group that only shares the family's views writes it too** (2026-09-01, decision
> 0116; the ⊘ that stood here is discharged). Its batches carry the family's columns under their
> plain names, its flush writes the extent into the **owner's** directory —
> `attrs/<column>/<owner group>/<key>/`, the cell's address — and its own rows carry the value in
> their lane. The single-claimant property that the old refusal protected is kept by the cell
> comparison above rather than by the refusal, and for a `text` family by that comparison's
> fail-closed form: an occupied cell takes no second string.
>
> ⊘ **What a flush cannot do is fill in a row that has already been written.** A cell's value
> reaches the row tails of the rows that carried it, and the backfill that fills a join's omitted
> `render` slot from the entity's stored value has no counterpart for a row a previous flush already
> published. So an entity holding rows in two views of one key, written through one door and joined
> through the other after that door's flush, renders the value under the writing view and the
> placeholder under the other; the **filter** answer is the cell's under both, the operand being the
> entity-space column. A build writes both lanes from the one column and has no such asymmetry.
> `verify --deep` keeps its build-only exemption for the lane.
>
> The lane is the one thing a segment may lawfully **not** hold, and both ends now say so at the
> same index: `gather_tile_columns` reads a missing scoped column as the row's placeholder, and
> the merge's and the fold's `gather_scalars` write one — which a view whose family list grew
> after some of its segments were flushed needs, its older ones having no lane. A *wrong type*
> under the name stays malformed, scoped or not.

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

> **Implemented 2026-08-31 — the gate, end to end** (contracts §3.2, §3.4 r57). `visibility` is
> accepted on a `[[view]]`, a `[[view_group]]`, an inline roster block, a `[view_group.views]`
> table row and `PUT /control/views/{group}/{key}`, and is stored on the manifest — the group's on
> its `GroupDescriptor`, each view's on its `ViewDescriptor` with the roster record carrying the
> published copy and a disagreement between the two refused at open. `public` compiles to the
> absence of a gate; any other label is put through the plugin's `terms_of_label` at acceptance —
> the same call an item's `access` bytes take at ingest — and a label the plugin cannot read, or
> one naming no terms at all, is refused where its author can read the message rather than stored
> as a gate nobody could satisfy.
>
> At authorise, after the mask is materialised, **every view of every group is evaluated whatever
> the outcome** and the result is an immutable per-session visible-view set: the label's term set
> against the principal's satisfied set, by intersection, the group's conjunctive with the view's
> own. The request path then makes **one set-membership lookup**, on both outcomes — no plugin call,
> no roster scan, and the probe is made whether or not a name resolved, so a gate-failed view and a
> never-declared one cost the same work as well as reading the same. Filtered: `/v1/meta`'s `views`
> and `groups` (a gate-failed group taking its whole roster), each served layer's `views` list,
> `filter_operands`' scoped entries, `/v1/viewport`'s and `/v1/artifacts`' view resolution, and a
> filter leaf's pin. The **control plane is not gated** — it holds the operator credential and is
> the single authority that writes the roster.
>
> **The set is fixed and a view created since is a 404 to a session already authorised**, until it
> re-authorises — the owner ruling this section records, and the price of the lookup above.
> **Ordinal gaps are visible and are not hidden** (spec §9, decision 0110), which is the accepted
> channel and not an unbuilt one.

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
> **A group with no roster at all is built** (2026-08-31, owner ruling): its keys are minted from
> the distinct values of the discriminator, read before pass two by the route a roster table takes
> and sorted by key bytes. What the mint produces is **ordinary roster records**, so the registry,
> the manifest's group descriptor and every verb above them cannot tell a minted roster from a
> written one — a minted group takes a create, a drop and a join at ingest exactly as a declared
> one does (decision 0091), and a batch naming a key it does not carry is the same 404 any unknown
> key gets. Spec §3.1 states the four rules the declaration does not settle. Populating a view at
> ingest — spec §4's join rule — is built (2026-08-31), §4's own markers saying what remains inside
> it.
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
  indistinguishable in outcome and in work from an absent one — by name, and now by the roster
  too. A filtered roster is a **shorter list and nothing else**: a view carries no position of its
  own, so nothing a principal reads counts what was withheld from them. That closes the ordinal
  gap the r6 review found rather than accepting it: ordinals are removed (owner ruling
  2026-08-31, decision 0113), which supersedes decision 0110 and deletes its register row C27.
  What remains observable is a group's *served* size, which is a fact about what this principal
  may reach.
- An ungated group's roster — keys and metadata — is public to every principal that authorises,
  by declaration.
- **Cross-view linkage.** `tessera_id` is the same for an entity in every view — the view is not
  an input to the keyed bijection — so a viewer can join a visible item to itself across views.
  That is the point, and C17's acceptance of the identifier as a stable handle covers it.
- **An item's presence in a view** is disclosed only through the mask: an entity the viewer
  cannot see is served in no view, and an entity absent from a view is indistinguishable from
  one invisible there. The drill-down names the views one item is in *(r27)* and adds no channel
  to that: it lists exactly the views this principal may reach, and for each of those membership
  is `mask ∩ members(view)`, which the viewport already serves — the equality the multi-view
  differential asserts as an equality rather than an inclusion. A view outside the gate is absent
  from the array, so the array is a function of what the principal already knows exists.
- **A point's position in each view** is the same quantity the viewport ships for it, in the same
  grid units, for a point the principal is already being served. Two views place it differently,
  which is a fact about the layouts and not about the corpus's other items.
- **The group-scoped values on the drill-down** are this item's own, read at an entity already
  established visible from a column indexed by entity id — no aggregate, nothing outside `M_auth`.
  A key is served where the principal may reach *a* view holding it, so the set of keys is the one
  their own roster already gives them.
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

No new verb, and no new register row: the ordinal gap that would have been one is gone with the
ordinal (decision 0113), and the drill-down's two new fields widen **C30** — the row that already
covers what that endpoint serves about an item — rather than earning one of their own (r27). The
C15/C17 notes stand.

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
| Contracts §2.1 | A bundle carries several views, `views/<view>/` and `views/<group>/<key>/`; the `group:key` id form; the roster and the key tombstones in the segments manifest, carried for ever |
| Contracts §2.2, §2.5 | The quantisation extent moves onto the view descriptor — first, ahead of any multi-view build |
| Contracts §2.2, §2.3 | **Done at contracts r55** for the attribute: a `groups` row carrying the roster and each group's `scoped_scalars`, with the column families under `attrs/<column>/<group>/<key>/`. **Done at contracts r59** for the layer: `scope` is a field of the declaration, so `SEGMENTS-<n>.json`'s `layers` carries it and a reopened bundle can tell a per-view artifact set from a shared one without the build's configuration |
| Contracts §3.2 | `/v1/meta`: per-view `extent` (r52), groups with their rosters and typed metadata (r53–r54), `filter_operands` carrying the scope and the pinned leaf `name@key` in the filter grammar (r55), and every one of them gate-filtered per principal (r56) — all done |
| Contracts §3.4 | The duplicate rule amended per spec §4; `PUT /control/views/{group}/{key}` and its drop with `delete_dangling`, the create taking a gate label checked against the plugin (r57); identifier forms with mandatory idset on the `tessera_id` form; `--view` withdrawn |
| Configuration §1, §8 | `[[view_group]]` with `[[view_group.view]]`, `[view_group.views]`, `members`, `metadata` and per-view `visibility` on the roster; `fields.view` on a group source, a scoped attribute and a scoped layer; `scope` on `[[attribute]]` and `[[layer]]` |
| Write-path §2, §4, §5 | The create record; the join rule at admission; one pending segment per view touched restated for several views; `delete_dangling` as submitted deletions |
| Compaction | Reclamation of a dropped view; the attribute pass over a family |
| Appendix C | **Made 2026-08-31**: the C15 and C17 annotations, and the gate's enforcement under C4's closure. C27 (the ordinal gap) was added the same day and **deleted at r16**, ordinals having been removed — nothing of this row remains owed |
| Conformance | **Done 2026-08-31** but for one clause: `conformance/tests/test_multiview_differential.py` over `reference/oracle/multiview.py`'s six views — the oracle answers per view, the pinned-leaf cases including the gate-failed pin, and the gate over a group whose `visibility` is a real access label (a failing principal finds it on no surface; a passing one is served the ungated expectation; the union-equals-mask equality holds over the views each principal may reach). ⊘ The gate's **work**-indistinguishability is a timing property and is not asserted there — the identical outcome is, the identical cost is not |
| Decisions | ~~The allocation key~~ — ruled, decision 0112. Ordinals removed, decision 0113, superseding 0110 |

## 12. Rulings

Made 2026-08-31 (owner): **ordinal addressing is removed.** A view of a group is addressed by
key only; a caller wanting a numeric ordering mints numeric keys. The architect's consequence,
taken with the ruling: the ordinal goes entirely rather than only as an address — no field on
`/v1/meta`, no roster or WAL record, no high-water, and no `@#n` pinned leaf — and a group's views
are served in creation order, which is roster-record order. Key tombstones are untouched. Recorded
as decision 0113, superseding 0110; Appendix C's C27 is deleted with it.

Made 2026-08-30 (owner), first pass: the `group:key` form (and a `group:#ordinal` alias, removed
2026-08-31 above); the paged
permutation as every view's representation; no plain view after the build; `visibility` as the
one gate key, defaulting to `public`, the per-view gate on the roster; `delete_dangling` kept;
typed metadata; the create operation ahead of the first batch; the two roster forms; the extent
move taken first.

Made 2026-08-30 (owner), dispositioning the review: the scoped-attribute surface is inside the
gate (spec §5); the ordinal gap is accepted as an Appendix C row (spec §9) — **superseded
2026-08-31**, the ordinal being removed; the visible-view set is fixed for the session's life and
a new view waits for re-authorisation (spec §6); the key is required; roster records are
immutable.

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
each view under spec §4's rule. Either batch may carry `sentiment`: the two views share the key
`2026-Q4`, and the key is the value's address (decision 0116).

## Appendix R — review trail

- **r27a (2026-09-01)** — review round on r27's implementation. One finding was a hole and the rest
  were cleanups. **The hole**: a scoped `text` family escaped the cell arm across a flush boundary —
  its column has no value per entity to compare against, so a second door's differing prose was
  neither deduped nor refused, and the flush wrote a second text layer stamped with the same view
  for `match` to union silently. The arm now asks occupancy from the layers' presence bitmaps and
  refuses a supplied string for an occupied cell, equal or not; the build's base writes no presence
  file and is not covered, which is stated at the claim. Also: the label and attribute arms, the
  descriptor drop and the render backfill are one pass per joining row, sharing one buffer lookup and
  one record-blob decompression; the refusal body is pinned whole by a test as well as compared
  across its two sources; and the demotion direction — a row that stops being a join keeps its label
  — is covered.

- **r27 (2026-09-01)** — **two owner rulings, implemented together.** (1) A scoped value's address
  is `(attribute → its group, key)`: §5's one-door rule is withdrawn, a batch through any view
  whose key is in the attribute's group's key set may carry the family's columns, and the value
  lands in the one cell. The old rule's argument — two views of one key would put two extents over
  one entity — is dissolved by a comparison rather than answered: an identical second value is
  deduped so there is one claimant, and a differing one is a 409 naming the column and the key. §5's
  remaining ⊘ is discharged and a narrower one takes its place, about a row tail a previous flush
  already published. (2) §4's label and attribute arms move from `/control/ingest`'s handler onto
  the serial writer, beside `established_collisions`, which is what settles join-ness — one site
  instead of two, closing the race in which a row promoted to a join between the handler's pass and
  the apply met no arm. The refusal bodies did not move with the site and are byte-identical. The
  handler keeps the duplicate answer, which is the one refusal that may name the caller's own ids,
  and the sidecar half of the join resolution, which cannot go stale. No other design content
  changed.

- **r26 (2026-08-31)** — **`render` alone makes a scoped family a filter operand** (owner ruling),
  and §5's remaining restriction is withdrawn rather than discharged: it was a hole, not a rule.
  The licence is now `index` **or** `render` on both sides of the scope, one predicate each side
  (`filter::scoped_is_filterable`, `filter::is_filterable`), and the leaf resolution, the operand
  list, `/v1/categories`' admission and the gate collapse follow from the one they already called.
  What the old marker had right is that a **lane** cannot answer a pin; what it had wrong is that
  the lane was ever the route. A scoped family's per-view column is entity space and every build
  has written it whatever the flags — the `index` gate stood at the *opener*, not at the writer —
  so a rendered family is answered by the same scan an indexed one is, from the same file, and a
  pin reads the named view's column rather than the rows in front of the request. The cost is
  therefore only what was previously written and never opened, plus the flush's extents, the empty
  base of a view created since the build, and a rendered **category**'s per-view postings — the
  last because one admission decides the filter surface and the value list together, so a rendered
  category earns `/v1/categories` as well and its postings must exist for it. `text` is excluded at
  the predicate: it owes no value column, and `render` on a scoped `text` family is refused at the
  declaration, so the combination would name a token index no pass wrote. No format moves, no
  version moves, and the gate collapse is the same single site (`viewport::resolve_filter_column`).
- **r25 (2026-08-31)** — §5's write half is built and its three ⊘ markers are discharged; no
  design content changed. Ingest: a batch into a view of a group carries the group's scoped columns
  under their plain names, in a second positional list against the group's own `scoped_scalars`,
  the entity-space refusal untouched. Flush: the family's per-view extents beside the build's, the
  empty base a runtime-created view has none of, and the render lane written from the row's own
  value — so a join row carries this view's scoped value and nothing else. Merge and fold: the
  **view's** writer schema rather than the bundle's, which is where the ⊘ was understating itself —
  a rewritten segment did not merely lack a column a rebuild would supply, it *lost* values already
  being served — and the fold's attribute pass now folds each family's per-view column, which it
  had never written at all. Two markers replace them, both narrow: a sharing group's views take no
  value from a batch (the column is the owner's, and a second writer for one `(entity, view)`
  column is two layers claiming one entity), and §4's attribute arm is unchanged.
- **r24 (2026-08-31)** — §4's attribute arm is exact past a flush and the section's last ⊘ is
  discharged. No design content changed: the rule §4 states is the one it always stated. What the
  marker got wrong is worth keeping, because both errors were in the direction of leaving it
  marked. It called the read-back "a second value oracle across every declared family"; it is one
  read per column, the declaration already deciding which of three homes holds the value (records
  §3, decision 0068), and the fold and the coalesce owe no pass at all — they rewrite those homes
  already. And it called the accepted row inert, which held for the two entity-space homes and not
  for the third: a joining row's scalars travel in its own row tail, so a rendered column's
  differing value was reaching the joined view's hot column. A refusal is withdrawn in the same
  change: a batch leaving a **category** null was a `409` against an entity holding a value,
  because the arm read absence only in its `null` spelling and a category says it with the reserved
  code. That was the buffered arm's behaviour, so the correction moves both. Two findings from the
  review of the change itself are folded in. The **backfill** is the owner's ruling on the
  divergence the third home exposed: an accepted join that omitted a `render` value must not write
  an absence into the joined view's tail while another view renders a value, so it takes the stored
  one. And the hot-column read scanned views out of a `HashMap` and stopped at the first that held
  a *row*, reading a clear presence bit as "no value held" — so the one disagreement the backfill
  cannot close, an entity holding nothing where it was ingested and a value where it was joined,
  answered a later join by hash order. It now scans every view and absence never pre-empts a value.
- **r23 (2026-08-31)** — §4's label arm is exact past a flush and its ⊘ is discharged; the
  attribute half stays marked, with what it would cost stated. No design content changed: the rule
  §4 states is the one it always stated, and what moved is that the deployment can now enforce it.
  The oracle is `contracts.md` §2.4's `entities/terms/`, built for the drill-down's `labels` array
  ([decision 0114](../decisions/0114-the-drill-down-serves-the-satisfied-labels-only.md)) and read
  here for the full set it also holds.
- **r22 (2026-08-31)** — spec §5's `render` marker is discharged and no design content changed.
  Built: each view's column of a rendered family permuted into that view's row space at the build,
  with its presence bitmap; a per-view, per-principal render list at the one site the head's names
  and the gather's buffers both come from; and `/v1/meta`'s `scoped_scalars`, a family's
  counterpart to `declared_scalars`, carrying the type, the vocabulary, the analyser, the two
  placement flags and the view ids that have a column — the vocabulary and the analyser moving
  there from the operand entry, which is the operand surface and carries neither for an
  entity-scoped column either. Review (2026-08-31) added three things and changed one: the
  published view list **expands** to every group sharing the family's views and is gate-filtered
  per id, without which a client under a shared view reads that the column it is receiving does not
  exist; `tessera verify --deep` checks the lane per build segment, the one defect here that
  serving cannot distinguish from an absent value; and the flushed-segment path — the one that
  turns a missing column into silence — is driven by a test rather than only described. What stays marked: the **ingest** half, so a view created while the
  service runs has no column until a rebuild; a merge or fold, which takes its writer schema from
  the bundle-wide list and so drops the column from a segment it rewrites; and the filter surface,
  where `index` remains the whole licence — a leaf resolves to one entity-space column and a pin
  may make that another view's, which a scan of the request's own rows cannot answer.
- **r21 (2026-08-31)** — §3.4's ⊘ is discharged by a test, and the test found a defect. What the
  section claims is reclamation *by omission*: a dropped view is absent from the bundle the fold
  plans over, so its segments are not carried into the new prefix and its files go with the
  superseded one. The first half held; the second did not. A drop removes the view from the bundle
  and from the roster, and leaves its `SegmentDescriptor`s in the partition's side-manifest until
  something rewrites it — so the fold's publication treated them as *not consumed*, tried to carry
  them forward, and discarded itself on the check that every carried segment must have a base in
  the plan. The effect was that **no fold ever published again** on a bundle a non-empty view had
  been dropped from: no segments merged, no deletions retired, no disc reclaimed, and only a
  warning to say so. The fix is one filter in the carry-forward, which is the omission the plan had
  already made. `tests/views_write.rs` drives the whole claim — the plan's omission, the files, a
  restart serving the survivors, and the tombstone outliving the fold's manifest rewrite.
- **r20 (2026-08-31)** — spec §5's two ⊘ markers are discharged and no design content changed.
  Built: a scoped **category**'s per-view keyed postings, which are both the filter route on a
  `public` vocabulary and what `/v1/categories` derives a value list from on a `derived` one; a
  scoped **text** column's per-view token dictionary and positional postings, that family owing no
  value column; `/v1/categories` addressed by view — `?view=` or the pinned path — through the same
  resolution and the same gate collapse the filter leaf takes; and an attribute's own `source`,
  one row per `(entity, view)` routed by `fields.view`. Two refusals are new and both are
  declaration-time: a scoped `text` column without `index = true`, whose prose would otherwise have
  no home at all — the record blob is bundle-wide and a family has no slot in it — and `fields` on
  an attribute with no view to choose between. Still marked, each at its claim: `render` on a
  scoped attribute, and the ingest half, which is absent by construction rather than by omission —
  a buffered row's scalars are positional against `MANIFEST.declared_scalars`, and a family is
  deliberately not in it.
- **r19 (2026-08-31)** — first-batch-creates is withdrawn (owner ruling): §3.2's exception for a
  record-less group is deleted, a batch naming an unknown key being a 404 for every group, and the
  §3.2 marker keeps only its category-metadata item. Creation is the explicit operation, so a
  mistyped key refuses instead of minting a view around the mistake.
- **r18 (2026-08-31)** — the last roster form is built: a group declaring neither
  `[[view_group.view]]` blocks nor a `[view_group.views]` table has its keys minted from the
  distinct values of its discriminator column (owner ruling: implement it). The design content is
  in §3.1's four rules, which are the points the declaration leaves open — order, an unusable
  value, metadata and the gate — and they are marked as recoverable architect's choices rather than
  as invariants. Two of them a principal does observe: roster order **is** served order (§3.2), and
  the gate decides which principals may reach a view at all. Neither is a **disclosure**. The
  order is a deployment constant, the same list for every principal, computed from the declaration
  and its sources rather than from anything inside `M_auth`; and a minted view taking the group's
  own gate is exactly what a roster record carrying no `visibility` already does, so no view is
  reachable that a written roster would have closed. They are recoverable because a later ruling
  costs a rebuild, not because nothing sees them. The mint hooks in where the roster table is read,
  so it produces roster records and forks no downstream path; §7's marker is now a description
  rather than a gap, and the register gains no row.
- **r17 (2026-08-31)** — **a shape layer is canonicalised per view, in each view's own frame**
  (decision 0111), and spec §2's marker and §11's contracts row record it. `canonical_shapes` takes
  a frame per view rather than one projection and one extent for every view of a layer;
  `/control/layers` resolves one per view; the build's layer read passes every view it
  materialises rather than the anchor's. The 2026-08-29 refusal of a shape layer whose views
  declare different projections is **deleted** — 0111 supersedes it — and two narrower refusals
  replace it: the mix of a projected view and a `projection = "none"` one, at the declaration on
  both entry points, and a `space = "view"` row over frames that are not identical, at the row. A
  shape wholly outside a view's extent warns with that view's own count and is published. A
  layer's `scope` is now a field of the declaration and therefore of the manifest (contracts r59),
  which is §11's remaining `§2.2` item. No design content changed: this revision records what was
  built against text r9 of `polygon-membership.md` §4.3 already specified.
- **r16 (2026-08-31)** — **ordinals are removed** (owner ruling), and the removal is total rather
  than an address form withdrawn. Gone: `/v1/meta`'s `ordinal` field, the roster record's and the
  `ViewDrop` record's ordinal, the per-group high-water, the manifest's stored number, the
  `<group>:#<ordinal>` id and the `@#n` pinned leaf. What replaces it is the order the records are
  already in: roster records are appended and never rewritten, so a group's views are served in
  creation order without a number to sort by, which is what a client walking `groups[..].views`
  was already doing. `#` is no longer reserved out of a key by name — the column-name charset
  refuses it, as it refuses every other punctuation — and an id in the old form now names a key
  nobody declared, which is the ordinary unknown-view `404`. **Key tombstones are unchanged**: a
  dropped key is refused for ever, and it was always the key rather than the number that repointed
  a client cache (decision 0029). The disclosure this closes is spec §9's own: with no position
  served, a gate-filtered roster is a shorter list and nothing else, so Appendix C's **C27 is
  deleted** and decision 0110 is superseded by 0113 — the register does not carry a row for a
  channel that no longer exists. Design content did change in this revision, which is what the
  ruling was; nothing else of §3.2 moved.
- **r15 (2026-08-31)** — the gate is built, and spec §5's and §6's markers record it. `visibility`
  is accepted on every surface that declares one and evaluated at authorise into a per-session
  visible-view set, by the intersection semantics this document has specified since r1 — *not* the
  conservative label join, whose required-set reading passes every principal on a disjunctive gate.
  Three properties are structural rather than incidental, and each is written at its site: the set
  is resolved **once**, over every view whatever the outcome, so the request path makes one
  set-membership lookup and asks the plugin nothing; that lookup is made on **both** outcomes, so a
  gate-failed name and a never-declared one cost the same work rather than merely reading the same;
  and the acceptance check is the plugin call an item's label already gets at ingest, so a gate no
  principal could satisfy is refused where its author can read the message instead of stored. A
  view's gate is written twice — the roster record publishes it, the view descriptor is what the
  evaluation reads — and a manifest whose two copies disagree refuses at open, which is the
  fail-closed direction for the one disagreement that matters. Not moved, and stated rather than
  assumed: the set is **fixed** for the session's life, so a view created since is a 404 until
  re-authorisation (owner ruling, spec §6); ordinal gaps are **visible and not hidden** (spec §9,
  decision 0110); and the control plane is ungated, holding the operator credential that writes the
  roster. Appendix C's amendments for the gate are made — C11's precedent for `/v1/meta`'s view
  fields, C4's closure for view existence; the ordinal gap's own accepted row (spec §11) is not,
  and the table says so. No design content changed in this revision.
- **r14 (2026-08-31)** — the write half is built, and the markers at spec §1, §3.2, §3.4 and §4
  record it. `PUT`/`DELETE /control/views/{group}/{key}` create and drop a view of a declared
  group; the roster's durable home is `SegmentsManifest.views`/`view_tombstones`, carried forward
  at every publication as `layer_tombstones` is, with the WAL's `ViewCreate`/`ViewDrop` replayed
  over it at open; a created view answers a viewer verb empty — a view with no files now has an
  empty row space rather than being absent from the bundle — and takes its first row space at the
  next flush; the ingest join is spec §4's rule, with a joining row carrying geometry and nothing
  else; and `delete_dangling` submits ordinary deletions on the deny lane, probe and submission in
  one step on the executor. Three things are recorded rather than assumed, each at its marker: a
  category-typed metadata name has no create that can satisfy it, the join's label and attribute
  arms are exact only while the entity's own row is still buffered, and a dropped view's files are
  reclaimed by the fold's omission rather than by a sweep, which no test drove until r21. Two defects the
  work surfaced and fixed, both older than it: a group's view laid its flush and merge files down
  at `views/<group>/<key>/` while naming them `views/<group>:<key>` in the manifest — every file
  under the view unverifiable at the next open — and the bundle-wide watermark was `entity_hi + 1`
  of whichever view flushed, which under several views regresses and is refused, leaving those rows
  buffered for ever. No design content changed in this revision.
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
