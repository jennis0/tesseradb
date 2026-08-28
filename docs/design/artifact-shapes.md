# The shape of a served artifact

**Status:** Normative — 2026-08-28 (r5). It governs what the `hull` vocabulary word means, and
`annotations.md` §4.2 and `contracts.md` §3.2 defer to it on the shape's geometry. The rulings that
closed it are in Appendix R.

**Owns:** which geometric family the derived `hull` belongs to, how its parameter is fixed, how its
members are grouped into rings, whether it carries holes, **which requests pay for it** (§8 C), and
what a client must do with the answer.
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
`hull_geometry.rs` for what the served shape costs and what reducing its input does to it
(`the_quantisation_sweep`) — because a Qhull-in-C against numpy-in-Python
column compares implementations rather than algorithms. Both read `notebook-2m4`: `clusters/hdbscan`
(197 artifacts, 6,146 … 2,422,486 members), with `clusters/kmeans` (64), `clusters/toponymy` level 3
(574) and `topics/hdbscan` (195) as controls.

## 1. What a viewer receives

For each served artifact whose layer declares `hull` **and whose request asked for it** (§8 C),
over `membership ∩ M_auth` and nothing else:

- The visible members are partitioned into **α-groups** (§5).
- Each group is drawn as **one simple ring**, counter-clockwise from its lowest vertex, dug inward
  from that group's convex wrap (§4).
- The rings are ordered by their first vertex, and travel as two `list<list<uint32>>` columns, one
  per axis (`contracts.md` §3.2 item 4).

Two parameters, and **neither is a caller's to set**. **α** is three times the median edge of the
visible members' own convex wrap — a length in the cloud's own units, robust because a single long
chord across a concavity is exactly the outlier a median ignores. It fixes both the grouping and
the digging. The **vertex budget** is 2,048 digs *per artifact*, spent longest bridge first across
every ring, so several groups do not multiply the wire. It is a guard on the wire and not a control
on the shape: on every layer measured the dig runs out of work of its own accord long before it
(§8 B).

What the shape guarantees:

- **Every vertex is a visible member's position.** No invented point, no cell corner, no smoothing.
- **Every member is within one cell of the ring of its own group**, and all but a handful are
  inside it. The shape is computed over one real member per occupied cell of a grid across the
  artifact's own extent (§7.1) rather than over every member, so a member can sit up to a cell
  beyond its own outline. Measured over `clusters/hdbscan` at full membership: **1,965 positions of
  12,808,679**, on 22 of 197 artifacts and at most 0.09% of any one artifact's members. Under §4's
  ruling that is an imprecise summary of where the cluster is, not a false claim about a member.
- **Each ring is simple** — it does not cross or touch itself.
- **Each ring is inside its group's convex wrap**, so the whole shape is inside the wrap of the
  visible members.
- **A degenerate group is its members.** One member is a ring of one vertex, two are a ring of two:
  rounding either up to a triangle would draw an area no member occupies.
- **It is a function of the member positions alone.** The construction is exact `i128` arithmetic
  apart from two floating-point steps, and both are identical on every platform because every IEEE-754
  operation they use is correctly rounded: the square root that turns α² into the grouping grid's
  cell side, whose result is rounded up to a whole grid unit before any join is decided, and the
  cross products of the eight-extreme filter that keeps α exact under the reduction (§7.1). Two
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

**Those three rows were measured at a budget of 64, which was then the limit and is no longer**
(§8 B, ruled 2026-08-28). At 64, 108 of 197 artifacts stopped at the cap with a bridging edge still
live, against 13 digs over the whole layer refused for want of a candidate — so the middle column
describes a shape the cap had truncated, and the fill and overlap figures in it are a **lower bound**
on what the budget of 2,048 draws. They have not been re-measured, because fill needs the Python
probe's α-complex and the ruling did not turn on them: the area figures that did move are re-measured
in §7 and they move the same way. Raising the budget closes the gap and closes it expensively — the
dig inserts one vertex per pass along the current longest edge, so at 1,024 vertices it reaches fill
0.993 for 2.7× the wire a triangulation-following shape needs for 0.992. §4 is where that trade is settled.

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
instead of being refused — is a smaller change to the code and was declined: it would make the
separation a function of how far the budget happened to reach. That was decided when 108 of 197
artifacts exhausted the cap and it survives the cap being raised, because a shape's components are
not a thing a wire-size guard should be allowed to decide.

## 4. The family

