# The shape of a served artifact

**Status:** Provisional — an investigation and a design, 2026-08-27. **What remains before it
becomes normative:** the five owner rulings in §8 (which family the `hull` vocabulary word names,
whether the vertex budget stays a flat 64, whether the wire carries more than one ring, who
declares the family, and whether the engine acquires a triangulation), and the one measurement §8's
recommendation rests on that this probe could not take — what a Rust Delaunay costs against a Rust
dig. **Nothing here is built**: `main` serves the shape §2 describes, and every claim about an
alternative is a claim about a probe.

**Owns:** which geometric family the derived `hull` belongs to, how its parameter is fixed, how many
rings it may have, who chooses, and what a client must do with the answer. It does not own the
declaration syntax (`annotations.md` §4.2), the wire columns (`contracts.md` §3.2) or the closure
rule that makes any of it safe (**I2**, restated at the artifact).

**Reads with:** [`annotations.md`](annotations.md) §4.2 (what `hull` means today and the closure
rule); [`contracts.md`](contracts.md) §3.2 item 4 (the artifacts frame — `hull_x` and `hull_y`, two
same-length `list<uint32>`); [`architecture.md`](architecture.md) §4 (**I2**) and Appendix C's head
note (the inclusion test §7 applies); [decision 0099](../decisions/0099-the-map-follows-datamapplot-and-cluster-colour-is-exact-only.md)
(exact only); [decision 0094](../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)
(the automatic-with-override shape §5 tests against);
[`2026-08-26-concave-hulls.md`](../evidence/memos/2026-08-26-concave-hulls.md) (what the built shape
costs).

**Measured in** [`probes/2026-08-27-artifact-shapes/`](../../probes/2026-08-27-artifact-shapes/),
over `notebook-2m4`'s `clusters/hdbscan` — 197 artifacts, 6,146 … 2,422,484 distinct member
positions — at **full membership**, with `clusters/kmeans` (64 artifacts) as a control. Every
quantity below is from that probe unless it names the memo. The probe reimplements the engine's
construction in Python and **agrees with the engine's published figures exactly** — 3,278 wrap
vertices and 12,388 shape vertices over the layer, area 0.870 median and 0.858 mean and 0.290
minimum, 108 of 197 artifacts at the vertex budget (`validate.py`) — which is what licenses
comparing anything else against it.

## 1. What the problem is

A cluster found by density is an irregular region. Its convex wrap swallows the space between its
arms, draws single straight chords across the viewport, and lies on top of clusters that share none
of its members. The shape `main` serves is a concave one and is a large improvement on that. It is
not the end of the question, for a reason the whole-layer picture shows and the per-artifact tables
confirm: **on the artifacts large enough to matter, the shape it serves is nearly the convex wrap**.

Three measurements say the same thing three ways.

| | convex wrap | shape on `main` | χ-shape (§3) |
|---|---|---|---|
| fill, artifacts of 50,000–200,000 members (23) | 0.834 | 0.893 | **0.995** |
| fill, artifacts of 200,000+ members (11) | 0.742 | 0.796 | **0.996** |
| layer area covered by two shapes on **different branches** | 24.52% | 15.86% | **1.39%** |

*Fill* is the share of the drawn shape that the α-complex over the same members at the same α also
covers — the members' own footprint at that scale — so `1 − fill` is void the shape claims. The
overlap row excludes ancestor–descendant pairs, since a parent legitimately covers its children;
what remains is two artifacts drawn over the same ground with no member in common (`overlap.py`).

The cause is not the construction. It is the **budget**: 108 of 197 artifacts stop at the 64-vertex
cap, and every one of those 108 stops with a bridging edge still live — the shape ran out of
vertices, not of concavity. Across the whole layer only **13** digs were refused for want of a
candidate or for simplicity, so the limitation the memo records — a concavity whose flanks are
flush with the edge bridging it cannot be dug — is real and is **not** what binds. Raising the
budget closes the gap: at 1,024 the dig reaches fill 0.993 against the χ-shape's 0.992
(`budget_sweep.py`, 186 artifacts under 200,000 members).

