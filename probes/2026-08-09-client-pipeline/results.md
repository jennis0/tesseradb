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
