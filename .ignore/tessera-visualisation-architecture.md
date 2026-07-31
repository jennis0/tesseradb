# Tessera — Visualisation and Client Architecture

**Companion to** `tessera-architecture-design.md` (r23) and `tessera-implementation-plan.md`.
**Supersedes** an earlier standalone thin-client profile, absorbed here as Profile B.

Two deployment profiles — GPU-capable and thin-client — over one service, one data contract and one interaction model. The document is organised around reuse: for each layer it states what exists, what its licence is, and what remains to be written.

---

## 1. The structural decision that makes two profiles affordable

**Everything that is not drawing is shared.** Tile request scheduling, Arrow decoding, viewport-to-tile-range arithmetic, the opaque handle table, selection semantics, label gating, filter state, prefetch policy — none of it touches a rendering API. Written once as a headless TypeScript module with no DOM dependency, it is testable in isolation and identical across profiles.

What differs is a thin renderer shell: how a point becomes a mark, and how a mark becomes a hit. That is a genuinely small surface, and keeping it small is what stops "we also support VDI" from meaning two frontends.

The service is unaware of which profile is running. It serves the same payloads either way. **No profile difference may ever reach the authorisation path** — if a rendering decision starts influencing what is served rather than how it is drawn, something has gone wrong at a level this document cannot fix.

---

## 2. The interaction model, which is profile-independent

Settled after working through the alternatives in section 8. All of this behaves identically in both profiles.

**Direct point interaction.** The marks on screen are the interactable objects. Every drawn point responds; nothing drawn is inert. This is the constraint that rules out several otherwise-attractive optimisations, and it is worth holding.

**Hit testing** is a uniform bucket grid over the *k* point coordinates the client holds: bucket the cursor, test the handful of candidates in the neighbouring buckets against their actual radii. Testing real geometry rather than cell membership is what makes overlap behave correctly — a point larger than one pixel is hit anywhere it is visibly drawn, regardless of which bucket its centre occupies. Roughly a hundred lines, and correct by construction. In Profile A, GPU picking replaces it and gives the same semantics.

**Region selection is server-authoritative, and this is a correctness requirement rather than a performance one.** The client holds a *sample* — *k* per tile, not the tile's contents. A lasso resolved client-side would report the points it happens to have, which is an arbitrary fraction of what is actually in the region. The server decomposes the region into Morton ranges, intersects each with the mask, and returns the exact masked count plus a sampled preview.

The client still highlights its own drawn points live during the drag, so the gesture feels immediate; the authoritative answer arrives on release. **The UI must be explicit about the gap** — "4,120 items selected (47 shown)" and never a bare "47". Silently presenting the sample as the selection produces confidently wrong downstream analysis, which in a system with this access model is worse than a slow query.

**Drill-down** follows §7.4: a mark stands for items behind it, and opening it queries the service rather than reading anything the client already holds.

**Labels** are served under containment gating (I3) and arrive already filtered — the client renders what it is given and never evaluates a visibility rule. Placement is layout, not policy.

---

## 3. Profile A — GPU capable

### 3.1 deck.gl as the base

MIT licensed, mature, actively maintained, and it covers four of the layers we would otherwise write.

`ScatterplotLayer` renders large point counts from binary typed arrays with no per-point object allocation, which matters because it lets Arrow buffers go to the GPU essentially untouched. `TileLayer` handles quadtree tile loading, caching and eviction against a custom `getTileData`, which is where the Morton tile fetch plugs in. GPU picking is built in — colour-buffer readback giving the point under the cursor without a client-side index. `TextLayer` with `CollisionFilterExtension` does de-overlapping label placement, which is the fiddliest part of the visual layer and the one we most wanted not to write.

It also runs standalone with `OrthographicView` and its own controller, so no base-map library is needed underneath. A UMAP projection is not geographic and should not be forced through a geographic coordinate system.

**Verify before committing:** that `TileLayer` composes cleanly with `OrthographicView` for non-geographic tiling, and that `CollisionFilterExtension` gives acceptable results on hierarchical labels at several zoom levels. Both are documented capabilities; neither has been tested against this data.

### 3.2 Why deck.gl rather than Apple Embedding Atlas

Embedding Atlas is MIT, is the best-benchmarked renderer in the field, is purpose-built for exactly this product category, and has better label placement than anything deck.gl offers. It is still the wrong choice here, and the reason is architectural rather than a judgement about quality.

