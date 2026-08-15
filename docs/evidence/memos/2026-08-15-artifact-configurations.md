# Eight artifact types: what the layer declares, and what one artifact carries

**Date:** 2026-08-15 (r3 — each type now carries its own artifact payload) · **Status:** Working
memo — evidence, not normative. **Illustrative, not a contract:** the wire and manifest shapes are
contracts work ([`contracts.md`](../../design/contracts.md)).
**Reads with:** [`annotations.md`](../../design/annotations.md),
[`annotation-representation.md`](../../design/annotation-representation.md),
[`annotation-write-cycle.md`](../../design/annotation-write-cycle.md), and decisions
[0074](../../decisions/0074-row-less-entities-are-allocated-downward.md)–[0081](../../decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md).

**Why write these.** The rulings of 2026-08-15 collapsed three gate modes to one flag, made the
visibility rule independent, and made an artifact whole-or-nothing. Each is defensible in prose. The
test is whether the artifact types a deployment actually wants are still *expressible* — first as a
declaration a caller could write without reading the design, then as a payload that has to hold real
data rather than describe it. **Seven things the prose had wrong fell out of writing them** (§10),
including two this memo's own earlier drafts put into the corpus — one into a design document and one
into a decision — before the owner caught them.

---

## 1. The two shapes

**The layer declaration** is TOML, one file, `[[layer]]` per layer, everything belonging to a layer
inside its own block, and keys grouped into inline tables only where one is meaningless without
another.

```toml
[[layer]]
name       = "<identity — tombstoned on drop, never reused>"
title      = "<human-readable, served as metadata>"
slices     = ["<slice>"]
membership = "enumerated" | "spatial" | "attribute"   # levels inherit it

access       = { label = "<layer gate>", artifacts_carry_own = true|false }
visible_when = { min_visible = <n> } | { min_fraction = <p> }   # omit: no such rule
hierarchy    = { kind = "flat"|"nested"|"stacked", prune_children = true|false }
content      = { derived = [...], supplied = [{ kind, corpus_derived }], on_member_deletion = … }
depends_on   = ["<layer this one's edges point into>"]

levels = [{ level = <k>, title = "…", zoom = [min, max] }]   # omit for a single-level layer
```

**The artifact payload** is a control-plane object — a layer is created by a verb with a WAL'd
registry entry, not by a file a build reads — so it is JSON, and the response is the artifact's
`tessera_id`. **Members are named by `external_id` or by `tessera_id`, resolved to entities at
admission exactly as a deny is** (write cycle §5). A caller never writes an entity ID: they do not
have one, and it is not theirs to know.

**Four things about the declaration are decisions rather than taste.** `access.label` gates the
layer while `access.artifacts_carry_own` says whether each artifact declares one besides — two halves
of one subject, composing as conjunction. The word is *label*, not *term*: a term is what a plugin
resolves a label into, and a caller never writes one. `visible_when` is its own key because it is
independent of everything else ([decision 0075](../../decisions/0075-the-masked-count-is-an-existence-criterion.md)),
and **its absence is the declaration that there is no such rule** — a disclosure control has no
default. Levels are declared in advance, because the zoom→level map is a property of the *set* and
the metadata endpoint publishes it. And levels carry explicit numbers, because edges reference
`(layer, level, ordinal)` and array position would silently renumber them.

---

## 2. A clustering — `clusters/hdbscan-2026-08`

```toml
[[layer]]
name       = "clusters/hdbscan-2026-08"
title      = "Topic clusters, August"
slices     = ["embedding-2026-08"]
membership = "enumerated"
access       = { artifacts_carry_own = false }   # a cluster exists because the data does
visible_when = { min_fraction = 0.05 }
hierarchy    = { kind = "nested", prune_children = false }
content      = { derived = ["centroid", "hull"] }
levels = [
  { level = 0, title = "Coarse" },
  { level = 1, title = "Mid" },
  { level = 2, title = "Fine" },
]
```

```json
{
  "layer": "clusters/hdbscan-2026-08",
  "level": 2,
  "stable_key": "c-vaccine-trials",
  "membership": { "external_ids": ["2401.00123", "2401.00456", "…31 000 of them"] },
  "edges": [
    { "layer": "clusters/hdbscan-2026-08", "level": 1, "stable_key": "c-immunology" }
  ]
}
```