> **RULED 2026-08-28: containment is not required, and the test below is narrowed to what it was
> for.** The owner: *"it really doesn't matter if a small number of points are outside the hull, so
> long as it's showing the overall shape correctly."* A shape is a **summary of where a cluster is**,
> not a per-point assertion — that is what the membership column is, and what
> [decision 0099](../decisions/0099-the-map-follows-datamapplot-and-cluster-colour-is-exact-only.md)
> governs. A member drawn a little outside its own outline is an imprecise summary; it asserts
> nothing false about that member and nothing at all about members the viewer cannot see, so it is
> not a disclosure question and it was wrong to treat it as one. What the test still forbids is a
> shape that **claims ground the members do not occupy** — the convex wrap's swallowed voids, an
> ellipse over a crescent — because that misstates where the cluster is.
>
> **What this re-admits**, all of it rejected below on containment alone: the **α-complex**, whose
> fill is 1.000 by construction and which yields components and holes without being asked; **spline
> smoothing** of the drawn ring, which is how DataMapPlot's contours are actually made
> (`alpha_shapes.py`: an α-complex over Delaunay simplices by circumradius, then a periodic
> smoothing spline through it); and **containment-breaking simplification**, which is the cheapest
> byte saving available. §4's table's *contains every member* column is retained as a fact about
> each family, no longer as a bar.

The admissibility test is one question, and it decides most of the survey: **does every vertex
correspond to a visible member's position, or does the shape claim ground no member occupies?** A
shape whose vertices are members says exactly what the data says; a shape whose vertices are
invented asserts a boundary the members never drew, and it costs the simplest form of the disclosure
argument in §10. This is the same objection that keeps a degenerate hull a point rather than
rounding it up to a triangle.

| family | vertices are members | contains every member | what it claims that is not true |
|---|---|---|---|
| convex wrap | yes | yes | 14.3% of its area is void; 24.5% of the layer's drawn area is one artifact over an unrelated one |
| **the dig, per group** (built) | yes | yes | at most 2.4% void at the median and 20.4% at 200,000+ members — the single ring's, at the budget of 64 it was measured at |
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
| the dig, per group (built) | 28,459 | 215 | 228,532 | 0.771 |
| χ-shape, per group | 9,424 | 223 | 76,284 | 0.789 |

**It costs a Delaunay triangulation, and the triangulation is the whole objection.** Timed in one
release-mode Rust process against the engine's own dig on the same gathered positions
(`hull_triangulation.rs`):

| | Delaunay | components from it | χ-peel | **the dig** |
|---|---|---|---|---|
| whole layer, 197 artifacts | 5.0 s | 1.0 s | 0.17 s | **1.92 s** |
| the 2,422,484-member artifact | 1.44 s | 0.29 s | 0.04 s | **0.17 s** |

The triangulated route is **3.2×** the dig over the layer, and a single artifact's triangulation
crosses a second on its own. **Half of ruling C's margin was the old budget**, and this is where
that has to be said plainly: at 64 the dig cost 0.82 s and the ratio was 7.6–8.1×, so raising the
budget to 2,048 (§8 B) bought fidelity partly out of the same time the triangulation was refused
for. What the χ-shape buys over a whole layer has changed shape too — it sends **67% fewer hull
bytes** than the dig does, and its area is no longer the tighter of the two: 0.789 of the wrap
against the dig's 0.771.

**Re-ruled 2026-08-28 at the scale that now matters, and the answer is the same one for a different
reason.** *(The per-artifact table below was measured on the whole membership; §4.2 re-measures it
on the reduced input the derivation now takes, where the margin narrows sharply.)* A response derives a hull for the artifact the client draws and for no other (§8 C), so a
layer sum is the wrong denominator: what a viewer waits for is *one* artifact's shape, and what
rides the wire is one artifact's vertices. Measured per artifact over the same 197, gather included,
in `hull_triangulation.rs`:

| | p50 | p90 | worst |
|---|---|---|---|
| the dig | 1.7 ms | 19.9 ms | **259 ms** |
| triangulated (sort + Delaunay + components + χ-peel) | 4.9 ms | 35.3 ms | **1,859 ms** |
| ratio, per artifact | 3.7× | 6.2× | 11.2× |
| one shape on the wire — the dig | 748 B | — | 6,060 B |
| one shape on the wire — the χ-peel | 300 B | — | 1,668 B |

**The byte argument is gone and the latency argument has arrived.** 67% of a layer's hull bytes was
131 KB; 67% of one shape's is about 450 bytes, which is nothing beside the 26 KB response it rides
in. Against that, the shape a viewer waits for went from a number nobody could feel to the whole
cost of a hover — and the root of the collapsed HDBSCAN tree, which holds the entire corpus and is
hovered like any other artifact, is 0.21 s with the dig against 2.11 s with a triangulation. Two
seconds is not a hover.

**The α-complex and the χ-shape are one decision here, not two.** The α-complex is what the
containment ruling re-admits, and it is the tighter of the two — fill 1.000 by construction against
the χ-shape's 0.992 — but both begin with the same Delaunay triangulation, which is **76% of the
triangulated route's own time** over the layer. Choosing between them moves nothing in the table
above, so nothing about the re-admission changes the comparison with the dig. **Ruling A stands: the
dig keeps the job**, now on interaction latency rather than on layer-wide cost. Revisit it if the
workspace acquires a triangulation for another reason, or if a way is found to bound the root's.

### 4.2 The same comparison on the input the derivation now takes

**Ruling A is not reopened here and this section rules nothing.** It reports a number, because the
figure ruling A turned on has moved by an order of magnitude and a design that left the old one
standing would be quoting a measurement of a construction that no longer runs.