**Its core premise is the inverse of this design's.** Embedding Atlas is explicitly brute-force — "no need to pre-sample or pre-render your data" — and everything good about it follows from having the entire dataset resident in the client. This system exists precisely because the client cannot hold the dataset: not for performance reasons but because most of it is unauthorised, and the LOD sampler exists to bound what crosses the wire at all. Adopting Atlas would mean removing its data model and substituting tile streaming, which is most of what the library is. `TileLayer` is built for incremental fetch, caching and eviction against a custom loader, so deck.gl gets used as designed rather than fought on the one axis that matters most.

**Its headline advantage does not apply.** Published figures of roughly 4M points at 60fps and 10M+ at 25fps describe rendering enormous *local* point counts. Here *k* is small by construction — if the GPU is ever the constraint in Profile A, *k* has been set wrong. Selecting on raw throughput would mean choosing a library on the one axis the design has already engineered away, while paying for it on the axes that remain: tile lifecycle and picking. Atlas has no reason to provide either, because with all data local you do not need tiles and CPU picking over a local index is sufficient.

**One of its flagship features would be actively harmful.** Its real-time KDE compute kernel, run client-side, produces the density of *the sample* — a mask-dependent, per-user subset — and presents it as the density of the data. Real density must come from the service's exact masked counts. The capability would have to be deliberately disabled, and the failure mode if it were not is a plausible-looking picture that is wrong differently for every user.

**Where Atlas wins, and what to do about it.** Label placement. Its published work targets map-like, zoom-stable placement for embedding maps specifically, which is a stronger thing than `CollisionFilterExtension`'s generic decision about what to hide when marks overlap. Because Atlas is MIT, the response if collision filtering disappoints on real Toponymy output is to port its placement algorithm as a custom deck.gl layer, not to adopt the whole stack. This is the likeliest place the plan needs revisiting (section 9).

**The caveat this rests on.** Most of the argument above assumes `TileLayer` composes cleanly with `OrthographicView`. If non-geographic tiling turns out to need substantial work, tile management becomes ours either way, the gap between the two libraries narrows considerably, and the decision is worth reopening on label placement alone. Verify this before anything about frame rates.

### 3.3 What remains to be written

The Morton tile source behind `getTileData`, carrying the token and decoding Arrow. Label hierarchy selection — deciding *which* labels to submit for placement at a given zoom, which is the gated fallback-ladder logic from §7.7 and is ours regardless. Lasso and rectangle gestures, which are a path drawn on an overlay plus a server call. And the drill-down and detail panels, which are application UI rather than visualisation.

### 3.4 Scale

Treat published point counts as unverified until benchmarked on target hardware with real payloads. The relevant number is not deck.gl's ceiling but *k* — the sampler bounds what reaches the client by construction, and the design's whole argument is that this stays small. If the GPU is ever the constraint, *k* has been set wrong.

---

## 4. Profile B — thin client

Browser executing on a virtualised desktop host, pixels streamed to the user's terminal. No usable WebGL. Panning must feel continuous; zoom may step.

### 4.1 The constraint is pixel churn, not the GPU

In this deployment the scarce resource is the rate at which pixels change on the user's screen, because every changed region is encoded and shipped down the remote-display protocol. Three consequences, all counterintuitive if you are picturing a laptop with no graphics card.

*Fetching data is cheap* — the browser runs in the datacentre, so the hop to the service is LAN-local. *Where rendering happens barely affects the wire cost to the terminal*, since the same pixels arrive either way; the argument for offloading work is host CPU contention, because fifty users share a VDI host, not bandwidth. *What matters enormously is whether screen content is being translated or re-rasterised.*

**A note on "no WebGL".** In VDI it usually means the browser has fallen back to software GL — present, far too slow, and liable to look fine in a demo. Some hosts have vGPU, in which case Profile A applies. Probe for a hardware renderer at startup (section 6).

### 4.2 Panning must translate, never redraw

Remote-display protocols are very good at content that moves without changing. Legacy RDP encodes a moved rectangle as a screen blit; modern H.264 modes achieve the same through motion vectors with almost no residual. Both reward translation and punish re-rasterisation — a canvas redrawing every frame produces subtly different antialiasing on every pixel, defeats motion estimation, and generates a large residual per frame.

**So a pan is a CSS transform on already-rendered layers, and nothing re-rasterises until the gesture ends.** This is a hard constraint on the frontend, not a performance tip: an implementation that redraws on every animation frame works perfectly in local development and is unusable in production, and the failure gets reported as "the network is slow".

### 4.3 Reuse: Leaflet or OpenLayers

Both BSD-2. Both give the tile grid, pan-as-transform, discrete zoom levels, layer compositing and tile lifecycle — the machinery that makes web maps usable over remote desktop today — with no GPU dependency anywhere. Both support non-geographic coordinates: Leaflet through `L.CRS.Simple`, OpenLayers through a custom projection.

