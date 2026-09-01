# Views and view groups — a guide

**Status:** Guide, not normative. Where this document and [`../design/views.md`](../design/views.md)
disagree, the spec wins — cite it (`views.md §n`) rather than trust a paraphrase here. The
configuration keys are owned by [`../design/configuration.md`](../design/configuration.md) §1; the
wire shapes by [`../design/contracts.md`](../design/contracts.md) §3.2, §3.4. Everything described
here is built, as of `views.md` r23 (2026-08-31).

This is a task-oriented walkthrough for the operator declaring a corpus and the client reading it —
what a view is, when to reach for a view group instead of a plain view, and how to grow a group
while the service is running. The worked examples are drawn from
[`test_corpora/multiview/corpus.toml`](../../test_corpora/multiview/corpus.toml), a fixture built
to exercise this whole surface; its [README](../../test_corpora/multiview/README.md) is a
feature-by-feature index into it.

## What a view is

A **view** is a named coordinate system over one shared entity space. The same document can sit at
a different position in each view it belongs to, but it has one identity, one access label and one
set of attribute values wherever it appears — those live in entity space, upstream of any view.
Positions, the Morton order, and the render columns live downstream, one per view.

Reach for a **plain `[[view]]`** when the whole corpus needs a second layout that will not change
again without a rebuild — a UMAP embedding beside a geographic map, say. Reach for a **`[[view_group]]`**
when the layout itself grows over time — a corpus re-embedded every quarter, with the next quarter
arriving while the service keeps running. A group's views are created and dropped at the running
service; a plain view is not — adding one is a rebuild.

## Declare a plain view

```toml
[[view]]
name             = "world"
title            = "Whole corpus"
projection       = "web_mercator"
extent           = { lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }
visibility       = "public"
point_visibility = { field = "access", default = "public" }
```

`extent` is the frame every position in this view is quantised against, for the view's life — it
belongs to the view rather than to the bundle, because an embedding and a map cannot share a frame
without one of them wasting most of the grid. Under a projection, `extent` is stated in longitude
and latitude; `"auto"` fits a box to the data with a small margin, and is the usual choice for an
unprojected embedding.

**With more than one view declared, `[defaults].allocation_view` is required:**

```toml
[defaults]
source          = "world"
entity_id_field = "entity_id"
allocation_view = "world"     # breaks entity-id ties within a signature group at build time
```

Entity ids are permanent, so the tie-break is a declaration rather than a default that could
silently re-key a rebuild if the file's block order changed (decision 0112).

## Declare a group of timeslices

A view group takes every key a plain view takes, with the same meaning, and adds a roster — which
views it has. There are three ways to write the roster.

**Form A — one file per view, written inline:**

```toml
[[view_group]]
name             = "quarter"
title            = "By quarter"
projection       = "none"
extent           = { x = [-40.0, 40.0], y = [-40.0, 40.0] }
visibility       = "public"
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us", ends = "timestamp_us" }

[[view_group.view]]
key    = "2026-Q1"
source = "quarter_2026_q1"
label  = "Q1 2026"
starts = 2026-01-01T00:00:00Z
ends   = 2026-04-01T00:00:00Z

[[view_group.view]]
key    = "2026-Q2"
source = "quarter_2026_q2"
label  = "Q2 2026"
starts = 2026-04-01T00:00:00Z
ends   = 2026-07-01T00:00:00Z
```

Each `[[view_group.view]]` block declares one view; the file named by its `source` *is* that view's
points, so the group itself declares no `source`. `metadata` names the typed per-view values every
roster record carries — `label`, `starts`, `ends` here — and each inline block must supply every
one of them.

**Form B — one file, a discriminator column:**

```toml
[[view_group]]
name             = "quarter_alt"
title            = "By quarter, geographic"
members          = "quarter"
projection       = "web_mercator"
extent           = { lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }
source           = "quarter_alt_pts"      # entity_id, quarter, lon, lat, access
fields           = { view = "quarter" }
point_visibility = { field = "access", default = "public" }
```

Here the group's own `source` holds every view's rows in one file, and `fields.view` names the
column that says which view each row belongs to. A row naming a key the roster does not carry is
refused; a key with no rows is an empty view.

**No roster at all** — the keys are minted from the discriminator's own distinct values, sorted by
key bytes:

```toml
[[view_group]]
name             = "quarter"
extent           = { x = [-40.0, 40.0], y = [-40.0, 40.0] }
source           = "quarter_papers"
fields           = { view = "quarter" }
point_visibility = { field = "access", default = "public" }
```

This is the cheapest declaration and the right default when a group needs no per-view metadata and
no per-view gate — every minted view takes the group's own `visibility`. The build refuses, naming
what it found, on: a discriminator value outside the column-name charset, a null in the
discriminator, a source with no rows at all, and a discriminator column that is not a string. It
also refuses `metadata` on this form — there is no roster record for a per-view value to sit on.

Declaring **both** a roster and letting the group mint one is refused, the same way `source` beside
inline `artifacts` is on a layer. `[defaults].source` does not reach a group at all — which of these
three shapes a defaulted file would mean is not something a default can decide, so a group always
names its own `source` (form B and the roster-less form) or none (form A).

## Share one set of views under a second layout

`members = "quarter"` on `quarter_alt` above says its views *are* `quarter`'s — the same keys, a
different layout. A group naming `members` declares no roster and no `metadata` of its own — those
belong to the owning group — but keeps its own `visibility`, which may narrow the owner's.
`members` chains are refused: the owner of a key set is always one hop away. Creating or dropping a
key on the owning group creates or drops it on every group sharing those views at the same moment.

## Addressing: `<group>:<key>`, and nothing else

A view of a group is addressed `group:key` everywhere a view id goes — the request body, the
`x-tessera-view` ingest header, `/v1/meta`. A plain view is addressed by its bare `name`. Keys are
the caller's own strings, required at creation, under the column-name charset (ASCII letters,
digits, `_`, `-`); `:`, `#` and `@` are reserved out of names and keys because they already mean
something in the addressing grammar. **There are no ordinals.** A group's views are served in
**creation order** — the order the roster records arrived in, nothing more — so a caller who wants
a numeric or time order sorts by their own metadata (`starts`, in the example above) rather than by
position.

## The gate: who may reach a view

`visibility` on a `[[view]]` or `[[view_group]]` is an access label or `public` (the default), and
means the same thing it means on a layer: a label the plugin resolves to a term set, satisfied
where that set meets the principal's. A group's view may carry its own narrower `visibility` on the
roster; a view carrying none takes the group's. The group's gate is the outer bound — a view's own
gate can narrow it and never widen it.

A principal's set of reachable views is resolved once, at authorise, and is **fixed for the
session's life**. A view created after a session has authorised is a 404 to that session until it
re-authorises — creating a view is rare and tokens expire, so re-evaluating per request was not
worth the cost. A gate-failed view is the **same 404** an unrecognised name gets, at the same cost,
on both the create/drop endpoints and every viewer route: there is nothing to distinguish "this
view does not exist" from "you may not see it". A gate-failed **group** takes its whole roster with
it — `/v1/meta` omits the group entirely, not just its ungated views.

## Create a view while the service is running

```
PUT /control/views/quarter/2026-Q4
{ "visibility": "public", "metadata": { "label": "Q4 2026", "starts": 1798761600000000, "ends": 1806537600000000 } }
```

The body is the roster record — every metadata name the group declared, typed, is required; an
undeclared name is refused. `timestamp_us` metadata is microseconds since the epoch as a JSON
integer. `visibility` defaults to the group's own. A `201` returns `{view, group, key}` — no
ordinal. `409` if the key is already taken **or was ever dropped**: a burnt key never comes back,
because a client cache, a bookmark or a cached θ keyed on the view would otherwise be silently
repointed at different contents.

**The roster record is immutable.** A wrong gate or wrong metadata is not corrected in place — drop
the key and create a fresh one under a new key. Creating `quarter:2026-Q4` also creates
`quarter_alt:2026-Q4` at the same moment, empty, since `quarter_alt` shares `quarter`'s views.

A created view answers every viewer route immediately, empty — it is in `/v1/meta` and it is a 404
to nobody — and takes its first row space at the **next flush**, not before.

```
DELETE /control/views/quarter/2026-Q1
DELETE /control/views/quarter/2026-Q1?delete_dangling=true
```

