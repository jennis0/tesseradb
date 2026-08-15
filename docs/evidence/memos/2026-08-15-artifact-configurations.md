# What a layer declaration looks like, for eight artifact types

**Date:** 2026-08-15 (r2 — the schema settled with the owner, and three of the first draft's
examples were wrong) · **Status:** Working memo — evidence, not normative. **Illustrative, not a
contract:** the wire and manifest shapes are contracts work
([`contracts.md`](../../design/contracts.md)).
**Reads with:** [`annotations.md`](../../design/annotations.md),
[`annotation-representation.md`](../../design/annotation-representation.md),
[`annotation-write-cycle.md`](../../design/annotation-write-cycle.md), and decisions
[0074](../../decisions/0074-row-less-entities-are-allocated-downward.md)–[0081](../../decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md).

**Why write these.** The rulings of 2026-08-15 collapsed three gate modes to one flag, made the
visibility rule independent, and made an artifact whole-or-nothing. Each is defensible in prose. The
test is whether the artifact types a deployment actually wants are still *expressible*, and whether
the declaration is one a caller could write without reading the design. **Writing them found four
things the prose had wrong** (§10), including one this memo's own first draft got wrong and put into
a design document before the owner caught it.

---

## 1. The declaration

One file, `[[layer]]` per layer, and everything belonging to a layer inside its own block. Keys group
into inline tables only where one is meaningless without another.

```toml
[[layer]]
name       = "clusters/incident-2026-08"      # identity; tombstoned on drop, never reused
title      = "Incident response — August clustering"   # human-readable, served as metadata
slices     = ["embedding-2026-08"]
membership = "enumerated"                     # enumerated | spatial | attribute — levels inherit it

access       = { label = "ir:analyst", artifacts_carry_own = true }
visible_when = { min_fraction = 0.05 }        # omit the key and there is no such rule
hierarchy    = { kind = "nested", prune_children = true }
content      = { derived = ["centroid", "hull"],
                 supplied = [],
                 on_member_deletion = "withdraw_content" }
depends_on   = ["clusters/hdbscan-2026-08"]   # layers this one's edges point into

levels = [                                    # declared up front; omit for a single-level layer
  { level = 0, title = "Coarse", zoom = [0, 8] },
  { level = 1, title = "Fine",   zoom = [8, 16], min_fraction = 0.10 },
]
```

**Four things about this shape are decisions rather than taste.**

**`access.label` gates the layer; `access.artifacts_carry_own` says whether each artifact declares
one besides.** These are the two halves of one subject and they compose as conjunction, never
disjunction. The word is *label*, not *term*: a term is what a plugin resolves a label into, and a
caller never writes one.

**`visible_when` is its own key because it is independent of everything else**
([decision 0075](../../decisions/0075-the-masked-count-is-an-existence-criterion.md)). Burying it in
`access` would re-assert the coupling that ruling removed. It takes `min_visible` (absolute) or
`min_fraction` (proportional, and the one that scales), never both, and **its absence is the
declaration that there is no such rule** — a disclosure control has no default.

**Levels are declared in advance and carry explicit numbers.** In advance, because the zoom→level
map is a property of the *set* — you cannot say which level is drawn at which scale without knowing
what sits either side — and the metadata endpoint publishes it, so a level appearing at publish time
would change the map under a client that had already fetched it. Explicitly numbered, because edges
reference `(layer, level, ordinal)`: if array position defined the index, reordering the file would
silently renumber levels and repoint every stored edge.