**Almost the simplest artifact there is: an identity, a set, and one edge.** No content, no access
label, no variations. Its count, centroid and hull are recomputed per viewer from that membership and
are never carried here — an unmasked hull in a payload would be the build-time geometry the design
deleted, arriving through the write path instead of the file layout.

**`nested`, and the edge is the point.** HDBSCAN's condensed tree splits branches, so a child's
members are a **subset** of its parent's and the lineage is explicit — throwing it away would discard
something the algorithm computed. What is *not* true is that children **exhaust** a parent: points
fall out as noise at each split, 20–25% of them on this corpus, so a parent holds members no child
holds. Those two are different claims and merging them is the mistake an earlier draft of this memo
made, and put into [decision 0080](../../decisions/0080-the-frontier-is-a-per-artifact-test.md)
before the owner caught it.

**The stacked case is a different layer, not this one.** Three *independent* runs at three
`min_cluster_size` settings — which is what the measurement campaign produced — genuinely have no
lineage, and there a point that was noise in the coarse run can join a cluster in the fine one. That
is a real configuration and it declares `kind = "stacked"`, no edges, and levels that mean three
analyses rather than three depths.

**No zoom ranges, deliberately.** An embedding projection has no units, so no level corresponds to a
zoom; level choice is the client's. **`min_fraction`** because a fixed bar of fifty protects a
cluster of a hundred and does nothing for one of ten thousand.

## 3. Its labels — `topics/ctfidf-2026-08`

```toml
[[layer]]
name       = "topics/ctfidf-2026-08"
title      = "Topic labels, August"
slices     = ["embedding-2026-08"]
membership = "enumerated"
access     = { artifacts_carry_own = false }
hierarchy  = { kind = "flat" }
content    = { supplied = [{ kind = "label_text", corpus_derived = true }],
               on_member_deletion = "shrink_generating_set" }
depends_on = ["clusters/hdbscan-2026-08"]
```

```json
{
  "layer": "topics/ctfidf-2026-08",
  "level": 0,
  "stable_key": "c-immunology/label",
  "membership": { "external_ids": ["…the 240 sampled documents"] },
  "edges": [
    { "layer": "clusters/hdbscan-2026-08", "level": 1, "stable_key": "c-immunology" }
  ],
  "variations": [
    { "rank": 0,
      "access_label": "programme:VAC",
      "content": { "label_text": "vaccine immunology and trial design" },
      "generating_set": { "external_ids": ["…the 62 documents under VAC"] } },
    { "rank": 1,
      "content": { "label_text": "immunology" },
      "generating_set": { "external_ids": ["…the 8 documents everyone here can see"] } }
  ]
}
```

**Three structures, three jobs, and conflating any two is where the fail-opens were.** `membership`
decides the number beside it; each variation's `generating_set` decides whether *that description*
may be shown; a variation's `access_label` is a further condition on which one a viewer gets. A
viewer satisfying neither variation receives no label at all — which is why this layer declares no
`visible_when` and is still not unguarded. Its existence is governed by containment, and what makes
that visible in the config is `corpus_derived = true`, the field that says containment applies.

**The generating sets are not nested and need not be.** Rank 1's set is smaller and is not a subset
of rank 0's: the ranking is the caller's declared preference, not a containment hierarchy.

**`shrink_generating_set`** is load-bearing here and inert almost everywhere else — a toponymy label
is written from a sample, so one member leaving changes nothing it asserted. The register carries it
as the caller's declaration, with `withdraw_content` the default.

## 4. Administrative boundaries — `boundaries/uk-2026`

```toml
[[layer]]
name       = "boundaries/uk-2026"
title      = "UK administrative boundaries, 2026"
slices     = ["geographic"]
membership = "spatial"
access     = { artifacts_carry_own = true }      # public — a boundary exists whatever the corpus does
hierarchy  = { kind = "nested" }
content    = { derived = ["centroid"],
               supplied = [{ kind = "polygon", corpus_derived = false },
                           { kind = "name",    corpus_derived = false }] }
levels = [
  { level = 0, title = "LSOA", zoom = [12, 16] },
  { level = 1, title = "MSOA", zoom = [9, 12] },
  { level = 2, title = "LAD",  zoom = [6, 9] },
]
```

