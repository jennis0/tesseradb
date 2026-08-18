# The configuration surface: one file, one vocabulary for visibility, and inputs by grain

**Date:** 2026-08-18 (r3 — reviewed for user experience and for fail-closed properties; owner
rulings of 2026-08-18 applied; `public` is now a reserved label rather than a config keyword) ·
**Status:** Proposal — evidence, not normative. It proposes edits
to [`per-point-attributes.md`](../../design/per-point-attributes.md) §4 and
[`annotation-write-cycle.md`](../../design/annotation-write-cycle.md) §6.1, which govern; nothing
here binds until those are revised and the four rulings in §11 are made.
**Reads with:** [`records-and-search.md`](../../design/records-and-search.md) §2,
[`annotations.md`](../../design/annotations.md), and the working memo
[2026-08-15 artifact configurations](2026-08-15-artifact-configurations.md), whose grouped-key
shape this supersedes.

## 1. What this is for

A caller builds a bundle by writing two TOML files and up to six Parquet files, and the notebook
that produces the demo corpus writes nine. The declarations they hold are sound — every refusal in
per-point-attributes §4.3 earns its place — but the surface around them has accreted three
separate spellings for *who may know this exists*, four mechanisms for *where a value set comes
from*, no way to give an attribute or a slice a human-readable name, and one input file per grain
even where two grains are the same grain.

None of that is a leak and none of it changes what the engine computes. It is a caller-facing
surface that is harder to write correctly than the rules behind it are, which matters here for one
specific reason: **every disclosure control in these files is deliberately undefaulted**, so a
caller must write each one explicitly, and a surface that makes them hard to write is a surface
that gets them written wrong.

Pre-release, none of this costs compatibility ([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)):
keys are renamed and mechanisms deleted, not aliased.

**What the review changed.** Two reviews ran against r1 — one on user experience and onboarding,
one narrowly on whether the renames preserve every refusal. The user-experience review found that
the shortest path to a first map is blocked almost entirely by things r1 did not touch (§9), and
that r1's own `derived` meant two opposite things in two adjacent keys (§4). The fail-closed review
found three refusals that the vocabulary collapse dropped (§3) and one that the input-shape change
creates (§8). All are dispositioned here.

## 2. The whole change, in one before and after

Today, to declare a category column drawn from an authored value set and a clustering over it:

```toml
# schema.toml
[[attribute]]
name       = "primary_category"
type       = "category"
width      = "u16"
render     = true
index      = true
vocabulary = "declared"
values_key = "primary_category"       # bound to a file on the command line
listing    = "public"

# layers.toml
[[layer]]
name                = "clusters/hdbscan"
title               = "HDBSCAN clusters"
slices              = ["s0"]
membership          = "enumerated"
ungated             = true
artifacts_carry_own = false
visible_when        = { min_fraction = 0.05 }
hierarchy           = { kind = "nested", prune_children = false }
```

After — one config file, and the vocabulary is an object rather than a key smuggled through three
attribute fields:

```toml
# schema.toml
[[vocabulary]]
name       = "primary_category"
title      = "arXiv category"
width      = "u16"                    # the code space's width, not the column's (§3)
value_set  = "closed"                 # or "open": may ingest mint a key nobody declared?
visibility = "public"                 # or "derived": a viewer sees the values their data carries
source     = { file = "primary_category" }   # or an inline [vocabulary.values] table

[[attribute]]
name       = "primary_category"
title      = "Category"
type       = "category"
render     = true
index      = true
vocabulary = "primary_category"

[[layer]]
name               = "clusters/hdbscan"
title              = "HDBSCAN clusters"
slices             = ["s0"]
membership         = "enumerated"
visibility         = "public"         # or an access label: visibility = "ir:analyst"
default_artifact_visibility = "inherited"   # or a label, for artifacts that carry none of their own
visible_when       = { min_fraction = 0.05 }
hierarchy          = { kind = "nested", prune_children = false }
```
```
tessera build --config schema.toml \
              --points points.parquet \
              --artifacts artifacts.parquet --artifact-members members.parquet \
              --values primary_category=primary_category.parquet
```

**Six build inputs become four, plus one vocabulary file per bound vocabulary.** Not three: r1
claimed members could fold into the artifacts table and that does not hold (§8). The notebook's
nine files become five, and `values_key`, `values_of`, `ungated`, `gate`, `listing` and
`artifacts_carry_own` are gone.

