# The configuration surface: two axes, one file, and inputs by grain

**Date:** 2026-08-18 (r4 — rewritten around the two-axis vocabulary settled with the owner on
2026-08-18, after two reviews of r1) ·
**Status:** Evidence — the design record, now **bound** by the documents it proposed edits to.
[`configuration.md`](../../design/configuration.md),
[`per-point-attributes.md`](../../design/per-point-attributes.md) §3.8 and §3.9, and
[`annotation-write-cycle.md`](../../design/annotation-write-cycle.md) §6.1 (r5) carry it;
[decision 0088](../../decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md) is
the ruling, superseding 0075's separation of the two membership tests; the register's C27 and C28
are renamed onto the new keys at architecture r46. Those documents govern where they and this
differ. The staged plan is
[`2026-08-18-configuration-surface-plan.md`](2026-08-18-configuration-surface-plan.md).
**Reads with:** [`annotations.md`](../../design/annotations.md),
[`records-and-search.md`](../../design/records-and-search.md) §2, and the working memo
[2026-08-15 artifact configurations](2026-08-15-artifact-configurations.md), which this supersedes.

## 1. What this is for

A caller builds a bundle by writing two TOML files and up to six Parquet files. The rules behind
them are sound — every refusal in the old per-point-attributes §4.3 earns its place — but the surface
around them accreted **three spellings for who may see a thing, four mechanisms for where a value
set comes from, two unrelated meanings of `derived`, and no way at all to say where several of the
inputs' fields are**. It is a surface harder to write correctly than the rules it expresses, which
matters here for one reason: every disclosure control in these files is deliberately undefaulted,
so a caller writes each one explicitly, and a surface that makes them hard to write is a surface
that gets them written wrong.

None of this changes what the engine computes. Pre-release it costs no compatibility
([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)): keys
are renamed and mechanisms deleted, never aliased.

## 2. The whole corpus in one file

This is the arXiv demo corpus — today two TOML files, six Parquet inputs and a command line
carrying the rest. It parses; the shapes below were checked against the project's TOML parser.

```toml
# Entity space: identity and attributes, shared by every view. `entity_id` is the
# canonical field name, so a source using it declares no field map at all.
[corpus]
source = "corpus"                      # entity space: identity and attributes

[[view]]
name             = "s0"
title            = "arXiv, August 2026"
source           = "geometry"
fields           = { x = "x", y = "y" }
point_visibility = { field = "categories", default = "public" }
# visibility = "ir:analyst"            # ⊘ the view's own gate — specified, not implemented

[[vocabulary]]
name       = "primary_category"
title      = "arXiv category"
width      = "u16"
value_set  = "closed"
visibility = "public"
source     = "primary_category"
fields     = { title = "label" }

[[attribute]]
name       = "category"
title      = "Category"
field      = "primary_category"
type       = "category"
vocabulary = "primary_category"
render     = true
index      = true

[[attribute]]
name  = "title"
title = "Title"
type  = "text"
index = true

[[layer]]
source     = "hdbscan"
fields     = { members = "members", parent = "parent_id" }
name       = "clusters/hdbscan"
title      = "HDBSCAN clusters"
views      = ["s0"]
membership = "enumerated"
hierarchy  = { kind = "nested", prune_children = true }

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = { fraction = 0.05 }
content                   = { computed = ["centroid", "box"] }   # + on_member_deletion

  [layer.labels]
  source                    = "hdbscan_topics"
  fields                    = { contents = "text" }
  name                      = "topics/hdbscan"
  title                     = "HDBSCAN topics"
  type                      = "text"
  membership                = "enumerated"
  require_member_visibility = "all"
```

A layer small enough to author by hand skips the file entirely, as a vocabulary's values may:

```toml
[[layer]]
name      = "regions/curated"
title     = "Curated regions"
views     = ["s0"]
artifacts = [{ key = "eu-west", contents = ["Western Europe"] }]
```

