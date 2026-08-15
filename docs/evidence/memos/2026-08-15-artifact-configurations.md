# What a layer declaration looks like, for nine artifact types

**Date:** 2026-08-15 · **Status:** Working memo — evidence, not normative. **Illustrative, not a
contract:** the wire and manifest shapes are contracts work
([`contracts.md`](../../design/contracts.md)), and nothing here fixes a key name.
**Reads with:** [`annotations.md`](../../design/annotations.md) (the model),
[`annotation-representation.md`](../../design/annotation-representation.md),
[`annotation-write-cycle.md`](../../design/annotation-write-cycle.md), and decisions
[0074](../../decisions/0074-row-less-entities-are-allocated-downward.md)–[0081](../../decisions/0081-a-replacement-mints-identities-an-edit-keeps-them.md).

**Why write these.** The rulings of 2026-08-15 collapsed three gate modes to one flag, made the
existence criterion independent, and made an artifact whole-or-nothing. Each of those is defensible
in prose. The test is whether the common artifact types are still *expressible* afterwards — and
whether the declaration that expresses them is one a caller could write without reading the design.
Nine types are declared below. **Two gaps fell out of the exercise and are in §11.**

---

## 1. The declaration, in one place

Everything a layer declares, with what it means and which ruling put it there.

```toml
[layer."<name>"]
gate           = ["<term>", …]   # may a viewer know this analysis exists? Resolved at authorise,
                                 #   version-keyed. Absent = every principal may know.
own_terms      = true | false    # do its artifacts carry their own access terms? (0079)
criterion      = …               # the existence criterion (0075). No default — see below.
hierarchy      = "flat" | "nested" | "stacked"
pruning        = true | false    # display policy only; a level may raise, never lower (0080)
slices         = ["<slice>", …]
derived        = ["centroid", "hull", …]   # count is intrinsic and never declared
deletion       = "strict" | "permissive"   # only where supplied corpus-derived content exists
edges_into     = ["<layer>", …]  # declared so a replacement that would dangle them is refused

[layer."<name>".levels.<k>]
membership     = "enumerated" | "spatial" | "attribute" | "computed"
zoom           = [min, max]      # advisory
criterion      = …               # optional override, raising only
```

**The criterion takes three forms and has no default**, because it is a disclosure control and this
corpus makes a config file double as a disclosure-review checklist:

```toml
criterion = { none = true }             # every artifact is served; its count is its own masked count
criterion = { min_visible = 50 }        # absolute: 50 visible members
criterion = { min_fraction = 0.10 }     # proportional: a tenth of declared membership
```

**Content is data, not configuration.** An artifact carries its membership, its own terms if the
flag is set, its variations — each a `(content, gate, rank)` triple with its own generating set —
and its edges. The layer declares only what is true of all of them.

---

## 2. A clustering — `clusters/hdbscan-2026-08`

The baseline: HDBSCAN over an embedding, three cuts published together.

```toml
[layer."clusters/hdbscan-2026-08"]
own_terms  = false                        # a cluster exists because the data does
criterion  = { min_fraction = 0.05 }      # scales with the cluster, where a fixed bar does not
hierarchy  = "stacked"                    # three independent runs; nesting was never verified
pruning    = false                        # nothing to prune across independent levels
slices     = ["embedding-2026-08"]
derived    = ["centroid", "hull"]

[layer."clusters/hdbscan-2026-08".levels.0]
membership = "enumerated"
zoom       = [0, 6]
```
*(levels 1 and 2 the same, at `zoom = [6, 10]` and `[10, 16]`.)*

**The proportional form is the interesting choice.** A fixed bar of fifty protects a cluster of a
hundred and does nothing for one of ten thousand, where fifty visible members is half a percent.
`min_fraction` is the same statement at every scale.

**`stacked` is what the campaign's Tier A actually produced** — three runs at three
`min_cluster_size` settings, whose nesting nobody verified — and declaring it honestly is what stops
`reach`, rollup and the frontier being claimed for a structure that does not have them.

## 3. Its labels — `topics/ctfidf-2026-08`

