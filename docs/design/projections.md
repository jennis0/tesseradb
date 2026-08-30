# Projections

**Status:** **Provisional — reviewed, built, and awaiting promotion.** The adversarial review is
done and dispositioned, and the four normative amendments this design forces have landed:
[`configuration.md`](configuration.md) §1 (the `[[view]]` block), [`contracts.md`](contracts.md) §2.2
(the bundle's recorded projection), §3.2 (the `/v1/meta` fields) and §3.4 (the ingest schema's
coordinate columns). What remains is the ladder — the two built geographic corpora rebuilt on a
declared projection — and the owner's ruling on promotion.

**⊘ Built, except the corpora.** The transform, the declaration, the frame and its snap, the
build- and write-path projections, the report, the `/v1/meta` fields and shapes declared in
longitude and latitude are all in place. What is **not** yet done is the ladder: the two built
geographic corpora are still placed by a Python module outside the build
([`../../test_corpora/common/projection.py`](../../test_corpora/common/projection.py)), with their
declarations describing its output, until each is rebuilt on a declared projection.

**Reads with:** [`configuration.md`](configuration.md) §1 (the `[[view]]` block),
[`polygon-membership.md`](polygon-membership.md) §4.3 (shapes declared in longitude and latitude,
which this unblocks), [`client-interaction.md`](client-interaction.md) §12 (the geographic mode) and
[`client-components.md`](client-components.md) (the basemap alignment condition).

---

## 1. What a projection is for here

A view is a named coordinate system, and what the engine stores is a position quantised against that
view's frame — 16 bits of cell and 16 bits of residual per axis, interleaved into a Morton code. A
projection is the function that turns a place on the Earth into a coordinate in that frame, and
declaring it does three things nothing else can:

- **It makes a basemap possible.** A tile basemap lines up with the points only when the frame *is*
  the basemap's tile grid. Declaring the projection and the frame together is what lets a client know
  whether that holds, rather than discovering it visually.
- **It makes a position invertible.** A client, an operator or an oracle can recover the longitude
  and latitude a stored position came from.
- **It makes two corpora comparable.** Two views under the same projection and the same frame address
  the same tile, so a point in one can be located in the other.

**The projection belongs to the corpus, not to a request.** Changing it re-places every point, so it
is fixed when a view is declared and every later batch lands under it. Adding rows to a database is
the same operation as building one, so a projection that had to be re-fitted to place a new point
would not be usable at all — a re-fit moves every existing point and invalidates every stored Morton
code and every artifact extent.

## 2. What a view declares

```toml
[[view]]
name       = "world"
projection = "web_mercator"
extent     = { lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }
```

| key | | |
|---|---|---|
| `projection` | D `none` | `web_mercator`, `equirectangular`, an equirectangular alias (§5.2), or `none` |
| `extent` | R | `auto`, or the box in **longitude and latitude** for a projected view; `configuration.md` §1's four spellings under `none` |

**The accepted input coordinate system is WGS84 longitude and latitude, in degrees.** Every dataset
in the test ladder is already in it, GeoJSON mandates it, and Web Mercator is defined on it. A caller
holding anything else converts before arriving, which is the one conversion every GIS tool does
without ceremony. A value outside ±180 or ±90 is not a coordinate and is refused.

**The service transforms; it does not negotiate.** No arbitrary coordinate systems, no datum shifts,
no national or regional grids. Those need grid files that are versioned data and change answers
between releases, and the set of them is unbounded. The enumerated set is the whole extent of what
can be asked for, and §5.4 says what is deliberately outside it.

**A projected view spells its coordinate columns `lon` and `lat`, and `x`/`y` is refused there.**
Longitude-then-latitude is the order GeoJSON and WKT use and the opposite of the order many sources
publish, and a corpus built with the two exchanged is silently mirrored about the diagonal. Naming
the axes for what they hold removes the ambiguity rather than documenting it. Under
`projection = "none"` there is no longitude, and the columns stay `x` and `y`.

**The other extent spellings are refused on a projected view.** `{ min, max }` and
`{ x = [...], y = [...] }` describe a frame in the space the projection produces, which §4.2 puts on
the wrong side of the transform; `{ auto = true, margin = f }` is refused because a projected frame's
headroom is the snap of §4.2 rather than a fraction of the data span, and a margin inside an aligned
square would only shrink the frame away from the alignment it exists to have.

## 3. Where the transform runs

