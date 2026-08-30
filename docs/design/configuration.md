# The configuration surface — design

**Date:** 2026-08-18
**Status:** **Normative for the build-time configuration surface.** One file declares the corpus,
its views, its vocabularies, its attributes and its layers; `tessera build` reads it and compiles
it into `MANIFEST.json`. §3 is the three-file split that makes `tessera build` the whole
invocation. Ruled by
[decision 0088](../decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md); the
design record is
[`../evidence/memos/2026-08-18-configuration-surface.md`](../evidence/memos/2026-08-18-configuration-surface.md)
and the staged plan is beside it.

**What this owns, and what it does not.** This document owns the *declaration* — which blocks exist,
which keys they take, what is required, and what is refused.
[`per-point-attributes.md`](per-point-attributes.md) owns what a category and its vocabulary *are*,
including the visibility semantics its §5 states; [`records-and-search.md`](records-and-search.md)
§3–§5 owns the three homes a field can occupy; and
[`annotation-write-cycle.md`](annotation-write-cycle.md) §6.1 owns what a build does with the
artifact and member grains once it has read them. Where this and those differ on *semantics*, they
govern; where they differ on *spelling*, this does.

**The declaration, the acquisition and the readers are built.** `tessera build` — with no flags at
all — finds `tessera.toml`, reads the declaration it names, compiles the blocks below with every
refusal §7 states, and reads each object's source under the names its `fields` map resolved.
`tessera check` is the same resolution against Parquet schemas alone, in seconds, and emits the
control-plane payloads (§2, §3). Every `source` **names a key of `[sources]`**, which is where the
paths live, each relative to the declaring document; `--file NAME=PATH` overrides one path, keyed
by the source's own name. `--extent`, `--id-key`, `--id-key-file`, `--points`, `--pairs`, `--values`,
`--artifacts`, `--artifact-members`, `--schema`, `--layers`, `schema.toml` as a fixed name and
`layers.toml` are all gone. What is **not** built, and is refused rather than accepted and ignored,
each naming what is absent per
[decision 0013](../decisions/0013-mark-specified-vs-implemented.md):

- **A view's or a view group's own `visibility` where it names a label** (`views.md` §6), and
  `withdraw_on_member_deletion = true` on a **layer** (not on its content, which needs a fold
  path) — each refused at parse rather than accepted and ignored. `visibility = "public"` compiles:
  it is the default and the current behaviour, so writing it records nothing that is not already
  true.
- **A `[[view_group]]`, at the *build*** (`views.md` §7): the whole declaration parses, is checked
  and is reported by `tessera check`, and `tessera build` against a declaration carrying one
  refuses — there is no multi-view build, so a group is a set of coordinate systems with nothing
  to materialise them.

⊘ A `title` on a view, an attribute or a vocabulary is compiled and **not yet published** — the
manifest carries no slot for one, and adding three is a contracts change; a level's title and a
layer's are served today, as is each vocabulary *value*'s.

## 1. The surface in full

**The configuration surface is a closed set**, and that is a property rather than an accident. Every
block parses under `deny_unknown_fields`, and every value that is a word rather than a caller's
string is drawn from an enumerated set — so a key this table does not name does not exist, and a
value it does not list is refused. That closure is what the leak register rests on: the register is
exhaustive *because* the surface is enumerable, and a key added without an entry here is a control
nobody has reasoned about.

Fourteen blocks. `R` = required, `D` = defaulted, `O` = optional with no default and no fallback.
**`source`, `fields` and inline data are acquisition keys** — a build reads them and a deployment
writing through the service omits them entirely (§2), so an `R` on one of those means *required to
build from a file*, never *required to declare*.

**`[sources]`** — the caller's own names for the files this declaration reads. Free-form keys, one
path each, relative to this document (§3). Every `source` below names one of them.

```toml
[sources]
points   = "papers.parquet"
geometry = "umap.parquet"
scores   = "sentiment.parquet"
```

**`[defaults]`** — what a block takes when it names neither of these itself.

| Key | | Value |
|---|---|---|
| `source` | O | a `[sources]` key, taken by a `[[view]]` or an `[[attribute]]` that names none |
| `entity_id_field` | D `entity_id` | the column an entity id is read from, wherever one is read |
| `allocation_view` | R when several views are declared | the view whose Morton code breaks entity-id ties within a signature group at a build ([decision 0112](../decisions/0112-the-anchor-view-orders-a-signature-groups-ids.md)); with one view, that view. ⊘ Not read yet — the multi-view build is unbuilt (`views.md` §7) |

**A `source` names a key, never a path, and there is no fallback between the two.** A name
`[sources]` does not carry is refused, listing the names that do exist. Reading an unmatched name
as a relative path instead would make a typo a *missing file* rather than a *declaration that does
not resolve*, and the message would come from a Parquet reader rather than from the document — the
same ambiguity this surface refuses everywhere else. It is also what lets `--file` key an override
by the source rather than by the object (§8): staging one file that three blocks read is one
override, where keying by the object made it three and left the one you missed quietly reading the
old file.

**`[defaults]` replaces `[corpus]`, and nothing is lost.** `[corpus]` named the one file every
attribute was read from and the one column its identity sat in, and no attribute could say
otherwise. Both are now defaults: a column may name its own `source` and its own `entity_id_field`,
because a file that carries entity ids can be joined whatever it calls them. What `[corpus]`
guaranteed — that every attribute lands in one entity space — is guaranteed by the entity id and
never was by the file.

```toml
[[attribute]]
name            = "sentiment"
field           = "score"
source          = "scores"
entity_id_field = "doc_id"
```

**`[defaults].source` reaches a `[[view]]` and an `[[attribute]]` and nothing else**, and the line
is where an absent source means *nothing* against where it means *something*. A view's geometry and
a column's values have to be read from somewhere, so a default fills them in. A vocabulary with no
source is one that **mints** rather than reads; a `[[layer]]` with no source is declared and empty;
a `[layer.members]` block is a membership that is not stored; a `point_visibility` with no source
reads the points' own column or takes its default. Filling any of those in would turn a declaration
into an acquisition nobody wrote.

**`[defaults].entity_id_field` reaches every source read under the canonical `entity_id`, and each
one may say otherwise** — a view through its own `fields.entity_id`, an attribute through its own
`entity_id_field`. The one it does not reach is `point_visibility`'s exploded relation, which takes
no `fields` map and so has no way to say otherwise; it is `(entity_id, term_id)` under those names
(§8). A default a block could not override would be a constraint rather than a default.

**`[[view]]`** — one named coordinate system. Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | identity; tombstoned on drop, never reused |
| `title` | O | human-readable, served on `/v1/meta` |
| `projection` | D `none` | `web_mercator`, `equirectangular`, `plate_carree`, `gall_isographic` or `none` — what turns this view's input coordinates into positions in its frame ([`projections.md`](projections.md) §5). See below |
| `source` | D | a `[sources]` key; `[defaults].source` where absent |
| `fields` | D | canonical `entity_id`, and `x`, `y` or `morton` + `residual` — or `lon`, `lat` under a projection. The geometry shapes are mutually exclusive (§8). `entity_id` defaults to `[defaults].entity_id_field` |
| `extent` | R | the quantisation frame: `"auto"`, `{ auto = true, margin = f }`, `{ min, max }` or `{ x = [a,b], y = [c,d] }` — and under a projection, `"auto"` or `{ lon = [a,b], lat = [c,d] }`. See below |
| `point_visibility` | R | `{ field, default }`, or `{ source, default }` — where each point's label is, and what a point carrying none gets. See below |
| `visibility` | D `public` | the view's own gate — an access label, or `public` ([`views.md`](views.md) §6). ⊘ No gate is evaluated, so a label is refused at parse and `public` is the only value that compiles |

**`[[view_group]]`** — a set of views sharing every setting, differing by a key and per-view
metadata ([`views.md`](views.md) §3,
[decision 0108](../decisions/0108-a-view-group-grows-by-its-roster.md)). Repeatable. **It takes
every `[[view]]` key above, with the same meaning**, and adds the four below. A group is not a
view: it cannot be named on a viewer verb and has no row space of its own; its views are, each
addressed `<group>:<key>` or `<group>:#<ordinal>`.

| Key | | Value |
|---|---|---|
| `members` | O | another `[[view_group]]`'s name: this group's views are that group's (views §3.3). Chains are refused, and a group naming it declares no `metadata` and no roster — those belong to the group that owns the keys |
| `metadata` | O | the per-view values a view carries, `name = type` over the `[[attribute]]` types; a category is `{ type = "category", vocabulary = … }`. A name the roster already uses — `key`, `source`, `visibility`, or the discriminator's own column — is refused |
| `[[view_group.view]]` | O, repeatable | **form A**: one view per block — `key`, `source`, `visibility`, and one key per declared metadata name. The file *is* the view, so the group declares no `source` of its own |
| `[view_group.views]` | O | **form B**: the roster as a table — `source` and `fields` over the canonical `key`, `visibility` and the metadata names — beside the group's own `source`, whose `fields.view` says which view each row of points lands in |

**The roster decides where the points come from**, and declaring both forms is refused, as `source`
beside inline `artifacts` is. A group declaring **neither** has its views minted from the
discriminator's distinct values and carries no metadata; it needs the group-level `source` that the
other two spellings of that arrangement need. **`[defaults].source` does not reach a group**: which
of those arrangements a defaulted file meant is not something a default can decide.

A view's `name` and a group's `name` and keys take the **column-name charset**, and `:`, `#` and
`@` are reserved out of them (views §3.2): the first two build a view id and the third pins a
group-scoped attribute to a view, so a name carrying one would make a request mean two things.

```toml
[[view_group]]
name             = "quarter"
extent           = { x = [-40.0, 40.0], y = [-40.0, 40.0] }
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us" }

[[view_group.view]]
key    = "2026-Q2"
source = "q2"
label  = "Q2 2026"
starts = 2026-04-01T00:00:00Z
```

**`[[vocabulary]]`** — a named value set. Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | identity; attributes share a vocabulary by naming it |
| `title` | O | human-readable |
| `width` | R | `u8` \| `u16` \| `u32` — the **code space's** width (`per-point-attributes.md` §3.6, `per-point-attributes.md` §3.9) |
| `value_set` | R | `closed` \| `open` — is an unknown key at ingest refused, or minted? |
| `visibility` | R | `public` \| `derived` — one axis, two settings; the slot takes no label ([decision 0090](../decisions/0090-a-vocabulary-has-one-visibility-axis.md)) |
| `source` | R for `closed`, unless inline | a `[sources]` key. **`[defaults].source` does not reach here** — an absent vocabulary source is a set that mints rather than reads (§8) |
| `fields` | D | canonical `key`, `code`, `title`; `code` may be absent — see below |
| `values` | R for `closed`, unless sourced | inline: an array of keys, or a `key = code` table |
| `reserved` | O | retired codes, never reassigned |

**`[[attribute]]`** — one per-point column, read from the source it names. Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | the served name, and the manifest's |
| `title` | O | human-readable |
| `field` | D | the source field, when it differs from `name` |
| `source` | D | a `[sources]` key; `[defaults].source` where absent |
| `entity_id_field` | D | the column this source spells the entity id in; `[defaults].entity_id_field` where absent |
| `type` | R | `bool`, `u8`…`u64`, `i8`…`i64`, `f32`, `f64`, `timestamp_us`, `text`, `keyword`, `category` |
| `vocabulary` | R for `category` | names a `[[vocabulary]]`; refused if undeclared |
| `render` | D `false` | a fixed-width slot in every row of `columns.arrow` |
| `index` | D `false` | the entity-space search structure |
| `analyser` | D | `text` only; `unicode` is the default and, today, the only one — see below |
| `multi` | ⊘ | refused at parse (`per-point-attributes.md` §3.7, records §6) |
| `render_in` | ⊘ | refused at parse (`per-point-attributes.md` §3.9) |
| `scope` | D `entity` | `"entity"` — one value per entity, under every view — or `{ group = "<view_group>" }`, one per view of that group ([`views.md`](views.md) §5, [decision 0109](../decisions/0109-scope-binds-an-attribute-or-layer-to-a-groups-views.md)). A scope naming a group that declares `members` is refused, pointing at the owner. A scoped attribute that names no `source` of its own is read from each of the group's views' own points files, so `[defaults].source` does not reach it. ⊘ The column family, the pinned leaf and the ingest rule are specified and not built |

**The extent is the frame every stored position is relative to**, and it belongs to the view rather
than to the invocation that built it. A coordinate is quantised across it into 32 bits — the top 16
naming the cell on the 65,536² grid, the bottom 16 the position within it — and quantisation
**clamps**, so a point outside lands on the boundary and the bundle is well-formed with the geometry
wrong. Two bundles built from one corpus under different extents place the same point in different
cells, which is why this is a property of the corpus and not a flag.

```toml
extent = "auto"                          # the square box around the data, with a small margin
extent = { auto = true, margin = 0.25 }  # a quarter of the data span as headroom on each side
extent = { min = -25.0, max = 25.0 }     # one range, both axes — preserves aspect ratio
extent = { x = [-18, 19], y = [-22, 24] }  # per axis, where stretching is meant
```

