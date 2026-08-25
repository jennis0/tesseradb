# Client components — delivery and status

**Status:** Living. **This file is the status record for the client-components work** — the steps,
what each must be true to be finished, and where it has got to. Tracked here rather than in
GitHub issues, on the convention the artifact work set ([`artifact-delivery.md`](artifact-delivery.md)).

**The rule that keeps it true: it moves in the change that moves the work.** A step that landed
without this file changing is a step whose status is now wrong, and a file that disagrees with the
code is the file that is wrong.

**How the work is done:** one worktree per step, branched from the previous step's merge, the gate
green before merge, this file updated in the same change. [`client-handover.md`](client-handover.md)
is the map — the work list expanded, the traps, the parallel tracks; the design is
[`design/client-components.md`](design/client-components.md) (r4), whose §9 is normative for the
order and §11 for the decisions.

## Where it stands

Steps 0 and 1 are built (branch `client/step-0-1`): the presented frame moved into the client and
the headless store is `@tesseradb/client`'s main export, the scope rename to `@tesseradb/*` landed
with it, and the viewer consumes the store. Step 2 is built (branch `client/step-2`): `@tesseradb/deck`
and `@tesseradb/components`, box selection, and the viewer as the explorer plus instruments. Steps
3–5 and the server tracks are ahead; the design
is at r4, reviewed across three lenses, with the owner's rulings of 2026-08-24/25 in its §11 and
recorded as decisions 0095–0101; the open decisions and what each gates are in the handover's §1.

**The smoke scripts ran for steps 0–1 on a rebuilt demo bundle** (2026-08-25, integration). Every
prebuilt bundle predated manifest fields the current binary requires (`vocabularies`, `visibility`),
and the demo build declaration `data/demo/config-2m4.toml` had gone; it was regenerated from
`probes/build_demo_datasets.py`'s templates and `run_demo.sh --scale 2m4` rebuilt the bundle (5m18s,
4.4 GiB peak, 1.4 GB on disk). Two things the offline gate had not caught then surfaced and are fixed
in the same change: `publish-clusters.mjs` registered its layer with the pre-`visibility` field names
(a 422 from the control plane), and **a `setLayers` issued before `/v1/meta` was lost** — the artifact
channel is built at meta and the call reached nothing, so the demo's layer choice vanished on every
principal switch while `smoke-artifacts.mjs` still printed OK with no count read. The store now holds
the intent, hands it to the channel at meta, gives the channel its injected clock, and asks at the
first drawn frame; the smoke fails when no principal is served a count.

## The steps

| step | what lands | proved by | needs | status |
|---|---|---|---|---|
| 0 | the presented frame into the client: `Presenter` holds the `Composition` and executes the driver's fold/derive verdict under an injected frame scheduler; `binding.ts` and the viewer's duplicate `ViewState` gone | `check-clients.sh` green; `smoke.mjs` green (996,488 of 1,856,276 shown, depth 8, no console errors) | — | **done** 2026-08-25 (`client/step-0-1`) |
| 1 | the store: `createStore` with the projections, `Count`/`Masked` and their formatters, `stale` on the content key, `setView`'s conversion, `dataXY`/`extentOf`, the token supplier, the encoding accumulators, filter composition, the artifact channel, item/artifact/category fetches and the session artifact table out of the viewer; the `@tesseradb` rename; the viewer consuming the store | store/channel/table/counts/encoding/driver-503 tests; `check-clients.sh` green; `smoke.mjs` and `smoke-artifacts.mjs` green (24 clusters served under the full mask, 16 under sparse, 6 under narrow) | D3 | **done** 2026-08-25 (`client/step-0-1`) |
| 2 | `@tesseradb/deck` and `@tesseradb/components`: `TesseraLayer` over `marks`/`tiles`/`artifacts` with the slab, rank-to-colour and the stand-in buffers moved out of the viewer, the density wash from the exact tiles' counts, artifacts at their wire centroid (the sidecar's rings gone); the nine elements of §9 step 2, a subpath entry each, the eight states through `part="state"`, the §5.9 mechanics (standard decorators with `accessor`, guarded defines, one `ContextRoot`, store precedence at connection, the `Deck` finalised a settle after disconnect, the single-file bundle with the inlined worker and its SRI hash); box selection as one `k = 0` `tiles`-form request under a 4,096-tile bound with `exact` set by the cell-versus-pixel rule; the viewer as `<tessera-explorer layout="overlay">` plus instruments | `check-clients.sh` green (core 198, deck 43, components 37, spike 5, wire-example 7); `smoke.mjs` OK through the parts (996,488 of 1,856,276 shown, strip `shown`, I7 holds across five encodings); `smoke-artifacts.mjs` OK (24 / 24 / 24 / 16 / 6 clusters full→narrow); `smoke-budget.mjs` OK (803,286 marks, spread 1.00×); the harness: **7 of 7 claims hold** | D1, D2, D2a, D5, D7 | **done** 2026-08-25 (`client/step-2`); see the measurements below and the notes the step left |
| 3 | layer picker with closure, artifact list and card, legend, wire geometry drawn, sidecar retired, lasso; **with D12:** the membership attribute, the lookup texture, colour coverage | `smoke-artifacts.mjs` under two principals; the harness | D12 for colour | not started |
| 4 | the examples (plain HTML, React explorer, canvas store) and `@tesseradb/react`, in the gate | typecheck; the harness against the C1 page | D10 for the production paragraph | not started |
| 5 | `tesseradb[widget]`: `Map`, operator-only `authorise`, the messages, the traitlets, the wheel's build hook | the notebook example; the build hook in the gate | D4, D10 | not started |
| 6 | C3's documents: contracts §3.2 amended, the OpenAPI description, worked decodes with a test, the obligations list | the decode test; `check-doc-links.py` | D9 | **landed** (branch `client/step-6-docs`): contracts r38 states the request in full; `docs/openapi/tessera.yaml` kept true by `tests/openapi.rs` (12 tests; the omitted-`layers` semantic test is `#[ignore]` until S3 lands); worked decodes in `reference/examples/` and `clients/ts/wire-example/` over one answer sheet; `design/client-obligations.md` (Provisional) |

