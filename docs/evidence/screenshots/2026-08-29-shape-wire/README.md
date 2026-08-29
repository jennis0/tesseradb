# The drawn shape under two principals — stage 3 of the shape work

**Status:** Evidence, never normative. Taken 2026-08-29 on the `artifacts/shape-wire` branch against
the Overture one-part ladder (`test_corpora/overture/prepare.py --parts 1`, 4,599,286 places,
17,540 divisions), rebuilt for this run with the taxonomy layer declaring `hull` so that one bundle
carries a **predicate** shape (`boundaries/divisions`, `membership = "spatial"`, polygons) and a
**derived** one (`places/taxonomy`, enumerated, `computed = ["centroid", "box", "hull"]`). Served
by `run_demo.sh --bundle`, shot by `clients/ts/viewer/smoke-shapes.mjs` — the instrument for
`polygon-membership.md` §7's claims — **headed** (`--headed`, Chromium 1208 on WSLg's display)
and **headless** (swiftshader). The pictures here are the headed run's; both runs' figures are in
`smoke-shapes.log`, and every claim below held in both. `shapes-headed.json` and
`shapes-headless.json` are the per-shot readings.

What the script checks, read off the map's probe and the explorer's store rather than the picture:
`/v1/meta` publishes a kind per layer; nothing is drawn at rest; opening an artifact draws exactly
one shape; the shape arrives by identifier at the view's zoom; a predicate shape asked for under
two principals at one zoom is byte-identical; a derived one is each principal's own; a hull has no
holes.

## The pictures

| | full — top 47 terms | heavy — 41% (46 terms) |
|---|---|---|
| **predicate, overview** | `boundary-overview-full.png` — México opened at the overview (camera zoom 1.1, request depth 8): 609 parts, 1,914 vertices after the vertex rule, the coastline and islands drawn as one polygon-with-holes path per part in the opened style; the card says *boundary — the same for every viewer* and *Filter to this* is greyed | `boundary-overview-heavy.png` — the same division, the same shape (`#733089`, identical bytes under both), fewer members counted beside it |
| **predicate, city** | `boundary-city-full.png` — Mexico City at camera zoom 9.7 (request depth 15): 938 divisions served, Chicoloapan opened and drawn among the marks, 55 vertices at this zoom | `boundary-city-heavy.png` — the same borough (`#057810`, identical bytes) drawn alone: this principal sees 1 of its members, and the outline is the boundary, not the member |
| **derived, overview** | `hull-full.png` — the `services_and_business` root of the taxonomy opened: 12 parts, 205 vertices, the hull of the 941,416 members this principal sees | `hull-heavy.png` — the same cluster (`#312641`) under the narrower principal: 18 parts, 210 vertices, a visibly different hull over 437,731 members — the derived kind moves with the principal, the predicate kind does not |

The overview pictures show the one-part corpus's frame: Latin America and the United States'
places occupy a corner of the world extent, so México's outline sits at the top-left of the map
pane at the overview. The México shape is served under the 2,048-vertex guard without the guard
firing (1,914); the United States at the same zoom is 683 parts at exactly 2,048 — the guard
fired, which the trailer's `stage_ns` companion counts (`smoke-shapes.log` from an earlier run of
the same script recorded it; the final pair chose México because the broadest principal is asked
first).

## What the run says beyond the pictures

- The vertex rule reads the camera zoom the shape was asked at, not the request depth the driver
  picks per principal: Chicoloapan is 55 vertices at zoom 9.7 under `full` and 71 under `heavy`
  in the same picture pair only because the two asks landed at slightly different fits; asked by
  identifier at one fixed camera box the bytes agree (`smoke-shapes.log`, *the same shape, across
  principals*).
- The request depth under `heavy` at the city fit is 3 — the driver saturates its mark budget on a
  principal who can see two points there — while the camera zoom is 9.7. The shape is asked at the
  camera zoom, so it is the same drawing as `full`'s; the probe's `depth` is not the number to read
  for this.
- Headless only, three `decoder closed` page errors: the store's decode worker rejecting the
  decodes still pending when the principal switch closed it — the same superseded event the smoke
  scripts already excuse, seen only where the software-rendered decode is outrun. Headed, none.
- Nothing draws at rest under either principal on either layer (`drawn=0` before every open).

## Reproducing

```bash
TESSERA_LADDER=$PWD/target/evidence ~/venvs/ingest/bin/python -m test_corpora.overture.prepare --parts 1
# add `[layer.content] computed = ["centroid", "box", "hull"]` to places/taxonomy in the copied corpus.toml
(cd target/evidence/overture && tessera build)
TESSERA_BIN=target/release/tessera ./run_demo.sh --bundle target/evidence/overture/bundle \
  --terms "$(cat target/evidence/overture/country-terms.txt)" --ranks target/evidence/overture/country-ranks.json --label 'Overture one part'
node clients/ts/viewer/smoke-shapes.mjs --shots /tmp/shapes
node clients/ts/viewer/smoke-shapes.mjs --shots /tmp/shapes-headed --headed --executable ~/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome
```

The scratch ladder under `target/` was deleted after the run (16 GB of prepared data plus a
637 MB bundle; the disk is tight).
