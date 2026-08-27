# The shape of a served artifact

**Status:** Normative — 2026-08-28. It governs what the `hull` vocabulary word means, and
`annotations.md` §4.2 and `contracts.md` §3.2 defer to it on the shape's geometry. The rulings that
closed it are in Appendix R.

**Owns:** which geometric family the derived `hull` belongs to, how its parameter is fixed, how its
members are grouped into rings, whether it carries holes, and what a client must do with the answer.
It does not own the declaration syntax (`annotations.md` §4.2), the wire columns (`contracts.md`
§3.2) or the closure rule that makes any of it safe (**I2**, restated at the artifact).

**Reads with:** [`annotations.md`](annotations.md) §4.2 (what a layer declares and the closure
rule); [`contracts.md`](contracts.md) §3.2 item 4 (the artifacts frame);
[`architecture.md`](architecture.md) §4 (**I2**) and Appendix C's head note (the inclusion test §10
applies); [decision 0099](../decisions/0099-the-map-follows-datamapplot-and-cluster-colour-is-exact-only.md)
(exact only); [`2026-08-26-concave-hulls.md`](../evidence/memos/2026-08-26-concave-hulls.md) (what
the single-ring shape cost, before this).

**Measured in** two places, and the distinction matters. The family survey is
[`probes/2026-08-27-artifact-shapes/`](../../probes/2026-08-27-artifact-shapes/), a Python
reimplementation that reproduces the engine's published figures exactly before comparing anything
against them. The two figures a *ruling* rests on are Rust against Rust in one process —
`crates/tessera-engine/tests/hull_triangulation.rs` for what a triangulation costs, and
`hull_geometry.rs` for what the served shape costs — because a Qhull-in-C against numpy-in-Python
column compares implementations rather than algorithms. Both read `notebook-2m4`: `clusters/hdbscan`
(197 artifacts, 6,146 … 2,422,486 members), with `clusters/kmeans` (64), `clusters/toponymy` level 3
(574) and `topics/hdbscan` (195) as controls.

## 1. What a viewer receives

For each served artifact whose layer declares `hull`, over `membership ∩ M_auth` and nothing else:

- The visible members are partitioned into **α-groups** (§5).
- Each group is drawn as **one simple ring**, counter-clockwise from its lowest vertex, dug inward
  from that group's convex wrap (§4).
- The rings are ordered by their first vertex, and travel as two `list<list<uint32>>` columns, one
  per axis (`contracts.md` §3.2 item 4).

Two parameters, and **neither is a caller's to set**. **α** is three times the median edge of the
visible members' own convex wrap — a length in the cloud's own units, robust because a single long
chord across a concavity is exactly the outlier a median ignores. It fixes both the grouping and
the digging. The **vertex budget** is 64 digs *per artifact*, spent longest bridge first across
every ring, so several groups do not multiply the wire.

What the shape guarantees:

- **Every vertex is a visible member's position.** No invented point, no cell corner, no smoothing.
- **Every member is inside the ring of its own group**, boundary included.
- **Each ring is simple** — it does not cross or touch itself.
- **Each ring is inside its group's convex wrap**, so the whole shape is inside the wrap of the
  visible members.
- **A degenerate group is its members.** One member is a ring of one vertex, two are a ring of two:
  rounding either up to a triangle would draw an area no member occupies.
- **It is a function of the member positions alone** — exact `i128` arithmetic throughout, apart
  from one square root that turns α² into the grouping grid's cell side. That square root is
  IEEE-754 correctly rounded and so is identical on every platform, its result is rounded up to a
  whole grid unit, and every join it then decides is an integer comparison of cell indices. Two
  principals' shapes differ only because their memberships do.

What it does **not** guarantee, stated because it is the property a reader will assume: a member may
lie inside a *second* ring of the same artifact, where one group wraps around another. Measured over
four layers: **0 on `clusters/hdbscan`, `clusters/toponymy` level 3 and `topics/hdbscan`, and 24
member positions on `clusters/kmeans`.** It costs nothing operationally — both rings carry the same
`tessera_id`, so a pick answers the same artifact either way — and it claims no ground the single
ring did not.

## 2. Why not the convex wrap

A cluster found by density is an irregular region. Its convex wrap swallows the space between its
arms, draws single straight chords across the viewport, and lies on top of clusters that share none
of its members. Three measurements say the same thing three ways, over `clusters/hdbscan`:

| | convex wrap | the dig at budget 64 | the α-complex over the same members |
|---|---|---|---|
| fill, artifacts of 50,000–200,000 members (23) | 0.834 | 0.893 | 1.000 |
| fill, artifacts of 200,000+ members (11) | 0.742 | 0.796 | 1.000 |
| layer area covered by two shapes on **different branches** | 24.52% | 15.86% | — |