**It closes it expensively.** The dig inserts one vertex per pass along the current longest edge,
which is a coarse way to spend a vertex; the χ-shape follows a triangulation's own boundary.

Over the **186 artifacts under 200,000 members**, which is what this probe's Python dig can reach
at a 1,024-vertex budget:

| construction | vertices | wire bytes | area / wrap | fill | artifacts at budget |
|---|---|---|---|---|---|
| dig, budget 64 (`main`) | 11,428 | 91,424 | 0.864 | 0.977 | 97 / 186 |
| dig, budget 128 | 16,162 | 129,296 | 0.821 | 0.988 | 52 / 186 |
| dig, budget 256 | 20,139 | 161,112 | 0.807 | 0.991 | 17 / 186 |
| dig, budget 512 | 22,328 | 178,624 | 0.804 | 0.992 | 4 / 186 |
| dig, budget 1,024 | 22,681 | 181,448 | 0.804 | 0.993 | 0 / 186 |
| **χ-shape** | **8,364** | **66,912** | 0.832 | 0.992 | — |

The χ-shape reaches the dig's best fidelity for **37% of its bytes**, and for 73% of what the dig
already sends at budget 64.

## 2. The construction on `main`, and where it fails

Start at the convex wrap, repeatedly replace the longest edge above α with two edges through the
member closest to it, refuse the dig when no member qualifies or when the ring would stop being
simple, and stop after 64 insertions. `annotations.md` §4.2 states it; `derived.rs` implements it.

Its properties are the ones that matter and it keeps all of them: every vertex is a visible
member's position, every visible member is inside, the result is a subset of the wrap, and the
arithmetic is exact in `i128` so the shape is a function of the member positions and of nothing
else. **Nothing below asks to give any of those up.**

Where it fails:

- **The budget binds, and binds worst where the map is busiest.** Above, and in the memo's own size
  table.
- **A vertex bought is not a vertex spent well.** The dig at 1,024 vertices and the χ-shape at 8,364
  over the same 186 artifacts reach the same fill; the dig uses 2.7× the wire to do it.
- **α is sound and its value is not obviously right.** Three times the median edge of the
  principal's own convex wrap is scale-free and robust, and it is **mask-stable**, which is a
  stronger property than it was given credit for: over 40 artifacts masked to uniform random
  subsets down to 0.2% of their members, with α recomputed per subset exactly as the engine does,
  the χ-shape's median fill stays between 0.984 and 0.996 and the count of artifacts whose members
  fall into more than one component does not rise (`mask_sweep.py`). The value 3 is a different
  question, and it interacts with the budget:

| α (× median wrap edge) | dig vertices | dig area/wrap | dig fill | dig at budget | χ vertices | χ area/wrap | χ fill |
|---|---|---|---|---|---|---|---|
| 1 | 15,656 | 0.823 | 0.846 | 185 / 197 | 28,886 | 0.633 | 0.995 |
| 1.5 | 15,070 | 0.826 | 0.910 | 166 / 197 | 18,828 | 0.700 | 0.995 |
| 2 | 14,272 | 0.841 | 0.961 | 140 / 197 | 14,057 | 0.754 | 0.995 |
| **3** | 12,388 | 0.870 | 0.976 | 108 / 197 | 9,441 | 0.825 | 0.992 |
| 5 | 9,953 | 0.917 | 0.982 | 68 / 197 | 6,192 | 0.911 | 0.988 |
| 8 | 7,202 | 1.000 | 0.982 | 41 / 197 | 4,602 | 1.000 | 0.987 |

  Read the dig's column and α is not a tightness knob at all: **the dig's fidelity is worse at a
  finer α**, because a finer α finds more bridges than 64 vertices can dig and the budget spends
  them on the longest ones. 3 is close to where that stops hurting, which is a defensible place to
  have landed. Read the χ column and α is what it is supposed to be — a monotone tightness knob
  whose fidelity does not move. **At α = 8 both families return the convex wrap**, which is what
  makes the wrap the honest answer at a coarse enough scale rather than a failure mode.