Leaflet is smaller and simpler; OpenLayers is more capable and has better non-geographic support. Either is a fit. The choice should turn on whichever the team can read.

### 4.4 What remains to be written

A custom tile source that carries the token. The point layer as a Canvas2D overlay — for the counts involved, `fillRect` per point is adequate; if it is not, write into an `ImageData` buffer and issue one `putImageData`, which removes per-point API overhead entirely. The bucket hit index from section 2. Labels as DOM text with a greedy priority-ordered collision pass, because DOM text moves as a translated region and gets text-optimised encoding paths, while text rasterised into a canvas is just expensive pixels.

### 4.5 Rendering constraints specific to the medium

Each of these is a consequence of pixels travelling through a video codec rather than general graphics advice.

Points must be **at least two to three pixels and distinguishable by luminance, not only hue** — H.264 modes use 4:2:0 chroma subsampling, so single-pixel coloured marks smear, ghost, or vanish, and a palette varying only in hue is substantially destroyed in transit. Colour scales should be **monotonic in luminance** for the same reason. **Avoid animation, transitions and tile fade-ins**, which are full-screen residuals for no informational gain. **Prefer flat fills to gradients and shadows**, which are cheap on a GPU and expensive through a block codec.

Every claim here is a prediction from how the codecs work, not a measurement on your stack, and behaviour varies with protocol version and configured graphics mode. A half-day spike against a real session is worth more than the rest of this section.

---

## 5. Reuse inventory

| Layer | Profile A | Profile B | Licence | Ours |
|---|---|---|---|---|
| Transport / decode | `apache-arrow` JS | `apache-arrow` JS | Apache-2.0 | — |
| Tile lifecycle | deck.gl `TileLayer` | Leaflet / OpenLayers | MIT / BSD-2 | Tile source, token, Morton arithmetic |
| Viewport and pan | deck.gl `OrthographicView` | Leaflet / OpenLayers | MIT / BSD-2 | — |
| Point rendering | deck.gl `ScatterplotLayer` | Canvas2D | MIT / — | ~50 lines for B |
| Hit testing | deck.gl GPU picking | Bucket grid | MIT / — | ~100 lines for B |
| Label placement | `TextLayer` + `CollisionFilterExtension` | DOM + greedy pass | MIT / — | Hierarchy selection (both) |
| Selection gestures | Overlay path | Overlay path | — | ~100 lines, shared |
| Headless core | Shared module | Shared module | — | All of it |

Roughly eighty percent reuse by volume. What is left is the part that is specific to this system — the Morton tile source, the gated label ladder, the selection semantics — which is the right place for bespoke work to be.

**Verify licences and capabilities before committing.** These are stated from a July 2026 vantage point and both licences and APIs drift.

---

## 6. Profile selection

Probe once at startup for a hardware renderer — a WebGL context plus an unmasked renderer string, rejecting known software implementations — and select the profile from the result, with a manual override for users whose environment lies. Cache the decision per session.

Building Profile B first is defensible: it is the more constrained target, it has no GPU dependency to mask mistakes, and Profile A can be added over the same headless core without touching the service. Building A first is also defensible if the majority of users have GPUs. What is not defensible is building one and assuming the other will drop in — the shared core has to be factored for both from the start, or it will silently acquire rendering assumptions.

---

## 7. Client-side security notes

Small, but each one is a place where a frontend convenience becomes an access-control defect.

**Opaque handles only (I10).** Entity IDs never reach the client, so the handle table is per-session and the client cannot construct a reference the service did not issue.

**Counts come from the service (I2).** Selection counts, cluster sizes and any density figure are masked quantities computed server-side. A count derived client-side from the sample is wrong as well as unsafe, and the two failures are hard to tell apart in a screenshot.

**The client never evaluates a visibility rule (I12, I3).** Filters narrow what is drawn; labels arrive pre-gated. Any client-side logic of the form "hide this unless…" is either redundant or a bug, and in both cases it is evidence that a decision migrated to the wrong side of the trust boundary.

**If server-side rasterisation is ever adopted** (section 8), raster tiles are exactly as sensitive as the counts they derive from: same authorised path, token carried, never a shared cache keyed on coordinates alone. Every geospatial tile server that tried this got it wrong — Tegola refuses to cache anything with a query string, pg_tileserv's documented answer is one service per permission class, GeoServer's tile cache silently ignores read filters the non-tiled path honours.

---

## 8. Options considered and rejected

Recorded because each will be proposed again.

**Density raster as the base layer.** Server-renders a KDE from exact masked counts. Rejected: it replaces identifiable documents with a smooth field, which discards the thing the map exists to let people find — the same criticism the prior-art review levelled at Mosaic's approach to 10⁹ points. Density information is better carried by the points themselves.