```bash
tessera build --config schema.toml \
              --file corpus=corpus.parquet   --file geometry=geometry.parquet \
              --file hdbscan=hdbscan.parquet --file hdbscan_topics=topics.parquet \
              --file primary_category=primary_category.parquet \
              --out bundles/notebook --extent auto --mint-id-key
```

**One flag, because there is now one idiom.** The config names a logical key; the command line
binds it to a path, so no environment-specific path appears in a file that belongs in git (§4.1).
`--points`, `--pairs`, `--artifacts`, `--artifact-members` and `--values` all become `--file`, and
a key bound but not declared — or declared but not bound — is a build failure naming both, the
rule `values_key` already had.

Gone: `values_key`, `values_of`, `listing`, `gate`, `ungated`, `artifacts_carry_own`,
`corpus_derived`, `visible_when`, `stable_key`, `label_text`, and the `[access]` block. Six build
inputs become four; the notebook's nine files become five.

## 3. Two axes, and only two

*(The full surface — six blocks, every key, every enumerated value — is tabulated in
[`configuration.md`](../../design/configuration.md) §1, which governs.)*


Everything about who may see an object answers one of two questions.

| Axis | Key | Answers |
|---|---|---|
| label-based | `visibility` | which access label must the viewer hold |
| membership-based | `require_member_visibility` | how much of this object's membership must the viewer already see |

**The second axis is the one the corpus had no name for**, and its absence is why three unrelated
keys were doing its work. A cluster served only when half its members are visible, a value visible
because one point carries it, and a label served only to a viewer holding its whole generating set
are the *same rule at three settings*:

```toml
require_member_visibility = "all"                # every member — containment, the label rule
require_member_visibility = { fraction = 0.5 }   # half of them
require_member_visibility = { count = 50 }       # fifty of them
require_member_visibility = "any"                # one is enough — the vocabulary rule
require_member_visibility = "none"               # no requirement
```

`visible_when`, `corpus_derived` and a vocabulary's `derived` listing all collapse into it.

**The name says what it does not do.** It requires that members be visible; it never *sets* their
visibility. That distinction is the whole of the second axis: a container never grants its members
anything, and reading it the other way round would invert the direction the system is built to
protect.

**This merges what [decision 0075](../../decisions/0075-the-masked-count-is-an-existence-criterion.md)
separated, and the owner has ruled the merge** (2026-08-18). What 0075 established is that the
masked-count test is independent *of the gate* — of `visibility` — and that survives untouched:
the two axes remain orthogonal, and an artifact must satisfy both. What it also happened to
separate, the count test from the containment test, was a distinction without a difference: both
ask how much of the membership the viewer can see, one with a threshold and one with all of it.
C27 and C28 followed those two fields and now follow one, which makes the register shorter rather
than weaker — the thing it watches is that the field is stated, and it still has no default.

## 4. Where a key lives, and the `{ field, default }` pattern

Two levels, and the same shape at both:

| container | its own gate | where each member's label comes from |
|---|---|---|
| `[[view]]` | `visibility` ⊘ | `point_visibility = { field, default }` |
| `[[layer]]` | `visibility` | `artifact_visibility = { field, default }` |

**`point_visibility` sits on the view** (owner ruling, 2026-08-18), which is where a caller looks
for it and where the points source already is. One rule keeps that honest: a point's label is
**entity-space and shared** — the term index and the mask serve every view (views §3) — so two
views may not disagree about a point. With a single view, which is every corpus today, the
question does not arise. With more than one, **the declarations must agree and disagreement is
refused**, naming both views. The alternative readings — labels duplicated per view, or a per-view
mask — are an architectural change rather than a config one, and neither is proposed here.

A container has **one** gate, so it is a bare label. Members have **one each**, so the key says
where to find them and what a member that carries none gets:

```toml
point_visibility    = { field = "categories", default = "public" }
artifact_visibility = { field = "visibility",  default = "inherited" }
```

**The presence of `field` is the declaration that members carry their own labels**, which is
exactly what `artifacts_carry_own` announced and what no key said for points at all. `default` is
what a member with none gets. Omit `default` for points and a point with no label is visible to no
principal — the narrow direction, so it may be omitted; a *widening* default never may.