- **The 64 is a magic number reached by a knee that no longer holds.** The memo found the knee by
  trading area against bytes at a fixed construction. Once the construction is in question the knee
  moves: the χ-shape at 8,364 vertices beats the dig at 22,681.

## 3. The families

The admissibility test is one question, and it decides most of the survey: **does every vertex
correspond to a visible member's position, or does the shape claim ground no member occupies?** A
shape whose vertices are members says exactly what the data says; a shape whose vertices are
invented asserts a boundary the members never drew, and it costs the simplest form of the
disclosure argument in §7. This is the same objection that keeps a degenerate hull a point rather
than rounding it up to a triangle.

| family | vertices are members | contains every member | rings | wire, whole layer | what it claims that is not true |
|---|---|---|---|---|---|
| convex wrap | yes | yes | 1 | 26,224 B | 14.3% of its area is void; 24.5% of the layer's drawn area is one artifact over an unrelated one |
| **α shape, dig (`main`)** | yes | yes | 1 | 99,104 B | 2.4% void at the median, 20.4% at 200,000+ members |
| **χ-shape** (Duckham et al.) | yes | yes | 1 | 75,528 B | 0.8% void at the median; on a genuinely multi-modal membership it draws filaments between the blobs |
| α-complex proper | yes | **no** | 1 … 10 | 80,648 B | nothing — it is the members' footprint; that is why it is the fill denominator |
| k-NN hull (Moreira–Santos) | yes | yes, by restart | 1 | not measured | nothing geometric; it may not terminate (below) |
| covariance ellipse | **no** | **no** | 1 | 48 v, fixed | an ellipse the cluster is not, excluding members and covering ground no member occupies |
| buffered union (dilate and union) | **no** | yes | 1 … n | 78 v on one artifact | a disc's worth of ground around every member |
| density level set | **no** | **no** | 21 on one artifact | 3,584 v on one artifact | a contour of a raster, at a level nothing in the data picks |
| any of the above, Douglas–Peucker | yes (keeps a **subset**) | **no** | unchanged | see below | a corner cut past the members it enclosed |
| any of the above, Chaikin or spline | **no** | **no** | unchanged | 832 v where 208 went in | a smooth boundary the members do not have |

Four of these are out on the vertex test alone. Rendered side by side over one 98,225-member
cluster in `figures/families-hdb-2422728.png`, where the ellipse and the level set are visibly
describing something other than the cluster.

**The α-complex is the interesting rejection.** It is the textbook object, it is the only family
whose fill is 1.000 by construction, and it costs *fewer* bytes than the dig. It fails on
containment: over the layer it leaves 24 members outside their own artifact's shape (2 of 197
artifacts), and on the k-means control 137 (4 of 64). A member outside its artifact's shape is a
point the client would draw in the cluster's colour, outside the cluster's outline — the exact
display contradiction decision 0099's *exact only* exists to prevent. It is retained here as the
**fill denominator** and as the thing a multi-ring wire would be carrying, not as a candidate.

**The k-NN hull terminates when it feels like it.** The Moreira–Santos walk restarts with `k + 1`
whenever it self-intersects or strands a member, and the paper bounds nothing. On one
16,929-member cluster (`knn_scaling.py`): at 400 sampled members it needs `k = 40`, at 800 `k = 64`,
at 1,500 `k = 120`, and at 3,000 it dead-ends at every `k` up to 120. `k` is a neighbour *count*,
not a length, so it does not transfer across memberships of different density — and a service that
derives a shape per request per principal is handed a different density every time. **Not
recommended, and the reason is termination, not shape.**

