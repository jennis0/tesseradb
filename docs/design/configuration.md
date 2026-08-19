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
refusal §7 states, and reads each object's source under the names its `fields` map resolved. Every
`source` is a path relative to the declaring document; `--file KEY=PATH` overrides one, keyed by
the object. `--extent`, `--id-key`, `--id-key-file`, `--points`, `--pairs`, `--values`,
`--artifacts`, `--artifact-members`, `--schema`, `--layers`, `schema.toml` as a fixed name and
`layers.toml` are all gone. What is **not** built, and is refused rather than accepted and ignored,
each naming what is absent per
[decision 0013](../decisions/0013-mark-specified-vs-implemented.md):

- **A view's own `visibility`**, and `withdraw_on_member_deletion = true` on a **layer** (not on
  its content, which needs a fold path) — each refused at parse rather than accepted and ignored.

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

Ten blocks. `R` = required, `D` = defaulted, `O` = optional with no default and no fallback.
**`source`, `fields` and inline data are acquisition keys** — a build reads them and a deployment
writing through the service omits them entirely (§2), so an `R` on one of those means *required to
build from a file*, never *required to declare*.

**`[corpus]`** — entity space: identity and attributes, shared by every view.

| Key | | Value |
|---|---|---|
| `source` | R | a path, relative to this document (§3) |
| `fields` | D | override map; canonical name is `entity_id` |

**`[[view]]`** — one named coordinate system. Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | identity; tombstoned on drop, never reused |
| `title` | O | human-readable, served on `/v1/meta` |
| `source` | R | a path, relative to this document (§3) |
| `fields` | D | canonical `entity_id`, `x`, `y`, or `morton` + `residual` — the geometry shapes are mutually exclusive (§8) |
| `extent` | R | the quantisation frame: `"auto"`, `{ auto = true, margin = f }`, `{ min, max }` or `{ x = [a,b], y = [c,d] }`. See below |
| `point_visibility` | R | `{ field, default }`, or `{ source, default }` — where each point's label is, and what a point carrying none gets. See below |
| `visibility` | ⊘ | the view's own gate; specified, not implemented (views §3) |

**`[[vocabulary]]`** — a named value set. Repeatable.

| Key | | Value |
|---|---|---|
| `name` | R | identity; attributes share a vocabulary by naming it |
| `title` | O | human-readable |
| `width` | R | `u8` \| `u16` \| `u32` — the **code space's** width (`per-point-attributes.md` §3.6, `per-point-attributes.md` §3.9) |
| `value_set` | R | `closed` \| `open` — is an unknown key at ingest refused, or minted? |
| `visibility` | R | `public` \| `derived` — no label; only a layer's gate takes one |
| `source` | R for `closed`, unless inline | a path, relative to this document (§3) |
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
| `multi` | ⊘ | refused at parse (`per-point-attributes.md` §3.7, records §6) |
| `render_in` | ⊘ | refused at parse (`per-point-attributes.md` §3.9) |

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
| `views` | R | the views this layer's artifacts are drawn on |
| `source` | R unless inline | one file per layer, so no discriminator field exists. Declaring it beside `artifacts` is refused |
| `fields` | D | canonical `key`, `contents`, `parent`, `attached_layer`, `attached_key`, and `members` or `excluding` where membership rides the artifact row. Naming both memberships is refused, as is a map beside inline `artifacts` |
| `artifacts` | O | inline array, instead of `source`, for an authored layer — the keys below |
| `membership` | R | `enumerated` \| `spatial` \| `{ attribute = <field> }` |
| `hierarchy` | R | `{ kind = flat \| nested \| stacked \| tiered, prune_children = bool }` — see below |
| `visibility` | R | an access label, or `public` |
| `artifact_visibility` | R | `{ field, default }`; `default` may be `inherited` |
| `require_member_visibility` | R | `all` \| `any` \| `{ fraction = p }` \| `{ count = n }` \| `none` |
| `withdraw_on_member_deletion` | D `false` | drop the **whole artifact** when one of its members is deleted, rather than letting its membership shrink. ⊘ `true` is refused at parse: the fold has no artifact-withdrawal path (`annotation-write-cycle.md` §6.1) |
| `depends_on` | O | layers this one's edges point into; must be declared before it |

**`artifacts = [{ … }]`** — one authored artifact, on the canonical field names. An inline row *is*
the canonical spelling, so there is no `fields` map beside it and no file for one to locate.

| Key | | Value |
|---|---|---|
| `key` | R | the caller's own name for it, which is what an edge into it names |
| `level` | D `0` | the resolution it sits at |
| `members` | O | the membership, by inclusion |
| `excluding` | O | the membership, by exclusion. Declaring both is refused |
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
| `source` | R | a path, relative to this document; one row per `(artifact, entity)` |
| `fields` | D | canonical `key`, `entity`, `rank` — a null `rank` is the artifact's own membership, `k` the generating set of `contents[k]` |

