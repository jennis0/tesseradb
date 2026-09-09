# 0137 — θ is anchored on the occupied-tile count, not on `4^d`

**Date:** 2026-09-09 · **Status:** Settled (owner ruling) · Built; design
[`architecture.md`](../design/architecture.md) §7.2 r62, [`contracts.md`](../design/contracts.md)
r91, `crates/tessera-engine/src/occupancy.rs`

## What this answers

Design §7.2 anchored the selection threshold as `P_0 = m_target · 2⁶⁴ / V_total` and progressed it
`P_d = P_0 << 2d`, so `θ_d = m_target · 4^d / V_total`. The `4^d` encodes an assumption: that a
viewer's visible items spread over about `4^d` occupied tiles at depth *d*, which makes the mean
occupied tile draw `m_target` marks at every depth.

That assumption fails past the shallowest depths on every corpus measured. Occupied tiles grow by
**at most 4× and typically 2.0–3.5× per level** (measured 2026-09-09 on `treeoflife` at 2.33 × 10⁸
rows, `treeoflife-1m`, `geonames` and `medcpt-10m-abs`; ratios at full coverage span 1.64–4.00;
[probe](../../probes/2026-09-09-theta-occupancy-anchor/README.md)). The full 4× occurs only where
the data really is space-filling — `geonames` holds it to depth 3 and `treeoflife-1m` to depth 1 —
and every corpus falls below it from depth 4 down. That is the shape of the correction rather than
an exception to it: where `N_occ(d) = 4^d` the new anchor reproduces the old one exactly. Marks
per occupied tile therefore climb with depth instead of holding at `m_target`, and the cap
(`k_max_marks`, 500) saturates every tile from depth 4–6 downward. On `treeoflife` at depth 12, θ
had reached 1.15 — saturated — and **289 of 289 tiles** in a dense view drew exactly the cap while
their true visible counts spanned **4,958 to 336,047**. A viewer sees the tile grid rather than
the density: square artefacts with straight axis-aligned seams.

§7.2 predicted this failure and priced it as an accepted residual, in a paragraph that named the
occupied-cell count `O_d` and gave the inflation as `m_target · 4^d / O_d`.

## The decision

**θ is anchored on the measured occupied-tile count:**

    θ_d = m_target · N_occ(d) / V_total
    N_occ(d) = the number of depth-d tiles holding at least one row visible to this session

`N_occ(d)` is counted inside the session's own **composed** mask and over the whole view. `P_d`
floors once over the whole product.

**There is one sampling strategy.** The `4^d` path is deleted. No configuration key, no per-view
anchor and no client-supplied θ selects it.

## Why this is the right anchor and not a patch

`N_occ(d)` *is* the `O_d` §7.2's residual paragraph named, so the inflation factor that paragraph
priced becomes 1 by arithmetic rather than by tuning. A tile of *n* visible items draws `θ_d·n`
marks, so the marks summed over the occupied tiles are `θ_d · V_total = m_target · N_occ(d)` and
the mean occupied tile draws `m_target` marks at every depth, whatever shape the data has.

Three properties the design relies on, each stated in the code where it is relied on.

**Monotone from the grid, not from a clamp.** Every occupied depth-*d* tile has at least one
occupied child, and children of distinct parents are distinct tiles, so `N_occ(d+1) ≥ N_occ(d)` and
θ is non-decreasing in depth. §7.2's nesting proof needs that and gets it structurally. An
implementation may not add a running maximum over depth: it would conceal a miscount rather than
prevent one.

**A tightening at every depth.** `N_occ(d) ≤ 4^d` always, so `θ_d` is at or below what the old
progression gave. No viewer is served more marks than before at any depth, with equality at depth 0
and wherever the data really is space-filling.

**Viewport-invariant.** `N_occ` is a function of `(mask, view, depth)` and of no viewport quantity,
so a pan does not move θ. It moves with zoom, which is what makes the per-tile expectation
depth-stable, and nesting survives that because θ is monotone.

## I2: it is the composed mask