**Simplification is not free, and Douglas–Peucker is the only post-process that keeps the vertex
property** — it selects a subset of its input, so a simplified χ-shape still has only members for
vertices. What it does not keep is containment. Over the layer (`dp_sweep.py`):

| tolerance | vertices | wire bytes | area / χ | members left outside |
|---|---|---|---|---|
| none | 9,441 | 75,528 | 1.000 | 0 |
| α/32 | 5,442 | 43,536 | 0.999 | 12,866 (0.10%) |
| α/16 | 4,375 | 35,000 | 0.996 | 44,219 (0.35%) |
| α/8 | 3,322 | 26,576 | 0.990 | 130,585 (1.02%) |
| α/4 | 2,337 | 18,696 | 0.978 | 309,227 (2.41%) |
| α/2 | 1,494 | 11,952 | 0.961 | 712,003 (5.56%) |

There is no tolerance at which containment survives. A containment-preserving simplification would
have to be outward-only and is not measured here.

**The χ-shape is the recommendation.** Take the Delaunay triangulation of the members; the boundary
starts as the convex wrap; repeatedly remove the triangle behind the longest boundary edge above α,
**unless its third vertex is already on the boundary**. That last clause is the whole of it: it is
what keeps the result one simple ring with no holes and no pinch points, and it is why the χ-shape
degrades to a filament rather than to a hole where the dig degrades to the wrap. Every vertex is a
member, every member stays inside (measured: 0 outside over 197 + 64 artifacts), and the result is
inside the convex wrap because a triangulation of the members fills exactly that wrap.

Its cost is a **Delaunay triangulation the workspace does not have**, which is one of the two
reasons the memo declined the α-complex. In this probe, Qhull triangulates and the peel runs in
18.4 s + 6.4 s over 186 artifacts against 6.7 s for the numpy dig at budget 64 and 23.1 s at
budget 1,024 — but the dig here is a numpy loop and Qhull is C, so **that column compares
implementations, not algorithms, and is not evidence.** What the engine's dig costs is measured
(841 ms over the layer, memo); what a Rust triangulator costs is not measured at all. §8's ruling C
names it.

## 4. One ring or several

The wire carries one ring, and an HDBSCAN cluster in a UMAP projection can be several separated
blobs. How often it is, on this corpus, is the measurement this section exists for. Two members are
in one component when a chain of members steps between them in steps of at most α — the same α the
shape uses; the Euclidean minimum spanning tree is a subgraph of the Delaunay triangulation, so
cutting Delaunay edges longer than α gives exactly the single-linkage components at α.

**On `clusters/hdbscan`, at full membership, multi-modality is rare.**

- Artifacts with two or more components holding at least 5% of members: **3 of 197**. At 10%: **1**.
- Members outside their artifact's largest component: **8,321 of 12,808,677 member rows (0.1%)**.
- The α-complex — the shape that is free to be disconnected — has more than one ring on **4 of
  197** artifacts, and a hole on **2**, three holes in total.

**It does not grow under a mask.** Over 40 artifacts thinned to uniform random subsets at 50%, 20%,
5%, 1% and 0.2% of their members, with α recomputed from each subset's own wrap, the count of
multi-modal artifacts is 2, 1, 0, 1 and 1 of 40, and 1 of the 33 that still have 16 members at
0.2% — flat rather than rising. The explanation is α's own definition: it is measured in the
cloud's own units, so it follows the cloud as the cloud thins. **This is measured against a uniform random mask only.** A real
mask follows terms, terms correlate with position in an embedding, and a spatially correlated mask
will break a cloud more than a uniform one does; that case is **not measured** and should not be
claimed either way.

**It is a property of the clustering, not of the map.** On the `clusters/kmeans` control the same
measurement gives **4 of 64** artifacts multi-modal at 5%, 1.9% of member rows outside their
largest component, and an α-complex with more than one ring on 7 of 64 — one of them with **51**
rings. k-means cells are convex by construction, and it shows in the other direction too: on that
layer every family's precision is exactly 1.000, meaning no k-means cluster's shape contains a
single point belonging to another. HDBSCAN's do: 81 of 197 convex wraps, 50 of 197 dig shapes and
27 of 197 χ-shapes contain more than 10% foreign points.