**A level names only what differs from its layer.** Membership source and the visibility rule sit on
the layer; a level typically carries a title and a zoom range. A layer that declares no `levels` has
exactly one — which is most of the examples below.

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
hierarchy    = { kind = "stacked" }
content      = { derived = ["centroid", "hull"] }
levels = [
  { level = 0, title = "min_cluster_size 6000" },
  { level = 1, title = "min_cluster_size 600" },
  { level = 2, title = "min_cluster_size 60" },
]
```

**No zoom ranges, deliberately.** An embedding projection has no units, so no level corresponds to a
zoom and there is nothing honest to declare; level choice is the client's.

**`stacked` is what the measurement campaign actually produced** — three independent runs whose
nesting nobody verified — and declaring it honestly is what stops rollup and the frontier being
claimed for a structure that does not have them.

**`min_fraction` over `min_visible`.** A fixed bar of fifty protects a cluster of a hundred and does
nothing for one of ten thousand, where fifty visible members is half a percent.

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

**No `visible_when`, and the reason is not that labels are unguarded.** A label's existence is
governed by containment: its text is corpus-derived, so a viewer who satisfies no variation's
generating set sees no label at all
([decision 0076](../../decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)). That is not
configurable and nothing declares it — what makes it *visible in the config* is
`corpus_derived = true`, which is the field that says containment applies. The first draft of this
memo claimed `criterion = none` expressed it; it does not, and the owner was right to say so.

**`shrink_generating_set` is load-bearing here and inert almost everywhere else.** A toponymy label
is written from a sample, so one member leaving changes nothing it asserted. The register carries it
as the caller's declaration, with `withdraw_content` the default.

Per artifact, as data: membership is the sample; ranked variations each with their own generating
set and gate; one edge into the cluster named.

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

**No `visible_when`, and that is the point of this example.** A boundary's masked count is a count of
the viewer's own visible documents — the quantity the density underlay already serves exactly at any
depth. A rule here would make a ward *absent* below the bar, which on a public, enumerable geography
is a louder signal than any number it withholds. **Nothing coarsens a number**: a caller wanting
coarser answers gets them from the coarser *artifact* one level up, which is a different object with
its own count.

**`membership = "spatial"` never goes stale.** A point ingested inside a ward is a member on the next
request, where the clustering above is stale until it is refreshed.

## 5. A tagged programme — `programmes/portfolio`

```toml
[[layer]]
name       = "programmes/portfolio"
title      = "Research programmes"
slices     = ["embedding-2026-08"]
membership = "attribute"                         # "the points carrying this programme tag"
access     = { artifacts_carry_own = true }      # satisfied by any authenticated principal
content    = { derived = [],
               supplied = [{ kind = "name",   corpus_derived = false },
                           { kind = "extent", corpus_derived = false }] }
