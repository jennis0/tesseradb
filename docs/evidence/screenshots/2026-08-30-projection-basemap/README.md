# A basemap under a projected corpus, and none under one no scheme addresses

**Status:** Evidence, never normative. Taken 2026-08-30 on the `projections/meta` branch (phase F of
the projections work — [`projections.md`](../../../design/projections.md) §9), shot by
[`clients/ts/viewer/smoke-basemap.mjs`](../../../../clients/ts/viewer/smoke-basemap.mjs) headless
(swiftshader, Chromium 1208) against the demo viewer at `vite dev`.

**What the pictures are for.** `/v1/meta` now publishes what a view is a picture of, and the field
that decides whether a basemap may be drawn is `tile_scheme`. A map whose numbers are right and
whose basemap is offset has failed, and no assertion shows that — only a picture does.

## The corpus, twice

224,398 GeoNames places — every 60th row of `allCountries.txt` from the 2026-08-27 snapshot, as
`prepare.py` here writes it: an entity id, a longitude, a latitude and a country code. Nothing is
projected upstream; the build projects it, from degrees, which is the whole point of the corpus.
`corpus.toml` here is the `web_mercator` declaration; the second bundle is that file with
`projection = "equirectangular"` and `lat = [-90, 90]`, and is otherwise identical — **the same
points, the same access terms, one word different**, so nothing but the projection can account for
the difference between the two pictures.

The access side is synthetic and says so: a point's term is its own country code, and the principal
shot here holds 210 of them — 223,236 of the 224,398 places.

## What `/v1/meta` published

`meta-views.json` here is the two `views` blocks verbatim.

| | `projection` | `world_aspect` | `tile_scheme` | `tile` |
|---|---|---|---|---|
| the Mercator bundle | `web_mercator` | `1.0` | `"xyz"` | `{z: 0, x: 0, y: 0}` |
| the equirectangular bundle | `equirectangular` | `2.0` | `null` | `null` |

**Both frames are `[0, 1]` on both axes** — the whole-world square, as aligned as a frame gets. That
is the point of the pair: alignment is identical and the answer is not.

## The pictures

| | |
|---|---|
| `mercator-world.png` | the whole world. The marks trace the continents and the OSM basemap sits under them; the basemap's square is the map's world square, because the frame **is** the scheme's `0/0/0` tile. Antarctica is basemap and no marks, GeoNames holding few Antarctic features and Web Mercator's domain stopping at ±85.0511° — 10 of this sample's points are beyond it (1 north, 9 south) and are clipped onto the frame's edge |
| `mercator-europe.png` | Britain, Ireland, the Low Countries and northern France at camera depth 8. **This is the picture that carries the claim**: the marks fill the land and stop at the coastline — the Bristol Channel, the Wash, the Dutch coast, Brittany — with no offset visible at a scale where one cell of the whole-world frame is about 600 m |
| `equirectangular-world.png` | the same 224,398 places under `equirectangular`, drawn with **no basemap at all**. Antarctica now carries marks (the projection reaches the poles), Greenland is smaller, and the world is squashed into the square because both axes use their full 16 bits — the filled grid of §5.2 |

## What the run says beyond the pictures

`mercator.json` and `equirectangular.json` are the two runs' readings.

- **The basemap decision is the published field's, in both directions.** The script asserts a
  basemap layer is present where `tile_scheme` is `"xyz"` and **absent** where it is `null`, and the
  viewer's own code (`clients/ts/viewer/src/basemap.ts`) reads nothing else — no dataset entry says
  whether its corpus is geographic and nothing inspects the extent. The equirectangular run drew
  none although its frame is a square of a square tiling: that is the case a boolean gets wrong,
  and it would have put a Mercator basemap under a corpus that cannot line up with one.
- **The inversion is checked numerically, because a picture cannot check it.** Six cities on both
  sides of the equator and the meridian are put through the published frame to the cell they would
  be stored in and inverted back: worst drift **0.0052°** under Mercator and **0.0052°** under
  equirectangular, against a whole-world cell of 0.0055° of longitude. A frame mirrored north-south
  round-trips perfectly on the equator and puts London in the southern ocean, which is why the
  places are off it.
- **The basemap is composed from the published tile address**, not from an assumption about the
  frame: the image is the `2^4 × 2^4` tiles that subdivide `view.tile`, so the same code serves a
  whole-world frame and an aligned sub-square. The tiles come from OpenStreetMap's own server,
  which is right for a screenshot and **wrong for a deployment** — its usage policy forbids
  production load, and the answer there is a self-hosted basemap (client-interaction §12).
- No console errors in either run.

## What these pictures do not show

The corpus carries no attributes, no layers and no prose, so the legend, the filters and the
artifact panels are empty in every shot — this is the projection surface and nothing else. And the
frames here are both the whole world: the **sub-square** address `/v1/meta` publishes for an
aligned sub-square frame is covered by test rather than by picture
(`crates/tessera-server/tests/meta_projection.rs`).
