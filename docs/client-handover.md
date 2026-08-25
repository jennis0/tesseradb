# Handover — the client components, from design to execution

**Date:** 2026-08-25 · **Status:** design at r4, reviewed; nothing built. Branch
`client/components-design`, worktree `.claude/worktrees/client-components`.

**Read [`client-delivery.md`](client-delivery.md) first** — it is the status record for this work,
on the convention the artifact work set ([`artifact-delivery.md`](artifact-delivery.md)): it moves in
the change that moves the work, and it wins over this document wherever they differ. This document
is the map: what the design decided, what it left for the owner, the order the work goes in, what
runs beside what, and the things in the existing client that will bite.

## 0. Where authority lives

- [`design/client-components.md`](design/client-components.md) — **the design**, Provisional r4.
  Organised by four customers (§1); the stack (§2); the wire as C3's product (§3); the store as
  C2's surface (§4); the components (§5), including how the map draws and what the client holds
  per point (§5.10) and selection (§5.11); artifacts (§6); the demo and the notebook (§7); the
  packages (§8); **the order the work lands in (§9)**; rejected options (§10); **the decisions
  (§11)**. Its Appendix R is the review trail; the three review lenses that produced r4 found
  forty-one things, all dispositioned into the text — do not re-derive them.
- [`design/client-architecture.md`](design/client-architecture.md) — the client/vis boundary,
  the driver, the replica, composition. Built through its §6 step 3 (half). The design amends
  its §1 (the second package splits) and its review finding F8 (the encoding accumulators move
  to the store); everything else in it stands.
- [`design/client-interaction.md`](design/client-interaction.md) — the obligations the design
  restates: §4 (P1–P6), §6.1–6.2 (staleness), §7 (topologies and the two anti-patterns), §9,
  §10, §12, §13, §15.
- [`design/artifact-system.md`](design/artifact-system.md) §6 and §10; contracts §3.1–§3.2 (the
  `tiles` request form, the refusal classes, what is and is not documented about `layers`).
- The design canvas — "Tessera Client Components", published 2026-08-24 — and its generator at
  [`evidence/mockups/client-components/`](evidence/mockups/client-components/README.md). The
  boards are the look; **the colouring on them assumed per-point membership the wire does not
  carry**, which §5.10 resolves. Read the boards for layout, states and density, not as a spec.
- [`../clients/ts/README.md`](../clients/ts/README.md) — the instrument that exists, its dev
  setup, its smoke scripts, and the measured figures.

## 1. Where it stands

**Ruled by the owner** (in conversation, 2026-08-24/25; to be written to `docs/decisions/` before
step 1 — the first thing to do, since decision files are frozen to everyone but the owner and a
ruling that exists only in a chat does not exist):

- one Python package, **`tesseradb`**, the widget as its `[widget]` extra, the SDK and the
  in-process instance joining it later; the npm scope **`@tesseradb/*`**, the repo's
  `@tessera/*` renamed at step 1;
- **layers are usually one**, several only for different kinds of feature, never stacked label
  layers; the picker offers a layer with its dependents;
- **the tile grid is never shown**; the lasso highlight is the shape drawn;
- **the status strip is the default**, the expanded card optional;
- **the map's look follows DataMapPlot**, and **colour by cluster is exact only** — no geometric
  guess; D12's membership column is a prerequisite;
- **the render target is multi-million marks and 10⁴-plus artifacts a layer** (a layer may hold
  10⁶–10⁷); `DEFAULT_BUDGET` in the viewer is only the input's default;
- the client is **never responsible for disclosure** — its obligations are truthfulness, plus two
  credential concerns (§5.3, §7).

**Open, with the design's recommendation** — get these before the step that needs them:

| decision | needed by | recommendation |
|---|---|---|
| D1 Lit custom elements; React via `@tesseradb/react` | step 2 | as written |
| D2 shadow DOM with tokens, parts, slots | step 2 | shadow |
| D2a default explorer layout | step 2 | docked |
| D3 the package split (§8) | step 1 | as written |
| D5 instruments stay in the demo | step 2 | yes |
| D7 the first-cut set (thirteen tags) | step 2 | as written |
| D10 viewer-plane `serve.cors_origins` for token presentation | step 4's production docs; step 5 | **yes** — enumerated, viewer plane only |
| D4 notebook arm: browser-direct or proxy first | step 5 | follows D10 |
| D12 membership column — the palette's centre | step 3 | corpus extent |
| D8 fetch-model hint, D9 wire idioms, D11 selection operand, D13 label target, D14 verified-assertion plugin | server tracks | asked for; the client work proceeds without them |

