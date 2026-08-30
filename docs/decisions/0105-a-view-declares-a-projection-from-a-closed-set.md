# 0105 — A view declares a projection from a closed set, and states its frame in degrees

**Date:** 2026-08-30 · **Status:** Settled (owner rulings, 2026-08-29/30)

## The decision

**A view declares a `projection`**, from a closed enumerated set: `web_mercator`,
`equirectangular` with `plate_carree` and `gall_isographic` as aliases onto its standard parallel,
and `none`. `none` is the default, so a corpus with no geography declares nothing.

**The service transforms and does not negotiate.** No caller-supplied projections, no datum shifts,
no national grids: those need grid files that are versioned data and change answers between
releases, and the set of them is unbounded. Adding an entry later is ordinary; changing one
re-places every point built under it.

**Every projection normalises to the unit square, x east and y south.** y runs south because an XYZ
tile `y = 0` is the northernmost row and the cell grid's y increases with the tile's — measured
against deck.gl's own tileset, not assumed — while EPSG:3857's northing increases northward. A
frame declared symmetrically in metres is mirrored against every basemap, and cannot be repaired
afterwards because a frame requires `y_max > y_min`. The negation is part of the projection.

**The extent is written in longitude and latitude**, on the input side of the transform, and is
snapped outward to the smallest enclosing 2^k-aligned square. A caller stating a projected frame
would have to project their own corners to discover what to write — the work the service has just
taken on, handed back at the one point where getting it wrong misplaces every stored position. This
holds only because every entry is **cylindrical**, so a longitude/latitude rectangle is still a
rectangle after projection; it is a further reason the set stays cylindrical.

**The grid is filled rather than letterboxed**, which is what keeps the equirectangular family one
entry: normalised, every standard parallel stores identical positions, and the parallel survives
only as the aspect a client draws at. Keeping cells square in the projected plane instead would put
the parallel into the stored format and cost up to half the grid.

## What it rejects

**Letterboxing to keep projected-plane cells square.** It buys the ability to move the latitude at
which ground cells are square, and costs up to half the north-south resolution and half the grid —
and makes the standard parallel part of the format.

**An exact frame spelling, `{ z, x, y }`.** The box with its outward snap reaches every frame such
an address could name. The caller who wants one is matching a foreign tile scheme, which nothing in
reach does.

**An equal-area entry.** Argued for — Web Mercator distorts area and this system's product is
counts — and held rather than built, Lambert cylindrical equal-area being the candidate.

## What it costs

A region crossing the prime meridian or the equator takes the world frame and no sub-square,
those being the boundaries at the first offset: the United Kingdom gets no sub-square, nor does
Kenya. Web Mercator's area distortion cannot be corrected inside the engine, masked counts being
bitmap cardinalities, so it is a caveat stated beside a density figure. Its domain cut at
±85.0511287798066° is data loss rather than distortion, and the clipped points are counted
separately from clamped ones, having a separate cause.

`projections.md` is the design; `configuration.md` §1 is the surface.
