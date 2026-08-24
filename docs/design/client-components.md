# Client components — four customers, one stack

**Date:** 2026-08-24
**Status:** Provisional r3 — under review. r2 was reviewed once across three lenses (Appendix R);
r3 is a re-cut on owner direction — organised by who is served rather than by the code that
exists, and with the client's obligations stated as truthfulness, not secrecy — and has **not**
been re-reviewed. **To become normative:** one adversarial review of r3 and owner rulings on
§11. Defers to [`client-interaction.md`](client-interaction.md) for every obligation it
restates. **Amends** [`client-architecture.md`](client-architecture.md) §1 in one respect (§8).
Nothing in it changes the wire except what §3 asks for, none of which it depends on.
**Touches:** client-interaction §4, §6.1, §7, §8.6, §10, §12, §13, §15; client-architecture §1,
§6, §7 (D3, D4); artifact-system §6; contracts §3; `docs/design/README.md`; `docs/roadmap.md`
([#10], [#46], [#47]); `clients/ts/README.md`; `run_demo.sh`.

---

## 1. Who this serves

Four customers, and they want different things from the same system. Serving one well at the
expense of another is the failure this document is organised to avoid.

| | who | what they need | what "done" looks like |
|---|---|---|---|
| **C1** | an application developer who wants a **drop-in** | a map and its panels in their page with the fewest lines and no state wiring; restyle it; rearrange it; in the extreme, replace one piece | one tag, two attributes, a working masked map; CSS to restyle; a slot to replace a piece |
| **C2** | someone with an **existing visualisation** switching to Tessera as the backend | our data in their tool — deck.gl, MapLibre, a canvas of their own — with our caching and scheduling doing the asking | a headless store that takes their camera and hands back typed arrays and counts; adapters for the common engines |
| **C3** | a **power user** building their frontend from scratch | a wire interface that is well designed and fully documented; **no dependence on our client** | an OpenAPI description, the framing documented to the byte, the obligations list, and a wire with no idioms you have to be told |
| **C4** | the **in-repo demo**, and the **notebook**, which is the same customer in a different container | out of the box, ready to go, showing everything | `run_demo.sh` and `tesseradb.Map(url, credential)` both open on the full experience |

The client is **never responsible for disclosure**. The server decides what a principal may
have before any byte leaves it, whatever the client asked for. What a client can get wrong is
**truthfulness** — presenting a sample as a set, a stale view as current, a refusal as an empty
corpus, a masked count as a size — and the obligations this document carries are all of that
kind (client-interaction §10: *"the kit's subject is truthfulness, not secrecy"*). The one
credential concern, the notebook's token custody, is a different kind of thing and is treated
as one (§7). Everything else is ergonomics, and is reported rather than refused.

## 2. The stack, and the rule that keeps it honest

Five layers. **Each is complete for its customer without the one above it.** That is the whole
design; the rest is what each layer contains.

| layer | what | for | package |
|---|---|---|---|
| L0 | the **wire**: `/session/*`, `/v1/*`, framed Arrow | C3 | none — documentation |
| L1 | the **store**: verbs, decode, replica, driver, the presented frame, artifacts, filters, selection — headless, no DOM | C2 | `@tesseradb/client` |
| L2 | **adapters**: a deck.gl layer; a tile-shaped adapter; React hooks | C2 | `@tesseradb/deck`, `@tesseradb/react` |
| L3 | **components**: the composite `<tessera-explorer>` and the pieces it is made of | C1 | `@tesseradb/components` |
| L4 | the **demo** and the **widget** | C4 | `@tesseradb/viewer`, `tesseradb[widget]` |

C1 never sees L1 — the composite constructs its own store. C2 never loads L3 — no Lit, no
custom elements, no deck.gl unless they asked for the deck adapter. C3 never installs anything.
C4 is L3 in two containers.

## 3. C3 — the wire is the product

The power user needs nothing built and three things written, all of which client-interaction
§13 already names for its mode 3 and none of which exist:

- **An OpenAPI 3.1 description** of the viewer and session planes, generated from the server's
  own types so it cannot drift (client-interaction §8.6 makes the same point: *"the server
  already holds every input"*). Errors, the closed code list, `Retry-After` on every 429, the
  three refusal classes (contracts §3.1).
- **The framing, to the byte**, with a worked decode in two languages that share no code with
  ours — Python with `pyarrow`, JavaScript with `apache-arrow` — so a reader can check the
  document against a program rather than against our client. contracts §5 has the framing;
  what is missing is the worked example and the test that keeps it true.
- **The client obligations list** — one page, the rules the server cannot enforce because they
  are about presentation: the display states and that only `shown` carries a number; both
  figures of `{shown, total}` or neither; a stale view is marked and refresh is reachable; a
  masked count is a count of what *you* can see and is not a size; an absent artifact has no
  reason; the artifact channel asks for itself rather than reading artifacts off a replica's
  point path; held state is dropped when the identity key or the filter changes. Each rule
  states what goes wrong on the screen if it is broken. This is the document the conformance
  harness (§9) is the executable form of.

**Pre-release, the wire may still change** (decision 0048), and writing the OpenAPI description
is where its idioms will show. Three are visible now, recorded so the description pass decides
them rather than documents around them:

- **Artifacts are requested with `k = 0`** — a viewport request that asks for no points is the
  idiom for "just the artifacts". It works and reads as a trick; a reader of the description
  would expect it named.
- **`layers` omitted means every layer**, which is the expensive default; a client that wants
  none must send `[]`. The safe default is the cheap one.
- **The artifacts frame is omitted when empty**, so a decoder must treat absence and emptiness
  as one state by knowledge rather than by shape.

None of these is a defect and all three are the kind of thing that is cheaper to change now
than to explain forever. They are listed, not ruled — §11 D9.

**One topology question is C3's alone.** A frontend built from scratch that calls Tessera from
the browser is client-interaction §7's T1, and the server's CORS is `dev_cors_origins` — dev
only, logged as such, deliberately. Either T1 is a supported production topology and needs an
enumerated `cors_origins`, or C3 is told that a browser never calls Tessera directly and their
app server sits in front (T2) — which is what every documented integration says today. §11 D10.

## 4. C2 — the store, and getting data out of it

C2 has a visualisation and a camera. What they want from us is: *tell the store where I am
looking, and hand me what to draw, from your cache, with your scheduling.* The store is that
object, and it is `@tesseradb/client`'s main export.

```ts
const store = createStore({viewerUrl, sessionUrl, token});   // or authorise: () => Promise<Token>
store.setView({bbox, width, height});                        // on every camera change; the driver debounces
store.subscribe('marks', (m) => draw(m));                    // typed arrays, ids, scalars, counts
store.pick(id).then(showRecord);
```

Three calls to a working integration. What is behind them is the machinery client-architecture
§3–§5 specifies and has built: the driver (the scheduler that decides when to ask, retry,
revalidate and look ahead — it takes a view and a size and owns every timer), the replica (the
cache, keyed by identity key, dropped when that changes), and frame composition (which held
bands contribute to the view, at what prefix, on what authority). Plus what is still in the
viewer today and moves here: the presented frame (`assemble.ts`, minus its buffer
concatenation), the artifact channel, item and artifact detail, filter composition, category
resolution and the legend fold. Client-architecture §6 ordered that move; its steps 1–2 are done,
3 is half done, 4–5 have not started. §9 finishes it.

**The store owns no camera.** C2's engine does; the store is told. Its `setView` takes a bbox in
data coordinates and a pixel size, which is what every engine can produce, and the driver's
motion region infers gesture-versus-still from the timing of the calls — the integrator does not
report gestures.

**Projections** — typed, observable, read-only — are the data-out surface:

| projection | carries |
|---|---|
| `meta` | `/v1/meta` as this principal sees it: columns, operands, layers, views, extent |
| `status` | `idle · loading · retrying · shown · empty · refused`; `sessionWarm`; the last refusal; **`stale`** — the change signal (`x-tessera-stale`) has fired against the presented view |
| `view` | the drawn region and depth; `visible` and `matched` as `Masked`; `served` as `Count`; provisional marks as a `Count` that is not exact |
| `marks` | the draw list: `ids` (`BigUint64Array`), `codes`, positions, `scalars` (typed arrays by column), per-tile provenance, and its `Count` |
| `artifacts` | the layer selected; the served set with geometry, content and `parentId`; the tree; the channel's status and refusal |
| `selection` | the picked item's record (named fields) or its refusal; the opened artifact or its refusal |
| `filters` | operands from meta; the composed `FilterExpr` as sent; per-column value lists and their refusals |
| `legend` | per column: the codes marks on screen carry and their resolved values |
| `replica` | bytes, points, bands held; the last plan's held-versus-fetched split; look-ahead spend |

**Positions come in the coordinate system the customer needs**, because every engine wants a
different one. `marks` carries the wire's cell-space positions (`Float64`, 32 bits per axis) and
the deck world `f32`; `store.dataXY(marks)` gives the corpus's own data coordinates from
`meta`'s quantisation for a renderer that is not ours, and a geographic corpus's extent is its CRS
(client-interaction §12). The transform is a per-point pass done on request, not on every
frame.

**Verbs** are the write surface: `setView`, `setFilters(expr)`, `setLayer(name | null)`,
`setColourBy`, `setBudget`, `pick(id)`, `openArtifact(id)`, `clear()`, `refresh()`, `dispose()`.
C2's own filter UI calls `setFilters` with a `FilterExpr` built from `meta.filterOperands`.

**Numbers are typed by what they are**, so a customer drawing their own panel gets the right
figure by taking the type's word for it:

- `Count = {shown, total, exact}` — a **served sample** of a set: `served`, the draw list,
  provisional marks. Show both figures, or neither when `exact` is false or the view is stale.
- `Masked = {value, exact}` — a **number-channel scalar** with no sample behind it: `visible`,
  `matched`, an artifact's count. One figure or none.

A formatter in the package renders each correctly; a customer who writes their own has the
types telling them which is which. The point is not that a customer cannot get it wrong — they
can — but that the one obvious way is right. "12,040 of 12,040" against a cluster is false, not
secret, and the type makes it unnatural to write.

**What the store drops, and when.** The replica, held artifacts, selection, per-column value
lists and legend resolutions go on identity-key change — a different principal's picture must
not sit under the new one's. Held bands also go on `setFilters`, because the identity key
excludes filters and the server cannot tell the client its holdings no longer match. A held
whole-layer artifact set goes when the content key it was fetched under rotates, and on
`refresh()`. These are staleness rules: what would otherwise be shown is a picture that is no
longer true.

**Adapters, one per engine family:**

- **`@tesseradb/deck`** — `TesseraLayer`, a deck.gl `CompositeLayer` over `marks` and `artifacts`,
  which is what the viewer's `buildViewportLayers` and its GPU slab are today, given a class
  boundary. For the customer who owns a `Deck` already — a basemap, their own layers. Peer
  dependency on `@deck.gl/*`; a customer with deck has one copy.
- **A tile-shaped adapter** for MapLibre, OpenLayers and any engine that asks per tile —
  client-architecture D3's ruling: per-tile asks batched into region fetches, each answered
  from the replica, empty distinct from refused per ask. ⊘ Specified there and in
  `tile-addressed-integration.md`; built when a customer with such an engine exists, and
  client-interaction §8.6's tile-addressed GET alias on the server is the cheaper half of the
  same story.
- **`@tesseradb/react`** — `useTesseraStore(options)` and `useProjection(store, name)` over
  `useSyncExternalStore`, with `dispose` in the effect cleanup so StrictMode's double mount
  does not leak a driver. Its `/components` entry carries the C1 element wrappers (§5), kept
  behind a separate entry so a C2 user who wants only hooks never loads Lit.

## 5. C1 — the components

The drop-in customer's minimal page:

```html
<script type="module" src="https://…/tessera-components.js"
        integrity="sha384-…" crossorigin="anonymous"></script>
<tessera-explorer viewer-url="https://tessera.example/v1" token="…"></tessera-explorer>
```

One tag. **`<tessera-explorer>`** is the composite: the map with its panels around it in a
default layout — view info, filters, legend, layer picker, item card, artifact card — the whole
demo experience without the instruments. It constructs its own store from its attributes;
there is no provider to write and nothing to wire. The token is an attribute because that is
the simplest thing, with an `authorise` property — a function returning a token — for renewal
when the deployment's lifetime runs out; a page that supplies neither shows the expired state
when it comes, which is a display state and not an error.

Where does the token come from? From the customer's own server, which holds the session
credential and calls `/session/authorise` for the signed-in user — client-interaction §7's T2,
the documented topology. The package ships a ten-line Express example, because the drop-in
story is two files and this is the second one.

**Restyle, rearrange, replace** — three levels, each cheaper than the next:

1. **Tokens.** Every colour, font, spacing and radius is a `--tessera-*` custom property with a
   neutral default that follows `color-scheme`, so light and dark come from the host page
   unconfigured. One token is a size — `--tessera-map-height`, the map's height when its
   container gives it none. The map's **data palette** is a `palette` property and not a token:
   it encodes data, and brand colours are the wrong thing to encode data in.
2. **Parts and slots.** Every structural element carries a `part` — `count`, `field`, `value`,
   `refusal`, `refresh` — for `::part()` from outside the shadow root. The explorer's layout is
   **named slots with default content**: `sidebar`, `detail`, `filters`, `legend`, `layers`,
   `overlay`. Put your own element in a slot and the default piece is gone; an attribute
   (`panels="filters legend"`) hides the ones you do not want.
3. **Pieces on their own.** `<tessera-map>`, `<tessera-view-info>`, `<tessera-item-card>`,
   `<tessera-filter-panel>`, `<tessera-layer-picker>`, `<tessera-artifact-card>`,
   `<tessera-legend>` are the explorer's contents and are usable without it, in the customer's
   own layout. A `<tessera-map>` on its own constructs a store from its attributes exactly as
   the explorer does; the others find the nearest store by context and need no attributes at
   all. **Every piece also takes its data as a property** — `.item`, `.artifact`, `.counts`,
   `.values` — and renders it; the store binding is what fills the property in when the
   customer does not. A card fed from the customer's own fetch, or from a Python cell, works
   without a store on the page.

Replacing a piece entirely is the slot; a replacement that wants live data reads the store from
its context (`this.closest('tessera-explorer').store`, or the Lit consumer) and subscribes — a
ten-line example ships with the package. **`<tessera-store>`** exists for the one case that
needs an explicit provider: two maps sharing a store for linked views. It is not needed for
anything else and the documentation does not lead with it.

**What the pieces owe, and it is all about not saying something false:** the display states are
drawn, on the canvas as well as in the panels, so a refused or expired view never reads as an
empty corpus; only `shown` renders a number; `<tessera-view-info>` shows `visible`, `matched`,
`served` in that order because their relationship is the content, and owns the **refresh
control**, present whenever `status.stale` is; the item card renders fields by name, never by
position (`/v1/items` omits absent fields, so position lies), and distinguishes a miss from a
broken pick; the filter panel is built from `meta.filterOperands` alone and renders a refused
enumeration as a refusal with a free-text entry rather than an empty list; the legend names only
values marks on screen carry. A piece with **no store and no data** renders a detached state —
no numbers, and neither "empty" nor "refused", both of which are answers.

**Authoring and mechanics** — decided here, not left to the examples:

- **Custom elements, with Lit.** The one embedding primitive React, Vue, Svelte, Angular, plain
  HTML and anywidget all share; one implementation of the obligations above. Lit for the
  context protocol, reactive properties and `@lit/react` — about 5 kB (assumed from its
  published size). React 19 takes custom elements natively; `@tesseradb/react/components` wraps
  them with typed props and events for typing and for React 18.
- **Two distributions.** Unbundled ESM with `lit`, `@deck.gl/*` and `@luma.gl/*` as peers, for
  anyone with a bundler; and one self-contained bundle — the `<script type="module">` above —
  for a page with no build step, which is also the widget's `_esm` (§7). The bundle is
  published with its subresource-integrity hash, and the snippet above carries it — a page
  that loads a script from a host it does not control should say which bytes it expected.
- **Client-only import**: Lit and deck.gl touch `window` at import, so a server-rendered page
  imports the package dynamically on the client. **Define before render**: the entry defines
  every element on import; Vue needs `isCustomElement: tag => tag.startsWith('tessera-')`, and
  Vue and Svelte need property bindings for object values. **Context before provider**: the
  context protocol's request is a one-shot event, so the package attaches Lit's `ContextRoot`
  once on import to replay a request that arrived before its provider.
- **Per instance, not per module**: a `<tessera-map>` owns its `Deck`, its slab and its trace
  probe (a property; the demo publishes the first map's onto `window` for the smoke scripts),
  finalises the `Deck` on disconnect because browsers cap live WebGL contexts, and is
  `display: block` with the height token because a custom element is inline and heightless and
  deck.gl sizes its canvas from its parent — without this a bare map is 0 × 0 and silent.
- **Standard decorators with `accessor`** (TypeScript ≥ 5.2): under the workspace's ES2022
  target, legacy `@property` on a plain field is shadowed by class-field definition and does
  nothing.

Two pieces the direction did not name and this design declines: a **search box** (text `match`
is a filter operand and lives in the filter panel) and a **principal switcher** (an
application's user is one principal; it is an instrument, §7).

## 6. Artifacts, at every layer

Artifacts are a data shape, not a customer, and each layer carries them: the wire has
`Artifact.centroid`, `box` and `hull` derived per principal, `content` and `parent_id`; the
store's `artifacts` projection holds the served set and builds the tree from `parentId`;
`TesseraLayer` draws a hull or box where the artifact carries one (each is per-artifact `null`
where the layer declares none), a centroid mark otherwise, the first content string as a label
where the layer's first supplied kind is text, and parent links where both ends are in the
response; the explorer has a layer picker and an artifact card. The viewer's `clusters.json`
sidecar — rings placed by the publishing script, from a time when no geometry crossed the wire
— is retired at §9 step 3.

What every layer states the same way, because the wire gives nothing to fill any other reading
from: the count is over the whole membership and does not move with the viewport, so a card is
steady during a pan; an absent artifact has no reason — no "hidden" rendering, no greyed entry,
and an artifact with no served parent is a root; one layer at a time, named in the request,
because omitting the selector pays for every layer; and the channel asks for itself with its
`k = 0` request per settled view, since the replica elides held tiles and an elided tile carries
no artifacts.

**Fetch model.** artifact-system §6 gives two — small layers fetched whole once and held, large
layers fetched as the budgeted coarse cut and refined on zoom — and records ⊘ that nothing in
`/v1/meta` says which a layer suits. The store picks by observation: it asks the whole map at
the layer's default cut on selection and holds the answer if it is under a configured size,
otherwise fetches per view. ⊘ A wide response over a large flat layer is unbounded today; that
configured size is the only bound this design puts between a customer and one, and streamed
continuation stays owned by the client epic. §11 D8 asks for the hint. **Specified and not
built, and unchanged here:** membership by predicate, runtime artifacts and the edit verb,
search over artifacts, polygon shapes beyond bbox (artifact-system §10).

## 7. C4 — the demo and the notebook

**The demo** is `<tessera-explorer>` plus the instruments — the things that measure Tessera
rather than use it and are deliberately not in the library: the dataset picker and principal
presets, the trace bar (`?trace=1`), the stage-timing readout (from the response trailer's
`stage_ns`), the request-failure list and the replica drawer's spend figures. Each reads a
projection like any piece. `run_demo.sh` keeps its invocation, its scales and its port table;
the smoke scripts keep running against the demo page, reading the probe from
`document.querySelector('tessera-map')`. `@tesseradb/viewer` keeps its name (client-architecture
D4) and becomes this page.

**The notebook** is the same explorer in an anywidget, shipped as the `widget` extra of
**`tesseradb`** — the one Python package, which later also carries the SDK (#47) and, in a
separate piece of work, an in-process Tessera for a notebook. The base install has no
JavaScript in it; `pip install tesseradb[widget]` adds anywidget and the bundle. The widget's
`_esm` is the self-contained bundle plus a `render({model, el, signal})` that mounts `<tessera-explorer>` into
`el`; `_css` is empty. anywidget reaches Jupyter, Marimo (`mo.ui.anywidget`, two-way), VS Code
and Colab from one package. What crosses the kernel boundary, as synced traitlets, is **control
and selection, never data**: URLs and view name down; `bbox`, `layer`, `colour_by` and
`filters` (the composed `FilterExpr`, applied without the panel's debounce) both ways;
`selected` and `selected_artifact` (one `tessera_id` each) up. `widget.selected` in the next
cell is the picked item; setting `widget.filters` redraws; the pan path stays browser → Tessera
and the kernel is never on it (client-interaction §7). A `to_arrow()` that ships the current
`marks` over the comm as Arrow buffers for a DataFrame is the natural next verb and is not in
the first cut.

**Token custody** is client-interaction §15's open item and the one credential concern in this
document. The answer is that **the token is never model state**: the kernel holds the session
credential and authorises; the JS side sends `ready` from `initialize({model})` — once per model,
so two views of one widget share it and two widgets do not — and the kernel answers with the
token as a **custom message**, which no route that serialises widget state can save: not the
frontend's opt-in "save widget state", not `nbconvert --execute`, not papermill, not a headless
run where no frontend mounts. Expiry sends `reauthorise`; a view rendered after a page reload
sends `ready` again. What remains is a token in browser memory for `token_max_lifetime`, which is
browser-direct's exposure everywhere. For a JupyterHub whose browser cannot reach Tessera, the
**`jupyter-server-proxy` arm**: the base URL is the proxied path, the proxy holds the credential,
nothing crosses. ⊘ Documented, not built, in the first cut; the widget's messages are built at
§9 step 5.

The SDK (#47) does not exist and the widget does not wait for it: authorise is one HTTP call
with `urllib`, which becomes the SDK's first verb when the SDK lands in the same package. The widget computes no masked
quantity and decides nothing about what is drawn.

## 8. The packages

The npm scope is **`@tesseradb`**, matching the Python package; the repository's two existing
packages, `@tessera/client` and `@tessera/viewer`, take the new scope at §9 step 1 — no
deployment holds the old names (decision 0048), and client-architecture D4's ruling is about the
viewer keeping its *name*, which it does.

| package | customer | depends on | contents |
|---|---|---|---|
| `@tesseradb/client` | C2 (and every layer above) | `apache-arrow` | verbs, decode, coordinates, replica, driver, composition, **the store** |
| `@tesseradb/deck` | C2 with deck.gl; C1 through the map | client; peers `@deck.gl/*`, `@luma.gl/*` | `TesseraLayer`, the slab, the encoding object |
| `@tesseradb/react` | C2 in React; C1 in React | client; peer `react`; `/components` entry also peers `@tesseradb/components` | hooks; element wrappers |
| `@tesseradb/components` | C1 | client, deck; peer `lit` | `<tessera-explorer>`, the pieces, `<tessera-store>`, the formatter, tokens and parts |
| `@tesseradb/viewer` | C4 | components | the demo page and its instruments |
| `tesseradb` (Python) | C4 | `[widget]` extra: anywidget, the components bundle committed | the widget class, the mount, the messages; later the SDK and the in-process instance |

This amends client-architecture §1's two-package split. Its second package — "the deck.gl
binding and its GPU slab … panels, the trace bar, DOM wiring" — becomes `@tesseradb/deck` and
`@tesseradb/components`, and `@tesseradb/viewer` keeps its name and its instruments and consumes
them. D1a's boundary rule is unchanged and is where `client` stops and `deck` starts; D3's
adapter story is §4's tile-shaped adapter; D4's ruling is kept. §11 D3 asks the owner to rule
the split explicitly rather than inherit it.

The colour encoding accumulators — palette ranks by observed frequency, sticky numeric domains —
live in `@tesseradb/deck`'s encoding object, shared by `TesseraLayer` and `<tessera-legend>`, fed by
the store's `legend` projection and reset by the store's `reset` event on identity-key change.
The store owns what was counted; the vis side owns what colour it gets, which is where
client-architecture §1 draws the line.

## 9. What lands, and what proves it

The order, as client-architecture §6 gives its own — the demo working at every step, the smoke
scripts as the net, one worktree per step:

0. **The presented frame moves** into `@tesseradb/client`: `assemble.ts`'s composition half and
   `binding.ts`'s frame-applying store writes; `ViewState` becomes the driver's type. Finishes
   client-architecture §6 step 3. Not separable from step 1 — the legend fold walks the
   assembled frame and the artifact channel reads its depth.
1. **The store**, with the projections, `Count` and `Masked`, `status.stale`, `dataXY`, and
   `createStore`. The artifact channel, item and artifact detail, category resolution, filter
   composition and the legend fold leave `main.ts`. The viewer consumes the store; nothing
   visible changes; `smoke.mjs` and `smoke-artifacts.mjs` green.
2. **`@tesseradb/deck` and `@tesseradb/components`**: `TesseraLayer` and the encoding object;
   `<tessera-map>`, `<tessera-view-info>`, `<tessera-item-card>`, `<tessera-filter-panel>`,
   `<tessera-explorer>`. The viewer becomes the explorer plus instruments. Smoke green.
3. **Artifacts**: `<tessera-layer-picker>`, wire geometry drawn, the sidecar retired,
   `<tessera-artifact-card>`, `<tessera-legend>`. `smoke-artifacts.mjs` asserts hull and label
   render under two principals.
4. **C1 and C2 examples in the gate**: a plain-HTML page, a React page using the explorer, a
   page using the store over a plain canvas with none of our rendering, and `@tesseradb/react`. `check-clients.sh`
   typechecks new workspaces as written. Vue and Svelte documented, not checked.
5. **`tesseradb[widget]`** and the notebook example, run in Jupyter and Marimo by hand once; the
   built bundle **committed** into the Python package so it installs without Node, and the gate
   rebuilds it and fails on a diff.
6. **C3's documents**: the OpenAPI description generated from the server's types, the worked
   decodes with their test, the obligations list; and whatever §3's idioms were ruled to become.

**Acceptance** is the conformance harness client-interaction §10 asked for and has had no
subject: the smoke scripts become DOM-level assertions through `::part()` — only `shown` renders
a count; both figures render or neither; a refusal renders as one; no count renders against a
stale view and a refresh control is present when one is; an artifact's count does not move
across a pan; a switch of principal empties every card. They run in the gate against the demo
page, and against the C1 example page, which is the same assertions with none of our layout.
The C2 example is the check that the store is usable with none of our rendering.

## 10. Options considered and rejected

- **React-native components with wrappers for the rest.** Notebooks, Vue and Svelte get the
  worse path, and the demo carries a React runtime to draw its panels.
- **Headless core only, no components.** Serves C2 and C3 and abandons C1 and C4 to assembling
  the demo themselves.
- **Components only, no separately usable store.** Serves C1 and makes C2 adopt Lit and deck.gl
  to get a cache.
- **A provider element required for every page.** One more thing to wire for C1, for a benefit
  — sharing a store — that one customer in a hundred wants. `<tessera-store>` exists for that
  customer and nobody else has to know.
- **Light DOM with a class contract.** Simpler to restyle from a host stylesheet, and impossible
  to keep from being restyled by accident; §11 D2.
- **Folding the deck adapter into `@tesseradb/client`.** Makes the headless package depend on the
  GPU path, which a customer drawing to their own canvas never wants; the boundary
  client-architecture §1 drew is worth a package.

## 11. Decisions for the owner

- **D1 — custom elements, authored with Lit; React through `@tesseradb/react`.** Recommended.
- **D2 — shadow DOM with tokens, parts and slots**, against light DOM with classes. Recommended:
  shadow.
- **D3 — the package split of §8**, which amends client-architecture §1 from two packages to
  four plus the demo. Recommended as written.
- **D4 — the notebook's first-cut posture.** (a) Browser-direct with the token as a custom
  message, the proxy arm documented; (b) proxy arm only. (b) is a base-URL change over (a).
  Recommended: (a).
- **D5 — instruments stay in the demo.** Recommended.
- **D6 — one Python package, `tesseradb`, at `clients/py/tesseradb/`**, the widget as its
  `[widget]` extra with the built bundle committed; the SDK and the in-process instance join
  it later; sharing no code with `reference/`. Ruled by the owner 2026-08-24 in conversation —
  recorded here so a decision file can carry it at promotion.
- **D7 — the first-cut set**: the explorer, seven pieces, `<tessera-store>`, `TesseraLayer`,
  the hooks. Two declined pieces recorded in §5.
- **D8 — a fetch-model hint in `/v1/meta`** (artifact-system §6's ⊘). Asked for, not depended
  on.
- **D9 — the wire idioms of §3**: whether `k = 0`, `layers`-omitted-means-all, and the omitted
  artifacts frame are changed before the OpenAPI description is written, or documented as they
  are. Recommended: decide each at that pass, with 0048 in hand.
- **D10 — whether browser-direct (T1) is a supported production topology.** If yes, an
  enumerated `serve.cors_origins`; if no, C3 is told an app server sits in front.
  Recommended: no for now — every documented integration is T2, and a production CORS surface
  is a decision about who may present tokens, not a client convenience.

## 12. Provenance

The owner's direction of 2026-08-24, in two parts: the four-point brief (package, split,
restyle, artifacts) and the four-customer re-cut with the correction that the client is never
responsible for disclosure. The boundary rule, the driver and the migration order are
client-architecture §1, §3 and §6 (driver built, `6aa84f7`). The three-layer stack, the
projections rule, mode 3's document set and the anywidget ruling are client-interaction §10, §12
and §13; the stale mark and the refresh affordance are its §4 and §6.1; the notebook fork is
its §7; the token custody item is its §15, answered here. The two artifact fetch models and the
⊘ hint are artifact-system §6; the wire's headers and refusal classes are contracts §3. anywidget's
`render`, `initialize`, custom messages and Marimo's two-way sync are from its current
documentation.

On the roadmap: [#10] becomes four packages; [#46]'s reference viewer becomes §7's demo; the
widget precedes [#47] and does not depend on it, and both live in `tesseradb`. The roadmap
paragraph is updated on promotion.

## Appendix R — review trail

- 2026-08-24: r1 drafted (Provisional).
- 2026-08-24: r2 — reviewed across three lenses (disclosure, embeddability, corpus fit),
  twenty-seven findings dispositioned in one pass. The findings that changed the shape: the
  token traitlet unsound on the routes that serialise kernel-side state (now a custom message);
  the artifact count typed as a sample (now `Masked` beside `Count`); a bare served count
  reachable as `marks.length` (now carried with its `Count`); a held artifact layer that never
  saw the content key rotate (now evicted on rotation and refresh); `setFilters` without its
  drop; `assemble.ts` and `binding.ts` coupling the migration's first two steps (now step 0);
  colour ranks placed on both sides of the boundary (now one encoding object); the change
  signal and refresh affordance absent (now `status.stale` and the view-info control); the
  drop-in deck.gl layer missing (now `TesseraLayer`); the third package amending
  client-architecture §1 unstated. Nothing re-decided client-interaction §14's rejections.
- 2026-08-24: r3 — re-cut on owner direction, not re-reviewed. Organised by four customers; the
  client's obligations restated as truthfulness rather than secrecy throughout; the composite
  `<tessera-explorer>` added as C1's one-tag entry, with a token attribute and no provider;
  every piece takes its data as a property with the store binding as the default; the store
  given its C2 surface (`createStore`, `setView` from any camera, `dataXY`, the hooks); the deck
  adapter split into its own package; C3's document set, the wire's idioms and the production
  CORS question added (D9, D10). Same day, owner: the Python package is `tesseradb`, one
  package with the widget as an extra (D6); the non-deck example renderer is a plain canvas;
  the npm scope is `@tesseradb` to match, the repository's `@tessera/*` names changing at §9
  step 1 (pre-release, decision 0048).
