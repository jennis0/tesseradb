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
and `@tesseradb/components`, box selection, and the viewer as the explorer plus instruments. Step 3
is built (branch `client/step-3`): the membership column consumed end to end, the lookup texture,
the four artifact elements, wire geometry drawn, lasso, the sidecar retired. Step 5 is built (branch
`client/step-5`): the `tesseradb` package with the widget as its `[widget]` extra, run by hand in
JupyterLab and Marimo. Step 4 and the server tracks are ahead; the design
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
| 3 | the membership column consumed: hashed to a response-local index in the decode worker, named on the main thread through the session table as each band is built (one reference per distinct ordinal per band, released on eviction; a layer's column carried over a refetch that did not name it), the point path naming the layers on with their closure (decision 0096); the `u32` ordinal as a per-point GPU attribute through the slab's dirty-span path and a lookup texture of one RGBA per live ordinal, 1,024 wide and grown in rows, the point shader reading `lut[ordinal]` behind a uniform switch — palette, level, highlight and the switch are texture rewrites, never a per-point pass (decision 0100); colour coverage per band over its distinct list, in the visible box, colour-stale bands refetched once per served set; the positional palette (decision 0099's default) with `spread` as the option; served hulls and boxes as hairline outlines, the opened one strong with a faint fill; names and counts at centroids, sized by masked count, placed by priority into a spatial hash with leader lines; the density wash supersampled and sampled linearly so the grid never shows (decision 0097); `<tessera-layer-picker>`, `<tessera-artifact-list>`, `<tessera-artifact-card>`, `<tessera-legend>` in the explorer's slots; `mode="lasso"` rasterised to the cells it meets at the box's bounded depth; `clusters.json` and `publish-clusters.mjs`'s sidecar half gone | `check-clients.sh` green (core 222, deck 54, components 44, spike 5, wire-example 7); `smoke-artifacts.mjs` OK (6 / 16 / 24 / 24 / 24 clusters narrow→full, a hull and a label under both principals shot); the harness **9 of 9 claims hold** headed, 9 of 9 headless (see the measurements) | D12 (built) | **done** 2026-08-25 (`client/step-3`) |
| 4 | the examples (plain HTML, React explorer, canvas store) and `@tesseradb/react`, in the gate | typecheck; the harness against the C1 page | D10 for the production paragraph | **done** 2026-08-25 (`client/step-4`): `@tesseradb/react` (hooks 4 tests, wrappers 3), the three example pages as workspaces (plain-html 10 tests incl. its app server), the harness's nine claims **hold against the plain-HTML page** (headless: region ≈ 587,471 inexact at depth 7 over 2,208 tiles; a switch of principal 996,488 → 243; sixteen sampled ordinals resolve); the D10 paragraph marked ⊘ |
| 5 | `tesseradb[widget]`: `Map`, operator-only `authorise`, the messages, the traitlets, the wheel's build hook | the notebook example; the build hook in the gate | D4, D10 | **done** 2026-08-25 (`client/step-5`): `clients/py` — `Map(url, token=…, view=None)`, `authorise` operator-only, the token never model state (`ready`/`token`/`reauthorise`/`refused`/`error` over the comm), the traitlets up-synced at the settle, ids as decimal `u64` strings; the bundle built by `hatch_build.py` at wheel-build time; `clients/py/check.sh` proves the wheel holds it (21 Python tests, 22 widget tests in the components' 66); **run by hand in JupyterLab 4.6.3 and Marimo 0.24.0** against the demo — see the notes below |
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

**The notebook corpus is the demo's fifth dataset** (2026-08-26, track `corpus`): `data/notebook/`'s
declarations migrated to the configuration surface (the notebook's writer already produced the
current form; the files on disk were reshaped to match it, and `hull` added to the clusterings'
computed content), built in 5 s, and served by `./run_demo.sh --scale notebook` on 37589/49307/45725
beside 2m4 — five layers on `/v1/meta` (k-means flat, HDBSCAN nested, the taxonomy tiered with its
two levels, a topic label layer depending on each clustering), a budgeted cut with `parent_id`s,
and labels carrying text and their target's count (263 of 263 equal). The numbers, the per-principal
served counts and one caveat — the labels' `all` content requirement means only the full principal
sees a label — are in [the memo](evidence/memos/2026-08-25-notebook-corpus-as-demo.md). The
dataset document is now merged across invocations and written without the viewer. The whole
corpus through the same pipeline, `data/notebook-2m4-live/` with its four-level toponymy layer and
its labels, is the `notebook-2m4` scale on 37590/49308/45726: built in 5m13s at 4.7 GiB peak, seven
layers on `/v1/meta`, every layer served under five measured principals (the memo's second table).

**Step 4's notes** (2026-08-25). The plain-HTML example's app server is the claim-minting proxy
under `builtin:passthrough` and its README says so where a C1 developer reads it (design §5.3);
its production-topology paragraph is the same-origin proxy of `/v1/*`, with D10's `cors_origins`
marked ⊘ asked-for. The harness turns the first layer on through the picker when a page opens with
none on (the example leaves that to the user; the demo does not). Under headless swiftshader the
plain page's box selection took 156 s from mouse-up to the panel — the same main-thread draw the
step-3 measurements describe, not the store (262 ms headed at step 3); re-measure headed before
quoting. Vue and Svelte are documented in the examples' README and not checked.

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

**Measured at step 3** (2026-08-25, `clients/ts/harness/harness.mjs` against the same demo, full
principal, 996,488 marks on screen at depth 8, colouring by cluster through the lookup texture;
**headed** Chromium 1208 on WSLg's display with `--headed --executable`, and headless swiftshader
beside it; re-run before quoting):

- **Per response, decode** (headed, 50 responses): as seen from the main thread median 42.7 ms,
  p95 185 ms, max 193 ms; in the worker median 11.2 ms, max 103 ms; queued behind the lane median
  40.7 ms, max 189 ms. The largest response, 747,268 points and 28.0 MB, took 67 ms in the worker
  and 77 ms as seen from the main thread. **Remap** on the main thread, over 41 responses: median
  4.1 ms, max 33.4 ms over 747,268 points; the absorb split median 33.2 ms, max 121 ms, the longest
  single slice 18.7 ms against the 6 ms budget (a slice checks the clock every 64 tiles).
- **Per settle** (headed, last settle): slab sync 3.0 ms, wash bin and filter 0.0 ms (memoised on
  the `tiles` object), lookup texture 0.1 ms, outlines 0.2 ms (21), labels 0.2 ms (21 placed), whole
  layer build 3.5 ms; the coverage check 2.8 ms over 16,100 bands, 0 stale. Twenty-four texture
  writes in the whole session, against 153 paints.
- **Per frame** (headed): mean 16.7 ms, p95 16.9 ms over 120 frames — the rAF cadence, with a
  million marks coloured through the texture; the GPU is not the limit on this box. Headless
  swiftshader is a different machine: mean 628 ms, p95 8.8 s, with main-thread tasks of 10–14 s
  while it rasterises 1.5 million marks, which is what the two step-2 latencies were.
- **The two latencies step 2 could not explain**, resolved. (1) *A 1M-point response decoding in
  8.3 s*: the worker decoded it in 49–77 ms every time; the seconds were the reply waiting for the
  main thread, which under headless swiftshader was inside a 10–14 s draw. Headed, the largest
  response waits 10 ms. Neither the decoder's queue nor the absorb budget is the cause, so neither
  changed: the worker now reports its own time so the lane's queue is a number rather than a
  suspicion. (2) *A box counted 8.8 s after settle*: headed and headless alike the region's three
  lanes are settle 201 ms, wire 52–56 ms (server 7–10 ms over 2,070 tiles at depth 7), projection
  0.2 ms — **262–268 ms select-to-counted on the store's clock**; the 8 s was the same main
  thread, and the earlier 100 s from mouse-up to the panel was the harness's box landing on the
  toolbar's panels (it now starts clear of them: 779–887 ms mouse-up to panel).
- **Under headless swiftshader the server sheds the stream** (integration, 2026-08-25): a
  10–14 s main-thread draw stops the client reading, `serve.stream_write_stall_ms` (10 s) fires,
  and the point path sees `fetch-failed` — five sheds in one harness run, one of which landed on
  the stale claim and failed it (8 of 9 headless; 9 of 9 headed, and 9 of 9 headless when no
  shed coincides). A shed mid-body is a retry case the driver does not yet treat as one — it
  retries 429 and backs off on 503, and a broken stream refuses. Recorded; not changed.
- **Layer-switch refill**: with one layer published the switch off and back on is the free case
  §5.10 describes — the columns survive on their bands, no band goes colour-stale (measured, both
  modes) — so the refill under a colour-stale refetch is **not measured**; it needs a second layer
  in the demo. What was measured is the coverage check itself (above), and one wrong turn: before
  coverage was scoped to the visible box it counted the render margin's bands, whose ordinals name
  artifacts the channel never served for this view, and refetched 1,320 of 16,100 bands for nothing.
- **Colour by cluster**: 16 ordinals sampled from the marks on screen, every one resolving through
  the table to one of the 21 served artifacts, none to an artifact not served (the harness's ninth
  claim).

**Step 5 ran by hand** (2026-08-25, against the demo's 2m4 bundle at the *medium* preset, 360,239
visible; headed Chromium 1208 driven by Playwright with the kernel read through `jupyter_client`,
so "by hand" means a scripted hand — the sequence a person would perform, with the readings taken
from the kernel rather than from a screenshot):

- **JupyterLab 4.6.3, anywidget 0.11.0**, `jupyter lab --port 5173` so the page's origin is the
  one the demo's `dev_cors_origins` names. `Map(...)` mounted the explorer and drew 327,125 of
  360,239 at depth 8 coloured by cluster; one `ready`, one token. A click set `selected` to
  `'14895740295145689746'` (a decimal string of a `u64`); a shift-drag box set `region` with its
  counts (25,372 visible, inexact, over 3,200 tiles at depth 8). `m.bbox = …` from the kernel
  moved the camera and the settle reported the box actually shown (the aspect differs).
  `m.filters = {"title": {"match": "quantum"}}` applied without the panel's debounce: 2,729
  matched of 26,614 visible, the region's matched dropping to 472. `{"any_of": …}` was refused
  to `last_error` with its reason and applied nothing. `m.colour_by = None; m.layers = []`
  redrew neutral with no layer. A second `m` in another cell drew the same view from its own
  store.
- **Marimo 0.24.0**, `marimo edit --port 5173`: `mo.ui.anywidget(Map(...))` mounted and drew the
  same 327,125 marks — which is the verification §7 asked for, since the map cannot draw until
  the kernel has answered `ready` with the token as a custom message. The reading cell re-ran
  at the settle with every synced trait in `.value`; a click on a hull set `selected_artifact`
  to `'12619485137644099626'`; a box set `region`.
- **What the run found and changed.** (1) deck.gl reads `process.env.NODE_ENV` unguarded, and
  the bundle was a `ReferenceError` under a Blob URL — vite's `define` now substitutes it. (2)
  ipywidgets' `DOMWidget` already owns a `layout` trait (its CSS layout model), so the
  explorer's is `explorer_layout`. (3) The design's store-per-model was wrong in practice: a
  store has one view input, and two explorers over one store fought for the camera, one of them
  drawing the other's frame — the store is per view, the token supplier per model, and the
  *active* view (the last mounted or moved) is the one whose settles sync up. (4) Under this
  JupyterLab and anywidget the module is evaluated **per view**, not per model: a second view
  of one widget sent a second `ready` (the kernel answered it; the token was the same), and
  luma.gl logged *This version of luma.gl has already been initialized* once per further
  evaluation — a console line, not a failure; the guarded defines are what keep a second
  evaluation harmless. Two views cost two stores and two replicas; a page of many views of one
  widget pays that.
- **Not measured**: the settle-to-kernel latency of an up-sync, and the kernel-to-camera latency
  of a down-sync; both were sub-second by eye in the runs above and neither was timed.
- **Not built** (design §7, D4 open): the proxy arm. The widget's browser-direct arm works
  under `dev_cors_origins` today and under D10's `cors_origins` if granted; the proxy arm's
  server-extension sketch is in `clients/py/README.md`.
- **The build hook in the gate**: `bash clients/py/check.sh` — a venv, an editable install
  (which runs the hook), ruff, pytest, a wheel built and opened. It runs in about five seconds
  with `node_modules` current; `npm ci` runs only when the lockfile is newer than the install.

**Notes step 3 left.** A colour-stale band keeps drawing what resolves: its ordinals that still
resolve to a served artifact keep that colour (exact, since membership in a served artifact is
what the wire said), the rest draw neutral, and the band is refetched — the design's "its points
draw neutral" would cost a per-point pass to zero the ordinals, which decision 0100 refuses.
Stand-ins carry no ordinal and draw neutral under cluster colour. The replica's counts-only
revalidation sends `layers: []`: it absorbs no points and would pay the artifact pass for nothing,
and the harness tells the channel's request from the point path's by `k = 0` with a named layer.
Headed runs need a full Chromium: Playwright 1.62's own could not be downloaded here (the CDN
timed out), and `--executable` points the harness at the 1208 build on the machine. Chromium logs
`ERR_INCOMPLETE_CHUNKED_ENCODING` against a streamed response the client abandoned mid-flight (a
superseded request's abort); the harness exempts it beside its own 403s.

**Notes step 2 left.** Vite 8's oxc lowers only legacy decorators, so the standard form the design
decides is lowered by `components/vite-plugin-decorators.ts` (esbuild) for the dev server and the
bundle; found by the first smoke run, where the browser received `accessor` raw. A `k = 0` response
decodes inline rather than in a worker lane, because the lanes are serial and a counts-only answer
was measured queueing behind a point sweep. Three corpus citations rotted with the files this step
moved — `docs/artifact-client-handover.md`, decision 0058 and `docs/design/client-obligations.md`
cite `viewer/src/artifacts.ts` and `viewer/src/viewportLayer.ts` — and are outside the track's
allowlist; `check-doc-links.py` reports them until the controller repoints them at
`core/src/artifactChannel.ts` and `deck/src/layer.ts`.
