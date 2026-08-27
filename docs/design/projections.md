# Projections — the high-level position

**Status:** **Provisional, high level — the shape of an answer, not the answer.** It records where
the owner and this session got to on 2026-08-27 and what that implies; the detail is being
researched separately and will correct parts of this. Nothing here is built. **To become
normative:** the research folded in, the enumerated set decided, and the configuration surface
amended.

**Reads with:** [`configuration.md`](configuration.md) §1 (the `[[view]]` block and `extent`),
[`client-interaction.md`](client-interaction.md) §12 (the geographic mode),
[`client-components.md`](client-components.md) (the basemap alignment condition),
[`views-and-multi-table.md`](views-and-multi-table.md) §3 (a view carries *projection provenance*),
and [`polygon-membership.md`](polygon-membership.md), which depends on this.

---

## 1. Where things stand today

**Tessera has no projection layer. It has a frame.** A view is defined as a named coordinate
system, but what the engine stores is numbers quantised against an `extent` — 16 bits of cell and
16 bits of residual per axis — and nothing records what those numbers mean. Projection happens
outside, before ingest, and leaves no trace.

The gap is already named twice, both as ⊘: *"CRS handling is an ingest contract"*
(`client-interaction.md` §12), and *"a geographic corpus's CRS is an ingest contract that does not
exist; until it does, data coordinates are quantisation-space numbers and nothing here names a
projection"* (`client-components.md`). The consequences are concrete rather than theoretical — a
basemap lines up with the points only when the quantised extent is the basemap's own tile grid, and
today nothing states that condition, checks it, or tells a client whether it holds.

## 2. The position

**Accept WGS84, declare the projection per view, transform once at the boundary.**

- **The accepted input coordinate system is WGS84 longitude and latitude.** Every dataset in the
  test ladder is already in it, GeoJSON mandates it, and Web Mercator is defined on it. A caller
  holding anything else converts before arriving — which is the one conversion every GIS tool does
  without ceremony.
- **A view declares its projection**, from a **closed enumerated set**, as the configuration
  surface requires of every value that is a word rather than a caller's string. `none` is the
  setting for a view whose geometry is an embedding, where no projection applies and coordinates
  are whatever produced them — and it stays the default, so a corpus with no geography is never
  asked to name one.
- **The service transforms; it does not negotiate.** No arbitrary coordinate systems, no datum
  shifts, no national or regional grids. Those need grid files that are versioned data and change
  answers between releases, and the set is unbounded.
- **The projection is a property of the corpus, not of a request.** Changing it re-places every
  point, exactly as re-fitting an embedding would, so it is decided once at build; and because
  [decision 0091](../decisions/0091-build-is-ingest-into-an-empty-database.md) makes build and
  ingest the same operation, a later batch must land under the transform the build used.
- **The extent follows from the projection** rather than being four numbers a caller types. Legal
  frames are the projection's own domain or a 2^k-aligned sub-square at an integer zoom offset —
  which is the condition `client-components.md` already states for basemap alignment, and which
  closes the trap where a corpus quantised to a tight bounding box can never line up with any tile
  grid and nothing says so.
- **The extent is written in WGS84, on the input side of the projection** (owner ruling,
  2026-08-27). A caller who had to state the frame in projected units would have to project their
  own corner coordinates to discover what to write — which is the work the service just took on, put
  back on them at the one point where getting it wrong misplaces every stored position. `y = [49.9,
  60.9]` for the United Kingdom is a number from an atlas; `y = [6417441, 8611763]` is the output of
  a calculation nobody should be asked to do by hand. This holds only because the candidate
  projections are **cylindrical** — longitude to x is linear and latitude to y is monotone under all
  three, so a longitude/latitude rectangle is still a rectangle after projection and its corners are
  its bounds. It would not hold for a conic or an azimuthal projection, which is a further reason
  the set stays cylindrical. Under `projection = "none"` there is only one space and the extent
  keeps exactly today's meaning.
- **A stated extent is snapped outward to the enclosing aligned square, and the build reports what
  it snapped to.** A box written in degrees will essentially never project onto a 2^k-aligned
  sub-square, so the two properties above are reconciled by the build taking the smallest legal
  frame that contains what was asked for — the caller says roughly where, the frame stays a Morton
  prefix of the tile grid, and the difference is printed beside the frame report rather than
  silently absorbed. `auto` is the same operation over the data's own longitude/latitude box; without
  the snap it would fit a per-dataset frame with no tile alignment at all, which is the trap the
  bullet above exists to close.
- **An exact frame is spelled as the tile it is**, not as projected coordinates. The one caller who
  genuinely wants a specific frame rather than a region is matching an existing tile scheme, and
  `{ z, x, y }` states that exactly, needs no snap, and is legible; projected units serve that case
  worse than the notation it is really asking for. ⊘ The spelling is not designed and the
  configuration surface does not have it.