**The container's gate conjoins and can only narrow**, at both levels. That is already normative
for views ([`views.md`](../../design/views.md) §3: a view's
gate is a label, evaluated by the item-visibility predicate verbatim, *conjunctive with item
labels, never substitutive*). ⊘ **View gating is specified and not implemented** — a bundle has
one coordinate system reachable by every principal that authorises — so the `[[view]]` block is
shaped to carry `visibility` and the key waits.

**Filling never overrides.** A point carrying terms of its own keeps exactly those; the default
lands only where the field is null or empty, and the build reports how many rows it filled.
Overriding is inadmissible rather than merely unwise: a point's terms are disjunctive, `M_auth`
being a union of posting lists, so **any label added to a point can only widen it** — a default
that overrode the field would make every point in a view visible to that label's holders, and
with `public` would make a corpus world-visible on one config line.

### 4.1 The three words

```toml
visibility = "public"       # the reserved label every principal holds
visibility = "ir:analyst"   # any other label
visibility = "inherited"    # (as a member default) the container's gate is the whole of it
                            # (on content) served whenever its artifact is
require_member_visibility = "any" | "all" | { fraction = … } | { count = … } | "none"
```

**`public` is a label, not a keyword** (owner ruling, 2026-08-18). It is interned at term `0`, that
id is never minted for anything else, and every resolved principal term set contains it **by
construction, inside the trust boundary** — not by grant, which would make *public* depend on grant
hygiene, and not by the plugin, which is caller-supplied code. A corpus whose data already carries
the descriptor `public` is adopted, not refused. Modelling open-to-all as a real term is an
ordinary design and Accumulo deployments do it; banning the word to protect a config keyword would
have been the wrong way round.

**`inherited` means the container is the whole of it.** Not `none`, which reads as *visible to
nobody* — the dangerous misreading in a visibility key. Not `public`, which would be false whenever
the container is gated.

**`derived` is retired.** It meant *visible if you can see any member* on a vocabulary and *visible
only if you can see every member* on content — one word, two quantifiers. Both are now settings of
`require_member_visibility`, and the ambiguity has nowhere to live.

**Reserved words collide with a caller's label in exactly one slot.** `public` *is* a label.
`inherited` and the `require_member_visibility` words sit in slots that take no arbitrary label —
except `inherited` as a member default, where a deployment whose label is literally `inherited` is
refused at parse with a message naming the rename. Proportionate for a word nobody chooses as an
access label, and the check the owner declined for `public` precisely because `public` *is* a word
people choose.

**`inherited` is not a value for `point_visibility.default`.** An artifact carrying no label is
reachable whenever its layer is, so *no extra requirement* means something. A point carrying no
terms is in no posting list and so in no principal's mask, and no view gate can add it back, a
gate narrowing and never widening. A point default is always a label.

## 5. Vocabularies are objects

`values_key` names a vocabulary while `key` names a value inside one — and under it sit four
mechanisms for one question:

| Today | What it does |
|---|---|
| `[attribute.values]` | pin codes inline |
| `values_key` + `--values k=<path>` | pin codes from a file, and name the shared vocabulary |
| `values_of = "other_attribute"` | share another attribute's keys, codes and properties |
| `vocabulary = "declared" \| "discovered"` | whether an unknown key at ingest is refused or minted |

A `[[vocabulary]]` block collapses all four. `name` is the identity, so attributes share by naming
and `values_of` has nothing to do. `source` is where values come from — an inline table or a bound
file. `value_set = "closed" | "open"` carries what `declared`/`discovered` carried, in words that
say what the caller gets; the *key* is `value_set` because `values` is the inline table's own name
and TOML cannot hold both on one block.

**`width` moves onto the vocabulary**, where it belongs: it is the code space's width, not a
column's. Sharing is by naming now, so two attributes naming one vocabulary with different widths
would otherwise be newly expressible, and today's refusal for that (§3.9) would have no site — the
minter bounds its draw by the first attribute it finds, so the narrower column would silently fail
to hold codes minted for the wider one.

