# Derived artifacts — one class, three gates, two axes

**Date:** 2026-08-01
**Status:** design, for review. **Candidate to graduate into the `docs/design/` design corpus once C1's owner review validates the taxonomy** — it generalises two sections of the specification, and putting an unreviewed framework where the specification defers to it would be the wrong order.
**Split from** `2026-07-31-client-interaction-architecture-design.md` §9.1, which had grown to a quarter of that document while answering a question that is not a client question.
**Touches:** design §7.5, §7.6, §8.4, §12.3, Appendix C (C1, C2, C3, C11), Appendix D, Appendix H; contracts §3.2; plan §7 (Phase 3).

---

## 1. Why this exists

The design specifies how **points** are masked, counted and sampled. It also serves things
that are not points — cluster nodes, labels, hulls, aggregation cells — and it gates two of
them by **different rules**, in two sections, with nothing connecting them:

- **Labels** (§7.6) gate on **exact containment**: served iff `and_cardinality(G, M_auth) == |G|`.
- **Cluster nodes** (§7.5) gate on a **threshold**: descend while masked membership exceeds
  `min_visible_members`, with C1 as the accepted residual.
- **Node extent and hull** (C2) are gated by nothing at all — recomputed per viewer from
  masked membership.

Three rules, no framework. A fourth artifact type will go looking for precedent and find
contradictory answers. This document states the rule the three instances are instances
*of*, so that the next one is decided rather than improvised — and records two findings
that fall out of stating it, one of which is a live security question.

**Altitude:** mechanisms and rules, not contracts. Where a wire shape is implied it is
named, not specified.

## 2. What the class is

**A cluster has no inherent geometry.** Each point is a member of some cluster set, and the
node's shape is *derived* from that membership. Generalising, every member of this class is
**a named subset of the point set, plus an attachment**:

| Artifact | Subset | Attachment |
|---|---|---|
| Cluster node | membership bitmap | geometry, **derived** per viewer |
| Label | generating set | content, **derived** from the corpus |
| Aggregation cell | a Morton range | none — the range *is* the subset |
| Boundary polygon | the points inside it, resolved at build | geometry, **supplied**, corpus-independent |

The engine already holds this shape. §7.5 stores a membership bitmap per node plus a
bounding box per slice for pruning, and recomputes geometry from masked membership. **So
the class needs no new mechanism — only a gate.**

**The exception, and it matters.** A cluster cannot exist without members; a boundary can.
A polygon is therefore not always a subset-with-attachment — §5 separates its three forms.

## 3. The three gates

**A derived artifact is gated by whether its attachment is derived from the corpus, and by
what that attachment discloses.**

| Attachment | Gate | Instance | Literature |
|---|---|---|---|
| Corpus-derived, **content**-bearing | **exact containment** — serve iff `G ⊆ M_auth` | labels (§7.6), annotations, region summaries | the derivation axiom, multilevel-secure databases (Appendix D) |
| Corpus-derived, **structure**-revealing | **threshold**, as a named disclosure control | cluster nodes, `min_visible_members` (§7.5) | **small-cell suppression**, statistical disclosure control (§6) |
| Corpus-derived, **recomputed per viewer** | **none** | hulls, centroids, contours (C2) | not a shared artifact, so nothing to gate |
| **Not corpus-derived** | none on the attachment; the induced count is masked as usual | boundary shapes, basemap outlines | reference data |

**Content-bearing artifacts need containment** because their content can reveal what is
inside documents the viewer cannot read. They inherit §7.6's availability pathology with
it: generating sets are immutable (**I8**), so a single deletion breaks containment for
*every* principal, and a nested chain dark-ships together.

**Structure-revealing artifacts** disclose only existence and shape, so a threshold is
defensible — but only as the security control §7.5 already insists it be named and reviewed
as, never as a display preference.

**Per-viewer-recomputed artifacts are safe by construction**, and the failure to avoid is
**serving a generated artifact instead of recomputing it**. A build-time centroid over full
membership, shown to a viewer who sees 5% of it, points straight at where the invisible
members are. C2 records that node extent and hull are recomputed and says it is "listed to
record that it was checked"; this tier is that check, generalised.