*Fill* is the share of the drawn shape that the α-complex over the same members at the same α also
covers — the members' own footprint at that scale — so `1 − fill` is void the shape claims. The
overlap row excludes ancestor–descendant pairs, since a parent legitimately covers its children. The
middle column is the **single-ring** dig, which is what all three were measured on; grouping the
members (§5) only removes area, so they are an upper bound on what the shape now claims.

**The limit is the budget, not the construction.** 108 of 197 artifacts stop at the 64-vertex cap,
and every one of those stops with a bridging edge still live; only 13 digs over the whole layer were
refused for want of a candidate. Raising the budget closes the gap and closes it expensively: the
dig inserts one vertex per pass along the current longest edge, so at 1,024 vertices it reaches fill
0.993 for 2.7× the wire a triangulation-following shape needs for 0.992. §4 is where that trade is
settled.

## 3. Why not one ring

A membership can be two separated clouds, and one ring around both claims the ground between them.
No α corrects it: digging works inward from a boundary, and a gap with a ring on both sides is not
reachable from either — the shape either draws a polygon over the gap or, in a family free to
degenerate, a filament of near-zero area between the blobs that reads as a line on the map.

**The frequency is not the argument, and this is the point on which the measurement was overruled.**
On `clusters/hdbscan` at full membership, 3 of 197 artifacts have two components holding at least 5%
of their members, and only 0.1% of member rows sit outside their artifact's largest component; the
count does not rise under uniform random masks down to 0.2% of members, because α is measured in the
cloud's own units and follows the cloud as it thins. But the wire is not shaped by one corpus's
statistics. The `clusters/kmeans` control on the same bundle is already twice as modal; a
*spatially correlated* mask — which is what a real mask is, since terms correlate with position in
an embedding — is **not measured and should not be claimed either way**; and a clustering that is
not density-based has no reason to be unimodal at all. A shape family whose failure mode is lying
about the ground is the wrong default whatever its frequency here.

What splitting buys, measured on the shape as built: the tightest artifact on `clusters/hdbscan`
goes from **0.290 of its convex wrap to 0.093**. The whole-layer cost is 1.7% more hull bytes and 8%
more time (§7).

**The split is decided before the budget is spent**, and that ordering is the design. The
alternative — discovering the split during the dig, by letting a pinching dig separate the ring
instead of being refused — is a smaller change to the code and was declined: it splits only where 64
digs happen to reach, so the separation the ruling is about would become a function of a cap. 108 of
197 artifacts exhaust that cap.

## 4. The family

The admissibility test is one question, and it decides most of the survey: **does every vertex
correspond to a visible member's position, or does the shape claim ground no member occupies?** A
shape whose vertices are members says exactly what the data says; a shape whose vertices are
invented asserts a boundary the members never drew, and it costs the simplest form of the disclosure
argument in §10. This is the same objection that keeps a degenerate hull a point rather than
rounding it up to a triangle.

| family | vertices are members | contains every member | what it claims that is not true |
|---|---|---|---|
| convex wrap | yes | yes | 14.3% of its area is void; 24.5% of the layer's drawn area is one artifact over an unrelated one |
| **the dig, per group** (built) | yes | yes | at most 2.4% void at the median and 20.4% at 200,000+ members, those being the single ring's |
| χ-shape (Duckham et al.) | yes | yes | 0.8% void at the median; needs a triangulation (§4.1) |
| α-complex proper | yes | **no** | nothing — it is the members' footprint; that is why it is the fill denominator |
| k-NN hull (Moreira–Santos) | yes | yes, by restart | nothing geometric; it may not terminate |
| covariance ellipse | **no** | **no** | an ellipse the cluster is not, excluding members and covering ground no member occupies |
| buffered union (dilate and union) | **no** | yes | a disc's worth of ground around every member |
| density level set | **no** | **no** | a contour of a raster, at a level nothing in the data picks |
| any of the above, Douglas–Peucker | yes (keeps a **subset**) | **no** | a corner cut past the members it enclosed |
| any of the above, Chaikin or spline | **no** | **no** | a smooth boundary the members do not have |

Four are out on the vertex test alone.