**At the boundary, once, in the same place for a build and for an ingest.** Building a database and
adding rows to one are the same operation, so a coordinate arrives as longitude and latitude on both
paths and is projected before anything else looks at it. Three consequences are worth stating, because
each is a place where the two paths could drift apart and would not obviously do so:

- **The wire carries `lon`/`lat` for a projected view**, exactly as the declaration does. The axis-order protection of §2 therefore holds on both
  paths, which is what stops a projected view being something that can be built correctly and
  ingested into wrongly.
- **The write-ahead log holds frame coordinates, not longitude and latitude.** Replay then reproduces
  the positions the original write produced, whatever the projection code has done since; a log
  holding degrees would re-run the transform at recovery and make a platform's floating-point library
  part of it (§11).
- **A latitude outside the projection's domain is clipped and counted at ingest, never refused.** The
  same row builds, and a row that a build accepts and an ingest rejects is a defect rather than a
  policy. The out-of-frame check on the write path therefore sees a coordinate that has been
  projected and clipped, and never an out-of-domain latitude. **That check refuses**, where the
  build's counterpart clamps and reports. ⊘ **Unresolved, and inherited rather than introduced
  here:** for *any* row outside the frame, a build clamps it, counts it and proceeds, while an
  ingest answers `422`. That is the divergence
  [decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md) exists to forbid, and
  it is general — clipping neither causes it nor is needed to reach it. What clipping does is
  manufacture rows on the world's own edge, which fall inside a whole-world frame (the bound being
  inclusive) and inside a sub-square only in the world's top or bottom tile row — so a polar corpus
  framed on a polar tile keeps them and one framed elsewhere does not. The two entry points must
  eventually agree; which way is not this design's to settle.

**The bundle records its view's projection**, beside the frame it already records. A bundle that
carries positions in a projected frame and cannot say so is one every second reader has to be told
about out of band — the write path, which would otherwise quantise a degree as though it were a frame
coordinate; the differential oracle, which re-quantises source coordinates against the recorded frame
and would do it without the transform; and any future reader of the artifact. The frame alone does
not imply it: a `[0, 1]` extent is a legal frame for a view with no projection at all.

## 4. The frame

**Every projection's output is normalised to the unit square, x east and y south.** The frame is
`[0, 1]` on both axes whatever the projection, so tile addressing is integer arithmetic and a
declaration carries no magic constant. **For `web_mercator` a 16-bit cell is then exactly an XYZ tile
at zoom 16** — the identity that makes the engine speak the addressing every map client wants. It is
a property of that projection and not of the normalisation: §5.2's world is a different shape, and a
cell of it addresses no published tile.

**y runs south, and this is applied at the definition rather than left to a caller.** An XYZ tile
`y = 0` is the northernmost row, and in this system's cell grid tile y and cell y increase together —
measured against deck.gl's own tileset implementation, not assumed
([`../../clients/ts/core/src/coords.ts`](../../clients/ts/core/src/coords.ts)). EPSG:3857's northing
increases *northward*, so a frame declared symmetrically in metres is mirrored against every basemap;
and because a frame requires `y_max > y_min`, that mirroring cannot be repaired by inverting the
extent afterwards. The negation is part of the projection.

### 4.1 Legal frames

A frame is either **the projection's whole domain**, or a **2^k-aligned sub-square** — the square
covered by one tile at some integer zoom offset. Nothing else is legal, because a frame that is not
one of these can never coincide with a tile grid, and a corpus quantised to a tight bounding box
would be unable to line up with any basemap with nothing saying so.

On the whole-world Web Mercator square a cell is about 611 m at the equator, which is coarse for a
city; an aligned sub-square recovers the resolution without losing tile addressing. A sub-square at
zoom offset *k* has cells `611.50 / 2^k` metres across at the equator.

**The offset is capped at 16.** At that offset the frame is exactly one cell of the whole-world grid,
and a frame finer than one cell of the grid it is meant to be a prefix of has stopped being a prefix
of anything. It is also far past any real corpus: a frame 611 m across, quantised to 9.3 mm cells.

**A region straddling a top-level tile boundary gains nothing from a sub-square**, and that is a
property of aligned frames rather than of this design — every tile scheme has it. The prime meridian
is one of the two boundaries at the first offset and the equator is the other, so a box crossing
either is contained by no square below the whole world. The United Kingdom therefore takes the world
frame, and so does Kenya. Great Britain west of the meridian reaches offset 3, Ireland 5, Switzerland
6, and Greater London west of the meridian 7. A corpus that crosses a boundary and wants the
resolution has to be declared as a box that does not cross; there is no frame that gives it both.

