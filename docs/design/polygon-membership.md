# Shape membership — requirements and design

**Status:** **Normative — 2026-08-31 (r10).** Designed, taken through one adversarial review
(Appendix R, r4), owner-ruled (§13), and **built in four stages on 2026-08-29** (§12), each in its
own worktree and gate-green: the core, the artifact type, the wire and client, the region leaf.
**`wgs84` shapes are built** (§4.3): a view now declares a projection, and a shape declared in
longitude and latitude goes through the view's own transform, each edge densified first — the last
of R9 to land. **§4.3's spanning is built** (r10, 2026-08-31, decision 0111): a layer's geometry is
canonicalised once per view, in that view's own frame, on both entry points. The figures in §9 are
measured on one Overture part; the world-scale figures stay modelled and say so. What remains unbuilt is scoped out in §11 — saving a selection as a shape
waits on the edit pass, and the export verb — and each is marked ⊘ at its claim. §2 is the
requirements set the owner directed on 2026-08-27, unchanged.

The document is named for the polygon because that is the consumer that forced it; the design is
for **shapes** — box, circle, ellipse and polygon — with one semantics (§4.1).

**Required by** [`../evidence/memos/2026-08-27-ingest-campaign-plan.md`](../evidence/memos/2026-08-27-ingest-campaign-plan.md) §9.
**Depends on** [`projections.md`](projections.md) §10 for the WGS84 half of R9 (§4.3) and on
[`selection-operand.md`](selection-operand.md) for the decomposition it shares (§5).
**Reads with** [`annotation-representation.md`](annotation-representation.md) §2.0 (a spatial
predicate stores no membership and never goes stale), `crates/tessera-engine/src/shapes.rs` (what
a spatial level holds and when it is built), [`artifact-shapes.md`](artifact-shapes.md) (the derived
hull, whose wire and ask conventions §7 follows) and `architecture.md` §4 and Appendix C.

---

## 1. Why this exists

Three consumers in current work need the same operation — selecting the points inside a polygon,
efficiently enough to answer inside a request:

1. **Published boundary sets.** Administrative geographies with many polygons at several nested
   levels (ONS output areas through to local authorities; Overture's nine division subtypes).
2. **Regions defined at request time.** A viewer drawing an area and asking what is in it.
3. **The join the writer is currently obliged to do offline**, stamping each point with the code of
   every region containing it, because the server cannot answer the question.

## 2. Requirements

**R1.** An artifact's membership may be declared as a polygon.

**R2.** Membership is **exact**: the members are the points inside the polygon and no others. A
membership wider than the declared shape is not acceptable at any tolerance.

**R3.** Membership does **not go stale**. A point ingested inside the polygon is a member on the
next request, with no refresh, republication or rebuild — the guarantee `bbox` spatial membership
gives today.

**R4.** Every quantity served from such a membership is computed from inside the viewer's own
visible set alone (§4's **I2**), and the shape is not a route by which a count, extent or shape
reveals a point outside it.

**R5.** Per-request cost is **bounded and declared**. A layer carrying many polygons, or a viewport
intersecting many of them, must not be able to produce an unbounded response or an unbounded
evaluation.

**R6.** A polygon may be **replaced or edited without re-ingesting the points it contains.**
Published boundary sets are revised, and the cost of a revision must fall on the boundary rather
than on the corpus.

**R7.** A layer may carry **many polygons across several levels**, with the containment between
levels expressible.

**R8.** Polygons are declared in the **view's own coordinate system**, on the same frame as the
points they select.

**R9.** A polygon is declared in **either WGS84 or the view's projected space**, and which of the
two is part of the declaration. A caller is not obliged to convert in order to submit.

**R10.** The space a polygon is declared in **defines the plane its edges are straight in**. The two
readings are not equivalent — an edge that is straight in one is a curve in the other — so this is
what the caller is choosing when they choose a space, and not an encoding detail.

**R11.** Transforming a polygon from the space it was declared in **does not change which points it
contains**, beyond the resolution of the grid the points are stored on.

**R12.** A declaration that does not match the coordinates supplied does not silently produce a
wrong membership.

**R13.** All three consumers in §1 are served. Request-time regions and published boundaries are the
same question asked at two times, and a capability that answers only one of them does not meet this
requirement.

## 3. The design in one paragraph

A polygon is **a further shape kind on the spatial membership that already exists**, not a new
kind of artifact. `membership = "spatial"` today means *the rows inside this shape*, resolved
against each generation's own segments and stored nowhere (`ranges.rs`); `bbox` is the one shape it
can resolve, and it resolves it as a tile cover at a declared depth. Every shape now resolves
**exactly**: tiles wholly inside the shape are whole row ranges, and the rows of the cells the
boundary crosses are tested one by one against the stored 32-bit position — the decomposition
[`selection-operand.md`](selection-operand.md) §3 specifies for a drawn lasso, lifted into one
module that needs two things of a shape (classify a tile, test a point) and called from three
places. The box loses its depth, and a circle and an ellipse join the polygon because they answer
the same two questions in the same integer arithmetic. A shape travels to the client as rings in
the artifacts frame, on the derived hull's conventions, and is drawn where the hull would be.
Filtering *inside* or *outside* a shape is the selection operand's `region` leaf, which gains a
second spelling naming a published shape by its `tessera_id` instead of by its geometry; `none_of`
over it is *outside*. Nothing in this adds a leak-register row, for the reasons selection-operand §7
already gives.

## 4. The geometry

### 4.1 The model: four kinds, one semantics

A **shape** is one of four kinds, and every kind means the same thing — *the rows whose stored
position is inside it*, exactly:

| kind | parameters | inside |
|---|---|---|
| `bbox` | `min_x, min_y, max_x, max_y` | closed on every side |
| `circle` | `cx, cy, r` | `(x−cx)² + (y−cy)² ≤ r²` |
| `ellipse` | `cx, cy, a, b, angle` | the same test in the ellipse's own frame, `angle` in degrees anticlockwise from the x axis |
| `polygon` | an OGC `MultiPolygon` | the even-odd rule, below |

**A polygon is a `MultiPolygon`** in the OGC Simple Features sense: one or more parts, each a
sequence of rings, each ring at least three vertices and implicitly closed. A `Polygon` is a
`MultiPolygon` of one part. Holes are rings after the first of a part; enclaves and exclaves — an
authority's detached territory, a hole in a county for a city — are what ONS and Overture data
both carry (38,475 of Overture's 1,074,177 divisions are multipolygons), so both are required by
consumer §1.1 rather than optional.

**Inside a polygon is decided by the even-odd rule over every ring of every part, and a point on an
edge is inside** (ruling (a)). This is the rule `clients/ts/core/src/region.ts` implements for the
lasso and the one selection-operand §3 fixes for the leaf; a published polygon uses the same
predicate so that the three callers of §5 cannot disagree. For an OGC-valid geometry — rings that
do not cross, holes inside their outer, parts disjoint — even-odd and non-zero winding give the
same answer. For an invalid one they differ, and the design takes even-odd as the definition rather
than refusing the input: a self-touching ring or two overlapping parts still **answer**, and are
reported at publication (§6.5), where the alternative is a refusal about a shape that leaks nothing
and whose owner can see the report.

**The box becomes exact and loses its depth** (ruling (g)). The cover-at-depth-*d* form existed
because a box was the one shape the service could resolve without a boundary test, and the depth
was declared as the membership so that the cover would not be mistaken for the box
([R3](../evidence/memos/2026-08-21-artifact-serving-scale-review.md): *the ranges are the
membership, the polygon is content*). With a boundary test the substitute is not needed, and
keeping both forms would give "inside a box" two meanings. Under decision 0048 the depth form is
deleted rather than carried; R3's ruling is superseded at the site that recorded it, and its
successor is this section: **the shape is the membership, exactly, for every kind.**

**Circles and ellipses exist because they cost nothing to add and a fitted cluster wants one.**
`annotations.md` §8.6's point-and-radius case anticipated them as *supplied content* — a drawing —
and nothing built either. Both are one conic in grid units — an extent whose axes scale
differently turns a circle into an ellipse and a rotated ellipse into a differently rotated one —
and the decomposition needs only its quadratic evaluated at a point and bounded over a tile. That
quadratic is evaluated in correctly-rounded `f64`, not integers: the matrix is not integral once
the extent has scaled it. It is deterministic across platforms on the licence `artifact-shapes.md`
§1 takes for the hull's two floating steps, and it is not exact at the last bit, which the tests
state by skipping positions within rounding of the curve rather than pretending otherwise. A radius
densified to a polygon by the caller would answer nearly the same question at a thousand times
the vertices; the parametric form is exact and five numbers. No curve beyond these: a further
family would be a further implementation of the same two questions, and nothing in §1 asks for it.

**Membership or content is the layer's choice, per shape, and the geometry does not know which it
is** (ruling (h)). A k-means centre-and-radius as *content* over an enumerated membership says
*here is the fitted blob*; the same circle as *membership* says *every point within r of the
centre*, which is not k-means. One module serves both; the difference is which field the row puts
the shape in, and both draw through one client path (§7). **What the field decides is the gate.** A
membership shape is served under the artifact's own verdict and nothing else, so declaring one is
declaring it **corpus-independent** — a boundary, not a fit. A shape fitted over members some
viewer cannot see belongs in *content*, where `require_member_visibility = "all"` exists for it
(`annotations.md` §8.6). The service cannot check which a shape is; the declaration is the
caller's, as §8.6 already says, and this sentence is where the design says it too.

### 4.2 Formats: WKB in the table, WKT inline, rings on the wire (ruling (c))

The **input** formats are the two OGC encodings every GIS tool writes, so that a boundary set is
handed over as it is distributed rather than converted:

- In an artifact **table**, a column of **WKB** — the GeoParquet convention, so Overture's
  `division_area.geometry` and an ONS boundary file exported from QGIS or DuckDB's `ST_AsWKB` are
  consumable as they are. The default column name is `geometry`, GeoParquet's own, overridable
  through the layer's `fields` map like every other canonical name.
- **Inline** in `corpus.toml`, `wkt = "POLYGON ((…))"` — the same text every tool prints, on the
  argument `configuration.md` §1 already makes for inline rows being the canonical spelling.

GeoJSON is not an input format. It mandates WGS84, which R9 makes one of two spaces, and its
coordinates in a projected frame are a specification violation the reader would have to decide to
tolerate; WKB and WKT are CRS-agnostic and the space is declared beside them (§4.3). A caller
holding GeoJSON converts with `ST_GeomFromGeoJSON` in the one tool they already use to prepare the
table.

A box, a circle and an ellipse are their parameters — columns named for them in a table, an
inline array in TOML — since no OGC encoding carries a curve without densifying it.

The **stored** form is none of these. It is the engine's own canonical geometry (§4.4) — parametric
for the three closed forms, so a circle stays exact — in the artifact's record blob, and the
**wire** form is Arrow list columns of grid-unit vertices (§7) for every kind, which is what the
hull already is. Serving GeoJSON is a job for the export verb, and is not here.

### 4.3 The space: a property of the submission, never of the layer

The space a shape is written in is a fact about **the bytes being submitted**, and it is consumed
at the boundary: whatever came in, the stored form is grid units (§4.4). So it is declared beside
the geometry rather than on the layer — a layer that said *this boundary set is in Mercator for
ever* would be encoding an input-side fact into storage configuration, and every later submission
would inherit a statement about a file it did not come from. Three places, one vocabulary:

```toml
[layer.artifacts]
path          = "divisions.parquet"
default_space = "view"            # the fallback for rows carrying no `space`; "view" if absent
```

- an artifact **row** may carry `space`, overriding the table's `default_space`, on the same
  register `[layer.artifacts]` already is — file paths, field maps, ingest-time facts;
- `PUT /control/layers` takes a batch-level `default_space` in its body and a per-row `space`
  column, identically;
- each `region` **leaf** carries its own `space`, `view` if absent;
- an **inline** artifact row (`[[layer.artifact]]`, which has no table to carry a default) may
  carry `space`, `view` if absent.

**A shape layer may span several views, and its geometry is declared once** (owner, 2026-08-29;
widened 2026-08-30, [decision 0111](../decisions/0111-a-shape-spans-projected-views-through-wgs84.md)).
The membership is resolved **per view**, because each view has its own row space: a view's rows
are tested against the shape in that view's frame, so a point carried in two views is in
whichever shape contains it *there*, independently. The views need share **neither the extent
nor the projection**: a `wgs84` shape is densified, projected, clipped and decomposed through
each view's own declaration — the same function that placed that view's points, run once per
view — so a Mercator view and an equirectangular one hold the same boundary, each in its own
frame. What cannot span is a `view`-space shape over unequal frames: its coordinates are one
specific frame's, so on a layer whose views do not share projection *and* frame it is refused
naming the reason, and `wgs84` is the spelling that spans. A layer's views are **all projected
or all `none`** — `wgs84` means nothing in an embedding, so no geometry spans the two kinds of
space, and the mix is refused at the layer declaration. Two `none` views sharing a layer are the
caller's own assertion that their spaces agree — spanning is opt-in, and no warning second-guesses
it; a layer scoped to a view group spans frames identical by construction. **A shape wholly
outside a view's extent is a warning, never a refusal**: its membership there is empty, the count
is reported beside the clip counts, and the operator decides — a wrong extent costs a rerun, not
a mission. One consequence owned rather than hidden: two projected views of one geography can
disagree about a boundary point, each testing the shape against its own quantised stored
position; that is the semantics — exact per view — and the divergence is bounded by quantisation.

On `PUT /control/layers` a row whose `space` the view cannot honour is `422` naming the row,
**whole batch without effect**, on the contracts convention.

`view` is the space the points are stored in — the quantisation frame, whatever produced it.
`wgs84` says the coordinates are longitude and latitude and asks the view to project them **with
the same function it projects points through**, which is what makes the two spaces comparable at
all: the projection is read off the view's own declaration and is not a parameter of the
submission, so a shape placed by a function the corpus was not placed by is not a thing a caller
can write (R12). Under `projection = "none"` a view has one space, `view` is the right word for
it, and `wgs84` is refused naming the view's own declaration — that did not change when
projections landed, and will not.

**A `wgs84` shape is densified before it is projected** (`projections.md` §10, R10): an edge
declared in longitude and latitude is straight in the longitude/latitude plane, so its image in
the frame is a curve, and each edge is subdivided until no point of that curve departs from the
polyline by more than **one depth-16 cell** — one cell of the grid the view's own points are
stored on, whatever zoom offset its frame sits at, which is the error R11 permits. A circle or an
ellipse in `wgs84` is densified to a polygon by the same rule, since a projected circle is no
longer one. A box is not: every projection in the set is cylindrical, so a meridian is a vertical
line in the frame and a parallel a horizontal one, and a box in degrees is a box. What leaves the
transform is a shape in the view's coordinates, which §4.4 then clips and quantises exactly as it
does one that arrived in them. A `wgs84` coordinate outside ±180 × ±90 is not a coordinate and is
refused (`projections.md` §2).

R12's other half — a `view` shape that was *written* in degrees — is not detectable in general
and is not refused. It is **reported**: a table whose every coordinate lies within ±180 × ±90 on a
view whose extent is not, is named in the build report with the count, and the build proceeds.
⊘ That report is blind on a **projected** view, whose frame is `[0, 1]` and therefore itself
inside ±180 × ±90, so a degree-looking table is never named there; the R12 half that matters on
such a view is the declared `space`, which is checked rather than guessed.

A shape partly outside the view's extent is **clipped to the extent and reported**, on the rule
that bounds warn and never exclude (`contracts.md` §3.4). A shape wholly outside holds no rows, is
published, and is reported beside the clipped ones.

### 4.4 The canonical form

Before anything tests or serves it, a shape is put into one form, and that form is what is stored.
A box, a circle and an ellipse quantise their parameters to the grid (a radius and the axes in grid
units, the angle as it was given) and are otherwise already canonical. A polygon:

1. Every vertex is quantised to the view's 32-bit-per-axis grid — `fixed32`, the same function
   the tiler applies to a point — so the shape lives on the grid the points do and a test between
   them is integer arithmetic.
2. Consecutive duplicate vertices and exactly collinear runs are removed; a ring left with fewer
   than three distinct vertices is dropped and reported; a part left with no rings is dropped.
3. Rings are oriented outer-first counter-clockwise, holes clockwise — presentation only, since
   even-odd does not read orientation — and parts are ordered by their lowest vertex, so two
   submissions of one shape store one byte sequence.
4. Each vertex is given its **simplification weight** (§7.2), computed once here.

**A position arrives as `float32`** — the ingest body's `x`/`y` pair (`contracts.md` §3.4) and the
parquet a build reads — and is quantised from that. So a shape smaller than `f32`'s resolution at
the coordinate's magnitude (about 2 m at Web Mercator's edge, ~128 grid units on a world extent)
may not hold the point it was drawn around: the point's *stored* position is what membership is
of, exactly, and three of Overture's division polygons a few metres across hold none of theirs
for this reason (§9). Not a defect of the shape; a fact about the input to state beside it.

Every arithmetic step after quantisation is exact in `i64`/`i128`, on the argument `artifact-shapes.md`
§1 makes for the hull: a shape that is a function of its input alone is one every platform and every
principal computes identically. Selection-operand §5's cache key is a digest of exactly this form,
which is the point of having one.

## 5. The shared core, and its three callers

One module, **`tessera_spatial::shape`**, owns the geometry: the model, the WKB and WKT readers,
canonicalisation, the decomposition against the Morton grid, and the point test. `tessera-spatial`
already owns quantisation and `tiles_for_bbox`, so this is the crate the bbox path already calls
into, and the module has no dependency on the store or the engine — it takes a shape and an extent
and yields tiles and predicates.

**The decomposition asks two questions of a shape and nothing else** — *classify this tile*
(disjoint, wholly inside, or crossed) and *is this point inside* — so it is written once over a
`Region` trait and the four kinds are four implementations of those two methods, each in exact
integer arithmetic. A box answers both by comparison; a circle and an ellipse by one quadratic,
bounded over a tile by its nearest and farthest corners in the shape's own frame; a polygon by the
edge lists and corner parity below. Nothing downstream of the trait knows which kind it holds,
which is what makes "the same semantics" a construction rather than a promise.

The **descent** is selection-operand §3 verbatim, and that section stays normative for it once
promoted: a descent from the root that discards a tile disjoint from the shape, emits a tile
wholly inside as an **interior tile**, and refines a tile the boundary crosses, carrying down the
edges that cross it and the parity of one corner, until depth 16, where a crossing tile is a
**boundary cell** — for a polygon, holding its own handful of edges and its corner's parity; for the
three closed forms, holding nothing, the test being the shape's own quadratic or comparison. Its
output is a function of `(canonical shape, extent)` and of nothing about the corpus. The test for a
row in a boundary cell is the kind's own predicate over the row's exact position recovered as
`unsplit32(cell, residual)` — four bytes read per row.

Three callers, and the reuse is structural rather than a matter of discipline because each one
holds the same two outputs:

| caller | when the decomposition runs | what consumes the interior tiles | what consumes the boundary cells |
|---|---|---|---|
| **a published shape** (§6) | once per publication, held beside the artifact | row ranges per generation, as `ranges.rs` does for a box | rows tested once per segment, held as a bitmap |
| **a region leaf by vertices** (selection-operand) | once per request, cached by digest | the same ranges, into one `FilterRows::Complete` | rows tested under the mask, per request |
| **a region leaf by artifact** (§8) | not at all — the published decomposition is reused | the artifact's own held ranges | the artifact's own held bitmap |

The offline classification the campaign does today (`test_corpora/overture/prepare.py`'s DuckDB
point-in-polygon join) is the same test run outside the service, and becomes optional: a boundary
layer declared with a shape needs no member table. The `tessera check` report gets the
polygon statistics of §6.5 so the operator can see what the service now does in the writer's place.

## 6. The artifact type

### 6.1 The declaration

```toml
[[layer]]
name       = "admin"
membership = "spatial"
hierarchy  = { kind = "nested" }
views      = ["map"]

[layer.shape]
kind = "polygon"                  # bbox | circle | ellipse | polygon

[layer.artifacts]
path          = "divisions.parquet"   # key, geometry (WKB), parent, name …
fields        = { geometry = "geom" } # only where the source disagrees
default_space = "view"
```

`[layer.shape]` names the kind and nothing else. **`depth` is deleted** (ruling (g)): every kind is
exact, so there is nothing for it to hold, and one written is refused naming this section.
`disclosure.json`'s membership spelling becomes `spatial:<kind>`.

Each artifact row carries its geometry in the kind's own fields — `geometry` (WKB) in a table or
`wkt` inline for a polygon; `bbox`, `circle = [cx, cy, r]` or `ellipse = [cx, cy, a, b, angle]` for
the others, with the same names as table columns — and optionally `space` (§4.3). A row with no
geometry, or with one that canonicalises to nothing, is **published and reported**: it is an
artifact with no members, which is a state the service already has (a spatial layer with no shape)
and not one that leaks.

A shape is also a **supplied content type** (ruling (h)): `[[layer.content.supplied]]` takes
`type = "polygon" | "circle" | "ellipse"`, and all three are read, canonicalised, stored and served
exactly as a membership shape is, and drawn through the same client path; what differs is that they
select nothing. The value in the content slot is WKT for a polygon and the numbers of the kind's row
field for a circle (`cx, cy, r`) or an ellipse (`cx, cy, a, b, angle`); at publication it goes
through the same reader, report, vertex cap **and space** as a membership shape, and the slot then
holds the canonical per-view bytes (§6.6). The space is the row's own (§4.3) — a drawing and the
membership beside it are one producer's geometry in one coordinate system, and a service that
projected the second and not the first would place a ±180 × ±90 outline in a corner of a projected
view's `[0, 1]` frame, with R12's degrees-looking report structurally unable to name it there. A
`polygon` content is never an opaque string: the wire serves its rings and the slot itself is
served blank. A layer declares at most one authored shape, and
none beside a `hull` or a membership shape (§7.1).

### 6.2 What a spatial layer may now declare

`LayerDeclaration::validate` today refuses a `spatial` layer supplied content, computed content,
`depends_on`, `levels` and any hierarchy but `flat`, on the argument that a predicate's artifacts
"carry their key and nothing else". That is true of an `attribute` predicate, whose artifacts are
the distinct values of a column and have no row to hang anything on. It is not true of a spatial
layer, whose artifacts are **published rows** — each has a key, a shape and, in every boundary set
in the ladder, a name and a parent. R7 requires the hierarchy; consumer §1.1 requires the name.

So the refusals are **lifted for `spatial` and kept for `attribute`**: a spatial layer may declare
supplied content, computed content (the comment in `validate` already observes that ranges make it
cheap), `depends_on`, `levels` and any hierarchy kind. What stays refused on a spatial layer is a `proportional` criterion (unchanged). The
`layout` pin is **no longer refused**: its refusal rested on a shape having no per-row source,
which was true of a tile cover and is not true once the flush tests every row (§6.3) — the
flush's output *is* a per-row source, and the pin selects between the same forms it selects
between for an enumerated layer. Every kind gets the lift; nothing about a box made the restriction
necessary either (ruling (b)).

Containment edges on a shape layer are **declared** — `parent` on the row, or the lineage a
`nested` layer's rows carry — and are **not derived from, or checked against, the geometry**.
Overture's own note is the reason: its polygons are generalised for cartography, so a point can lie
inside a locality and outside the locality's county, and a service that derived the tree from
containment would build a different tree from the publisher's. The build **reports** a child whose
canonical bounding box escapes its parent's, with the count, and proceeds.

### 6.3 What is held, and when it is built

`ranges.rs` today derives a level's ranges on the request thread at the first request after every
flush, and answers candidacy by probing every artifact in the level and a masked count by one
`count_range` per range. The review held that against the owner's requirement — **10⁴ shapes
tested across 10⁶ visible points inside one request** — and against Overture, where a country is
10⁴–10⁵ ranges and `levels: "all"` puts every one of 1.07M artifacts in the scan. Neither survives.
So this section does not inherit `RangeSets`' shape; it replaces it with three held structures,
and moves their construction off the read path.

**Per artifact, for the artifact's life** — built at publication, replaced at republication:

- the **canonical shape** (§4.4) and, for a polygon, its **edge table**, indexing the canonical
  `u32` vertex array rather than copying it, so that a held polygon costs its vertices once.
  ⊘ *Partially implemented:* the canonical shape and the decomposition are held
  (`tessera_engine::shapes`); the edge table is rebuilt from the held vertices at each resolution —
  O(V) per artifact per flush, against the O(V·16) descent already paid once — because the table
  borrows the vertex array and holding both in one structure is a self-reference the code does not
  yet express;
- its **decomposition** (§5): interior tiles, and boundary cells as **cell code and corner parity
  only** — 4 bytes and a bit per cell, never the per-cell edge list. A cell's edges are re-derived
  from the held edge table when a segment first puts rows in that cell, which is a bounded walk
  of the edges meeting one cell and is paid once per (cell, segment).

**Per segment — the membership of every row in it, resolved at the flush**, in the write executor,
before the generation is published, never lazily on a request. The interior tiles' row ranges are
members whole; the rows in boundary cells are tested one by one against the shapes whose boundary
cells contain them (the index below says which). **The output is a per-row source** — for each
row, the artifacts it is in — which is exactly what an enumerated layer's member table is, and it
is written into **the serving layout the existing pick chooses** ([decision 0094](../decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)):

- **`rows`** — one run-optimised Roaring bitmap per artifact in view row space, interior rows as
  runs and boundary rows as scattered entries, at O(containers touched). Right for a level of
  few, large shapes.
- **`column`** — one label per row, 4 B, where the level's shapes partition the corpus, which a
  boundary level almost always does. **This is the form that scales**: the delivery record's row 7
  measured artifact-major at 10⁹ points as 78.5 GB against 4 GB for this, flat in the artifact
  count, and it is the only layout that fits a level of 10⁶ shapes over 10⁹ points.
- **`list`** — a list of labels per row where the shapes overlap.

The masked count is then whatever the layout's existing path computes — one `and_cardinality` per
artifact under `rows`, the row-major histogram over `column ∩ M_auth` under the other two — and the
first draft of this section, which fixed a bitmap per artifact, was fixing the layout the layout
machinery exists to choose. **Per generation** the per-segment pieces are joined with the row bases
applied — an O(containers) union for `rows`, a concatenation for the columns.

**Per level — a spatial index over the artifacts.** Each artifact's bounding tiles at depth 8,
geometry-derived and principal-independent, mapping tile → artifacts
(`tessera_store::derived::ShapeIndex`; eight because a depth-8 tile is 1/256 of the extent on each
axis, so a country meets tens and a locality one, at 65,536 keys). It is read at every
**resolution**, so a segment is tested only against the shapes whose bounds it meets. A request's
candidacy is answered by the row-extent tile index the enumerated layers use, built over the
resolved membership — the same question, asked of the rows the shapes resolved to rather than of
the geometry; ⊘ the geometry index is not consulted on the request path, and the enumerated
index's `everywhere` set is what a country-sized shape lands in.

With those, the request path is bitmap arithmetic and nothing else:

| quantity | cost |
|---|---|
| candidacy | O(viewport tiles) index lookups → the artifacts near the viewport |
| masked count | under `rows`, one `and_cardinality(artifact ∩ M_auth)` per candidate at O(containers touched), ~15 containers for a country over 10⁶ points; under `column`/`list`, the row-major histogram the enumerated layers already use |
| declared size | the bitmap's cardinality, held; never on the wire (§10) |
| the leaf by artifact (§8) | the held bitmap, into `FilterRows::Complete` |

The existence criterion, the frontier, the cut and the layout are untouched, because they consume
a masked count and a candidate set and do not ask where either came from. The per-request bound
is **O(artifacts near the viewport × containers per artifact)**, and the Overture zoom-0 request
that served 8,448 divisions is the first number stage 2 measures against it.

**R3 holds, and it holds because the flush builds before it publishes.** A point ingested inside a
shape is in the new segment's bitmap when the generation that carries that segment is published,
so it is a member on the next request with nothing rebuilt on that request. The rebuild is
incremental because the held structures are **keyed by segment**: a flush resolves only its own
segment, against decompositions that are segment-independent, and a boundary row is tested once
in its life. A fold renumbers rows and re-resolves **everything** — 10⁶ shapes, every boundary cell
whose rows moved, every row in one — and that cost sits inside the fold's artifact pass, inline
as `annotation-representation.md` §5.0.3 requires and sized against compaction §9's nightly
window. **What the build and the fold resolve, they persist** (owner ruling 2026-08-29), in the
layout's own form and beside the other derived files decision 0094 governs: the row-major column
where the level is `column` or `list`, and a `shape-rows` row form — one run-optimised bitmap per
artifact of the segment's rows — where it is `rows`, each keyed by the segment id, the segment's
row count and the level version. The decomposition is persisted too, per artifact per view in one
`shape-held` file per level, each entry carrying the length and a digest of the canonical bytes it
was descended from — because on one Overture part the descent was 8.9 s of a 9.3 s open once the
pieces were claimed, and the decode alone is 76 ms. **The engine's open claims those files and
resolves only the segments no file covers** — the ones flushed since the fold that wrote them,
each of which its own flush already resolved on the pool, so at steady state that is zero or a few
small pieces. Every key is an equality test and a file that fails one is refused, never adapted
(I11), the shape decomposed or the segment resolved again from the geometry, and the refusal
said at `warn`; a segment no file names — a flushed one — is said at `info`. A publication into a
layer moves its level version past every file the prefix holds for it, so until the next fold an
open re-resolves that layer alone. A merge resolves its merged segment before publishing it, the
consumed segments' pieces not carrying over. Every route that
introduces a segment resolves it before the swap; a request finding one unresolved resolves it on
the request path and warns, so the fallback keeps R2 and the hooks keep the cost budget.

**Small shapes are all boundary, and that is the enumerated layout arriving by another door.** A
neighbourhood two cells across, or a cluster's radius at a fine extent, has no interior tile; every
member is a tested row and its bitmap is scattered rows. A county is runs and a thin scatter. The
structure adapts to the shape without a route chooser, and a request cannot tell which it is.

### 6.4 Replacing a shape

R6 is met because the shape is a property of the artifact row and the membership is derived from
it: **republishing the row with the same key** replaces the shape, the next generation resolves the
new decomposition, and no point is touched. ⊘ Until the edit pass lands (decision 0077 defers it),
the route is the one every artifact has — suppress, then republish — slower and not weaker.

At **ingest** (`PUT /control/layers`, decision 0091) the row shape is the build's: the kind's own
columns, `default_space` and `space` (§4.3), the same canonicalisation, the same reports returned in
the response body. A shape published at ingest is resolved at the next generation like one published
at the build.

### 6.5 What the build reports

**The decomposition is corpus-independent, so its size is known before any memory is committed to
it**: `tessera check` computes it from the geometry alone and reports it beside the layer, before
a build, and that is where an operator sizing a world-scale boundary set reads it. Per shape layer,
in that report and in the build's: artifacts; parts and rings;
vertices before and after canonicalisation; polygons clipped to the extent, wholly outside it, or
canonicalised to nothing; rings dropped; **degrees-looking coordinates under `space = "view"`**
(§4.3); rings that self-touch or parts that overlap (§4.1); children escaping their parent (§6.2);
and the decomposition's size — interior tiles and boundary cells, total and maximum per artifact —
because that is the number §9 needs and nobody can estimate from a vertex count.

None of these refuses a build. What does: a coordinate that is not one — non-finite, or a `wgs84`
value outside ±180 × ±90; a `space` the view cannot honour, which is `wgs84` on a view that
projects nothing (§4.3); an inverted box; a polygon over the publication vertex cap (§9); and a
circle or ellipse with a non-positive radius or axis — each the caller's own arithmetic with a
one-line fix.

### 6.6 Storage

The record blob's `shape` tail (`membership.rs`, `encode_record`) gains a variant:

```text
shape := u8 0                                       -- none
       | u8 1 | 4 × u32 LE                          -- bbox: min_x, min_y, max_x, max_y
       | u8 2 | 2 × u32 LE | 3 × f64 LE             -- conic: cx, cy, m11, m12, m22
       | u8 4 | u16 LE parts                        -- polygon
               | per part:  u16 LE rings
               | per ring:  u32 LE n | n × (u32 LE x, u32 LE y, u32 LE weight)
```

A circle and an ellipse are stored as the one conic the grid holds (§4.1), so the blob carries no
caller parameter that the extent has already transformed. Every coordinate is the canonical
form's grid units; no quantisation happens at serve; the box's four `f64`s are replaced, not kept.
The blob wraps the bytes per view — `u8 3 | u16 views | per view: u16 len | view | u32 len |
shape` — since a layer spanning several views stores one canonical form for each (§4.3); tag 3 is
the blob's and the shape module leaves it unused. The `weight` is §7.2's simplification rank. A supplied content shape (§6.1) is the same encoding in the content
slot. Under decision 0048 this is a format change made freely — the artifacts are recreated — with
`bundle_format` bumped so a stale local bundle refuses loudly rather than decoding one kind as
another.

**Two derived files beside the record** (§6.3), written by the build's artifact pass and by every
fold under the naming rule every derived kind follows (`tessera_store::derived`), named in the
side-manifest and claimed at open under equality on every key:

```text
shape-rows  TSSR | u16 version | u16 0 | u32 ordinals | u32 segment rows | u64 level version
                 | u16 len | seg_id
                 | per ordinal: u32 len (u32::MAX a hole, 0 empty) | portable Roaring bitmap
shape-held  TSSH | u16 version | u16 0 | u32 ordinals | u64 level version
                 | per ordinal: u32 canonical len (u32::MAX a hole) | u64 canonical digest
                 | u8 has bounds | [4 × u32] | u32 interior | u32 boundary
                 | interior × (u64 prefix, u8 depth) | boundary × (u32 cell, u8 parity)
```

Both are derived: a level without one resolves or decomposes on open, which is what every open
did before they existed. Neither is compatibility machinery — a version bump makes a stale file a
refusal.

## 7. The wire and the client

### 7.1 Columns

The artifacts frame (`contracts.md` §3.2 item 4, r45) carries **`shape_x` / `shape_y`** as
`list<list<list<uint32>>>` — parts, then rings, then vertices — at the tail, where `hull_x`/`hull_y`
stood, on the hull's own rules: **omitted from the schema when no served layer declares a drawn
geometry or the request narrowed it away**, null on an artifact whose layer declares none, never
null otherwise. **Every kind is served as rings** — a box as
its four corners, a circle or an ellipse densified to the request's depth so that no chord departs
from the curve by more than a cell — so the client has one drawing path and no parametric branch;
the parametric form is storage's, where exactness matters, and not the wire's, where a pixel is
the resolution.

**A served ring is a drawing, never the predicate.** It differs from the membership by up to a
cell at the request's depth (§7.2), and a client must never test a point against `shape_x`/
`shape_y` to decide whether it is a member — the `membership:<layer>` column is that answer, and
the only one.

**An artifact has one drawn geometry, and the layer declares which of three kinds it is** (owner,
2026-08-29): **derived** — the hull over the visible members, per principal; **predicate** — the
membership shape, identical for every principal; **authored** — a supplied drawing over an
enumerated or attribute membership, a fitted circle over a k-means cluster being the model's case
(`annotations.md` §8.6). The membership and the outline are distinct questions, but the answer to
*what is drawn* is always one shape, so there is one column pair: **`shape_x` / `shape_y`** carry
whichever kind the layer declared, and `hull_x`/`hull_y` become that pair with the derived kind —
each α-group of a hull its own part with no holes, since a second ring in one part is a hole to a
renderer and two groups are two shapes. `/v1/meta` publishes the
kind per layer so a client knows whether the outline moves with the principal, which decides
whether it may cache the geometry against a `tessera_id`. The gate differs by kind and each is
already specified: derived is over `membership ∩ M_auth` by construction; predicate is served under
the artifact's verdict; authored under its own `require_member_visibility`. A supplied `polygon`,
`circle` or `ellipse` content kind is therefore the *authored* geometry of its layer, read and served
as any shape is, and a layer declares at most one drawn geometry — `hull` and an authored shape on
one layer is refused. `/v1/artifacts/{id}`'s JSON carries the same nesting under the same names
(`docs/openapi/tessera.yaml`, `ArtifactResponse`).  Three levels rather than the hull's
two because a hole and a second part are different things to a renderer, and flattening them into
one ring list would have a client draw a second part as a hole of the first.

It is **not** the hull (ruling (d), as ruling (i) re-cut it: one column pair, three kinds). The hull
is derived from `membership ∩ M_auth` and differs per principal; a polygon is supplied,
corpus-independent content, identical for every principal who is served the artifact. `annotations.md` §8.2 already governs the pairing — *supplied shape, masked
number* — and its rule that a boundary is served whole or absent whole is what makes the shape safe
to serve at all: a viewer below the artifact's criterion is not served the artifact, so there is no
state in which the shape is served and the number withheld, or the reverse. A shape layer does not
declare `hull`; there is nothing for α to summarise that the shape does not already say. A
*content* shape is gated as every supplied content is — by its own `require_member_visibility`,
which for a fitted circle is `all` (`annotations.md` §8.6) — and the derived hull may sit beside it.

The ask vocabulary of `artifact-shapes.md` §8 stays three words, with **`shape` replacing `hull`**:
a hull is asked for by asking for the shape of a layer whose shape is derived. The narrowing rule is
unchanged — a request that does not ask is not served; one that asks on a layer with no drawn
geometry is narrowed, not refused; `422` is for a word outside the vocabulary. The client asks the
viewport for `centroid` and `box` and `/v1/artifacts/{id}` for the one shape it draws — or asks for
every shape at a zoom where it means to draw them all, which the vertex rule below makes affordable.

### 7.2 The vertex rule: presimplified once, filtered per request

A country border is 10⁵ vertices and a viewport at zoom 0 over one Overture part served 8,448
divisions; whole shapes would be tens of megabytes per frame. The hull's answer is a per-artifact
budget spent at derivation. The polygon's answer is the same budget applied at serve, against
weights computed once:

- At canonicalisation, every vertex gets a **weight**: its Visvalingam–Whyatt effective area in
  grid units², computed by the standard progressive removal so that weights are monotone along the
  removal order (a vertex's weight is at least the weight of every vertex removed before it). This
  is the `topojson` *presimplify* construction and costs one pass with a heap at publication.
- At serve, a shape is filtered to the vertices whose weight is at least **one cell at the
  request's depth** — the request already carries the depth; `/v1/artifacts/{id}` takes it as an
  optional `zoom`, and without one serves the whole presimplified shape under the guard — so a
  vertex that would move the drawn edge by less than a cell at that zoom is not sent. The cell is
  the one a screen pixel covers: a depth-`z` tile is 512 pixels wide on the client, so the
  tolerance is the depth-`z + 9` cell's side, `2^(23 − z)` grid units
  (`tessera_engine::shapes::served_tolerance`). A ring reduced below three vertices is
  sent as its two extreme vertices or one, exactly as a degenerate hull is; a shape reduced to
  nothing is a shape the client draws as a point at the centroid.
- A **per-artifact vertex budget of 2,048** — the hull's, for the hull's reason — applies after the
  filter as a guard: a shape still above it at the request's depth is filtered to its 2,048
  heaviest vertices, and the trailer's `stage_ns` companion records that the guard fired — for a
  densified curve as for a polygon.
- **A ring's role survives the filter or the ring does not.** An outer ring that filters below one
  vertex takes its whole part with it — otherwise a surviving hole would be served first and drawn
  as the polygon; a hole that filters below three vertices is dropped, never served degenerate.
  An outer left with one or two vertices is served as its extremes, as a degenerate hull is.
- **Rings simplify independently, so two neighbours' shared border can diverge on screen** at a
  coarse depth — slivers and overlaps a topology-aware simplifier would avoid. Accepted: the
  divergence is under a cell at that depth, and the number beside each shape is unaffected.

The drawn shape is therefore a **generalisation of the boundary at the pixel, and the number beside
it is exact for the boundary** — the reverse of the hull, where the shape is exact for the members
and the members are what the number counts, and it is the ordinary relationship between a basemap's
coastline and a census figure. Simplification is a function of the stored shape and the request's
depth alone, so two principals at one zoom receive one drawing.

### 7.3 What the client does

- **Draws** each part as a polygon with holes — deck's `PolygonLayer` takes exactly that nesting —
  in the outline style the hull uses, the opened artifact strong with a faint fill; names and counts
  at the centroid as today. Nothing distinguishes a shape from a hull in the picture except that a
  shape does not move with the principal.
- **Picks** as it picks a hull: the pick answers the artifact; the card names it and offers *filter
  to this*, which is §8's leaf by id.
- **Keeps `insidePolygon`** for the lasso's live highlight under selection-operand §8's
  obligation, now also the predicate the server applies to a published polygon. That obligation
  gains a fourth part: at a tie — a point on the ray through a vertex, an edge through a grid
  position — the client's rule is the server's symbolic perturbation (`tessera_spatial::shape`,
  `polygon.rs`), stated there once and not re-derived.
- `/v1/meta` publishes the layer's shape kind, so a client knows whether to expect `shape` or
  `hull` for a layer before it asks.

## 8. Filtering: inside and outside

The `region` leaf of selection-operand §2 is the filter, unchanged. It gains a second spelling:

```json
{"region": {"polygon": [[x, y], …], "space": "view"}}   // by geometry, as specified — or bbox, circle, ellipse
{"region": {"artifact": "<tessera_id>"}}                // by a published shape
```

The leaf takes exactly one of `polygon`, `bbox`, `circle`, `ellipse` or `artifact`; the first four
are §4.1's parameters and carry `space` (§4.3), the fifth carries nothing else. A leaf by artifact
names a **membership** shape; a content shape selects nothing (§4.1) and naming its artifact is an
empty operand.

*Inside* is the leaf; **outside is `none_of` over it**, which selection-operand §5 already shows is
well defined because every rowed entity carries a position: the complement of the shape within the
candidate, with a buffered entity matching neither. *Points in this county but not in this city* is
`all_of: [region(county), none_of: [region(city)]]`, and needs nothing further.

**By artifact reuses the held membership and costs no descent**: the leaf's row set is the
artifact's held per-generation membership — the rows bitmap the level's row form holds for it —
into the same `FilterRows::Complete`, so it is cheaper than the same shape sent as vertices, and
it is always **exact** with no cover fallback, because the publication paid the whole
decomposition. The leaf works for any layer whose artifacts have a membership, spatial or not; an
artifact whose layer draws an **authored** shape is an empty operand, its drawing being content
and not a membership (§4.1).

**One gate, and it is the artifact's own.** The leaf is answered only for an artifact this
principal would be **served** — the same verdict `ArtifactView::verdict` gives before any content
reaches a response. Otherwise the leaf is an **empty operand**, identically for an id that does not
exist, one that is suppressed, one on a layer the gate refuses, one on a view other than the
request's, and one below this viewer's criterion — the C17 posture, the same one-set-probe rule
the layer registry keeps. Timing separates *unknown* from *withheld by criterion* — the second
costs the masked count the verdict needs — exactly as `/v1/artifacts/{id}` does today; C4's family,
no new channel. Without it a
viewer below a boundary's criterion could filter their own points through a shape they were not
served and read its outline back from where their marks disappear; with it, a shape shapes nothing
for a viewer who cannot see it. `annotations.md` §8.2's *absent whole* rule, applied to the operand.

## 9. Cost and bounds (R5)

Costs are stated as selection-operand §4 states them. The table is the model; the paragraph
that follows it is what stage 2 **measured** on one Overture part, and the world-scale figures
below that stay modelled. Let a polygon have *V* vertices and perimeter *p* in depth-16 cells.

| where | cost | bound |
|---|---|---|
| publication: canonicalise, weight, decompose | O(V log V) + **O(V·16 + p·16)** — every edge is tested against the children of every tile it meets at every depth | **`max_shape_vertices`**, a deployment constant published on `/v1/meta`; over it is a `422` naming the count and the cap, because a vertex count is the caller's arithmetic and `ST_Simplify` is the fix. Default proposed 10⁶ (ruling (e)) |
| held per artifact (§6.3) | 12 B per vertex canonical + the edge table's indices; ~5 B per boundary cell; 16 B per interior tile; the per-segment and per-generation bitmaps | reported by `tessera check` **before a build** (§6.5), gauged at serve, **not capped** (ruling (e)) |
| per flush | one segment: O(p) range probes per shape whose tiles it touches; the new rows in boundary cells tested once, O(edges meeting the cell) each, with the cell's edges re-derived once per (cell, segment) | in the write executor, before publish; a flush's cost, reported in its trace |
| per fold | everything re-resolved: 10⁶ shapes against every segment | inside the fold's artifact pass, against compaction §9's window — stage 2's first measurement |
| per request | candidacy O(viewport tiles); masked count O(containers touched) per candidate; **no per-point test** | O(artifacts near the viewport × containers per artifact) |
| the leaf by vertices | the descent O(p·16) per request, cached by digest; boundary rows tested **under the mask**; one `tile_ranges_all` probe per boundary cell **per segment** | `max_region_vertices`, `max_region_cells` — the second is really bounding cells × segments, which trickle ingest multiplies until the next merge |
| the wire | ≤ 2,048 vertices per served shape, fewer at coarse depth | §7.2 |

**Measured, 2026-08-29, Overture places part 0** — 4,599,286 places, the 17,551 divisions their
lineages name, 12.5×10⁶ vertices in, 12.45×10⁶ out after canonicalisation, 50,660 parts and
52,208 rings, release build, one segment. `tessera check`'s report from the geometry alone: 14 s;
2,536,333 interior tiles (max 380,789 in one artifact) and 3,433,114 boundary cells (max
375,155); **held 57.7 MB** beside 150 MB of canonical bytes in the blobs; 299 children whose
bounds escape their declared parent's. The build: **80 s wall, 1.87 GB peak RSS** (75.5 s before
it wrote the two derived files below), against the enumerated declaration of the same layer at
55 s and 1.33 GB (`test_corpora/overture/README.md`); of that, resolving the segment against the
shapes is 11.6 s — 4,277,462 rows tested one by one in boundary cells, 14,395,202 interior
admissions summed over artifacts — and the layout pass 12.4 s. **Two counts, both stated**: 17,551
artifacts hold a shape and 17,544 hold at least one row of this part; the seven that hold none
are two countries whose lineage members are overseas (France, the United Kingdom — the polygon
is the mainland), two whose members lie just outside a generalised polygon, and three
neighbourhood-sized polygons a few metres across whose one place is inside at the source's
coordinates and outside at its **stored** position, the stored position being the `f32`
coordinate quantised — up to ~128 grid units at this extent. Membership is of the stored
position, exactly (§4.1), and the build names the seven.

**The engine's open, before and after persisting what the build and the fold resolve** (§6.3,
owner ruling 2026-08-29), process start to `/readyz`: **11.3 s → 0.69 s**, and after a fold and a
restart **12.9 s → 0.71 s**. Before, the open decoded and decomposed every shape in 8.4–9.5 s — of
which the decode was 70–80 ms and the descent the rest — and resolved the segment in 2.6–3.0 s.
After, it decodes the shapes and assembles their held form from the `shape-held` file in 256 ms,
claims the segment's `shape-rows` piece in 3 ms, and resolves nothing. The zoom-0 whole-map
request for a three-country principal (MX, US, EC), naming the layer, serves 8,451 divisions
with masked counts — 8,448 from the lineage join — in **182 ms cold and 21–24 ms warm** (369 and
31–75 ms before), 1.09 MB; a request naming no layer serves none of them, the nested layer
declaring no zoom range. A fold re-resolves the segment in 2.6 s inside a 4.5 s fold (14.3 s
`POST` to counter), writes the forms again, and the request after it is 51 ms. ⊘ The two files'
sizes were not measured on this part; from the format the decompositions are ~40 MB
(2,536,333 × 9 B + 3,433,114 × 5 B + 21 B per artifact). So on this part the per-flush row is
~0.6 µs per row over a segment of 4.6×10⁶, the held structure is 3.3 kB per division, and the
request pays nothing for the geometry. ⊘ The world-scale figures below remain modelled: full
Overture is sixteen parts and a division set thirty-six times this one.

**The owner's requirement — 10⁴ shapes across 10⁶ visible points in one request — is met for
published shapes by there being no per-point work at request time at all**: a shape's rows are
held in the serving layout, a count is one intersection or one histogram pass, and 10⁴ of them
cost what 10⁴ enumerated artifacts cost today, which row 7 of the delivery record measured.

**This is a lot of precomputation, deliberately, and it is the same trade the enumerated layers
made.** The per-point cost is small — a new row is tested against the few shapes whose boundary
cells contain it, at ~100 ns each, so 10⁹ points × 5 nesting levels is minutes of CPU across a
build and the same again at a fold. What it buys is a render path with no per-point work. What it
costs at scale is not the tests but the **memory**, and that is why the layout is chosen rather
than fixed: a bitmap per artifact for 10⁶ shapes over 10⁹ points is the 78.5 GB form, and a label
column is the 4 GB one. What stays per artifact whatever the point count is the decomposition and
edge table — ~2 GB for 10⁶ polygons of 4×10⁸ vertices — reported before a build. What §6.3's index buys is that a request does not also pay for the 10⁶ artifacts
it is *not* near. For a **lasso**, the per-point work is the boundary rows under the mask: a
mouse-drawn shape at a zoomed-out depth tests tens of rows; a lasso a few cells across over a
dense pile is the worst case and tests every visible row in it — 10⁶ rows at ~50–100 ns plus a
residual read, 0.1–0.3 s, which `max_region_cells` bounds by cells and not by rows (selection-operand
§6, and the reason it must not).

**Held size at Overture scale, modelled.** With a mean edge of 30–100 m the total perimeter is
1–4×10¹⁰ m, so **2–6×10⁷ boundary cells** at 611 m — and a shared border is a cell in *each*
polygon at *each* nesting level, a coast being a country's, a region's, a county's and a
locality's cell at once. At ~5 B per cell that is 0.1–0.3 GB; the interior tiles a comparable
count at 16 B; the edge tables 4 B per vertex on 3.8×10⁸ vertices, 1.5 GB; the canonical form 4.6 GB
in the blobs, read as needed. The figure the first design would have committed — a per-cell edge
list at ~110 B — was 2–7 GB and dominated by the 874k localities and neighbourhoods rather than the
378 countries, which is why §6.3 holds codes and re-derives edges. The measurement is Overture at
full scale, the fixture the campaign built for exactly this rung; the model says the held structure
is linear in total perimeter in cells, and §6.5 puts that number in `tessera check` so it exists
before the memory does.

## 10. Invariants, and why there is no register row

The argument is selection-operand §7's, and it transfers because a published polygon is the same
operand held rather than sent. What is different is said here.

- **I2.** Masked count, declared size, candidacy and the served shape: the first three are computed
  over `membership ∩ M_auth` by arithmetic over ranges and one bitmap, exactly as a bbox layer's
  are; the shape is supplied content served under the artifact's own verdict (§7.1). No quantity
  reads a row outside the mask and then gates it. The boundary rows tested at a generation are not
  a served quantity: they are the membership itself, resolved, on the same footing as a box's
  ranges.
- **I3 / I12.** A region leaf filters points; it moves no artifact's existence and no count
  (decision 0104), and the leaf by id is subject to the artifact's verdict rather than moving it.
- **I7.** Selection continues to evaluate its definition from the mask; the polygon narrows before
  it, as any filter does.
- **I10.** The leaf by id takes a `tessera_id` and the wire carries grid units. No entity id is
  involved at any point.
- **I11.** The per-generation bitmap is built for one `segments_version` and read under it; a
  fold renumbers rows and re-resolves, so nothing built against one generation is read under
  another.
- **The declared size** — the bitmap's cardinality over every row, masked by nothing — is held
  for the operator's report and the layout choice and is **never on the wire**; the proportional
  criterion stays refused on predicate layers (§6.2), which is what keeps it off.
- **C17.** The leaf by id's empty-operand rule (§8) is what keeps *unknown*, *suppressed*,
  *unreachable* and *withheld* one answer.
- **C12's shape** — a supplied shape is corpus-independent content served whole under a masked
  number — is the row `annotations.md` §8.2 already sits on; this design adds a second content type
  to that row, not a row.

**What could have been a disclosure and is closed in the mechanism.** A cover fallback on a
published polygon would have let two viewers' *counts* differ by the density along a border they
cannot see; there is none (R2, and §8's always-exact). The build's reports (§6.5) are
operator-facing and never on the wire. Simplification is a function of the shape and the depth
alone (§7.2).

## 11. Not in this design

- **Saving a selection as a polygon** — the runtime-artifact path (selection-operand §9.2). This
  design gives it its storage form and its wire, and it waits for the edit pass as before.
- **The export verb** and GeoJSON output.
- **Deriving hierarchy from containment**; edges stay declared (§6.2).
- **Shapes beyond the four** (§4.1).

## 12. Order of work, and what each stage amends

Each stage in its own worktree, on the artifact convention; the status record is
[`../artifact-delivery.md`](../artifact-delivery.md), which gains a row per stage as it lands.

1. **The core** — `tessera_spatial::shape`: the `Region` trait and its four kinds, WKB and WKT
   readers, canonical form with weights, decomposition, point test, densification for the wire.
   Property-tested: the descent's answer against the direct test at random positions and along
   every edge, for each kind; every boundary cell's carried parity against a full ray cast; a
   budgeted cover a superset; a ring canonical whatever its start. No engine change. *Amends:*
   nothing normative; selection-operand §3 is its specification. **Built 2026-08-29**
   (`artifacts/shape-core`), gate green.
2. **The artifact type** — the four `shape.kind`s with `depth` deleted, `default_space` and
   `space`, the row fields, the blob variants, §6.3's held structures replacing `RangeSets` (the
   per-segment bitmap built at the flush, the per-generation union, the artifact index), the
   validate lift, disclosure spelling, `tessera check`'s decomposition report, `PUT /control/layers`,
   and the three shape content types read rather than passed through. Overture's boundary layer
   re-declared against `division_area` directly, the offline join left in place for the attribute
   columns. First measurements: §9's held size, the fold, and the zoom-0 request. **The oracle
   gains spatial membership**: an even-odd evaluation over the source geometry on the quantised
   grid with on-edge inside, compared per artifact against masked counts and per point against
   `membership:<layer>` — today no second reader computes a spatial membership at all, so R2 is
   checkable by nobody but the implementation. *Amends:* `configuration.md` §1 (the `shape` row,
   `disclosure.json`'s spelling, the refusals at lines 1055–1061), `annotations.md` §4.2 and §8.6,
   `annotation-representation.md` §2.0's ⊘ per-request bound and §2.2, `crates/tessera-build/src/disclosure.rs`
   and `tests/build_layers.rs`'s `spatial:bbox:depth=` assertion, `docs/evidence/analysis/sizing.py`,
   R3's ruling in the 2026-08-21 review memo (a superseded-by note), and `conformance.md`'s matrix.
   **Built 2026-08-29** (`artifacts/shape-type`) but for the three content kinds (⊘ at §6.1), the
   held edge table (⊘ at §6.3), the sizing script, the memo's superseded-by note and the
   conformance matrix row; the oracle is in `conformance/tests/test_shape_membership.py`, a plain
   even-odd walk because shapely is not in the reference venv.
3. **The wire and the client** — `shape_x`/`shape_y`, the `shape` ask, presimplification and
   densification at serve, `/v1/meta`'s kind, deck drawing, the pick. *Amends:* `contracts.md`
   §3.2, `artifact-shapes.md` §8, `client-components.md`. **Built 2026-08-29**
   (`artifacts/shape-wire`), with the three authored content kinds stage 2 had left: the derived
   hull, the membership shape and an authored shape are one column pair of three declared kinds,
   served at the request's depth under the 2,048 guard, drawn by `@tesseradb/deck` through one
   polygon-with-holes path, picked by even-odd over each part, the kind on the card. ⊘ Two things
   are not this stage's: the client's golden captures still carry the r44 columns and are refused
   by name until recaptured against a served corpus (`client-delivery.md`), and the reference
   oracle's wire reader (`reference/oracle/wire.py`) still reads `hull_x` — outside this track's
   allowlist, reported rather than edited.
4. **The region leaf**, both spellings, on the core — selection-operand promoted with its three
   rulings, `region.ts`'s raster path retired. *Amends:* `architecture.md` §8.2 as that document
   already requires. **Built 2026-08-29** (`artifacts/shape-region`): `FilterExpr::Region`
   routed row-space over the whole view, the decomposition cached per generation across principals
   and the boundary rows tested under each request's mask (`tessera_engine::region`); the leaf by
   artifact through the same `gated_artifact` predicate `/v1/artifacts/{id}` answers by; the
   verdict on `x-tessera-region`; `region` reserved at the build; the two constants on
   `/v1/meta`; the client's selection as the filter with *filter to this* and *outside this* live.
   ⊘ `architecture.md` §8.2's marker is that document's owner's to remove; this track reports it.

Stage 4 could precede 2; it is placed after because the campaign's boundary rungs are waiting on 2
and nothing is waiting on 4 that 2 does not also unblock.

**Stage 1 owes one more test file before 2 begins**, from the review: the property tests draw
star polygons at random `f64` positions and will essentially never put a vertex on a tile
boundary, which is where a wrong carried parity flips an *interior tile* rather than a cell.
Adversarial fixtures — vertices on tile corners and edges at every depth, axis-aligned edges lying
on tile boundaries, collinear runs, a hole touching its outer, two parts sharing an edge — with
probes placed exactly on those grid lines.

**What the conformance suite aims at.** R2 fails silently when membership is wrong while every
served number stays self-consistent, and the two places that can happen are bookkeeping and ties,
not the test itself: a segment resolved twice or not at all, a row-base slip, a bitmap built under
one `segments_version` and read under another; and a tie mishandled in the descent. Neither is
visible from inside the service. The suite holds the adversarial polygons above over a corpus with
points placed on the same grid lines, against shapely over the same quantised vertices with
on-edge inside, and compares per-artifact masked counts and per-point `membership:<layer>` under
several masks — **at the build, after a flush, and after a fold**.

## 13. Rulings

All taken by the owner on 2026-08-29, in discussion, on the recommendations as drafted.

- **(a)** Even-odd over every ring of every part is the definition of inside a polygon (§4.1).
- **(b)** A spatial layer may declare content, levels, hierarchy and `depends_on`; the restriction
  stays on `attribute` (§6.2).
- **(c)** WKB in tables, WKT inline, GeoJSON excluded as input (§4.2).
- **(d)** A separate `shape_x`/`shape_y` pair and a fourth ask word, `shape`, rather than deepening
  `hull`'s nesting (§7.1).
- **(e)** `max_shape_vertices` refuses at publication; the held decomposition is reported and not
  capped (§9).
- **(f)** `wgs84` is refused until a view can project; no shape-side projection (§4.3). A view
  can now project, and the ruling's second half is what §4.3 is built on: the transform is the
  view's own and this design still supplies none.
- **(g)** Every kind is exact; `bbox` loses its depth-cover form and R3's *ranges are the
  membership* ruling is superseded (§4.1).
- **(h)** Circle and ellipse join box and polygon as kinds; a shape is membership or supplied
  content by the layer's declaration and the geometry does not know which; every kind is served as
  rings (§4.1, §6.1, §7.1).
- **The space lives with the submission, not the layer** — `default_space` on `[layer.artifacts]`
  and the ingest body, `space` per row and per leaf (§4.3). Raised by the owner against the r2
  draft, which had put it on `[layer.shape]`.

Three contract points from the review were put to the owner and ruled 2026-08-29, two of them
differently from the recommendation:

- **(i)** *Recommended* own columns for a content shape. *Ruled instead:* an artifact has **one
  drawn geometry** of a declared kind — derived, predicate or authored — in one column pair,
  `shape_x`/`shape_y`, which absorbs `hull_x`/`hull_y`; the ask word `shape` replaces `hull` (§7.1).
  The recommendation had mistaken two *questions* for two *drawings*.
- **(j)** *Recommended* one view per shape layer. *Ruled instead:* several views sharing a
  coordinate system, geometry declared once, membership resolved per view, extents free (§4.3).
- **(k)** The per-segment membership is resolved in the write executor at the flush, before the
  generation publishes (§6.3) — *taken as recommended*; [`write-path.md`](write-path.md) gains the
  paragraph.

## Appendix R — review trail

- **r10 (2026-08-31)** — §4.3's cross-projection spanning is **built**. `canonical_shapes` takes a
  frame — view id, projection, extent — per view; the build's layer read and `PUT /control/layers`
  each resolve one per view of the layer, neither reading a first or an anchor view for all of
  them. The layer-level refusal (projected mixed with `none`) is at the declaration on both entry
  points and names the layer and both sides; the row-level one (`space = "view"` over unequal
  frames) is at the row, the space being a fact about the submission. Out-of-extent is a per-view
  count in the build's report and a per-view flag in the publication's, never a refusal. The
  pre-0111 refusal of a shape layer whose views declare different projections is deleted from the
  config parse. `test_corpora/multiview` carries the case (`regions` over `world` and
  `world_flat`); `crates/tessera-build/tests/shape_span.rs` and
  `crates/tessera-server/tests/shape_span_serving.rs` are the tests. No design content changed.
- **r9 (2026-08-30)** — cross-projection spanning (decision 0111): a `wgs84` shape spans any
  projected views through each view's own transform; `view`-space geometry spans only equal
  frames; all-projected or all-`none` per layer; the two-`none` warning removed (opt-in needs no
  second-guess); wholly-out-of-extent shapes warn and never block. Owner-ruled; the mechanism
  review folds into the shape stage.

**r1 (2026-08-27).** Written from the owner's ruling that three consumers needing one operation
makes it a first-class capability rather than a per-consumer workaround. Requirements only, by owner
direction: assumptions and candidate mechanisms deliberately excluded. R9–R12 added later the same
day on the owner's ruling that a polygon may arrive in either coordinate space rather than the
caller being forced to pick one. **Not reviewed.**

**r2 (2026-08-29).** The design, §3–§13, on the owner's direction to design and build polygons as
an artifact type sharing the selection and classification paths. §4 of r1 (open questions) is
answered in place: R5 is bounded by a publication vertex cap, the artifact pass's existing budget
and a reported-not-capped held structure (§9); a request-time region is an operand, and a
published polygon is that operand held (§5); multipolygons and holes are required by consumer
§1.1 and admitted under even-odd (§4.1); `depth` has no meaning on a polygon and is refused
(§6.1). **Not reviewed; six rulings open.**

**r3 (2026-08-29).** The six rulings taken as recommended, and two questions from the owner folded
in. The space moved off the layer and onto the submission (§4.3): a layer-level declaration encoded
an input-side fact into storage configuration. Circles and ellipses joined as kinds, and the design
generalised from *polygon* to *shape* on the observation that the decomposition asks two questions
of a shape and every kind answers them in the same arithmetic — which also made the box's
depth-cover form a second meaning of "inside" and deleted it (rulings (g), (h)). `tessera_spatial::polygon`
became `tessera_spatial::shape`. **Not reviewed.**

**r4 (2026-08-29).** One adversarial review under three lenses — disclosure and the invariants;
cost at scale, with the owner's requirement that 10⁴ shape tests across 10⁶ visible points fit
one request named as the lens; and the contracts. Seventeen findings, dispositioned in one pass.
The four majors on cost changed §6.3 and §9 together: (1) candidacy scanned every artifact and a
count cost O(ranges), which `ranges.rs` gets away with because a box is few ranges and a polygon
is not — replaced by one run-optimised bitmap per artifact and a geometry-derived artifact index,
so the request path is bitmap arithmetic with no per-point test; (2) "incremental by construction"
had no structure to rest on and inherited a rebuild on the request thread — replaced by
per-segment bitmaps built in the write executor before the generation publishes (ruling (k));
(3) the design did not say the edge table is held, and it must be — now stated, indexing the
canonical vertices; (4) per-cell edge lists were the dominant memory at world scale and the
fallback aimed at countries when localities are the mass — boundary cells hold codes and parity
only, edges re-derived per (cell, segment), and `tessera check` reports the decomposition's size
before a build. The fifth major found the content-shape sentence unspecifiable against
`list<utf8>` — own columns (ruling (i)); the sixth found the served ring could promote a hole to
an outer — fixed in the core and stated in §7.2. Minors: a membership shape is corpus-independent
by declaration (§4.1); geometry-derived candidacy bounds rather than a row statistic; one view per
shape layer (ruling (j)); refusal scope of `space` at ingest; the `spatial:bbox:depth` dependents
enumerated in §12; a served ring is never the predicate and the tie rule joins the client's
obligation; the publication cost corrected to O(V·16 + p·16) and `max_region_cells` shown to
bound cells × segments; the oracle owes a spatial evaluation, there being no second reader.
Notes: the leaf-by-id's timing is C4's family; independent simplification of shared borders is
accepted; the declared size is never on the wire. What the review could not break is recorded in
its report: the leaf-by-artifact gate, `none_of` over a served shape, simplification as a function
of shape and depth alone, the boundary bitmap as membership rather than a served quantity. The R2
failure mode it named — bookkeeping and ties, invisible from inside — is now what §12 aims the
conformance suite at.

**r5 (2026-08-29).** The three contract points ruled (§13 (i)–(k)), two against the
recommendation, and the consequences written in: one drawn geometry per artifact of a declared
kind in one column pair (§7.1), views sharing a coordinate system sharing a shape (§4.3), the
flush resolving membership before publish (§6.3). Also from the owner's questions the same day:
the flush is a per-row source and the serving layout is chosen rather than fixed (§6.2, §6.3), with
the precomputation trade stated against the delivery record's 78.5 GB / 4 GB measurement (§9).

**r6 (2026-08-29).** Promoted to Normative on the four stages landing (§12) — the last, the
region leaf, promoting `selection-operand.md` with it and removing `architecture.md` §8.2's ⊘.

**r7 (2026-08-30).** `wgs84` unblocked, on `projections.md` §10 landing: §4.3's ⊘ comes off and the
paragraph says what is true — the view's own declared transform, each edge densified to one
depth-16 cell before projection, a box in degrees still a box, a coordinate outside ±180 × ±90
refused. §11 loses its `wgs84` bullet and ruling (f) keeps its second half, which is what the
build rests on. Two things the document asserted became checkable at the same moment and are now
checked: a shape layer whose views declare **different** projections is refused at the
declaration, and the several-views warning fires only where nothing says whether the views share a
space — two views both declaring `projection = "none"`. One thing it asserts is now blind and says
so: the degree-looking report cannot fire on a projected view, whose frame is itself inside
±180 × ±90. **Not reviewed** — the design is unchanged, R10 included; what changed is which of it
is built.

**r8 (2026-08-30).** §6.1 names the **space** in the list of what an authored shape content shares
with a membership shape. It was covered by "read exactly as a membership shape is" and by nothing
more specific, and both entry points had read the shorter list — the reader, the report and the
vertex cap — as the whole of the parity and fixed the space at `view`. The design did not change;
the sentence now says the one property the enumeration left implicit, and both the build and
`PUT /control/layers/{name}/artifacts` resolve a table's `default_space` and a row's own `space`
for authored content by the same route they resolve them for a membership shape.