**`auto` squares the box** rather than fitting each axis tightly, so a circle stays a circle; a
per-axis fit would use the grid better and silently stretch the map, which is a rendering decision
the build has no business making. It also carries a **small default margin** — 1% of the data span
on each side — because the extent is a half-open interval and a point exactly at the maximum would
otherwise quantise to the clamp.

`auto` reads the view's points source to fit the box, which is a whole pass over its two coordinate
columns and the one thing here that costs anything. Parquet statistics are deliberately not used:
they are per row group and may be absent, so the extent — and therefore every stored cell — would
depend on how the producer laid the file out. A **Morton** points source has no coordinates to fit
a box around and is refused, naming the extent to write instead: codes are exact only against the
grid's own frame.

**`margin` is a fraction of the data span, added on each side**, and it exists for growth rather
than for tidiness: `auto` sees only the data present at build, so a corpus that will be written to
needs headroom or the first out-of-range ingest clamps. A caller who knows the bounds writes them.
There is no constant for the full float range: spanning ±3.4×10³⁸ over 65,536 cells makes each cell
10³⁴ wide, so every real dataset lands in one of them — it avoids clamping by destroying all
resolution.

**A `projection` makes the view a coordinate system on the Earth, and changes what its other three
keys mean.** The function itself, the closed set it is drawn from and the frame model it implies are
[`projections.md`](projections.md) §4–§5; what belongs here is the declaration.

```toml
[[view]]
name       = "world"
projection = "web_mercator"
extent     = { lon = [-8.6, 1.8], lat = [49.9, 60.9] }
```

- **The coordinate columns become `lon` and `lat`**, in that order, and `fields.x` or `fields.y` on
  such a view is refused naming the geographic spelling. A corpus built with the two exchanged is
  mirrored about the diagonal and nothing downstream can see that it is. `fields.lon` on a view with
  no projection is refused the same way: there is nothing to turn a degree into a coordinate.
  `morton`/`residual` is refused too — a code is a position already placed, so there is no longitude
  to transform.
- **The extent is written in longitude and latitude**, and is projected and then **snapped outward
  to the smallest aligned square containing it**, with the zoom offset capped at 16. `"auto"` is the
  same operation over the data's own longitude/latitude box. The other three spellings are refused:
  `{ min, max }` and `{ x, y }` state a frame in the space the projection *produces*, and
  `{ auto = true, margin = f }` asks for headroom the snap already supplies.
- **A coordinate outside ±180 or ±90 is not a coordinate**, in the extent or in a row, and is
  refused naming WGS84. So is a box crossing the antimeridian, which an aligned square cannot wrap;
  the refusal names the wider box that does not cross.
- **A latitude outside the projection's own domain is clipped, counted and never refused** — it is
  moved onto the frame's edge, where the clamp rule below says nothing is clamped, so the two counts
  are separate and neither can stand in for the other.

The frame a stated box snaps to is a function of the declaration alone, so `tessera check` prints it
— the square, and whether the offset cap chose it rather than the box. Under `"auto"` it cannot, the
frame being a function of the data, and it says so.

**Every build reports what the frame does to the data, and past half the corpus it refuses.** The
extent alone is four plausible-looking numbers whatever the corpus holds, so the build prints the
data's own bounds beside them, how much of the 65,536² grid that leaves the data occupying, and how
many points **clamp** — land on the frame's boundary rather than where they were written. The
report is unconditional, `auto` and a stated extent alike: `auto` only moves the trap, because a
caller who states a frame by hand is exactly the caller who gets it wrong, and a report they pay a
pass for is a report they would rather not have.

```
view 's0': quantising against x [0, 65536], y [0, 65536]
        the data spans x [-16.99, 17.76], y [-20.83, 22.55] — 18 x 23 of the 65536 x 65536 cells
        154 of 200 point(s) (77.0%) CLAMP onto the frame's edge — 109 on x, 97 on y
```

A point sitting exactly at the maximum is **not** clamped: cells are half-open and the maximum
lands in the top cell by construction, so counting it would report every tightly-fitted corpus as
damaged. The count is `v < min` or `v > max`, per axis.

**Past half the points, the build refuses**, and half is chosen for what a clamped point *is*: its
stored position is not its own, it is the frame's — so a frame that misplaces the majority of a
corpus is not that corpus's frame, it describes some other data. Below half a clamp is a tail —
outliers, headroom left for growth, a deliberately generous box — and the caller may well mean it,
which is why only this one is a refusal. There is **no flag that admits it**: quantisation clamps
rather than filters, so a frame chosen to crop piles the rest of the corpus onto the border instead
of excluding it, and filtering the source is what that caller wants.

The report costs one pass over the view's two coordinate columns — the pass `auto` was already
paying, now paid either way, which is what stops a caller avoiding it by writing their extent out.
A Morton points source arrives already placed, so nothing is quantised and nothing clamps; that
line says so instead.

**A frame goes wrong the other way too, and the clamp count cannot see it.** Data *outside* the
frame is pushed onto its edge, so those positions are actively wrong — that is the clamp above.
Data *tiny inside* the frame clamps nothing at all: every position is correct, and nearly all of
the resolution is gone, because points a long way apart in the source land in one cell and can no
longer be told apart. Coordinates spanning 100…118 against a 0…65536 frame do exactly this with
**zero** clamps. The bounding box printed above does not close the gap either, being derived from
the data's extremes: two far-flung outliers make the box span most of the grid while the rest of
the corpus shares a handful of cells.

So every build also reports **how many cells the points actually landed in**, counted exactly over
every point it placed, with the point count and the average points per occupied cell beside it. The
numbers are printed whether or not anything is wrong, so a frame this does not warn about is still
one the caller can judge, and silence never means nobody looked.

```
view 's0': 10000 point(s) landed in 122 distinct cell(s) of the 65536 x 65536 grid — 82.0 point(s) per occupied cell
view 's0': RESOLUTION LOST — on average 82.0 points share each occupied cell, so points that are
far apart in the source are stored at the same position and cannot be told apart. […]
```

**Below a tenth of the points having a position of their own, the build says so emphatically, and
it is never a refusal.** The reported figure is that proportion — 100% is a point per cell, 10%
means nine points in ten share a position with another. The obvious reading of *how full is the
grid* is not usable: a corpus can never occupy more cells than it has points, so a perfectly framed
ten-thousand-point build fills 0.0002% of the 4.3×10⁹ cells and a collapsed one fills 0.000003% —
both round to nothing, and the figure measures corpus size rather than the frame. Against the
corpus's own points those two builds read 100% and 1.2%. The measure is a proportion rather than a
count of cells because ten points in ten cells is a perfectly framed tiny corpus and only a count
would scold it.

A tenth is the line because 4.3×10⁹ cells exist, so points spread over the whole grid collide
rarely: about 99% of them keep a position of their own at 10⁸ and 89% at 10⁹, and a corpus
concentrated into a tenth of its frame's *area* still holds 89% at 10⁸.

**Where that argument stops holding, stated rather than glossed:** the ratio a uniform corpus
reaches depends on points per available cell, so it climbs with both scale and concentration. At
10⁹ points in a hundredth of the frame's area only 4% keep a position of their own, and that build
warns even under `extent = "auto"` — a dense core with two distant outliers stretching the box is
exactly such a corpus. At 10⁸ and below, which is this system's measured operating point, no concentration falls
that far. The warning is therefore reliable at the scale it was chosen for and can fire on a
well-framed 10⁹ corpus, which costs a line of output rather than a build.

The other corpus it warns about honestly and unhelpfully is a source whose positions genuinely
coincide. In both cases the printed numbers beside the data's own bounds are what let a caller tell
a real collapse from one of these.

Not a refusal, unlike the clamp: a clamped corpus is stored **wrong** and is worth stopping for,
while a sparse one is stored **correctly but coarsely** — a pilot corpus, a deliberately coarse
frame, or headroom left for data still to arrive are all reasons to mean it, and refusing would
block builds the caller intended. The count is taken at each build's segment write, off the codes
it is about to write to `morton.u32`: they are `(morton, tessera_id)` ascending by contract, so
distinct cells is one comparison per point with nothing retained, and both build paths — the
linear one and the streaming pipeline — report the identical figure.

**A point's label comes from a field or from a source, never both.** `field` names a field of the
view's own source, one value or a list per point. `source` names a separate exploded
`(entity_id, term_id)` relation — the shape the probe generators produce natively at 10⁹, and the
one the build writes as oracle output regardless, so the reader exists either way.

```toml
point_visibility = { field = "categories", default = "public" }   # a field of the points source
point_visibility = { source = "pairs", default = "public" }       # an exploded relation
point_visibility = { default = "public" }                         # no relation: every point takes the default
```

Declaring both is refused. `default` is legal alone and is the corpus with no permission model, so
the two acquisition keys are optional where `default` is not — a point's label has to come from
somewhere, and *nowhere* is a decision rather than an omission.

**A `field` is a `list<string>`, or a plain `string` where a point carries one term**, and its
terms are minted as an open vocabulary is: whatever the column holds becomes a term. Three rules
govern what a row means, and each is the fail-closed half of a plausible misreading:

- **A null value and an empty list both mean *no access terms*, which means visible to no
  principal.** Neither means unrestricted. Where a `default` is declared those are the rows it
  fills, so under `default = "public"` an unlabelled point is public and under a default nobody
  holds it is invisible — but the label is the one the declaration named, never *everyone*.
- **Terms are trimmed** of surrounding whitespace, matching what the plugin already does to the
  label it is handed, so ` cs.LG` and `cs.LG` are one term rather than two that no credential
  spells the same way. A term empty after trimming is not a term.
- **Filling never overrides.** A point carrying terms of its own keeps exactly those, and the build
  reports how many rows it filled. That is inadmissible rather than unwise: a point's terms are
  disjunctive — `M_auth` is a union of posting lists — so any label added to a point can only widen
  it.

⊘ **The `source` route fills nothing.** A point with no row in the exploded relation carries no
term and so sits in no principal's mask, where the same point read from a `field` would take the
default. The two should agree; not filling is the narrow half, so the divergence is a deferral
rather than a hole, and closing it needs the streaming build to know which ordinals the relation
never named.

**`public` is reserved at term `0`** (`per-point-attributes.md` §3.8). Every build interns it first,
so it is term 0 in every bundle and is minted for no other descriptor; every principal's resolved
term set contains it **by construction, inside the trust boundary** — not by grant, which would
make the one universal label depend on grant hygiene, and not in the plugin, which is
caller-supplied code deciding what a credential's bytes mean.

**A value set is *inline or sourced*, and its codes are *pinned or assigned* — two independent
choices.** Where the values come from is `source` against `values`; where the codes come from is
whether they are stated at all:

```toml
values = ["low", "medium", "high"]        # codes assigned by the build, in the order given
  [vocabulary.values]                     # or: codes pinned by the caller
  low = 1
  medium = 2
```

A bound source is the same pair: a `code` field pins, and its absence assigns. **A caller who does
not care which integer a value gets should not have to invent one** — pinning exists so a rebuild
preserves codes, not because choosing them is part of declaring a vocabulary.

**Assigned codes are recorded and replayed, exactly as the identity key and the term dictionary
are.** They are baked into every row, so the compiled vocabulary in `MANIFEST.json` is the record,
and a rebuild carries it: a value keeps its code, a new value takes the next free one, and a
removed value's code moves to `reserved` and is never reassigned. **Reordering the list does not
reorder the codes** once a vocabulary has been built — the carried record wins, and a build that
would have to change a live code refuses rather than silently recolouring history. Without that
rule, assignment by declaration order would make an editor's tidy-up rewrite the meaning of every
stored row.

Code `0` stays the *absent* sentinel: it is refused in a pinned set and never assigned
(`per-point-attributes.md` §3.6).

**The analysers, in full.** A `text` column's `analyser` selects the pipeline that produces its
tokens, and the set is closed the way every other value word here is: **a name this list does not
carry is refused, with no fallback** — falling back would index a column with a pipeline its
declaration did not ask for, which is the silent mismatch
[decision 0070](../decisions/0070-analysers-are-named-and-declared-per-column.md) exists to
prevent.

| Name | Identity | What it does |
|---|---|---|
| `unicode` | `unicode/icu4x-2.2/p1` | General prose: Unicode word segmentation, NFC normalisation and case folding, keeping segments carrying at least one `Alphabetic` or numeric code point. Every input comes from one pinned `icu4x`, so a data bump is a deliberate edit |

**The identity is resolved at parse and recorded per column** in the manifest, not per bundle,
because two `text` columns may be analysed differently — and because the failure it guards is
silent: an index built by one analyser and queried by another matches on precisely the strings
whose segmentation differs, with no error anywhere. Changing a column's analyser rebuilds that
column's index and nothing else.

