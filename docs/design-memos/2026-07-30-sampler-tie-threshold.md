# Sampler tie-threshold confirmation (2026-07-30)

**Confirms:** `docs/design-memos/2026-07-30-priority-as-identity-prefix.md`'s requested
measurement — "over the existing 10⁹ k-sweep, count the (tile, principal) pairs with
V > 2×10⁶."

## Route taken

Checked both prior baselines first, as instructed:

- `docs/superpowers/plans/bench-baselines/2026-07-29-1e9-k-sweep.json` — only
  percentile latency/points-returned stats at a single grant width (w=10⁴); no per-tile
  `visible` values at all.
- `probes/work-correlation.json` — carries `sigma_visible` per request, but only summed
  over a whole viewport's tiles, never per tile, and again only w=10⁴.

Neither lets the per-(tile, principal) `V` be recovered. **A new server run was
required.** Booted `tessera serve` once against the existing `/tmp/tessera-1e9` bundle
(not rebuilt), authorised 9 grant widths spanning the achieved-coverage range 0.0057% to
100% (`w = 5, 20, 100, 500, 2000, 8000, 16000, 30000, 47968` out of a 47,968-term
dictionary — the top width is the whole dictionary), and for each (width, zoom) pair
issued one full-extent viewport query (`k=1`, since `visible`/`matched` are full masked
counts computed before the k-cap and are unaffected by k), decoding the per-tile
`visible` column directly via `reference/oracle/wire.py::decode_viewport`. Zooms 0–8
probed (4^8 = 65,536 tiles at the deepest depth swept). Script:
`scripts/measure_sampler_tie_threshold.py`. Raw output:
`probes/sampler-tie-threshold.json`.

Achieved coverage was measured directly from each grant's zoom-0 (whole-extent,
single-tile) query rather than assumed linear in term count — it is not: w=8000 (16.7%
of the dictionary) already reaches 30.6% visible-coverage, w=30000 (62.5% of dictionary)
reaches 72.1%.

## Headline result

Over 560,761 (tile, principal) pairs swept (9 grant widths × zooms 0–8):

- **1,110 pairs have V > 2×10⁶ — 0.198% overall.**
- This overall figure is dominated by the huge tile counts at fine zoom (depths 7–8
  alone contribute 523,352 of the 560,761 pairs, nearly all resolved) — the fraction at
  coarse zoom, where the defect actually bites the default overview, is far higher (see
  below).

## Breakdown by zoom depth (aggregated across all 9 grant widths)

| depth | pairs | exceeding V>2×10⁶ | fraction |
|---|---|---|---|
| 0 | 9 | 7 | 77.8% |
| 1 | 35 | 24 | 68.6% |
| 2 | 132 | 81 | 61.4% |
| 3 | 503 | 255 | 50.7% |
| 4 | 1,886 | 431 | 22.9% |
| 5 | 7,169 | 237 | 3.3% |
| 6 | 27,675 | 58 | 0.21% |
| 7 | 107,287 | 11 | 0.010% |
| 8 | 416,065 | 6 | 0.0014% |

**Crossover depth:** the aggregate majority-exceeding fraction (>50%) holds through
depth 3 and drops sharply at depth 4 (61%→23%). This is coverage-dependent: at w=8000
(≈31% coverage) depth 3 is 89% tie-dominated and depth 4 falls to 15%; at w=47968 (100%
coverage) tie-domination persists further, with depth 4 still at 68% and depth 5 down to
13%. The crossover sits at **depth 3–4** for the coverage range that matters most (head
principals, tens-of-percent coverage), consistent with the memo's own depth-3 estimate.

## The memo's two specific predictions

1. **"Tie-dominated from roughly depth 3 upward [towards the root] at ~25% coverage"** —
   confirmed. The closest measured width is w=8000 at 30.6% coverage: depth 0–2 are
   100% tie-dominated, depth 3 is 89% (57/64 tiles), and it collapses to 15% at depth 4.
   The 25%-coverage prediction's depth-3 boundary is right where the measured collapse
   happens.

2. **"Resolution at ~0.01% coverage"** — confirmed. The closest measured width is w=5 at
   0.0057% coverage: **zero** tiles exceed the threshold at any depth 0–8, including
   depth 0 (whole-extent single tile, V=57,117). w=20 at 0.0295% coverage is likewise
   fully resolved at every depth swept.

Both predictions held.

## Scope note

This measurement only counts how often the sampler's priority resolution breaks down
(V > 2×10⁶); it says nothing about the proposed `priority = high16(tessera_id)` fix,
`sort_batch`, or the comparator — those are out of scope here and owned elsewhere per
the coordinator's instructions.