Dropping a view tombstones the key for ever and reclaims its row-space artifacts at the next fold;
requests naming it become the ordinary unknown-view 404. **Dropping a view does not delete an
entity.** An entity whose only view was `quarter:2026-Q1` still exists — with its label, its
attributes, its layer memberships — in no view at all, and a later batch into a different view picks
it back up by `external_id` under the ordinary join rule below. `delete_dangling=true` is sugar for
the caller who does mean "and drop the entities this view was the only home of": the service finds
every entity of the dropped view holding no row anywhere else (the ingest buffer included) and
submits them as ordinary deletions on the deny lane — the same retirement route (Rule F) any other
deletion takes, never a second one. The response's `deleted` count is `0` without the option.

## Add a quarter while the service runs

Create the view (above), then ingest into it:

```
POST /control/ingest
x-tessera-view: quarter:2026-Q4
x-tessera-batch-id: <uuid>

(external_id, x, y, access, sentiment, mood, note, coverage)   # Arrow, application/octet-stream
```

A row belongs to one view; a point that belongs to several is several batches, one per view, one
`external_id`. Getting the key wrong is a plain 404 — creation is explicit precisely so a typo
cannot mint a view around the mistake, ever since `views.md` r19 withdrew the earlier
first-batch-creates route.

## Join an entity already known into a second view

An `external_id` already present elsewhere is not a duplicate when the batch names a view it is not
yet in — it is a **join**: the row lands in that view's pending segment carrying the existing
entity, and the entity, its label and its entity-scoped attributes are untouched. Three refusals
guard the entity from being silently changed by a second view's row:

- **Already in the named view** (including a row still sitting unflushed in the commit window) —
  `409`.
- **A different `access` value on a known id** — `409`. A re-label is delete-plus-re-ingest
  (decision 0047), never a value carried in on a join.
- **An entity-scoped attribute value that neither byte-matches the stored one nor is null** —
  `409` naming the column.

A **group-scoped** attribute may not appear on a plain view's batch at all — it is an undeclared
column there. On a batch into any view whose key the attribute's group holds — the owner's own
views, and every view of a group declaring `members` of it — it may and should: a join carries
geometry **and this key's scoped values**, and nothing else. It allocates no id, writes no
descriptor and contributes no postings, which is what keeps a label supplied on a joining row inert
rather than a quiet widening — but a scoped value belongs to the `(entity, attribute, key)` cell
the row addresses, not to the entity, so it is the one thing such a row legitimately brings.

All three refusals are decided on the write executor, not in the handler, so a row that becomes a
join between your request arriving and the write landing meets them too (decision 0116). You still
get the `409` in the same request, and a refused batch leaves nothing behind.

## Give an attribute a per-quarter value

```toml
[[attribute]]
name   = "sentiment"
type   = "f32"
scope  = { group = "quarter" }
index  = true
render = true
# no source: read from each quarter's own points file, the group being form A
```

An attribute with no `scope` is entity-scoped — one value under every view, the ordinary case, and
needs no declaration to say so. `scope = { group = "quarter" }` makes it a family of entity-space
columns, one per view of `quarter` (and of any group sharing those views, `quarter_alt` included),
each with its own presence bitmap. All four families answer: numeric and category (postings) and
text (a per-view token dictionary) — a scoped `text` column additionally **requires** `index = true`,
because the record blob has no slot for a family.

`index` **or** `render` puts the family on the filter surface, exactly as it does for an
entity-scoped column: a family declared `render = true` alone filters and pins like an indexed one,
because what it filters is the per-view entity-space column and not the row tail. The two flags
still say different things: `render` puts the value in each view's row tail, where a client draws
it, and a family you filter but never draw wants `index = true` and `render = false`.

A scoped attribute can read its values from a separate file instead of each view's own points:

```toml
[[attribute]]
name   = "coverage"
type   = "f32"
scope  = { group = "quarter" }
index  = true
source = "attrs_scoped"      # entity_id, quarter, coverage — one row per (entity, view)
fields = { view = "quarter" }
```

**Filtering.** Under a view of the attribute's own group (or a group sharing its views), a bare leaf
resolves to that view's column:

```json
{"sentiment": {"range": {"gte": 0.5}}}
```

Anywhere else, the leaf must **pin** a view by key — `column@key`:

```json
{"sentiment@2026-Q3": {"range": {"gte": 0.5}}}
```

