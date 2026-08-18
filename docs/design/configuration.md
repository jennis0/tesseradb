# The configuration surface — design

**Date:** 2026-08-18
**Status:** **Normative for the build-time configuration surface.** One file declares the corpus,
its views, its vocabularies, its attributes and its layers; `tessera build --config` reads it and
compiles it into `MANIFEST.json`. Ruled by
[decision 0088](../decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md); the
design record is
[`../evidence/memos/2026-08-18-configuration-surface.md`](../evidence/memos/2026-08-18-configuration-surface.md)
and the staged plan is beside it.

**What this owns, and what it does not.** This document owns the *declaration* — which blocks exist,
which keys they take, what is required, and what is refused.
[`per-point-attributes.md`](per-point-attributes.md) owns what a category and its vocabulary *are*,
including the visibility semantics its §4 states; [`records-and-search.md`](records-and-search.md)
§3–§4 owns the three homes a field can occupy; and
[`annotation-write-cycle.md`](annotation-write-cycle.md) §7.1 owns what a build does with the
artifact and member grains once it has read them. Where this and those differ on *semantics*, they
govern; where they differ on *spelling*, this does.

⊘ **The declaration is built; acquisition is not.** `tessera build --config` reads one document and
compiles the blocks below, with every refusal §6 states; `schema.toml`, `layers.toml`, `--schema`
and `--layers` are gone. What is **not** built, and is refused at parse rather than accepted and
ignored: `source`, `fields`, a layer's inline `artifacts`, an attribute's `field` and the `--file`
binding that would give them meaning (§7) — a build still acquires through `--points`, `--pairs`,
`--values NAME=PATH`, `--artifacts` and `--artifact-members`; the `[layer.members]` and
`[layer.labels]` blocks, which are acquisition and expansion rather than declaration; a view's own
`visibility`; and `withdraw_on_member_deletion = true` on a **layer** (not on its content), which
needs a fold path. Each is a refusal naming what is absent, per
[decision 0013](../decisions/0013-mark-specified-vs-implemented.md). ⊘ A `title` on a view, an
attribute or a vocabulary is compiled and **not yet published** — the manifest carries no slot for
one, and adding three is a contracts change; a level's title and a layer's are served today, as is
each vocabulary *value*'s.

## 1. The surface in full

**The configuration surface is a closed set**, and that is a property rather than an accident. Every
block parses under `deny_unknown_fields`, and every value that is a word rather than a caller's
string is drawn from an enumerated set — so a key this table does not name does not exist, and a
value it does not list is refused. That closure is what the leak register rests on: the register is
exhaustive *because* the surface is enumerable, and a key added without an entry here is a control
nobody has reasoned about.

Ten blocks. `R` = required, `D` = defaulted, `O` = optional with no default and no fallback.
**`source`, `fields` and inline data are acquisition keys** — a build reads them and a deployment
writing through the service omits them entirely (§2), so an `R` on one of those means *required to
build from a file*, never *required to declare*.

**`[corpus]`** — entity space: identity and attributes, shared by every view.

| Key | | Value |
|---|---|---|
| `source` | R | a logical key bound by `--file` |
| `fields` | D | override map; canonical name is `entity_id` |

**`[[view]]`** — one named coordinate system. Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | identity; tombstoned on drop, never reused |
| `title` | O | human-readable, served on `/v1/meta` |
| `source` | R | a logical key bound by `--file` |
| `fields` | D | canonical `entity_id`, `x`, `y`, or `morton` + `residual` — the geometry shapes are mutually exclusive (§7) |
| `point_visibility` | R | `{ field, default }` — where each point's label is, and what a point carrying none gets |
| `visibility` | ⊘ | the view's own gate; specified, not implemented (views §3) |