```json
{
  "layer": "boundaries/uk-2026",
  "level": 0,
  "stable_key": "E01000001",
  "access_label": "public",
  "membership": { "shape": { "type": "Polygon", "coordinates": [[["…"]]] } },
  "content": {
    "polygon": { "type": "Polygon", "coordinates": [[["…"]]] },
    "name": "City of London 001A"
  }
}
```

**No generating set anywhere, and declaring one would be refused.** The polygon and the name are
corpus-independent — true whether or not a single document exists — so containment is vacuous and
they serve unconditionally.

**The shape appears twice, for two different reasons**: once as the membership predicate, deciding
which points are in it, and once as content, being what is drawn. They are the same geometry here and
need not be — a boundary may be drawn simplified and counted exactly.

**No `visible_when`, and that is the point of this example.** A boundary's masked count is a count of
the viewer's own visible documents, which the density underlay already serves exactly at any depth. A
rule here would make a ward *absent* below the bar, and on a public, enumerable geography that is a
louder signal than any number it withholds. **Nothing coarsens a number**: a caller wanting coarser
answers gets them from the coarser *artifact* one level up, a different object with its own count.
And **spatial membership never goes stale** — a point ingested inside a ward is a member on the next
request, where the clustering above is stale until it is refreshed.

## 5. A tagged programme — `programmes/portfolio`

```toml
[[layer]]
name       = "programmes/portfolio"
title      = "Research programmes"
slices     = ["embedding-2026-08"]
membership = "attribute"                         # "the points carrying this programme tag"
access     = { artifacts_carry_own = true }      # satisfied by any authenticated principal
content    = { derived = ["hull", "centroid"],
               supplied = [{ kind = "name",   corpus_derived = false },
                           { kind = "extent", corpus_derived = false }] }
```

```json
{
  "layer": "programmes/portfolio",
  "level": 0,
  "stable_key": "prog-alpha",
  "access_label": "staff",
  "membership": { "attribute": { "column": "programme", "value": "alpha" } },
  "content": {
    "name": "Programme Alpha",
    "extent": { "type": "Polygon", "coordinates": [[["…"]]] }
  }
}
```

**The derived geometry is what makes this an artifact rather than a category**, and without it the
example is a category with extra steps. A category answers *which items carry this value*; it cannot
be drawn. An artifact answers *where, how many and what shape*, over the viewer's own visible
members — and a hull is content a vocabulary has nowhere to put. **The test before declaring one: if
nothing would render it, use a category with an authored gate**, which the register already accepts
(**C23**).

**Its membership is a query, not a list**, so it costs nothing to store and never goes stale: a
document tagged tomorrow is a member on the next request. The authored `extent` exists precisely
because the derived hull is unavailable to a viewer who can see none of its documents — who still
receives the name, the extent, and a count of **zero**.

## 6. Fitted circles — `clusters/kmeans-2026-08`

```toml
[[layer]]
name       = "clusters/kmeans-2026-08"
title      = "k-means, August"
slices     = ["embedding-2026-08"]
membership = "enumerated"
access       = { artifacts_carry_own = false }
visible_when = { min_visible = 50 }
hierarchy    = { kind = "flat" }
content      = { derived = ["centroid"],
                 supplied = [{ kind = "circle", corpus_derived = true }],
                 on_member_deletion = "withdraw_content" }
```

```json
{
  "layer": "clusters/kmeans-2026-08",
  "level": 0,
  "stable_key": "k-0142",
  "membership": { "external_ids": ["…4 100 of them"] },
  "variations": [
    { "rank": 0,
      "content": { "circle": { "centre": [0.412, 0.887], "radius": 0.0193 } },
      "generating_set": { "external_ids": ["…the same 4 100"] } },
    { "rank": 1, "content": {} }
  ]
}
```

**The trap layer: geometry that looks derived and is supplied.** A centre and radius fitted over full
membership describes members the viewer may not see, so it carries a generating set and gates by
containment exactly as a label does. Declaring it `corpus_derived = false` would serve it to every
principal — the caller assertion the service cannot check, and one of the two fields the register
watches.

**Rank 1 is the whole point.** An empty variation, satisfied vacuously by everyone, is how a caller
declares the degrade-to-derived behaviour the rulings withdrew as an automatic service: fail
containment on the circle and fall through to a variation with no supplied content, receiving
existence, a masked count and a recomputed centroid. **Omit rank 1 and the artifact vanishes
instead** — the same file expresses both policies, which is what it means for the behaviour to be
declared rather than assumed.