**The α-complex is the interesting rejection.** It is the textbook object, it is the only family
whose fill is 1.000 by construction, it costs fewer bytes than the dig, and it yields components and
holes without being asked. It fails on containment: over `clusters/hdbscan` it leaves 24 members
outside their own artifact's shape (2 of 197 artifacts), and on the k-means control 137 (4 of 64). A
member outside its artifact's shape is a point the client would draw in the cluster's colour,
outside the cluster's outline — the display contradiction [decision 0099](../decisions/0099-the-map-follows-datamapplot-and-cluster-colour-is-exact-only.md)'s
*exact only* exists to prevent. It is retained as the fill denominator, not as a candidate.

**The k-NN hull terminates when it feels like it.** The Moreira–Santos walk restarts with `k + 1`
whenever it self-intersects or strands a member, and the paper bounds nothing. On one
16,929-member cluster: at 400 sampled members it needs `k = 40`, at 800 `k = 64`, at 1,500 `k = 120`,
and at 3,000 it dead-ends at every `k` up to 120. `k` is a neighbour *count*, not a length, so it
does not transfer across memberships of different density — and a service that derives a shape per
request per principal is handed a different density every time. Refused for termination, not shape.

**Douglas–Peucker is the only post-process that keeps the vertex property**, since it selects a
subset of its input. What it does not keep is containment: over the layer, at a tolerance of α/32 it
already leaves 12,866 members outside, at α/8 130,585, at α/2 712,003. There is no tolerance at
which containment survives. A containment-preserving, outward-only simplification is not measured
here.

### 4.1 What a triangulation costs, and why the dig keeps the job

The χ-shape is the better shape. It follows a Delaunay triangulation's own boundary — peel the
triangle behind the longest boundary edge above α, unless its third vertex is already on the
boundary — where the dig inserts one vertex per pass along the current longest edge, which is a
coarse way to spend a vertex. Over `clusters/hdbscan`, run per α-group exactly as the dig is:

| | vertices | rings | wire bytes | area / wrap, mean |
|---|---|---|---|---|
| the dig, per group (built) | 12,497 | 215 | 100,836 | 0.852 |
| χ-shape, per group | 9,424 | 223 | 76,284 | 0.789 |

**It costs a Delaunay triangulation, and the triangulation is the whole objection.** Timed in one
release-mode Rust process against the engine's own dig on the same gathered positions
(`hull_triangulation.rs`):

| | Delaunay | components from it | χ-peel | **the dig** |
|---|---|---|---|---|
| whole layer, 197 artifacts | 5.0–5.3 s | 1.0–1.1 s | 0.17–0.18 s | **0.82 s** |
| the 2,422,484-member artifact | 1.43–1.55 s | 0.27–0.34 s | 0.04 s | **0.16 s** |

The triangulated route is 7.6–8.1× the dig over the layer and about 11× on the largest artifact, and
a single artifact's triangulation crosses a second on its own. That is the price refused: **24% more
hull bytes and a shape 8% looser in area than the χ-shape would have given.** The refusal is about
the triangulation and nothing else — if the workspace ever carries one for another reason, the peel
itself is cheap and this is the first thing to revisit.

## 5. Grouping the members

Two members belong to one group when a chain of members steps between them in steps of at most α.
That is single-linkage at α, and it is computed conservatively rather than exactly.

**Grid connectivity at α.** The members are bucketed into a square grid anchored at their own
bounding box, with a cell side of α/2, and two members are joined when their cells are within two
cells of each other along both axes. A displacement of at most α moves a cell index by at most two
per axis, so **every pair within α lands in one group**: the grouping never separates members
single-linkage would join. It does join members up to 2.12α apart, and that is the safe direction —
an over-joined group draws the single ring the wire drew before, while an over-split one would claim
a gap the members do not have. Where the members are so scattered that a grid at that resolution
would hold more cells than there are members, the cell side doubles until they fit, which only ever
joins more. The pass is `O(members)` with no data-dependent worst case.

**The exact route is the one ruled out in §4.1.** The Euclidean minimum spanning tree is a subgraph
of the Delaunay triangulation, so cutting the triangulation's edges longer than α gives exactly the
single-linkage components — and costs the triangulation. What the approximation gives up, measured
against that exact partition over 197 artifacts:

| cell side | joins members up to | agrees exactly | artifacts with more than one ring |
|---|---|---|---|
| α | 2.83α | 190 / 197 | 2 |
| **α/2** | **2.12α** | **192 / 197** | **4** |
| α/3 | 1.89α | 191 / 197 | 3 |
| α/4 | 1.77α | 192 / 197 | 4 |
| exact (Delaunay) | α | — | 7 |

The exact partition finds 223 rings where the grid at α/2 finds 215. A finer grid does not converge
on the exact answer, because no grid can: the best a neighbourhood-of-cells join can do is √2·α as
the cell shrinks. α/2 is where the curve flattens.

