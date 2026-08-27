# The served hull follows its cluster

**Date:** 2026-08-26 · **Track:** `hull` (branch `client/hull`)
**Status:** Evidence. Not normative. The rule it records lives in
[`annotations.md`](../../design/annotations.md) §4.2; the construction lives in
`crates/tessera-engine/src/derived.rs`.
**Measured on:** `notebook-2m4`'s `clusters/hdbscan` layer — 197 artifacts, 6,146 … 2,422,486
members — at **full** membership, on a release build, one thread. Re-run before quoting:
`TESSERA_HULL_BUNDLE=<bundle root> cargo test --release -p tessera-engine --test hull_geometry -- --ignored --nocapture`.

## What changed, and what it costs

`ComputedProperty::Hull` was the **convex** wrap of the visible members. It is now a **concave
(alpha) shape** over the same members. Nothing else moved: same inputs, same per-request derivation,
same grid units, same wire columns, no configuration key, no leak-register row.

| | convex wrap | concave shape |
|---|---|---|
| area, median over the layer | 1.000 | **0.870** of the wrap (mean 0.858, min 0.290) |
| vertices, whole layer | 3,278 | **12,388** |
| hull columns, whole layer | 26,224 bytes | **99,104 bytes** (+278%) |
| time, whole layer | 577 ms | **841 ms** (1.5×) |
| time, the 2,422,486-member artifact | 129 ms | **158 ms** |

Position gathering — one read per member, which a declared `box` already pays — is 172 ms over the
layer and 43 ms on the largest artifact, and sits under both columns.

By size (medians within the band):

| members | artifacts | wrap vertices | shape vertices | gather | wrap | shape | area |
|---|---|---|---|---|---|---|---|
| < 10,000 | 60 | 15 | 52 | 0.10 ms | 0.28 ms | 0.52 ms | 0.843 |
| 10,000 – 50,000 | 103 | 16 | 79 | 0.23 ms | 0.70 ms | 1.28 ms | 0.864 |
| 50,000 – 200,000 | 23 | 21 | 85 | 1.04 ms | 3.50 ms | 5.48 ms | 0.907 |
| ≥ 200,000 | 11 | 23 | 87 | 4.91 ms | 16.97 ms | 23.71 ms | 0.934 |

**Containment was checked, not assumed.** Every one of the 197 shapes was tested against its whole
membership by a point-in-polygon oracle written from the definition
(`crates/tessera-engine/tests/common/ring.rs`, compiled into the measurement binary). No member fell
outside any shape.

**The layer's own range is not the brief's.** The brief quoted 176 … 900,000 members; at full
membership the layer runs 6,146 … 2,422,486, the top of it being the root of the collapsed HDBSCAN
tree, which holds the whole corpus. The smaller figures are presumably a masked principal's. The
measurement is over the full membership because that is the largest input the derivation can be
given, and a masked one is strictly cheaper.

## The construction

Start at the convex wrap, which contains every member. Repeatedly take the longest edge `(a, b)`
above α and replace it with `(a, c)` and `(c, b)`, where `c` is the member closest to the line
through `a` and `b`, among those on the interior side of `a → b` that project strictly inside the
segment. Refuse the dig, and retire the edge, when there is no such member or when the ring would
stop being simple.

Two properties come from `c` being the *closest*:

- **Containment.** A member strictly inside triangle `a c b` would be on the interior side of
  `a → b` and strictly closer to the line than `c`, contradicting `c`'s minimality. The triangle is
  empty, so removing it removes no member, and the shape contains every member by induction from the
  wrap. The projection restriction is load-bearing here rather than cosmetic: the triangle lies
  inside the strip between the perpendiculars at `a` and `b` exactly because `c` does, so a member
  strictly inside it is itself a candidate.
- **No epsilon.** Minimising the perpendicular distance to the line is minimising the cross product,
  the divisor being fixed per edge. Everything is exact in `i128`, so the shape is a function of the
  member positions and of nothing else — no float, no platform drift, no order-dependent tie-break.

Simplicity is **checked** rather than argued: an empty triangle holds no vertex, but an edge from
elsewhere on the boundary can still cross it with both endpoints outside, which two arms of a
crescent digging towards each other would do. The check is O(V) per dig against a bounded boundary.

Cost is O(n) to bucket the members plus, per dig, one pruned pass over the buckets and one over the
boundary. The bucket grid holds about 64 members to a bucket, at most 64 divisions per axis; each
bucket carries the bounding box of what it holds, so one corner bounds the cross product over the
whole bucket and the bucket is skipped when that bound cannot beat the best candidate so far.

## The two parameters, and why neither is a caller's

**α = 3 × the median edge of that principal's own convex wrap.** Scale-free — a length measured in
the same cloud's units — and robust, since a single long chord across a concavity is exactly the
outlier a median ignores and a mean would chase. It is a function of `membership ∩ M_auth` like
everything else here, so two principals' shapes differ only because their memberships do and a
request cannot dial one. **It is not a declared per-layer key**, and the brief's stop-and-report
condition for one was not reached.