**Rendering N points server-side while shipping interaction data for k < N.** Visually rich, cheap client. Rejected on discoverability: some marks respond and some do not, with nothing to distinguish them, which teaches users the tool is unreliable. It also requires the image and the interaction list to be generated atomically under one version pin, or clicks land on the wrong document.

**Grid cells as the interaction unit.** Attractive because a cell is a contiguous Morton range, making its masked count free and the payload bounded by screen area. Rejected for three reasons, the last fatal: a regular lattice cuts across UMAP cluster structure arbitrarily, so "what is in this square" is a semantically empty question; region selection quantises to cell boundaries, which is unusable for lasso and box gestures; and a point drawn larger than one pixel overlaps several cells, so a correct hit region requires a spatial join against the point radius — no longer a contiguous range, no longer free, and needing recomputation whenever point size changes with zoom or device pixel ratio. The cost argument that motivated it does not survive its own correctness requirement.

A uniform grid remains the right *internal* hit-test index (section 2). The rejection is of the grid as an interaction unit, not as a data structure.

**Fully server-rendered points with an interaction overlay.** Retained as a documented fallback rather than rejected outright. Reach for it only if Profile B measurement shows client-side drawing of *k* points is itself too expensive on the VDI host — which would be surprising at these counts. It costs registration accuracy between image and hit targets, a round trip for every appearance change, and the atomicity requirement above.

**deepscatter.** Closest architectural match, CC-BY-NC-SA. NonCommercial turns on the character of the use rather than on whether anything is sold, and ShareAlike would force any release to carry a licence that is not open source by the OSI definition. Independently, its quadfeather tiler assigns points to tiles in fill order with no per-point priority, which is exactly the mechanism the design replaces — so the half that makes it architecturally close is the half that cannot be used. Borrow the manifest-with-per-tile-ranges shape and the sidecar-column split as patterns.

**Apple Embedding Atlas as the Profile A renderer.** The strongest rejected candidate, and the only one whose rejection is not about quality — full rationale at §3.2. In one line: its brute-force, all-data-resident premise is the inverse of this design's, and its throughput advantage applies to an axis the LOD sampler has already removed. It also has no software path, so it could never have served Profile B. Its label placement remains the best in the field and should be ported if needed.

**DataMapPlot as the frontend.** Right labels, wrong architecture — whole dataset shipped upfront, no tiling, no LOD, no server. Its label logic is deck.gl-based, so the relevant parts are reachable through deck.gl directly.

**SVG for points.** Dies in the low thousands of elements. Bunka topped out around 29k with poor interaction.

---

## 9. What to measure

**First, before anything about frame rates:** whether deck.gl's `TileLayer` composes cleanly with `OrthographicView` for non-geographic tiling. Most of the §3.2 argument for deck.gl over Embedding Atlas rests on this, and a negative result reopens that decision rather than merely adding work.

The floor for *k* — the point count at which the map still supports its task, and at which the live highlight during a lasso drag looks representative of the region rather than misleadingly sparse. This is an empirical question for the walking skeleton and it sets a parameter the service uses.

Canvas2D throughput on real VDI hardware, which decides whether `fillRect` per point suffices or the `ImageData` path is needed.

The translate-versus-rasterise asymmetry over the actual remote-display protocol at the actual bandwidth — a half-day spike validating the assumption section 4 rests on, and the cheapest item in the plan.

Whether hierarchical label placement holds up under `CollisionFilterExtension` at several zoom levels with real Toponymy output, rather than with synthetic strings of uniform length. Real cluster labels are long, of wildly varying length, and unevenly distributed — the conditions under which generic collision filtering is weakest. If the result is poor, the fallback is porting Embedding Atlas's placement algorithm as a custom layer (§3.2), which should be scoped as a possibility rather than discovered as a surprise.

---

## 10. Effect on the implementation plan

Phase 1's exit criteria become profile-specific: for Profile A, interactive frame rates with a real mask applied; for Profile B, pan implemented as a transform with zero mid-gesture re-rasterisation, measured over a real remote-display session, plus stepped zoom within a stated budget.

The frontend estimate falls relative to the thin-client profile's assumption that a renderer had to be written. With deck.gl and Leaflet or OpenLayers carrying the tile, viewport and rendering layers, what remains is the headless core, two thin shells and the label hierarchy logic.

The dependency register gains `apache-arrow`, deck.gl and Leaflet or OpenLayers. Apple Embedding Atlas leaves it, retained only as a reference for label placement.

Phase 0 gains the remote-display spike from section 9.