## 6. Holes

**The wire carries no holes, and no ring encloses another.** Three reasons, in the order they bind.

The construction cannot produce one. Digging moves a boundary inward from a group's convex wrap, so
a void with members all the way around it is never reachable; the family that does emit interior
rings is the α-complex, which is refused for leaving members outside its own shape (§4). Carrying
holes would therefore mean changing the family, not adding a column.

Nothing would consume them. A hole is a claim — *no members here* — of exactly the same kind as the
outer boundary and exactly as exact, so a client could honour it. But no client tests a point
against a served shape: [decision 0099](../decisions/0099-the-map-follows-datamapplot-and-cluster-colour-is-exact-only.md)
forbids the geometric guess, and `extentOf` reads the served `box`. A hole would be drawn and never
used, and it would need a third level of nesting on the wire and an even-odd rule in the pick path
to be drawn correctly at all.

The residual is small and is stated rather than hidden. **An annulus of members is drawn as a
disk** — a unit test says exactly that, so a reader meets the decision at the mechanism. Over
`clusters/hdbscan` the α-complex, which is free to have them, has a hole on 2 of 197 artifacts and
three holes in total. Revisit this with §4.1: a triangulation is what both the χ-shape and a hole
would need, so they are one question and would be reopened together.

## 7. What it costs

Measured at **full membership**, which is the largest input the derivation takes; a masked principal
gathers fewer positions and digs a smaller cloud. Release build, one thread; timings vary about ±4%
run to run.

| layer | artifacts | members | rings | > 1 ring | wrap vertices | shape vertices | hull bytes | wrap | shape | area / wrap |
|---|---|---|---|---|---|---|---|---|---|---|
| `clusters/hdbscan` | 197 | 6,146 … 2,422,486 | 215 | 4 | 3,278 | 12,497 | 100,836 | 573 ms | 906 ms | 0.852 mean, 0.093 min |
| `clusters/kmeans` | 64 | 13,658 … 73,360 | 190 | 7 | 1,587 | 5,494 | 44,712 | 92 ms | 170 ms | 0.874 mean, 0.017 min |
| `clusters/toponymy` L3 | 574 | 1,211 … 11,682 | 618 | 18 | 7,846 | 25,237 | 204,368 | 57 ms | 153 ms | 0.798 mean, 0.012 min |
| `topics/hdbscan` | 195 | 200 | 197 | 2 | 2,098 | 3,833 | 31,452 | 1 ms | 3 ms | 0.874 mean, 0.113 min |

*Hull bytes* is 8 per vertex plus 4 per ring, and excludes the Arrow list offsets and validity, which
do not move with the shape. Position gathering — one read per member, which a declared `box` already
pays — is 181 ms over `clusters/hdbscan` and 46 ms on its largest artifact, and sits under both
columns.

Against the single ring it replaces, on `clusters/hdbscan`: **12,388 → 12,497 vertices, 99,104 →
100,836 bytes (+1.7%), 841 → 906 ms (+8%)**. The extra vertices are the additional groups' own
wraps; the extra time is the grouping pass. The shape's area is 0.858 → 0.852 of the wrap on
average, and 0.290 → 0.093 at its tightest — the whole gain is on the multi-modal artifacts, which
is what the ruling was about.

## 8. Who chooses

**Nobody, and no request field names a family or an α.** There is one family, so there is nothing to
declare; if a second is ever added, the choice belongs to the layer author at declaration, because
the right family follows the clustering that produced the layer, which is what the author knows and
neither the service nor the viewer can see. The service may *measure* a layer's shape and report
what it found, in the way [decision 0092](../decisions/0092-the-build-reports-a-layers-shape-and-no-layer-carries-a-declared-bound.md)
has the build report a layer's shape without binding anything to it. It must not flip one at a fold:
[decision 0094](../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)'s
automatic-with-override shape does not transfer, because a serving layout puts nothing on the wire
and both layouts answer identically, where a shape family *is* the bytes of `hull_x` and `hull_y`.

**A dialable α is the one that must not exist**, and for a sharper reason than a dialable family. A
family is a function of `membership ∩ M_auth` like everything else here, and five families over one
membership are five functions of the same visible members — there is no disclosure argument against
one, and inventing one would be a refusal outside the disclosure surface. α is a *length*. A caller
free to vary it receives a monotone family of nested shapes over the same members and can read the
cloud's boundary at every scale, which is more than any one response gives. Deriving α from the
principal's own wrap is what closes that, and it is why α is not a declared per-layer key either.

## 9. What the client must do