Both routes are now given the reduced input of §7.1 — one real member per occupied cell, plus every
member that could be a convex-hull vertex — because handing the triangulation the whole membership
while the dig is given a reduction of it compares two constructions over two different clouds. The
reduction and the sort are charged to the triangulated route explicitly and the dig pays them inside
its own timing (`hull_triangulation.rs`).

| | on the whole membership (r4) | on the reduced input |
|---|---|---|
| whole layer — the dig | 1,882 ms | **900 ms** |
| whole layer — sort + Delaunay + components + χ-peel | 6,835 ms | **1,852 ms** |
| the ratio over the layer | 3.44× | **1.77×** |
| the corpus root's Delaunay alone | 1,533 ms | **53 ms** |
| one shape, gather included — the dig | p50 1.7 ms, p90 19.9 ms, worst 263 ms | **p50 1.7 ms, p90 14.6 ms, worst 82 ms** |
| one shape, gather included — triangulated | p50 4.8 ms, p90 35.6 ms, worst 2,017 ms | **p50 4.9 ms, p90 23.9 ms, worst 121 ms** |
| the ratio per artifact | 3.7× / 6.2× / 11.8× | **3.6× / 6.1× / 7.4×** |
| one shape on the wire — the dig | p50 748 B, worst 6,060 B | p50 748 B, worst 4,980 B |
| one shape on the wire — the χ-peel | p50 300 B, worst 1,668 B | p50 300 B, worst 1,668 B |

**What has changed is the argument ruling A rests on, not its direction.** The dig is still cheaper
everywhere — 3.6× at the median, 7.4× at the worst — and the χ-shape still sends about 60% fewer
hull bytes. What has gone is *"two seconds is not a hover"*: the corpus root, which holds the whole
corpus and is hovered like any other artifact, costs **121 ms** through a triangulation now rather
than 2,017 ms. A hundred milliseconds is a hover. The Delaunay is also no longer the overwhelming
term — 67% of the triangulated route's own time, against 76% — because what it was overwhelming was
the cost of triangulating millions of points that the drawing could not resolve.

So the sentence in §4.1 that reads *"revisit it if the workspace acquires a triangulation for
another reason, or if a way is found to bound the root's"* has had its second clause met. **Whether
that is enough to move ruling A is the owner's**, and three things would go with it: the α-complex's
fill of 1.000 and its free components and holes (§6), a `delaunator` dependency the workspace does
not carry, and a shape whose vertices are still members but whose containment is the one §4's table
records as **no**.

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
on the exact answer, because no grid can: joining occupied cells within a fixed neighbourhood is
complete only when that neighbourhood's own diameter exceeds α, so it always joins members further
apart than α, and the best such a rule can do is √2·α as the cell shrinks. α/2 is where the measured
agreement stops improving — α/3 and α/4 do not beat it on this layer, and each costs more cells.

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
| `clusters/hdbscan` | 197 | 6,146 … 2,422,486 | 215 | 4 | 3,278 | 26,815 | 215,380 | 556 ms | 903 ms | 0.771 mean, 0.072 min |
| `clusters/kmeans` | 64 | 13,658 … 73,360 | 190 | 7 | 1,587 | 8,424 | 68,152 | 93 ms | 240 ms | 0.835 mean, 0.010 min |
| `clusters/toponymy` L3 | 574 | 1,211 … 11,682 | 618 | 18 | 7,846 | 28,400 | 229,672 | 57 ms | 174 ms | 0.785 mean, 0.012 min |
| `topics/hdbscan` | 195 | 200 | 197 | 2 | 2,098 | 3,833 | 31,452 | 1 ms | 3 ms | 0.874 mean, 0.113 min |

**Only the first row moved when the input reduction landed** (§7.1), and the other three did not
move at all: reduction engages on an artifact whose members outnumber the cells of its own grid, and
no artifact of `clusters/kmeans` (13,658 … 73,360 members), `clusters/toponymy` level 3 (1,211 …
11,682) or `topics/hdbscan` (200) is dense enough to reach it. That was checked rather than assumed
— re-running the sweep over `clusters/kmeans` returns the identical 8,424 vertices and 239 ms.

**No artifact on any of those four layers exhausts the budget**, which is the property §8 B is
about: the shape each row describes is the one the dig stops at on its own.

*Hull bytes* is 8 per vertex plus 4 per ring, and excludes the Arrow list offsets and validity, which
do not move with the shape. Position gathering — one read per member, which a declared `box` already
pays — is 163 ms over `clusters/hdbscan` and 42 ms on its largest artifact, and sits under both
columns. **It is now the floor**: on the corpus root the gather is 42 ms and the shape 39 ms, where
before the reduction the shape was 167 ms.

### 7.1 Reducing the input before computing the shape