⊘ **Segmentation quality in the no-space scripts is a known shortfall**, not a coverage hole: all
six dictionary scripts segment, but Japanese and Thai can mis-split word-internally, so a query for
the mis-split word does not find the document. `lindera` is the design's named escalation.

**`[[layer]]`** — one annotation layer. Repeatable. Artifact-side semantics are
`annotation-write-cycle.md` §6.1's; this is the declaration.

| Key | | Value |
|---|---|---|
| `name` | R | identity; tombstoned on drop |
| `title` | O | human-readable; absent is served as absent |
| `views` | R | the views this layer's artifacts are drawn on. A name here is a `[[view]]` or a whole `[[view_group]]`, and naming a group draws the layer on every view of it, present and future ([`views.md`](views.md) §3.5) |
| `scope` | D `entity` | `"entity"` — one artifact set, drawn on every view the layer names — or `{ group = "<view_group>" }`, a different set per view of that group (views §3.5, decision 0109). A scoped layer's rows carry a `view` column (`fields.view`), its artifacts are keyed per `(layer, view)`, and its `views` may name only that group and groups sharing its views |
| `source` | R unless inline | a `[sources]` key: one file per layer, so no discriminator field exists. Declaring it beside `artifacts` is refused. **`[defaults].source` does not reach here** — a layer with no source is declared and empty (§8) |
| `fields` | D | canonical `key`, `contents`, `parent`, `attached_layer`, `attached_key`, the shape kind's own columns (`min_x`, `min_y`, `max_x`, `max_y`; `cx`, `cy`, `r`; `cx`, `cy`, `a`, `b`, `angle`; `geometry`) and `space` on a layer declaring `shape`, and `members` or `excluding` where membership rides the artifact row. Naming both memberships is refused, as is a map beside inline `artifacts` |
| `default_space` | D `view` | the space the artifact table's shapes are written in where a row carries no `space` of its own ([`polygon-membership.md`](polygon-membership.md) §4.3). `view` is the space the points are stored in; `wgs84` is longitude and latitude, honoured on a view that declares a projection and refused on one that does not, and the shape goes through that view's own transform — the same function the points went through, which is what stops it selecting the wrong rows ([`projections.md`](projections.md) §10). Only on a layer declaring `shape` |
| `artifacts` | O | inline array, instead of `source`, for an authored layer — the keys below |
| `membership` | R | `enumerated` \| `spatial` \| `{ attribute = <field> }`. `{ attribute = f }` is a **predicate**: its artifacts are derived from the indexed column `f`, whose distinct values they are, so such a layer declares no `content`, no `depends_on`, no `levels`, no `artifact_visibility.field`, no `layout` and no hierarchy but `flat` — each of those would register a layer that is reachable and serves nothing. `spatial` reads `[layer.shape]`, and its artifacts are **published rows** each carrying a shape ([`polygon-membership.md`](polygon-membership.md) §6.2): a spatial layer may declare content, `depends_on`, `levels`, any hierarchy and a `layout` pin; what it may not declare is a proportional criterion or `artifact_visibility.field` |
| `value_set` | D `closed` | whether a member key the layer's artifacts do not declare is refused, or creates an artifact carrying nothing but its name ([`artifacts-from-points.md`](artifacts-from-points.md) §3). `closed` makes `artifacts` the roster; `open` makes it enrichment, so a cluster the points name and the table omits exists without a title, a cluster the table carries and no point names is an artifact with no members, and neither is an error. **It governs both entry points**: a build mints from a member source, and an ingest batch mints from a column named for the layer, at the close of the commit window that allocates the points. What `open` costs is that a mistyped key becomes a permanent object rather than a refusal — reported, at both entry points, and not bounded |
| `hierarchy` | R | `{ kind = flat \| nested \| stacked \| tiered, prune_children = bool }` — see below |
| `layout` | O | the **serving-layout pin**: `rows` (one row-space bitmap per artifact), `column` (one artifact label per row, for a level whose memberships partition the corpus) or `list` (a list of labels per row, where they overlap). Absent — the pick is automatic, taken **at the build** from the bundle's own row space and re-evaluated at every compaction fold ([decision 0094](../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)). What it reads is the level's **`everywhere` fraction** — how much of the level is too wide for any node of the tile index — and its artifact count; ⊘ the threshold on the first is provisional (`tessera_store::derived::ROW_MAJOR_EVERYWHERE_FRACTION`). Blocks per artifact is **reported and no longer read** (decision 0092's (c), now emitted by the build itself): the 2026-08-22 bracket moved it 6 → 12 with the cost *falling*, and the one quantity tracking the cost was the `everywhere` fraction. Present, it pins **every level of the layer**, at the build and at every fold after it, and a fold never overturns it. **Nothing on the wire names a layout** — both forms answer identically, so this is a latency choice and not a contract. A word outside the three is refused, and a pin on an attribute membership is refused: a single-valued attribute's membership *is* the column, so it has no second form for a pin to select between. A spatial layer takes a pin: its membership is resolved into a per-row source when a segment is published, and the pin selects between the same forms it selects between for an enumerated layer. ⊘ `column` on a level whose memberships turn out to **overlap** cannot be refused at parse — single-valuedness is a property of the data — so it is checked at the first fold, which composes the level artifact-major and says so in its trace rather than writing a column whose labels would each be whichever artifact wrote last |
| `shape` | O | `{ kind = "bbox" \| "circle" \| "ellipse" \| "polygon" }` — **only** on `membership = "spatial"`, and refused elsewhere as a rule nothing evaluates. The kind and nothing else: every kind is **exact** — the members are the rows whose stored position is inside the shape, closed on every side for a box, even-odd with an edge inside for a polygon ([`polygon-membership.md`](polygon-membership.md) §4.1) — so there is no depth, and a `depth` written is refused naming §6.1 of that design. Each artifact carries its geometry in the kind's own fields: `min_x`, `min_y`, `max_x`, `max_y` (inline `bbox = [min_x, min_y, max_x, max_y]`); `cx`, `cy`, `r` (`circle = [cx, cy, r]`); `cx`, `cy`, `a`, `b`, `angle` (`ellipse = [cx, cy, a, b, angle]`, the angle in degrees anticlockwise from the x axis); a WKB `geometry` column — GeoParquet's own name — for a polygon (`wkt = "POLYGON ((…))"` inline). A polygon is an OGC `MultiPolygon`: parts, each a ring and its holes. The shape is canonicalised at the build to the view's grid and what that did is **reported, never refused** — clipped to the extent, wholly outside, rings dropped, a table that looks written in degrees for a view that is not; what refuses is a coordinate that is not one, an inverted box, a non-positive radius or axis, and a polygon over the deployment's `max_shape_vertices` (default 10⁶). A row with no geometry is published with an empty shape and reported. A layer may span several views: the geometry is declared once and resolved per view; ⊘ views declare no `projection` yet, so a layer in more than one is warned rather than refused. ⊘ A `spatial` layer with **no** `shape` is the state this surface has always had: declared, registered, and holding nothing, because it has no shape to publish artifacts against |
| `visibility` | R | an access label, or `public` |
| `artifact_visibility` | R | `{ field, default }`; `default` may be `inherited` |
| `require_member_visibility` | R | `all` \| `any` \| `{ fraction = p }` \| `{ count = n }` \| `none` |
| `withdraw_on_member_deletion` | D `false` | drop the **whole artifact** when one of its members is deleted, rather than letting its membership shrink. ⊘ `true` is refused at parse: the fold has no artifact-withdrawal path (`annotation-write-cycle.md` §6.1) |
| `depends_on` | O | layers this one's edges point into; must be declared before it. **The edge carries deletion and visibility, neither configurable** ([decision 0089](../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)): an artifact here is deleted when the artifact it attaches to is deleted, and served only where that artifact is served. Every artifact of a layer declaring this must declare an attachment, into a layer named here — refused at build and at ingest alike |

**`artifacts = [{ … }]`** — one authored artifact, on the canonical field names. An inline row *is*
the canonical spelling, so there is no `fields` map beside it and no file for one to locate.

| Key | | Value |
|---|---|---|
| `key` | R | the caller's own name for it, which is what an edge into it names |
| `level` | D `0` | the resolution it sits at |
| `members` | O | the membership, by inclusion |
| `excluding` | O | the membership, by exclusion. Declaring both is refused |
| `bbox`, `circle`, `ellipse`, `wkt` | O | the artifact's shape, in its layer's kind's field and no other — `[min_x, min_y, max_x, max_y]`, `[cx, cy, r]`, `[cx, cy, a, b, angle]`, or WKT text. Refused on a layer declaring no `shape`. It is the artifact's whole membership |
| `space` | D `view` | the space this row's shape is written in, overriding the layer's `default_space` |
| `contents` | D `[]` | the ranking, best first: one entry per rank, each a value per supplied kind |
| `parent` | O | the parent artifact, by key |
| `attached_layer`, `attached_level`, `attached_key` | O | the edge this artifact hangs from; half an edge is refused |

and four sub-blocks, each below: `[layer.members]` (O), `[layer.content]` (O),
`[[layer.levels]]` (R for `stacked` and `tiered`, refused for `nested`) and `[layer.labels]` (O).
`[[layer.content.supplied]]` sits under `[layer.content]`, not under the layer, and
`[layer.labels.members]` under `[layer.labels]`.

**`withdraw_on_member_deletion` exists at two levels, and their defaults differ.** On `[[layer]]`
it governs the **artifact**; on `[layer.content]` it governs **supplied content** alone. Declaring
it on the layer subsumes the content one — there is no content left to withdraw once the artifact
is gone.

- **On the layer, the default is `false`**, and keeping the artifact is safe rather than merely
  convenient: everything the artifact carries of its own is either its identity or a **computed**
  property, and computed properties are recomputed per viewer from current membership, so a
  surviving artifact carries no residue of the deleted member. Setting it `true` is a *semantic*
  declaration — *this set is the object, so it is no longer that object* — which is right for a
  curated set or a case file and wrong for a cluster, whose membership was always going to move.
- **On the content, the default is `true`**, because there the residue is real: supplied content
  was generated from a set including the deleted item, and serving it on afterwards lets a
  principal satisfying the survivors read something derived from what was removed. That one is a
  disclosure control, so its default is the half that cannot widen (C7).

The two defaults therefore point opposite ways for the same rule, which is not an inconsistency:
one is a semantic choice with no disclosure content, and the other is a disclosure control. What
withdrawal does to artifacts attached to the withdrawn one is the write cycle's
(`annotation-write-cycle.md` §5), not this document's — a dangling dependent is refused rather than
repaired, and nothing here changes that.

**`[layer.members]`** — membership as its own source, for a membership no single cell should hold.

| Key | | Value |
|---|---|---|
| `source` | R | a `[sources]` key; one row per `(artifact, entity)`. **`[defaults].source` does not reach here** either |
| `fields` | D | canonical `key`, `entity`, `rank` — a null `rank` is the artifact's own membership, `k` the generating set of `contents[k]` |

A member source without the layer's own artifacts — its `source` or its inline `artifacts` — is
refused **while the layer's value set is closed**: they are the roster a member row's key resolves
against, and without one a mistyped key would publish a phantom artifact rather than fail to find
one. Under `value_set = "open"` that is the declaration rather than the mistake — a bare clustering
has no artifact table and its clusters exist because its points name them.

**A member source's `key` may be text or an integer**, and an integer is read as its decimal
spelling, so `3` and `"3"` name one artifact. A **null** key, and exactly `-1`, mean *this point is
in no artifact*: the row is skipped and counted, never refused, noise being a fifth to a quarter of
the points at each split of a condensed tree. The count is printed and reaches the build report.

**It may also be a *list* of those**, which is what a hierarchical clusterer emits — one row per
point, naming every artifact the point belongs to. Each entry is a key on the rule above, and what
the positions mean is the hierarchy kind the layer already declares
([`artifacts-from-points.md`](artifacts-from-points.md) §4): one entry per declared level under
`stacked` and `tiered`, whose consecutive entries are `tiered`'s containment edges, and a lineage
under `nested`, where entry *k* is the parent of entry *k+1* and every artifact sits at level 0. The
declaration and the data must agree — a variable-length list against `stacked` or `tiered`, or a
fixed-size one against `nested`, is refused rather than guessed, since choosing one reading would
publish a hierarchy the caller did not write. **Under `flat` a list is plain multi-membership**: no
positions are read, and the point is a member of every artifact its list names, which is what the
same membership written as several member rows has always meant. So is **a child named under
two different parents**, whether the two come from two rows of the column or from the column and an
artifact row's `parent`. A null or `-1` entry places the point at no artifact *at that level* and
links nothing across itself; a row of nothing but those is one unclustered row. A `level` column
beside a list key is ignored and said so, the positions being what carry the levels.

**The four hierarchy kinds, and which of them carry levels.** The kind is declared and never
inferred from the edges, and the levels rule follows from it:

| `kind` | Lineage | `[[layer.levels]]` |
|---|---|---|
| `flat` | none | optional |
| `nested` | a tree in the edges, every artifact at level 0 | **refused** — a tree's structure is its edges, not a ladder |
| `stacked` | none; independent analyses, one per level | **required** |
| `tiered` | containment edges running coarser → finer between levels | **required** |

`nested` and `tiered` differ in what their edges are *for* — roll-up the cut climbs, against
information a client nests with — which is
[decision 0087](../decisions/0087-cross-level-edges-are-information-not-rollup.md)'s subject and
`annotations.md`'s to state; the config's part is that the two are declared, never guessed. Three
further refusals: a layer declaring no lineage that carries an edge, an edge running against the
levels, and a parent key resolving in two coarser levels.

**`[[layer.levels]]`** — one resolution. Repeatable, and **the set must be exactly `0..n`**: levels
are addressed by `entity − level.entity_base` over a reserved run, so a repeated number would give
two levels one base and a gap would reserve a run no address reaches.

| Key | | Value |
|---|---|---|
| `level` | R | the number. **Explicit, not array position**, because edges reference `(layer, level, ordinal)` and reordering the file would silently renumber them |
| `title` | O | human-readable; the metadata endpoint publishes the zoom→level map |
| `zoom` | O | `[min, max]` — **the default bound on this layer's response** (2026-08-28). A `/v1/viewport` naming no `levels` is answered at the levels whose range covers the depth asked at; naming them overrides it, and a layer where no level declares a range serves every level. A treed layer declares no levels, so its response is bounded by the request's artifact budget as before. *(Was advisory and bounded nothing: this table asserted the bound while nothing on the wire could ask for a level.)* |

**`[layer.content]`** — what this layer's artifacts carry, in three parts.

| Key | | Value |
|---|---|---|
| `computed` | D `[]` | properties the engine recomputes per viewer from `membership ∩ M_auth` and nothing else: `centroid`, `hull`, `box`, and ⊘ `extractive_terms`, which is specified and not implemented
(`annotations.md` §4.2) and refused at registration. **Contained by construction**, so they take no visibility declaration and pass containment automatically. The masked count is intrinsic and is never declared here |
| `supplied` | D `[]` | `[[layer.content.supplied]]` entries, below |
| `withdraw_on_member_deletion` | D `true` | drop supplied content when one of its generating set is deleted, rather than shrinking the set |

**`withdraw_on_member_deletion` decides whether supplied content outlives its sources.**

- **`true`**, the default — the content and its generating set are dropped together at the fold and
  the caller regenerates. Right where the exact membership *is* the object: a curated set, a case
  file. Containment being all-or-nothing, a set that loses a member would otherwise fail for every
  principal for ever.
- **`false`** — the fold removes the deleted member and the content goes on serving. Right where
  the membership is statistical — a topic label drawn from documents it does not enumerate — and it
  means a principal satisfying the survivors may read content generated from the deleted item.

That second one is a **caller's declaration and never a service behaviour** (C7), which is why
`true` is the default: the widening half is the one that must be typed.

It was spelled `on_member_deletion`, with values `withdraw_content` and `shrink_generating_set`.
Both values repeated the object the key had already supplied, and as a boolean it now reads on the
same rule as `render`, `index` and `prune_children` — an imperative saying what the build should do.

⊘ **It sits on `[layer.content]`, and probably belongs on each supplied kind.** One layer may
legitimately carry a curated boundary polygon and a statistical label over the same artifacts, and
today they must share this declaration. Recorded, not changed: it is a model question for
`annotations.md` rather than a spelling one.

**`[[layer.content.supplied]]`** — one kind of content a caller supplies on this layer's artifacts.
Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | distinguishes two contents of one type |
| `type` | R | `text`, `polygon`, `extent`, `point` — published on `/v1/meta` so a client knows what to draw |
| `require_member_visibility` | R | `all` for content generated from documents; `inherited` for content true whether or not a document exists. The second **must not** declare a generating set, a set never tested being a claim the service would carry without meaning (C28) |

**`[layer.labels]`** — sugar expanding to a layer of its own (`annotation-write-cycle.md` §6.1):
same views, flat, `depends_on` the parent, content wrapper supplied.

| Key | | Value |
|---|---|---|
| `name`, `title`, `source`, `fields`, `[layer.labels.members]` | as `[[layer]]` | |
| `type` | R | the content type — `text`, `polygon`, `extent`, `point` |
| `membership` | R | written out: a label's members are its generating set, which the build cannot derive |
| `require_member_visibility` | R | the **layer** grain: how much of a label's membership a viewer must see for the label to appear |
| `artifact_visibility` | R | as `[[layer]]`; declared, never supplied |
| `[layer.labels.content]` | R | one key, `require_member_visibility` — **where the text came from**: `all` if it was generated from the documents it names, `inherited` if it is true whether or not any of them exists |
| `visibility` | D | the parent layer's; any declared label is taken as written, because the dependency edge is what bounds it — see below |

**The two requirements are not one dial at two grains, and that is why they are two keys.** The
layer's is a threshold — *how much of this set must a viewer already see for the label to appear at
all* — and admits `{ fraction = p }` and `{ count = n }`. The content's is a **provenance
declaration** and admits exactly two words: `all` says the text is a synthesis of the members, so a
viewer reads it only where it can already read every document that went into it; `inherited` says
the text is true whether or not any of those documents exists — a name a person wrote — and so adds
no requirement beyond the artifact's own gate.

**On a `text` label the two normally agree at `all`**, and a threshold looser than the content's is
usually a mistake rather than a subtlety: the sugar supplies no computed properties, so an artifact
whose text is withheld has nothing left to draw. The case for their differing is `inherited`
content — a curated region whose name was authored rather than derived, which a viewer may read
having seen only part of the set. Where both grains genuinely need different thresholds, that is
past what the sugar is for: write the second `[[layer]]` out.

**The expansion is textual, and that is the whole claim**: the block above becomes a `[[layer]]`
block before anything compiles, so it meets every refusal and every reader a hand-written layer
meets, and **a declaration written this way and the same one written out as a second `[[layer]]`
build a byte-identical bundle**. Five things are supplied — the parent's `views`,
`hierarchy = { kind = "flat" }`, `depends_on = [the parent]`, one
`[[layer.content.supplied]]` entry named for the label layer, typed by `type` and carrying the
caller's own `[layer.labels.content]` requirement.

**The sugar supplies mechanism and not one disclosure control.** Everything it fills in is
structural — which views, which shape, which parent, which wrapper — and every control is the
caller's: both member requirements and the artifact gate are required keys, and `visibility` is
the single defaulted one below. An earlier expansion fixed the content requirement at `all` and
the artifact gate at `inherited`; both were choices belonging to the caller, and a value an
expansion picks for a caller who wrote nothing is a value nobody chose.

**`visibility` is the one defaulted disclosure control in this surface**, and it is admissible
because the value it defaults to is the parent layer's own and never the widest one. **A gate
declared here is taken as written, and nothing compares it against the parent's.** It does not need
to: a label is served only where the cluster it attaches to is served
([decision 0089](../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)), so a
gate written here can narrow what a principal sees and cannot widen it. A `public` label layer
under a parent gated on `ir:analyst` discloses nothing — the viewer who cannot reach the cluster
cannot reach its labels either — and two distinct opaque labels need no ordering, which is
fortunate, because whether every principal holding one also holds the other is a fact about grants
and grants do not exist in the declaration.

The expansion used to refuse the `public` case, and that refusal is deleted. It was the one place
the sugar was *stricter* than the two `[[layer]]` blocks it expands to, escapable by writing them
out — and the rule that made it unnecessary is stronger than the check ever was, because it holds
for the two hand-written blocks as well. **A check became a property.**

**Three words are reserved**, and only one occupies a slot that otherwise takes a caller's label:

| Word | Where | Collides with a label? |
|---|---|---|
| `public` | anywhere a label appears | **No** — it *is* a label, reserved at term `0` (`per-point-attributes.md` §3.8) |
| `derived` | a vocabulary's `visibility` | **No** — that slot takes no label ([decision 0090](../decisions/0090-a-vocabulary-has-one-visibility-axis.md)) |
| `inherited` | a member default, and supplied content | **Yes** — an access label spelled `inherited` is refused at parse |

## 2. Declaring without building

**The surface has two halves, and only one of them is about a build.** Everything that says what an
object *is* — its identity, its type, its visibility, its membership rule — is route-independent.
Everything that says *where its rows come from* is a build input and is simply absent from a
deployment that writes through the service instead.

| Half | Keys | On the write path |
|---|---|---|
| Declaration | everything else | identical, and required |
| Acquisition | `source`, `fields`, inline `artifacts`, and the `field` half of `point_visibility` / `artifact_visibility` | omitted |

**A deployment that never builds still builds once**, and that is the whole of the answer to *where
does the schema live*: `tessera build` over a declaration with no sources compiles it into a
bundle with no rows in it, and the service writes into that. The schema never appears in the
server's own half of `tessera.toml` (§4), so an empty bundle is what carries it — and a config with
no sources is a legal config rather than a special mode.

**The declaration is already what the write path consumes**, which is why this costs nothing:

- **Attributes.** `/control/ingest` takes a row as `(external_id, x, y, access, …declared scalars)`
  and builds each row's vector **in declared order**, so the attribute list *is* the wire schema.
  Declaration order is load-bearing on both routes for the same reason.
- **Vocabularies.** A category arrives as a `utf8` **value key**, never a code — codes are the
  server's to assign. So `value_set` governs ingest exactly as it governs a build: an unknown key
  under `closed` is a `422` naming the column and the key, whole batch without effect, and under
  `open` it is minted. A closed vocabulary's authored values are therefore required on both routes;
  they are declaration, not acquisition, even when written inline.
- **Labels.** The row carries `access` itself, so `point_visibility.field` — which names a *column*
  — has nothing to name on the wire and is build-only. Its `default` is not: it is what a row
  supplying no label gets, on either route.
- **Layers.** `PUT /control/layers` takes the same declaration as JSON, and the build runs the same
  registry and allocator the control plane runs (`annotation-write-cycle.md` §6.1). A `[[layer]]`
  block minus its acquisition keys *is* that payload. `[layer.labels]` expands to a second
  declaration, so the sugar is available to both.
- **Memberships.** A point names its artifacts in a column **named for the layer** — the
  declaration's `name`, exactly as an attribute column is named for the attribute's `name`
  ([`artifacts-from-points.md`](artifacts-from-points.md) §6.2). `[layer.members]`'s `source` and
  `fields` are the acquisition half and stay build-only for the reason above: they say which *file*
  and which of its columns, which is nothing a running node could check. The values and the list
  rules are identical on both routes.
- **Views.** A view is created online by a control verb carrying `{name, gate, projection
  provenance}`, or declared here and compiled. Same object either way.

**What differs is only what a missing source means.** At build, a `source` naming no key in
`[sources]` is a refusal listing the names that exist (§8). With no source declared at all, the object is declared
and empty — a layer with no artifacts yet, a vocabulary with no minted values yet — which is a
legal state and the normal one for a write-path deployment. An empty *closed* vocabulary is still
refused, because closed means the set is authored and an authored set of nothing refuses every
ingest.

**What may be declared *after* the first build differs by object, and the differences are not
arbitrary.**

| Object | Declared later? |
|---|---|
| Artifacts, layers | **Yes, today.** That is the control plane's whole job; the build plane exists only so a 10⁷-artifact level need not ride the trickle path (`annotation-write-cycle.md` §6.1) |
| Vocabulary values | **By design, no endpoint.** Appending a value and retiring one are both safe — a new code is assigned, a retired one moves to `reserved` and is never reassigned (`per-point-attributes.md` §2.2) — but ⊘ `/control/categories` is still owed, so today the route exists on paper only |
| Views | ⊘ **Specified, not implemented.** Creation is a control verb carrying `{name, gate, projection provenance}`, WAL'd and materialised at the next flush (views §6); a bundle has one coordinate system, so nothing evaluates it yet |
| Attributes | **Deliberately not online.** Adding `index` is a build pass with no row rewrite; adding `render` rewrites every segment; changing a width or a type is refused outright. The convention is Elasticsearch's, and stolen on purpose: *mappings are immutable; you reindex* |

⊘ **Declaring a wholly new attribute after a build is not specified**, as distinct from altering an
existing one, and it is not simply the append it looks like: the scalar tail is stored and read back
**positionally**, so segments built before and after the addition disagree about the tail's length.
That is a records-and-search question rather than a configuration one; it is named here because a
caller reading §2's *declare now, write later* will reasonably expect it and there is no answer to
give them.

**`tessera check --payloads` writes the layer bodies out**, which is what closes the gap between
declaring here and creating online. A write-path deployment used to author every layer twice —
once as TOML to compile the empty bundle, once as JSON to `PUT /control/layers` — and the parser
has produced the second by the time it could print it. The output is a JSON array of
`LayerDeclaration` bodies in declaration order, on stdout alone, so a CI job pipes it straight at
the control plane. It is a **serialisation and not a translation**: the payload type is the type
the build compiled to, so there is no second authority to drift, and `[layer.labels]` appears as
the layer it expands to.

⊘ **Views and vocabularies have no payload to write**, and their absence here is the endpoints'
rather than this verb's: view creation is specified and not implemented (views §6), and
`/control/categories` is owed. When either lands its payload is the same serialisation.

## 3. Three files, three jobs

The declaration is one of three inputs, and the split is what keeps a secret out of git and a
machine-specific path out of a portable file:

| File | Holds | In git |
|---|---|---|
| `schema.toml` | **what the corpus is** — sources, attributes, vocabularies, views, layers, extent | yes |
| `tessera.toml` | **what this deployment is** — bundle path, cache and WAL directories, listen addresses, tuning, and which environment variable carries the identity key | yes, minus the secret |
| the environment, or a `.env` beside it | **the secrets** — `TESSERA_IDENTITY_KEY` | never |

`tessera build` and `tessera serve` both read `tessera.toml`, found by walking up from the working
directory as `Cargo.toml` is; `--deployment <path>` names one outright. A build's output path and a
server's `bundle_path` are one value seen from two sides, so they are declared once. In the
ordinary case neither verb takes a flag:

```bash
tessera build      # finds tessera.toml, reads the declaration, writes the declared bundle path
tessera check      # same file, same declaration, same overrides — schemas only, no row read
tessera serve      # same file, opens what that build wrote
```

**`tessera check` is the seconds-long half of a build, and it is what a CI job calls.** It resolves
exactly what `tessera build` resolves — the same deployment file found the same way, the same
declaration, the same `--file` overrides, through the same code, so a check cannot see a different
set of files from the build it guards. It then opens each source's Parquet **footer** and asks
three things of it: that every declared attribute's column is there and can carry the type declared
for it, that every source a declaration names is present and readable, and that every field a
`fields` map locates exists in the file it locates it in. Every layer's `views` and every
disclosure decision are already settled by the parse. It prints those decisions as a table, or —
under `--payloads` — the control-plane bodies (§2).

**It collects rather than stopping.** A build refuses at the first thing wrong because everything
after it is work nobody wants; a check exists to be run and fixed in one pass, so it reports every
finding and its exit status is the summary.

⊘ **A clean check is not a clean build**, and the list of what it cannot see is short and worth
knowing: whether a closed vocabulary's keys cover the values in the data, whether a member id
resolves to an entity the build would assign, whether two artifact rows share a key, whether the
hierarchy's edges contain one another, and where the data actually sits inside its view's extent.
All five need a row. The last is the build's own clamp report (§1). It also cannot see a canonical
column a `fields` map never named and the file does not carry — the readers treat that as absent by
design, `fields` locating what is declared rather than asserting it (§8).

**Two reports land beside the bundle**, in `reports/`, and neither is read by anything that serves:

| File | Written by | Holds |
|---|---|---|
| `containment.json` | the build | edges whose child holds a member its parent does not, and the splits that lose the most |
| `disclosure.json` | the build, and `tessera check` computes the same document | every layer's `visibility`, `require_member_visibility` and **`membership`** — the last spelled `enumerated`, `attribute:<field>` or `spatial:<kind>`, because a predicate *is* who belongs and every shape kind is exact — every vocabulary's `visibility` and `value_set`, every attribute's placement, each view's point-label default, and which layers `[layer.labels]` wrote and for whom |

**`disclosure.json` exists for the diff.** The controls it records are individually small and
collectively the whole of who may see what, and a reviewer's real question — *which disclosure
decision moved between these two builds?* — has no cheap answer from a config written to be
authored or from a manifest interleaved with segment digests. So it carries **no timestamp, no
path and no machine-dependent value**, keeps declaration order where declaration order is
load-bearing (attributes, layers) and sorts what has none (vocabularies), and is derived from the
declaration and from nothing the build computes — which is why the check can emit it without
opening a data file. `containment.json` is the other way round: a result, needing every artifact
published. A declaration with no layer, no vocabulary and no attribute writes neither and creates
no `reports/` — a bare-geometry bundle's only control is the label every point takes, and creating
the directory an operator polls for the fold's notices to say nothing is worse than saying nothing.

A layer's `depends_on` is in there, which is not obviously a disclosure decision and is one: an
artifact is served only where the artifact it attaches to is served
([decision 0089](../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)), so
adding an edge narrows this layer and removing one widens it, neither visible in the layer's own
gate.

**A missing `tessera.toml` is a refusal naming what to create**, never a silent set of defaults:
every path in it is a decision, and a guessed one is a build writing where nobody asked or a server
opening a bundle nobody built. Three keys are this document's; the rest are the serving side's
(SA §7):

```toml
[bundle]
path  = "bundles/corpus"          # tessera build --out writes here; tessera serve opens it
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "schema.toml"            # the corpus declaration. This is the default

[identity]
env = "TESSERA_IDENTITY_KEY"      # the variable carrying the key, never the key. This is the default
```

**Two of the serving side's keys belong in this document even so**, because they are disclosure
controls rather than tuning and §1's closure argument applies to them: a control nobody has
enumerated is a control nobody has reasoned about. Both name browser origins, both are absent by
default, and **neither takes a wildcard** — a list carrying `*` is refused at startup, naming the
key, because an origin list is a deployment's statement about which pages may call it and `*` is
not a statement.

| Key | | Which planes | Value |
|---|---|---|---|
| `serve.dev_cors_origins` | O | viewer **and session** | a development affordance. It opens `/session/authorise` to a browser, which means the page holds the deployment's **session credential** — the secret that decides who may mint tokens at all. Logged at `warn` on every start |
| `serve.cors_origins` | O | **viewer only** | the production list ([decision 0102](../decisions/0102-the-viewer-plane-gains-an-enumerated-cors-origin-list.md)). A page it names may present a **token** — already per-principal, already scoped to what the server decided that principal may see, already expiring — and can reach `/session/authorise` no more than any other origin can. Silent at startup |

**The difference between them is which bearer a browser ends up holding**, and that is why the
production key stops at the viewer plane rather than covering both for symmetry. Letting a named
origin present a token creates no authority that did not already exist; letting one present the
credential creates the authority to mint tokens for anybody. Both lists may be set at once, and an
origin appearing in both is not an error.

**An origin list is not an authorisation boundary**, and nothing in the engine may come to treat it
as one. It decides which page a browser will hand a response to. What the response *contains* is
settled by the bearer and by `M_auth`, before CORS is consulted at all — which is also why the six
headers a client keys and revalidates a replica by are explicitly exposed (`delta-serving.md` §2):
a browser that cannot read them is a browser that cannot cache, not one that is being protected
from something. The control plane is never wrapped by either list.

The rest of `[serve]` and all of `[ingest]` are tuning, documented at SA §7 under its own rule —
performance knobs default, disclosure controls do not. Four of them are the shape work's and are
named here because a reader of `[layer.shape]` will look for them: `max_shape_vertices` (default
10⁶ — a published shape over it is refused at the build and at `PUT /control/layers`, the one
input a caller can simplify; the held decomposition is reported and never capped),
`max_region_vertices` (10,000 — a `region` leaf's polygon over it is `422` naming the cap),
`max_region_cells` (262,144 — the crossing tiles a region leaf's descent may hold at one depth;
**not a refusal**: over it the descent stops at the deepest depth that fits and the answer is a
cover, said on `x-tessera-region`), and `region_cache_bytes` (256 MiB — the decomposition cache,
shared across principals, pruned per generation; eviction costs latency and nothing else). The
first three are published on `/v1/meta` so a client can predict a refusal rather than discover it
([`polygon-membership.md`](polygon-membership.md) §9, [`selection-operand.md`](selection-operand.md)
§2).

**Every path in it resolves against its own directory**, on the same rule a `source` follows — a
relative `bundle.path` that moved with the shell's working directory would make `cd crates &&
tessera serve` open a different bundle from the one `tessera build` had just written. Both
`[build]` and `[identity]` default entire, so the ordinary file writes neither.

**Sources are paths relative to the declaring file, written once in `[sources]`.** An earlier
revision forbade every path here and bound each source on the command line, which produced an
invocation naming five files and a config that could not be read without it. The rule it was
protecting is narrower than it was written: what must not appear is an **absolute or
machine-specific** path, and a path relative to the config is neither — it travels in git with the
file that describes it and is exactly as reproducible as the declaration around it. A later
revision then wrote that path at every block that read the file, so one file read by three blocks
was three paths to keep in step; `[sources]` is that table pulled out, and every `source` elsewhere
names one of its keys. `--file NAME=PATH` survives as an **override**, for the deployment that
stages one source elsewhere, and it now moves every reader of that source at once.

**The identity key never appears in either file.** It is sixteen bytes keying the bijection that
turns an internal entity id into the `tessera_id` a client sees
([decision 0005](../decisions/0005-tessera-id-keyed-bijection.md)) and it is part of the storage
sort key, so rotating it invalidates every identifier a client holds and reorders every row. It is
read from the environment, which `tessera.toml` names, or from a `.env` beside that file where the
process environment carries none; `--identity-file <path>` is accepted as an alternative, since a
`0600` file is not readable from `/proc` where an environment is. **There is no flag that takes a
key**: one on a command line reaches shell history, process listings and CI logs. Writing `key`
into `tessera.toml` is refused with its own message rather than as an unknown field, the mistake
being a reasonable one — `--identity-file`'s own format does spell it `key`.

The flags that survive are *decisions about* the key, not the key: `--mint-id-key`,
`--carry-id-key-from`, `--rotate-id-key`, `--bump-idset`, `--idset`. A build with no key from
anywhere and none of those refuses **before any work**, naming the variable this deployment's own
file asked for.

## 4. A build input, not server config

The build config never appears in the *server's* config; the server reads the compiled schema from
the bundle's `MANIFEST.json`. A server reading a schema of its own could be restarted against a
bundle whose columns disagree, and the mismatch would surface as wrong codes rather than a startup
error. Config compiles to manifest, source to binary. **The capability model does not reach the
manifest**, which stays flat and per-placement — hot columns, filter operands, categories — because
a reader should never need to understand intent to know what to load.

**One file, named by `tessera.toml`.** Attributes, vocabularies, views and layers are declared
together; `build.schema` names it, defaulting to `schema.toml` beside the deployment file, and
`--config` overrides that. Corollary: **no absolute or machine-specific paths in it** — every path
sits in `[sources]`, written relative to the document, an absolute one is refused at parse, and
`--file NAME=PATH` is where a staged path goes (§3, §8).

## 5. Two axes, and only two

Everything about who may see an object answers one of two questions
([decision 0088](../decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md)):

| Axis | Key | Answers |
|---|---|---|
| label-based | `visibility` | which access label must the viewer hold |
| membership-based | `require_member_visibility` | how much of this object's membership must the viewer already see |

```toml
visibility = "public"        # the reserved label every principal holds (`per-point-attributes.md` §3.8)
visibility = "ir:analyst"    # any other access label
require_member_visibility = "all" | "any" | { fraction = … } | { count = … } | "none"
```

The second key **requires** members to be visible and never *sets* their visibility: a container
grants its members nothing, and the reverse reading inverts the direction `per-point-attributes.md` §3.3 protects. It
replaces `visible_when` on a layer, `corpus_derived` on supplied content, and a vocabulary's
`per_viewer` listing, which were one test at three settings.

**A container's own gate, and a default for members that declare none**, are separate keys and
compose as conjunction:

```toml
visibility          = "ir:analyst"                              # the container itself
artifact_visibility = { field = "visibility", default = "inherited" }  # each member, and the fallback
```

The presence of `field` is the declaration that members carry their own labels — what
`artifacts_carry_own` said (C27) — and `default` is what a member carrying none gets. For points
the same pair is `point_visibility` on the view. A member default of `"inherited"` means the
container's gate is the whole of it; it is legal for artifacts and **not** for points, since a
point carrying no terms is in no posting list and so in no principal's mask, and a gate narrows
rather than widens.

**Filling never overrides.** A member carrying its own label keeps exactly that; the default lands
only where the field is null or empty, and the build reports how many rows it filled. Overriding is
inadmissible for points rather than merely unwise: a point's terms are disjunctive — `M_auth` is a
union of posting lists — so any label added to a point can only widen it.

## 6. The declaration

```toml
[sources]                            # every file this declaration reads, named once
corpus            = "corpus.parquet"
geometry          = "umap.parquet"
sentiment         = "sentiment.parquet"
hdbscan           = "hdbscan.parquet"
hdbscan_members   = "hdbscan_members.parquet"
hdbscan_topics    = "hdbscan_topics.parquet"
hdbscan_topic_members = "hdbscan_topic_members.parquet"

[defaults]
source          = "corpus"           # entity space: what a block that names none reads
entity_id_field = "id"               # how this corpus spells identity, wherever one is read

[[view]]
name             = "s0"
title            = "arXiv, August 2026"
source           = "geometry"        # this one is a different file, so it says so
fields           = { x = "x", y = "y" }
point_visibility = { field = "categories", default = "public" }

[[vocabulary]]
name       = "severity"
title      = "Severity"
width      = "u8"
value_set  = "closed"                # or "open": may ingest mint a key nobody declared?
visibility = "public"
reserved   = [5]                     # retired codes, never reassigned
  [vocabulary.values]
  low = 1
  medium = 2
  high = 3

[[attribute]]
name       = "severity"
title      = "Severity"
type       = "category"
vocabulary = "severity"
render     = true
index      = true

[[attribute]]
name  = "notes"
type  = "keyword"                    # neither flag: blob-resident, drill-down alone

# A column from somewhere else, joined on a column that file calls something else. Neither is a
# second corpus: the entity id is what puts it in this entity space, and the file never was.
[[attribute]]
name            = "sentiment"
field           = "score"
type            = "f32"
source          = "sentiment"
entity_id_field = "doc_id"

[[layer]]
source     = "hdbscan"               # one row per artifact
fields     = { members = "members", parent = "parent_id" }
name       = "clusters/hdbscan"
title      = "HDBSCAN clusters"
views      = ["s0"]
membership = "enumerated"
hierarchy  = { kind = "nested", prune_children = true }

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = { fraction = 0.05 }
content                   = { computed = ["centroid", "box"] }

  # Membership in its own source instead of a list field, where a single cell
  # would have to hold the whole corpus. One of the two, never both.
  [layer.members]
  source = "hdbscan_members"         # one row per (artifact, entity)

  [layer.labels]
  source                    = "hdbscan_topics"
  fields                    = { contents = "text" }
  name                      = "topics/hdbscan"
  title                     = "HDBSCAN topics"
  type                      = "text"
  membership                = "enumerated"
  artifact_visibility       = { default = "inherited" }
  require_member_visibility = "all"

    # The topic text is a synthesis of the abstracts it was drawn from, so it is read
    # only by a viewer who can already read all of them. A hand-authored name would be
    # `inherited` here, and the threshold above could then be looser than `all`.
    [layer.labels.content]
    require_member_visibility = "all"

    # A label's members are the documents it was generated from, and a ranked content's
    # are the documents that rank was generated from — one row per (artifact, rank, entity).
    [layer.labels.members]
    source = "hdbscan_topic_members"
```

**Every block of that example builds**, `[layer.labels]` included: it expands to a
`topics/hdbscan` layer drawn on `s0`, flat, depending on `clusters/hdbscan`, carrying one supplied
`text` content, and gated `public` because that is what its parent declared.

**One file's path is written once, and moving it is one flag.** `--file corpus=/mnt/staged/corpus.parquet`
moves the view's geometry, every attribute reading it and any relation named on it together; there
is no per-block key to miss.

**A vocabulary is an object, not three attribute fields.** `name` is the identity, so attributes
share one by naming it; `source` or an inline `[vocabulary.values]` table is where the values come
from; `value_set` decides whether an unknown key at ingest is refused or minted; `width` belongs
here because it is the **code space's** width, not a column's. This retires `values_key`,
`values_of` and `vocabulary = "declared"|"discovered"` — one mechanism where there were four.

**A title is always optional.** It is presentation metadata on an object whose visibility is
already decided, it discloses nothing a name does not, and the name is already served — so absent
is served as **absent** rather than as the name. Displaying an identity like `clusters/hdbscan` is
a client's choice, not one the service makes on its behalf.

**`title` on everything nameable**: attributes, vocabularies, each vocabulary value (replacing
`label`, a word that means *access label* everywhere else), views, layers and levels. It is
presentation metadata on an object whose visibility is already decided (`per-point-attributes.md` §3.8's rule that properties
are not separately gated), and a title discloses nothing a name does not.

## 7. Required, defaulted, refused

SA §9's rule governs: *performance knobs default; disclosure controls do not*.

**Required for every attribute:** `name` and `type`. **Required for `type = "category"`:** a
`vocabulary` reference. **Required on every vocabulary:** `width`, `value_set` and `visibility`.
**Required on every layer:** `visibility`, `artifact_visibility` and `require_member_visibility`.
**Defaulted:** `render`, `index` and `multi`, each `false` — the cheapest home, made more expensive
only by an explicit word.

**A declaration claiming neither placement flag is legal**: blob-resident (records §5), no
hot-column slot, no entity-space structure, no operand on `/v1/meta`, its values in the record blob
for drill-down to return.

**Refused at parse**, each naming what is absent per decision 0013:

- `render` with `multi = true` — decision 0039's fence, checked first so a caller setting both
  hears the permanent refusal; and `multi = true` at all (⊘, `per-point-attributes.md` §3.7, records §6);
- `render` on `keyword`: the hot column is served, so a rendered keyword would put either the
  value's bytes in every row at 0.93 GiB per byte per row per 10⁹, or an ordinal that is an index
  internal and never crosses the trust boundary (**I10**). `index = true` puts the string in entity
  space and costs the hot column nothing;
- `render_in` (⊘, `per-point-attributes.md` §3.9); `record` as a column name; a column named for a filter combinator —
  `all_of`, `any_of`, `none_of` — and a column shadowing a fixed, reserved or already-declared name;
- `width`, `visibility`, `value_set` or a value set on a non-category, refused rather than ignored:
  an ignored disclosure control is one its author believes is set;
- code `0` in a value set, since it is the *absent* sentinel and `low = 0` would make every
  value-less row a member of `low`; a code in both `reserved` and the live set;
- **an attribute naming a vocabulary no `[[vocabulary]]` block declares**, refused at config parse
  before a data file is opened and never an implicitly minted open vocabulary — the fall-through
  §8 forbids, arriving through a typo. Two blocks of one name likewise;
- **`value_set = "closed"` with no value source**; and **attributes sharing a vocabulary declaring
  different widths** is not expressible, `width` having moved to the vocabulary (`per-point-attributes.md` §3.9);
- **two answers to one question on a layer**: `source` beside an inline `artifacts` list, a `fields`
  map beside one, `members` beside `excluding` — in the map, on an inline row, or as two columns of
  one file — and a `[layer.members]` source beside either. Each leaves two memberships or two
  rosters for one artifact, and every masked count and every criterion divides by one of them, so
  which won would be the reader's order rather than anything the caller wrote. Two rows for one key
  in an artifact source is the same refusal: one row is one artifact;
- **`membership = "attribute"` as a bare word**, which was the spelling before the field had a
  home: a predicate over a value column is not declared until the column is named, so the refusal
  gives the table — `{ attribute = "<field>" }` — rather than reporting an unknown value. **A
  `membership` naming a column no `[[attribute]]` block declares** is refused with it, at parse and
  before a data file is opened, on the rule an undeclared vocabulary reference already follows:
  a predicate with nothing to read publishes every artifact on the layer with an empty membership.
  **A column that is not indexed, or is not `u8`/`u16`/`u32`**, is refused with it: the predicate
  reads the entity-addressed value column `index = true` writes, and only a single-valued
  category-width column partitions — which is what makes one label per row the whole membership.
  ⊘ A `keyword` or `text` column is refused by the same rule, its ordinals being per index layer;
- **what a predicate layer may not declare** — `content` (supplied or computed), `depends_on`,
  `levels`, a hierarchy other than `flat`, `artifact_visibility.field`, and any `layout`. Its
  artifacts are derived from the rule and carry their key and nothing else, so each of these would
  register a layer that is reachable and serves nothing, which no client can tell from a layer whose
  artifacts were all withheld. ⊘ The computed one is scope rather than principle: a property is a
  function of `membership ∩ M_auth`, and reaching one artifact's membership on such a level costs a
  scan of the whole column;
- **`[layer.shape]` on a layer whose `membership` is not `spatial`** — the members come from the
  stored set or the predicate the membership names, so a shape beside them decides nothing — a
  `shape.depth`, which every kind being exact has nothing to hold ([`polygon-membership.md`](polygon-membership.md)
  §6.1), and a `shape.kind` outside `bbox`, `circle`, `ellipse` and `polygon`;
- **an artifact's shape and its layer's `shape` written apart** — a shape on a layer that declares
  no kind is a region nothing evaluates, and a shape of another kind is in a field the layer never
  declared. A **transposed** box (a maximum below its minimum), a non-finite coordinate or a
  non-positive radius or axis is refused rather than corrected: swapping it would publish a
  membership over a region nobody wrote. A kind's numbers with some present and some absent is the
  same refusal. A row with **no** shape is not refused: it is published with an empty shape, holds
  no rows, and is reported;
- **`space = "wgs84"`** on a view that declares no projection — naming
  [`projections.md`](projections.md) §10: a view with one space has no second space to convert
  from, and a shape placed by a function other than the one the points went through would select
  the wrong rows. On a projected view it is honoured;
- **an artifact declaring no attachment in a layer that declares `depends_on`**, and one attaching
  into a layer that layer did not name. A dependent is served only where what it depends on is
  served ([decision 0089](../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)),
  so an artifact with no dependency would be gated on nothing — refused here and at ingest alike,
  an ingest that admitted what the build refuses being the fail-open half of one rule;
- an access label spelled `inherited`, the one reserved word occupying a slot that otherwise takes
  a label (§5). `public` is **not** refused: it is a label (`per-point-attributes.md` §3.8), and `derived` and `none` sit
  in slots that admit no label;
- **a `layout` outside `rows`, `column` and `list`**, and a pin on an attribute membership.
  The first is the surface's ordinary rule — a pin the build ignored would leave an operator having
  declared a layout and got another — and the second names a form the layer cannot be stored in: a
  single-valued attribute's members *are* the column, so it has no second form for a pin to select
  between. A spatial layer's membership is resolved into a per-row source at every segment's
  publication, so it takes a pin as an enumerated layer does. This is a
  **performance** knob and it still refuses rather than ignoring, which is not a contradiction with
  SA §9's rule: what defaults is the *absence* of the key, and an absent pin is a complete statement
  — *no opinion, pick automatically*.

`index` on a **rendered** number or datetime is **admitted**, not refused: decision 0064's presence
bitmap beside the hot column is what lets the row route tell an absence from a stored zero, and
without it an item with no value would match every range containing zero
(`per-point-attributes.md` §3.9, `filter-index.md` §2).

`value_set = "open"` with `visibility = "public"` is **warned about, not refused** (`per-point-attributes.md` §3.8, owner
ruling 2026-08-07). Every retired key — `listing`, `values_key`, `values_of`, `gate`, `ungated`,
`artifacts_carry_own`, `corpus_derived`, `visible_when`, `on_member_deletion`, `derived`, `kind`
and `width` on an attribute — is refused by the parser's unknown-field rule rather than aliased:
decision 0048's shape, replaced rather than carried.

## 8. Sources, fields and the binding

**Every object that has data names its own source, and the name is a `[sources]` key**; column
names default to the canonical ones:

```toml
[sources]
hdbscan = "hdbscan.parquet"                              # the path, written once

[[layer]]
source = "hdbscan"                                       # the name, wherever it is read
fields = { members = "members", parent = "parent_id" }   # only where the source disagrees
```

**They are fields, not columns**: in a Parquet source they are columns, in an inline one they are
keys of a table, and Arrow's schema calls them fields either way. **The map says *where*, never
*whether*** — the object's own keys assert existence (`hierarchy` that there are parent edges,
`depends_on` that there are attachment edges, `membership` that there are members,
`content.supplied` that there is content) and `fields` only locates what is already declared. Three
refusals follow, each naming both halves, and they fall in the two places that can make them. The
**parser** refuses a name that is not one of that object's fields at all, and a field named in the
map that the object never declared — both answerable from the declaration alone. The **readers**
refuse a declared field whose name is absent from the source, naming the object, the field, the
column it looked for and the columns the file carries; that one needs the file open, and it is what
turns a silent empty column into a build failure. Silence is the whole reason it exists: an absent
geometry column puts every point at the origin, and an absent access column puts every point in no
principal's mask. A map with no `source` is refused too: it locates fields in a file the object
never names.

**`fields.view` is a discriminator, and it exists only where something declares one.** On a
`[[view_group]]` it is form B's own key — the column saying which view a row of points lands in —
and on a `[[layer]]` it is `scope = { group = … }` that asserts it, a layer with one artifact set
having no row that could say which view an artifact belongs to. Named on either without that
declaration, it is refused as a field the object never declared, which is this section's rule and
not a new one.

**`level` and `attached_level` are read under their own names.** A layer's `fields` map is closed
to the names §1's tables give it, and a level is an address rather than a value — it is what makes
`(layer, level, key)` an artifact's identity. A producer whose source spells one otherwise renames
the column.

**A layer's membership has two shapes, and it names whichever it uses.** A `members` field on the
artifact row carries the membership as a list — the natural shape, and the one that makes an
authored layer writable inline. A `[layer.members]` block instead names a **separate source**, one
row per `(artifact, entity)`, for a membership no single cell should hold: a condensed tree's root
holds the whole corpus, which cannot stream and which a producer must otherwise materialise whole.
Declaring both is refused. `excluding` is the third spelling of the first shape and not a fourth
thing — the entities a membership leaves out, complemented once at build
(`annotation-write-cycle.md` §6.1).

**Or the data sits inline**, as a vocabulary's values may — for what a person authors, a dozen
curated regions rather than a corpus.

**Every path sits in `[sources]`, relative to this document** (§3), so the ordinary build names no
files at all. An **absolute** path there is refused at parse: it is the one shape that cannot
travel with the document, and the refusal names where an absolute path does belong. A `source`
naming a key `[sources]` does not carry is refused too, listing the names that exist — the one
refusal this arrangement adds, and it exists because there is no correct output: a name is not a
path, so an unmatched name resolves to nothing at all rather than to some file.

`--file NAME=PATH` **overrides one source's path**, for the deployment that stages one elsewhere,
and `NAME` is the source's own name in `[sources]`:

```
tessera build --file points=/mnt/staged/papers.parquet
```

**The key is the source, and that is the whole of why it moved.** It used to be the *object* whose
source was being replaced — `corpus`, `view:s0`, `view:s0:point_visibility`, `vocabulary:severity`,
`layer:clusters/a`, `layer:clusters/a:members` — a path synthesised by the parser rather than a
word the caller wrote, and one per block. Staging a file that three blocks read meant three
overrides, and missing one left that block quietly reading the old file while the build reported
success. Keying by the source moves every reader of it at once, and there is no key to miss.

Three rules, all fail-closed and unchanged in substance: a `source` naming no `[sources]` key is a
refusal listing the names that exist; an override naming no `[sources]` key is a refusal too, or
the declaration's own path stays quietly in force under a command line asking for another corpus;
and an override **never creates** a source — it replaces a path `[sources]` already writes, so a
closed vocabulary cannot be opened, nor a view given geometry, from the command line alone. One
name overrides one path — two of one name is a refusal rather than a last-one-wins, the two paths
being two corpora.

**A build materialises one view**, named by `--view` or — where the declaration has exactly one —
by there being only one. A declaration with several and no `--view` is refused listing them, and a
`--view` no `[[view]]` block declares is refused: two views quantise the same corpus differently,
so choosing would produce a bundle that is well-formed and not the one asked for.

**The identity field is `entity_id`**, or whatever `[defaults].entity_id_field` says this
declaration spells it. The canonical name is not `entity`, which names the object rather than the
value, and not `id`, which collides with `tessera_id` and with an external id. It is entity-space
and shared: a point has one identity across every view it appears in, and it is what a member row
names — so a source that spells the column differently is joined by naming the column, never by
being a second entity space.

**The exploded label relation is `(entity_id, term_id)` under those names.** It takes no `fields`
map and `[defaults].entity_id_field` does not reach it: a default a block has no way to override
would be a constraint rather than a default, and this is the one reader with nowhere to write one.

**Coverage is reported at every build, per attribute source, and never refused.** Each source's
pass counts the entities that came away with a value and the rows that named an entity this build
did not load:

```
attribute 'sentiment': 49,812 of 50,000 entities have a value
        1,950,188 source row(s) named entities this build did not load
```

**Entities covered is the denominator, and rows dropped is not.** A legitimate superset and a
broken join both drop an overwhelming fraction of their rows — a sentiment table covering every
paper arXiv ever published, against a 50,000-paper build, drops 97% of itself and is exactly
right — so the number that separates the two is how much of *this* corpus came away with a value.
Both are printed; only the first is the measure.

**An unmatched row is ignored, which is what a join does**, and it is fail-closed in both
directions that matter: an absent attribute matches fewer points in a filter, and an absent access
label leaves a point visible to nobody. **Zero coverage says so emphatically and still builds** —
the ids may simply be another corpus's, and only the operator knows which; a refusal here would
block the legitimate superset as loudly as the broken join, and this is build input, recoverable,
disclosing nothing.

## 9. What is stolen, and from where

| Convention | Source |
|---|---|
| Per-field placement booleans, `index` among them by name | Elasticsearch mappings (`index`, `doc_values`, `store`) |
| *Mappings are immutable; you reindex* | Elasticsearch |
| Explicit codes in the declaration | ClickHouse `Enum8('low'=1,…)` |
| `reserved` for retired codes | Protobuf `reserved 3;` |
| Append-only value addition | Postgres `ALTER TYPE … ADD VALUE` |
| `public` as an access label, satisfied by everyone | Accumulo column visibility |
| The secret in the environment, its name in the file | twelve-factor, and this repo's own `*_credential_env` |

---


## Appendix R — review trail

**2026-08-30 — a view declares a projection, and its frame is then written in degrees.** One key is
added, `projection`, from the closed set [`projections.md`](projections.md) §5 enumerates and
defaulting to `none` — so a corpus with no geography declares nothing and behaves exactly as it did.
What the key changes is the meaning of the two beside it: the coordinate columns become `lon` and
`lat`, and the extent becomes a box in longitude and latitude that the build projects and snaps
outward to the enclosing aligned square. `extent` gains two entries in its table for that box and
the surface stays closed, every one of them asserted against this section.

Six refusals arrive with it, each naming what to write instead: a projection outside the set; a
coordinate outside ±180 or ±90, in the declaration or in a row; `x`/`y` as a projected view's
columns and `lon`/`lat` as an unprojected one's; the three unprojected extent spellings on a
projected view; a box crossing the antimeridian, which an aligned square cannot wrap; and `auto`
over a source selecting no rows, which was already refused and now says both spellings. The count
is high for one key because the two spellings do not overlap at all — a caller is in one world or
the other, and every refusal exists to say which one they are in.

The **clip** count is new beside the clamp count and is deliberately not folded into it: a clipped
point lands exactly on the frame's edge, which is where this section's clamp rule says a point is
*not* clamped, so one counter would have to report the wrong cause. What a build *prints* about the
projection, the snap and the clip is not yet written; the numbers are computed and carried.

**2026-08-26 — the two CORS origin lists are enumerated here.** §3 gains the pair. They are the
serving side's keys and this document had deferred all of those to SA §7, which was right for
tuning and wrong for these two: they are disclosure controls, and §1's whole argument is that the
leak register is exhaustive *because* the surface is enumerable. `serve.cors_origins` is new
([decision 0102](../decisions/0102-the-viewer-plane-gains-an-enumerated-cors-origin-list.md)) — the
production origin list, **viewer plane only**, silent at startup — and it sits beside
`dev_cors_origins`, which is a development affordance precisely because it also opens the session
plane and so puts the credential that mints tokens in front of a browser. Both refuse a wildcard at
parse; previously neither was checked, and a `*` in either would have panicked the process at
router construction rather than being refused by name.

**2026-08-20 — `[sources]` names the files, `[defaults]` replaces `[corpus]`, and an attribute may
read from its own.** Two shapes went, and both were arbitrary. A `source` was a path, so a file
three blocks read was three paths to keep in step and three `--file` keys to remember; it is now a
**name**, and `[sources]` is where the paths live — one entry per file, still relative to this
document, still no absolute path. Every `source` in the declaration names one of those keys, and a
name the table does not carry is refused listing the ones it does: the one refusal this adds, and
it exists because a name that resolves to nothing has no correct reading, where reading it as a
relative path would turn a typo into a missing file rather than a declaration that does not
resolve. `--file` follows the same key, so `--file points=/mnt/staged/papers.parquet` moves
everything reading that source at once — where the object-keyed form left the block you missed
quietly reading the old file.

`[corpus]` is deleted and `[defaults]` takes its place, with the constraint removed rather than
renamed: `source` and `entity_id_field` are now defaults any block may write over, so a column may
name its own file and its own identity column and a file carrying entity ids joins whatever it
calls them. Nothing is lost — what `[corpus]` guaranteed, that every attribute lands in one entity
space, is guaranteed by the entity id and never was by the file. The default `source` reaches a
`[[view]]` and an `[[attribute]]` and deliberately nothing else, because elsewhere an absent source
is itself a declaration (a vocabulary that mints, a layer declared empty, a membership that is not
stored, labels riding the points' own column); `entity_id_field` reaches every reader that can say
otherwise, and not the exploded label relation, which takes no `fields` map and so has nowhere to.

The attribute pass is now **grouped by source** — one merge sweep per `(source, identity column)`
against this build's ordinals, where before it was one pass over one corpus file. The sweep itself
is unchanged, its measured cost and the correction to an earlier overstatement of it intact. What
changed is what an unmatched row means: it was `input_changed`, on the reasoning that the file
could only be the points file having moved under the build, and it is now **counted and reported**,
because a source covering a superset of this build's entities is the ordinary case for a column
that lives elsewhere. Every build prints, per source, how many entities came away with a value and
how many rows named entities it did not load — **entities covered is the denominator**, since a
legitimate superset and a broken join both drop an overwhelming fraction and only the first number
separates them. Zero coverage says so emphatically and still builds: recoverable, disclosing
nothing, and the operator is the one who knows whose ids those were.

**2026-08-20 — `value_set` now governs both entry points, and its row loses its marker.** No key is
added and none changes meaning: `open` said *a key no artifact declares creates one*, and until now
it said it of a build alone. It now says it of an ingest batch too — a column named for the layer
carrying a key nothing holds creates the artifact, at the close of the commit window that allocates
the points, with a lineage's chain created and linked in the same batch
([`artifacts-from-points.md`](artifacts-from-points.md) §6.3). What the row gains is the cost stated
plainly: under `open` a mistyped key is a permanent object rather than a refusal, which is reported
at both entry points and deliberately not bounded.

**2026-08-20 — the wire takes a membership column, and the acquisition split is what decides its
name.** No key is added and no block changes: `/control/ingest` now accepts a column named for a
declared layer, which §2's list of what the write path already consumes gains a bullet for. The
name is the *declaration's* — the layer's `name` — because `[layer.members]`'s `fields` maps a
file's column onto the canonical meaning and a deployment writing through the service has no file;
that is the same split `[[attribute]]` has between its `name` and its `field`, and this document's
§2 rule ("`source`, `fields` and inline data are acquisition keys") is what settles it rather than a
new decision. Full argument in [`artifacts-from-points.md`](artifacts-from-points.md) §6.2.

**2026-08-19 — a member key column may be a list, and the hierarchy kind says what its positions
mean.** No key is added and no block changes: a hierarchical clusterer's one list per point is
already one row per `(artifact, entity)`, several times over, and `hierarchy.kind` was already the
declaration of what a layer's lineage is. What the surface gains is a paragraph saying which shapes
agree with which kind and which disagreement refuses. Full argument in
[`artifacts-from-points.md`](artifacts-from-points.md) §4.

**2026-08-19 — `value_set` reaches a layer, and a key column may be an integer.** A clusterer emits
one integer per point and no artifact table, which the surface could not express: membership had to
be a roster plus a member table, a key had to be UTF-8, and a null key — the ordinary output of
every clusterer, at a fifth to a quarter of the points — refused the build. The declaration gains
one key, `value_set`, which is the word this document already uses for *is an unknown key refused or
minted* on a vocabulary, asked of a layer's artifacts; the readers gained two types and one skip
rule, neither of which is a surface change. `[layer.members]` itself needed nothing at all — a point
table with a cluster column already is one row per `(artifact, entity)`, asserted by a build from a
points file and a build from a member table producing the same bundle byte for byte. The key sits on
`[[layer]]` rather than on `[layer.members]` because ingest has no member block and the same key must
govern the write path. Full argument in
[`artifacts-from-points.md`](artifacts-from-points.md).

**2026-08-19 — a vocabulary keeps one visibility key, and the contradiction with decision 0088 is
closed** ([decision 0090](../decisions/0090-a-vocabulary-has-one-visibility-axis.md)). 0088 declared
the word `derived` retired; this document kept it, and the two have read as disagreeing since. The
design was right. A vocabulary is not merging two axes into one word — it has no label axis at all,
and `public`/`derived` are *no membership requirement* and *any member*: one axis, two settings.
0088's objection was the collision with supplied content, which expired when content moved to
`all`/`inherited`.

The two-key form was written and reverted rather than merely argued about. Splitting the key
appears to buy label-gating a vocabulary, and does not: a vocabulary hangs off a column, a column
reaches a principal through six surfaces, and a gate at two of them is fail-open. What landed was
`visibility` with one legal value and the capability absent — worse than either alternative. The
decision records the six surfaces so a future attempt starts from a column-level gate.

**2026-08-19 — the frame report also says how much resolution the corpus actually got.** The clamp
count caught data *outside* the frame and nothing else, so the opposite failure was still silent:
data far too small for its frame clamps nothing, every stored position is correct, and nearly all
the resolution is gone — coordinates spanning 100…118 against a 0…65536 frame with **zero** clamps.
The bounding box already printed could not close it either, being derived from the extremes, so two
outliers make the box look healthy while the corpus shares a handful of cells. Every build now
counts **how many cells the points actually landed in** and prints it with the point count and the
average points per occupied cell, warning emphatically past ten per cell. A ratio rather than a
count, because ten points in ten cells is a well-framed tiny corpus; ten because uniformly spread
data averages about 1.1 per occupied cell even at 10⁹ and heavy clustering only reaches about 1.2,
so an order of magnitude past it is a statement about the frame. **A warning and never a refusal**
(owner's ruling): a clamped corpus is stored wrong, a sparse one is stored correctly but coarsely,
which a pilot corpus or a deliberately wide frame may well mean. Counted at each build's segment
write off the codes bound for `morton.u32`, which are already in `(morton, tessera_id)` order — one
comparison per point, nothing retained, and the same figure from the linear build and the streaming
pipeline, which the byte-equality oracle now asserts.

**2026-08-19 — the build says what the frame does to the data, and `tessera check` is the CI half.**
Three things, and the first is the one that matters. **The clamp report**: a coordinate is
quantised across its view's extent and quantisation *clamps*, and the notebook shipped a
grid-shaped extent over UMAP coordinates spanning about −17…18 — every point folded into a
nineteen-cell corner, the bundle well-formed, and the build silent. `extent = "auto"` only moved
that trap, since a caller who states a frame by hand still got silence. So every build now prints
the data's own bounds beside the extent it was given, how much of the grid that leaves the data
occupying, and how many points land on the boundary rather than where they were written — and
**refuses past half of them**, on the argument that a clamped point's position is the frame's
rather than its own, so a frame misplacing the majority of a corpus is not that corpus's frame. A
point exactly at the maximum is not counted: cells are half-open and it lands in the top cell by
construction. The cost is the pass `auto` already paid, now paid either way, which is what stops a
caller avoiding the report by writing their extent out. **`tessera check`** parses the declaration
and reads only Parquet schemas — every declared attribute against the field that must carry it,
every source present, every located field there — collecting every finding rather than stopping at
the first, and printing the disclosure decisions as a table. It resolves through the same code
`tessera build` does, so the two cannot see different files. Its `--payloads` closes §2's ⊘: the
control-plane bodies a declare-only deployment used to author a second time by hand are a
serialisation of the type the build already compiled to. And **`reports/disclosure.json`** lands
beside `containment.json`, written to be diffed between builds — stable order, no timestamp, no
path — so which disclosure decision moved is a question with a cheap answer for the first time.
`depends_on` is in it, decision 0089 having made a dependency edge a gate.

**2026-08-19 — a dependency edge carries deletion and visibility, so one refusal became a
property.** `depends_on` declared an edge and said nothing about what it meant once both ends
existed. It now means both things the member grain already means at the other grain
([decision 0089](../decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)): an
artifact is deleted when the artifact it attaches to is deleted, and served only where that
artifact is served — **per artifact, not per layer**, and neither configurable. Two consequences
reach this document. Every artifact of a layer declaring `depends_on` must declare an attachment
into a declared layer, refused at build and at ingest alike, because an artifact with no dependency
has nothing for the prerequisite to gate on. And the **`public`-under-a-gated-parent refusal is
deleted**: it was the one place the sugar was stricter than the two `[[layer]]` blocks it expands
to, and rule 2 makes it unnecessary rather than merely inconsistent — a viewer who cannot reach the
cluster cannot reach its labels, whatever the label layer's own gate says. With it gone,
`[layer.labels]` supplies mechanism only.

**2026-08-19 — a membership names its column, and the label sugar is real.** Two changes, and the
second retires the last ⊘ on a whole block. **`membership` is two words and a table**:
`{ attribute = "<field>" }` names the value column an attribute membership is a predicate over,
because a rule with nothing to evaluate is not a declaration — and a column no `[[attribute]]`
block declares is refused at parse, on the rule an undeclared vocabulary reference follows. The
bare word `"attribute"` is refused with the table to write instead — it was the spelling, and a caller who writes it is
looking for a column to name, not for a fourth kind of membership. **`[layer.labels]` builds**: it
expands to a `[[layer]]` block *before anything compiles*, so the sugar meets every refusal, every
allocator rule and every reader a hand-written layer meets, and the two spellings are asserted to
produce a **byte-identical bundle** — the same statement the three membership spellings carry, and
for the same reason. The expansion supplies the parent's views, a flat hierarchy, `depends_on` the
parent, the content wrapper around `type`, and `artifact_visibility = { default = "inherited" }`.
Two things fell out of building it. The block needs **`[layer.labels.members]`**, added to its key
table above: a ranked content's generating set is a `(artifact, rank, entity)` row and there is no
other shape that carries one, so without it the sugar could declare content the build would refuse
to publish — the worked example in §6 was in exactly that state and is corrected with it. And the
**"never wider" rule is only checkable at `public`**: two opaque access labels carry no ordering
the build could compute, so the general case is admitted and marked as unenforceable at the claim
rather than enforced by a check that could not do what it appeared to.

**2026-08-19 — one source per layer, one row per artifact, and membership by exclusion.** The
artifact grain was one row per `(artifact, rank)` in one file for every layer, which needed a
`layer` discriminator column, repeated each artifact's key, parent and attachment on every row of
it, and needed a cross-row agreement refusal to catch the copies disagreeing. **A layer now names
its own source**, so the discriminator is gone — there is no second layer's rows to tell apart —
and **an artifact is one row carrying its `contents` as a ranked list**, so the agreement refusal
is retired rather than replaced: the condition it detected is not expressible when a key appears
once. What one row per artifact does admit — the same key written twice — is refused as two
artifacts under one name. **A layer's `fields` map reaches its readers**, the ⊘ that held it back
being exactly the discriminator and the two columns this grain no longer has; `level` and
`attached_level` stay unmovable, this section's tables not naming them. **`artifacts = [{ … }]`
writes a layer out in the declaration** for what a person authors, with its own key table above and
its own row in §1's closure test. **`excluding` names the entities a membership leaves out**,
complemented once at build against the corpus and materialised: the three pairs of spellings —
inline against sourced, `excluding` against `members`, a row's membership against a
`[layer.members]` source — are each asserted to produce a **byte-identical bundle**, which is the
strongest statement of the property and the one that makes *no request-time complement* structural.
An excluded id this build did not assign refuses the build, an exclusion resolving to nothing being
a silent widening where an unknown member is a silent narrowing.

**2026-08-19 — the artifact and member readers take `rank` and `entity`.** §1's field tables were
already written on these names; the readers now use them, so §8's unbuilt note narrows: what still
keeps a layer's `fields` map refused is the `layer` discriminator column, the per-row `values` and
`parent_key`, not the membership grain's own columns. The caller's key is `key` throughout — the
qualifier in `stable_key` said nothing the type did not. Names only; no refusal, no default and no
disclosure control moved.

**2026-08-18 — the plugin takes a term list, and the comma stops being a delimiter.** The build no
longer joins an item's terms into one `access` string for the plugin to split apart: it hands the
plugin the list it already has, through a second data-side entry point (`terms_of_labels`) that
`builtin:passthrough` implements as the identity — one descriptor per term, verbatim, in order,
with an empty element refused because an empty descriptor is not a grant. So **an access term or a
declared label may contain a comma**, and the two refusals that held that line — one in the access
column decode, one at the declaration — are deleted along with the paragraph above that recorded
them; a term is whatever the caller wrote, whole. The wire path is unchanged: an ingest request
carries one opaque `access` byte string and still goes through `terms_of_label`, which still
splits on commas, because only the plugin can decompose it. The plugin identity moves to
`builtin:passthrough:2`, which moves both hashes — the auth side is untouched, but sharing one
identity string means fragment caches recompute and tokens re-mint, which is the conservative
direction and is why the hash is in `MANIFEST.json` at all.

**2026-08-18 — the readers take the names, and a point's terms come from a field.** Two changes,
and only the second moves a request. **Every object but a layer now reads its source under the
names its `fields` map resolved** — a view's geometry, `[corpus]`'s identity, a vocabulary's
`key`/`code`/`title`, an attribute's `field` — so §8's third refusal exists at last: a declared
field the file does not carry is a build failure naming the object, the field, the column looked
for and the columns the file has, where before it would have read an empty column and said nothing.
A layer's map stays refused, its readers still spelling `layer`, `values` and `parent_key`.
**`point_visibility = { field }` reads each point's access terms from a `list<string>` (or a plain
`string`) column of the view's own source**, and `{ default }` alone gives every point one label —
so all three shapes §1 declares now acquire. Terms are trimmed; a null value and an empty list both
mean *no access terms*, which is *visible to no principal* rather than unrestricted, and are what a
`default` fills; filling never overrides. **`public` is interned at term `0` by every build and
added to every principal's resolved term set inside the engine** — not by grant and not in the
plugin. ⊘ The `source` route fills nothing, where the `field`
route does; that divergence is recorded above and is the narrow half.

**2026-08-18 — the invocation is `tessera build`.** The previous revision made acquisition real and
produced a nine-flag command line beside a detailed config, which is the problem it was meant to
solve, moved. Three changes close it. **A `source` is a path relative to the document that declares
it**: the rule §4 was protecting is narrower than it was written — what must not appear is an
*absolute or machine-specific* path, and a relative one travels in git with the declaration around
it. An absolute `source` is refused, naming the override; `--file` survives keyed by the **object**
whose source it replaces, since the source string is a path now rather than a name. **`extent` moves
into `[[view]]`** in §1's four spellings, `--extent` is deleted, and `auto` reads the points source
to fit a squared box with a 1% margin — a whole pass over two columns, not the file's statistics,
which are per row group and would make every stored cell depend on the producer's layout. **A
`tessera.toml` names the deployment** and both verbs walk up to find it, so the build's output path
and the server's `bundle_path` are one value declared once; its absence is a refusal naming what to
create. The identity key comes from the environment variable that file names, from a `.env` beside
it, or from `--identity-file`: `--id-key` is deleted, its own help having already said why. One
thing changed that this did not set out to: **a serving credential is now read at startup rather
than at parse**, because `tessera build` reads the same file and had begun refusing to write a
bundle until two serving secrets were exported. `--view` also became optional where a declaration
has exactly one view; several with none named is refused listing them.

**2026-08-18 — acquisition is real.** Every object that has data names a logical key and
`--file KEY=PATH` binds it; `--points`, `--pairs`, `--values`, `--artifacts` and
`--artifact-members` are deleted, and `--config` is now required because the config is what says
where the corpus is. §8's three fail-closed rules are one mechanism — declared-and-unbound,
bound-and-undeclared, and never a fall-through — with a fourth that fell out of building it: one
key binds one path, since two bindings of one key are two corpora. `point_visibility` gained a
`source` beside its `field`, which is where the exploded `(entity_id, term_id)` relation now
arrives; `[corpus].source` and a view's are separate keys, usually bound to one file, and that is
what lets a build read attributes from entity space and geometry from the view. Two corrections
this made necessary: §8 claimed two field refusals where there are three (a name that is not one of
the object's fields at all was missing, and the *absent from the source* one is the readers'
rather than the parser's), and it named §10, which does not exist. Field **renames** are refused
until the readers take names — accepted-and-disregarded is the one shape this surface exists to
prevent — so the map's validation is built and its effect is not.

**2026-08-18 — the declaration half is built.** One parser reads one document
(`tessera-build`'s `config` module); `schema.toml`, `layers.toml`, `--schema` and `--layers` are
deleted. §1's table is a **test**: the parser's own accepted key set is read out of serde's
unknown-field message and compared against the table block by block, so the assertion fails both
when a key here disappears and when one this document does not name appears. Every refusal §7
states exists and has a case, plus the three new ones — an attribute naming an undeclared
vocabulary (at parse, before a data file opens), two vocabulary blocks of one name, and
`value_set = "closed"` with no value source. Two corrections fell out of building it: §6 had
`reserved` inside `[vocabulary.values]`, where a bare key array cannot carry it and where §1 does
not put it, and §7 listed `index` on a rendered number as refused, which decision 0064's presence
bitmap admitted and every other document already says. The acquisition half is unbuilt and refused
rather than ignored — see the ⊘ note at the head.

**2026-08-18 — the two halves are separated.** §2 is added: the surface splits into a declaration
that is route-independent and an acquisition half that only a build reads, and a deployment writing
through the service omits the second entirely. Nothing moved to say it — the declaration was
already what `/control/ingest` and `PUT /control/layers` consume, down to attributes riding in
declared order and categories arriving as value keys — but the document read as though a build were
the only route, and an `R` against `source` said *required* where it meant *required to build from a
file*. ⊘ The two routes remain one implementation and two authored formats; nothing yet emits
control-plane payloads from this config.

**2026-08-18 — extracted, and enumerated.** This surface lived as `per-point-attributes.md` §5,
which was where it began: a schema for per-point attributes. It outgrew that document — of the six
blocks it declares, only `[[attribute]]` and `[[vocabulary]]` are that document's subject — so it
moves here whole, on the rebuild that
[decision 0088](../decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md) ruled.
§1 is new: the whole surface in one table, which is worth having because the set is **closed** —
`deny_unknown_fields` on every block, an enumerated set behind every value that is a word rather
than a caller's string. Closure is what the leak register rests on, the register being exhaustive
because the surface is enumerable, so a key added without an entry in §1 is a disclosure control
nobody has reasoned about.