**Nothing is built.** The store, the packages, the components, the widget, the C3 documents, and
every server-side ask are all ahead. The instrument (`clients/ts/`) is what exists and is what
step 0 starts from.

## 2. The work list, in order

Design §9 is normative for the order; this expands each step into what an orchestrator needs.
**One worktree per step**, branched from the previous step's merge (owner direction, as for the
artifact stages). Each step ends with the gate green and `client-delivery.md` updated in the
same change.

### Step 0 — the presented frame moves into the client

*Lands in* `clients/ts/core`. The store holds the `Composition` (already in `core/src/compose.ts`)
as its presented frame and absorbs the frame-applying store writes `viewer/src/binding.ts` makes
today; the duplicate `ViewState` in `viewer/src/viewportLayer.ts` goes (the driver's is the
type). `viewer/src/assemble.ts` stays — it is buffer concatenation, which is vis.
*Proves it:* `smoke.mjs` and `smoke-artifacts.mjs` green against the demo; `check-clients.sh`.
*Needs:* nothing ruled. **Not separable from step 1** — the legend fold walks the assembled
frame and the artifact channel reads its depth — so plan them as one track with two commits.

### Step 1 — the store

*Lands in* `clients/ts/core` (renamed `@tesseradb/client`): `createStore({viewerUrl, token |
authorise})`; the projections table of §4 with `Count` and `Masked` and the formatter;
`status.stale` on the **content key** (never `x-tessera-stale`); `setView`'s conversion (data
bbox → world → driver target/zoom, queued before `meta`); `dataXY`, `extentOf`; the encoding
accumulators moved from `viewer/src/colour.ts`; the artifact channel from `viewer/src/artifacts.ts`;
item and artifact detail, category resolution, filter composition and the legend fold from
`viewer/src/main.ts`; drops on identity-key change and on `setFilters`; the session artifact
table with refcounted ordinals (§5.10) ready for D12's column. `viewer` renamed `@tesseradb/viewer`
and consuming the store with nothing visible changed.
*Proves it:* smoke green; the driver's and replica's existing tests; new tests for `Count`/`Masked`
rendering, the stale rule, the drops, `setView`'s conversion.
*Needs:* D3.

### Step 2 — deck and components, box selection

*Lands in* new workspaces `clients/ts/deck` (`@tesseradb/deck`: `TesseraLayer` over `marks`,
`tiles`, `artifacts`; the slab moved from `viewer/src/slab.ts`; rank-to-colour; the density
texture from tile counts) and `clients/ts/components` (`@tesseradb/components`: `<tessera-store>`,
`<tessera-map>`, `<tessera-status>`, `<tessera-count>`, `<tessera-item-card>`, `<tessera-filter>`,
`<tessera-filter-panel>`, `<tessera-selection>`, `<tessera-explorer>`); box selection in the
`tiles` request form at a bounded depth with `Masked.exact` set honestly; the eight states of
§5.4; the mechanics of §5.9 (per-element subpath entries, `ContextRoot`, guarded defines,
deferred finalize, `accessor` decorators, the inline worker for the single-file bundle). The
viewer becomes `<tessera-explorer layout="overlay">` plus its instruments.
*Proves it:* smoke green **through shadow-piercing locators** (`page.locator('[part="count"]')`,
not `#stats`/`#panels`); the states rendered; the harness's first assertions (§9 acceptance).
*Needs:* D1, D2, D2a, D5, D7. Both new workspaces go into the root `package.json` `workspaces`
array with a `typecheck` script, which is all `check-clients.sh` needs.

### Step 3 — artifacts and encoding

