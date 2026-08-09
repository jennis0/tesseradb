# Where the client's time goes — a browser-free measurement of the read pipeline

**Date:** 2026-08-09 · **Corpus:** the 2.4M demo bundle, broad principal (201 terms)
**Run:** `cd clients/ts && npx tsx ../../probes/2026-08-09-client-pipeline/pipeline.mts [--ring]`
Needs a running `tessera serve` (`./run_demo.sh --no-viewer`).

## Why this exists

The browser probes (`clients/ts/viewer/smoke-*.mjs`) attribute nothing: they time a gesture end to
end and leave which layer is responsible to be inferred. Worse, they run headless Chromium under
`--use-gl=swiftshader`, so **every mark is rasterised in software** — and the cost that dominates
them turns out to be exactly that. Conclusions drawn from them about *data* cost were wrong.

This drives the same layers with no browser: plan → fetch → decode → absorb → assemble. It runs in
~15 s against ~2 minutes, and it reports per phase.

## What it measures

Five pans of a third of a viewport each, at the zoom the demo lands on, 500k mark budget.

| | requests | wire + decode | assemble | wire bytes |
|---|---|---|---|---|
| foreground only | 6 | 134 ms | ~30 ms | 6.9 MB |
| with the anticipatory ring | 21 | 464 ms | ~34 ms | **65.0 MB** |

**The data pipeline is not the problem.** Across five pans, everything from issuing a request to
having drawable buffers costs under half a second in total, and assembly — the part that runs on
every redraw rather than every fetch — is single-digit milliseconds per step.

**The ring's cost is bandwidth, not time.** It is ~10× the foreground's bytes: 13 MB per pan on a
corpus of 2.4M items. On a local socket that decodes in tens of milliseconds and looks free. Over a
real network it is the dominant cost of the whole design, and nothing here measures that.

## The per-frame cost, which is what "smooth" actually means

`redraw` above is the whole render-side path with nothing fetched — plan, select the bands in the
region, assemble the buffers — which is what runs on every animation frame of a drag.

| held bands | redraw, before | redraw, after |
|---|---|---|
| 24 × 10³ | 14–20 ms | 0.2–6.0 ms |

Before, it scaled with **bands held** rather than with marks drawn: a view drawing 5,700 marks cost
14.4 ms because the region query recovered each band's tile index from its Morton prefix with a
per-bit `BigInt` loop, over every held band, every frame. A small mark budget therefore bought
nothing, and a broad principal — which fills the cache fastest — was the worst case.

Bands now remember their own tile index, and the cost scales with what is drawn: 211,900 marks cost
6.0 ms and 5,700 cost 0.4 ms.

**Then the buffers stopped being rebuilt.** A redraw used to concatenate every drawn mark into fresh
typed arrays and hand deck.gl new references, which is a full re-upload whether or not anything had
changed. With a persistent slab — one buffer, a stable slot per band, written once — the steady frame
is a residency check:

| | select | assemble | slab | stand-in colour | redraw |
|---|---|---|---|---|---|
| 35,296 marks / 24,462 bands | 0.6 ms | 0.1 ms | 0.1 ms | 0.0 ms | **0.8 ms** |

Assembly is 0.1 ms because exact bands are no longer copied at all — they are handed to the slab by
reference, and only the stand-in bands, whose clip changes whenever a response lands, are still
concatenated per frame. Across the seven pans redraw ran 0.8–4.2 ms, the high end being `select`
under 6,359 stand-in bands rather than anything the slab does.

**Then the frame stopped being re-derived.** At a 10⁶-mark budget the same redraw measured 18–26 ms,
and none of it was the slab: deriving the frame is per-*band* work, and a zoom-in leaves 3.9 × 10⁴
stand-in bands carrying 2.0 × 10⁵ marks between them — five marks each.

| | select | assemble | slab | stand-in colour |
|---|---|---|---|---|
| 39,121 stand-in bands | 5.9 ms | 18.6 ms | 1.2 ms | 0.5 ms |

The cost is per band and not per mark, which two negative results establish. Short-circuiting the
per-point restriction where a band's tile sits wholly inside the clip changed nothing measurable —
**every** stand-in band was already whole (12,639 of 12,639), so no per-point filtering was running.
And a micro-benchmark of the concat at that exact shape — 1.96 × 10⁵ marks in 3.9 × 10⁴ pieces —
costs 2.8 ms, against the 18.6 ms measured. What remains is ~0.8 µs of overhead per band, spread
across the region query, the restriction and the column fold.

A derived frame is now reused while the depth, the store's change counter and a containing rectangle
all hold, so it is derived when the store changes rather than on every animation frame. Measured in
the browser on the broad principal: a 350 px drag produces **6–8** frame derivations, one per
arriving response, against one per rendered frame before.

**The upload is still not measured, and is the reason to be careful about this table.** This harness
has no GPU. What it establishes is that the *CPU* side of a steady frame is now under a millisecond
and that no buffer is rebuilt; whether the remaining upload on an arrival frame is what a user feels
is a question only real hardware answers.

## Anticipation, bounded in bytes

With three bites per still period the ring moved **7.6–12.7 MB per pan at a 5 × 10⁴ mark budget** —
an order more than the view itself needed, on a corpus of 2.4M items. Decode is in a worker, but
splitting a response into bands is not, and that scales with points rather than with responses.

Bounding the budget in bytes instead of in requests: **24 requests / 86.5 MB → 12 / 31.5 MB** over
the same six pans, reaching the same 3.3 × 10⁵ points held. A request-count budget spends wildly
different effort on a dense corpus and a sparse one; a byte budget spends the same, and bytes are
what the interaction feels.

## What this corrects

Earlier the same interaction measured 8–29 s of "fetch + decode" in headless Chromium, which led to
a chain of wrong diagnoses — a coarse/fine fetching problem, then a decode-throughput problem, then
a worker. The pipeline numbers above are 20–60× smaller. The difference is software rasterisation
of ~700k marks, which scales with **marks drawn** and not with bytes fetched: at the default 50k
budget the same gestures measured 12–19 ms.

Two things that are still true and were found along the way, both fixed: the position decoder took
three `BigInt` allocations per point (7.9× slower than reading the same bytes as `u32`s), and a
single response of 2.3 × 10⁶ points blocked whichever thread decoded it.

## What is NOT measured here

- **Render cost.** deck.gl layer construction, buffer upload and rasterisation are absent, and on
  the evidence above they are where the seconds live at high mark counts. Measuring them needs real
  hardware; software rasterisation overstates them enormously.
- **Network.** Everything is a local socket, so the 65 MB above costs almost nothing. This is the
  ring's real price and it is unmeasured.
- **Concurrency.** One client.