**`[[vocabulary]]`** — a named value set. Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | identity; attributes share a vocabulary by naming it |
| `title` | O | human-readable |
| `width` | R | `u8` \| `u16` \| `u32` — the **code space's** width (`per-point-attributes.md` §3.6, `per-point-attributes.md` §3.9) |
| `value_set` | R | `closed` \| `open` — is an unknown key at ingest refused, or minted? |
| `visibility` | R | `public` \| `derived` — no label; only a layer's gate takes one |
| `source` | R for `closed`, unless inline | a logical key bound by `--file` |
| `fields` | D | canonical `key`, `code`, `title`; `code` may be absent — see below |
| `values` | R for `closed`, unless sourced | inline: an array of keys, or a `key = code` table |
| `reserved` | O | retired codes, never reassigned |

**`[[attribute]]`** — one per-point column, read from `[corpus]`'s source. Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | the served name, and the manifest's |
| `title` | O | human-readable |
| `field` | D | the source field, when it differs from `name` |
| `type` | R | `bool`, `u8`…`u64`, `i8`…`i64`, `f32`, `f64`, `timestamp_us`, `text`, `keyword`, `category` |
| `vocabulary` | R for `category` | names a `[[vocabulary]]`; refused if undeclared |
| `render` | D `false` | a fixed-width slot in every row of `columns.arrow` |
| `index` | D `false` | the entity-space search structure |
| `analyser` | D | `text` only; `unicode` is the default and, today, the only one — see below |
| `multi` | ⊘ | refused at parse (`per-point-attributes.md` §3.7, records §5) |
| `render_in` | ⊘ | refused at parse (`per-point-attributes.md` §3.9) |

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
| `views` | R | the views this layer's artifacts are drawn on |
| `source` | R unless inline | one file per layer, so no discriminator field exists |
| `fields` | D | canonical `key`, `contents`, `parent`, `attached_layer`, `attached_key`, and `members` or `excluding` where membership rides the artifact row |
| `artifacts` | O | inline array, instead of `source`, for an authored layer |
| `membership` | R | `enumerated` \| `spatial` \| `{ attribute = <field> }` |
| `hierarchy` | R | `{ kind = flat \| nested \| stacked \| tiered, prune_children = bool }` — see below |
| `visibility` | R | an access label, or `public` |
| `artifact_visibility` | R | `{ field, default }`; `default` may be `inherited` |
| `require_member_visibility` | R | `all` \| `any` \| `{ fraction = p }` \| `{ count = n }` \| `none` |
| `withdraw_on_member_deletion` | D `false` | drop the **whole artifact** when one of its members is deleted, rather than letting its membership shrink. ⊘ `true` is refused at parse: the fold has no artifact-withdrawal path (`annotation-write-cycle.md` §6.1) |
| `depends_on` | O | layers this one's edges point into; must be declared before it |

and four sub-blocks, each below: ⊘ `[layer.members]` (O), `[layer.content]` (O),
`[[layer.levels]]` (R for `stacked` and `tiered`, refused for `nested`) and ⊘ `[layer.labels]` (O).
`[[layer.content.supplied]]` sits under `[layer.content]`, not under the layer.

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
| `source` | R | a logical key; one row per `(artifact, entity)` |
| `fields` | D | canonical `key`, `entity`, `rank` — a null `rank` is the artifact's own membership, `k` the generating set of `contents[k]` |

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
| `zoom` | O | `[min, max]`, **advisory** — it bounds no work; a tiered layer's response is bounded by the level asked for and a treed layer's by the request's artifact budget |

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
| `name`, `title`, `source`, `fields` | as `[[layer]]` | |
| `type` | R | the content type — `text`, `polygon`, `extent`, `point` |
| `membership` | R | written out: a label's members are its generating set, which the build cannot derive |
| `require_member_visibility` | R | typically `all` — containment |
| `visibility` | D | the parent layer's; overridable narrower, never wider |

**Three words are reserved**, and only one occupies a slot that otherwise takes a caller's label:

| Word | Where | Collides with a label? |
|---|---|---|
| `public` | anywhere a label appears | **No** — it *is* a label, reserved at term `0` (`per-point-attributes.md` §3.8) |
| `derived` | a vocabulary's `visibility` | **No** — that slot takes no label |
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
does the schema live*: `tessera build --config schema.toml` with no sources compiles the
declaration into a bundle with no rows in it, and the service writes into that. The schema never
appears in the server's own config (§3), so an empty bundle is what carries it — and a config with
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
- **Views.** A view is created online by a control verb carrying `{name, gate, projection
  provenance}`, or declared here and compiled. Same object either way.

**What differs is only what a missing source means.** At build, a declared source that is not bound
by `--file` is a refusal naming both (§7). With no source declared at all, the object is declared
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
| Views | ⊘ **Specified, not implemented.** Creation is a control verb carrying `{name, gate, projection provenance}`, WAL'd and materialised at the next flush (views §5); a bundle has one coordinate system, so nothing evaluates it yet |
| Attributes | **Deliberately not online.** Adding `index` is a build pass with no row rewrite; adding `render` rewrites every segment; changing a width or a type is refused outright. The convention is Elasticsearch's, and stolen on purpose: *mappings are immutable; you reindex* |

⊘ **Declaring a wholly new attribute after a build is not specified**, as distinct from altering an
existing one, and it is not simply the append it looks like: the scalar tail is stored and read back
**positionally**, so segments built before and after the addition disagree about the tail's length.
That is a records-and-search question rather than a configuration one; it is named here because a
caller reading §2's *declare now, write later* will reasonably expect it and there is no answer to
give them.

⊘ **The two routes are one implementation and are not yet one file.** The build reads TOML and the
control plane takes JSON; nothing today reads this config and emits control-plane payloads, so a
write-path deployment authors the declaration twice — once to build the empty bundle, once per
online creation. Making `tessera check` emit the payloads it has already parsed is the obvious
close, and is not proposed here.

## 3. A build input, not server config

The build config never appears in the *server's* config; the server reads the compiled schema from
the bundle's `MANIFEST.json`. A server reading a schema of its own could be restarted against a
bundle whose columns disagree, and the mismatch would surface as wrong codes rather than a startup
error. Config compiles to manifest, source to binary. **The capability model does not reach the
manifest**, which stays flat and per-placement — hot columns, filter operands, categories — because
a reader should never need to understand intent to know what to load.

**One file, named on the command line.** Attributes, vocabularies, views and layers are declared
together; the examples here call it `schema.toml`, and nothing depends on the name. Corollary:
**no environment-specific paths in it** — a source names a logical key and `--file KEY=PATH` binds
it (§7).

## 4. Two axes, and only two

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

## 5. The declaration

```toml
[corpus]
source = "corpus"                    # entity space: identity and attributes

[[view]]
name             = "s0"
title            = "arXiv, August 2026"
source           = "geometry"
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
  require_member_visibility = "all"
```

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

## 6. Required, defaulted, refused

SA §8's rule governs: *performance knobs default; disclosure controls do not*.

**Required for every attribute:** `name` and `type`. **Required for `type = "category"`:** a
`vocabulary` reference. **Required on every vocabulary:** `width`, `value_set` and `visibility`.
**Required on every layer:** `visibility`, `artifact_visibility` and `require_member_visibility`.
**Defaulted:** `render`, `index` and `multi`, each `false` — the cheapest home, made more expensive
only by an explicit word.

**A declaration claiming neither placement flag is legal**: blob-resident (records §4), no
hot-column slot, no entity-space structure, no operand on `/v1/meta`, its values in the record blob
for drill-down to return.

**Refused at parse**, each naming what is absent per decision 0013:

- `render` with `multi = true` — decision 0039's fence, checked first so a caller setting both
  hears the permanent refusal; and `multi = true` at all (⊘, `per-point-attributes.md` §3.7, records §5);
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
  §7 forbids, arriving through a typo. Two blocks of one name likewise;
- **`value_set = "closed"` with no value source**; and **attributes sharing a vocabulary declaring
  different widths** is not expressible, `width` having moved to the vocabulary (`per-point-attributes.md` §3.9);