Every construction in §4 consumes one position per visible member to produce something whose
resolution is bounded by the drawing: the largest shape here is 757 vertices over 2,422,486 members,
drawn about a thousand pixels wide. So the members are binned to a square grid over their own
bounding box and **the shape is computed over one real member per occupied cell** — the member
nearest that cell's centre, ties broken on the position itself.

**It is a quantisation and not a sample.** Every member falls in some cell, every occupied cell
contributes, and every vertex is still a visible member's own position, so §1's vertex property is
untouched. What it gives up is §1's old containment: a member can sit up to a cell beyond its own
outline, which §4's ruling permits and §1 quantifies.

**The resolution is 1,024 cells along the artifact's longer axis, and it is relative to the artifact
rather than to the request's zoom.** A shape drawn at all is drawn at most a viewport wide, so a
cell is a pixel or two of displacement in the case that matters. Making it depend on the zoom would
put the zoom in the cache key (§7.2), give a viewer a shape that flickers as they zoom, and hand the
identifier route — which carries no zoom — no answer at all. The stability is worth more than the
extra fidelity at a deep zoom, where the client's own drawn curve is already the coarser of the two:
the spline through the served ring leaves it by up to a third of the longest adjacent edge (§9), and
those edges are α-scale.

**α is the part that does not survive a naive reduction, and it is stated here because it was
nearly missed.** α is three times the median edge of the members' own convex wrap, and that
statistic follows the *sampling density* — the wrap of a sparser sample of the same region has fewer
vertices and longer edges. Taken over the representatives alone it moved by up to **3.6×** on one
artifact and took its shape from 0.29 to 1.29 of the unreduced one's area, which is a different shape
rather than a blurred one. So every member that could be a convex-hull vertex is carried through
beside the representatives, by Akl–Toussaint's filter against the polygon of the eight extremes of
`x`, `y`, `x + y` and `x − y`. The wrap of what the construction receives is then **exactly** the
wrap of the whole membership, and α is not approximated at all — measured as an exact match on every
artifact of the layer.

What it costs and what it buys, over the 22 of 197 artifacts dense enough to reduce, whose
9,287,043 members become 1,717,984 representatives:

| | median | p90 | worst |
|---|---|---|---|
| the boundary's departure from the unreduced shape, as a fraction of the artifact's own extent | 0.005 | 0.025 | 0.044 |
| the area, against the unreduced shape | 1.000 | — | 0.988 … 1.007 |

**Where it is not invisible, stated rather than averaged away.** At the median the departure is half
a percent of the artifact's extent — a pixel or two on a thousand-pixel drawing. On one artifact of
the 197 it is 4.4%: a single concavity that the unreduced dig opens and the reduced one does not,
because the members it would have dug to are no longer candidates. The area is within 1.2% there, so
it is one notch rather than a shape that has moved. Below 1,024 divisions that case gets common
enough to matter — at 512 the worst departure is 17% of an artifact's extent — which is what fixes
the resolution here rather than lower, where the time would be better.

**The derivation is single-threaded and stays so** (owner ruling, 2026-08-28). Parallelism in this
engine lives at the *request* level, so concurrent requests use the cores; a `par_iter` over an
artifact's members would let one request oversubscribe the pool the others are queued behind, which
trades a served viewer's latency for a hovering one's. The two things that made the derivation cheap
are the ones a second thread would have hidden: the input is reduced, and the answer is not computed
twice.

### 7.2 Holding the answer

Nothing held a derived centroid, box or hull, so the identical artifacts request three times running
cost 2.4 s, 2.9 s and 2.8 s. It is now held per principal, under a key whose first term is the
principal and whose other terms are each a reason that principal's visible set or the artifact's
membership moved: the view, the layer, the level and its write counter, the ordinal, the geometry
version, the overlay's counter, the fragment's identity and watermark, and the property set the
request asked for (§8 C).

**A shape is never shared across principals, and that term is the whole of the disclosure argument**
(§10). Two viewers of one artifact have two different clouds, so one viewer's outline is not an
answer to another's request; a hit answers the request that would have computed the same value.
The attribute filter is deliberately *not* a term, because derived content is filter-blind exactly
as the masked count beside it is (**I12**).

The cache is bounded at 64 MiB resident and evicts least recently used. There is no configuration
key, and that is a deliberate difference from the two caches that have one: their entry size scales
with the *corpus* — a row projection is a measured 125 MB at 10⁹ items — where an entry here is one
artifact's outline, bounded by the vertex budget whatever the corpus does. A miss costs the
derivation: a measured p50 of 1.6 ms, p90 of 14.4 ms and 82 ms on the corpus root.

**What a pan gets, measured rather than modelled.** Twelve overlapping viewports walking across the
map, each followed by three of the hovers a client makes when a pointer lands on a shape, against a
server over `notebook-2m4` as the principal holding its 176 terms: **220 ms out, 27 ms back over the
same twelve, 26 ms out again**. At the level below, a pan of eight viewports over a fixture whose
eight artifacts are all in every one derives each shape exactly once — 56 hits against 8 misses,
which is the shape of the property rather than of the corpus
(`tests/artifact_cache_cadence.rs`).

