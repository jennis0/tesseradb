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

**All six steps and the server track the design depended on are merged to `main`** (2026-08-27).
Read this section for the state; the tables below for what each step owes and what is left.

- **The client** is five packages: `@tesseradb/client` (the headless store), `@tesseradb/deck`,
  `@tesseradb/components` (thirteen elements), `@tesseradb/react`, `@tesseradb/viewer`, plus the
  Python `tesseradb` with the widget as its `[widget]` extra, run by hand in JupyterLab and Marimo.
  Three example pages are in the gate, and C3's documents — contracts r39, the OpenAPI description
  with the test that keeps it true, worked decodes in `pyarrow` and `apache-arrow`, the obligations
  page — are written.
- **The wire gained** the per-point membership column (D12), `layers` as *omitted means none* with
  the string `"all"` (D9), and a dependent artifact carrying its target's masked count (D13). The
  viewer plane gained `serve.cors_origins` (D10, [decision 0102](decisions/0102-the-viewer-plane-gains-an-enumerated-cors-origin-list.md)).
  A served hull is now a **concave** shape over the masked members rather than a convex wrap.
- **Two owner reviews are answered**, both recorded below: the first on the built explorer (the
  four bugs and the boards), the second on the 2.4M map (the glow, the polygons, and the hierarchy
  reading as clutter). The map now draws colour and names at rest, a hull only for the hovered and
  the opened artifact, labels on the frontier alone, and the density wash off by default.
- **Three demo corpora** are served by `run_demo.sh`: `notebook-2m4` (2.42M, seven layers — the one
  to judge on), `notebook` (50,000, the same shape), and `2m4` (k-means only, the original).
