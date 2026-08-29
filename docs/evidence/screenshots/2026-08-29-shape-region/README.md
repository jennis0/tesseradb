# The region leaf under two principals — stage 4 of the shape work

**Status:** Evidence, never normative. Taken 2026-08-29 on the `artifacts/shape-region` branch against
the Overture one-part ladder (`test_corpora/overture/prepare.py --parts 1`, 4,599,286 places,
17,551 divisions, built with the ladder's own `corpus.toml` — the boundary layer as a `spatial`
polygon layer, no hull declared), served by `run_demo.sh --bundle`, shot by
`clients/ts/viewer/smoke-region.mjs` — the instrument for `selection-operand.md` §8's and
`polygon-membership.md` §8's claims — **headed** (`--headed`, Chromium 1208 on WSLg's display) and
**headless** (swiftshader). The pictures here are the headed run's; both runs' readings are in
`region-headed.json` and `region-headless.json`, and every claim below held in both once the
count was read after the frame settled (the headless run's first pass read it a frame early and
said so — `smoke-region.log` is the headed run's).

What the script checks, read off the store and the map's probe rather than the picture: every
request issued after a selection carries the `region` leaf and **none is a counts-only
`tiles`-form request of its own**; the store's verdict is `x-tessera-region: exact`; the region's
`matched` is typed exact and the panel renders it so; under a second principal the same lasso is
that principal's own count with the same verdict; *filter to this* on a division leaves the map
narrowed to the card's own count.

## The pictures

| | full — top 47 terms | heavy — 41% (46 terms) |
|---|---|---|
| **lasso** | `lasso-full.png` — a seven-vertex lasso over central Mexico at camera depth 12: the marks drawn are the ones inside it and nothing else; the panel says *Matched inside 1,618,017 · Visible inside 1,618,017*, exact (no ≈), *Shown inside 406,358* held marks; the status strip's *1,618,017 matched* is the same number, because it is the same frame | `lasso-heavy.png` — the same lasso, the same verdict, this principal's own **24,500** (the heavy preset holds no Mexican term, so what is inside is what its Caribbean and Central American terms reach), exact, 15,209 shown |
| **filter to this** | `filter-full.png` — Florida (`boundaries/divisions`, 838,679 members visible to this principal) opened and *Filter to this* pressed: the map narrows to the peninsula, the status strip reads *838,679 matched* of 854,494 visible in view, and the selection panel's *Matched inside* is the card's own 838,679 | `filter-heavy.png` — Miami-Dade under the narrower principal, 166,817 members visible to them: the map narrows to the county, *166,817 matched*, the region's count the card's |

## What the run says beyond the pictures

- **One request, no round trip.** Under `full` eleven viewport requests followed the lasso in the
  headed run and every one carried the `polygon` leaf; none was a `k = 0` `tiles`-form request.
  The count is the presented frame's `matched` sum — 1,618,017 against the strip's 1,618,017 — and
  it is typed exact because the wire said `exact` *and* the replica held every tile of the shape's
  extent at the frame's depth (`store.ts`'s two-part rule). The headless run's first reading,
  taken on the first frame carrying the leaf rather than after the settle, was the same number
  under `full` and 24,500 under `heavy` typed **inexact** — the shape's extent not yet held at that
  depth — which is the honest answer at that instant and the reason the script now reads after the
  settle.
- **The verdict is the shape's.** Both principals received `exact` for the lasso; the descent
  fit the default `max_region_cells` (262,144) with room — a hand-drawn lasso at this scale is
  hundreds of boundary cells, not hundreds of thousands.
- **Filter to this is the leaf by artifact, and the count is the card's.** `full`: 861,768
  matched in the fitted view before the press, 838,679 after — the card's *members visible to
  you* exactly, so the 23,089 that left the count are places in view but outside the polygon.
  `heavy`: 264,398 → 166,817, again the card's. The region's *Matched inside* renders **≈** for
  an artifact selection: the store cannot say the replica holds the artifact's whole box at the
  frame's depth — the box is the served artifact's and the fit leaves its margin outside the
  fetched ring — so it does not claim exactness it cannot check, though the wire's verdict was
  `exact` and the number equals the card's.
- **Nothing was refused and nothing drew outside the shape** under either principal; the two
  console errors are the superseded-stream aborts every smoke script excuses.

## What was not measured

`selection-operand.md` §4's cost model — descent, interior and boundary terms — is still modelled,
not measured: this run reads counts and verdicts, not per-stage timings, and the trailer's
`stage_ns` was not enabled. A probe sweeping shape size and vertex count over the synthetic 10⁹
corpus is what would settle it.

## Reproducing

```bash
TESSERA_LADDER=$PWD/target/evidence ~/venvs/ingest/bin/python -m test_corpora.overture.prepare --parts 1
(cd target/evidence/overture && TESSERA_IDENTITY_KEY=<32 hex> ../../release/tessera build)
TESSERA_BIN=target/release/tessera ./run_demo.sh --bundle target/evidence/overture/bundle \
  --terms "$(cat target/evidence/overture/country-terms.txt)" --ranks target/evidence/overture/country-ranks.json --label 'Overture one part'
node clients/ts/viewer/smoke-region.mjs --shots /tmp/region
node clients/ts/viewer/smoke-region.mjs --shots /tmp/region-headed --headed --executable ~/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome
```

The scratch ladder under `target/` was deleted after the run (3.5 GB of prepared data plus a 637 MB
bundle; the disk is tight).