### 4.2 The extent is written in longitude and latitude

A caller who had to state the frame in projected units would have to project their own corner
coordinates to discover what to write — which is the work the service has just taken on, handed back
at the one point where getting it wrong misplaces every stored position. `lat = [49.9, 60.9]` for the
United Kingdom is a number from an atlas; `[6417441, 8611763]` is the output of a calculation nobody
should do by hand.

This holds because **every projection in the set is cylindrical**: longitude maps linearly to x and
latitude monotonically to y, so a longitude/latitude rectangle is still a rectangle after projection
and its corners are its bounds. It would not hold for a conic or an azimuthal projection, which is a
further reason the set stays cylindrical.

**A stated box is snapped outward to the enclosing aligned square, and the build reports the snap.**
A box written in degrees will essentially never project onto an aligned square, so the caller says
roughly where and the build takes the smallest legal frame containing it. `auto` is the same
operation over the data's own longitude/latitude box; without the snap it would fit a per-dataset
frame with no tile alignment at all.

The difference between the frame asked for and the frame taken is resolution the corpus does not get,
so it is printed beside the frame rather than absorbed. **⊘ There is no way to name a frame exactly.**
A caller matching a foreign tile scheme would want to write the tile address itself; the box with its
snap reaches every frame such an address could name, so the spelling is not provided.

**Containment comes first and the cap second, and the order is what makes the degenerate cases
right.** A box that constrains nothing — a single point, or a box small enough to sit inside one cell
of the finest legal frame — is contained in aligned squares at every offset without bound, so it takes
the cap of §4.1 and the build reports the frame as **floored rather than fitted**. But a degenerate
*axis* is not a degenerate *box*: a box of zero width spanning thirty degrees of latitude is
constrained by its height exactly as any other box is, and flooring it at the cap would hand back a
frame excluding almost all of its own data — which quantisation clamps onto the border rather than
filters. The frame contains the box first; the cap only bounds how far the search may go.

A box whose corner lies exactly on a tile boundary is resolved by the half-open convention the cell
grid already uses — a coordinate on a boundary belongs to the higher cell — so the containing square
is unique at each offset. The consequence is worth stating, because it looks like an off-by-one: a box
whose maximum sits exactly on a boundary spans two tiles there and snaps one offset coarser, which is
correct, the frame having to contain the box.

A box crossing the antimeridian is **refused**: an aligned square does not wrap, so `lon = [170, -170]`
cannot be honoured and reads as an inverted box; the frame to write is the wider one that does not
cross. And `auto` over a source selecting no rows is refused, naming that the frame must be stated —
there is no data to fit and a default frame would be four numbers nothing justifies.

## 5. The projections

### 5.1 `web_mercator`

The projection of every slippy-map tile scheme, and the reason the geographic mode works at all: a
Web Mercator frame makes an XYZ tile *identically* a Morton prefix, so the engine already speaks the
addressing MapLibre, OpenLayers and QGIS want.

```
x = (λ° + 180) / 360
y = 0.5 − ln(tan(π/4 + φ/2)) / 2π          φ in radians, λ in degrees
```

**The domain is cut at ±85.0511287798066°**, the latitude whose projected northing reaches half the
projected world. That cut is what makes the world square, and every tile scheme makes it for the same
reason. A point beyond it is **clipped** — moved onto the frame's edge — which is data loss rather
than distortion, and §7 says how it is counted.

**This is the *pseudo*-Mercator, and it is conformal only to about 0.7%.** It puts a geodetic latitude
through the spherical formula, using the ellipsoid's semi-major axis as a sphere radius, so the scale
factors along the meridian and along the parallel differ by 0.674% at the equator, falling to zero at
the poles. True ellipsoidal Mercator is exactly conformal and places a point up to 42.6 km away in
projected metres — 30.2 km at 45°N — so the two are not interchangeable, and this set holds the one
every tile scheme uses. Within that tolerance a small circle on the ground is a small circle on the
map, which is what makes a drawn circular selection mean what a viewer expects, and no other entry in
the set has the property at all.

### 5.2 `equirectangular`