**The recommendation is one ring, and it is not a claim that one ring is always honest.** On the
three multi-modal artifacts the single ring is visibly a lie in one of two ways
(`figures/multimodal.png`): the dig draws a polygon over the gaps (fill 0.089 on the worst), and
the χ-shape draws a spider — filaments of near-zero area joining the blobs, which claims almost no
ground but reads as lines on the map and is awkward to pick. The α-complex draws ten rings and is
right.

What carrying several rings would cost, so the ruling is made with it in view:

- **The wire.** Two same-length `list<uint32>` columns become either a list of lists, or the same
  two columns plus a third `list<uint32>` of ring start offsets. Either is a schema change to one
  frame under decision 0048's rules, and neither is expensive: the whole layer's rings add 4 bytes
  each.
- **The client's drawing.** deck's `PolygonLayer` takes a simple ring or a ring-with-holes in one
  datum, and a **disconnected** shape is not one datum: it becomes several outline rows sharing one
  `tessera_id`, and the pick path's `artifactIds` parallel array stops being one row per artifact.
- **The client's containment reasoning.** Nothing in the client tests containment today — decision
  0099 forbids the geometric guess, and `extentOf` reads the served `box`, not the hull — so a
  multi-ring shape changes **nothing** there. That is the cheapest half of this question and it is
  worth saying plainly.
- **The truthfulness rules.** They do not move. A ring per component says *the members are here and
  here*; one ring around both says *the members are somewhere in this region*. The first is
  strictly more true, which is why the α-complex is the object a multi-ring wire would carry — and
  the α-complex is the family that drops members, so a multi-ring wire would want a
  **containment-preserving** disconnected shape, which is the χ-shape run per component. That is a
  design, not a parameter, and it is why this is ruling D rather than a default.

## 5. Who chooses

Four surfaces, against the question the brief asks and the property §7 must preserve.

**The layer author, at declaration.** `ComputedProperty` is `Centroid | Box | Hull` and cannot
express a family. It could: `hull` gains a family name and, if the family has one, its α factor.
The author knows what produced the layer, which is the thing the choice actually depends on — the
k-means control needs no concavity at all and the HDBSCAN layer needs a great deal.

**The service, automatically, with a declared override**, which is exactly decision 0094's shape
for the serving layout. **The argument does not transfer, and the reason is short: 0094's choice
puts nothing on the wire.** Both layouts answer identically, so a fold may flip one freely. A shape
family is *on the wire* — it is the bytes of `hull_x` and `hull_y` — so an automatic flip at a fold
would change what every client draws, for the same `tessera_id` and the same principal, with no
version move and nothing in the response saying so. What does transfer is the *measurement* half:
the service can measure a layer's shape and **report** which family fits, in the way decision 0092
has the build report a layer's shape without binding anything to it.

**The viewer, per request.** This is the one to be careful about, in both directions.

It does not leak. Every family in §3 is a function of `membership ∩ M_auth` and of nothing else; a
caller who asks for the same membership under five families receives five functions of the same
visible members, and the union of five functions of a set is still a function of that set. There is
no *disclosure* argument against a per-request family, and inventing one would be the failure mode
CLAUDE.md names — a refusal outside the disclosure surface that looks principled.

The arguments against it are cost and coherence. The shape is derived per request per principal
already, so a family is not a new axis of work — but a *selectable* family is a reason to ask
twice, and two responses would carry different geometry for one `tessera_id` within one session,
which is a stronger version of the caching hazard `contracts.md` §3.2 already warns about across
principals. And the viewer has no basis on which to choose: the right family follows the clustering
that produced the layer, which the viewer cannot see.

