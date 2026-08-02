# Per-point attributes and categories — design

**Date:** 2026-08-02
**Status:** **Provisional — reviewed, no open decisions.** To become normative: the §6 amendments
folded into `architecture.md` and `contracts.md`, and an owner ruling on whether Appendix A gains a
residency *ceiling* (§2.3 — the plan step reports against it either way).
**Reads against:** architecture §4 (I2, I3, I9, I12), §5.3, §8.2, §8.3, §10.3, §10.5, Appendix A,
Appendix C (C8, C11); contracts §2.1–§2.4, §3.2, §3.4; `slices-and-multi-table.md` §51, §53, §61,
§80, §87 (itself provisional); `system-architecture.md` §7; design memo 2026-07-29 (secondary
attribute indexing); decision [0013](../decisions/0013-mark-specified-vs-implemented.md).
**Citation convention:** unprefixed §n is the architecture design; this document's own sections are
cited as **spec §n**.

---

## 1. Summary

Per-point data is declared once by what it is **used for** — render, filter, inspect — and the
system derives where it lives from §10.3's routing principle. The caller never names a placement.

Categories are the first-class case: a fixed-width code on the hot path, a term posting in entity
space for filtering and vocabulary visibility, and a vocabulary table carrying presentation. Codes
are **scattered, not dense** (§3.4), so a visible code is not a lower bound on how many values a
principal cannot see.

> **⊘ Specified, not implemented.** *The streaming half:* there is no flush, so buffered rows have
> no row in `columns.arrow` and cannot be exercised on the read path; and
> `dictionary/terms-<k>.dict` has only ever had one extent, so a category value minted at ingest
> lives in the WAL overlay and its in-memory dictionary extension and folds at a flush that does not
> exist. *`inspect`:* §8.3's vector sidecar and §10.3's per-interaction row are one slot whose first
> occupant, the external-ID store, is explicitly transitional — `inspect` is declarable and
> **refused at parse** until that slot is filled. *Multi-valued attributes* (§3.7), likewise refused.

---

## 2. Declare the use, derive the placement

§10.3 places data by access ratio: per **rendered mark** to a fixed-width hot column; per **query**
to an entity-space bitmap behind §8.2's filter contract; per **interaction** to a cold sidecar.
Those three cadences are what a caller already wants to say — **render**, **filter**, **inspect** —
so an attribute is declared once with a `used_for` set and the placements follow. Entries are
orthogonal and additive: all three yields a hot column *and* a posting *and* a sidecar row.

This corrects a present flaw. Contracts §2.2's `declared_scalars` conflates "per-item column" with
"on the hot path", so there is no way to declare a field for drill-down only — which per §10.3 is
where most per-point metadata belongs. The current surface can express only the most expensive
placement.

### 2.1 Derive what is cheap to re-derive; declare what is baked into rows

Filter *structure* is derived from measured cardinality at build and re-derived at compaction — memo
§6.6 puts a level tree below ~10⁶ distinct values and BSI above, and a caller who picks it turns a
cardinality shift into an undiagnosable performance cliff. Report the choice; do not ask for it.

Hot column **width** is the opposite case. It is baked into every row, so changing it rewrites the
corpus — I9's side of the line.

### 2.2 What is alterable

| Change | Cost |
|---|---|
| Append a vocabulary value | Safe. New code assigned, never reusing a retired one |
| Retire a vocabulary value | Safe. Moves to `reserved`, never reassigned |
| Amend a value's properties | Safe, no rebuild — control-plane upsert (§5) |
| Add `inspect` or `filter` | Build pass; no row rewrite |
| Add `render` | Rewrites every segment |
| Change `width`, reuse a reserved code, change type | **Refused.** Rebuild |

### 2.3 Cost is reported, never hidden

Deriving placement is only safe if the consequence is visible, and §10.5 prices a hot column at
**0.93 GiB per byte per row per 10⁹ items** — a `u64` added speculatively is 7.45 GiB resident.

A plan step takes the schema plus corpus statistics and reports, per attribute: derived placement,
chosen structure, bytes per row, resident total, and alterability. Three things it must do that a
per-attribute table would not:

- **Total residency across attributes**, not per column. Several categories are what makes this
  bite.
- **Report §8.2 filter-surface growth.** Appendix C's exhaustiveness depends on that surface staying
  enumerable, and a config that widens it silently is the one way that property erodes without
  anyone deciding to erode it.
- **Warn on the combinations that break in practice** — `discovered` + `u8` (§3.6), dense codes
  under `listing = "per_viewer"` (§3.4), and `render_in` left at its default across
  non-overlapping slices (§3.9).

**It reports; it cannot refuse.** Appendix A is a *sizing* table — per-structure figures at 10⁷ and
10⁹, with no total, no ceiling and no machine-memory figure anywhere in the architecture. §10.3's
own instruction to "state that number against Appendix A's budget" points at something that does not
exist. Giving the plan step a refusal means giving Appendix A a deployment ceiling first, which is
an owner decision rather than a derivable number.

---

## 3. Categories

### 3.1 The object

| | Where | Supplied or derived |
|---|---|---|
| Value key | the data row | supplied, per point |
| Code | assigned at mint, scattered (§3.4) | derived, append-only, pinned |
| Membership posting | from the keys | **derived** — nothing to supply |
| Properties (label, colour, gate) | vocabulary table | supplied, per value |

The row carries **only the code**; the column's position says which attribute it is. A row with
`severity` (`u8`) and `department` (`u16`) is three bytes, and the strings appear nowhere in row
space.

**The key in the row is not the display name.** `sev_1` is a stable opaque key; "Critical" is a
property. Conflating them makes renaming for display a rewrite of every row.

### 3.2 Identity is `(column, key)`

`severity=high` and `priority=high` are different categories sharing a key. Interning the descriptor
from the key alone would give them one term and one posting, so a point with `priority=high` would
register as a member of `severity=high` — a wrong membership set, and since vocabulary visibility is
membership-derived, one that can reveal a value to a principal who can see no member of it. This is
memo §6.1's naming-collision hazard one level below where the memo was looking: inside the attribute
namespace rather than between namespaces.

Descriptors, tombstones and the `/v1/categories` response are all column-qualified. **The visibility
predicate is per-column**, and normatively so: two attributes sharing a vocabulary (§3.8) still have
distinct member sets, so a visible `reviewing_department = finance` must not reveal `finance` under
`owner_department`, where the principal may see none.

### 3.3 Visibility is derived, not maintained

A value is visible to a principal iff at least one of its members is: `∃ e ∈ members` visible. Exact
rather than conservative, and computed entirely from inside `M_auth` (I2).

**The alternative — a maintained union of members' term signatures — is rejected.** Such a union is
monotone under ingest and *non-monotone under deletion*: when the last point granting a term is
deleted or suppressed, the union must shrink or the value stays visible to a principal who can no
longer see any member. That needs a fourth retirement rule beside lifecycle §3's three, two of which
are themselves unbuilt. Derivation self-retires.

This is C11, already ruled closed — *"vocabulary containment-filtered against `M_auth`"* (§8.3).

**It is evaluated in entity space, against the composed verdict — not against the cached fragment,
and not by projecting into row space.** Three facts make that precise:

- A suppression never touches postings (lifecycle §3's second rule), so a fragment built from
  postings alone still contains a suppressed item. Filtering a vocabulary against it would keep
  listing a value whose only visible member has been suppressed.
- The *composed mask* is a row-space object: it exists to answer range cardinalities over row
  ranges, and reaching it costs a `Permutation::project` — O(cardinality), documented as seconds at
  10⁹. Vocabulary visibility needs no ranges, so it needs no projection.
- The deny precedence is already factored out as a single per-entity function — the one `compose`
  itself calls, and the one `visible_to` reads directly. Nothing here re-derives it, which matters
  because a second transcription of that precedence is how a suppression stops suppressing.

So the evaluation is two entity-space bitmaps built by walking the overlay through that same
function once per generation, and then per value:

```
visible(v) = !((members(v) ∩ fragment).andnot(overlay_fail)).is_empty()
          || !(members(v) ∩ overlay_pass).is_empty()
```

Every term is O(containers touched). Note the shape of the cost: a sparse principal's fragment has
few containers, so the intersection is *cheapest* exactly where the principal is least privileged —
the opposite of the asymmetry I7 warns about elsewhere. That is a prediction, not a measurement, and
§8's authorise arm exists to check it.

Any cache of the resulting visible-vocabulary set is keyed by
`(auth fingerprint, generation, overlay version)`. The overlay has no version counter today; an
overlay-blind cache row is the same fail-open by another route, and under continuous ingest such a
cache is cold on most requests anyway.

### 3.4 Codes are scattered, and pinned

**Two hazards, one mechanism.**

*Silent and corpus-wide.* The hot column stores the code, not the key. If a build re-derives codes
from a re-supplied vocabulary file — regenerated, re-sorted, hand-edited — every stored code
silently means something else and the whole corpus recolours, with no error and no digest mismatch.
**Codes are assigned once and never reused or reordered**: I9's discipline on a second dimension,
wanting an invariant with a test rather than a convention.

*A disclosure.* Dense first-seen codes make a visible code a lower bound on vocabulary cardinality,
and several visible codes invite the classic estimator over dense serial numbering. Structurally
this is I10's argument about entity-ID gaps applied to a second dense ID space — which is why the
system already has the answer.

**Codes are assigned at random from the unused space and recorded.** `tessera_id` solves the
identical problem with a keyed permutation because 10⁹ mappings cannot be stored; a vocabulary table
exists anyway, so random assignment gives the same property with no key and no new mechanism.
Density was never load-bearing: width is *declared* (§3.6), not derived from cardinality, so
scattering uses space already reserved.

**The scattered code is stored, not computed at gather** — contracts §2.6 r6's reason for storing
`tessera_id` at the row: the identity the service shows is stored at the row it is shown from.

`0` remains the reserved *absent* sentinel and is excluded from assignment; `reserved` tombstones
work identically; nothing sorts by category code; codes stay vocabulary-scoped, so §3.9's
cross-slice legends compose.

**Authoring.** Declared vocabularies pin their codes, following ClickHouse's `Enum8('low'=1,…)` — a
re-sorted file cannot recolour anything when the mapping is in a reviewed artifact. The system
scatters when it *mints*; an author writes what they like, and the plan step warns on dense codes
under `listing = "per_viewer"`. Retired keys move to `reserved` (Protobuf's mechanism and its
reasoning), so the tombstone is auditable in the schema rather than buried in the manifest — reusing
a key silently recolours history, on the argument slices §87 makes for slice names.

### 3.5 Two dictionaries, two postings files

A membership posting is an **attribute** term, which may only narrow `M_sel` (I12). A gate label
(§3.8) is an **auth** label evaluated by the plugin, which gates containment (I3). Memo §6.1
requires separate namespaces.

**A namespace tag inside the descriptor is insufficient, and one direction is an authorisation
bypass.** `DictWriter` is a single flat interner keyed on raw descriptor bytes, and descriptors are
arbitrary plugin-supplied bytes — so a tag lives in a space the caller also writes into. An
attribute descriptor that byte-equals a satisfied auth descriptor would union every item carrying
that category value into `M_auth`.

Separate files make the collision impossible rather than prevented, which is the house criterion for
a failure whose worst direction is an authorisation bypass. The cost is smaller than it sounds and
is not really a trade: the two files reuse the postings format verbatim — same CSR Arrow shape, same
tagged records, same reader — and the consumers were already disjoint, the fragment builder reading
auth postings and the filter path attribute postings. Separating them removes the need for either
reader to know the other exists.

Two real costs, stated rather than waved at: the per-partition manifest gains an attribute
counterpart to `dict_extents`, and `PostingsReader::open` validates every record, round-tripping
each Roaring payload — so a second file with a large vocabulary adds that many deserialisations to
the startup path.

### 3.6 Width

Declared, required, no default: `u8` (255 usable values), `u16`, `u32`. Code 0 is reserved for
**absent**, which preserves `columns.arrow`'s contractual non-nullability (R4) without a validity
buffer — the reader rejects any nullable column outright.

**`u8` buys residency, not speed**, against an 18 B row: a `u16` category is +11% and a `u8` +5.6%,
so with three categories at 10⁹ the difference is 2.79 GiB. *Whether the two are truly identical on
the gather path is **assumed**, not measured* — the argument is that a scattered gather costs one
cache line per point whatever the element width, but the gather is columnar, several category
columns are several independent streams, and the repo's own gather arm models cost per column *by
width*. §8 settles it.

The discriminator is not today's cardinality but whether the domain is **closed by nature**.
`severity: {low, medium, high, critical}` cannot reach 255; `department` starts at 20 and is 300
after two reorganisations. So `discovered` + `u8` is the combination that breaks in practice, and
the plan step warns on it by name.

Exhausting the code space is a typed error naming the column and its width — `422` at ingest, a
failure at build. Never widen, never wrap; the error states that the remedy is a rebuild.

**There is no clever third option**, recorded so it is not rediscovered: sub-byte bit-packing breaks
the aligned single-load that justified the hot path and saves nothing real since the cache line
dominates; Arrow dictionary encoding on the column is refused by the reader; RLE breaks random
access.

### 3.7 Single- and multi-valued

**Multi-valued is a slow-path shape**: admissible under `filter` and `inspect`, never under
`render`. A rendered mark has one colour, so declaring `render` on a multi-valued attribute is
refused at parse with that reason — and **no projection, derived value or summary of one earns a
hot column on its behalf either** (decision [0039](../decisions/0039-multi-valued-categoricals-are-slow-path-only.md)).
A caller who wants to colour by a value drawn from a multi-valued field declares an ordinary
single-valued attribute carrying that value: they say which single value they mean, in a column
that means exactly that, with no mechanism between the declaration and the row.

This is a rule about placement, not a claim about which encodings exist — the earlier phrasing said
multi-valued attributes "have no hot-path component", which reads as the latter and invites an
encoding to be offered against it. Several were, and 0039 records why each fails: slots past the
first have no reader; a per-row vocabulary bitset is 29.8 GiB at 10⁹ and ships bits at the
positions of invisible values; a dictionary of value combinations is refuted by measurement
(80,902 combinations on the arXiv corpus, open-ended under ingest). **The one that looks free is a
disclosure:** a "has more values" bit is computed over the full value set and baked into the row,
so a principal who knows their only visible value on a point and reads that bit has learned the
point carries at least one value they cannot see — §3.4's property, defeated in item space rather
than in vocabulary space.

⊘ **Multi-valued attributes are specified and not built.** They are memo §6.2's keyword case —
postings only, one per (item, value), no fixed width for a column — so deferring them costs no
format change. `multi = true` is refused at parse. The rule above is stated now so that lifting
that refusal for `filter` and `inspect` cannot quietly acquire a `render` path.

### 3.8 Listing, and where a gate comes from

Two independent axes:

- **`vocabulary = declared | discovered`** — operational. Does the caller know the value set up
  front? Governs whether an unknown key at ingest is a `422` or an auto-mint.
- **`listing = per_viewer | public`** — a disclosure control. Is the *existence* of a value
  sensitive? Governs whether `/v1/categories` is filtered per principal.

All four combinations are coherent except one, and the exception is a rule. **`listing = "public"`
requires `vocabulary = "declared"`:** a discovered vocabulary's values are inferred from whatever is
in the corpus, so publishing them discloses data-derived names on nobody's authority — C11 with no
accountable party. `declared` therefore means *the value set comes from an authored artifact* —
inline in the schema, or a vocabulary file bound at build — so a 400-value published vocabulary stays
practical. `declared` + `per_viewer` is the combination that matters: a known schema whose value
names are themselves sensitive.

**Where a `per_viewer` gate comes from.** Membership-derivation (§3.3) is the gate everywhere: a
value inherits its members' labels. A **declared** vocabulary may additionally carry an explicit gate
label per value in its vocabulary file — restricted to declared vocabularies because you cannot
author a gate for a value nobody declared. Two properties of that gate:

- **An explicit label replaces membership-derivation for that value**, rather than conjoining with
  it. That is the point of it — "visible to finance regardless of whether finance can yet see a
  member" — and what makes the empty-value case coherent. It follows that a caller can make a
  value's *name* more visible than any of its members: an explicit assertion of the same class as
  `public`, recorded in Appendix C as such.
- **Satisfaction is intersection** with the principal's satisfied term set, per slices §61 —
  *not* a conservative label join, which yields an empty required set for a disjunctive gate
  (`finance | legal`) and admits every principal.

A gate label requires the file form (§4.3): the inline `values` block pins codes only, and anything
else about a value is per-value data belonging with the value's other data.

**Properties are not separately gated.** A principal who can see a value gets all of its properties;
they are presentation metadata on a value whose visibility is already established.

**A filter naming an invisible value contributes an empty operand, never a `422`.** Refusing would
make the filter surface an existence oracle over exactly the vocabulary `per_viewer` hides — a
caller could enumerate hidden values by observing which keys are refused. This follows contracts
§3.2's unmatched-token precedent, and makes "no such value" and "a value you cannot see"
indistinguishable in outcome *and* in work.

**Counts are a different question.** A legend with counts is C8, not C11: every count is an
`and_cardinality` against `M_auth`, never precomputed. Colour-by-category leads there quickly, and
this design does not (§7).

**`/v1/categories` is a new endpoint, and it needs reconciling with `/v1/meta`**, which contracts
§3.2 already designates for the C11-gated label vocabulary. A large vocabulary is megabytes against
a measured 79 KB viewport response, and per-principal filtering means no shared cache — so it wants
pagination and a cap, neither of which `/v1/meta` has.

### 3.9 Shared vocabularies, slices, and the absence of a metadata table

**A vocabulary is a named object and a column references one** (`values_of`). Keys, codes and
properties are shared — the part humans maintain; postings, membership and therefore visibility stay
per-column, since points where `owner_department = eng` and where `reviewing_department = eng` are
different sets. **Attributes sharing a vocabulary must agree on `listing`, `vocabulary` and
`width`**, or the weaker setting governs both and the gated column's value set publishes through the
published one. Refused at parse.

**There is no metadata table.** "Table" implies entity-major and dense — a row per entity, a column
per attribute — which for two non-overlapping slices is a mostly-empty rectangle worth real
gigabytes at 10⁸ + 10⁸ entities. What is shared is attribute-major and sparse, which is what the
term index already is: vocabulary and postings in entity space, one posting per value; hot columns
in row space, dense only over a slice's own rows; cold `inspect` data per entity, absent for
entities carrying none. Slices §51's "shared metadata" therefore means *shared vocabulary and
postings*, not that every entity has every attribute.

**`render` is per-slice; `filter` and `inspect` are not.** They are entity-space, declared once,
applying everywhere. `render` is row-space, so an attribute declared for a document corpus would
otherwise materialise a column of 10⁸ `absent` codes in an unrelated sensor slice — slices §53
already permits per-slice columns. `render_in` names the slices carrying the column; omitted means
every slice, which is the expensive default and therefore one the plan step warns about (§2.3).

**Codes are shared across slices**, being vocabulary-scoped and entity-space, so the same code means
the same key in every slice that renders the attribute. A legend built for one slice is correct for
another and a client compositing two reconciles nothing. Server-side compositing stays out of scope
— coordinate systems, θ and tile ranges are per-slice, and contracts §3.2's viewport names one — but
nothing here forecloses it.

---

## 4. The configuration surface

### 4.1 A build input, not server config

The schema never appears in `tessera.toml`; the server reads the compiled schema from the bundle's
`MANIFEST.json`. A server reading a schema of its own could be restarted against a bundle whose
columns disagree, and the mismatch would surface as wrong codes rather than a startup error.

Config compiles to manifest, source to binary. **The capability model does not reach the manifest**,
which stays flat and per-placement — hot columns, filter operands, categories — because a reader
should never need to understand intent to know what to load. Corollary: **no environment-specific
paths in the schema**; file locations are CLI bindings (§4.3).

### 4.2 `schema.toml`

```toml
[[attribute]]
name       = "severity"
type       = "category"
width      = "u8"
used_for   = ["render", "filter"]
render_in  = ["docs_2024", "docs_2025"]
vocabulary = "declared"
listing    = "public"                 # legal: the value set is authored below

  [attribute.values]                  # codes pinned; dense is fine under `public`
  low = 1
  medium = 2
  high = 3
  critical = 4
  reserved = [5]

[[attribute]]
name       = "department"
type       = "category"
width      = "u16"
used_for   = ["render", "filter"]
vocabulary = "declared"
values_key = "departments"            # authored, bound at build
listing    = "per_viewer"

[[attribute]]
name       = "reviewing_department"
type       = "category"
width      = "u16"
used_for   = ["filter"]
vocabulary = "declared"
values_of  = "department"             # shared keys, codes and properties
listing    = "per_viewer"             # must match the referent (§3.9)

[[attribute]]
name       = "ingested_at"
type       = "timestamp_us"
used_for   = ["filter"]
```

```
tessera build --schema schema.toml \
              --points data/points.parquet \
              --values departments=data/departments.parquet
```

### 4.3 Required, defaulted, refused

SA §7's rule governs: *performance knobs default; disclosure controls do not*.

**Required for every attribute:** `name`, `type`, `used_for` (it sets the cost).

**Required for `type = "category"`:** `width` (an unsupported migration — §3.6); `listing` (a
disclosure control, so absence is a build error exactly as `[disclosure]`'s absence is a startup
error); `vocabulary` (it decides whether `public` is even legal, and a safe default there is a
decision nobody made).

**Defaulted:** `render_in` = every slice; `multi` = false.

**Refused at parse:** `render` with `multi = true`, and `multi = true` at all (⊘, §3.7); `render` on
a non-fixed-width type; `listing = "public"` with `vocabulary = "discovered"`; more than one of
`[attribute.values]`, `values_key`, `values_of`; code `0` in a `values` block, since it is the
*absent* sentinel and `low = 0` would make every value-less row a member of `low`; a code appearing
in both `reserved` and the live set; disagreement with a `values_of` referent on `listing`,
`vocabulary` or `width`; `inspect` (⊘, §1) and any other unimplemented `used_for` entry, each naming
itself per decision 0013.

### 4.4 Binding vocabulary files

The schema names a logical key and the CLI binds it to a path — this repo's own `--id-key-file`
pattern. Three rules, all fail-closed:

- `[attribute.values]` and `values_key` are two spellings of one thing, so declaring both is a parse
  error rather than a precedence question.
- A declared attribute must have exactly one of them (or a `values_of` reference), and its key must
  be bound at build. An unbound `values_key` is a build failure naming the attribute and the key —
  **never a silent fall-through to auto-mint**, which would convert a closed vocabulary to an open
  one without anyone deciding to.
- A `--values` binding naming a key no attribute declares is also an error, or the previous rule
  merely relocates the typo.

The file is `(key, code, …properties, gate?)`. `values_key` is permitted on a discovered vocabulary,
where it seeds keys and properties rather than closing the set — `vocabulary` is the key that says
which — and seeded-but-open is still open, so §3.8's rule still forbids `public`.

### 4.5 What is stolen, and from where

| Convention | Source |
|---|---|
| Per-field use flags | Elasticsearch mappings (`index`, `doc_values`, `store`) |
| *Mappings are immutable; you reindex* | Elasticsearch |
| Explicit codes in the declaration | ClickHouse `Enum8('low'=1,…)` |
| `reserved` for retired codes | Protobuf `reserved 3;` |
| Append-only value addition | Postgres `ALTER TYPE … ADD VALUE` |
| Logical key bound at invocation | this repo's own `--id-key-file` |

---

## 5. Ingest and build

**Batch build.** Points carry value keys as a column; vocabulary files are bound per attribute. Codes
for a declared vocabulary come from the schema or the file; for a discovered one they are minted by
random assignment (§3.4) and recorded in the manifest.

**Streaming ingest.** `/control/ingest`'s scalar-tail validation already exists and is strict — a
column the manifest does not declare, a declared column the batch omits, and a declared column at the
wrong type are each a `422` naming the column, with the tail built in declared order. It extends to
attributes unchanged, having only ever run against an empty declaration.

**Runtime vocabulary amendment** is a control-plane operation: properties are upserted without a
rebuild, so recolouring a legend never touches the build path.

**Declare-then-use.** An ingest row naming an undeclared value under `vocabulary = "declared"` is a
`422`, following slices §80 — *"no same-batch creation, no auto-create on first reference"* — for the
same reason: a category carries properties and, through its postings, a visibility consequence, so a
typo must not create one.

---

## 6. Amendments

- **contracts §2.2** — `declared_scalars` becomes the compiled per-placement attribute record.
- **contracts §2.4** — the attribute dictionary namespace and its postings file (§3.5), and the
  per-partition manifest's attribute `dict_extents` counterpart.
- **contracts §2.6** — attribute columns and their widths, per slice.
- **contracts §3.2** — `/v1/categories` and its relationship to `/v1/meta`'s vocabulary field; the
  empty-operand rule for filters naming invisible values.
- **contracts §3.4** — ingest carries attribute columns; declare-then-use; `/control/categories`.
- **architecture §5.3** — the hot-column list. **§8.2** — category filter operands.
- **architecture Appendix A** — attribute columns join the sizing tables; a residency *ceiling* is an
  owner decision (§2.3).
- **architecture Appendix C** — C8 and C11 annotated for category counts and vocabulary listing; a
  new entry for §3.8's explicit gate label making a value's name more visible than its members.

---

## 7. What this does not do

- **No aggregation.** Category counts are C8's existing shape; a breakdown surface is a separate
  design against §8.2.
- **No multi-valued attributes** (⊘, §3.7), no cold `inspect` sidecar (⊘, §1), no resolution of
  flush.
- **No server-side multi-slice composition** (§3.9).
- **No vocabulary cardinality limit.** §3.6's guidance is judgement, not measurement, and §8's
  authorise arm must run before any number becomes a documented limit.

---

## 8. Benchmarks

The fixtures carry no attribute tail today, so no arm can see any of this.

- **Fixture generator** gains an attribute-tail knob: width and type mix, declared and discovered.
- **Gather and viewport arms** split per-point cost into geometry and attribute tail, and report
  bytes touched per served point — the arm that settles §3.6's assumed claim, and it must vary the
  number of category columns, not only their width.
- **An authorise arm** for vocabulary-filter cost against vocabulary size, at cold and cached
  fingerprints, against overlay depth, and **against principal sparsity** — §3.3 predicts sparse
  principals are cheapest, and an arm that does not vary sparsity cannot check the prediction it
  most needs to.
- **Ingest arms**: the build's stage decomposition gains an attribute-column stage. The batch arm is
  fsync-dominated below ~1000 items (F3 remains **NOT confirmed by measurement**), so a sweep
  stopping short of that will report attribute cost as free and be wrong.

---

## Appendix R — review trail

**Reviewed 2026-08-02**, three lenses across successive drafts.

The findings that changed the design: a namespace tag inside the descriptor is insufficient, because
`DictWriter` interns caller-supplied bytes — hence separate dictionaries and postings files (§3.5);
dense codes on the wire are a cardinality lower bound, closed structurally by random assignment
rather than accepted into the register (§3.4); a filter naming an invisible value must contribute an
empty operand or become an existence oracle (§3.8); membership-derived visibility retires correctly
only against the composed verdict, since a suppression never touches postings (§3.3); and attributes
sharing a vocabulary could disagree on `listing` (§3.9).

Corrected against the corpus: Appendix A carries no residency ceiling, so the plan step reports
rather than refuses (§2.3); and the claim that category codes cost the same at any width is assumed
rather than measured, the repo's own gather arm modelling cost per column by width (§3.6).