## 3. Vocabularies are objects, not attribute fields

**The problem is that `values_key` names a vocabulary while `key` names a value inside one.** The
vocabulary file's columns are `(key, code, label)`, where `key` is `math.GT`; the schema key that
binds that file is `values_key = "primary_category"`, which is the name of the whole set. Two
different things, one word, in the same feature.

Underneath it there are three mechanisms for one question — *where does this value set come from?*
— and a fourth key deciding whether the set is closed:

| Today | What it does |
|---|---|
| `[attribute.values]` | pin codes inline |
| `values_key = "k"` + `--values k=<path>` | pin codes from a file, and *name the shared vocabulary* |
| `values_of = "other_attribute"` | share another attribute's keys, codes and properties |
| `vocabulary = "declared" \| "discovered"` | whether an unknown key at ingest is refused or minted |

A `[[vocabulary]]` block collapses all four. `name` is the identity, so two attributes share a
vocabulary by naming it and `values_of` has nothing left to do. `source` is where the values come
from — an inline table or a bound file — so the inline/file distinction stops being a pair of
mutually exclusive attribute keys with a parse error between them.

**`value_set = "closed" | "open"`** carries what `declared`/`discovered` carried (owner ruling,
2026-08-18: the words are widely understood and stay). The *key* is `value_set` rather than
`values`, because `values` is also the inline table's name and TOML cannot hold both on one block:
`values = "closed"` and `[vocabulary.values]` in the same `[[vocabulary]]` is a duplicate-key
error, which would make a closed vocabulary with an inline set unwritable.

**`width` moves onto the vocabulary**, where it belongs: it is the width of the code space, not of
a column. This is not tidying. Sharing is by naming now, so two attributes naming one vocabulary
with different widths becomes newly expressible, and today's refusal for that case (§3.9 — *"a
shared vocabulary is one code space; two widths over it is one column unable to hold the other's
codes"*) would have no site. The minter bounds its draw by the width of the first attribute it
finds naming the vocabulary, so the narrower column would silently fail to hold codes minted for
the wider one.

### 3.1 The refusals, restated against the new shape

Four survive verbatim and must be written into §4.3 against the new keys: an unbound file source is
a build failure and **never a silent fall-through to minting**, which would open a closed
vocabulary with nobody deciding to; a `--values` binding naming no declared vocabulary is an error,
or the typo merely relocates; code `0` stays the *absent* sentinel and is refused in any source;
and a code in both `reserved` and the live set is refused.

Three are new, each closing a hole the collapse would otherwise open:

- **An attribute naming a vocabulary no `[[vocabulary]]` block declares is a config parse error**
  — refused when the file is read, before a single data file is opened, and never an implicitly
  minted open vocabulary (owner ruling, 2026-08-18). Two blocks of one name is likewise a parse
  error. This matters because `vocabulary` changes meaning: today it holds the words `declared`
  and `discovered`, and under this proposal it holds a reference, so a config carrying the old
  word becomes a reference to a vocabulary named `declared` and must fail as one.
- **`value_set = "closed"` requires a `source`**; `open` does not, and starts empty. Without this
  the failure moves from the declaration to whichever ingest first carries a key — a much worse
  message for the same fault.
- **Attributes sharing a vocabulary inherit its width**, so disagreement is not expressible (above).

`value_set = "open"` with `visibility = "public"` keeps its warning — §3.8's C11-with-no-
accountable-party — and stays a warning, not a refusal.

Two clarifications the review asked for, neither changing behaviour: vocabularies are **resolved by
name in a second pass**, so block order in the file does not matter (the file-order constraint
exists today only because `values_of` pointed at another *attribute*, and nothing can cycle now);
and the per-value `gate` column in a vocabulary file **stays refused** as ⊘ specified-not-built —
§2's list of deleted keys is the layer's `gate`, not that one.

## 4. One vocabulary for visibility

Three spellings answer one question today — *may a viewer know this exists?* An attribute says
`listing = "public" | "per_viewer"`. A layer says `gate = "<label>"` or `ungated = true`, in two
mutually exclusive keys because TOML has no null. And a layer separately says
`artifacts_carry_own = true | false`, a clause about artifacts sitting in a block about a layer.

One key, and its value is **an access label** — with one label reserved:

```toml
visibility = "public"        # the reserved label every principal holds (§4.1)
visibility = "ir:analyst"    # only a principal satisfying this label
visibility = "derived"       # vocabularies only: per viewer, from what their data shows them
```

`public` is the established word for this in Accumulo's visibility model, which is the vocabulary
this system already borrows from (owner ruling, 2026-08-18).

**`derived` is the only word that is not a label**, and it is legal on a vocabulary alone. A
vocabulary's value visibility can be computed from its members (§3.3); a layer's existence cannot
be, so a layer takes a label and nothing else. Any other bare word on a vocabulary is refused
rather than read as a label, since only layers admit arbitrary ones — without that rule the retired
`per_viewer`, or a plain typo, would be accepted as a gate nobody satisfies and recorded as a
control its author believes is set.

**`default_artifact_visibility` is the second, independent axis**, and it stays separate because
the two compose as a conjunction: an artifact is served when the layer is reachable **and** the
artifact's own label is satisfied.

```toml
default_artifact_visibility = "inherited"    # an artifact saying nothing is gated by the layer alone
default_artifact_visibility = "ir:analyst"   # an artifact saying nothing takes this label besides
```

**This replaces `artifacts_carry_own` rather than renaming it**, and the flag disappears. Whether a
given artifact carries a label of its own is a fact about the data, which the data can state; what
the declaration has to settle is what happens to an artifact that states nothing. So the key holds
the default, exactly as a slice's `default_point_visibility` does, and an artifact carrying its own
label always uses it.

It is **required, with no default**, on §4.2's rule: the value an absent line would supply is
`inherited`, the wider of the two, so the caller writes the word.

**A layer is to an artifact what a slice is to a point**, and the two blocks are deliberately the
same shape:

| | gate on the container | default for a member that declares nothing |
|---|---|---|
| `[[layer]]` | `visibility` | `default_artifact_visibility` |
| `[[slice]]` | `visibility` ⊘ | `default_point_visibility` |

The container's gate **conjoins** in both cases and can only narrow — that is already normative for
slices ([`slices-and-multi-table.md`](../../design/slices-and-multi-table.md) §3: a slice's gate is
a label, evaluated by the item-visibility predicate verbatim, *conjunctive with item labels, never
substitutive*, the I12 direction). The member-level default **fills** in both cases. Neither level
does the other's job.

**⊘ A slice gate is specified and not implemented** (slices §3), because a bundle has one
coordinate system reachable by every principal that authorises at all. The `[[slice]]` block is
shaped to carry `visibility` when it lands; until then a slice has no gate to conjoin with, and
§8.1's fill is the whole of what a bulk point label can do.

That is also the correction to an earlier draft of this memo, which called the two levels
asymmetric on the grounds that a point's terms are a posting union and cannot be conjoined. The
union is the *member* level, where fill is right for artifacts and points alike; conjunction was
never the member level's job, and at the container level it applies to both.

Three corrections to r1 here, all from the review and all load-bearing:

- **Not `item_visibility`.** *Item* means point-or-document everywhere else in this system, so that
  key on a layer block reads as a control over the documents — a different and far more alarming
  thing than what it does.
- **Not `derived` for the value.** r1 claimed `derived` meant one thing across both keys. It does
  not: on a vocabulary it is the *computed* case, where visibility falls out of members' labels
  with nobody authoring it, and on a layer it would have been the *authored* case. Same word,
  inverted meaning, in adjacent disclosure controls. A label, or the word `inherited`, says what
  each one does.
- **Not `public` for the inherited state.** It does not make artifacts public; it makes them
  reachable exactly when the layer is, which under a gated layer is not public at all.

### 4.1 `public` is a reserved label, interned at term 0

**`public` is a label like any other, reserved by the system and satisfied by every principal**
(owner ruling, 2026-08-18). It is interned at term id `0`, that id is never minted for anything
else, and every resolved principal term set contains it by construction rather than by grant.

This is why there is no keyword-versus-label ambiguity to disambiguate, and both earlier drafts of
this section were solving a problem that need not exist. r2 refused a corpus term named `public` to
protect a config word; r3 kept two readings of the word apart with an inline-table escape.
**There is one meaning, and it is the same everywhere a label can appear.**