## 3. What it resolves

- **The ingest contract that does not exist** starts existing, and both ⊘ marks above close.
- **Polygon membership gets its coordinate question answered by construction.** Shapes arrive in
  WGS84 like points and pass through the same declared transform, so there is no mismatch to
  detect and no risk of corpus and geometry being placed by different code. Several of
  [`polygon-membership.md`](polygon-membership.md)'s open questions are consequences of this
  document rather than of that one.
- **A client can be told what it is looking at** — whether a basemap may be drawn, and how to
  invert a stored position back to longitude and latitude.
- **Resolution for a regional corpus.** On the whole-world Web Mercator square a Morton cell is
  ~611 m, which is coarse for a city; an aligned sub-square recovers it without losing tile
  addressing.

## 4. What it costs, stated rather than discovered later

- **An enumerated set is a permanent commitment.** Geometry is hashed rather than seeded, so a
  projection's arithmetic is part of the format: adding an entry later is ordinary, changing an
  existing one is a format break.
- **Web Mercator distorts area,** and this system's product is counts and densities. A cell at 60°N
  covers about a quarter the ground area of one at the equator, so a density map in Mercator is
  density per screen area. It cannot be corrected inside the engine — masked counts are bitmap
  cardinalities and Appendix H's line is a counting engine, not an aggregation engine — so it is a
  reporting caveat and a client-side display choice.
- **Web Mercator clips at ±85.0511°,** and that is data loss rather than distortion. Two rungs are
  measured and both are a tail rather than a loss: GBIF holds 68,581 above and 905 below of
  3,761,740,868 georeferenced (18 per million, §4a), and GeoNames 18 above and 553 below of
  13,463,857 (42 per million, measured 2026-08-27 over `allCountries.txt`) — asymmetric in both
  cases, and Antarctic in GeoNames'. ⊘ The rest of the ladder is unmeasured.
- **Regional accuracy is given up deliberately.** A corpus that would be better served by a local
  projection cannot have one.

## 4a. What the research found — verified, and it changes §2's detail

Research report of 2026-08-27, its two load-bearing claims about this repository verified here.

**The coordinate path is `f32`, and that is a prerequisite rather than a footnote.**
`crates/tessera-build/src/input.rs:1269` narrows an `f64` Parquet column with `*v as f32`;
`FlushRow.x`/`.y` are `f32`; the control-plane ingest schema is `Float32`. An `f32` ULP at Web
Mercator magnitude is **exactly 2 m**, against a cell of 611.50 m at the world extent but 0.597 m at
a zoom-10 sub-square — so past roughly zoom offset 8 the **cell** is wrong, not merely the residual,
and nothing in any report can see it. At the world extent it only wastes residual bits, which is
already true of today's embedding coordinates and has never mattered. **A tile-aligned sub-square
cannot exist until the coordinate path reads and quantises in `f64`.** `PointRow` already carries
the quantised `u32` pair, so nothing downstream changes.

**Cell y must be north, and the first extent written down was upside down.**
`clients/ts/core/src/coords.ts` pins — measured against deck.gl's `Tileset2D`, not assumed — that
tile y and cell y increase together, and XYZ tile y=0 is north. EPSG:3857 northing increases
*northward*, so a frame declared symmetrically in metres mirrors the map against every basemap. The
projection's output must be defined **y-south** at the definition, and `/v1/meta` must say so rather
than let each client re-derive it. The ingest campaign plan carried the wrong extent and is
corrected.

**The ±85.0511° clip is measured, and the clamp report structurally cannot see it.** Live GBIF
counts: **68,581 occurrences above +85.0511° and 905 below**, of 3,761,740,868 georeferenced — 18
per million. Clipping before quantising lands every one exactly at the frame maximum, where
`contracts` §2.5 says a point is *not* clamped. **A clip counter distinct from the clamp counter is
required**, for `web_mercator` only.

**No crate earns its place.** `geo` has no projection maths at all; `geo`'s `proj` feature *is* the C
library. Both candidate forward/inverse pairs are about twenty lines of `f64`. A dependency only
becomes unavoidable for a projection with an iterative inverse — which is an argument for not
choosing one.

**Equirectangular has zero transcendentals** and is therefore bit-exact on every platform, where Web
Mercator composes one. The determinism exposure is small and lands on the *oracle contract* rather
than on correctness: a 1-ULP libm disagreement flips a residual bit for ~4×10⁻⁷ of points — ~2,800
at GBIF scale — while a **cell** flip is 0.04 expected across the entire corpus. The conformance
suite compares quantised coordinates exactly, with no tolerance, so the exposure is a flake in a
differential test rather than a wrong answer.