A member source without the layer's own artifacts — its `source` or its inline `artifacts` — is
refused: they are the roster a member row's key resolves against, and without one a mistyped key
would publish a phantom artifact rather than fail to find one.

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
| `name`, `title`, `source`, `fields`, `[layer.labels.members]` | as `[[layer]]` | |
| `type` | R | the content type — `text`, `polygon`, `extent`, `point` |
| `membership` | R | written out: a label's members are its generating set, which the build cannot derive |
| `require_member_visibility` | R | the **layer** grain: how much of a label's membership a viewer must see for the label to appear |
| `artifact_visibility` | R | as `[[layer]]`; declared, never supplied |
| `[layer.labels.content]` | R | one key, `require_member_visibility` — **where the text came from**: `all` if it was generated from the documents it names, `inherited` if it is true whether or not any of them exists |
| `visibility` | D | the parent layer's; narrower is admitted, and only one widening is checkable — see below |

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
because the value it defaults to is the parent layer's own and never the widest one. What the
default *cannot* be is checked: the design's rule is *narrower, never wider*, and **only one case
of that is computable**. An access label is an opaque interned term, so whether every principal
holding one also holds another is a fact about grants, which do not exist in the declaration.
`public` is the exception — every principal holds it by construction
(`per-point-attributes.md` §3.8) — so a label layer declaring `public` under a parent gated on
anything else is refused.

⊘ **Two distinct non-`public` labels are admitted and not ordered.** The parent's gate and the
child's are then two independent gates, and a principal holding the child's and not the parent's
reaches the labels without reaching the layer they describe. That is the same declaration a caller
makes by writing two `[[layer]]` blocks — which this expansion is defined to be identical to — so
a refusal here would be a lint on one spelling of something the other spelling still admits, not a
control. Closing it needs an ordering over access labels, which is a question about grants and not
about configuration.

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
- **Views.** A view is created online by a control verb carrying `{name, gate, projection
  provenance}`, or declared here and compiled. Same object either way.

**What differs is only what a missing source means.** At build, a declared source that resolves to
no path is a refusal naming the object (§8). With no source declared at all, the object is declared
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

⊘ **The two routes are one implementation and are not yet one file.** The build reads TOML and the
control plane takes JSON; nothing today reads this config and emits control-plane payloads, so a
write-path deployment authors the declaration twice — once to build the empty bundle, once per
online creation. Making `tessera check` emit the payloads it has already parsed is the obvious
close, and is not proposed here.

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
tessera serve      # same file, opens what that build wrote
```

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

**Every path in it resolves against its own directory**, on the same rule a `source` follows — a
relative `bundle.path` that moved with the shell's working directory would make `cd crates &&
tessera serve` open a different bundle from the one `tessera build` had just written. Both
`[build]` and `[identity]` default entire, so the ordinary file writes neither.

**Sources are paths relative to the declaring file.** An earlier revision forbade every path here
and bound each source on the command line, which produced an invocation naming five files and a
config that could not be read without it. The rule it was protecting is narrower than it was
written: what must not appear is an **absolute or machine-specific** path, and a path relative to
the config is neither — it travels in git with the file that describes it and is exactly as
reproducible as the declaration around it. `--file KEY=PATH` survives as an **override**, for the
deployment that stages one source elsewhere.

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
`--config` overrides that. Corollary: **no absolute or machine-specific paths in it** — a `source`
is written relative to the document, an absolute one is refused at parse, and `--file KEY=PATH` is
where a staged path goes (§3, §8).

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
  a predicate with nothing to read publishes every artifact on the layer with an empty membership;
- **`[layer.labels].visibility = "public"` under a parent gated on anything else**, the one
  widening an opaque label ordering is not needed to decide;
- an access label spelled `inherited`, the one reserved word occupying a slot that otherwise takes
  a label (§5). `public` is **not** refused: it is a label (`per-point-attributes.md` §3.8), and `derived` and `none` sit
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

## 8. Sources, fields and the binding

**Every object that has data declares its own source**, and column names default to the canonical
ones:

```toml
source = "hdbscan"                                       # a key bound on the command line
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

**A source is a path relative to this document** (§3), so the ordinary build names no files at
all. An **absolute** path is refused at parse: it is the one shape that cannot travel with the
document, and the refusal names where an absolute path does belong.

`--file KEY=PATH` **overrides** one source, for the deployment that stages one elsewhere, and
`KEY` is the **object** whose source it replaces — the source string being a path now rather than
a name:

| Declared at | Override key |
|---|---|
| `[corpus].source` | `corpus` |
| `[[view]].source` | `view:<name>` |
| `[[view]].point_visibility.source` | `view:<name>:point_visibility` |
| `[[vocabulary]].source` | `vocabulary:<name>` |
| `[[layer]].source` | `layer:<name>` |
| `[layer.members].source` | `layer:<name>:members` |

Three rules, all fail-closed: a source with no path from anywhere is a refusal naming the object;
an override no object declares is an error listing the keys that exist, or the declaration's own
path stays quietly in force under a command line asking for another corpus; and an override
**never creates** a source, so a closed vocabulary cannot be opened, nor a view given geometry,
from the command line alone. One key overrides one path — two of one key is a refusal rather than
a last-one-wins, the two paths being two corpora.

**A build materialises one view**, named by `--view` or — where the declaration has exactly one —
by there being only one. A declaration with several and no `--view` is refused listing them, and a
`--view` no `[[view]]` block declares is refused: two views quantise the same corpus differently,
so choosing would produce a bundle that is well-formed and not the one asked for.

**The identity field is `entity_id`**, declared once on `[corpus]` and defaulting to that name.
Not `entity`, which names the object rather than the value, and not `id`, which collides with
`tessera_id` and with an external id. It is entity-space and shared: a point has one identity
across every view it appears in, and it is what a member row names.

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