Latitude and longitude used directly as coordinates. It has no transcendental functions at all, so it
is bit-exact on every platform, and its inverse is trivial. Unlike Web Mercator it reaches the poles.

```
x = (λ° + 180) / 360
y = 0.5 − φ° / 180
```

**The standard parallel does not appear in that transform, and this is the whole reason the family is
one entry.** Equidistant cylindrical is parameterised by a standard parallel φ₁, giving a world of
aspect `2cos φ₁ : 1` — 2:1 at the equator, √2:1 at 45°, square at 60°, and taller than wide beyond
that. Normalised to the unit square, every member of the family produces **identical stored
positions**, and the parallel survives only as the aspect a client draws the world at. So it is a
display parameter, published on `/v1/meta` and carried by a name:

| name | φ₁ | world aspect |
|---|---|---|
| `plate_carree` | 0° | 2:1 |
| `gall_isographic` | 45° | √2:1 |
| `equirectangular` | 0° | 2:1 |

**The grid is filled rather than letterboxed.** Both axes use their full 16 bits, so a cell is 305.75 m
north-south everywhere and `611.50 × cos(latitude)` east-west — square on the ground at ±60°, and
progressively wider than tall towards the equator. The alternative — scaling both axes together to
keep cells square in the projected plane — would cost up to half the grid and up to half the
north-south resolution, and would buy only the ability to move the latitude at which ground cells are
square. It would also make the standard parallel part of the stored format, which is precisely what
makes the family one entry rather than five.

**Equirectangular is not conformal**, and under the filled grid a circle in stored view coordinates is
a ground ellipse everywhere except ±60°. A drawn circular selection over such a view therefore selects
an ellipse on the ground, which is a reason to prefer `web_mercator` for any corpus whose viewers will
draw shapes.

### 5.3 `none`

No projection. Coordinates are whatever produced them — an embedding layout, a synthetic corpus, any
space that is not the Earth — the extent keeps exactly the meaning it has today, the axes are `x` and
`y`, and no basemap or inversion is offered. This is the default, so a corpus with no geography is
never asked to name a projection.

### 5.4 What is deliberately absent

- **An equal-area projection.** Web Mercator's area distortion is real and this system's product is
  counts and densities, so an honest-density entry has a genuine argument — Lambert cylindrical
  equal-area is the candidate. It is not in the set because nothing in reach needs it, and an entry
  can be added later without disturbing the ones that exist.
- **Conic and azimuthal projections**, which would break §4.2: a longitude/latitude rectangle is not a
  rectangle after either, so the extent could no longer be written in degrees.
- **Datum shifts, national grids, and caller-supplied projections**, per §2.

## 6. Precision

**The coordinate path is `f64` from the input file to the quantiser, and from the wire through the
write-ahead log to the flush.** The build reads a coordinate column at
either width and widens the narrower; the wire, the log record and the flush row carry `f64`.

An `f32` value over the unit square resolves to about 2^24 steps per axis in the worst case, against a
grid of 2^16 cells — 256 steps per cell at the whole-world frame, but only `2^(8−k)` at a sub-square
at zoom offset *k*. Past roughly offset 8 there is less than one `f32` step per cell, so the **cell** a
point lands in is wrong rather than merely its residual, and nothing in any report can see it. `f64`
resolves far finer than the 16-bit grid can consume at any offset §4.1 permits.

**Both widths are accepted on input and the narrower is widened.** A whole-world frame is perfectly
served by `f32` coordinates, and a corpus emitting them should not be made to double the size of its
largest columns to be read; a sub-square frame needs `f64` and the caller supplies it. Precision is a
property of the corpus rather than of the release.

**The stored form does not change.** A position is a 32-bit fixed-point pair against the frame,
interleaved into a Morton code, and no float reaches the bundle.

## 7. Clipping is not clamping

Two different things move a point onto the frame's edge, and conflating them hides the one that
matters.

**Clamping** is a point outside the frame it was quantised against. It is reported unconditionally
and refused past half the corpus, because a frame that misplaces the majority of a corpus describes
some other data.

**Clipping** is a point outside the *projection's own domain* — for `web_mercator` alone, a latitude
beyond ±85.0511°. Clipping lands the point exactly on the frame's edge, where the clamp rule says a
point is not clamped, so the clamp counter structurally cannot see a single clipped point. It is
counted and reported on its own.