**Its generating set *is* its membership**, because the circle was fitted over all of it. That
coincidence is what makes this layer dangerous: the payload looks like §2's cluster until you notice
the geometry is carried rather than computed.

## 7. Analyst selections — `selections/analyst`

```toml
[[layer]]
name       = "selections/analyst"
title      = "Saved selections"
slices     = ["embedding-2026-08"]
membership = "enumerated"
access     = { artifacts_carry_own = true }      # the analyst's term, or their team's
content    = { derived = [] }
```

```json
{
  "layer": "selections/analyst",
  "level": 0,
  "access_label": "analyst:jrivera",
  "membership": { "tessera_ids": ["8f2a…", "b013…", "…ten of them"] }
}
```

**Named by `tessera_id` because that is what the analyst has** — they assembled this from things on
their screen, and the opaque identifier is the only one that ever crossed to them. No stable key: it
is not an edge target and nothing outside joins to it.

**No `visible_when`, and this is the clearest case for its absence.** The rule is a disclosure control
over *corpus* structure, and a hand-assembled selection's structure is the analyst's own — a bookmark
set of three would otherwise vanish. A set of ten shared with a colleague who cannot see three of its
members shows **seven**, which is the system working on the object most likely to cross a clearance
boundary.

## 8. A restricted analytic — `clusters/incident-2026-08`

```toml
[[layer]]
name       = "clusters/incident-2026-08"
title      = "Incident response — August clustering"
slices     = ["embedding-2026-08"]
membership = "enumerated"
access       = { label = "ir:analyst", artifacts_carry_own = true }
visible_when = { min_fraction = 0.05 }
hierarchy    = { kind = "nested", prune_children = true }
content      = { derived = ["centroid", "hull"] }
levels = [
  { level = 0, title = "Coarse", zoom = [0, 8] },
  { level = 1, title = "Fine",   zoom = [8, 16], min_fraction = 0.10 },
]
```

```json
{
  "layer": "clusters/incident-2026-08",
  "level": 1,
  "stable_key": "inc-4471-c07",
  "access_label": "ir:case-4471",
  "membership": { "external_ids": ["…2 300 of them"] }
}
```

**The only layer using all three controls, and the payload shows why they are three.** The layer's
`ir:analyst` decides whether the analysis is discoverable at all — a viewer without it finds the name
indistinguishable, in outcome and in work, from one that never existed. The artifact's
`ir:case-4471` is narrower and decides who may see *this* cluster within it. And `min_fraction`
decides whether a viewer who passes both can already see enough of it to be shown it. Discoverability
of the analysis, access to one artifact, and how much of it you hold: three questions, three fields.

## 9. Density cells — `density/morton`

```toml
[[layer]]
name       = "density/morton"
title      = "Density"
slices     = ["embedding-2026-08"]
membership = "spatial"
access     = { artifacts_carry_own = false }
hierarchy  = { kind = "nested" }
content    = { derived = [] }
levels     = [{ level = 8, zoom = [8, 8] }, { level = 10, zoom = [10, 10] }]
```

**No payload. This layer publishes nothing at all.** Its artifacts are the grid cells, implied by the
quantisation bounds the metadata already carries, and their membership is the points inside each,
resolved per request. **A layer whose artifacts are never ingested is a coherent thing**, and it is
the check that the abstraction is not secretly a table.

**No access label and no visibility rule — the fourth cell of the matrix**: existence discloses
nothing, since the grid is a function of published bounds, and the count is masked. Reaching today's
underlay through the general rules rather than as a special case is the check that the abstraction is
real. **Rebuilding the underlay on it is not proposed.**

---

## 10. What the exercise found

**A second wrong finding, caught by the owner, after it had reached a *decision*.** An earlier draft
declared the HDBSCAN layer `stacked`, on the reasoning that its hierarchy does not cover — and that
merged two different claims. HDBSCAN's condensed tree splits branches, so a child's members are a
**subset** of its parent's and the lineage is explicit; what is true is that children do not
**exhaust** a parent, because points fall out as noise at each split. Only the second is a property
of a condensed tree, and only the first would break a downward walk. The stacked case is three
*independent* runs, which is what the campaign produced and what the design generalised from.
[Decision 0080](../../decisions/0080-the-frontier-is-a-per-artifact-test.md) carried the merged claim
as its stated reason and is corrected in place; **its conclusion survives on the two grounds that do
not depend on monotonicity**, and whether the walk should return as a display policy for nested layers
is reopened there.