Four refusals survive verbatim and three are new. Surviving: an unbound source is a build failure
and **never a fall-through to minting**; a `--vocabulary` binding naming no declared vocabulary is
an error; code `0` is the *absent* sentinel; a code in both `reserved` and the live set. New:

- **An attribute naming a vocabulary no block declares is a config parse error**, refused before a
  data file is opened, and never an implicitly minted open vocabulary (owner ruling, 2026-08-18).
  Two blocks of one name likewise. This matters because `vocabulary` changes from holding a keyword
  to holding a reference — a config still saying `vocabulary = "declared"` becomes a reference to a
  vocabulary of that name and must fail as one.
- **`value_set = "closed"` requires a `source`**; `open` does not and starts empty.
- **Attributes inherit their vocabulary's width**, so disagreement is not expressible.

Vocabularies resolve **by name in a second pass**, so block order does not matter; the per-value
`gate` column stays refused as ⊘ unbuilt. `value_set = "open"` with `visibility = "public"` keeps
its warning (C11 with no accountable party), not a refusal.

## 6. Everything nameable takes a `title`

Layers and levels carry one; nothing else does. Attributes have none, so a client shows
`primary_category`. Views have `display_name` in the manifest, served on `/v1/meta`, which the
build hardcodes to the view id with no route to set it. Vocabularies have none. Vocabulary values
have one spelled `label`, a word meaning *access label* everywhere else here.

So `title` on `[[attribute]]`, `[[vocabulary]]`, `[[view]]`, each vocabulary value, and the
existing two. It is presentation metadata on an object whose visibility is already decided, and a
title discloses nothing a name does not.

**`--view` selects which declared `[[view]]` a build writes**, and a value naming none is
refused — otherwise the identifier is spelled twice with nothing reconciling it.

## 7. Two naming rules, and what they change

**A key naming a property is a noun; its value carries the choice.** **A boolean key is an
imperative saying what the build should do.** `render`, `index` and `prune_children` already are.

| Today | Becomes | Why |
|---|---|---|
| `content = { derived = [...] }` | `content = { computed = [...] }` | these are properties the engine recomputes per viewer; `derived` had to stop meaning two things |
| `kind = "label_text"` on supplied content | `name` + `type = "text"` | mirrors `[[attribute]]`; `kind` is a free string the engine never reads, so `label_text` was a tag, not a concept — and a layer carrying both a name and a description is now expressible |
| `stable_key` | `key` | the caller's own address for an artifact; `stable_` is work the prose should do, and `values_key` no longer competes for the word |
| `multi` | left alone | ⊘ unbuilt and refused at parse; half-renaming it buys nothing |

Not `id` for the artifact key, which implies the system assigned it and collides with entity ids.
Not `name`, which suggests something human-facing — a cluster's human name is a **label artifact
attached to it**, with its own visibility, and that distinction is the point of the label mechanism.

## 8. Inputs by grain, and where their fields are

Two grains — one row per point, one row per artifact — plus the members relation. **Nothing in the
config currently says where any of their fields are**: `entity_id`, `x`, `y`, `term_id`, `layer`,
`stable_key` and `member` are hardcoded in the readers, so every corpus renames its columns to suit
us.

**`slice` becomes `view`** (owner ruling, 2026-08-18). A view is a named coordinate system over
the shared entity space — disjoint time ranges, several embedding spaces, several datasets — and
*slice* reads as the temporal case that was merely the first instance. This is the one rename in
this memo that reaches beyond the config: `slices-and-multi-table.md` (now
[`views.md`](../../design/views.md)) is a normative document whose
title, sections and every citation carried the word, as did `SliceDescriptor`, `slice_id`, the
per-slice layout paths and `--slice`. Pre-release that costs nothing but the edit
([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)); it
is simply a larger edit than the rest.