**Not-corpus-derived attachments need no gate on the attachment.** A city boundary exists
whether or not the corpus does; its shape discloses nothing. Only the count within it is a
masked quantity.

## 4. The second axis: cardinality sets the mechanism

**Disclosure chooses the gate. Cardinality chooses how it is evaluated**, and the two are
independent.

**Artifact-scale (≈10⁵).** Cluster nodes, labels, and boundary polygons — the last because
boundaries are *"city, town, postcode level, rather than per point"* (owner, 2026-08-01).
At this size a per-item test against the mask is affordable, and membership bitmaps are the
right representation. Everything in §3 assumes this scale.

**Point-scale or larger (10⁹–10¹⁰).** **Edges are the sole member, and they break the
mechanism while keeping the rule.** A sparse graph over 10⁹ points carries 10⁹–10¹⁰ edges,
so a per-item containment test is impossible however correct it is. Edges need the *points'*
machinery — an ordering, contiguous ranges, bitmap arithmetic, a priority prefix. The
structural form of their gate: with edges sorted by `(source, target)`, the visible set is
the adjacency runs of visible sources intersected with visible targets — O(visible edges),
bounded by the mask rather than by the corpus.

**The lesson to carry forward:** getting the gate right does not mean the artifact is
handled. A correct rule at an impossible cardinality is not a design.

## 5. Polygons, in three forms

Three distinct things hide under one word, and only the first belongs to this class.

**A geometry that *induces* a subset.** A postcode exists whether or not any document falls
in it. It is not a subset with an attachment; it is a shape whose relation to the point set
is derived and may be **empty**. It joins the class through its induced count, not through
its existence, and its shape falls in §3's fourth row.

**A geometry used purely as *context*.** Reference outlines the client draws.
Viewer-independent, disclosing nothing, contributing to no displayed quantity. Free, and the
only rule it carries is that it never contributes to a displayed number.

**A polygon that is an access-controlled *item* in its own right**, with its own identity
and its own terms. **This is not a derived artifact at all** — it is an item that happens to
have an extent, governed by the points framework. At these cardinalities it is cheap: give
it an entity ID and a code from its containing cell and mask it exactly as a point is
masked. The extent matters only for tile assignment, which the smallest-containing-tile
convention handles, and at 10⁴–10⁶ objects a full scan per viewport is defensible anyway.

**Consequently, rare polygons need no new spatial index for the query that matters.** "How
many points in this postcode" is a build-time membership bitmap and one `and_cardinality`,
with no query-time spatial join at all. Only "which polygons intersect this viewport" wants
a structure, and at these cardinalities that is the bounding-box prune §7.5 already performs
for nodes. Administrative boundaries are naturally hierarchical, so the frontier machinery
applies unchanged.

### 5.1 The trap between the first two forms

**If a client draws only the boundaries that contain visible points, that filtering is
small-cell suppression with a threshold of one.** Displaying a boundary asserts "at least
one visible item here"; omitting it asserts "none". One is precisely the threshold the
census literature identifies as too low.

So either draw **all** boundaries — context, viewer-independent, free — or gate them on
`min_visible_members` like every other structure-revealing artifact. **Gating on
non-emptiness is the option that must not be taken by default**, and it is dangerous
because it looks like no decision at all.

### 5.2 Both kinds coexist, and mixing them needs no new mechanism

A deployment may hold boundaries tied to point visibility by a threshold *and* boundaries
under independent term-based access control — and the same boundary may be both *(owner,
2026-08-01)*.

If a boundary carries its own terms then it is an item, so it lives in entity space and the
existing mask covers it. One token, one satisfied-term set, two populations:
`M_auth ∩ boundary_ids` gives the boundaries a viewer may see, and
`and_cardinality(members(B), M_auth ∩ point_ids)` gives the masked count within one. The
induced-membership relation is §7.5's node membership bitmap under another name.

**Two composition rules.**

***Terms first, always.*** The boundary's own mask decides whether the viewer learns of it
at all; the threshold applies only after. **The fail-open to name is the reverse** — a
healthy induced count surfacing a boundary whose terms the viewer does not satisfy, which is
contained data granting access to its own container. Conjunction, never disjunction, in the
same shape as `M_sel = M_auth ∧ filters`.