```

**This is what attribute-predicate membership is for**, and it replaces the subject-taxonomy example
the first draft used. Reaching for an artifact where the value belongs to the corpus's own vocabulary
and thousands of items carry it is the case the model says should be a **category**; a programme is
the other side of that test — the set needs a name, an owner and its own access, and its members are
whatever carries the tag.

**The count may be zero**, and that is the sharpest illustration of the rule: staff must know a
programme exists over documents they cannot read, and answering the count from anything but their own
masked membership would be a plain I2 breach. The authored extent exists precisely because the
derived one is unavailable.

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

**The trap layer: geometry that looks derived and is supplied.** A centre and radius fitted over full
membership describes members the viewer may not see, so it carries a generating set and gates by
containment exactly as a label does. **Declaring it `corpus_derived = false` would serve it to every
principal** — the caller assertion the service cannot check, and one of the two fields the leak
register watches.

Each artifact carries **two variations**: rank 0 the circle, rank 1 carrying no supplied content at
all. That second one is how a caller gets the degrade-to-derived behaviour decision 0076 withdrew as
an automatic service — a viewer failing containment falls through and receives existence, a masked
count and a recomputed centroid, never the circle. **Declared, not automatic**: omit rank 1 and the
artifact vanishes instead. Variations are per-artifact data; what the layer declares is the *kinds*.

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

**No `visible_when`, and this is the clearest case for its absence.** The rule is a disclosure
control over *corpus* structure; a hand-assembled selection's structure is the analyst's own, and a
bookmark set of three would otherwise vanish. A set of ten shared with a colleague who cannot see
three of its members shows **seven** — the system working, on the object most likely to cross a
clearance boundary.

## 8. A restricted analytic — `clusters/incident-2026-08`

The §1 example, and the one layer that uses all three controls at once. A viewer without `ir:analyst`
finds the name indistinguishable — in outcome and in work — from one that never existed; an analyst
sees the layer, and each cluster still answers to its own label and to the visibility rule. **Three
controls, three jobs:** discoverability of the analysis, access to one artifact within it, and
whether you can already see enough of that artifact for it to be shown.

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

**No access label and no visibility rule — the fourth cell of the matrix**, an artifact whose
existence discloses nothing (the grid is a function of published quantisation bounds) and whose count
is masked. Reaching today's underlay through the general rules rather than as a special case is the
check that the abstraction is real. **Rebuilding the underlay on it is not proposed.**

---

## 10. What the exercise found

**A wrong finding, caught by the owner, after it had reached a design document.** The first draft
claimed density cells needed a fourth membership source — *computed* — because a cell's members are a
prefix of its row ids rather than a shape intersection. They do not: a cell **is** a shape, its
members are the points inside it, and it changes when a point is written, which is the spatial row
exactly. What is particular is only that an aligned cell resolves to one contiguous range and its
geometry is implied by its own identity, so nothing is stored — a property of that shape, not a kind
of membership. The row was added to the representation and has been withdrawn. **Recorded because
the exercise produced it, and because a plausible-looking taxonomy addition is the kind of thing that
survives review by being uninteresting.**

**An example that the model says should not exist.** The first draft used arXiv subject classes as an
artifact layer. The model's own test is to reach for a category when the value belongs to the
corpus's vocabulary and many items carry it, and for an artifact when the set needs identity, its own
access, or content — so that example was the case the design explicitly rules the other way. Replaced
by the tagged programme, which earns the attribute-predicate source properly.

**A missing declaration that hid a security-consequential field.** The first draft's schema had
`derived` and no `supplied`, so nothing in any config said whether an artifact's supplied content was
corpus-derived — which is what decides whether containment applies, and one of the two assertions the
register watches. Two of the draft's confusions, about labels and about variations, were both this
omission wearing different clothes.

**Two collisions with the corpus's own vocabulary**, worth stating because config keys outlive
memos. **Item** means a *point* here — documents have entity IDs and the item-visibility predicate is
about them — so `item_*` keys for artifacts would collide in exactly the place people reason about
disclosure. And **attribute** is the column schema's word for a per-point field, so an artifact's
content cannot borrow it. The settled keys use neither.

**One rename that would have misdirected an implementation.** `on_member_deletion` was nearly named
for deleting the artifact or shrinking its membership. It governs neither: membership shrinks
unconditionally when a point is deleted, the artifact is never deleted by a point event at all, and
the only thing declared is what becomes of **supplied content and its generating set**. A caller who
wants a curated set voided when a member goes is asking for a third mode, not a rename — and one the
register would need to look at, since it turns a point deletion into an artifact deletion.

**And the standing question the schema keeps raising**: a layer is created by a control verb with a
WAL'd registry entry, not by a file that a build reads. So the primary declaration surface is a
control-plane payload and this file form is a convenience — for declaring layers at build time, and
for checking a deployment's layers into source control. That does not change the schema, and it does
mean its ergonomics are not load-bearing.

**Everything else expressed cleanly.** Eight types, four cells of the label-by-visibility matrix,
three membership sources, both content kinds, variations, edges, the layer gate. No type needed a
mechanism the rulings removed; the "fourth gate mode" the review asked for turned out to be
whole-or-nothing doing the work; degrade-to-derived survives as something a caller declares; and
rollup needed no configuration at all, being the coarser artifact passing its own rule.

**What none of these exercise**, so what this memo does not test: edges at scale (only the label layer
declares any), the proportional rule on a predicate layer (the one combination still forbidden while
its denominator is undefined), and any layer past three levels.