which is how "documents that were negative in Q3" becomes a filter on the whole-corpus map. A bare
leaf with nothing to decide the view is `422`, naming the group — a leaf with no column to read is
malformed, not empty. A pin naming a key that view does not have (or that this principal's gate
fails) is the ordinary unknown-view `404`. Where the principal cannot reach the group at all, the
attribute is undeclared to them entirely, and both spellings collapse to the plain unknown-column
`422` — nothing here confirms that a group or a key exists.

`GET /v1/categories/{column}` takes a view the same two ways for a scoped **category**:
`?view=quarter:2026-Q3` or the pinned path `mood@2026-Q3` — two views of a group hold two value
sets, and each is genuinely that view's own.

### Populating one by ingest

A batch into a view of the group carries the group's scoped columns **under their plain names** —
`sentiment`, not `sentiment@2026-Q3`. The view comes from `x-tessera-view`, so the column is not
qualified and the view decides which of the family's columns the value lands in:

```
POST /control/ingest
x-tessera-view: quarter:2026-Q3

external_id | x | y | access | kind | sentiment
```

Nulls are absences, a category arrives as its **key** (never a code), and a column the batch
omits entirely means every row of it is absent — a family has no slot in the positional scalar
tail, so leaving it out misaligns nothing. A view created while the service runs acquires its
columns at the **first flush** that covers it, with no rebuild: from then on it filters, pins,
renders and answers `/v1/categories` like any other, and `/v1/meta`'s `scoped_scalars[..].views`
names it.

**Either door writes the cell.** A group that declares `members` of another shares its keys, and a
scoped value is addressed by `(attribute → its group, key)` — never by the view — so a batch into
`quarter_map:2026-Q3` may carry `sentiment` exactly as one into `quarter:2026-Q3` may, and both land
in the one cell (decision 0116). What is refused is a **disagreement**: a row naming a cell that
already holds the same value is accepted and its copy dropped, and one naming a different value is a
`409` naming the column and the key. A view whose key the attribute's group does not hold refuses
the column as undeclared, exactly as a plain view does.

⊘ **A row already written is not filled in retroactively.** The value reaches the row tails of the
rows that carried it, so an entity holding rows in both views of a key — written through one door
and joined through the other after that door's flush — draws with the value under the writing view
and the placeholder under the other. Filters answer the cell's value under both, the operand being
the entity-space column, and a build writes both tails from the one column and has no such
asymmetry.

## Layers and shapes over groups

A layer names either plain view ids, a group (every view of it, present and future), or a mix:

```toml
[[layer]]
name       = "collections"
views      = ["world", "quarter"]
source     = "collections"
membership = "enumerated"
...
```

By default (`scope = "entity"`) a layer draws **one artifact set** on every view it names — a
curated reading list shown identically on the map and on every quarter. `scope = { group = "quarter" }`
instead draws a **different artifact set per view**, the artifact rows carrying a `view` column via
`fields.view`; edges may not cross views, and keys are unique per `(layer, view)`:

```toml
[[layer]]
name   = "quarter_clusters"
views  = ["quarter"]
scope  = { group = "quarter" }
source = "clusters_q"
fields = { view = "quarter" }
...
```

A **shape** layer (`membership = "spatial"`, a `wgs84` boundary) may span views that project
differently — a Web Mercator map and an equirectangular one — because the geometry is declared once,
in longitude and latitude, and canonicalised **separately per view** through that view's own
transform. Two views sharing one frame (every view of one group, by construction) canonicalise to
identical bytes and pay only the repeated work; two views with different frames genuinely
decompose differently. A layer mixing a projected view with `projection = "none"` is refused at
declaration; a `space = "view"` shape row spanning views with different frames is refused at the
row, since a raw coordinate names a different place in each frame.

A newly created view has no artifact extents of either kind until the fold writes them — the
ordinary state of a layer over a segment nothing has flushed yet.

## What `/v1/meta` publishes

```json
{
  "views": [
    { "name": "quarter:2026-Q1", "group": "quarter", "key": "2026-Q1",
      "metadata": { "label": {"type": "text", "value": "Q1 2026"}, "starts": {...} },
      "quantisation": { "x_min": -40.0, ... }, "projection": "none", ... }
  ],
  "groups": [
    { "name": "quarter", "title": "By quarter", "members_of": null,
      "views": ["quarter:2026-Q1", "quarter:2026-Q2", "..."] }
  ],
  "declared_scalars": [...],
  "scoped_scalars": [
    { "name": "sentiment", "scope": {"group": "quarter"}, "render": true, "index": true,
      "views": ["quarter:2026-Q1", ..., "quarter_alt:2026-Q1", "..."] }
  ],
  "filter_operands": [...]
}
```

