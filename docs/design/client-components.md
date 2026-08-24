# Client components — embeddable, restyleable, and covering artifacts

**Date:** 2026-08-24
**Status:** Provisional r2 — under review. Reviewed once across three lenses (disclosure,
embeddability, corpus fit; Appendix R) and revised in one pass. **To become normative:** owner
rulings on the decisions in §11. Defers to [`client-interaction.md`](client-interaction.md) for
every obligation it restates. **Amends** [`client-architecture.md`](client-architecture.md) §1
in one respect, stated in §2: the display package it describes becomes `@tessera/components`,
and `@tessera/viewer` becomes that package's first consumer. Nothing in it changes the wire
except D8, which it asks for and does not depend on.
**Touches:** client-architecture §1, §6, §7 (D1a, D3, D4 — kept; §2 records the amendment);
client-interaction §4, §6.1 (the stale mark and the refresh affordance), §7 (T3), §10, §12,
§15 (notebook token custody — addressed here); artifact-system §6; `docs/design/README.md`;
`docs/roadmap.md` ([#10], [#46], [#47]); `clients/ts/README.md`; `run_demo.sh`.

---

## 1. What this is for

The viewer that exists is an instrument. `clients/ts/README.md` says so in its first line, and
it is right: `@tessera/viewer` is one `main.ts` of 1,160 lines that wires a store to a deck.gl
canvas and renders its panels as HTML strings. It demonstrates a correct client at 10⁹ items,
and it cannot be embedded in anything.

This document designs four things:

1. **The client packaged as reusable components** that drop into the common JS frameworks and
   into Jupyter and Marimo notebooks.
2. **The design split into pieces** — a viewer, a point-info card, a filter panel and a
   view-info panel are the starting set; the set itself is decided in §5.
3. **Clean, modern, and restyleable** to fit whatever application embeds them (§7).
4. **Artifacts covered as well as points** (§6).

The output is a set of components that integrate simply into web applications, that show the
right way to work with Tessera — the replica cache and its session-bound lifetime, the display
states, `{shown, total}`, the refresh affordance — and that are **not required** to work with
it: a frontend that uses the headless client and draws what it is handed is conformant without
them (client-interaction §10). `run_demo.sh` serves a demo app assembled from them.

What it is not: a redesign of the scheduler, the replica or the wire. Those are settled and
built (client-architecture §3–§5) and this document composes them.

## 2. The shape

Three layers, each usable alone — client-interaction §10's rule for a component library:

- **`@tessera/client`** stays headless and gains the one object components need from it: a
  **store** that owns every piece of conformance-bearing state and exposes it as observable
  projections (§4). This is client-interaction §10's *replica store* layer, widened to hold
  filter, selection and artifact state; §10's *session client* remains `TesseraClient` beneath
  it. It is called a store and not a session because "session" already names the
  credential-bound thing on the wire and in the server, and the two must not be read as one.
- **`@tessera/components`** is new: custom elements over the store, written with Lit, styled
  through a token-and-parts contract, with a React wrapper and native embedding everywhere
  else (§3, §5, §7). It also carries client-interaction §10's third layer, the **drop-in
  deck.gl layer** — a `TesseraLayer` an integrator adds to a `Deck` they already own — which
  `<tessera-map>` composes rather than replaces (§5).
- **`tessera-widget`** is a Python package embedding the same components bundle through
  anywidget, so Jupyter, Marimo, VS Code and Colab get one map from one build (§8).

`@tessera/viewer` becomes the demo: the components plus the instruments that measure Tessera
rather than use it (§9). Client-architecture §1 describes a two-package split whose second
package holds "the deck.gl binding and its GPU slab … panels, the trace bar, DOM wiring"; this
document moves the binding, slab and panels into `@tessera/components` and leaves the trace
bar and the demo wiring in `@tessera/viewer`. D4's ruling — the package keeps its name — is
kept; what changes is what the package holds, and D3 in §11 asks the owner to rule the third
package explicitly rather than inherit it.

The rule that keeps the layers honest is client-interaction §12's: **components consume the
store's projections and register gestures as verbs; they never fetch.** Everything a component
could get wrong about disclosure is something it cannot do at all — it has no client to fetch
with.

## 3. Why custom elements

The framework question has one answer every other answer has to be wrapped in. A custom
element is the embedding primitive React, Vue, Svelte, Angular and plain HTML share, and it is
exactly what anywidget's `render({model, el})` asks for: something to append to `el`. One
implementation of every display obligation; one place the conformance harness has to look.

Lit is the authoring layer: small (about 5 kB, assumed from its published size rather than
measured here), the reference implementation of the community **context protocol** (how a card
finds its store without the integrator threading a prop through their tree), and the home of
`@lit/react`, which produces typed React components with event props from custom elements.
React 19 handles custom elements natively; the wrapper exists for typing and for React 18.

Options considered and rejected:

- **React-native components with wrappers for the rest.** Notebooks, Vue and Svelte get the
  worse path, and the demo would carry a React runtime to draw its panels.
- **Headless core only, framework-native components per framework.** N implementations of the
  display obligations, each a place for a bare sample count to be rendered — the ground on
  which client-interaction §10 rejects reimplementation.
- **Custom elements with no library.** Viable; the components are small enough. Lit buys the
  context protocol, reactive properties and a maintained React bridge; those three are what
  would age worst hand-written.

**Framework mechanics that are part of the design, not left to the examples:**

- **Client-only import.** Lit and deck.gl touch `window` and `customElements` at import, so a
  server-rendered page (Next, Nuxt, SvelteKit) imports `@tessera/components` dynamically on
  the client — `ssr: false`, or inside `onMount`. The package documents this at the top.
- **Define before render.** Vue resolves unknown tags as components unless
  `compilerOptions.isCustomElement` admits `tessera-*`; Vue and Svelte bind an object to an
  attribute (`"[object Object]"`) unless the element is upgraded before render or the binding
  is a property binding. The package's entry defines every element on import, and the
  documentation states the two lines each framework needs.
- **Context before provider.** The context protocol is a one-shot event: a consumer that
  upgrades before its provider is connected loses the request. The package attaches Lit's
  `ContextRoot` to the document once on import, which replays late requests. React rendering
  both elements needs nothing — it inserts parents before children.
- **Dependencies are peers, not bundled.** `@deck.gl/*`, `@luma.gl/*` and `lit` are peer
  dependencies and the package ships unbundled ESM, so an integrator who already has deck.gl
  has one copy. The anywidget `_esm` is the one bundled artifact (§8).
- **Decorators.** Standard decorators with the `accessor` keyword (TypeScript ≥ 5.2; the
  workspace has 5.9) — under `tsconfig.base.json`'s ES2022 target, legacy `@property` on a
  plain field is shadowed by class-field definition and silently does nothing.

deck.gl stays as the map's renderer. It is the heavy dependency (about 1 MB, assumed), it is
the GPU path that reaches 10⁹, and lonboard is prior art for the embed itself — deck.gl
mounted from anywidget — though not for a pan path that fetches from the browser, which is
this design's own claim to make good on. A `<tessera-map>` without deck.gl is a different
component.

## 4. The store, and its projections

Today `viewer/src/state.ts` holds forty-odd fields in one `AppState`, and `main.ts` mutates
them from handlers that also issue requests: item detail on pick, category resolution, filter
value enumeration, the artifact channel, the legend fold. Client-architecture §1 assigns every
one of those to the client side and §6 orders their move; steps 1 and 2 of that order are done
(the driver landed, `6aa84f7`), step 3 is half done — `compose.ts` is in core while
`assemble.ts` and the frame-applying half of `binding.ts` are still in the viewer — and steps
4 and 5 have not started. The artifact channel (`viewer/src/artifacts.ts`) issues requests
from the vis package, which is the boundary rule broken outright. This document is where the
rest moves, because a component library cannot be built over state that lives in the app it
replaces.

**`TesseraStore`** is one object in `@tessera/client`, constructed from a `TesseraClient`, the
driver's injected clock, and a **token supplier** — `() => Promise<{token, expiresAt}>` — so
that the T2 integrator whose app server mints tokens (client-interaction §7) and the demo,
whose supplier is `TesseraClient.authorise`, construct it the same way. It owns:

- the token and its lifetime, and the **replica**, whose cache it drops on identity key change
  (client-interaction §10: the cache lifetime is the session's) and **on `setFilters`** — the
  identity key deliberately excludes filters, so the client drops held bands itself when they
  change, as `main.ts` does by hand today;
- the **driver** (client-architecture §3), unchanged;
- the **presented frame**: what `assemble.ts` computes today — which bands contribute per tile,
  at what prefix, on what authority — as client-architecture §5 specifies, minus the buffer
  concatenation, which stays vis-side;
- the **artifact channel** (§6): which layer, the request cadence, the held set;
- **filter state**: the draft, the composed expression, operand values and their refusals;
- **selection**: the picked item's record, the opened artifact's detail, and their refusals;
- the **legend fold**: codes seen per column per frame, and their resolved values.

Everything a component shows is a **projection** — typed, observable, read-only:

| projection | carries | who reads it |
|---|---|---|
| `meta` | `/v1/meta` as this principal sees it: columns, operands, layers, views | every component, once |
| `status` | the display state — `idle · loading · retrying · shown · empty · refused` — `sessionWarm`, the last refusal, and **`stale`**: whether the change signal (`x-tessera-stale`) has fired against the presented view | every panel, to decide whether it may show a number and whether to offer refresh |
| `view` | the presented frame's `visible` and `matched` as `Masked`s, `served` as a `Count`, provisional marks as a `Count` with `exact: false`, the drawn region, the depth choice | `<tessera-view-info>`, `<tessera-map>` |
| `marks` | the draw list — positions, ids, scalar columns, per-tile provenance — **with its `Count` beside the arrays** | `<tessera-map>`, `TesseraLayer`, `<tessera-legend>` |
| `selection` | the picked item's record (named fields, external id) or its refusal; the opened artifact or its refusal | `<tessera-item-card>`, `<tessera-artifact-card>` |
| `filters` | operands from meta, the draft, per-column value lists and their refusals, the composed expression as sent | `<tessera-filter-panel>` |
| `artifacts` | the layer selected, the served set, the tree built from `parentId` (`parent_id` on the wire), the channel's status and refusal | `<tessera-layer-picker>`, `<tessera-map>`, `<tessera-artifact-card>` |
| `legend` | per column: the resolved values marks on screen carry, and the codes seen | `<tessera-legend>` |
| `replica` | bytes, points, bands held; the last plan's held-vs-fetched split; prefetch spend | `<tessera-view-info>`'s drawer; the demo's instruments |

Gestures enter as **verbs**, the only write surface a component has: `setView(bbox)`,
`pick(id) / clear()`, `openArtifact(id)`, `setLayer(name | null)`, `setFilters(expr)`,
`setColourBy(column)`, `setBudget(marks)`, `refresh()`. A component that needs something not
on this list needs a store change, not a `fetch`.

**Two count types, because two kinds of number cross the wire.** `clients/ts/README.md`
records that `{shown, total}` is honoured in the panel and not enforced by the type system.
The store enforces it with two types and one helper that knows the difference:

- **`Count = {shown: bigint; total: bigint; exact: boolean}`** for a **served sample** of a
  set — `served`, the draw list, provisional marks. The helper renders both figures or, where
  `exact` is false or `status.stale` is set, neither.
- **`Masked = {value: bigint; exact: boolean}`** for a **number-channel scalar** with no sample
  behind it — `visible`, `matched`, and an artifact's masked count. The helper renders one
  figure or none, and its signature does not accept a `Masked` where a `Count` is wanted or the
  reverse. An artifact's count therefore never renders as "N of N", which would assert a total
  the wire deliberately withholds.

`marks` carries its `Count` so that `marks.length` is never the only number a component can
reach; the acceptance assertion in §10 — only `shown` renders a count — then has a type behind
it for every projection.

**Staleness and refresh.** Client-interaction §4 makes the refresh affordance mandatory and
§6.1 forbids a number against a view older than the last signal. `status.stale` is set from
the header the server already sends; the helper treats it as it treats non-exact; and
`<tessera-view-info>` owns the affordance — a refresh control present whenever `stale` is, wired
to `refresh()`, which also evicts a held artifact layer (§6).

**What the store drops on identity-key change** is everything per-principal: the replica, the
held artifact set, the selection, the filter value lists and legend resolutions (per-column
under C11), and — by a `reset` event — the vis-side colour accumulators below.

**What stays vis-side** is client-architecture §1's list: the deck.gl binding, the GPU slab,
rAF coalescing, and the **colour encoding accumulators** (palette ranks by observed frequency,
sticky numeric domains). Those live in one encoding object that `<tessera-map>`, `TesseraLayer`
and `<tessera-legend>` share, fed by the `legend` projection's codes and reset by the store's
event — so the store owns what was counted and the vis side owns what colour it gets, which is
§1's line drawn where it was drawn.

## 5. The components

Every component takes its store by **context** from an enclosing `<tessera-store>` or by a
`.store` property; the second is for integrators who construct the store in their own code, the
first for markup. A component with **neither** — rendered outside any provider, or connected
before the provider has authorised — renders a **detached** state: no numbers, and neither
"empty" nor "refused", both of which are answers. It subscribes to the context so a store
arriving later lands.

**`<tessera-store>`** — the provider. Attributes for the viewer and session URLs and the view;
the token supplier arrives as a property or as an `authorise` callback, never as an attribute,
so it is never in markup. Renders nothing. Constructs the store, owns its lifetime, tears it
down on disconnect.

**`TesseraLayer`** — client-interaction §10's drop-in deck.gl layer: a `CompositeLayer` over the
`marks` and `artifacts` projections and the shared encoding object, for an integrator who owns a
`Deck` already (a basemap, their own layers). It is what `buildViewportLayers` and the slab are
today, given a class boundary; `<tessera-map>` is a `Deck` around it. Client-architecture D3's
adapter for tile-shaped engines sits beneath the store, over `fetch`/`read`, and is unchanged.

**`<tessera-map>`** — the viewer. A `Deck` with an `OrthographicView` and a `TesseraLayer`;
hover and pick. Properties: `colourBy`, `budget`, `layer`, `palette`; reflects the view (`bbox`,
`zoom`) and emits `viewchange`, `pick` and `hover` events carrying `tessera_id`s and nothing
else. Draws the display states on the canvas — empty, refused and retrying are drawn, because a
blank map under a refusal reads as an empty corpus (client-interaction §4). Per instance: its
`Deck`, its slab, its trace phases and its probe (exposed as a property; the demo publishes the
first map's onto `window` for the smoke scripts). `disconnectedCallback` finalises the `Deck`,
because a browser caps live WebGL contexts and a framework remount would otherwise leak one per
mount. `:host { display: block }` with a default height token (§7) — a custom element is inline
and heightless by default, and deck.gl sizes its canvas from its parent, so without this a bare
`<tessera-map>` in plain HTML is 0 × 0 and silent.

**`<tessera-view-info>`** — `visible`, `matched`, `served` for the drawn region, in that order,
because their relationship is the content: a filter narrows the answer and never the grant
(client-interaction P2, §4). Provisional marks as a count of marks, never folded into a figure.
The refresh affordance (§4). A drawer for the `replica` projection.

**`<tessera-item-card>`** — the picked item's record, fields in declaration order, keyed by name
and never by position (`/v1/items` omits absent fields, so position lies), with the
miss-versus-fault distinction the current panel makes. A refusal renders as a refusal. A slot
per field for the integrator's own rendering — the common case is a title that should be a
link into their application.

**`<tessera-filter-panel>`** — controls from `meta.filterOperands` and nothing else: a column
absent from meta is absent from the panel. Category value lists come from the store's
enumeration; a refused enumeration renders as a refusal with a free-text entry, not as an empty
list.

**`<tessera-layer-picker>`** — which annotation layer the map draws, from `meta.layers`, one at
a time (§6). Never a count of a layer's artifacts: the wire never carries one.

**`<tessera-artifact-card>`** — the opened artifact: layer, key, masked count as a `Masked`,
its content, its subtree from the held set. Steady during a pan by construction (§6).

**`<tessera-legend>`** — the colour encoding: values the marks on screen carry, resolved per
column and never per vocabulary (visibility is per column, contracts §3.2); numeric domains
sticky within a session and cleared with it.

Two the direction did not name and this design declines:

- **A search box.** Text `match` is a filter operand and lives in the filter panel; a separate
  component would be a second route to the same verb.
- **A principal switcher.** The demo needs one; an application's user is one principal. It is
  an instrument (§9).

## 6. Artifacts in the components

Artifacts are on the wire and the viewer draws them through a scaffold: rings placed from a
`clusters.json` sidecar the publishing script writes, from a time when no geometry crossed the
wire. Geometry now does — `Artifact.centroid`, `box` and `hull`, derived per principal from the
members they can see — and the sidecar is retired at §10 step 3.

What the map draws per layer, from the wire alone: a hull or box where the artifact carries one
(each is per-artifact `null` where the layer declares none), a centroid mark otherwise, the
first content string as a label where the layer's first supplied kind is text, and parent links
from `parentId` where both ends are in the response. The card shows §5's list.

The obligations, each honoured today in a comment and here in a type or a construction:

- **The count is over the whole membership and does not move with the viewport.** A
  per-viewport count could be differenced across two boxes. The card renders the held set's
  `Masked`, which is what the wire said; nothing is recounted from marks on screen.
- **Absence has no reason.** Below criterion, unreachable layer, suppressed, never published:
  one state, absent. No "hidden" rendering, no greyed entry, no placeholder; an artifact with
  no served parent is a root of the tree.
- **Geometry is per principal and never cached across an identity key.** It lives in the
  store's held set, which drops with everything else.
- **One layer at a time, named in the request.** Omitting the selector answers for every layer
  this principal reaches and pays for all of them; naming one never widens what comes back.
- **The channel asks for itself.** The replica elides held tiles and elided tiles carry no
  artifacts, so artifacts are never read off the point path — the store issues the `k = 0`
  request per settled view as `artifacts.ts` does today, moved across the boundary.

**Fetch model.** artifact-system §6 gives two — small layers fetched whole once and held, large
layers fetched as the budgeted coarse cut and refined on zoom — and records ⊘ that nothing in
`/v1/meta` says which a layer suits. The store picks by observation: it asks the whole map at
the layer's default cut on selection and holds the answer if it is under a configured size,
otherwise fetches per view. **The hold is bounded by the same staleness budget as tiles**: the
identity key excludes the overlay, so a suppression rotates the *content key*, which arrives on
point responses; the store evicts a held layer when the content key it was fetched under
rotates, and on `refresh()`. Without that rule a held layer would answer from held state
indefinitely, which client-interaction §4 forbids. ⊘ A wide response over a large flat layer is
unbounded today (artifact-system §6); the configured size is the only bound this design puts
between a component and one, and streamed continuation stays owned by the client epic. §11 D8
asks for the meta hint; the design does not depend on it.

**Specified and not built, and unchanged here:** membership by predicate, runtime artifacts and
the edit verb, search over artifacts, polygon shapes beyond bbox (artifact-system §10). The
components gain a card field when those land and nothing else.

## 7. Restyleable

Three levels, each cheaper than the next; the first two are the contract:

1. **Tokens.** Every colour, font, spacing and radius is a CSS custom property under
   `--tessera-*` with a neutral default that follows `color-scheme`, so light and dark come from
   the host page unconfigured. One token is a size: `--tessera-map-height`, the map's default
   height when its container gives it none. The map's **data palette** is a `palette` property,
   not a token — it encodes data, and a page's brand colours are the wrong thing to encode data
   in.
2. **Parts.** Every structural element carries a `part` name — `count`, `field`, `value`,
   `refusal`, `refresh` — so `::part()` reaches it from outside the shadow root. The names are
   the contract; the markup is not.
3. **Source.** The components ship as readable source as well as built ESM, on
   client-interaction §12's *editable rather than merely themable*. Forking a card is the
   supported answer to "completely different".

Shadow DOM is the default and is what makes the parts contract mean anything: under light DOM
every host stylesheet reaches every internal element and the integrator's `td` rule restyles
the item card by accident. The cost is the usual one — fonts inherit, a host reset does not
reach inside, so the components carry their own minimal reset. §11 D2 records the alternative.

The components draw **no chrome they are not asked for**: no panel frames, no columns, no
floating layout. The demo's two-column overlay is the demo's stylesheet. An integrator places a
card in their own sidebar and it fills it.

## 8. Notebooks

**One core, Python embeds it** (client-interaction §10). `tessera-widget` is an anywidget
package whose `_esm` is the components bundle plus a `render({model, el, signal})` that mounts
a `<tessera-store>` and a `<tessera-map>` into `el`, and whose `_css` is empty because the
components carry their own. anywidget reaches Jupyter, Marimo (`mo.ui.anywidget`, two-way), VS
Code and Colab from one package, so no notebook-specific code exists beyond the mount.

What crosses the kernel boundary, as synced traitlets, is **control and selection, never
data**: URLs and view name down; `bbox`, `layer`, `colour_by` and `filters` — the composed
`FilterExpr`, not the draft, applied without the panel's debounce — both ways; `selected` (one
`tessera_id`) and `selected_artifact` up. `widget.selected` in the next cell is the picked
item; setting `widget.filters` from Python redraws; and the pan path stays browser → Tessera
with the kernel never on it (client-interaction §7's notebook fork).

**Token custody** is client-interaction §15's open item — *the widget must never serialise a
token into saved output* — and the answer is that **the token is never model state**:

- The kernel holds the session credential and performs `POST /session/authorise` itself. The
  credential never enters the model.
- The token crosses as a **custom message**, not a traitlet: the JS side sends `ready` from
  `initialize({model})` (once per model, so two views of one widget share it and two widgets
  do not), the kernel answers with the token, and the JS holds it keyed on the model. A view
  rendered after a page reload finds no token and sends `ready` again; expiry sends
  `reauthorise`. Custom messages are not model state, so no route that serialises widget state
  — the frontend's opt-in "save widget state", `nbconvert --execute`, papermill, a headless run
  where no frontend ever mounts — can carry a token into an `.ipynb`. A traitlet cleared after
  read was the first draft and is unsound on exactly those routes: the clear is issued by JS,
  and kernel-side state is what those routes save.
- The exposure that remains is a token in browser memory for `token_max_lifetime`, which is
  browser-direct's exposure everywhere.
- For a remote JupyterHub whose browser cannot reach Tessera, the **`jupyter-server-proxy`
  arm**: the widget's base URL is the proxied path, the proxy holds the credential and
  authorises, and nothing crosses to the browser. ⊘ Documented, not built, in the first cut;
  the `reauthorise` message above is likewise not built until §10 step 5.

The Python SDK (#47) does not exist and the widget does not wait for it: authorise is one HTTP
call, done with `urllib`, moved into the SDK when there is one. The widget never computes a
masked quantity, gates a label or decides what is drawn — none of that is in Python.

## 9. The demo, and what stays an instrument

`run_demo.sh` keeps its invocation, its scales and its port table. `@tessera/viewer` becomes a
page composed from the components plus what is an instrument of Tessera rather than a use of it,
deliberately **not** in the library: the **dataset picker** and **principal presets**; the
**trace bar** (`?trace=1`) and **stage-timing readout** (`x-tessera-stage-ns`); the
**request-failure list** and the replica drawer's spend figures. Each reads a projection like
any component, so keeping them in the demo costs nothing. The smoke scripts keep running against
the demo page, reading the probe from `document.querySelector('tessera-map')`.

Beside the demo, **examples that are checks**: a plain-HTML page and a React page, typechecked
through the workspace by `check-clients.sh` as written (new workspaces with a `typecheck` script
need no script change); Vue and Svelte are documented (§3) and not checked in the first cut. The
notebook example is exercised by the widget bundle: the built `_esm` is **committed** into the
Python package so it is `pip`-installable without Node, and the gate rebuilds it and fails on a
diff — which keeps `check-clients.sh` npm-only and makes the check mean "the components still
embed".

## 10. What lands, and what proves it

The order, as client-architecture §6 gives its own — demo working at every step, smoke scripts
as the net, one worktree per step:

0. **The presented frame moves.** `assemble.ts`'s composition half and `binding.ts`'s
   frame-applying store writes land in `@tessera/client` as the store's frame; `ViewState`
   becomes the driver's type. This finishes client-architecture §6 step 3. Not separable from
   step 1: the legend fold walks the assembled frame, the artifact channel reads its depth.
1. **The store lands** with the projections, `Count` and `Masked`, `status.stale`, and the token
   supplier. The artifact channel, item and artifact detail, category resolution, filter
   composition and the legend fold leave `main.ts`. The viewer consumes the store; nothing
   visible changes; `smoke.mjs` and `smoke-artifacts.mjs` green.
2. **`@tessera/components`** with `<tessera-store>`, `TesseraLayer`, `<tessera-map>`,
   `<tessera-view-info>`, `<tessera-item-card>`, `<tessera-filter-panel>`. The viewer is
   rebuilt as a composition plus its instruments. Smoke green.
3. **Artifacts**: `<tessera-layer-picker>`, wire geometry drawn, the sidecar retired,
   `<tessera-artifact-card>`, `<tessera-legend>`. `smoke-artifacts.mjs` asserts hull and label
   render under two principals.
4. **The styling contract and the two web examples**, in the gate.
5. **`tessera-widget`** and the notebook example; run in Jupyter and Marimo by hand once, the
   bundle diff in the gate thereafter.

**Acceptance** is the conformance harness client-interaction §10 asked for and has had no
subject: the smoke scripts become DOM-level assertions through `::part()` — only `shown` renders
a count; both figures render or neither; a refusal renders as one; no count renders against a
stale view and a refresh control is present when one is; an artifact's count does not move
across a pan; a switch of principal empties every card. They run in the gate against the demo
page: the first component of the kit that binds something.

## 11. Decisions for the owner

- **D1 — custom elements, authored with Lit, React through `@lit/react`.** Alternatives §3.
  Recommended as written.
- **D2 — shadow DOM with tokens and parts.** The alternative is light DOM with a class
  contract: simpler to restyle from a host stylesheet, impossible to keep from being restyled by
  accident. Recommended: shadow DOM.
- **D3 — three packages, and the store in `@tessera/client`.** This amends client-architecture
  §1's two-package split (§2): the display package becomes `@tessera/components`, and
  `@tessera/viewer` is its consumer, name kept (D4). The store could instead live in the
  components package and leave the client stateless, which would make a stranger's conformant
  frontend depend on the component package for its cache and display states — the wrong side
  of the line for client-interaction §10's first-class distributable. Recommended: three
  packages, store in client.
- **D4 — the notebook's first-cut posture.** (a) Browser-direct with the token as a custom
  message, proxy arm documented; (b) proxy arm only. (a) is what a laptop needs, (b) what a
  JupyterHub needs, and (b) is a base-URL change over (a). Recommended: (a).
- **D5 — instruments stay in the demo.** Trace bar, stage timings, dataset and principal
  pickers, failure list. Publishing them makes the library a benchmark harness. Recommended:
  demo only.
- **D6 — where the Python package lives.** `clients/py/widget/` beside `clients/ts/`, with
  `reference/` staying the oracle and sharing no code with it; the built `_esm` committed
  (§9). Not waiting on #47.
- **D7 — the first-cut set**: §5's — store, `TesseraLayer`, map, view-info, item-card,
  filter-panel, layer-picker, artifact-card, legend. Two declined are recorded there.
- **D8 — a fetch-model hint in `/v1/meta`.** artifact-system §6's ⊘: a per-layer hint (whole or
  cut). Server-side, additive, and this design works without it. Asked for, not depended on.

## 12. Provenance

The owner's four-point direction of 2026-08-24; the one-worktree-per-step convention is the
delivery record's (owner direction 2026-08-15). The boundary rule, the driver and the migration
order are client-architecture §1, §3 and §6 (driver built, `6aa84f7`). The three-layer stack,
the projections rule and the anywidget ruling are client-interaction §10 and §12; the stale
mark and the refresh affordance are its §4 and §6.1; the token custody item is its §15, which
this document answers. The two artifact fetch models and the ⊘ meta hint are artifact-system
§6. The artifact obligations in §6 are the ones `viewer/src/artifacts.ts`, `state.ts` and
`panels/layers.ts` carry as comments today. anywidget's `render({model, el, signal, host})`,
`initialize`, custom messages and Marimo's two-way sync are from its current documentation.

On the roadmap: [#10] gains `@tessera/components`; [#46]'s reference viewer becomes §9's demo;
the widget precedes [#47] and does not depend on it. The roadmap paragraph is updated when this
promotes.

## Appendix R — review trail

- 2026-08-24: drafted (Provisional).
- 2026-08-24: reviewed across three lenses — disclosure, embeddability, corpus fit — twenty-seven
  findings, dispositioned in one pass into r2. The disclosure lens found the token traitlet
  unsound on the routes that serialise kernel-side state (now a custom message), the artifact
  count wrongly typed as a sample (now `Masked` beside `Count`), `marks.length` a bare served
  count (now carried with its `Count`), a held artifact layer that never saw the content key
  rotate (now evicted on rotation and on refresh), and `setFilters` missing its drop. The
  embeddability lens found `assemble.ts` and `binding.ts` coupling step 1 to step 2 (now step
  0), ranks and domains placed on both sides of the boundary (now one vis-side encoding object,
  store-reset), the map's singleton state, inline-and-heightless hosts, the lost context request,
  SSR and property-binding mechanics, two views of one widget model, peer dependencies, and the
  decorator target. The corpus-fit lens found the third package amending client-architecture §1
  unstated (now §2 and D3), the change signal and refresh affordance absent (now `status.stale`
  and `<tessera-view-info>`), "session" colliding with the wire's term (now the store), the
  drop-in deck.gl layer missing (now `TesseraLayer`), and the Status line contradicting D8.
  Nothing re-decided client-interaction §14's rejections; no decision-0013 breach found.