*Lands in* deck and components: `<tessera-layer-picker>` offering a layer with its dependency
closure (`meta.layers[].depsOn`), `<tessera-artifact-list>`, `<tessera-artifact-card>`,
`<tessera-legend>`; wire geometry drawn (served hull/box as outlines; names and counts at
centroids placed by priority in a spatial hash; a dependent label's text at its own declared
centroid, no count); the `clusters.json` sidecar and the rings retired; lasso selection.
**When D12 serves:** the membership column decoded in the worker to a response-local index, named
on the main thread, the `u32` attribute per point in the slab, the lookup texture in the shader,
colour coverage and the colour-stale refetch (§5.10). Until it serves, points colour by column
only and clusters are outlines and names — do not build a geometric fallback.
*Proves it:* `smoke-artifacts.mjs` asserts hull and label under two principals; the harness
asserts an artifact's count does not move across a pan; with D12, that a coloured point's
ordinal resolves to a served artifact.
*Needs:* D12 (server track S3) for colour; nothing else.

### Step 4 — the examples, in the gate

*Lands in* `clients/ts/examples/{plain-html,react-explorer,canvas-store}` and `clients/ts/react`
(`@tesseradb/react`: hooks over `useSyncExternalStore`; `/components` wrappers with an optional
peer). The C1 pages carry the ten-line app-server example for the token and the §5.3 paragraph
about what it is under the passthrough plugin. Vue and Svelte documented, not checked.
*Proves it:* typecheck through the workspace; the harness's assertions against the C1 page.
*Needs:* D10 for the production-topology paragraph; the examples themselves run on the dev CORS.

### Step 5 — the widget

*Lands in* `clients/py/tesseradb/`: `Map(url, token=…)`, `authorise(session_url, credential,
terms)` marked operator-only, the anywidget class, the mount, the `ready`/token/`reauthorise`
custom messages, the traitlets (ids as decimal strings; up-sync at the settle), the wheel's build
hook that runs the bundle at wheel-build time. Run in Jupyter and Marimo by hand once; Marimo's
delivery of kernel-side custom messages verified there, not assumed.
*Proves it:* the notebook example; the build hook in the gate.
*Needs:* D4 and D10.

### Step 6 — C3's documents

*Lands in* `docs/design/contracts.md` §3.2 (amended: `layers`, `artifact_budget`, `k = 0`), an
OpenAPI 3.1 description generated from the server's types, worked decodes in `pyarrow` and
`apache-arrow` with a test, and the one-page obligations list (§3's twelve items).
*Needs:* D9. **Runs beside steps 2–5**, in its own worktree — it touches no client code.

### Server tracks — asks the design makes

Each is server-side, in its own worktree on the server crates, and each is a design of its own
with its own review where it touches the leak register:

- **S1 — D10:** `serve.cors_origins` for the viewer plane, enumerated, session plane excluded.
- **S2 — D9:** the wire idioms, decided at the contracts amendment.
- **S3 — D12:** the per-point membership column — deepest served artifact per named layer, per
  response, null otherwise — with its leak-register pass. Gates step 3's colouring.
- **S4 — D13:** a dependent artifact's target on the wire.
- **S5 — D8:** the fetch-model hint in `/v1/meta`.
- **S6 — D11:** the selection operand; behind it the export verb and the runtime-artifact path.
- **S7 — D14:** a verified-assertion auth plugin. Outside this design; named because C1's example
  is the claim-minting proxy until it exists.

## 3. What runs beside what

```
step 0 ──► step 1 ──► step 2 ──► step 3 ──► step 4 ──► step 5
                          │          ▲
                          │          └── S3 (D12) gates colour only
                          └──────────► step 6 (C3 documents), in parallel
S1 (D10) ── needed by step 4's production docs and step 5
S2, S4, S5, S6, S7 ── independent of the client steps
```

Steps 0–3 are serial on `clients/ts/core` and the new packages; do not split them across agents.
Step 6 and every server track are parallel to them and want their own implementers. The
allowlist for a parallel wave is the orchestrator's to write (`.claude/track-allowlist.toml`,
per [`agents/parallel-work.md`](agents/parallel-work.md)); note that `docs/decisions/*` and
`scripts/*` are frozen to everyone but the owner, and that the current file's tracks belong to
the artifact scale campaign and are not this work's.

## 4. Things that will bite

Found by the reviews and verified against the code; each cost a finding, so do not rediscover it.