- A **point** carrying `public` in its access list is visible to every principal.
- An **artifact** whose own label is `public` is served to every principal that reaches its layer.
- A **layer** declaring `visibility = "public"` is reachable by every principal.
- A **vocabulary** declaring it publishes its value set to every principal.

Four properties this rests on, each of which belongs in the design rather than in the
implementation that happens to satisfy it:

- **Satisfaction is by construction, not by grant.** The engine adds term `0` to every resolved
  principal term set, inside the trust boundary — not the plugin, which is caller-supplied code,
  and not the credential, which would make *public* depend on grant hygiene and fail differently
  for a principal whose grant was mislaid.
- **Term `0` is reserved in the dictionary** and never minted for another descriptor. A corpus
  whose data already carries the descriptor `public` resolves to `0`: it is adopted, not refused.
- **The descriptor is matched exactly, after trimming.** `Public` is a different label; `public `
  is the same one, per §8.1's trimming rule.
- **Reserving it does not weaken a deny.** Suppression and deletion act on entities through the
  overlay, never through terms, so a suppressed point tagged `public` stays suppressed. The two
  mechanisms do not meet.

An empty string stays refused rather than read as a label nobody satisfies, since that is what an
unset template variable renders to.

### 4.2 The no-default rule, and where it is not yet kept

Three keys in this family are required with no parser-level default — a vocabulary's `visibility`,
a layer's `visibility`, and `default_artifact_visibility` — plus `visible_when`. SA §7's rule
governs and none of them may become an `Option` with a fallback: the value an absent line would supply is the
widest one there is.

The proposal makes them *easier to write correctly*: `visibility = "public"` is one word where
`gate`/`ungated` was a two-key dance whose omission had to be caught by hand. But only one of the
four currently teaches the caller anything when it is missing. The gate refusal names both
spellings and the reason; `artifacts_carry_own` and `visible_when` are bare required fields today, so
omitting either yields raw serde text with no statement of what the values do and no mention of the
layer. Both should be parsed as optional and hand-validated, on the template the gate refusal
already sets: *what is missing · the values, spelled out · what each one does · why there is no
default*.

Two smaller things in that same message: the gate refusal's string literal is wrapped without a
continuation, so the operator reads twenty-two spaces mid-sentence, twice, and the slice-mismatch
refusal has the identical bug. They are the most-read messages in the file.

Note what does **not** move: `visible_when` stays its own key. It is a masked-count test, not a
disclosure control over existence-in-principle, and it is independent of everything else
([decision 0075](../../decisions/0075-the-masked-count-is-an-existence-criterion.md)).

## 5. Everything nameable takes a `title`

Layers and levels carry `title` and it reaches the client. Nothing else does:

- **Attributes have no title**, so a client's filter UI shows `primary_category`.
- **Slices have one and it cannot be set.** `MANIFEST.slices[].display_name` exists and is served
  on `/v1/meta`; the build assigns it the slice id and there is no route to say otherwise.
- **Vocabularies have no title** — they have only the logical key that binds their file.
- **Vocabulary values have one, spelled `label`.** That word means *access label* everywhere else
  in this system. It becomes `title` like every other one.

So: `title` on `[[attribute]]`, `[[vocabulary]]`, each vocabulary value, each declared slice, and
the existing two. It is presentation metadata on an object whose visibility is already decided —
per-point-attributes §3.8's rule that properties are not separately gated covers it, and a title
discloses nothing a name does not.

This needs a `[[slice]]` block, since a slice is named on the command line today and has nowhere
to carry a title. With both, the rule must be stated: **`--slice` selects which declared
`[[slice]]` this build writes, and a value naming no declared slice is refused** — otherwise the
same identifier is spelled in two places with nothing reconciling them.

## 6. Key names, by two rules

The keys are in at least four styles: bare verbs (`render`, `index`), bare adjectives (`multi`,
`ungated`), preposition fragments (`visible_when`, `depends_on`, `values_of`, `render_in`), and one
clause about a different subject (`artifacts_carry_own`).

**A key naming a property is a noun; its value carries the choice.** `visibility`, `hierarchy`,
`vocabulary`, `analyser`, `membership` already are. This is the rule `visibility` applies over
`ungated`, and it is what removes the clause-shaped key.

**A key that is a boolean is an imperative saying what the build should do.** `render`, `index`,
`prune_children` already are.

What the two rules change, beyond §3 and §4:

| Today | Becomes | Why |
|---|---|---|
| `corpus_derived` on supplied content | `visibility` in §4's vocabulary | a disclosure control wearing an adjective |
| `values` (a vocabulary's value set) | `[vocabulary.values]` only, with `value_set` as the switch | the word also names an artifact's content payload; one meaning each |
| `stable_key` | `key` | §7 |
| `children_keys` in the artifacts file | deleted | §8.3 |

`multi` is left alone. r1 proposed renaming it; it is ⊘ unbuilt and refused at parse, and
half-renaming an unbuilt key buys nothing. `visible_when` and `depends_on` stay — they read as
fragments, but each is already in the corpus and understood, and renaming them buys nothing.

## 7. `stable_key` becomes `key`

It is the caller's own address for an artifact: the name that survives a rebuild, the thing an edge
points at, and the sort key ordinals are assigned in. `stable_` is doing work that the surrounding
prose should do instead — nothing about a bare `key` suggests it is unstable.

Once `values_key` is gone (§3), `key` is unambiguous, and the artifact columns come out as one
family: `layer`, `key`, `attached_key`, `parent_key`.

Not `id`, which implies the system assigned it and collides with entity ids and `tessera_id`. Not
`name`, which suggests something human-facing — a cluster's human name is a **label artifact
attached to it**, in its own layer with its own visibility, and that distinction is the point of
the whole label mechanism.

## 8. Inputs by grain

There are two grains here — one row per **point**, one row per **artifact** — and one relation that
is neither. The current split has four files across them plus one vocabulary file per vocabulary.

**Points already carry their attributes.** The build reads the declared attribute columns off the
points file, so the flat-table-plus-column-mapping paradigm is implemented; only geometry and
access terms sit outside it.

**Column names come from the config.** `entity_id`, `x`, `y` and `term_id` are hardcoded in the
readers, so every corpus must rename its columns to suit us. Under a config that already maps
columns to attribute slots, mapping these too is the same mechanism:

```toml
[points]
entity = "entity_id"
x      = "x"                          # or: morton = "m", residual = "r"
y      = "y"
```

The three accepted geometry shapes (`x`/`y`, `morton` + `residual`, bare `morton`) are unchanged
and remain mutually exclusive — declaring keys from two of them is refused, and both Morton shapes
still require the identity extent. The config must show one example of each, because a caller with
a Morton column cannot otherwise tell whether the `x`/`y` keys must be absent (they must).

### 8.1 The access relation: a column, or a label on the slice

**Two sources. A declaration may name either, or both — where both, the slice label fills
what the column leaves empty.**

```toml
[[slice]]
name = "s0"
title = "arXiv, August"
default_point_visibility = "public"   # points that carry no terms of their own take this label
# visibility = "ir:analyst"           # ⊘ the slice's own gate — specified, not implemented

[access]
terms = { column = "categories" }     # list<string>, or a plain string for one term per point
terms = { file = "pairs" }            # the exploded (entity_id, term_id) form, bound with --pairs
```

The **column** is the general case: a `list<string>` where a point carries several terms, or a
plain `string` where it carries one, minted the way an open vocabulary is minted. Two consequences
beyond one fewer file: a grant is written in category names instead of integers, and
`terms.parquet` — which the build never reads, and which exists so a human can translate a grant
back — is replaced by a real build output (§8.2).

**`default_point_visibility` on the slice is the bulk form**, for the corpus where every point
carries the same label. The name states the behaviour: it is what a point takes when it says
nothing, never what overrides what it said. It is a better answer than the constant column it replaces, on three counts and not
merely on convenience:

- **It is visible where disclosure decisions are reviewed.** A label in the config appears in a
  config diff and in the disclosure report (§9); the same label repeated down a data column appears
  in neither, and no reviewer reads 10⁶ rows to find it.
- **It costs nothing to store.** At 10⁹ points a constant column is 10⁹ duplicated strings in the
  producer's memory and on disk, for one fact.
- **It is the honest shape of the statement.** *Every point in this slice is public* is a property
  of the slice, and writing it per row makes a corpus-wide decision look like per-row data.

It takes a list as well as a single label, since a point may carry several terms.

**Where both are declared, the slice label fills and never overrides.** A point whose column value
is null or empty takes the slice's label; a point carrying terms of its own keeps exactly those.
The build reports how many points were filled, and declaring both is a warning rather than a
refusal — the caller is told, and the build proceeds.

**The rule is *fill*, not *take the config*, because a point's terms are disjunctive.** `M_auth` is
a union of posting lists (§6.1–§6.3), so a point carrying `math.GT` is visible to every principal
holding `math.GT`, and **adding a term to a point can only widen it**. A slice label that overrode
or joined the column would therefore make every point in the slice visible to every holder of that
label, discarding the corpus's access relation — and with `public` it would make the whole slice
world-visible on the strength of one config line and a warning nobody is obliged to read. That is
C-register widening with no accountable party, arriving through a convenience.

Nor is conjunction the safe fallback it looks like: it is **not expressible** in the current mask
model at all. AND is reachable only through the access-expressions design
([`core-access-expressions.md`](../../design/core-access-expressions.md), provisional), and there
it works by minting a compound term at build time, not by combining sets per request. So the choice
is genuinely between *fill* and *widen*, and only one of them is admissible.

Filling is also what the bulk case actually wants: a corpus with no per-point terms has an empty
column everywhere, so every point takes the label and the outcome is identical to declaring no
column at all. A corpus with a partly-populated column gets the label exactly where it said
nothing — which is the reading a caller who wrote both would expect, and the only one that cannot
widen a point they had already restricted.

The exploded file stays **as a source, not as a second concept**. That is not compatibility: the
build *writes* an exploded `pairs.parquet` as an output for the reference oracle, and the probe
generators at 10⁹ scale produce that shape natively, so the reader exists either way.

Three rules the fail-closed review requires, all of which must be written into §6.1:

- **With no slice label declared, a null value and an empty list both mean no access terms, which
  means visible to no principal.** Neither means unrestricted. A column introduces null where the
  pairs file had only absence, and "null is unspecified, so unrestricted" is the plausible
  misreading and the permissive one. Where a slice label *is* declared, those are the rows it
  fills, and the count is reported.
- **Terms are trimmed of surrounding whitespace** (owner ruling, 2026-08-18), matching what the
  passthrough plugin already does, so `" math.GT"` and `"math.GT"` are one term.
- **`public` resolves to the reserved term `0`** (§4.1), neither minted nor refused, so a corpus
  that already models open-to-all as a term keeps writing it and gets the meaning it expects.

### 8.2 The term dictionary is a build output

Term ids are assigned in first-appearance order, which is canonical today only because the pairs
reader hands each item its terms already sorted. With caller strings there are no ids yet, so
first-appearance order becomes the order the caller happened to write their lists in.

**The owner has ruled this acceptable, on the condition that the term list is maintained.** So the
build writes the descriptor→id dictionary as a first-class output beside the bundle, and that file
— not a notebook byproduct — is what a grant is written from and what a rebuild replays. Term `0`
is `public` in every such dictionary (§4.1).

r2 proposed additionally sorting each row's list, to make the dictionary a function of content
rather than of layout. **That is withdrawn** (owner ruling, 2026-08-18): it holds only for a
one-shot build. Once terms arrive across writes, a descriptor first seen in a later batch takes a
later id however each row was ordered — assignment is a function of history, not of any one file —
so the sort would buy determinism in the mode where it matters least, at the cost of a rule that is
true in one mode and false in the other. The carried dictionary is the mechanism, and ingest needs
it regardless.

The consequence to state plainly rather than discover: term ids feed the signature sort key, the
signature order assigns entity ids, and entity ids are permanent (I9) with `tessera_id` derived
from them. So a rebuild preserving identity replays the recorded dictionary exactly as it already
replays the recorded batch size and identity key — and `--carry-id-key-from` should carry the
dictionary with them, since a caller who remembers one and forgets the other gets a silently
renumbered corpus.

Deduplication is required either way: a duplicate term inflates the item's descriptor count against
`max_terms_per_item`, whose over-bound counter is documented as correct *because* the pairs reader
dedups.

### 8.3 Members keep their own file

r1 proposed folding members into the artifacts table as a list column. **That does not hold**, on
two counts the review made concrete. The grain is wrong: membership is per artifact, a generating
set is per variation, and the two are distinguished today by a null variation — on one
`(artifact, variation)`-grained table with one member list, either the full membership repeats on
every variation row or it is meaningful on one row and silently ignored on the others. And the
scale is wrong: the HDBSCAN root's membership is the whole corpus, so the list column is a single
cell holding millions of ids, which cannot stream and which a producer must materialise whole. The
current file streams in batches. The dataframe argument does not favour the collapse either —
`groupby().agg(list)` and `explode()` are one line each.

`children_keys` **is** deleted from the artifacts file. It is read, validated for agreement across
rows, and copied forward, and nothing ever walks it; the parent edge is the only spelling, which
is what the notebook's own prose already says. An input column a caller can populate in good faith
and have silently ignored is the same failure as an ignored `listing`, in different clothes.

### 8.4 The delimiter, and the better fix

An item's access label is built by **joining its terms with commas**, and the plugin splits that
string on commas, trims, and drops empties. Today the terms are integers, so no term can contain
the delimiter. Move caller strings into that position and a category containing a comma splits into
two terms — and the item becomes visible to a principal holding *either*, silently wider than the
corpus states. Trimming does not help with this one; only a refusal or a change of representation
does.

**The representation is the better target.** The join exists because the build's only input was
integer ids and the plugin boundary takes an opaque label. A list column already *is* the term
list, so the build joins strings only to re-split them. Giving the plugin interface an entry point
that takes a list of terms — the passthrough implementation of which is the identity — removes the
delimiter from the path entirely, and the refusal becomes unnecessary.

**Ruled 2026-08-18: the plugin interface takes a list.** So the delimiter is gone rather than
defended, and §8.1's comma refusal goes with it — a category containing a comma is an ordinary
term. The plugin boundary is unchanged in kind: it still maps caller-supplied bytes to descriptors,
and passthrough still implements the identity.

## 9. Getting from a Parquet file to a map

The review's sharpest finding is that r1 rearranged the config without shortening the path to a
first map, and that the things actually blocking it are elsewhere. Four, in the order a newcomer
hits them:

**The extent silently corrupts geometry, and this repository's own notebook was in the trap.**
Coordinates are quantised against `--extent` by clamping, so a point outside it lands on the
boundary and the build reports nothing. `notebooks/arxiv-corpus.ipynb` passed the grid extent
`0,65536,0,65536` while writing real UMAP coordinates spanning roughly −17…18 and −21…23: every
negative coordinate collapsed onto an axis and the rest occupied a corner nineteen cells wide out
of 65,536. The bundle was well-formed and the map was garbage. The notebook is fixed — it computes
its bounding box, records it in its manifest and passes that — but **the build-side gap is the real
finding**: nothing reports a point outside the extent. Two changes, and the second matters more
than the first: **`--extent auto`**, computing the bounding box from the points file and recording
it in the manifest; and a **build-time report of how many points clamped to an extent edge, printed
with the data's actual bounds beside the extent given**. Without the second, `auto` only moves the
trap. (Every other caller of the grid extent is correct — their points files hold Morton codes,
where it is the required value.)

**`--pairs` is mandatory, so there is no way to build from a bare points file.** A data scientist
with a dataframe and no permission model has nothing to write, and that is where a first attempt
stops. This is a disclosure question, so it must not acquire a default — but `default_point_visibility` on
the slice (§8.1) answers it in one line:

```toml
[[slice]]
name = "s0"
default_point_visibility = "public"
```

Nothing is defaulted, the caller states the disclosure explicitly in the file where disclosure
decisions are read, and it carries into the disclosure report below without anyone inspecting the
data. An earlier draft put the same statement in a constant data column; that works and is strictly
worse, because a corpus-wide decision then lives where no reviewer looks.

**`--slice` carries no disclosure content and should default** to the single declared slice when
there is exactly one.

**`tessera check`** — accepted by the owner, 2026-08-18. It parses the config and reads only the
Parquet *schemas*, reporting: every declared attribute against the column that must carry it
(present? compatible type?), every vocabulary source bound, every layer's slice declared, every
member id in range where cheap to tell, and every disclosure decision as a table. It runs in a
second and it is what goes in CI. Today the only way to discover an unbound vocabulary or a missing
column is to run a full build.

**A disclosure report beside the bundle.** The build already writes `reports/containment.json`; it
should also write `reports/disclosure.json` and print its table — every layer with its `visibility`
and `default_artifact_visibility`, every vocabulary with its `visibility` and `value_set`, every attribute
with its placement. It is diffable between builds and it is the artefact a reviewer signs off,
which is a better answer to *what does this deployment expose* than reading TOML.

One thing already right, and the model the rest should copy: the identity-key refusal names all
four routes out of the failure it reports.

## 10. What this does not change

- **No invariant moves.** Every rule about what may be computed, gated or served is untouched;
  this is the spelling of the declarations, the grain of the inputs, and one deleted mechanism.
- **The undefaulted disclosure controls stay undefaulted**, and §4.3 keeps every existing refusal:
  `render` on `keyword`, `index` on a rendered number, code `0`, reserved-and-live collision,
  shadowed column names, `multi = true` at all (⊘), `render_in` (⊘), and the vocabulary file's
  per-value `gate` column (⊘).
- **The compiled form is the contract**, not the config. `MANIFEST.json` gains `title` fields and
  loses nothing; the server keeps reading only the compiled form, per §4.1.
- **The control plane is unaffected.** `PUT /control/layers` takes JSON, which has null and needs
  none of TOML's workarounds; it should adopt the same words so a layer means one thing in both
  routes, but that is a rename in one struct.
- **Vocabulary files keep their shape**, minus `label` → `title`.
- **`deny_unknown_fields` covers the merged config**, as it covers both files today. It is the rule
  that keeps a mistyped disclosure control from reading as an absent one.
- **A config declaring no `[[layer]]` is refused when `--artifacts` is given.** Today that refusal
  rides the absence of the `--layers` flag, which the merge removes; without restating it, a
  truncated config yields a bundle with no layers and no error, which no client can distinguish
  from layers whose artifacts were all withheld.

## 11. Rulings

All settled as of 2026-08-18. Recorded here so the design edits can be made without reopening them.

- **The config file is named on the command line**, as `--schema` and `--layers` are today, so it
  has no fixed name; the examples here and in the design documents use `schema.toml`. That also
  disposes of the collision the review raised — `tessera.toml` is the *server* config, and nothing
  now proposes reusing the name. §4.1's line *"the schema never appears in `tessera.toml`"* still
  needs rewording so it is not read as forbidding the merge.
- **The plugin interface gains an entry point taking a list of terms**, so the build stops joining
  strings with commas only to re-split them, and the delimiter leaves the path entirely (§8.4).
  The passthrough implementation is the identity. The comma refusal in §8.1 is then unnecessary and
  is dropped with it.
- **`open`/`closed` for a vocabulary's value set** (§3), on the key `value_set` — the word `values`
  being unavailable, since TOML cannot carry it as both a switch and the inline table.
- **`public` is a reserved label interned at term `0`** (§4.1), meaning the same thing on a point,
  an artifact, a layer and a vocabulary.
- **Access terms are trimmed** (§8.1); **term ids stay order-dependent with a maintained
  dictionary**, and the proposed sort is withdrawn (§8.2).
- **An unresolved vocabulary reference is a config parse error** (§3.1), refused before any data
  file is opened.
- **`default_point_visibility` on a slice** as the bulk access source (§8.1). Declared alongside an
  access column it **fills** points with no terms of their own and never overrides one that has
  them, warning with the count rather than refusing. Overriding is inadmissible: a point's terms
  are disjunctive, so any join widens, and conjunction is not expressible in the current mask
  model.
- **`default_artifact_visibility` replaces `artifacts_carry_own`** (§4), on the same shape: the
  data says whether an artifact carries a label, the declaration says what an artifact that says
  nothing gets. Required, with `"inherited"` written out where the layer alone gates them. The
  layer and slice blocks are one shape — a container gate that conjoins, and a member default that
  fills — with the slice's gate ⊘ until slice gating is built.
- **`tessera check`** (§9).

## 12. Cost

Two parsers, the input readers' hardcoded column names and a new list-column reader, the CLI's
`--schema`/`--layers`/`--pairs` flags and the new `check` subcommand, the manifest's title fields,
the layer declaration struct shared with the control plane, the two wrapped refusal messages, and
the notebook. Every existing test that writes a `schema.toml` or a `layers.toml` moves with them,
which is the bulk of the mechanical work.

Normative edits: per-point-attributes §3.8, §3.9, §4.2, §4.3, §4.4 and Appendix R;
annotation-write-cycle §6.1 and Appendix R; and whatever in `records-and-search.md` §2 and
`annotations.md` cites the renamed keys.
