# How the density underlay scales toward full resolution

**Date:** 2026-08-01 · **Harness:** `crates/tessera-bench/src/bin/underlay_route.rs`
**Raw:** `ul2m4.csv` (single-tile), `ul1e8-full.csv` and `ul1e9.csv` (single-tile **and**
whole-viewport blocks) · **Machine:** WSL2, 47 GB, 14 compute threads

*A first draft of this document quoted whole-viewport figures for 1e8 and 1e9 that were never
captured to a file — the harness prints two CSV blocks and only the first was saved. Independent
review caught it. Both are now committed and every figure below is reproducible from them.*

`max_underlay_offset` and `max_underlay_cells` are plain `EngineConfig` fields, so the **existing**
per-sub-cell route can be driven past both guards with no engine change. That is what was measured:
the question "is today's algorithm affordable at full resolution?" is answered before anyone writes
a second one.

---

## The headline: the premise was wrong, and the plan shrinks

Client-interaction §9's annotation reasoned that full resolution means offset 9 (512 × 512
sub-cells per tile), extrapolated ~0.5 s per tile from a single data point, and concluded that both
the algorithm and the wire encoding must be replaced.

**Two of those three claims do not survive.**

### 1. "Full resolution" is a property of the viewport, not of a tile

This is the correction that collapses most of the work. A view showing *T* tiles across a ~10⁶-pixel
screen needs ~10⁶/*T* sub-cells **per tile** — so the offset that matters *falls* as the tile count
rises. And Task 1 fixes the tile count at `B / m_target`, a constant.

At the operating point Phase 1 creates — depth 6, 4,096 tiles — one screen-pixel per sub-cell is
`10⁶ / 4096 ≈ 256` cells per tile, which is **offset 4: exactly today's cap**.

The MVP looked blocky because it requested *one* tile at depth 0–3 and asked for 64 sub-cells over
the entire screen. The offset cap was never the binding constraint; the tile count was. **Fixing
the depth choice fixes the underlay resolution as a side effect.**

### 2. The per-sub-cell algorithm is affordable at the operating point — but the "100× pessimistic" claim was measuring a different regime

*Corrected 2026-08-01 after independent review.* The annotation being refuted (client-interaction
§9) reasoned about **low zoom**, explicitly: *"loses badly at low zoom where the visible set is
5 × 10⁸."* The table below is **zoom 6**, where a tile is 0.3% full. Compared like with like:

| fixture | zoom | offset 9, underlay |
|---|---|---|
| 2m4 `everything` | 2 | **152 ms** |
| 1e8 `everything` | 2 | **417 ms** |

Against an extrapolation of ~0.5 s, the annotation was within ~20% **for the regime it was talking
about**. It was not wrong by 100×; the first draft of this document compared a shallow-zoom
prediction against a deep-zoom measurement and declared a 100× win. The "cost per cell falls 18×"
claim has the same defect: 18× is the zoom-6 figure, and at zoom 2 the fall is 3.8× (2m4) and 2.2×
(1e8) — because the saving *is* emptiness, which a deep tile has and a shallow one does not.

**What survives is the conclusion, on different grounds.** The operating point Phase 1 creates is
depth 6+, where tiles are sparse and the route is cheap; the expensive shallow regime is the one
Phase 1 stops visiting anyway. So the existing route is affordable **where it will actually be
used** — which is enough to withdraw the second algorithm, but is not the same as the route being
cheap everywhere, and does not close the route-chooser question on its merits. **The alternative
single-pass route was never measured**, so Task 6 is *deferred pending evidence*, not refuted.

Measured, one tile, 2m4, `everything`, **zoom 6**:

| offset | cells/tile | emitted | underlay | µs per cell evaluated |
|---|---|---|---|---|
| 3 | 64 | 53 | 22 µs | 0.344 |
| 5 | 1,024 | 209 | 102 µs | 0.100 |
| 7 | 16,384 | 482 | 474 µs | 0.029 |
| 9 | 262,144 | 765 | **4.9 ms** | 0.019 |

Full resolution on **a deep, sparse tile** costs 4.9 ms, and cost per cell falls 18× from offset 3
to 9 because deeper sub-cells search narrower ranges and empty cells are nearly free. The cost is
**sub-linear in cells** here, which the "×4 per level" reasoning did not anticipate — but read this
table with the correction above: it is the deep-zoom regime, and the shallow one behaves differently
(3.8× at 2m4 zoom 2, and 152–417 ms rather than 4.9 ms).

### 3. What it actually costs at the operating point

1e8, `everything` (53.3 M visible), full extent, whole-viewport request:

| depth | tiles | offset | total sub-cells | emitted | wall |
|---|---|---|---|---|---|
| 6 | 4,096 | — | — | — | 74 ms |
| 6 | 4,096 | 2 | 65,536 | 42,495 | 96 ms |
| 6 | 4,096 | 3 | 262,144 | 154,196 | 87 ms |
| 6 | 4,096 | **4** | **1,048,576** | 551,607 | **179 ms** |
| 6 | 4,096 | 5 | 4,194,304 | 1,866,807 | 474 ms |

**A full-screen-resolution underlay — 1.05 M sub-cells, one per screen pixel — costs 179 ms against
74 ms without it, in the same request that fetches the marks.** That is a real cost and not a
negligible one, but it is affordable and it needs no new algorithm.

Underlay CPU at that cell is 1.52 s against 179 ms wall: the existing parallel sweep is giving
~8.5× and is what makes this viable.

---

## What survives, and what is withdrawn

**Withdrawn — but on narrower grounds than the first draft claimed:**

- *"the per-sub-cell evaluation breaks before full resolution"*. Not at the operating point: 179 ms
  for a screen's worth at 1e8, 466 ms at 1e9. It **does** get expensive at shallow zoom (417 ms for
  a single tile at 1e8 zoom 2), which is what the annotation actually said — see ruling (2). Since
  Phase 1 stops requesting shallow depths, the route survives where it is used. **Task 6 is
  deferred, not refuted**: the single-pass alternative was never measured, and at 1e9 the underlay
  doubles the request, which is exactly the margin a route chooser would be for.
- *"full resolution means offset 9"*. It means `screen_pixels / tiles`, which at the Phase 1
  operating point is offset 4 — the current cap.

**Survives, and is now the whole of Phase 2:**

- **`max_underlay_cells = 8,192` is the binding guard**, exactly as annotated. One screen's worth is
  1,048,576 — **128× the current limit.** This is the one number that must move, and it is a
  disclosure-neutral availability bound, so raising it is a sizing decision rather than a design one.
- **The sparse encoding does lose at the operating point, by 2–4×.** At depth 6/offset 4 the fill
  ratio is 551,607 / 1,048,576 = **53%**, so `(cell u64, count u64)` costs 8.8 MB where a dense
  `u32` raster costs 4.2 MB and a `u16` raster 2.1 MB. Worth doing — but note the fill ratio is
  strongly regime-dependent (0.3% at 2m4/offset 9, 73% at 1e8/depth 2/offset 9), so the encoding
  should be **negotiated, not switched**, and the sparse path must stay for sparse principals.

**New, and not in the plan at all:**

- **Cost is dominated by cells *emitted*, not cells evaluated.** Compare 1e8 depth 6: offset 3
  evaluates 202 k cells for 87 ms, offset 4 evaluates 809 k for 179 ms — 4× the cells for 2× the
  time, because emptiness is cheap. Any future optimisation should target the emitted set.
- **Underlay cost tracks the *principal*, hard.** At depth 6/offset 4, `medium` (19 k visible) pays
  8.2 ms of underlay; `everything` (53 M) pays 1.52 s of CPU. A fixed cell budget is therefore not
  a fixed cost budget, and the guard should be sized for the broadest principal a deployment
  serves — the §8.1 per-principal support surprise, appearing in a third place.

---

## 1e9 — and it settles the encoding decision

`everything` (518.5 M visible), full extent, whole-viewport request:

| depth | tiles | offset | total sub-cells | emitted | fill | wall |
|---|---|---|---|---|---|---|
| 6 | 4,096 | — | — | — | — | 222 ms |
| 6 | 4,096 | 2 | 65,504 | 65,430 | 99.9% | 240 ms |
| 6 | 4,096 | 3 | 262,016 | 261,143 | 99.7% | 287 ms |
| 6 | 4,096 | **4** | **1,048,064** | 1,039,247 | **99.2%** | **466 ms** |
| 6 | 4,096 | 5 | 4,192,256 | 4,107,471 | 98.0% | 1,088 ms |

**A full-screen-resolution underlay at 10⁹ costs 466 ms against 222 ms without it** — the underlay
roughly doubles the request. Heavy, but it is one request and it is the whole density field.
Quarter resolution (offset 3, 262 k cells) costs 287 ms, so there is a genuine
resolution-versus-latency trade for the client to expose rather than hard-code.

**The fill ratio settles the encoding, and it settles it the other way from 2m4.** Across regimes:

| regime | fill |
|---|---|
| 2m4, one tile, offset 9 | 0.3% |
| 1e8, depth 6, offset 4 | 53% |
| **1e9, depth 6, offset 4** | **99.2%** |

At 10⁹ essentially every sub-cell is occupied, so the sparse `(cell u64, count u64)` encoding
carries a 64-bit key per cell for a set that is *dense*: **16.6 MB, against 4.2 MB for a dense
`u32` raster and 2.1 MB for `u16`** — a 4–8× saving, and it grows with scale. The dense raster is
firmly justified at the operating point.

**But the 0.3% row is why it must be negotiated rather than switched.** A narrow principal at high
offset is genuinely sparse, and a dense raster would then ship megabytes of zeros to the viewer
that needs them least. Keep both encodings; let the request ask.

**Sizing `max_underlay_cells`:** one screen's worth is 1,048,576, and the guard is 8,192.
Both 1e8 and 1e9 sustain that cell count in a single request (179 ms and 466 ms respectively), so
**≥ 1,048,576 is supportable** — but see the per-principal caveat above: the same cell budget costs
`medium` 46 ms of CPU and `everything` 2.9 s of CPU at 10⁹. The guard bounds cells, not cost, and
sizing it is a deployment decision about the broadest principal served.