**Clipping itself never earns a refusal, at any proportion.** The clamp refusal exists because a
clamped point's stored position belongs to the frame rather than to the point, and a frame is a
choice the caller can correct. A clipped point's position is the projection's own domain boundary,
which no choice of frame moves; an Antarctic corpus under `web_mercator` is a caller asking the wrong
projection for the job, and the report says so loudly while the build proceeds.

**At a sub-square frame the two counts overlap, and the clamp refusal still applies.** Clipping lands
a point on the *world's* edge, which for a frame that does not reach that edge is simply outside the
frame — so such a point is clamped as well as clipped, and counts toward the refusal like any other
out-of-frame row. That is the right behaviour rather than an exception to be carved out: a frame
holding a minority of its corpus is the wrong frame whatever moved the rest out of it. The two counts
are separate because they have separate causes, not because a clipped point is exempt from the frame.

The tail is small and real, and it is not symmetric. Of GBIF's 3,761,740,868 georeferenced records,
**68,581 lie above +85.0511° and 905 below** — 18 per million. Of GeoNames' 13,463,857, **18 above and
553 below**, the southern ones Antarctic. ⊘ The rest of the ladder is unmeasured.

## 8. What the build reports

The frame report gains the projection and the snap, beside the extent, the data's own box, the grid
the data occupies and the clamp count it already carries:

```
view 'alps': web_mercator, quantising against x [0.515625, 0.53125], y [0.34375, 0.359375]
        asked for lon [5.9, 10.5], lat [45.8, 47.8] — snapped outward to the square at z6 (33, 22),
        lon [5.625, 11.25], lat [45.089035564831036, 48.922499263758255]
        the data spans x [...], y [...] — 32623 x 18603 of the 65536 x 65536 cells
        3 point(s) placed, none clamped onto the frame's edge
        none of them outside web_mercator's ±85.0511287798066° domain, so nothing was clipped
```

**The frame is printed in the space the positions are stored in, and the snap line carries the
degrees.** The line below it reports where the data sits in that same stored space, so a first line
in degrees would put two coordinate systems on adjacent lines with nothing saying which is which. The
snap line is where the two meet: the box the caller asked for, in the units they wrote it in, beside
the frame that was taken, inverted back through the projection into those same units. That is the
comparison §4.2 exists to make, and it puts both on one line.

**The clip line prints at zero too**, naming the projection's domain. A count that appears only when
it is non-zero teaches a reader nothing about what was checked, and this is the report whose whole
purpose is that a silent build once shipped a degenerate map.

The snap is printed as raw numbers whether or not it is large, for the same reason the frame is: a
caller who can see both can judge them, and a resolution loss the caller did not intend shows up in
the occupancy line that follows. A frame that was floored rather than fitted (§4.2) says so here.

**The ingest response carries the same clip count**, beside the out-of-bound count it already returns.
A projected view that is fed polar rows one batch at a time would otherwise lose the only report that
mentions them.

**`tessera check` prints the frame and the snap for a stated box without opening a data file.**
Under `auto` it cannot: the frame is a function of the data, so the check reads the points source like
the build does, and says so.

## 9. What a client is told

`/v1/meta` publishes, beside the frame:

| field | |
|---|---|
| `projection` | the name declared, including an equirectangular alias |
| `world_aspect` | the ratio the world should be drawn at — 1 for `web_mercator`, `2cos φ₁` for an equirectangular alias |
| `tile_scheme` | the tile scheme the frame addresses — `xyz` for an aligned `web_mercator` frame, `null` for everything else |
| `tile` | the `{ z, x, y }` the frame corresponds to under that scheme, when there is one |

**`tile_scheme` is what decides whether a basemap may be drawn, and grid alignment alone is not
enough.** An equirectangular frame is aligned to a square tiling that no tile server serves — the
published longitude/latitude schemes are 2:1 at their top level — so a host that read alignment as
availability would draw a Mercator basemap against a corpus that cannot line up with one. A `null`
scheme means: draw the points, draw no basemap.

Together with the frame these are enough to decide whether to draw a basemap, which tiles to ask a
tile server for, and how to invert a stored position back to longitude and latitude. A client that
reads none of them still behaves as it did, the frame being unchanged beside them.

A client does not choose a basemap — it is given one — so these fields are for the host that chooses.

## 10. Shapes

A shape — a polygon boundary, a drawn selection — may be declared in longitude and latitude, and is
then passed through the **view's own declared transform** before it is canonicalised against the
grid. Corpus and geometry are placed by one function, which is what makes the two spaces comparable
at all.