**A dialable α is the one that must not exist**, and for a sharper reason than a dialable family.
α is a length. A caller free to vary it receives a monotone family of nested shapes over the same
members and can read the cloud's boundary at every scale — which is more than any one response
gives, even though it is still bounded by `membership ∩ M_auth`. It is a probe in the brief's sense
whether or not it crosses the register's inclusion test, and the derivation of α from the
principal's own wrap is what closes it. **Keep that.**

**Recommended: the layer author declares the family; the build reports what it measured; nothing
flips at a fold; no request field names a family or an α.** If that is wrong, the cost is a layer
declared with a family that suits it badly — visible on the map, fixed by editing one declaration
and rebuilding, and disclosing nothing. That is the cheapest wrong answer of the four.

## 6. What the client must do

**Drawing.** Nothing changes for a single-ring shape of any family: `outlineOf` maps the wire's
vertices through `gridToWorld` and hands them to a `PolygonLayer`, and it already refuses to smooth
them, because Chaikin cuts a reflex corner *outward* and a drawn shape must not claim area the
served shape does not have. A χ-shape has more reflex corners than the dig's, which makes that rule
matter more rather than differently. Vertex counts are of the same order — the dig's largest is 94
and the χ-shape's is 208 over this layer — so the outline path's per-artifact cost does not change
character. A multi-ring shape needs one outline datum per outer ring, with holes as inner rings of
the datum that contains them.

**Picking.** The outline polygon is what answers a pick — filled at zero alpha so the pick pass
sees it — and the parallel `artifactIds` array assumes one polygon per artifact. A multi-ring shape
breaks that assumption and nothing else: the fix is a map from row to artifact rather than an index
match. A filament, which is what a single-ring χ-shape draws across a multi-modal membership, is
pickable in the sense that it has area, and not in the sense that anyone can hit it.

**`extentOf` and fit.** Unaffected. It reads the served `box`, which is the members' bounding box
whatever the hull is, and a hull is never the right thing to fit a viewport to — the box is a
superset of every family here.

**The truthfulness rules do not move.** A hull is a display of where the visible members are; it is
never evidence of membership, and no client tests a point against it (decision 0099). A shape that
is tighter, that has holes, or that has several rings says *less* about the members a principal
cannot see, never more, so nothing the client is obliged to do gets harder as the shape gets
honest.

## 7. Disclosure

Appendix C's inclusion test: *a row exists only where a viewer, reading responses they are entitled
to, can end up knowing something about data they were not served.* Applied to every family
recommended or rejected above:

- The input is `membership ∩ M_auth` and nothing else, which is the closure rule of
  `annotations.md` §4.2 and **I2** at the artifact. α derives from the same visible members.
- Every vertex of the χ-shape, the dig, the wrap, the α-complex and the k-NN hull is a visible
  member's position. The ellipse, the buffered union, the level set and every smoothing invent
  vertices — which is an argument against them and **not** a disclosure, since an invented vertex
  is a function of the same visible members too. They are refused on truthfulness, not on leakage,
  and §3 says so in that order.
- Every recommended shape is a **subset of the convex wrap**, so it says strictly less about where
  the members a principal cannot see are sitting.

**No register row, and this note is where that is recorded** — Appendix C's head note asks that a
check which found nothing sit beside the mechanism it checked rather than in the register.

The one property that must be preserved by whatever §5 is ruled: **no request field may name a
family or an α**. §5 argues that a family is not a disclosure and an α is a probe; the rule that
follows is the same either way, and it is cheaper to keep than to reason about per feature.

## 8. The open decisions

**A — Which family does `hull` name?**
1. Keep the dig at budget 64, as built. Cheapest; leaves the largest artifacts at fill 0.796 and
   the layer at 15.9% cross-branch overlap.