**Server tracks the design asks for** (each its own design and worktree; status kept here so the
client steps can see what they wait on):

| track | ask | gates | status |
|---|---|---|---|
| S1 | D10 — viewer-plane `serve.cors_origins` | step 4's production docs, step 5 | not started |
| S2 | D9 — the wire idioms at the contracts amendment | step 6 | **`layers` built on `server/s3-membership`** (2026-08-25): omitted or `[]` is none, `"all"` is every reachable layer, `all` refused as a name; the contracts wording is in S3's report for step 6 to fold |
| S3 | D12 — the per-point membership column, deepest served, with its leak-register pass | step 3's colouring | **built** on `server/s3-membership` (2026-08-25): `membership:<layer>` per served layer, nullable `u64`, after the scalars; both layouts; measured; the register row (C30) is **proposed** in [the memo](evidence/memos/2026-08-25-d12-membership-column.md), **owner ruling pending** |
| S4 | D13 — a dependent artifact's target | label counts | **ruled and built** on `server/s3-membership` (2026-08-25): not the target's id — a dependent carries its target's masked count; register note proposed in S3's memo; the drill-down route left for a ruling |
| S5 | D8 — the fetch-model hint in `/v1/meta` | nothing; the store observes | not started |
| S6 | D11 — the selection operand; the export verb; the runtime-artifact path | *filter to this*, *export*, *save* | not started |
| S7 | D14 — a verified-assertion auth plugin | C1's production token story | not started |

## What each step owes a measurement

The design's §5.10 figures are modelled. Step 2's harness measures, and this file records:
per-response decode and remap cost at the render target; per-settle resolve, coverage check,
texture writes and label placement; frame time at several million marks with the lookup texture;
the layer-switch refill time under the colour-stale refetch. A figure quoted anywhere else is
re-measured before it is trusted.

**Measured at step 2** (2026-08-25, `clients/ts/harness/harness.mjs` against the demo: the 2m4
bundle — the only one the demo was serving — full principal, 996,488 marks on screen at depth 8,
headless chromium under swiftshader on WSL2; re-run before quoting):

- **Per response, decode** (worker, bytes in to typed arrays out, over 25 responses): median
  2.7 ms, p95 95 ms, max 7.7 s — the maximum is the full principal's million-point response. **Remap
  is not measurable on this branch**: the membership column (D12) is built on `server/s3-membership`
  and has not merged, so there is no column to hash or remap.
- **Per settle**: slab sync 0.1 ms, wash bin < 0.1 ms, whole layer build 0.1 ms at the last settle
  (a settle whose bands were already resident; a first-fetch settle at the full principal pays the
  slab writes, which the trace records and this run did not isolate). The resolve walk, the
  coverage check, the lookup-texture write and label placement are step 3's and are not measured.
- **Per frame**: mean 16.6 ms, p95 16.8 ms over 120 frames — the rAF cadence under software GL,
  which says the main thread is not the limit at 10⁶ marks on this box; it is not a GPU figure.
- **Box selection**: 1,419 tiles at depth 7, `k = 0`, the server answering in 5–19 ms
  (`x-tessera-server-us` 4–17 ms; measured by hand with the same request); **8.0 s select-to-counted
  on the store's clock**, which is not the server and not the decode (a `k = 0` response now decodes
  on the main thread; the figure did not move). The remaining suspect is the main thread absorbing
  the full principal's response at the same moment — unresolved, and recorded as measured rather
  than explained. The 100 s from mouse-up to the panel is headless input waiting on deck's
  software-GL hover picks and says nothing about the client.
- **Layer-switch refill under the colour-stale refetch**: step 3's; not measured.

**Notes the step left.** Vite 8's oxc lowers only legacy decorators, so the standard form the design
decides is lowered by `components/vite-plugin-decorators.ts` (esbuild) for the dev server and the
bundle; found by the first smoke run, where the browser received `accessor` raw. A `k = 0` response
decodes inline rather than in a worker lane, because the lanes are serial and a counts-only answer
was measured queueing behind a point sweep. Three corpus citations rotted with the files this step
moved — `docs/artifact-client-handover.md`, decision 0058 and `docs/design/client-obligations.md`
cite `viewer/src/artifacts.ts` and `viewer/src/viewportLayer.ts` — and are outside the track's
allowlist; `check-doc-links.py` reports them until the controller repoints them at
`core/src/artifactChannel.ts` and `deck/src/layer.ts`.