⚠ **One thing to weigh before it is executed:** *viewer* is load-bearing vocabulary here — the
principal is a viewer, visibility is resolved per viewer, and `/v1/meta` is a per-viewer document.
`the view's visibility` and `the viewer's visibility` are different claims one word apart, and the
corpus makes both constantly. `projection` or `space` carries the same generality without the
near-collision. Recorded as a concern, not a refusal; the ruling stands unless reversed.

### 8.0 Every object declares its own source and its own fields

Today `entity_id`, `x`, `y`, `term_id`, `layer`, `stable_key` and `member` are hardcoded in the
readers, so every corpus renames its columns to suit us. Two earlier drafts answered that badly —
first with global `[artifacts]` and `[members]` column maps that floated free of the layers they
described, then with per-layer maps that still let two layers read one file with nothing saying
which rows were whose. **One file per layer settles it by elimination**: no discriminator column,
no selector to configure, and no way for a layer to ingest another's rows. It also matches how the
data is produced — you ran HDBSCAN and you got a file.

So every object that has data declares two things, in its own block:

```toml
source = "hdbscan"                                       # a key bound on the command line
fields = { members = "members", parent = "parent_id" }   # only where the source disagrees
```

**`source` is a bare string** because it can only ever be a bound file key — inline data is an
array or a table on its own key (`artifacts = [...]`, `[vocabulary.values]`), so the two are told
apart by shape and `{ file = … }` was a wrapper earning nothing. Nesting the column map inside it
cost a second level of inline table for a rename list, which is the worst thing on the page for the
least reason.

**They are fields, not columns.** In a Parquet source they are columns; in an inline one they are
keys of a table, and Arrow's own schema calls them fields either way. Naming the map for one
storage form would make the config describe Parquet rather than describe the input.

**The map says *where*, never *whether*.** This is the ambiguity that sank the previous spelling:
`parent = "parent_id"` was doing double duty, asserting both that a layer's artifacts have parents
and where they are — so omitting it could mean either *my source uses the default name* or *my
artifacts have no parents*. The object's own keys are what assert existence — `hierarchy` says whether there are edges and which
way they run, `membership` says there are members, `type` says there is content — and
`fields` only ever answers where to find something already declared. Two refusals fall out and
should be written into §4.3: a field named in the map that the object never declared, and a
declared field whose default name is absent from the source, each naming both halves.

So `fields` is an **override map**: names default to the canonical ones and the table carries only
what differs, which is why most objects name a source and stop.

It is a table rather than loose keys because of the rule the rest of this surface keeps — **a data
reference is keyed `column`/`columns` or wrapped in a table; a bare string is a literal.** Written
flat, `members = "members"` (a column) would sit beside `membership = "enumerated"` (a value) with
nothing distinguishing them.

**The identity column is named once**, on `[corpus]`, and a view repeats it only if its own file
disagrees — identity is entity-space and shared, so declaring it per view would be one fact in two
places waiting to diverge.

**Or the data sits inline**, exactly as a vocabulary's values may. Inline is for what a person
authors — a dozen curated regions, a handful of boundaries — and it makes a small layer writable
with no Parquet pipeline. The corpus and views are file-only: a corpus is never something you type.

**Entity space is its own block, and this is a correction.** An earlier draft hung the attributes
and the access terms off `[[view]].source`, which contradicted its own next paragraph: entity space
is the invariant plane and only coordinates are per view. With one view nothing shows; with two,
`category` would be either duplicated across both points files or undefined as to which the build
reads. So `[corpus]` owns identity and attributes, `[[view]]` owns coordinates and the access
field, and the two may name the same physical file when there is one view — their fields are
disjoint but for the id. The access field is the one entity-space fact declared on a view, by
ruling, under the agreement rule in §4.

**Membership follows the same rule as `visibility`** — bare where it is a literal, a table where it
carries a part:

```toml
membership = "enumerated"                        # rows or a list in the layer's source
membership = "spatial"                           # a shape, supplied as content
membership = { attribute = "primary_category" }  # one artifact per value of that column
```

`kind` would be a wrapper around nothing in two cases out of three.

### 8.0.1 Membership by exclusion, an input spelling