2. Keep the dig and raise the budget (§1's table; 512 is where it stops binding). No dependency,
   no new code beyond a constant. Costs 2× the wire the shape sends today for fill 0.992.
3. **Replace it with the χ-shape.** Same fidelity as (2) for 37% of its bytes and 73% of today's.
   Costs a Delaunay triangulation the workspace does not carry.

*Recommended: 3, subject to C.* If it is wrong, the cost is a dependency and a rewrite of one
function; the shape's properties are unchanged, so nothing downstream moves.

**B — Does the vertex budget stay a flat 64?** The 64 was a knee measured against the dig. Under
(3) the χ-shape's own peel stops at α with no cap at all, and its largest shape on this layer is
208 vertices — 1,664 bytes. Either keep a cap (the peel is longest-edge-first, so a cap gives a
coarser shape and never a wrong one, exactly as the dig's does) or drop it. *Recommended: keep a
cap, well above the measured maximum, as a wire-size guard rather than a fidelity control.*

**C — Does the engine acquire a triangulation?** A is not rulable without this and this probe
cannot answer it: Qhull-in-C against numpy-in-Python is not a comparison. The measurement that
settles it is a Rust Delaunay over the same 197 memberships timed against `derived.rs`'s dig on the
same machine, which is an afternoon and no design. *Recommended: take that measurement before
ruling A; if a Rust triangulation of the 2.4M-member artifact costs more than about 1 s, rule A2
instead.*

**D — Does the wire carry more than one ring?** Measured need at full membership: 3 of 197
artifacts on `clusters/hdbscan`, 4 of 64 on `clusters/kmeans`, and flat under a uniform mask —
with a correlated mask unmeasured. Carrying several rings costs a third column, one outline datum
per ring in the client, and a change to the pick path's index assumption; it changes no
truthfulness rule and no leak-register row. *Recommended: one ring, and the χ-shape's filament as
the honest degradation. Revisit if a layer arrives whose clustering is not density-based —
k-means is already twice as modal, and the 51-ring artifact on that layer is what this looks like
when it goes wrong.*

> **RULED 2026-08-27: several rings.** The owner overrides the recommendation, and the reason is
> that the measurement above answers the wrong question: it says multi-modality is rare *on this
> corpus*, and the wire is not shaped by one corpus's statistics. Multi-modal memberships exist
> generally — the k-means control on this very bundle is already twice as modal, a correlated mask
> is unmeasured, and a clustering that is not density-based has no reason to be unimodal at all. A
> single ring around two separated components is a claim about where the members are that no α
> corrects, and a shape family whose failure mode is *lying about the ground* is the wrong default
> whatever its frequency here. **A and C are re-opened in this light**: the χ-shape is defined to
> keep one simple polygon, while an α-complex yields components naturally, so the family question
> and the multi-ring question are one question and are answered together.

**E — Who chooses (§5)?** The options are: the layer author at declaration; the service
automatically with a declared override (decision 0094's shape); the viewer per request. *Recommended:
the author declares, the build reports what it measured, nothing flips at a fold, and no request
field names a family or an α.*

## Appendix R — Review trail

- **r1** — Drafted 2026-08-27 from the investigation in
  [`probes/2026-08-27-artifact-shapes/`](../../probes/2026-08-27-artifact-shapes/). Not yet
  reviewed. Three findings shape it and each is a measurement rather than a judgement: the built
  shape's limit is its **vertex budget** and not the flush-flank case its own documentation
  records (108 of 197 artifacts stop at the budget with a bridge live, against 13 refused digs over
  the whole layer); **multi-modality is rare on this layer and mask-stable** (3 of 197 at full
  membership, flat under uniform random masks down to 0.2%), which is the opposite of what the
  brief expected and is the reason ruling D recommends against a multi-ring wire; and the
  **χ-shape reaches the dig's best fidelity for 37% of its wire**, which is the reason ruling A
  recommends replacing the construction rather than raising its budget. The comparison the ruling
  most needs — a Rust triangulation against the Rust dig — is **not measured**, and ruling C says
  so rather than estimating it.