```toml
[layer."topics/ctfidf-2026-08"]
own_terms  = false
criterion  = { none = true }              # existence follows containment — see below
hierarchy  = "flat"
slices     = ["embedding-2026-08"]
derived    = []                           # the count is intrinsic; nothing else is wanted
deletion   = "permissive"                 # a statistical sample; one document leaving changes nothing
edges_into = ["clusters/hdbscan-2026-08"]
```

**A label declares no criterion, and that is not a hole.** Its existence is decided by containment:
an artifact is served whole or not at all (0076), so a viewer who satisfies no variation's generating
set sees no label. The "fourth gate mode" the review asked for turns out to need no mode — it falls
out of whole-or-nothing.

**`permissive` is load-bearing here and inert almost everywhere else.** A toponymy label is written
from a sample; one member's deletion changes nothing it asserted, so the fold removes the member and
the label serves on. The register carries that as the caller's declaration, strict by default.

An artifact in this layer, as data: membership is the sample; three variations ranked by clearance,
each with its own generating set; one edge into the cluster it names.

## 4. Administrative boundaries — `boundaries/uk-2026`

```toml
[layer."boundaries/uk-2026"]
own_terms  = true                         # a boundary exists whether or not the corpus does
criterion  = { none = true }              # every ward is drawn, with its own exact masked count
hierarchy  = "nested"
pruning    = false
slices     = ["geographic"]
derived    = ["centroid"]

[layer."boundaries/uk-2026".levels.0]     # LSOA
membership = "spatial"
zoom       = [12, 16]
```
*(MSOA at `[9, 12]`, LAD at `[6, 9]`.)*

**`criterion = none` is the point of this example.** A boundary's masked count is a count of the
viewer's own visible documents — the quantity the density underlay already serves exactly at any
depth. Declaring a criterion here would make a ward *absent* below the bar, which on a public,
enumerable geography is a louder signal than the number it withholds. Nothing coarsens a number:
where a caller wants coarser answers they get them from the coarser **artifact**, a level up, which
is a different object with its own count.

**`membership = "spatial"` never goes stale.** A point ingested inside a ward is a member on the next
request, with nothing rebuilt — unlike the clustering above, which is stale until it is refreshed.
A deployment carrying both should expect them to age differently.

## 5. A subject taxonomy — `taxonomy/arxiv-2026`

```toml
[layer."taxonomy/arxiv-2026"]
own_terms  = true
criterion  = { none = true }
hierarchy  = "nested"
slices     = ["embedding-2026-08"]
derived    = ["centroid"]

[layer."taxonomy/arxiv-2026".levels.0]    # archive: math, cs, …
membership = "attribute"                  # the existing category column; no new storage
zoom       = [0, 8]
```

**Costs nothing to store.** Attribute-predicate membership is the value column and postings that
already exist; the masked count is the filter machinery answering a question it can already answer.

## 6. Fitted circles — `clusters/kmeans-2026-08`

The trap layer: geometry that looks derived and is supplied.

```toml
[layer."clusters/kmeans-2026-08"]
own_terms  = false
criterion  = { min_visible = 50 }
hierarchy  = "flat"
slices     = ["embedding-2026-08"]
derived    = ["centroid"]                 # recomputed from masked members
deletion   = "strict"
```

Each artifact carries **two variations**: rank 0 is the fitted centre and radius, declared
`corpus_derived` with the fitted membership as its generating set; rank 1 carries no supplied
content at all.

**That second variation is how a caller gets the old degrade-to-derived behaviour**, which 0076
withdrew as an automatic service. A viewer failing containment on the circle falls through to rank 1
and receives existence, a masked count and a recomputed centroid — never the circle. The difference
from the withdrawn behaviour is that it is **declared**: a caller who wants the artifact to vanish
instead simply omits rank 1.

**Declaring the circle `corpus_independent` would serve it to everyone.** That is the C12-class
assertion the service cannot check, and it is why the field is one of the two the register watches.

## 7. Analyst selections — `selections/analyst`

```toml
[layer."selections/analyst"]
own_terms  = true                         # gated on the analyst's term, or their team's
criterion  = { none = true }              # a bookmark set of three must not be suppressed
hierarchy  = "flat"
slices     = ["embedding-2026-08"]
derived    = []
```