`N_occ` carries the signal `V_total` carries, so it takes the same rule. A viewer can aggregate
mark counts across tiles, solve for θ through the published `theta_target_marks`, and difference it
against the per-tile `visible` §7.1 discloses exactly. Anchoring either factor on the cached
pre-overlay `RowProjection` would hand back a running estimate of how many of the viewer's own items
have been denied — a count of items outside `M_auth`. Both factors are therefore taken from the
composed `EffectiveMask`, and a suppression that empties a tile lowers `N_occ`.

Both are also filter-blind (**I12**): the anchors are taken before a request's filter is evaluated,
so θ does not move as a viewer types.

**No new leak-register row.** Solving through `theta_target_marks` now recovers `N_occ(d)` as well
as `V_total`. Counting the non-empty tiles of a full-extent request at depth *d* returns `N_occ(d)`
exactly, §7.1 omitting an empty tile, so it is obtainable without the constant and
[decision 0023](0023-derivable-quantities-are-not-disclosures.md) applies. Appendix C's **C18**
mitigation is amended to say so.

## How it is computed, and the routes that were rejected

Five routes were profiled. The shipped one walks the mask's visible runs and, inside a run, hops
from one occupied tile's Morton boundary to the next by exponential search — `O((runs + N_occ(d)) ·
log)` per depth. It is evaluated **lazily, per requested depth**, and memoised on
`(token_id, view, depth, segments_version, overlay_version, fragment identity, fragment watermark)`.
Measured single-depth cost at *d* ≤ 12 was 0.12–13.7 ms across 1M–13.5M-row corpora, against 21–191
ms for all seventeen depths; a session touches a handful.

**Endpoint arithmetic is wrong, not merely approximate.** Crediting `tile(end) − tile(start) + 1`
per run counts the tile indices a run spans rather than the occupied tiles inside it — a run's
Morton codes are not dense in tile-index space. Measured against a linear-scan oracle it is wrong
by 1.6×–225× at depth 12. The gallop is what makes the walk exact.

**A descent over the tile tree is not affordable.** Visiting occupied nodes breadth-first pays a
Morton binary search per node plus a mask question per child, and the node count is the quantity
being computed: reaching depth 16 visits `Σ_d N_occ(d)` nodes where the run walk visits one depth's
runs. Measured at 9.2–59 s for seventeen depths over 1M–13.5M-row corpora, and the variant using a
short-circuiting emptiness test rather than a full count stayed far behind.

**Extrapolation from an exact shallow prefix is rejected.** Fitting a growth factor to depths 0–*d₀*
and projecting it deeper errs to **+950%**: `N_occ` saturates toward the mask's cardinality on
smaller corpora, and a fitted factor overshoots badly exactly where the cap then binds.

## What it changed

Measured against this implementation on `treeoflife` at 2.33 × 10⁸ rows, depth 12, the dense view
above (289 tiles, true visible counts spanning **5,159 to 252,262**): capped tiles fall from
**289 of 289** to **25 of 289**, and the spread of per-tile sampling rates falls from **48.9×** to
**5.19×**. Over the 264 tiles the cap no longer binds on, the spread is **1.77×** — quote that
figure only against the same subset; the 48.9× is over all 289. The old figure is exact rather than
re-measured: under the `4^d` anchor every tile served exactly the cap, so its all-tiles spread is
`max(visible) / min(visible)` by arithmetic.

**The walk's cost at that scale, measured the same day**: 33–183 ms for the full principal and
36–83 ms for a 0.9% principal, across depths 4 to 16, one-off per `(session, depth)` behind the
memo. That is a wall-clock delta between a cold and a warm request over HTTP, so it is an upper
bound including any other per-depth cold state, not the `theta_occupancy_ns` stage timer. Against
1.6–2.2 ms on `treeoflife-1m` it is sublinear in a 233× larger corpus: the concern that a ~932 MB
Morton column would punish the walk's random access did not materialise.

`clients/ts/core/src/budget.ts` needs no change. Its average-model fallback assumes
`marks ≈ m_target × tiles`; this decision is what makes that assumption true on a clustered corpus.

The reference oracle counts `N_occ` by bucketing every row's tile recomputed from the source
geometry, where the engine gallops the stored Morton column. The differential's independence
therefore moves from the θ arithmetic — which no longer has a recurrence for the two
implementations to write differently — to the count itself, which is the stronger place for it.