Against the single ring it replaces, measured at the budget of 64 both were built at:
**12,388 → 12,497 vertices, 99,104 → 100,836 bytes (+1.7%), 841 → 906 ms (+8%)**. The extra vertices
are the additional groups' own wraps; the extra time is the grouping pass. The shape's area went
0.858 → 0.852 of the wrap on average and 0.290 → 0.093 at its tightest — the whole gain was on the
multi-modal artifacts, which is what that ruling was about. The budget then went 64 → 2,048 (§8 B),
which is the larger of the two moves and is the one the table above measures.

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

**The vertex budget is 2,048, and it is a wire-size guard.** It was 64, and at 64 it was choosing
the shape rather than bounding it: 108 of 197 artifacts on `clusters/hdbscan` ran out of budget with
a bridging edge still live, 34 of 64 on `clusters/kmeans` and 100 of 574 on `clusters/toponymy`
level 3 — so what a viewer saw was a polygon the cap had truncated, and rounding its corners could
not make it follow its members because the polygon underneath was coarse. Swept over those three
layers at the grouping as it is now built:

| budget | vertices | hull bytes | exhausted, of 197 / 64 / 574 | area vs the group wraps, median | area, worst | dig, whole layer |
|---|---|---|---|---|---|---|
| 64 | 12,497 / 5,494 / 25,237 | 100,836 / 44,712 / 204,368 | **108 / 34 / 100** | 0.870 / 0.942 / 0.835 | 0.290 / 0.699 / 0.177 | 0.89 / 0.17 / 0.16 s |
| 128 | 17,862 / 6,925 / 28,146 | 143,756 / 56,160 / 227,640 | 58 / 11 / 10 | 0.832 / 0.928 / 0.833 | 0.290 / 0.540 / 0.177 | 1.01 / 0.20 / 0.18 s |
| 256 | 22,918 / 7,778 / 28,400 | 184,204 / 62,984 / 229,672 | 26 / 2 / 0 | 0.804 / 0.928 / 0.833 | 0.276 / 0.435 / 0.177 | 1.22 / 0.22 / 0.18 s |
| 512 | 27,169 / 8,103 / 28,400 | 218,212 / 65,584 / 229,672 | 11 / 1 / 0 | 0.803 / 0.928 / 0.833 | 0.255 / 0.366 / 0.177 | 1.62 / 0.23 / 0.18 s |
| 1,024 | 28,459 / 8,424 / 28,400 | 228,532 / 68,152 / 229,672 | **0 / 0 / 0** | 0.803 / 0.928 / 0.833 | 0.255 / 0.366 / 0.177 | 1.88 / 0.24 / 0.18 s |
| **2,048** | **28,459 / 8,424 / 28,400** | **228,532 / 68,152 / 229,672** | **0 / 0 / 0** | **0.803 / 0.928 / 0.833** | **0.255 / 0.366 / 0.177** | 1.92 / 0.24 / 0.18 s |
| unbounded | 28,459 / 8,424 / 28,400 | 228,532 / 68,152 / 229,672 | 0 / 0 / 0 | 0.803 / 0.928 / 0.833 | 0.255 / 0.366 / 0.177 | 1.89 / 0.24 / 0.18 s |

The three figures in each cell are `clusters/hdbscan` / `clusters/kmeans` / `clusters/toponymy`
level 3, at full membership, from `hull_geometry.rs`'s `the_budget_sweep`. *Exhausted* is reported by the construction — the budget ran out while
a bridging edge was still live — and not inferred from a vertex count, so a dig that happened to
stop at the cap with nothing left to dig is not counted. The area denominator is the **groups' own
convex wraps**, which is the dig with the digging switched off, because the question is what the
digging buys and the grouping has already been paid for.

**The dig runs out of work on its own at 732, 833 and 197 digs** on the three layers, and past that
every column is identical to the unbounded dig. So the value is not chosen at a knee — the area
curve has no knee left, it simply stops — but at a distance above the largest number of digs any
artifact actually wanted. **2,048 is 2.5× that**, which is what leaves a corpus rougher than these
three still getting the shape its members ask for; 1,024 would sit 1.23× above it, close enough that
the cap could start deciding shapes again on a layer nobody has measured. Past 2,048 nothing is
bought on any measured layer, and the worst case a pathological membership can put on the wire —
16 KB of `hull_x`/`hull_y` for one artifact — is what the guard is for.

**What a request costs, before and after `computed`** (§8 C, 2026-08-28). A `k = 0` artifacts
request over the whole extent, as the principal holding this bundle's 176 terms, against the same
server on one machine — the median of four runs, nothing cached anywhere:

| request | before `computed` | with `computed: ["centroid", "box"]` | the same, this principal's second time |
|---|---|---|---|
| `clusters/hdbscan`, 197 artifacts | **2,032 ms**, 263,079 B | **167 ms**, 26,402 B | **1.2 ms** |
| `clusters/toponymy`, `levels: "all"`, 797 artifacts | **803 ms**, 534,630 B | **84 ms**, 91,236 B | **2.3 ms** |
| `taxonomy/arxiv`, 209 artifacts — declares no hull | 78 ms, 26,915 B | 78 ms, 26,915 B | **2.3 ms** |
| `clusters/hdbscan` asking for **every** hull | **2,032 ms**, 263,079 B | **1,046 ms**, 249,508 B | **1.3 ms** |