***Where the geometry is independently authorised, the threshold governs the count, not the
shape.*** For a cluster the hull **is** corpus-derived, so a threshold must suppress the
geometry — the shape is itself the disclosure. For a boundary the viewer is cleared for, the
shape discloses nothing they are not already entitled to, so withholding it achieves nothing
and costs the map: what must be withheld is the **number**. That is §7.5's
rollup-rather-than-suppression applied to a second object — a boundary with no count, or a
count at a coarser level of the administrative hierarchy, rather than a hole.

**This is the design's first deliberately *partial* threshold**, and it should be reviewed
as such. Everywhere else, falling below `min_visible_members` withholds the whole artifact.
The argument for the exception is that an independently authorised shape carries no corpus
information, so suppressing it protects nothing — but "we made the control weaker for a good
reason" deserves the same scrutiny as the control itself.

## 6. The threshold tier has a fifty-year literature, and a named attack we have not checked

**`min_visible_members` is small-cell suppression** — the statistical-disclosure-control
rule from census-table publication. That field's central known weakness is the
**differencing attack**: two overlapping releases whose difference isolates a cell below the
threshold.

**One half is already blocked.** §8.4 fixes maximum depth against `M_auth` and never against
`M_sel`, which is the operational form of **I12** — a filter may move the frontier up, never
down — so no sequence of filters differences a suppressed node into view.

**The other half has never been posed: differencing the frontier across pan, zoom and
slice**, where the releases are viewport-shaped rather than filter-shaped. The descent is
restricted to nodes intersecting the viewport, so two overlapping viewports return
overlapping frontiers, and whether their difference can isolate a below-threshold node is an
open question.

**And boundary-bucketed counts make the analogy literal rather than structural.** Counts by
administrative area is the case small-cell suppression was invented for, and nested
administrative geographies — a postcode inside a ward inside a district — are its textbook
differencing vector. Unlike a semantic hierarchy, those geographies are **public and
enumerable**, so an attacker need not discover the structure first. That is a materially
easier attack than the one the design was implicitly reasoning about, and the geographic
stretch makes it a plausible deployment rather than a hypothetical.

Appendix C lists C1's owner and review date as outstanding before launch. **That review is
the place for this, and the finding is that it has a literature and a named attack to run
against rather than being a fresh judgement.** Both halves are annotated at design §7.5.

## 7. The differentiator check

**Nobody gates shared precomputed derived artifacts per viewer.** A survey of the adjacent
fields (2026-08-01) found exactly two other moves, both near misses:

**Re-derive per query under a filter.** Elasticsearch's `geotile_grid`/`geohex_grid`
aggregations under document-level security — the nearest thing to our underlay that exists
in production, at per-query cost, and with the seam leaks its own documentation concedes.
PostGIS `ST_ClusterDBSCAN`/`ST_ConcaveHull` under row-level security — possible, O(corpus)
per query, and heir to the query-plan disclosures Appendix D demolishes.

**Regenerate per viewer and never share.** The whole permissions-aware retrieval pattern,
which avoids the gating problem by paying generation cost per viewer per query.

**Nobody does the third thing: share the expensive artifact and gate it with a cheap
containment test.** That is §7.6's "appears unpublished" claim, now checked against the
adjacent fields rather than assumed.

§7.5's **rollup-rather-than-suppression** frontier likewise has no analogue found in any
clustering or mapping system — every one surveyed either recomputes per query or regenerates
per viewer. This is a claim of absence and recorded as one, and it **raises** rather than
lowers the burden on §6's review: there is no prior art whose failure modes we inherit and
can borrow.

## 8. Delivery