A member source may name the entities a membership **excludes** rather than those it includes:

```toml
fields = { excluding = "not_members" }
```

**This is a spelling of the input file and nothing more.** The build complements it once against
the view's entity set and materialises exactly the membership the included form would have
produced; the segment, the manifest and every read path are byte-identical and never learn which
way the file was written. So there is no masking arithmetic to specify, no leak-register entry, and
no question for I2 — the choice has been made and discarded before anything is stored.

**What it buys is the producer's side.** A list column is the natural shape for membership and
falls over on one case: a condensed tree's root holds every point, so writing it means one cell
carrying the whole corpus, which the producer must materialise whole and which no reader can
stream. Written as an exclusion the same root is *empty* — and the clusters deep enough to have
large exclusion sets are exactly the ones whose member lists are small, so the two spellings cover
each other and a caller writes whichever side is shorter.

**Not to be confused with a complement taken at request time.** That would be a fourth membership
source, with the *"never stale"* character `spatial` and `attribute` already have — an artifact
gaining members whenever a point is ingested, with nobody publishing to it. A coherent thing to
want and a reasonable feature; it is not this, and building it under this name would change what an
artifact means while looking like a file-format convenience.

### 8.1 The access relation

A `list<string>` column on the points table, or a plain `string` where a point carries one term,
minted the way an open vocabulary is minted. A grant is then written in category names rather than
integers, and `terms.parquet` — which the build never reads, and which exists so a human can
translate a grant back — is replaced by a real build output (§8.2).

The exploded `(entity_id, term_id)` file stays as a **source**, named by
`point_visibility.source` like every other, not as a second concept: the build writes that shape as
oracle output regardless and the probe generators produce it natively at 10⁹, so the reader exists
either way. Declaring both is refused.

Three rules for §6.1:

- **A null value and an empty list both mean no access terms, which means visible to no principal.**
  Neither means unrestricted — "null is unspecified, so unrestricted" is the plausible misreading
  and the permissive one. Where a view default is declared, those are the rows it fills.
- **Terms are trimmed** of surrounding whitespace, matching the passthrough plugin.
- **`public` resolves to the reserved term `0`** (§4.1), neither minted nor refused.

**The comma stops being a delimiter** (ruled 2026-08-18). The build joins terms with commas only so
the plugin can re-split them, which was harmless while terms were integers and is not once they are
caller strings: a category containing a comma would split into two, and the point would become
visible to holders of *either*. The plugin interface gains an entry point taking a **list**, the
passthrough implementation of which is the identity, and the delimiter leaves the path rather than
being defended with a refusal.

### 8.2 The term dictionary is a build output

Term ids are assigned in first-appearance order. With caller strings that order is the caller's,
so the build writes the descriptor→id dictionary as a first-class output, and that file is what a
grant is written from and what a rebuild replays. Term `0` is `public` in every one.

A sort was proposed and **withdrawn** (owner ruling, 2026-08-18): it makes the dictionary a
function of content only for a one-shot build, since across writes a descriptor first seen later
takes a later id however each row was ordered. Assignment is a function of history, and the carried
dictionary is the mechanism — which ingest needs regardless.

The consequence to state rather than discover: term ids feed the signature sort key, the signature
order assigns entity ids, entity ids are permanent (I9) and `tessera_id` derives from them. So
`--carry-id-key-from` must carry the dictionary alongside the key; remembering one and forgetting
the other silently renumbers the corpus. Deduplication is required either way — a duplicate
inflates the item's count against `max_terms_per_item`, whose over-bound counter is documented as
correct *because* the reader dedups.

### 8.3 The two grains, and what their columns are called

**An artifact source is one row per artifact; a member source is one row per `(artifact, entity)`.**
Only the second is long, and the asymmetry rests on cardinality rather than on taste.

**The artifact source is one row each.** It was one row per `(artifact, variation)`,
which meant `key`, `parent` and `attached` were repeated on every variation row so that a single
column, the content, could differ. The build then has to *check that the copies agree* and refuse
when they do not — a refusal that exists only because of the duplication that caused it. So the
content becomes a **list column, ordered best first**:

```toml
contents = "contents"   # ["quantum error correction", "a cluster of papers"]
```

Rank is the position in that list, the duplicated identity columns go, and the agreement refusal
goes with them. This is safe precisely where §8.4's is not: a fallback chain is two or three
entries, not millions.

**Two names change with it.** `variation` becomes **`rank`** wherever it survives — in the member
source, where a generating set is per rank. It is a fallback order: the viewer is served the
**first content whose sources they can see entirely**, or nothing, and `variation` named the fact
that the entries differ without saying what orders them, which is the only part a caller must get
right. And `member` becomes **`entity`**, because the member source is long: the column holds a
single entity id drawn from `[identity]`'s space, and a column called `member` on a long table
reads as though it should hold the whole membership — which is the first thing a reader assumes
and the wrong one.

A null `rank` on a member row is the artifact's own membership; rank *k* is the generating set of
`contents[k]`. That is why `require_member_visibility = "all"` on a label means all of *that
rank's* sources rather than all of the cluster's members.

### 8.4 Members keep their own source

Membership is per artifact; a generating set is per rank; the two are distinguished by a null rank.
Contents fold into the artifact row (§8.3) because a fallback chain is two or three entries. Members
do not, and the scale is the whole of the reason: the HDBSCAN root's membership is the whole corpus, so a list column is one cell holding
millions of ids, which cannot stream and which a producer must materialise whole. `groupby().agg`
and `explode` are one line each, so the dataframe idiom is indifferent; the memory profile is not.

`children_keys` **is** deleted from the artifacts file: it is read, validated across rows and
copied forward, and nothing walks it — the parent edge is the only spelling. An input column a
caller populates in good faith and has silently ignored is an ignored `listing` in other clothes.

### 8.5 Labels are declarable where they are used

A label is a first-class artifact by design — its own visibility, its own suppression, because a
synthesis can be more sensitive than its sources. The *config* made a caller hand-assemble that: a
second layer, its own views, `depends_on`, a hierarchy, a criterion to switch off and a content
sub-block. `[layer.labels]` expands to exactly that layer, and the caller declares only what is
theirs to decide.

**What the sugar supplies:** `views`, a flat hierarchy, the `depends_on` edge back to the parent,
and the content wrapper. **What it must never supply:** the gate, the membership requirement, or
the existence of membership data. `membership` is written out because a label's members are its
generating set — the documents it was actually generated from, which the producer knows and the
build cannot derive — and that line is what tells a reader the members file carries label rows.

The label layer's `visibility` defaults to its parent's. That is a default on a disclosure control,
admissible because it is the *parent's* value rather than the widest one, and it may be overridden
narrower — a label set more sensitive than the clusters it names is exactly the case the separate
layer exists for.

## 9. Getting from a Parquet file to a map

The user-experience review's sharpest finding was that r1 rearranged the config without shortening
the path to a first map. Four things block it, and only one is a config key.

**The extent silently corrupts geometry, and this repository's own notebook was in the trap.**
Coordinates are quantised against `--extent` by clamping, so a point outside lands on the boundary
and the build reports nothing. `notebooks/arxiv-corpus.ipynb` passed the grid extent
`0,65536,0,65536` while writing real UMAP coordinates spanning roughly −17…18 and −21…23: every
negative coordinate collapsed onto an axis and the rest occupied a corner nineteen cells wide out
of 65,536. The bundle was well-formed and the map was garbage. The notebook is fixed — it computes
its bounding box, records it and passes that — but **the build-side gap is the finding**: nothing
reports a point outside the extent. Two changes, the second mattering more: **`--extent auto`**,
computing the box from the points file and recording it; and a **report of how many points clamped
to an edge, printed with the data's actual bounds beside the extent given**. Without the second,
`auto` only moves the trap. Every other caller of the grid extent is correct — their points files
hold Morton codes, where it is the required value.