The taxonomy row is the control and it does not move under `computed`, which is the point: it was
already the cost of an artifacts request without a hull, and the two clustering rows now sit beside
it. **The hull was 92% of the first request's time and 90% of its bytes.**

The fourth row is the request that still asks for all 197 shapes, and it is where §7.1 and §7.2 show
up on the wire: **2,032 → 1,046 ms** for reducing the input, and 1.3 ms for the same principal
asking again. The last column is that second ask on every row — the geometry a request derives is
held for the principal that derived it (§7.2), so the first two columns are what a *new* principal
pays and the last is what panning costs.

**What a hover costs, end to end**, on the same server: the one `/v1/artifacts/{id}` call the client
makes when the pointer lands on a shape (§9), with the session already warm so that only the shape
is cold. JSON bodies of 0.7–14 KB throughout.

| members | before §7.1 | after §7.1 | the same shape again (§7.2) |
|---|---|---|---|
| 2,422,486 — the corpus root | 188 ms | **70 ms** | 0.48 ms |
| 1,844,620 | 265 ms | **73 ms** | 0.45 ms |
| 507,264 | — | 48 ms | 0.41 ms |
| 6,146 … 34,091 | 2–5 ms | **0.9 … 4.8 ms** | 0.35 … 0.43 ms |

So the whole layer's shapes cost less than one of them did, the two that are slow are slow only when
they are pointed at, and pointing at one twice costs nothing the second time.

**What it cost on the wire before any of this**, kept because §8 B's ruling rests on it. The same
`clusters/hdbscan` request went from **130,471 to 262,055 bytes** when the budget moved 64 → 2,048,
of which `hull_x` and `hull_y` are **103,288 → 230,984** — 79% of that response before, 88% after. The same request at `k = 5,000` over a zoom-4 viewport, which also carries tiles
and 5,000 points, goes from **287,223 to 418,807 bytes**: the layer's shapes are 46% on top of a
response that was 124 KB of points and tiles without them. The derivation cost is the other half of
the price and is in §7 — 0.91 → 1.91 s for all 197 artifacts at full membership. The largest
single artifact barely moves — 0.16 → 0.17 s — because it never exhausted 64 in the first place:
the root of the collapsed tree holds the whole corpus and is very nearly convex, so it spends 29
digs and stops.

**A request says which declared properties it wants, and that is the one thing it does choose**
*(C, 2026-08-28)*. `/v1/viewport` carries `computed`: absent is the layer's own declaration, a list
is that list **intersected** with the declaration, and the empty list is counts with no geometry.
It narrows and can never widen, so a layer that declares no hull serves none however it is asked,
and the intersection is taken before the property is computed rather than after — what the field
buys is the work not done.

**It exists because the declaration is per layer and the drawing is per artifact.** A client draws
a hull for the artifact under the pointer and centroids for the rest, so with only a layer-level
declaration it was served 197 shapes to draw one: 92% of the request's time (§7). The client now
asks the viewport for `centroid` and `box` and asks `/v1/artifacts/{id}` for the one shape it
draws.

**It is not the dialable family this section refuses, and the difference is not a matter of
degree.** A family or an α would let a caller ask the same members a *different question* and read
the answer against the first; `computed` asks strictly fewer of the same questions. There is one
value of each property for a given membership, every request that receives it receives the same
one, and a request that asks for less is served less. A name outside the three-word vocabulary is a
`422` rather than an absence — the vocabulary is deployment schema, fixed and published in
`/v1/meta`, so refusing discloses nothing, where an unreachable *layer* name is viewer data and is
absent instead.

**A dialable α is the one that must not exist**, and for a sharper reason than a dialable family. A
family is a function of `membership ∩ M_auth` like everything else here, and five families over one
membership are five functions of the same visible members — there is no disclosure argument against
one, and inventing one would be a refusal outside the disclosure surface. α is a *length*. A caller
free to vary it receives a monotone family of nested shapes over the same members and can read the
cloud's boundary at every scale, which is more than any one response gives. Deriving α from the
principal's own wrap is what closes that, and it is why α is not a declared per-layer key either.

## 9. What the client must do

**Asking for the shape.** The viewport request carries `computed: ["centroid", "box"]` (§8 C) and
the shape for the artifact that draws is fetched from `POST /v1/artifacts/{id}`, which has always
served the same geometry from the same predicate. The client asks when the pointer lands on a shape
and when one is opened; the answer is held per identifier, and **dropped whenever the principal
could have changed**, because a hull is derived from that principal's own visible members.