**Drawing.** One outline datum per ring, all of them carrying the artifact's `tessera_id`.
`outlineOf` maps a ring's vertices through `gridToWorld` and hands them to a `PolygonLayer`, and it
must keep refusing to smooth them: Chaikin cuts a reflex corner *outward*, and a drawn shape must
not claim area the served shape does not have. Vertex counts do not change character — the largest
shape on `clusters/hdbscan` is 144 vertices across at most 10 rings — so the per-artifact cost is
what it was.

**Picking.** The outline polygon is what answers a pick, filled at zero alpha so the pick pass sees
it, and the parallel `artifactIds` array assumes one polygon per artifact. That assumption is what
breaks, and it is the only thing that does: the fix is a map from row to artifact rather than an
index match. Two rings of one artifact may overlap (§1), and it costs nothing — both carry the same
identifier, so the pick answers the same artifact either way.

**`extentOf` and fit.** Unaffected. It reads the served `box`, which is the members' bounding box
whatever the hull is, and a hull is never the right thing to fit a viewport to — the box is a
superset of every ring.

**Decoding.** `hull_x` and `hull_y` are `list<list<uint32>>`: descend two levels, and **check that
the two axes agree on the ring count and on each ring's length** rather than assuming it. They agree
by construction, and a decoder that assumes it will misdraw silently on the day something else does
not. A single-level decode fails its downcast, which is the point of the nesting.

**The truthfulness rules do not move.** A hull is a display of where the visible members are; it is
never evidence of membership, and no client tests a point against it (decision 0099). A shape that is
tighter, or that has several rings, says *less* about the members a principal cannot see, never more,
so nothing the client is obliged to do gets harder as the shape gets honest.

## 10. Disclosure

Appendix C's inclusion test: *a row exists only where a viewer, reading responses they are entitled
to, can end up knowing something about data they were not served.*

- The input is `membership ∩ M_auth` and nothing else, which is the closure rule of
  `annotations.md` §4.2 and **I2** at the artifact. α and the grouping derive from the same visible
  members.
- Every vertex is a visible member's position — including the single vertex of a one-member group,
  which is the same property every hull vertex has always had, and the same one the `box` already
  exposes at the extremes.
- The whole shape is a **subset of the convex wrap** of the visible members, so it says strictly less
  about where the members a principal cannot see are sitting. Several rings say less again: they are
  the same members drawn without the ground between them.

**No register row, and this note is where that is recorded** — Appendix C's head note asks that a
check which found nothing sit beside the mechanism it checked rather than in the register.

## Appendix R — Review trail

- **r1** — Drafted 2026-08-27 as an investigation from
  [`probes/2026-08-27-artifact-shapes/`](../../probes/2026-08-27-artifact-shapes/), with five open
  rulings. Three measurements shaped it: the built shape's limit is its **vertex budget** and not
  the flush-flank case its own documentation records (108 of 197 artifacts stop at the budget with a
  bridge live, against 13 refused digs over the whole layer); **multi-modality is rare on this layer
  and mask-stable** (3 of 197 at full membership, flat under uniform random masks down to 0.2%);
  and the **χ-shape reaches the dig's best fidelity for 37% of its wire**.
- **r2 — 2026-08-28. Promoted, and the shape changed.** Six rulings, in the order they fell.
  **D (owner, 2026-08-27): several rings**, overriding r1's recommendation, on the ground that the
  measurement answered the wrong question — the wire is not shaped by one corpus's statistics, and a
  family whose failure mode is lying about the ground is the wrong default whatever its frequency
  (§3). **C: the engine does not acquire a triangulation** — measured Rust against Rust, 1.4–1.5 s
  for the 2.4M-member artifact's Delaunay against 0.16 s for the whole dig, 7.6–8.1× over the layer
  (§4.1). **A: the dig is kept**, which C forces; the χ-shape's 24% fewer bytes and 8% tighter area
  are the stated price. **B: the budget stays 64**, now per artifact and shared across rings, so
  several groups do not multiply the wire. **E: nobody chooses** — one family leaves nothing to
  declare, and the rule that survives is that no request field names a family or an α (§8).
  **F (new): no holes** — the construction cannot produce one, nothing would consume one under
  decision 0099, and the residual is an annulus drawn as a disk (§6). Two findings are recorded as
  negative results rather than smoothed over: **a member may lie inside a second ring of its own
  artifact** where one group wraps around another (24 positions on `clusters/kmeans`, 0 on three
  other layers), and **grid grouping is not exact** — it agrees with single-linkage at α on 192 of
  197 artifacts and coarsens the rest, and no grid can converge on exactness (§5).