**`criterion = none` is the whole point.** The criterion is a disclosure control over *corpus*
structure; a hand-assembled selection's structure is the analyst's own, and a set of three would
otherwise vanish. A set of ten shared with a colleague who cannot see three of its members shows
**seven** — the system working, on the object most likely to cross a clearance boundary.

## 8. A programme portfolio — `programmes/portfolio`

```toml
[layer."programmes/portfolio"]
own_terms  = true                         # satisfied by any authenticated principal
criterion  = { none = true }
hierarchy  = "flat"
slices     = ["embedding-2026-08"]
derived    = []
```

Each artifact: an authored name and an authored extent, both `corpus_independent`, so an empty
generating set and unconditional service. **The count may be zero** — staff must know a programme
exists over documents they cannot read, and answering the count from anything but their own masked
membership would be a straightforward I2 breach.

## 9. A restricted analytic — `clusters/incident-2026-08`

```toml
[layer."clusters/incident-2026-08"]
gate       = ["ir:analyst"]               # the layer is unreachable without it
own_terms  = true                         # and each artifact carries its own besides
criterion  = { min_visible = 20 }
hierarchy  = "flat"
slices     = ["embedding-2026-08"]
derived    = ["centroid", "hull"]
deletion   = "strict"
```

**Three controls, three different jobs**, and this is the layer that shows they are independent: the
layer `gate` decides whether the analysis is discoverable at all, `own_terms` decides who may see a
given cluster within it, and `criterion` decides whether a cluster is dense enough in your own view
to be shown. A viewer without `ir:analyst` finds the name indistinguishable — in outcome and in work
— from one that never existed.

## 10. Density cells — `density/morton`

The recast of the existing underlay, written as a layer to test that the abstraction reaches it.

```toml
[layer."density/morton"]
own_terms  = false
criterion  = { none = true }
hierarchy  = "nested"                     # cells strictly nest, level by level
slices     = ["embedding-2026-08"]
derived    = []

[layer."density/morton".levels.8]
membership = "computed"                   # the cell is a prefix of the row id; nothing is stored
zoom       = [8, 8]
```

**The fourth cell of the matrix**: no own terms, no criterion — an artifact whose existence discloses
nothing (the grid is a function of published quantisation bounds) and whose count is masked. Reaching
today's underlay through the general rules, rather than as a special case, is the check that the
abstraction is real. **Rebuilding the underlay on it is not proposed**; the value is that the next
thing to arrive has somewhere to go.

---

## 11. What the exercise found

**The membership sources are four, not three.** The model enumerates *enumerated*, *spatial
predicate* and *attribute predicate*. The density layer needs a fourth — **computed**, where
membership is a function of the row id itself and nothing is stored or evaluated per request. It is
not a predicate: there is no shape to intersect and no column to scan, and its masked count is a
`range_cardinality` over a contiguous range, the cheapest operation in the system. The
representation describes this mechanism at §10 and does not carry it in the §2.0 table that
enumerates sources. ⊘ **A one-row correction, owed.**

**`deletion` is inert on six of the nine layers.** It governs what happens to supplied
corpus-derived content when a member is deleted, and six of these declare no such content — a
boundary's polygon, a programme's name and a taxonomy's label are all corpus-*independent*, and a
clustering's geometry is derived. Under [decision 0045](../../decisions/0045-inert-config-keys-are-deleted.md)
an inert key is deleted rather than kept parsed, which here means: **the declaration should be
refused where the layer declares no corpus-derived supplied content**, rather than silently ignored.
⊘ **A parse rule, owed.**

**Everything else expressed cleanly**, which is the result worth recording. Nine types, four cells of
the flag-by-criterion matrix, three of the four membership sources, both content kinds, variations,
edges and the layer gate — and no type needed a mechanism the rulings removed. In particular:

- the "fourth gate mode" the review asked for is not needed: a label declares no criterion and its
  existence follows containment, which is whole-or-nothing doing the work;
- degrade-to-derived survives as something a caller **declares** — a last-ranked variation with no
  supplied content — rather than something the service does;
- rollup needed no configuration at all: it is the coarser artifact passing its own criterion.

**What no layer here exercises**, and so what this memo does not test: edges between artifacts *at
scale* (only the label layer declares any), the proportional criterion on a predicate layer (the one
combination §5's ⊘ gap forbids until it is ruled), and any layer with more than three levels.