**Until the shape arrives, the artifact's `box` is drawn and answers the hover.** That is the same
fallback a layer declaring no hull has always taken, and it is a real change to what a hover feels
like: at rest every candidate is a rectangle, so two clusters whose boxes overlap are separated by
depth, by the smaller box, and by the artifact the mark under the pointer belongs to — which is the
wire's own membership column and a better answer than geometry gave. The index is rebuilt around
the true shape when it lands.

**Drawing.** One outline datum per ring, all of them carrying the artifact's `tessera_id`.
`outlineOf` maps a ring's vertices through `gridToWorld` and hands them to a `PolygonLayer`. Vertex
counts changed with the budget (§8 B) and the client's work did not: the largest shape on
`clusters/hdbscan` is **757 vertices** across at most 10 rings, against 144 before, and a
`PolygonLayer` ring of 757 vertices is the same call as one of 144.

**Smoothing, and where it belongs.** The drawn ring is a **periodic uniform cubic B-spline** through
the served vertices, sampled four times per span — DataMapPlot's construction (`alpha_shapes.py`
fits `splprep(..., s=spline_coeff, per=True)` and evaluates it with `splev` at a multiple of the
vertex density; the α shape underneath is as angular as ours), in its closed knot-free form. It
replaces a containment-preserving corner cut, which is deleted: that refused to round a **reflex**
corner because the chord across a notch lies outside the polygon, so a dug shape's concavities
stayed as angular as the wire while its convex arcs rounded, which is the opposite of what a
contour should look like.

**It is client-side, and the served ring does not move.** Three reasons, in the order they bind.
The guarantee in §1 that every vertex is a visible member's position is worth more than the curve
is: a served spline would put invented points on the wire, and every containment argument here —
the ring inside its group's wrap, the shape inside the members' wrap — is about vertices that are
members. Second, the curve is a **display** choice that depends on the zoom it is read at, which
the server does not know; the client picks the sample density and can raise it on a deep zoom for
nothing. Third, it costs four times the vertices, and serving them would put that multiple on
every shape rather than on the one that draws.

**What the curve may and may not do.** It smooths rather than interpolates: at a knot it sits at
`(Pᵢ₋₁ + 4Pᵢ + Pᵢ₊₁)/6`, a sixth of the second difference from the vertex — inward at a convex
corner and outward at a reflex one — and every point of a span lies in the convex hull of its four
control points, so the excursion outside the served ring is bounded by a third of the longest
adjacent served edge. A dug ring's edges are α-scale lengths in the cloud's own units, so the curve
reaches at most a fraction of the members' own spacing past their outline. Under §4's ruling that
is an imprecise summary of where the cluster is, not a claim about ground the members do not
occupy. **Anything that reasons about containment reads the served ring**: the pick, the hover
index, and the containment oracle the client's own tests use.

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

- **Asking for fewer properties serves fewer** (§8 C). The intersection is with the layer's own
  declaration, so no request reaches a property a layer did not declare, and every property that is
  served is the same value it would have had if it had not been asked for by name. There is nothing
  a caller can learn by asking twice with different selections that one of the two answers did not
  already carry.

- **A held shape is one principal's own** (§7.2). The cache's key names the session, the geometry,
  the overlay's counter and the fragment the request composed against, so a hit answers the request
  that would have derived the same value from the same visible members. Sharing one entry across
  principals would be exactly the disclosure the closure rule forbids, and it is the key's first
  term that makes it unexpressible rather than merely avoided. Nothing in the cache is ever
  mutated: a deny rotates the key rather than editing an entry, so the corrected outline is served
  on the next request rather than at the next refresh.

**No register row, and this note is where that is recorded** — Appendix C's head note asks that a
check which found nothing sit beside the mechanism it checked rather than in the register.

## Appendix R — Review trail

- **r5 — 2026-08-28. The derivation is made cheap, twice over, and the family question is
  re-measured on what it now costs.** Deriving one shape was ~10 ms and linear in the visible
  membership, when what it produces is bounded by the drawing.
  **The input is reduced before the shape is computed** (§7.1): one real member per occupied cell
  of a 1,024-across grid over the artifact's own extent, which is a quantisation and not a sample.
  The whole layer's digging goes 1,906 → 903 ms and the corpus root 167 → 39 ms. Two things are
  recorded rather than smoothed over. **α does not survive a naive reduction** — it follows the
  sampling density, and over the representatives alone it moved by up to 3.6× and changed a shape
  rather than blurring it; carrying every possible convex-hull vertex through the reduction makes it
  exact instead. And **§1's containment is given up**: 1,965 member positions of 12.8M now sit
  outside their own shape, by at most a cell, which is admissible only because of §4's ruling and
  which §1 now quantifies rather than promising the opposite.
  **Derived geometry is held per principal** (§7.2), keyed so that no entry is ever shared across
  principals — a hover on the corpus root is 70 ms cold and 0.48 ms warm, and a pan of twelve
  viewports with three hovers each is 220 ms out and 27 ms back.
  **Ruling A is not reopened, and §4.2 is the number that would reopen it.** On the reduced input
  the triangulated route is 1.77× the dig over the layer rather than 3.44×, and the corpus root's
  triangulated shape is **121 ms** rather than 2,017 ms — so *"two seconds is not a hover"*, which
  is what r4's re-ruling rested on, is no longer the argument. The dig is still 3.6× cheaper at the
  median and 7.4× at the worst, and whether that is enough is the owner's to rule.
  The derivation acquires no `rayon` and must not: parallelism stays at the request level so that
  concurrent requests use the cores rather than one request oversubscribing them (owner ruling).

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
  (§4.1). **A: the dig is kept**, which C forces; the χ-shape's 24% fewer bytes and 7% tighter area
  are the stated price. **B: the budget stays 64**, now per artifact and shared across rings, so
  several groups do not multiply the wire. **E: nobody chooses** — one family leaves nothing to
  declare, and the rule that survives is that no request field names a family or an α (§8).
  **F (new): no holes** — the construction cannot produce one, nothing would consume one under
  decision 0099, and the residual is an annulus drawn as a disk (§6). Two findings are recorded as
  negative results rather than smoothed over: **a member may lie inside a second ring of its own
  artifact** where one group wraps around another (24 positions on `clusters/kmeans`, 0 on three
  other layers), and **grid grouping is not exact** — it agrees with single-linkage at α on 192 of
  197 artifacts and coarsens the rest, and no grid can converge on exactness (§5).
