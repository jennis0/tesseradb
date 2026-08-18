# The configuration surface: one file, one vocabulary for visibility, and inputs by grain

**Date:** 2026-08-18 · **Status:** Proposal — evidence, not normative. It proposes edits to
[`per-point-attributes.md`](../../design/per-point-attributes.md) §4 and
[`annotation-write-cycle.md`](../../design/annotation-write-cycle.md) §6.1, which govern; nothing
here binds until those are revised and the rulings in §10 are made.
**Reads with:** [`records-and-search.md`](../../design/records-and-search.md) §2,
[`annotations.md`](../../design/annotations.md), and the working memo
[2026-08-15 artifact configurations](2026-08-15-artifact-configurations.md), whose grouped-key
shape this supersedes.

## 1. What this is for

A caller builds a bundle by writing two TOML files and up to six Parquet files, and the notebook
that produces the demo corpus writes nine. The declarations they hold are sound — every refusal in
per-point-attributes §4.3 earns its place — but the surface around them has accreted three
separate spellings for *who may know this exists*, three mechanisms for *where a value set comes
from*, no way to give an attribute or a slice a human-readable name, and one input file per grain
even where two grains are the same grain.

None of that is a leak and none of it changes what the engine computes. It is a caller-facing
surface that is harder to write correctly than the rules behind it are, which matters here for one
specific reason: **every disclosure control in these files is deliberately undefaulted**, so a
caller must write each one explicitly, and a surface that makes them hard to write is a surface
that gets them written wrong.

Pre-release, none of this costs compatibility ([decision 0048](../../decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)):
keys are renamed and mechanisms deleted, not aliased.

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
```
tessera build --schema schema.toml --layers layers.toml \
              --points points.parquet --pairs pairs.parquet \
              --artifacts artifacts.parquet --artifact-members members.parquet \
              --values primary_category=primary_category.parquet
```

After — one config file, and the vocabulary is an object rather than a key smuggled through three
attribute fields:

```toml
# tessera.toml
[[vocabulary]]
name       = "primary_category"
title      = "arXiv category"
values     = "closed"                 # or "open": may ingest mint a key nobody declared?
visibility = "public"                 # or "derived": a viewer sees the values their data carries
source     = { file = "primary_category" }   # or an inline [vocabulary.values] table

[[attribute]]
name       = "primary_category"
title      = "Category"
type       = "category"
width      = "u16"
render     = true
index      = true
vocabulary = "primary_category"

[[layer]]
name            = "clusters/hdbscan"
title           = "HDBSCAN clusters"
slices          = ["s0"]
membership      = "enumerated"
visibility      = "public"            # or an access label: visibility = "ir:analyst"
item_visibility = "inherited"         # or "derived": each artifact carries its own label
visible_when    = { min_fraction = 0.05 }
hierarchy       = { kind = "nested", prune_children = false }
```
```
tessera build --config tessera.toml \
              --points points.parquet --artifacts artifacts.parquet \
              --values primary_category=primary_category.parquet
```

Nine notebook files become three; six build inputs become three; and `values_key`, `values_of`,
`ungated`, `gate`, `listing` and `artifacts_carry_own` are gone.

## 3. Vocabularies are objects, not attribute fields

**The problem is that `values_key` names a vocabulary while `key` names a value inside one.** The
vocabulary file's columns are `(key, code, label, gate?)`, where `key` is `math.GT`; the schema key
that binds that file is `values_key = "primary_category"`, which is the name of the whole set. Two
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
mutually exclusive attribute keys with a parse error between them. `values = "closed" | "open"`
carries what `declared`/`discovered` carried, in words that say what the caller gets rather than
what the caller did.