**Hierarchies ride the verbs, not the tiles.** There is no wire standard for delivering a
cluster hierarchy: MVT is flat per tile, and the maps industry encodes hierarchy as per-zoom
membership plus `rank` properties. What *is* de-facto standard is an **interaction API** —
supercluster's, which every mapping developer has met: `getClusters(bbox, zoom)` returning
flat features with `cluster_id` and `point_count`, plus `getChildren`,
`getLeaves(id, limit, offset)` and `getClusterExpansionZoom`. That maps almost one-to-one
onto what Phase 3 will build — session node handle for `cluster_id`, masked count for
`point_count`, the keyset cursor for paged leaves, frontier depth for expansion zoom — so
**speak that idiom**. Its architecture, a KD-tree over fully resident data, is unavailable
and irrelevant; the API shape is the transferable part. An MVT adapter accordingly emits the
current frontier as flat per-zoom features carrying rank, exactly as basemap schemas do.

**Contours: serve nothing.** They are a client-side derivation of already-masked data —
marching squares over an aggregation grid — and §7.3's underlay *is* that grid, masked and
exact. Client-derived isolines over served sub-cell counts are per-viewer and correct with
**zero new server surface**. This generalises §3's third tier: **any client-derived geometry
over served masked aggregates is safe**, because it derives from what the viewer can already
see.

**Edges travel as index pairs into a node buffer** — uniform across every graph renderer
surveyed. Their gate is cheap (`|G| = 2`, so one hidden endpoint kills one edge rather than
a chain, and the label pathology does not bite). Their hard problem is **sampling, not
masking**: an edge is drawable only if both endpoints are in the **served** set, not merely
the visible one. A future graph domain must either restrict edges to served × served —
degree-biased, and a named hard problem, the **induced-subgraph sampling problem** — or let
edges pull their endpoints into the served set, perturbing the point sample. Parked with
that name. Appendix H's masked-degree aggregates need none of it and remain the near-term
graph story.

**Three traps.** Adapter cluster and cell tiles must never be cached across viewers — the
same rule as point tiles, restated because aggregate tiles *look* shareable. Cluster
identifiers in the wild are ephemeral per rebuild, which matches per-session node handles,
so promise no more stability than supercluster taught people to expect. And any client-side
derivation computing breakpoints "from the data" means **the viewer's masked data** —
automatic in this model, but state it, so nobody imports a library preset expecting
corpus-global breaks.

*(One presentational idiom worth copying, from Kibana Maps: a "blended" layer that
auto-switches between individual documents and cluster marks on a count threshold — the
marks-to-underlay transition, decided client-side from served counts.)*

## 9. What this changes for the client

**A cluster hull is an aggregate over the visible set.** So a client that draws a hull around
the *k* points it holds and calls it the cluster boundary has committed the sample-as-set
error in **geometry** rather than in numbers — the same failure, in a shape nobody thinks to
check.

Which corrects the naming in the client design's §2: **the number channel is really the
exact masked-aggregate channel, and geometry travels on it** — hulls, centroids, contour
inputs and cell counts alike. The mark channel carries the sample; the other carries
whatever is exact, whatever its shape.

## 10. Open questions

- **C1's review against the small-cell-suppression literature**, covering frontier
  differencing across pan, zoom and slice, and treating boundary-bucketed counts as a
  first-class case (§6). Outstanding before launch; annotated at design §7.5.
- **The partial threshold of §5.2** — the design's first, and the place a weakening is most
  likely to be wrong.
- **Whether boundary polygons are in scope at all**, and if so whether as induced-subset,
  context, or access-controlled items (§5). The owner has indicated interest in the first
  and third; nothing is committed.
- **Edges** remain unscoped: the rule is settled (§3), the mechanism is not (§4), and the
  sampling problem is named but unsolved (§8).
- **Whether this document graduates into the `docs/design/` corpus**, which turns on whether the
  taxonomy survives C1's review.

## 11. Provenance

Split from the client interaction architecture design (2026-07-31) on 2026-08-01, at the
owner's prompting, once §9.1 had grown to a quarter of that document while answering a
question that is not a client question: it generalises design §7.5 and §7.6, its audience is
a security reviewer, and it had already produced two annotations in the specification.

The material came from an owner observation that clusters and labels were instances of
something wider, developed through a survey of how the field delivers non-point artifacts,
and sharpened by two further owner observations — that a cluster is a point-level property
rather than an inherent geometry, and that polygons may be independent of points and may
carry their own access control. The small-cell-suppression identification and the
differentiator check came from the survey; the differencing question and the partial
threshold are consequences nobody has yet reviewed.