- **r3 — 2026-08-28. Ruling B reopened on fresh measurement and settled the other way: the budget
  is 2,048, not 64.** The multi-ring grouping landed after r2's figures were taken, and a budget
  shared across an artifact's rings does not buy what a single ring's did, so the sweep was re-run
  on the built shape over three layers (§8 B). It found the cap doing the job B itself says it must
  not: 108 of 197, 34 of 64 and 100 of 574 artifacts ran out of budget with a bridging edge still
  live, which is why a shape rounded at the client still failed to follow its members. The dig
  **terminates on its own** at 732, 833 and 197 digs, and 2,048 is 2.5× the largest of those, so
  the cap is now a guard on the wire rather than a control on the shape — the number is chosen from
  where the digging stops, not from an area curve, which no longer has a knee. Nothing else moved:
  family, grouping, α and the wire's shape are r2's.
  Three consequences are recorded rather than smoothed over. The wire roughly doubles — a real
  `k = 0` artifacts response for `clusters/hdbscan` goes 130,471 → 262,055 bytes, hull columns 79%
  → 88% of it — and derivation over that layer at full membership goes 0.91 → 1.91 s. **Ruling C's
  margin narrowed from 7.6–8.1× to 3.2×**, because half of it was the old budget; C was not
  re-litigated, and the χ-shape's advantage is now 67% fewer hull bytes rather than 24%, while its
  area is no longer the tighter of the two (0.789 against the dig's 0.771). And §2's fill and
  overlap figures were measured at 64 and are left standing as a **lower bound**, marked at the
  claim, because re-measuring fill needs the probe's α-complex and no ruling turned on them.
- **r4 — 2026-08-28. The shape is served where it is drawn, the family is re-ruled at that scale,
  and the drawn ring is a spline.** Two things forced it. The owner ruled (§4's head) that
  containment is not required, which re-admits the α-complex and spline smoothing; and a
  measurement found the hull was **92% of a `k = 0` artifacts request** — 2.03 s for
  `clusters/hdbscan`'s 197 shapes against 0.07 s for a layer declaring none — while the client
  draws **one**.
  **C (new): `/v1/viewport` carries `computed`** (§8 C), a narrowing of each layer's own
  declaration and never a widening of it. The request above is now **164 ms and 26 KB**, and
  `clusters/toponymy` at every level 803 ms → 81 ms. It is not the dialable family §8 refuses: a
  family would let a caller ask the same members a different question, where this asks strictly
  fewer of the same ones.
  **A re-ruled, and the answer did not move**: at one shape per request the χ-shape's 67% fewer
  hull bytes is worth about 450 bytes on the one shape drawn, while the triangulation it needs is
  3.7× the dig at the median and 11.2× at worst — 2.11 s against 0.21 s on the corpus root, which
  is hovered like any other artifact. The α-complex and the χ-shape share that triangulation (76%
  of the route's time), so the re-admission changes nothing in the comparison. The dig keeps the
  job, now on interaction latency rather than on layer-wide cost.
  **Smoothing landed client-side** (§9): a periodic uniform cubic B-spline through the served
  vertices, DataMapPlot's construction, replacing the containment-preserving corner cut that
  refused to round a reflex corner. The served ring is unchanged and is what every containment
  argument and the pick still read; the drawn curve leaves it by at most a third of the longest
  adjacent edge.
  One consequence is recorded rather than smoothed over: **a hover is resolved against the `box`
  until the shape arrives**, so at rest the map's hover index is rectangles. Depth, the smaller
  box and the mark's own membership column separate them, and the index is rebuilt on the true
  shape when it lands — 2–5 ms for an ordinary cluster, 188 ms for the corpus root.