- **What is not built** is in the server-tracks table: the selection operand and its verbs (S6,
  designed and reviewed, not built), the fetch-model hint (S5, declined), predicate membership for
  k-means (S8, deferred), a per-branch cut (S9, closed as not a defect), and the tiered layer's
  `level` on the wire (S10). The design is at **r5**, with what the building changed in its
  Appendix R; the owner's rulings are decisions 0095–0102.

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
| S1 | D10 — viewer-plane `serve.cors_origins` | step 4's production docs, step 5 | **ruled 2026-08-26 — yes, option (a)**: an enumerated `serve.cors_origins` on the viewer plane, session plane untouched; browser-direct ships and the proxy arm is documented for the containers an origin list cannot name ([decision 0102](decisions/0102-the-viewer-plane-gains-an-enumerated-cors-origin-list.md)). **Built** on `track/s1` (2026-08-26): `serve.cors_origins` beside `serve.dev_cors_origins`, no startup warning, both lists may be set and a duplicate origin across them is not an error; the viewer router takes the union, the session router takes the development list alone and reads it from the state itself so no call site can widen it; the six replica headers (`etag`, `x-tessera-identity-key`, `x-tessera-stale`, `x-tessera-pin`, `x-tessera-server-us`, `x-tessera-admission-us`) are exposed and `Authorization` is allowed, with credentials mode off. A wildcard in **either** list is now refused at parse naming the key — previously unchecked, and one would have panicked the process at router construction. 11 tests in `tests/cors.rs`, the session-plane exclusion mutation-checked |
| S2 | D9 — the wire idioms at the contracts amendment | step 6 | **`layers` built on `server/s3-membership`** (2026-08-25): omitted or `[]` is none, `"all"` is every reachable layer, `all` refused as a name; the contracts wording is in S3's report for step 6 to fold |
| S3 | D12 — the per-point membership column, deepest served, with its leak-register pass | step 3's colouring | **built** on `server/s3-membership` (2026-08-25): `membership:<layer>` per served layer, nullable `u64`, after the scalars; both layouts; measured; the register row (C30) is **proposed** in [the memo](evidence/memos/2026-08-25-d12-membership-column.md), **owner ruling pending** |
| S4 | D13 — a dependent artifact's target | label counts | **ruled and built** on `server/s3-membership` (2026-08-25): not the target's id — a dependent carries its target's masked count; register note proposed in S3's memo; the drill-down route left for a ruling |
| S5 | D8 — the fetch-model hint in `/v1/meta` | nothing; the store observes | **not pursued** (owner, 2026-08-25). A hint cannot be the artifact cardinality — that is a corpus-wide count over objects the principal may not individually see (C8) — and a bucketed form still leaks an inequality about it. The store picks its fetch model by observation, which is what it does today |
| S6 | D11 — the selection operand; the export verb; the runtime-artifact path | *filter to this*, *export*, *save* | **approach ruled 2026-08-26**: the server intersects the shape with the Morton cell structure — interior cells are whole ranges and cost containers touched, boundary cells take a per-point in/out test, so cost follows the perimeter — and the result composes as one filter operand — so *filter to this* needs no second mechanism — and is resolved in **row space**, not entity space: `probes/2026-08-11-viewport-crossing/` measured a Morton-cell→entity structure as the slowest of three routes by 4–400× and larger than the `row_to_entity` array it emulates, because entity ids are assigned in permission-signature order and a cell's entities scatter (the controller's earlier wording said entity space and is corrected here). The boundary test makes the count **exact** for the drawn shape, which the client's cell cover cannot be; past a decomposition cap it falls back to the cover and marks the answer inexact. Touches the filter contract (§8.2), so it wants its own design and review. **Designed and reviewed 2026-08-26** — [`design/selection-operand.md`](design/selection-operand.md), Provisional, one adversarial review dispositioned in its Appendix R. The decomposition, the depth-16 floor the grid imposes, the cover fallback past a published perimeter budget, the cache key, and the argument that **no leak-register row follows**. It leaves three rulings, the first being that the design specifies the result in **row space** rather than entity space: a per-Morton-cell entity set is scattered in entity space and was measured and refuted in `probes/2026-08-11-viewport-crossing/`, which would invert the O(containers touched) interior this ruling asked for. No code |
| S7 | D14 — a verified-assertion auth plugin | C1's production token story | **not pursued** (owner, 2026-08-26). The C1 example keeps the paragraph describing what `builtin:passthrough` makes the host's server; it no longer points at an unplanned plugin |
| S8 | membership by predicate — a k-means layer whose membership is *nearest model centroid*, so every point is a member and colour covers the corpus (artifact-system §10's specified-not-built path) | k-means colouring beyond the sampled members | **deferred** by the owner 2026-08-25; a client-side Voronoi over served centroids was declined the same day — it would bake a classification into our client that the wire does not carry, so a C2/C3 client would draw a different map; decision 0099 stands |
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
layers on `/v1/meta`, every layer served under five measured principals (the memo's second table). Its HDBSCAN root chain is collapsed (the notebook's writer and `notebooks/collapse-hdbscan.py`:
257 nodes at depth 58 → 197 at depth 11, placeholder topics dropped); the honest tree has 26 nodes
at depth 1, so a single-depth cut under a budget of 27 serves the root alone at the full principal.
| S9 | a per-branch cut of a nested tree under `artifact_budget` — the engine cuts the whole tree at one depth, so a root of 26 children is served alone under any budget below 27 and the overview shows one artifact | the overview's cut; the client's base budget could fall to the labels a viewport fits | **closed 2026-08-26, not a defect.** The engine bisects for the deepest depth that fits and serves depth 0 when even the roots do not — the budget is a *resolution* knob and never a selection, because dropping nodes to reach a number is the sampling [decision 0083](decisions/0083-the-frontier-is-a-request-time-budget.md) forbids. A mixed-resolution frontier would also stop every shape on screen being the same kind of thing: a level-1 cluster of 900,000 beside a level-4 cluster of 3,000 reads as two peers. The 1 → 27 step at the overview is the tree's own 26-wide root, not the rule; the client asks for a budget that suits the viewport |
| S10 | a tiered layer's **level** on the wire — the artifacts frame carries no `level` column and the request no level selector, so a client has only `parent_id` to reconstruct a resolution from | the level a tiered layer draws at; the bytes a chosen level costs | **found 2026-08-26** ([the memo](evidence/memos/2026-08-26-tiered-levels.md)). Counting parent links puts 490 of `clusters/toponymy`'s 797 artifacts at the wrong level and draws 186 at level 0 where 16 are declared — the layer's edges skip levels (90 of 611) and 170 artifacts have no parent to be given, their members being unclustered at every coarser rung. `taxonomy/arxiv` hides it: two levels, complete single-step edges, so depth and level cannot disagree. A `level` column repairs the drawing; a level selector also bounds the response (797 artifacts, 181,286 bytes at every budget today). Owner's choice; server work not started |
| S11 | the served hull is a **convex** wrap of the visible members, so a density cluster's arms are bridged by straight edges across the viewport and every sibling overlaps | the contours the map draws | **built 2026-08-26** (track `hull`, branch `client/hull`): the hull is now a concave (alpha) shape over the same masked members — dug inwards from the wrap, α derived from the wrap's own median edge and never declared, a bounded 64-vertex budget beyond the wrap's own count. No wire change and no leak-register row (same inputs, same derivation, a strictly tighter shape). On notebook-2m4's `clusters/hdbscan`: 14% tighter in area on average, 1.4× the convex path's time over the whole 197-artifact layer, hull columns 26 KB → 99 KB. [The memo](evidence/memos/2026-08-26-concave-hulls.md) |
| S12 | a served hull is **one ring**, so a membership that is two separated clouds is drawn as one polygon over the gap between them | the contours the map draws; the pick path's one-polygon-per-artifact assumption | **built 2026-08-28** (track `rings`, branch `design/rings`; owner ruling 2026-08-27). The engine groups the visible members at α and digs a ring per group; `hull_x`/`hull_y` become `list<list<uint32>>`, which is a **breaking** wire change made so a single-ring reader fails its downcast rather than concatenating the rings. `artifact-shapes.md` is promoted to normative with all six rulings folded in — the family stays the dig, because a Rust Delaunay costs 1.4–1.5 s on the 2.4M-member artifact against 0.16 s for the whole dig, and there are **no holes**. On `clusters/hdbscan`: 215 rings over 197 artifacts, 4 carrying more than one, +1.7% hull bytes, +8% time, and the tightest shape 0.290 → 0.093 of its wrap. One file outside the track's allowlist is required and was not touched: `docs/openapi/tessera.yaml`'s `ArtifactResponse.hull`. **The client followed on the same day** (track `ringsclient`, branch `client/rings`), which is `artifact-shapes.md` §9 in full: the decoder descends two levels and refuses a body whose axes disagree on the ring count or on a ring's length; `outlineOf` returns rings and `outlineData` emits one datum per ring, every one carrying the artifact's `tessera_id` and drawing alike; `artifactIds` is a row-to-artifact map over rings, so two rings of one artifact hold two rows and both answer it; `extentOf` is untouched, reading the served `box`. Nothing assumes a member sits in exactly one ring, which is not a theorem (24 member positions on `clusters/kmeans` sit in two rings of their own artifact). **The two artifact goldens were not recaptured**: they are pre-r40 captures, the decoder refuses them, and three tests that need a post-r40 body are skipped and say so — see the note below |

**Notes the `ringsclient` track left** (2026-08-28). **The golden fixtures carrying a hull could
not be recaptured here and were left stale.** `core/test/fixtures/viewport-artifacts.bin` and
`viewport-membership.bin` carry `hull_x`/`hull_y` as a flat `list<uint32>`, the pre-r40 shape; the
demo servers on 37585/37589/37590 run a binary built before the rings change, so a live recapture
against them would have written the old shape back, and no built `tessera serve` carrying the
change exists on the machine. Capturing against those servers was refused rather than done — a
decoder test passing against a stale golden is worse than no test.

What that costs, stated rather than absorbed: **three tests are skipped** — two in
`core/test/artifacts.client.test.ts` (one row per served artifact and no points beside them; the
derived geometry in grid units) and one in `core/test/membership.test.ts` (the membership column
and the artifacts frame agreeing in a body the server actually sent). Each names the recapture at
its own claim. What restores them is a `tessera serve` built from this branch and
`node scripts/capture-golden.mjs --artifacts-only` against it; the geometry test's hull assertions
are already written for rings, so the recapture unskips something correct. The refusal itself is
covered against those same real bytes, and the ring decode against bodies
`core/test/hull.test.ts` builds — two levels, the degenerate one- and two-vertex rings, a null
hull, and the four disagreements the axes can carry. `clients/ts/README.md`'s fixture rule is
outside the track's allowlist and was not amended; it should say the artifact goldens are pre-r40.

**Step 4's notes** (2026-08-25). The plain-HTML example's app server is the claim-minting proxy
under `builtin:passthrough` and its README says so where a C1 developer reads it (design §5.3);
its production-topology paragraph is the same-origin proxy of `/v1/*`, with D10's `cors_origins`
marked ⊘ asked-for. The harness turns the first layer on through the picker when a page opens with
none on (the example leaves that to the user; the demo does not). Under headless swiftshader the
plain page's box selection took 156 s from mouse-up to the panel — the same main-thread draw the
step-3 measurements describe, not the store (262 ms headed at step 3); re-measure headed before
quoting. Vue and Svelte are documented in the examples' README and not checked.

**Reviewed by the owner, 2026-08-25** — eight points on the built explorer, and where each stands
after the `ui` track (branch `client/ui`, 2026-08-26). The design boards
(`docs/evidence/mockups/client-components/shots/`) are the acceptance; the track's report carries a
board-crop/screenshot pair per element and state.

| # | the owner's point | where it stands |
|---|---|---|
| 1 | the filter panel is ugly and unusable with 171 categories | **done**: chips with × and *Clear all*; a category is a search over the enumeration with the top four as checkboxes and *Show N more…* (171 never listed at once); text is a field with *all words / phrase*; ranges two inputs — `ExplorerOverlay.png` |
| 2 | initial load at 1.8M points is laggy and the loading pill says nothing | **done**: measured headed (Chromium 1208, RTX 3080) on 2m4's full principal, 996,488 marks — before: first marks at 1.86 s all at once in a 275 ms task, frame gaps 328/289/276 ms; after: first marks at 1.29–1.55 s streaming to 996,488 by 1.9–2.1 s, the longest task during streaming 63–85 ms (the 209 ms and 169 ms left are WebGL context creation and the warm shader link, before any data). The pill is gone; the strip's *Starting session…* and *Loading* rows are the progress. Under software GL (headless swiftshader) a frame is seconds, so slices are stored without presenting and the response paints once, as before |
| 3 | clicking an artifact shows nothing; click-to-filter is not wanted | **done**: a click selects — the card and the highlighted outline — and never moves the camera or filters; *Fit to cluster* is the card's button; the card is a live read of the served set (children as the channel answers) |
| 4 | no way to see the HDBSCAN and toponymy artifacts | **done with the corpus track's 2.4M corpus** (viewer 37590): the channel and the point path carry `artifact_budget` (`BASE × 2^zoom`, 48 at the overview, capped at 2,048 — `core/src/artifactBudget.ts`; 48 because the server cuts a whole tree at one depth, and the rebuilt tree's root has 26 children, so any budget below 27 served the root alone — S9 below), so HDBSCAN is served a 71-artifact cut at the overview (the root, its 26 splits, 26 and 18 beneath; 69 topics with text) and 84 three notches in; a tiered layer the server serves whole (toponymy, 797 at every budget) draws at the level the budget would have cut — `levelForBudget` — and the legend's *Level* select shows `auto · 16 topics…`, refining to 574 on zoom, or a chosen level. Topic labels attach to their cluster by the count a dependent carries (D13) and name it where it has no name of its own |
| 5 | it does not look like the designs | **done**: every element restyled to `gen.py`'s tokens and markup — `Main.png`, `ExplorerOverlay.png`, `StatusStates.png` (nine states), `SelectionFlow.png`, `ExplorerNarrow.png`; light and dark follow the host's `color-scheme`. **The map at 2.4M** (2026-08-26, judged on notebook-2m4's full preset against `gen.py`'s `datamap2` / `datamap_layers2`): marks translucent and sized by count and zoom (`deck/src/marks-style.ts` — the function and its numbers are below); a nested layer's contours are a hairline each with a 7–9% fill at the cut's leaves and none above; an artifact with no text draws no label, and a viewport places the top N by masked count that fit (`labelBudget`, 36 at 1440 × 900); a flat layer's hulls draw only for the hovered and the opened artifact — the hovered one found through the hull, the label, or a mark it holds, resolved through the ordinal and the table (`artifactOfMark`). An open or a close now repaints the map, which the highlighted outline of point 3 had needed and not had: the layer follows the store's projections on its own, but the opened artifact is a property the host computes at a paint |
| 6 | words instead of icons in the interaction menu | **done**: pan, box, lasso, a rule, fit as `gen.py`'s icons, top-left docked and top-right overlay |
| 7 | the mode toolbar is buggy — select tools do nothing, then pan breaks | **done**: deck's input layer saw a selection's pointerdown and never its pointerup, so its session stayed pressed; the gestures are now taken in the capture phase before the canvas sees them. `harness/modes.mjs` drives every transition with human-paced pointer input: 13 of 15 against the old handlers, 15 of 15 now |
| 8 | text like *how many artifacts a layer holds is never published…* in a panel | **done**: every explanatory sentence is out of the panels — a state is one line — and in `clients/ts/README.md` (*What the panels do not say*) |
| 9 | zooming in causes notable reloading on colour — points go grey in bands, then slowly regain colour | **done** (2026-08-26, on the running 2.4M demo): two things drew grey. Stand-ins carried no ordinal and were written `unmapped` under cluster colour — 56% of the marks on screen mid-zoom — though each stand-in mark is a real point of a real band carrying the ordinal the response that served it named; they now travel with their ordinals and read the same lookup texture. And the colour walk stopped at the artifact channel's latest served set, which is 200 ms behind the gesture, so a band arriving from the point path named artifacts the colour map had never seen and a band held under a coarser cut named artifacts the channel had moved off — a parent walk cannot go down. The session table now carries each entry's `centroid` from whichever response's frame named it, and the store colours **every live ordinal**. Measured below |
| 10 | the text is quite hard to read — bigger, wrapped to smaller widths, more of a border against the coloured points | **done**: the name's size band raised to the boards' own (`gen.py`'s `datamap_layers`, `12 + 10·√(count/max)` — 12–22 px, was 10–15); a name wraps on words to at most three lines of fourteen characters, the count beside the last line and the topic beneath, with the **wrapped** box driving the spatial hash and the 40 px displacement rule; the halo is the distance field's, which needed a padded atlas before it could draw at all. Measured below |

**Measured at the review** (2026-08-26, harness headed on 2m4's full principal; re-run before
quoting): decode as seen from the main thread median 21.9 ms, p95 88 ms; the largest response
748,096 points in 77 ms in the worker, 88 ms as seen; absorb split median 20 ms, longest slice
11 ms; per settle slab sync 2.4 ms, lookup texture 0.1 ms, layer build 2.8 ms, coverage check 2.0 ms
over 16,100 bands; per frame mean 16.7 ms, p95 17.6 ms; box selection 262 ms select-to-counted.
**Harness 9 of 9**; `smoke`, `smoke-artifacts`, `smoke-budget` OK on 2m4.

**Measured for the map at 2.4M** (2026-08-26, headed Chromium 1208 on notebook-2m4's full preset at
1440 × 900, the probe's `timings.markRadius` / `markAlpha` / `markCount`; re-run before quoting).
The mark style is `markStyle(marks, zoom)` over the resident count `marks` (every mark the slab
holds for the frame plus the stand-ins, margin included — a screen fact) and deck's zoom:
`t = clamp((log10(marks) − 2) / 4, 0, 1)` (0 at a hundred marks, 1 at a million), radius
`1.2 + 1.0 (1 − t) + 0.08 clamp(zoom, 0, 10)` px, alpha `min(0.95, 0.5 + 0.3 (1 − t) + 0.02 clamp(zoom, 0, 10))`;
a host's `radius` attribute pins the radius and the alpha still follows. deck raises its
`opacity` prop to `1 / 2.2` before the shader reads it, so the layer passes `alpha^2.2`. Before:
1.6 px at full alpha whatever the count — 1,012,813 marks at the overview drew as opaque blobs
(`m24-hdbscan-z0`). After, at the same view: **1.20 px at 0.50** over 1,012,813 resident marks;
three notches in **1.43 px at 0.56** over 1,578,845 (zoom 2.92); the boards' 1,600 marks would
draw at 1.9 px at 0.71 (the boards: 1.5 px at 0.68 light / 0.78 dark). A style change is two
uniforms; no per-point pass. The contours' fill is 18/255 light, 23/255 dark at a leaf, 0 above,
hairline 41/255 light, 56/255 dark at 0.8 px; hovered 26/33 fill, 150 line at 1 px; opened 41 fill,
200 line at 1.2 px. Labels: toponymy's overview drew 28 names at 1440 × 900 where it had drawn
dozens of overlapping `tpN-` keys; HDBSCAN's overview 28, k-means's 21 — none overlapping, the
placement unchanged and the candidates cut to what has text and to the budget. The flat layer
(k-means, 64 hulls in view) draws no hull at rest, one while hovered, one strong while opened.
Harness **9 of 9** headed, `modes.mjs` **15 of 15**, `smoke`, `smoke-artifacts`, `smoke-budget` OK.

**Measured for colour on a zoom in** (2026-08-26, headed Chromium 1208 on notebook-2m4's full
preset at 1440 × 900, five zoom notches from the overview at 260 wheel units each; the probe's
`view.provisional` and `cluster.coverage`; re-run before quoting). Bands that go **colour-stale**,
which is what retracts a tile and refetches it: 1,572 of 15,018 when colour by cluster is first
chosen and 2,267–2,382 of 15,006 at the first notch, then 0 — **0 at every one of the six points
after**.
Stand-in marks drawn neutral, mid-zoom before the settle: 748,282 then 2,702,504 (of 4,841,708 on
screen — 56%) then 1,676,221 then 1,156,345 — **none after, at any notch**. The costs are
unchanged: the lookup texture is rewritten on the table's version as well as the served set's, which
is O(table range) and measured at 0.10–0.20 ms a settle; the coverage check is 4.4–9.0 ms over
15,018–41,862 bands.

**The one deviation from §5.10, reported rather than taken quietly.** The design defines a band as
colour-current when every ordinal it carries "resolves to something served now", and the store now
resolves against the **colours** — every artifact the session table holds — instead. Resolving
against the channel's served set made every band in view stale the moment a zoom moved the cut
finer, refetching tiles that had just arrived and drawing them neutral until they came back. A band
whose ordinals resolve to an artifact the table holds is coloured, exactly, by an artifact the wire
said its points belong to, so it needs no refetch to be correct; what stays stale is what
staleness is for — a band with no column for a layer just switched on (the harness's layer-switch
refill still measures it), and one whose parent chain was never seen. The design is the
controller's file and is not edited here.

**Measured for the labels** (same run and viewport, both trees instrumented to count labels
placed rather than text rows — a wrapped name is several rows of one label, and `timings.labels`
counts labels now for that reason). Labels placed over the overview and five notches:
**13 / 23 / 27 / 26 / 11 / 4 before, 10 / 21 / 22 / 25 / 11 / 4 after** — slightly *fewer*, which
is what a larger size band costs; wrapping the box the spatial hash packs against recovers most of
it, and at the two deepest notches all of it. The name's size band is 12–22 px (was 10–15); a line
is at most fourteen characters over at most three lines; the halo is 0.16 em, which is 1.9 px on
the smallest name and 3.5 px on the largest. The halo needed the atlas, not the prop: deck divides
`outlineWidth` by `fontSettings.radius` and clips the glyph's distance field at
`fontSettings.buffer` atlas pixels — 4 at a 64 px atlas by default — so the previous
`outlineWidth: 2.5` could draw about **a third of a pixel** on a 12 px name whatever it said.
`fontSettings.buffer` is 12 now, and the halo's own alpha is the boards' 0.85 on both grounds.
Label placement stays O(artifacts): 0.30–0.40 ms a settle. Light and dark both shot; the demo
declares `color-scheme: dark`, so the light ground was seen by overriding it on the page.

**Found and not fixed here** (the server or the corpus): the server cuts a nested tree at one
depth, so the overview's `artifact_budget` cannot ask for the first split of a wide root without
also paying for the depth beneath it (S9); the wire gives a dependent artifact no centroid and no
target id, so a topic label can be placed only through its count (exact where
counts are distinct, unattached where two share one); the server serves a tiered layer whole
whatever `artifact_budget` says (toponymy: 797 at budget 42, 819 KB), so the client cuts by level
itself, and it cuts by the wrong thing — the wire names no level, so the client counts parent links
instead and draws 186 artifacts at level 0 where the layer declares 16
([the memo](evidence/memos/2026-08-26-tiered-levels.md): the 170 extra hold no member any coarser
artifact holds, so they have no parent to be given, and 90 of the edges that do exist skip a level);
the notebook 50,000 corpus counts zero — every tile arrives without
counts and the strip reads `0 shown · 0 matched · 0 visible` while 49,944 marks draw (present
before this track's changes; the 2.4M corpus counts).

**Gate after points 9 and 10** (2026-08-26): `check-clients.sh` green (core 228 with 6 skipped,
deck 80, components 69, react 10, spike 5, wire-example 7, plain-html 4, canvas-store 3);
`check-doc-links.py` clean; harness **9 of 9** headed on notebook-2m4 — its ninth claim now reads
**16 of 16** sampled ordinals drawn in a colour beside the 16 that resolve to a served artifact,
which are different questions and were the same number only by accident — `modes.mjs` **15 of 15**;
`smoke` and `smoke-artifacts` OK on 2m4, `smoke-budget` OK on notebook-2m4 while the machine was
quiet. The
smoke scripts now excuse the one console error a healthy run produces — Chromium's
`ERR_INCOMPLETE_CHUNKED_ENCODING` against a streamed response the driver abandoned when the view
moved, which the harness has exempted since step 3 and which failed `smoke.mjs` on `main` too.
**The smoke scripts launch headless swiftshader and ignore `--executable`, so notebook-2m4 is at
the edge of what they can drive.** `smoke-artifacts` runs a layer × principal grid, which is
twenty-five settles on that corpus's five layers and does not finish inside a ten-minute budget;
`smoke.mjs` did not finish either under load; and `smoke-budget` reads `0, 0, -849,156, …` marks
across its zoom steps when the box is busy, because a software-GL frame at 2.4M outlasts its
settle window — **identically on `main`**, checked by reverting the branch's client tree and
running it there. They are run on 2m4, as at the review, and `smoke-budget` passes on
notebook-2m4 when the machine is quiet. Giving them the headed browser the harness takes is the
obvious repair and is not made here. The harness's fourth claim — no count against a stale view — is **timing-dependent**: it
failed twice and then passed four times on the same tree, and its evidence now reports how many
revalidations had their key moved, so a run that fails says which half of the claim went.

Two loose ends were found while re-measuring and fixed here. deck warned `Multiple new layers with
same id tessera-marks-standin` on any frame the slab had no partition for — the warm sublayers and
the real stand-in layer were both pushed under that id, and deck kept one of the two. And the
harness's click on the strip's *Refresh*, which is not a claim but the way back to `shown` for the
claims after it, raced the strip's own re-render and timed out with *element was detached from the
DOM*, on `main` as well as here: while the moved content key is in play the strip alternates
between the stale row and the counts row as responses land. It retries now.

**Reviewed by the owner again, 2026-08-26** — on a screenshot of `clusters/hdbscan` zoomed in:
*the dot glow is way too strong*; *the group polygons remain really ugly*; and *something is not
quite right about the way we render the stacked layers — hierarchy gets lost in the label size, and
the multiple overlapping layers make it hard to read*. Answered on the `look` track (branch
`client/look`).

| # | the owner's point | where it stands |
|---|---|---|
| 11 | the group polygons are ugly, and the stacked layers make the map hard to read | **done**: a hull is drawn only for the **hovered and the opened** artifact, on every layer, nested included. A response carries a frontier *and its ancestors*, so drawing every served hull gave a region its own contour plus its parent's plus its grandparent's — translucent fills stacking into a wash with no cue that the big shape contained the small ones, and the long straight chords crossing the screenshot were single edges of the large ancestors. The rule already held for a flat layer and the argument one level up is the same: exact colour already says where a cluster is and how far it reaches, per principal (decision 0099). At rest the map is colour and names. Every other artifact stays in the data at zero alpha, which is what answers a pick — the flat path's own arrangement, reused rather than forked. The fill that faded with height below, the flat/nested split and the twelve-pixel floor went with the hairlines |
| 12 | hierarchy gets lost — an ancestor of something drawn is labelled beside it | **done**: **only the frontier is labelled**. A served artifact with a served child in the same response draws no label; `frontier` computes it from `parentId` over the served set, which `lineage` already holds, so nothing new is asked of the wire. With no chosen level this is the cut's leaves, as before; with one it is that level's artifacts **and** every shallower branch the level cut away, which the plain `depth === level` test dropped. Checked against the running corpus below |
| 13 | label size spends its whole range on the wrong distinction | **done**: **a name's size is its masked count**, on a logarithmic band over the range the drawn frontier holds — `12.5 + 11.5·log(c/lo)/log(hi/lo)` px, `lo` and `hi` the smallest and largest counts among the names that survive the label budget. The ends are the level ladder's ends, so nothing else about the map's weight moved. The band is logarithmic because these counts run three or four orders of magnitude — 380,069 against 176 on one screen — and a linear map, like the square-root one before it, spends the range on the top few. **Level is not encoded at all**, having been for a day: stepping the size by depth assumed depth tracks scale, which holds in a balanced tree and not in HDBSCAN's condensed tree, so *image object video* at 29,369 members drew a step larger than *algebras equations spaces* at 380,069 (`before-clusters-hdbscan-overview.png`). The hierarchy is carried by row 12's rule instead: a frontier partitions the drawn view, so its counts are directly comparable. A frontier of one name, and one whose counts are all equal, have no range to divide by and take the top of the band — the largest count on screen is the largest name on screen in every case. `LEVEL_SIZES`, `WITHIN_LEVEL_PX` and the rank argument are gone. The key is never drawn as a name anywhere: `artifactName` and `displayName` answer nothing where there is no supplied text and no attached topic, and the *IN VIEW* list, the cluster card's headline and its children draw a neutral em dash beside the count instead of `hdb-2422486` and `tp2-000002`. The key keeps the card's field that says *key* |
| 14 | the dot glow is way too strong, at every zoom | **done**: two things made it, and both come down — `markStyle`'s band, and deck's own half-pixel feather. Measured below |
| 15 | the density wash confounds the pass | **done**: `<tessera-map>`'s `wash` defaults **false** (owner direction: how density should be rendered is a separate conversation). Only the default moved — `<tessera-map wash>` turns it on and the layer still builds the image from the exact tiles' counts, filtered so the grid never shows (decision 0097) |

**The marks' style, recalibrated** (2026-08-26, headed Chromium 1208 on notebook-2m4's full
principal at 1440 × 900, the probe's `markRadius` / `markAlpha` / `markCount`; re-run before
quoting). The band was `1.2 + 1.0(1−t) + 0.08z` px at `0.5 + 0.3(1−t) + 0.02z`; it is now
**`1.1 + 0.6(1−t) + 0.05z` px at `0.34 + 0.44(1−t) + 0.015z`**, capped at 0.9, over the same
`t = clamp((log10(marks) − 2) / 4, 0, 1)`. The sparse end is pinned where the boards put it — at the
boards' own count (about 1,600 marks) 1.53 px at 0.65 against their 1.5 px at 0.68 light / 0.78
dark, and a hundred marks land on 0.78 exactly — and everything above it comes down. On
`clusters/hdbscan`: the overview **1.10 px at 0.34** over 1,012,813 resident marks (was 1.20 at
0.50); three notches in **1.25 px at 0.385** over 2,451,556 (was 1.44 at 0.56).

**The second half of the glow was deck's, not ours, and is the larger one.** deck's
`ScatterplotLayer` feathers a disc over a **half pixel** either side of its radius, so a 1.2 px mark
reached 1.7 px and most of that area was ramp rather than disc — the feather is worst exactly where
the marks are smallest, which is where the blooming was. `markStyle` now returns `antialiasing`,
off below **1.4 px** and on above it, so the feather is kept where it is an edge and dropped where
it is the mark. Together that is about **half the ink** a mark laid down: alpha times the footprint
the feather reaches, 0.34 × π·1.1² against 0.5 × π·1.45². Judged at the overview and three notches
in on both grounds; the demo declares `color-scheme: dark`, so the light ground was seen by
overriding it on the page, as at the previous review.

**The frontier, checked against the response** rather than assumed: `clusters/hdbscan` at the
overview on the full principal serves 71 artifacts of the layer at depths 0–3, and the frontier is
the **58 leaves at depths 1, 2 and 3** — the real splits. The root and the twelve internal nodes of
the near-root chain draw no label. A tiered layer at a chosen level (`clusters/toponymy`, 574 topics
three notches in) has one level in its frontier, which the same rule reaches by the same route.
Labels placed, overview then three notches in: `clusters/hdbscan` 10 → 7 and 23 → 27;
`clusters/toponymy` 7 → 5 and 13 → 16. Fewer at the overview is what larger names cost; the
placement is unchanged.

**The size encoding, corrected the day after** (2026-08-27, headed Chromium 1208 on notebook-2m4's
full principal at 1440 × 900; shots in `/home/joe/tessera-shots/2026-08-27-labelsize/`, before and
after on each view). Level-first sizing lasted one review. On `clusters/hdbscan` at the overview the
largest name on screen was *image object video* at **29,369** members, over *algebras equations
spaces* at **380,069**, because the first sits a level shallower — the encoding was telling the eye
the opposite of what the counts say. With the band over the count the largest name is *neural
network deep* at **507,264**, and the order down the screen is the order of the counts.
`clusters/toponymy` at the overview shows the other half: one level in the frontier meant one size,
so *Higgs Physics* at 42,979 drew exactly as large as *Condensed Matter* at 194,276; they are now
three sizes apart on a band whose ends the smallest and largest names hold. Labels placed, before →
after: `clusters/hdbscan` 7 → 7 at the overview and 27 → 25 three notches in; `clusters/toponymy`
5 → 6 and 16 → 19. Smaller names pack, which is why the tiered layer draws more of them. The
harness holds **9 of 9** and `modes.mjs` **15 of 15** on the same corpus.

**A hull that could not be smoothed any more.** `outlineOf` ran three rounds of Chaikin's corner
cutting over the served hull, on a comment claiming every vertex stayed inside the hull's convex
extent. That held only while the hull was convex; it is now a concave (alpha) shape over the masked
members (annotations §4.2). Chaikin replaces a corner with a chord between points on its two edges,
and at a **reflex** corner the triangle that chord spans lies *outside* the polygon — so the drawn
contour reached into every concavity by up to a quarter of the shorter adjacent edge. On the notched
square in the test, three rounds put the boundary through (0.47, 0.55) of a side where the served
shape has nothing, **while the ring's total area fell**, because the convex corners take off more
than the reflex one puts back: an area comparison alone would have missed it, so the test is a
containment one. It is a display matter and not a disclosure — the client invents no vertex from
data — but a drawn shape must not claim ground the served shape does not have. The smoothing is
**gone** rather than made reflex-aware: the shape no longer needs it, its corners being the members'
own positions rather than a convex wrap's artefacts, and it cost eight times the vertices on every
served outline (about 130 to 700 on one shape) with an outline materialised for every served
artifact, drawn or not, because the polygon is what answers a pick.

**Two things the smoke scripts stopped checking, and now check again.** `smoke-artifacts.mjs` keyed
its list rows by **what the row said**, so once a nameless artifact drew a placeholder instead of its
key every nameless cluster of a layer collapsed onto one entry and *the same cluster across
principals* compared a cluster with itself — one row where 2m4's k-means has twenty-four. It keys by
`data-id` now, the opaque `tessera_id`, which is the same for every principal (I10). And *a hull
rendered under two principals* read `timings.outlines`, which counts the shapes the layer **holds**
rather than the ones that draw, so it would have passed against a map that drew nothing;
`LayerTimings` gains `outlinesDrawn` and the script asserts what the rule says. On 2m4: **24 shapes
held, 0 drawn at rest, 1 drawn on opening a cluster**.

**Gate after this pass** (2026-08-26): `check-clients.sh` green (core 228 with 6 skipped, deck 90,
components 72, react 10, spike 5, wire-example 7, plain-html 4, canvas-store 3);
`check-doc-links.py` 0 errors over 735 files (15 warnings, all pre-existing citation drift under
`probes/`); harness **9 of 9** and `modes.mjs` **15 of 15** headed on notebook-2m4; `smoke`,
`smoke-artifacts` and `smoke-budget` OK on 2m4. The demo servers were left running throughout and
the branch's viewer was served on its own port, reached through a same-origin proxy — the running
deployments enumerate only `http://localhost:5173` in `serve.dev_cors_origins` and restarting them
to add an origin was not worth the interruption.

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