**The vertex budget is 64, beyond the wrap's own count.** An absolute cap is not available: every
vertex is a visible member's position and every member is inside the shape, so the wrap's vertex
count is a floor — going below it means leaving a member outside or inventing a vertex no member
occupies. Measured across the layer, the wrap's own count is small (median 15–23), so the budget is
what the wire cost is made of.

| budget | vertices | hull columns | at the budget | area (mean) | layer time |
|---|---|---|---|---|---|
| 8 | 4,712 | 37,696 B | 175 / 197 | 0.976 | 713 ms |
| 16 | 6,052 | 48,416 B | 164 / 197 | 0.950 | 689 ms |
| 32 | 8,431 | 67,448 B | 138 / 197 | 0.910 | 772 ms |
| **64** | **12,388** | **99,104 B** | **108 / 197** | **0.858** | **841 ms** |
| 128 | 17,783 | 142,264 B | 62 / 197 | 0.811 | 991 ms |

64 is the knee: the area keeps improving past it, but the wire cost is linear and the artifacts
still at the budget at 128 are the very large ones, whose boundaries would need thousands of
vertices before the area moved much. **What the cap costs in fidelity** is visible in the size table
— the ≥ 200,000-member artifacts reach only 0.934 of the wrap's area, and the 2.4M-member root
0.998, because digging spends the budget longest edge first and stops with the shorter bridges of a
long boundary undug. A shape that runs out of budget is a coarser shape, never a wrong one: it still
contains every member and is still a subset of the wrap.

## Disclosure: no register row, and why

The brief's argument holds and was checked against Appendix C's inclusion test — *a row exists only
where a viewer, reading responses they are entitled to, can end up knowing something about data they
were not served.*

- The inputs are `membership ∩ M_auth` and nothing else, which is the closure rule of
  `annotations.md` §4.2 and **I2** at the artifact. α is derived from the same visible members, so it
  carries nothing extra; a build-time α, or one fitted over full membership, *would* have been a
  disclosure, and is why the derivation is per request.
- Every vertex is a visible member's position, exactly as before.
- The result is a **subset** of the convex hull, so it says strictly less about where the members a
  principal cannot see are sitting.

Nothing in that lets a viewer learn about data they were not served. The register is not touched.

## Negative results and refuted approaches

- **α from a nearest-neighbour distance quantile: refuted, modelled not measured.** For a uniform
  planar sample the convex hull's edges run about `7·n^(1/6)` times the mean nearest-neighbour
  distance — the hull's vertices are sparse where the sample is not — so an α set from
  nearest-neighbour spacing digs a genuinely convex cloud all the way down to its sampling scale and
  spends the whole budget doing it. The wrap's own edge distribution is the right statistic because
  it is measured on the same object α is applied to.
- **Nearest to the *line*, without the projection restriction: refuted by measurement** on the moon
  fixture. The unrestricted minimiser is routinely a member just past an endpoint, lying along the
  arm the edge springs from — it was already a boundary vertex, so every dig was refused and the
  shape stayed convex. It also carves a sliver along the edge rather than into the void the edge
  bridges.
- **Excluding members that lie on the edge being dug: refuted.** They sit on the boundary the dig
  moves inwards, so containment broke on lattice-aligned data. They are now the nearest candidate
  there is, and digging to one carves a triangle of zero area — the shape does not change and the
  edge gains that member as a vertex, at the cost of one from the budget. On real positions a third
  member exactly on the line through two others is a fluke of quantisation; on a synthetic
  axis-aligned cloud it is common, and there the shape degrades towards its wrap rather than
  breaking.
- **A Delaunay-based α-complex: not pursued.** It is the textbook construction and it does not fit
  the wire: its boundary is a set of edges that may be disconnected and may enclose holes, and the
  artifacts frame carries one ring per artifact. It also needs a triangulation this crate does not
  have and a dependency the workspace does not carry.
- **A raster shape — occupancy grid plus a boundary walk: declined.** It is the cheapest correct
  option and its vertices are cell corners, not member positions. That draws area no member
  occupies, which is the same objection that keeps a degenerate hull a point rather than rounding it
  up to a triangle, and it costs the simplest form of the disclosure argument above.

**A known limitation, stated rather than fixed.** A concavity whose flanks are flush with the edge
bridging it cannot be dug: the members bounding the gap project onto the edge's endpoints, so no
candidate qualifies. A comb of teeth flush with the wrap's base is the clean example. Digging works
on gaps whose interior is *visible* from the edge that spans them, which every concavity in the
measured layer is.

## Not measured

- The shape under a **mask**. Everything above is full membership, which is the largest input the
  derivation takes; a masked one gathers fewer positions and digs a smaller cloud.
- The wire cost **in a real response**. The 8 bytes per vertex above are the `hull_x`/`hull_y` values
  alone, not the Arrow list offsets and validity, which do not move with the shape.
- Anything on a second corpus. One layer of one corpus, and the shapes are what that clustering
  produced.