Three refusals in per-point-attributes §4.4 survive verbatim and must be restated against the new
shape: an unbound file source is a build failure and **never a silent fall-through to minting**,
which would open a closed vocabulary with nobody deciding to; a `--values` binding naming no
declared vocabulary is an error, or the typo merely relocates; and code `0` stays the *absent*
sentinel and is refused in any source. `values = "open"` with `visibility = "public"` keeps its
warning (§3.8's C11-with-no-accountable-party), not a refusal.

**A vocabulary block also has somewhere to put a title**, which a `values_key` string did not.

## 4. One vocabulary for visibility

Three spellings answer one question today — *may a viewer know this exists?* An attribute says
`listing = "public" | "per_viewer"`. A layer says `gate = "<label>"` or `ungated = true`, in two
mutually exclusive keys because TOML has no null. And a layer separately says
`artifacts_carry_own = true | false`, a clause about artifacts sitting in a block about a layer.

One key, one set of words:

```
visibility = "public"        # everyone; the value set or the layer is known to all
visibility = "<label>"       # only a principal satisfying this access label
visibility = "derived"       # per viewer, from what their own data shows them
```

`derived` means the same thing in both places it appears: worked out per viewer rather than fixed
in advance. On a vocabulary that is today's `per_viewer` listing — a value is visible because the
viewer can see a member of it (§3.3), and the value set they are shown is theirs. There is no
`derived` for a layer, because a layer's existence is not derived from its members; a layer is
`public` or it names a label.

**`item_visibility` is the second, independent axis**, and it must stay separate because the two
compose as a conjunction: an artifact is served when the layer is reachable **and** the artifact's
own label is satisfied.

```
item_visibility = "inherited"   # today's artifacts_carry_own = false
item_visibility = "derived"     # today's artifacts_carry_own = true
```

**`inherited`, not `public`.** The off state does not make artifacts public — it makes them
reachable exactly when the layer is, which under a gated layer is not public at all. A word that
claims more than it means is the failure this corpus keeps a `Status:` line to prevent, and it
would appear here in a disclosure control.

Two properties of the current design that this must not weaken:

- **No default, on any of them.** Both `visibility` keys and `visible_when` stay required, on
  SA §7's rule and for the reason `ungated = true` exists: the value an absent line would supply
  is the widest one there is. `visibility = "public"` is one explicit word, so the ergonomics
  improve and the explicitness does not change.
- **`public` becomes a reserved word, with a refusal.** A deployment whose access label is
  literally `public` must fail the build — *"`public` is reserved as a visibility value; rename
  the label"* — and must never quietly become world-reachable. Labels here look like `ir:analyst`
  or `math.GT`, so the collision is unlikely; unlikely is not a reason to leave it silent.

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

This needs a `[[slice]]` block in the config, since a slice is named on the command line today and
has nowhere to carry a title.

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
| `multi` | `multi_valued` on the type, not a flag | it describes the data, not an instruction to the build (⊘ unbuilt either way, records §5) |
| `corpus_derived` on supplied content | `visibility` in §4's vocabulary | it is a disclosure control wearing an adjective |
| `values` (a vocabulary's value set) | `[vocabulary.values]` only | the word also names an artifact's content payload; one meaning each |
| `stable_key` | `key` | §7 |

`visible_when` and `depends_on` stay. They read as fragments but each is a genuine relation to
something else, and renaming them buys nothing.

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

## 8. Inputs by grain, not by kind

There are two grains here, not six. One row per **point**, and one row per **artifact**. The
current split has four files across those two grains plus two vocabulary files.

**Points already carry their attributes.** The build reads the declared attribute columns off the
points file — the flat-table-plus-column-mapping paradigm is implemented and only geometry and
access terms are outside it.

**The access relation becomes a list column.** A `list<string>` column on the points table, named
by the config, minted the way an open vocabulary is minted. Two consequences beyond one fewer
file: a grant is written in category names instead of integers, and `terms.parquet` — which the
build never reads, and which exists so a human can translate a grant back — has nothing left to
do. Nothing downstream depends on the exploded file's `(term, entity)` ordering; the pairs scanner
already guarantees no row order at all.

The exploded file stays **as a source, not as a second concept**: one config key, two spellings.

```toml
[access]
terms = { column = "categories" }     # or { file = "pairs" }, the exploded (entity_id, term_id) form
```

That is not compatibility. The build *writes* an exploded `pairs.parquet` as an output for the
reference oracle, and the probe generators at 10⁹ scale produce that shape natively, so the reader
exists either way and the alternative would be rewriting fixtures to no benefit.

**Artifacts and members are one file.** Different grain from points — a tree whose children do not
exhaust their parents is not expressible as a column on a point, and neither is a per-variation
generating set — but the same grain as each other, with membership as a list column on the artifact
row. One row per `(artifact, variation)`, as today, carrying its own members.

**Column names come from the config.** `entity_id`, `x`, `y` and `term_id` are hardcoded in the
readers, so every corpus must rename its columns to suit us. Under a config that already maps
columns to attribute slots, mapping these too is the same mechanism:

```toml
[points]
entity = "entity_id"
x      = "x"
y      = "y"
```

The three accepted geometry shapes (`x`/`y`, `morton` + `residual`, bare `morton`) and their rules
— in particular that both Morton branches require the identity extent — are unchanged.

## 9. What this does not change

- **No invariant moves.** Every rule about what may be computed, gated or served is untouched;
  this is the spelling of the declarations, the grain of the inputs, and one deleted mechanism.
- **The undefaulted disclosure controls stay undefaulted**, and §4 keeps every existing refusal:
  `render` on `keyword`, `index` on a rendered number, code `0`, reserved-and-live collision,
  shadowed column names, `multi = true` at all (⊘), `render_in` (⊘).
- **The compiled form is the contract**, not the config. `MANIFEST.json` gains `title` fields and
  loses nothing; the server keeps reading only the compiled form, per §4.1.
- **The control plane is unaffected.** `PUT /control/layers` takes JSON, which has null and needs
  none of TOML's workarounds; it should adopt the same words so a layer means one thing in both
  routes, but that is a rename in one struct.
- **Vocabulary files keep their shape**, minus `label` → `title`.

## 10. What the owner must rule

1. **`public` as a reserved word** (§4) — refuse a `public` access label, or find another
   disambiguation. Recommended: refuse, with a message naming the rename.
2. **`item_visibility = "inherited"` rather than `"public"`** (§4). Recommended as written: the
   off state is not public under a gated layer, and a disclosure control must not overclaim.
3. **`values = "closed" | "open"` replacing `declared` | `discovered`** (§3). These words appear in
   §3.4, §3.8 and the ingest path's `422`-or-mint rule; renaming them touches prose in three
   documents. Recommended, but it is the largest prose cost in this memo and is severable.
4. **One config file** (§2) — `schema.toml` and `layers.toml` merge into `tessera.toml`. Both are
   build inputs compiled into the same manifest and read by the same command, so the split has no
   remaining argument, but §4.1's *"the schema never appears in `tessera.toml`"* line was written
   against a **server** config of that name and must be reworded so it is not read as forbidding
   this.
5. **The exploded pairs file as a second source** (§8) — accepted as a source under one key, or
   deleted outright in favour of the list column with the probe generators rewritten.

## 11. Cost

Two parsers (`schema.rs`, `layers.rs`), the input readers' hardcoded column names and a new list
column reader, the CLI's `--schema`/`--layers`/`--pairs`/`--artifact-members` flags, the manifest's
title fields, the layer declaration struct shared with the control plane, and the notebook. Every
existing test that writes a `schema.toml` or a `layers.toml` moves with them, which is the bulk of
the mechanical work.

Normative edits: per-point-attributes §3.8, §4.2, §4.3, §4.4 and Appendix R; annotation-write-cycle
§6.1 and Appendix R; and whatever in `records-and-search.md` §2 and `annotations.md` cites the
renamed keys.