- **`x-tessera-stale` is not the change signal.** It is the broadcast geometry stamp and moves at
  every flush, merge and compaction. Staleness keys on the replica's content key
  (`replica.ts` exposes `currentContentKey` for exactly this).
- **Bands hold f32 world positions only** (`bands.ts`); the f64 cell positions and `codes` exist
  only in the transient `ViewportResult`. `marks` is f32; `dataXY` derives from it.
- **The driver takes `{target, zoom}` and a size** (`driver.ts` `schedule`), not a bbox; it has
  no gesture input — stillness is the gap since the last call. `setView` converts (§4).
- **Composition is already in core** (`compose.ts`); `assemble.ts` is concatenation. Step 0 is
  the store absorbing `binding.ts`'s writes, not moving `assemble.ts`.
- **Point requests send `layers: []` today** (`viewer/src/artifacts.ts` explains why); with D12
  they must name the on layers and pay the pass. The `k = 0` channel stays for the in-view list.
- **`parentId` is on the wire only when both ends are in one response**, deliberately. The
  session table's parent links are partial; a walk that fails resolves to neutral.
- **A clustering's labels are a second layer** with `depends_on` (`[layer.labels]` expands to it;
  `publish-clusters.mjs --labels` publishes it). The picker names the closure.
- **The three decode lanes cannot share state** (`decoder.ts`); ordinal naming is on the main
  thread from a worker-local index (§5.10).
- **The decode worker is a relative URL** (`new URL('./decode.worker.js', import.meta.url)`);
  in a single-file bundle it silently falls back to inline decoding. Inline the worker as a Blob.
- **`customElements.define` throws on a second definition**; anywidget evaluates `_esm` per
  model. Guard every define.
- **Finalising a `Deck` synchronously on disconnect thrashes** under JupyterLab windowing and
  framework reparenting. Defer a settle; cancel on reconnect.
- **Lit decorators under `tsconfig.base.json`'s ES2022 target:** use standard decorators with
  `accessor`, or `@property` on a plain field silently does nothing.
- **`check-clients.sh` typechecks only workspaces in the root `workspaces` array** with a
  `typecheck` script.
- **The smoke scripts read `#stats` and `#panels`** (`smoke.mjs:99`, `smoke-budget.mjs:43`);
  shadow DOM hides both. Use shadow-piercing locators; move the budget figures to the probe.
- **`authorise` is gated by the operator session credential** (`session.rs`) and the only plugin
  is passthrough bare claims. `session-url` is never on a C1/C2 surface; the browser never holds
  the credential; C1's example is the claim-minting proxy until D14.
- **The 401/403 split is best-effort** (`state.rs`): a swept session is a 401. `expired` covers
  both on a token that previously worked.
- **The driver retries only 429** (`driver.ts:592`); 503 `not-ready` needs a bounded backoff.
- **`dev_cors_origins` is dev-only** (`cors.rs`, warns at startup); production browser-direct is
  D10.
- **A `tessera_id` is a `u64`**: `BigInt` off Arrow, a decimal string in JSON, events and
  traitlets; never a JS number.
- **`max_tiles_per_request` is 262,144** and the grid is 2¹⁶ per axis: a full-screen region "at
  pixel depth" is 10⁶ tiles. Use the `tiles` form at a bounded depth and set `exact` honestly.
- **The slab's CPU colours are rewritten per point on a colour-by change** (`slab.ts` `sync` with
  `writeColours`); cluster colour goes through the lookup texture instead, or several million
  marks pay tens of milliseconds and a 12 MB upload per interaction.

## 5. The gate, and what sits beside it

Every step runs the five gate commands from `CLAUDE.md` and reads the count. The smoke scripts
and the harness's DOM assertions need a served bundle, ports and headless Chromium, so they are
a target beside the gate — run them and report the numbers; they are not a step in it. The
figures in §5.10 are modelled; step 2's harness is where they become measured, and the delivery
record carries what was measured.

## 6. Talking to the owner

Escalations must be rulable without opening a source file: what the design says, what the change
would make it say, what the code does in one sentence, the options with consequences, a
recommendation with what it costs if wrong. Joe reads plain English; identifiers in brackets.
Agreement is a complete reply. The four constructions `CLAUDE.md` names do not belong in a
message.