Every one of `views`, `groups`, `filter_operands` and `scoped_scalars` is filtered to what the
asking principal may reach, computed once at authorise. `views` carries each view's own frame — a
client must never assume two views share one quantisation. `groups[..].views` is roster order, and
is the only ordering `/v1/meta` publishes for a group — there is no ordinal anywhere on the wire. A
plain view's `group`, `key` and `metadata` are all `null` — one record, not three independent
absences. `scoped_scalars[..].views` lists every view id that actually carries the column,
including every group sharing it via `members` under its own ids — it is not the group's roster,
because a view created since the build has no column yet.

## Worked example, end to end

Declare (excerpted from `test_corpora/multiview/corpus.toml`): a plain view `world`, a form-A group
`quarter` with four inline quarters, a second group `quarter_alt` sharing `quarter`'s views under a
geographic layout, an entity-scoped `kind` attribute, a group-scoped `sentiment` attribute, and a
`collections` layer over `world` and `quarter` together. Build and check it:

```
tessera check  data/ladder/multiview/corpus.toml
tessera build  data/ladder/multiview/corpus.toml  -o bundle/
tessera serve  bundle/
```

Ten row spaces come out of one entity space: `world`, `quarter:2026-Q1..Q4`, and
`quarter_alt:2026-Q1..Q4` (`world_flat` and `regions`' two frames are the fixture's shape-layer
case, not part of this walkthrough).

Query the whole-corpus map, filtered by a quarter's sentiment:

```
POST /v1/viewport
{ "view": "world", "zoom": 4, "bbox": [-40,-40,40,40],
  "filters": {"sentiment@2026-Q3": {"range": {"gte": 0.5}}} }
```

Add a fifth quarter while the service is running:

```
PUT /control/views/quarter/2026-Q5
{ "visibility": "public",
  "metadata": { "label": "Q5 2026", "starts": 1798761600000000, "ends": 1806537600000000 } }
```

This also creates `quarter_alt:2026-Q5`, empty, immediately. Ingest its points:

```
POST /control/ingest
x-tessera-view: quarter:2026-Q5
(external_id, x, y, access, sentiment, mood, note, coverage)
```

An entity already seen in `world` or an earlier quarter that also appears in this batch is a join,
not a duplicate — its label and its constant attributes are unchanged, and its `sentiment` for
`2026-Q5` is a fresh value in a fresh column. Until the next flush, `quarter:2026-Q5` answers every
viewer route empty. `/v1/viewport` against `quarter:2026-Q5` after the flush returns the new
quarter's points; `/v1/meta` lists `quarter:2026-Q5` in `groups[..].views` from the moment the
`PUT` was acknowledged, and in `scoped_scalars[..].views` from the first flush that covers the new
quarter — before that a request under it simply has no `sentiment`.

## Sharp edges

- **A key, once created, is burnt for ever** — dropped or not, it never comes back under different
  contents (decision 0029). Get the metadata right before creating, because there is no update.
- **Visibility is fixed per session.** A view created mid-session is invisible to that session until
  it re-authorises, whatever its gate says.
- **A typo on ingest is a 404, never a new view.** `x-tessera-view: quater:2026-Q1` does not mint
  `quater`; creation is always the explicit `PUT` first.
- **There are no ordinals.** Sort a group's views by your own metadata (`starts`, a numeric key you
  minted) — creation order is served order and nothing else.
- **A sharing group's views write the owner's cell, not a second one.** A group declaring `members`
  shares the keys, and the key is the value's address (decision 0116): either door writes the same
  cell, an identical second write is deduped, and a differing one is a `409` naming the column and
  the key. What that refusal is telling you is that the key already holds a value — not that you
  used the wrong door.
- **A `text` scoped column cannot be rewritten once it has flushed.** Its stored prose is a
  dictionary and postings with no value to compare against, so a second batch naming that cell is
  refused whether the string agrees or not. Change it with a delete and a re-ingest; omit the column
  to leave it alone.