**The space a shape is declared in defines the plane its edges are straight in**
(`polygon-membership.md` R10), and this design supplies the transform that rule was waiting for
rather than changing it. An edge declared in longitude and latitude is straight in the
longitude/latitude plane, so its projected image is a curve, and it is **densified before projection
to a tolerance of one depth-16 cell** — which bounds the departure to the grid's own resolution. A
circle or an ellipse declared in longitude and latitude is densified to a polygon by the same rule,
a projected circle no longer being one.

**The two planes disagree by more than rounding on any edge that spans both longitude and latitude.**
An edge across the United Kingdom, from 8°W 50°N to 2°E 58°N, has midpoints 21.5 km apart between the
two readings; a 1° × 1° edge at 50°N, 0.29 km — under one cell at the whole-world frame; and a
60° × 60° edge, 586 km. An edge along a meridian or along the equator is straight in both planes and
does not diverge at all, which is why a published boundary set, whose vertices are dense, is barely
affected and a sparse hand-written polygon is.

## 11. What it costs

- **Web Mercator distorts area,** and a cell at 60°N covers about a quarter the ground of one at the
  equator. Masked counts are bitmap cardinalities, so this cannot be corrected inside the engine: a
  density map in Mercator is density per screen area, which is a caveat to state beside a density
  figure and a display choice for a client.
- **Web Mercator loses the poles** (§7).
- **Regional accuracy is given up deliberately.** A corpus better served by a local projection cannot
  have one; what it can have is an aligned sub-square, which recovers resolution but not the local
  projection's shape fidelity.
- **An enumerated set is a durable commitment.** Geometry is quantised against a declared frame and
  the artifact *is* the record, so a projection's arithmetic is part of the format: adding an entry
  is ordinary, changing an existing one re-places every point built under it.
- **Web Mercator is not bit-exact across platforms.** It composes a logarithm and a tangent, so a
  1-ULP disagreement between two C libraries flips a residual bit for about 4×10⁻⁷ of points — some
  1,500 at GBIF's scale — while a *cell* flip is 0.04 expected across that entire corpus. The
  quantised comparison a differential oracle makes is exact and has no tolerance, so the exposure is
  a rare flake in a differential test rather than a wrong answer. `equirectangular` has no
  transcendentals and is bit-exact by construction, which makes it the right projection for a
  geographic test fixture.

## Appendix R — review trail

**r7.** Four corrections the build found, each at a claim the implementation could check and the
design could not. §3 had the write path's out-of-frame check *warning*; it refuses, where the build's
counterpart clamps — and the asymmetry that exposes between the two entry points is recorded as open
rather than resolved. §3 gains the bundle's recorded projection, without which no second reader can
tell a projected bundle from one holding raw coordinates. §4.2's degenerate-box rule was written for
a point and wrong for a box degenerate on one axis only, where the cap returns a frame excluding its
own data. §8's example report was composed rather than computed, in both its frame and its numbers.

**r6.** Three corrections the implementation forced. §4.2 had a box of zero width or height taking
the offset cap, which is true of a point and false of a zero-width box spanning thirty degrees of
latitude — the cap would return a frame excluding its own data. Containment comes first and the cap
second. §4.1 gains the consequence nobody had stated: the prime meridian and the equator are
boundaries at the first offset, so a region crossing either takes the world frame and no sub-square
at all. §8's example report was illustrative rather than computed, and its box straddles the meridian,
so the tile beside it was one no box could snap to.

**r5.** Reviewed adversarially. The frame model, the y-south rule, the enumerated set and the
precision argument survived recomputation. Four things changed: §10 had a shape's edges straight in
the projected plane, contradicting `polygon-membership.md` R10, and supported it with meridional
figures that measure a parameterisation shift no membership depends on; §3 did not exist, so the
transform's place on the write path, the ingest schema's axis names and a clipped latitude at ingest
were all unassigned; §4.2 had no answer for a degenerate box, an antimeridian box or an empty source,
and §4.1 no maximum offset; and `/v1/meta` published grid alignment as though it were basemap
availability, which is true only for Web Mercator.

**r4.** The design completed from the high-level position: the enumerated set and its spelling, the
filled grid with the standard parallel as a display parameter, `f64` on the coordinate path, the axis
names, the clip counter, the build report and the `/v1/meta` fields. **Not reviewed.**