**A polygon edge means the projected-plane edge**, and the divergence is not small: the midpoint of
a 50°→52° edge differs by **1.2 km** between the lat/lon plane and the Mercator plane, and a
−40°→−20° edge by **57 km**. That is a sentence [`polygon-membership.md`](polygon-membership.md)
owes its readers, not a defect.

**The equirectangular family collapses under normalisation, so it is one entry and not five.**
Equidistant cylindrical is parameterised by a standard parallel φ₁ — `x = R(λ−λ₀)cos φ₁`, `y = Rφ` —
giving a world of aspect `2cos φ₁ : 1`: plate carrée at φ₁ = 0 is 2:1, Gall isographic at 45° is
√2:1, and 60° is exactly 1:1. Normalised to the unit square, `tx` and `ty` are the same expressions
whatever φ₁ is, so **every member of the family stores identical positions** and the parallel
survives only as the aspect a client draws it at. Structurally the same result the research found
for Lambert cylindrical equal-area. One enumerated entry, with the parallel as a display parameter.

⊘ **What does not vanish, and is open: how to allocate resolution to a non-square world.** Filling
the square grid with a 2:1 world makes cells anisotropic on the ground — 611 m wide by 305 m tall at
the equator — so a Morton tile covers a 2:1 ground rectangle. Using only the middle half of the grid
vertically keeps cells square and spends half the grid. This is a real choice, it is independent of
which parallel is named, and it does not arise for Web Mercator, whose world is square by
construction.

**The oracle's exposure is a matter of scope, and the owner has scoped it** (2026-08-27): the
differential oracle need cover only **one geographic dataset at fixture size**, not a pass over the
corpus. At 4×10⁻⁷ divergence per point per axis that is 0.08 expected differing residuals at 10⁵
points and ~0.8 at 10⁶ — so at fixture scale no mitigation is required at all, and the concern
existed only under an assumption of full-corpus differential coverage that was never the intent.
Two cheap reinforcements if wanted: test the projection **formula** against published test vectors,
which is exact and catches a wrong formula immediately and is a different question from whether the
pipeline agrees; and make the geographic fixture `equirectangular`, whose zero transcendentals make
it bit-exact by construction.

**Recommended shape, not yet adopted:** normalise every projection's output to the unit square, x
east, y south, so tile alignment is integer arithmetic and the world's true aspect ratio becomes one
number in `/v1/meta`; enumerate `web_mercator`, `equirectangular`, `none` and hold the equal-area
entry, naming Lambert cylindrical equal-area as its candidate; spell a projected view's geometry
`lon`/`lat` and refuse `x`/`y` there, which removes the axis-order footgun for free.

## 5. Open

- Which projections the enumerated set holds. Candidates: Web Mercator (tiles and basemaps),
  equirectangular (poles, trivial inversion, one entry per §4a), one equal-area (honest density),
  and `none`.
- How resolution is allocated for a world that is not square — fill the grid and accept anisotropic
  ground cells, or keep cells square and spend half the grid (§4a).
- Whether the axis names for a geographic view stay `x`/`y` read as longitude/latitude, or gain
  their own spelling.
- What the build reports about the projection, alongside the frame report it already prints — which
  now has to include the snap of §2, and the frame that was asked for beside the frame that was
  taken.
- How the `{ z, x, y }` frame spelling of §2 is written, and whether a zoom offset with a corner is
  a better shape than a tile address.
- What a client does for a view that is not Web Mercator, where no basemap tile scheme matches.
- The footguns — axis order, y-direction, the antimeridian, floating-point reproducibility of a
  hashed artifact across platforms — which are under research and are expected to change §2's
  detail.

## Appendix R — review trail

**r3 (2026-08-27).** §2 gains the space the extent is written in, on the owner's ruling that it is
WGS84 rather than projected units — the observation being that a caller stating a projected frame
must project their own corners to find it, which is the work the service had just taken off them.
Two consequences are recorded with it and are this session's rather than the owner's: the ruling
depends on every candidate projection being cylindrical, and a stated box has to be snapped outward
to the enclosing aligned square for §2's tile-addressing condition to survive an extent written in
degrees. The `{ z, x, y }` spelling is named as the shape the projected-units case actually wanted
and is marked ⊘. **Not reviewed.**

**r2 (2026-08-27).** Research folded in (§4a), its two claims about this repository verified against
the code rather than accepted. Three owner rulings the same day: the equirectangular family is one
enumerated entry with the standard parallel as a display parameter; the differential oracle is
scoped to one geographic dataset at fixture size; and a polygon may be declared in either WGS84 or
projected space, which is [`polygon-membership.md`](polygon-membership.md)'s R10–R12.

**r1 (2026-08-27).** Written from a working session with the owner, on the owner's direction to
record the high-level position while the detail is explored separately. The position is the
owner's; the consequences and costs in §3 and §4 are this session's and are not reviewed.
**Not reviewed, and the research it depends on is outstanding.**