- an access label spelled `inherited`, the one reserved word occupying a slot that otherwise takes
  a label (§4). `public` is **not** refused: it is a label (`per-point-attributes.md` §3.8), and `derived` and `none` sit
  in slots that admit no label.

`index` on a **rendered** number or datetime is **admitted**, not refused: decision 0064's presence
bitmap beside the hot column is what lets the row route tell an absence from a stored zero, and
without it an item with no value would match every range containing zero
(`per-point-attributes.md` §3.9, `filter-index.md` §2).

`value_set = "open"` with `visibility = "public"` is **warned about, not refused** (`per-point-attributes.md` §3.8, owner
ruling 2026-08-07). Every retired key — `listing`, `values_key`, `values_of`, `gate`, `ungated`,
`artifacts_carry_own`, `corpus_derived`, `visible_when`, `on_member_deletion`, `derived`, `kind`
and `width` on an attribute — is refused by the parser's unknown-field rule rather than aliased:
decision 0048's shape, replaced rather than carried.

## 7. Sources, fields and the binding

**Every object that has data declares its own source**, and column names default to the canonical
ones:

```toml
source = "hdbscan"                                       # a key bound on the command line
fields = { members = "members", parent = "parent_id" }   # only where the source disagrees
```

**They are fields, not columns**: in a Parquet source they are columns, in an inline one they are
keys of a table, and Arrow's schema calls them fields either way. **The map says *where*, never
*whether*** — the object's own keys assert existence (`hierarchy` that there are parent edges,
`membership` that there are members, `type` that there is content) and `fields` only locates what
is already declared. Two refusals follow: a field named in the map that the object never declared,
and a declared field whose default name is absent from the source, each naming both halves.

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

`--file KEY=PATH` binds every source, replacing `--points`, `--pairs`, `--artifacts`,
`--artifact-members` and `--values`. Three rules, all fail-closed: a key declared but not bound is
a build failure naming both; a binding no object declares is an error, or the previous rule merely
relocates the typo; and an unbound source is **never a silent fall-through to minting**, which
would open a closed vocabulary with nobody deciding to.

**The identity field is `entity_id`**, declared once on `[corpus]` and defaulting to that name.
Not `entity`, which names the object rather than the value, and not `id`, which collides with
`tessera_id` and with an external id. It is entity-space and shared: a point has one identity
across every view it appears in, and it is what a member row names.

## 8. What is stolen, and from where

| Convention | Source |
|---|---|
| Per-field placement booleans, `index` among them by name | Elasticsearch mappings (`index`, `doc_values`, `store`) |
| *Mappings are immutable; you reindex* | Elasticsearch |
| Explicit codes in the declaration | ClickHouse `Enum8('low'=1,…)` |
| `reserved` for retired codes | Protobuf `reserved 3;` |
| Append-only value addition | Postgres `ALTER TYPE … ADD VALUE` |
| `public` as an access label, satisfied by everyone | Accumulo column visibility |
| Logical key bound at invocation | this repo's own `--id-key-file` |

---


## Appendix R — review trail

**2026-08-18 — the declaration half is built.** One parser reads one document
(`tessera-build`'s `config` module); `schema.toml`, `layers.toml`, `--schema` and `--layers` are
deleted. §1's table is a **test**: the parser's own accepted key set is read out of serde's
unknown-field message and compared against the table block by block, so the assertion fails both
when a key here disappears and when one this document does not name appears. Every refusal §6
states exists and has a case, plus the three new ones — an attribute naming an undeclared
vocabulary (at parse, before a data file opens), two vocabulary blocks of one name, and
`value_set = "closed"` with no value source. Two corrections fell out of building it: §5 had
`reserved` inside `[vocabulary.values]`, where a bare key array cannot carry it and where §1 does
not put it, and §6 listed `index` on a rendered number as refused, which decision 0064's presence
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