**A wrong finding, caught by the owner, after it had reached a design document.** An earlier draft
claimed density cells needed a fourth membership source — *computed* — because a cell's members are a
prefix of its row ids rather than a shape intersection. They do not: a cell **is** a shape, its
members are the points inside it, and it changes when a point is written, which is the spatial row
exactly. What is particular is only that an aligned cell resolves to one contiguous range and its
geometry is implied by its own identity, so nothing is stored. The row was added to the representation
and has been withdrawn. **Recorded because a plausible-looking taxonomy addition is exactly the kind
of thing that survives review by being uninteresting.**

**An example the model says should not exist.** An earlier draft used arXiv subject classes as an
artifact layer, which is the case the model explicitly sends to a category — the value belongs to the
corpus's vocabulary and thousands of items carry it. Replaced by the tagged programme, which earns
the attribute-predicate source properly, and only because it declares derived geometry: without
something to draw, it is a category with extra steps.

**A missing declaration that hid a security-consequential field.** An earlier schema had `derived` and
no `supplied`, so nothing in any config said whether an artifact's supplied content was
corpus-derived — the field that decides whether containment applies, and one of the two assertions
the register watches. Two separate confusions, about labels and about variations, were that one
omission wearing different clothes.

**Stable keys are effectively mandatory for any layer that is an edge target.** An edge names its
target, and at publish time the caller has no `tessera_id` for it — the service assigns them. So
either the caller retains every id returned from the target's publish (10⁷ of them for a clustering,
to attach labels afterwards) or the target layer carries stable keys and the edge names one.
[Decision 0081](../../decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md) made the
key optional by withdrawing the refusal that had forced it; **edges force it back for an unrelated
reason**, and the design says only that a key is *offered*. ⊘ Worth stating at the site: a layer named
in another layer's `depends_on` needs keys.

**Membership and content can be the same value without being the same thing.** A boundary's shape is
both its membership predicate and what is drawn; a fitted circle's generating set is its entire
membership. Both payloads repeat the value rather than offering a *same as membership* shorthand —
because the shorthand would make the dangerous case look like the safe one, and they differ precisely
in whether the geometry was computed from members the viewer may not see.

**Two collisions with the corpus's own vocabulary**, worth stating because config keys outlive memos.
**Item** means a *point* here — documents have entity IDs and the item-visibility predicate is about
them — so `item_*` keys for artifacts would collide in exactly the place people reason about
disclosure. And **attribute** is the column schema's word for a per-point field, so an artifact's
content cannot borrow it.

**One rename that would have misdirected an implementation.** `on_member_deletion` was nearly named
for deleting the artifact or shrinking its membership. It governs neither: membership shrinks
unconditionally when a point is deleted, the artifact is never deleted by a point event at all, and
the only thing declared is what becomes of **supplied content and its generating set**. A caller
wanting a curated set voided when a member goes is asking for a third mode, not a rename — and one
the register would need to look at, since it turns a point deletion into an artifact deletion.

**Nothing in any payload carries an unmasked derived quantity**, and that is a property to test for
rather than assume: no count, no hull, no bounding box, anywhere. Every one is recomputed per viewer,
and a payload field for any of them would be the build-time geometry the representation deleted,
arriving through the write path.

**And the standing question the schema keeps raising**: a layer is created by a control verb with a
WAL'd registry entry, not by a file a build reads. The primary declaration surface is the payload, and
the TOML is a convenience — for declaring layers at build time, and for checking a deployment's layers
into source control. That does not change the schema, and it does mean its ergonomics are not
load-bearing.

**Everything else expressed cleanly.** Eight types, four cells of the label-by-visibility matrix,
three membership sources, both content kinds, variations, edges, the layer gate, and one layer that
ingests nothing. No type needed a mechanism the rulings removed: the "fourth gate mode" the review
asked for turned out to be whole-or-nothing doing the work, degrade-to-derived survives as something
a caller declares, and rollup needed no configuration at all, being the coarser artifact passing its
own rule.

**What none of these exercise**, so what this memo does not test: edges at scale (only the label layer
declares any), the proportional rule on a predicate layer (the one combination still forbidden while
its denominator is undefined), and any layer past three levels.
