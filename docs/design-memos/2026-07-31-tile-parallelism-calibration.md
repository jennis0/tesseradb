# Tile-loop parallelism: what the three-scale calibration found, chose, and recommends

**Status:** landed on main (merge 55eab5f). Constants in `crates/tessera-engine/src/viewport.rs`;
raw sweep data and the full process history in the concurrency worktree's SDD workspace
(`.superpowers/sdd/i-d-like-you-to-jiggly-cupcake/`, `bench-runs/recalibration/` +
`calibration-report.md`). This memo is the decision record; read nothing else unless auditing.

## Results

**1. B9's tiered decode made pre-mask row count a broken predictor of work.** A natural zoom-4
viewport at 10⁹ spans ~320 M rows-in-range and completes selection **serially in ~135 µs** — the
decode walks the mask at container granularity, so span no longer measures work (the design's own
cost model, "O(containers touched), not O(cardinality)", now holds on the selection path). The
original `SERIAL_FALLBACK_MAX_ROWS = 200_000`, fitted pre-B9, misclassified by three orders of
magnitude.

**2. Serial wins every natural viewport shape, at every scale, under both grant densities.**
Sweeps at 2.42 M / 10⁸ / 10⁹, zooms 4–14, dense and sparse grants: serial is 1.4×–10× faster than
the pool on all of them. The fan-out's ~0.5–1 ms scheduling overhead dwarfs post-B9 tile work.

**3. Parallelism pays in exactly one regime — whole-corpus sweeps — and pays well.** Full-extent
shapes (the zoom-0/1 "first paint of the whole map"): **0.28× at 10⁹ over HTTP (3.6× faster)**,
0.54–0.72× in-engine at z2–z5. This is also the shape a deployment most wants fast.

**4. Validation after re-calibration:** small-viewport `threads=default` vs `threads=1` ratios
**1.07 / 1.08 / 0.98** at 2.42 M / 10⁸ / 10⁹ (was 2.5–3× *slower* before), with the full-extent
win retained. The regression is gone; the win survives.

**5. A telemetry defect was found and fixed on the way:** `StageTimings.rows_in_ranges` (the C4
leak-register numerator's mask-independent term) had become mask-dependent in the tile-loop
restructure — mask-empty tiles' counts were discarded, a 26× under-count under sparse grants.
Restored to the serial prefix (exact pre-restructure semantics); regression test pins
mask-independence and serial/parallel agreement.

## Choices

| Constant | Value | Argument |
|---|---|---|
| `SERIAL_FALLBACK_MAX_ROWS` | 200 000 → **500 000 000** | The serial-favouring family (natural viewports, up to ~355 M rows at 10⁹) and the parallel-favouring family (full-extent, from ~10⁸ rows) **overlap in row count** — no single threshold separates them perfectly. 500 M sits above every measured natural shape with margin and below full-extent-at-10⁹. Cost: the fan-out is dormant below ~full-corpus-at-10⁹ scale. Deliberate, safety-first. |
| `TILE_PAR_MIN_LEN` | **8 (unchanged)** | Re-swept 8/32/128/512 at all three scales. 8 is decisively best on the shapes that actually reach the pool (full-extent z2–z5 — few, enormous tiles want fine-grained stealing). Named exceptions where larger chunks won (full-extent z1, natural z4) do not ship above the threshold. |
| Test reachability | `#[doc(hidden)]` per-`Engine` threshold override, setter gated behind `bench-timing` | With the threshold at 500 M no fixture reaches the pool, which would have made every byte-equality test silently serial-vs-serial. The override keeps the parallel branch's invariant tests real without adding a user knob. The backing field and its one relaxed load per request exist in every build (documented; gating them would fork the hot path for a negligible saving). |

The parallel machinery is **kept, not deleted**: it is load-bearing for whole-corpus sweeps today
and is the mechanism any future re-calibration re-arms with one constant.

## Recommendations

1. **A corpus-size-aware threshold is the complete fix** the 500 M compromise approximates. If
   full-extent latency at 10⁸-class corpora comes to matter (today it measures ~parity), derive
   the threshold from the bundle's row count at `Engine::open` rather than a universal constant.
   Needs its own argued design; not landed unprompted.
2. **Re-run `calibration_sweep`/`min_len_sweep` whenever the cost model moves**: a new box, a
   corpus with different container occupancy, or any change to the selection decode (a B9-class
   change invalidated the first calibration within a day). The tools are parameterised for
   arbitrary bundles; a sweep is minutes.
3. **The mis-prediction bound is small but real:** the predictor is pre-mask by design, so mask
   density is unmodelled; the measured worst case in the ambiguous band is ≤ ~1.3× on a ~1 ms
   request. Acceptable; recorded so nobody rediscovers it as a bug.
4. **Unrelated but queued behind this work** (workstream follow-ups, in the SDD ledger): bounding
   the two in-memory single-flight caches before sustained-load/large-corpus deployment; an
   inline-below-threshold server fast path to recover the ~25% small-request closed-loop
   throughput still paid to the off-reactor hop; widening `shed_total` (or documenting it) to
   count cold-build 429s.