**A corpus with no permission model** writes `point_visibility = { default = "public" }` and no
column. Undefaulted, one line, and it states the disclosure rather than acquiring it.

**`tessera check`** parses the config and reads only the Parquet *schemas*, reporting every declared
attribute against the column that must carry it, every vocabulary source bound, every layer's view
declared, and every disclosure decision as a table. It runs in a second and it is what goes in CI;
today the only way to find an unbound vocabulary or a missing column is a full build.

**A disclosure report beside the bundle.** The build already writes `reports/containment.json`; it
should write `reports/disclosure.json` and print its table — every layer with its `visibility` and
`require_member_visibility`, every vocabulary with its `visibility` and `value_set`, every
attribute with its placement, and what `[layer.labels]` expanded to. Diffable between builds, and a
better answer to *what does this deployment expose* than reading TOML.

**The refusal messages.** Three undefaulted controls fail three different ways today: the gate
refusal names both spellings and the reason; `artifacts_carry_own` and `visible_when` are bare
required fields, so omitting either yields raw serde text naming neither the layer nor the choice.
Both should be optional-and-hand-validated on the gate refusal's template — *what is missing · the
values, spelled out · what each does · why there is no default*. That refusal's own string literal
is wrapped without a continuation, so the operator reads twenty-two spaces mid-sentence, twice; the
view-mismatch refusal has the identical bug.

## 10. What this does not change

- **No invariant moves.** This is the spelling of declarations, the grain of inputs, and the merge
  of two tests that asked one question.
- **Undefaulted disclosure controls stay undefaulted**, and §4.3 keeps every refusal: `render` on
  `keyword`, `index` on a rendered number, code `0`, reserved-and-live collision, shadowed column
  names, `multi` (⊘), `render_in` (⊘), the vocabulary file's `gate` column (⊘).
- **The compiled form is the contract.** `MANIFEST.json` gains titles and loses nothing; the server
  reads only the compiled form.
- **The control plane is unaffected** — JSON has null and needs none of TOML's workarounds — though
  it should adopt the same words so a layer means one thing in both routes.
- **`deny_unknown_fields` covers the merged config**, as it covers both files today.
- **A config declaring no `[[layer]]` is refused when `--artifacts` is given.** That refusal rides
  the absence of `--layers` today, which the merge removes; without restating it a truncated config
  yields a bundle with no layers and no error, indistinguishable from layers whose artifacts were
  all withheld.

## 11. Settled, and what remains

Ruled by the owner on 2026-08-18 and recorded so the design edits need not reopen them: the two
axes and the `require_member_visibility` merge (§3); `public` as a reserved label at term `0`
(§4.1); `inherited` over `none`; `{ column, default }` at both member levels (§4); `open`/`closed`
on `value_set`, and an unresolved vocabulary reference as a parse error (§5); the config named on
the command line, examples using `schema.toml`; trimming, and order-dependent term ids with a
maintained dictionary and no sort (§8.1–8.2); the plugin entry point taking a list (§8.1);
`tessera check` (§9).

Two things this memo settles by argument rather than by ruling, and either could be reversed
without disturbing the rest: the label layer's `visibility` defaulting to its parent's (§8.4), and
`membership` becoming a table so `attribute` membership can name its column (§8).

## 12. Cost and the plan

The staged plan is
[`2026-08-18-configuration-surface-plan.md`](2026-08-18-configuration-surface-plan.md).



Two parsers merging into one, the input readers' hardcoded column names and a new list-column
reader, the CLI's flags and the new `check` subcommand, the manifest's title fields, the layer
declaration struct shared with the control plane, the plugin trait's new entry point, the two
wrapped refusal messages, and the notebook. Every test writing a `schema.toml` or `layers.toml`
moves with them, which is the bulk of the mechanical work.

Normative edits: per-point-attributes §3.8, §3.9, §4.2, §4.3, §4.4 and Appendix R;
annotation-write-cycle §6.1 and Appendix R; decision 0075 and the C27/C28 entries, for the merge;
and whatever in `records-and-search.md` §2 and `annotations.md` cites the renamed keys.
