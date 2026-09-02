# Tessera clients — an instrument, not a product

Five TypeScript packages, three example pages, a headless-browser smoke test and an acceptance
harness, built to answer one question: **does a running Tessera actually work?**

- `core/` — `@tesseradb/client`. The **headless store** is its main export: `createStore({viewerUrl,
  token | authorise})` hands a visualisation the projections to draw and the verbs to steer, over
  the driver, the replica, the presented frame, the artifact channel, the encoding accumulators,
  filter composition, the session artifact table and the region counter (design client-components
  §4, §5.11). The four viewer/session verbs, the framed-Arrow decoder and the coordinate arithmetic
  sit beneath it. No DOM: the frame scheduler and the clock are injected, so the whole store is
  testable in node.
- `deck/` — `@tesseradb/deck`. `TesseraLayer`, a deck.gl `CompositeLayer` over the store's `marks`,
  `tiles` and `artifacts` — the GPU slab, rank-to-colour, the density wash, the artifact markers,
  the selection highlight — for a host that owns a `Deck` already. It never fetches; deck.gl and
  luma.gl are peers.
- `components/` — `@tesseradb/components`. Lit custom elements: `<tessera-explorer>`, `<tessera-map>`,
  `<tessera-status>`, `<tessera-count>`, `<tessera-item-card>`, `<tessera-filter>`,
  `<tessera-filter-panel>`, `<tessera-selection>`, `<tessera-store>` — a subpath entry each, the
  eight display states rendered through `part="state"`, tokens `--tessera-*`, parts, slots and
  events per design §5. `npm run build -w @tesseradb/components` writes the single-file bundle with
  the decode worker inlined and its SRI hash beside it (`dist/`).
- `react/` — `@tesseradb/react`. `useTesseraStore(options)` and `useProjection(store, name)` over
  `useSyncExternalStore`; the store is built in an effect paired with its `dispose`, so
  StrictMode's double mount leaks no driver. `@tesseradb/react/components` wraps every element
  through `@lit/react` with typed props and events, behind optional peers on
  `@tesseradb/components` and `@lit/react`, so a hooks-only install pulls neither Lit nor deck.gl.
- `examples/` — the three pages design §9 step 4 asks for, each a workspace the gate typechecks:
  `plain-html` (no build step; the bundle with its integrity hash, and the app server that holds
  the session credential), `react-explorer` (React 19 through the wrappers, one slot replaced by
  a host component) and `canvas-store` (the store under a hand-rolled camera on a 2D canvas, with
  none of our rendering — the check that C2 works).
- `viewer/` — the demo, `@tesseradb/viewer`: `<tessera-explorer layout="overlay">` plus the
  instruments — dataset and principal pickers (where the session credential stays), the layer and
  colour controls, the depth and request readouts, the trace bar.
- `harness/` — the acceptance harness (design §9): the components' claims, checked through
  shadow-piercing locators against the demo page and against the plain-HTML example page
  (`--url`), and the §5.10 measurements printed.
- `spike/` — the deck.gl tile-convention spike, kept as a regression guard.

**Vite 8 and the decorators.** The elements use standard (TC39 stage-3) decorators with `accessor`,
as design §5.9 decides; Vite 8's oxc transform lowers only the legacy form, so
`components/vite-plugin-decorators.ts` runs esbuild over the component sources first. Both the dev
server and the bundle use it; a host bundling the unbundled distribution needs the same or a
transform of its own.

The npm scope is `@tesseradb/*`, matching the Python package (design §8); it was `@tessera/*` until
the store landed.

Design: [`docs/archive/plans/2026-08-01-mvp-client-and-deckgl-viewer-design.md`](../../docs/archive/plans/2026-08-01-mvp-client-and-deckgl-viewer-design.md).
It is the first slice of
[`client-interaction.md`](../../docs/design/client-interaction.md),
which owns the client architecture proper.

---

## ⚠ Development only

Two things here exist **only** to let a browser talk to a local bundle, and neither is a pattern
to copy:

- **`serve.dev_cors_origins`** in `tessera.toml` lets an enumerated browser origin call the viewer
  **and session** planes. It is off unless typed, has no wildcard and no environment variable, and
  the server logs a warning at `warn` when it is on. Opening the session plane to a browser is the
  part that makes it development-only.
- **`VITE_TESSERA_SESSION_CREDENTIAL`** puts the deployment's *session credential* into the browser
  bundle, because `POST /session/authorise` is gated by it and the viewer re-authorises whenever
  you switch principal.

**A production browser origin list does exist, and it is a different key.**
`serve.cors_origins` ([decision 0102](../../docs/decisions/0102-the-viewer-plane-gains-an-enumerated-cors-origin-list.md))
is enumerated in the same way — no wildcard, none by default — and covers the **viewer plane
only**, so a page it names may present a *token* and can no more reach `/session/authorise` than
any other origin. It is silent at startup rather than warned about, because a deployment that
types it has said something deliberate. That is the key a drop-in `<tessera-explorer>` on a
customer's page runs on; `dev_cors_origins` is not, and the two are not interchangeable.

The token still comes from somewhere the credential is held. The documented integration topology
is **T2 with verified assertions** — credential construction at the integrator's app server, where
the authority is (client-interaction §7) — and `cors_origins` does not revise that: it decides
which page may *present* the token that server minted, not who may mint one.

## Embedding the elements

The elements are custom elements, so every framework takes them; what differs is how each sets an
object-valued property and whether it needs telling the tag is not its own.

**Plain HTML** — `examples/plain-html`: the self-contained bundle with its integrity hash and
`<tessera-explorer viewer-url token>`; the token from the page's own server, which holds the
session credential (`examples/plain-html/README.md` says what that server is under the
passthrough plugin, and what the production topology is until D10 is ruled).

**React** — `examples/react-explorer`: the wrappers in `@tesseradb/react/components` set object
props as properties and type the events (`onPick`, `onSelectChange`, …); the hooks in
`@tesseradb/react` read the store. React 19 sets properties on custom elements natively, so the
raw tags work there too; the wrappers are for React 18 and for the typing.

**Vue** — not checked in the gate; two things are needed. The compiler must be told the tags are
custom elements, or it warns that `tessera-map` failed to resolve as a component:

```ts
// vite.config.ts
vue({template: {compilerOptions: {isCustomElement: (tag) => tag.startsWith('tessera-')}}})
```

and object values go through **property** bindings, since an attribute can only carry a string:
`<tessera-status .store="store" />`, `<tessera-count .count="served" />`, `<tessera-explorer
.authorise="getToken" />`. Events are ordinary: `@tessera-pick="onPick"`. Import
`@tesseradb/components` from a client-only path — Lit and deck.gl touch `window` at import, so a
server-rendered app imports it dynamically in `onMounted` or a `<ClientOnly>` boundary.

**Svelte** — not checked in the gate. Svelte sets a property when the element has one of that
name and an attribute otherwise, so object values need no marking in most cases; where the
element is upgraded after Svelte set the attribute (the module still loading), bind explicitly:
`<tessera-count bind:this={el} />` then `el.count = served`, or use Svelte's `prop:` directive
(`<tessera-status prop:store={store} />`). Events: `on:tessera-pick={onPick}`. The same
client-only import rule applies under SvelteKit — import the package in `onMount`, not at module
scope.

**Every framework**: define before render. The package's root entry defines every element on
import; a panel rendered before the import resolves is an unknown element until it upgrades,
which is harmless but reads as a blank. The context root is attached once on import, so a panel
above the explorer in the DOM still finds its store.

## Prerequisites

- Node 22 (`node -v`)
- A release binary: `cargo build --release`
- A built bundle. `data/bench-fixtures/{2m4,1e8,1e9}` are the development ones.

## Running it

Write a `tessera.toml` in this directory (untracked). **Every path in it resolves against the file
itself**, and `tessera serve` finds it by walking up from wherever you run it — so the paths below
are relative to `clients/ts/`. What the run produces goes under `tessera-demo/` at the checkout
root, which is gitignored: nothing a run writes belongs in the source tree, for the reason
`run_demo.sh` gives at its head.

```toml
[bundle]
path = "../../data/bench-fixtures/2m4"
cache = "../../tessera-demo/manual/cache"
wal = "../../tessera-demo/manual/wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
min_visible_members = 10
token_max_lifetime = 3600

[serve]
viewer = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
max_k = 5000
session_credential_env = "TESSERA_SESSION_CRED"
operator_credential_env = "TESSERA_OPERATOR_CRED"
dev_cors_origins = ["http://localhost:5173"]
```

Then, with the viewer's own configuration in the environment — Vite exposes `VITE_`-prefixed
process variables to `import.meta.env` just as it does the ones in a file, and an exported variable
leaves nothing behind under `clients/` for the next run to read:

```bash
cargo build --release
mkdir -p tessera-demo/manual
export TESSERA_SESSION_CRED=dev-session-credential
export TESSERA_OPERATOR_CRED=dev-operator-credential
./target/release/tessera serve --deployment clients/ts/tessera.toml &

export VITE_TESSERA_VIEWER_URL=http://127.0.0.1:37585
export VITE_TESSERA_SESSION_URL=http://127.0.0.1:49303
export VITE_TESSERA_SESSION_CREDENTIAL=dev-session-credential

cd clients/ts && npm install
node scripts/measure-principals.mjs --terms 0..200 --out ../../tessera-demo/presets/manual.json
npm run dev -w @tesseradb/viewer                       # http://localhost:5173
```

One server needs no dataset document: with no `?datasets=` in the URL the viewer serves the single
entry those three variables name. `run_demo.sh` is the route that measures several and offers a
picker.

The Vite port is `strictPort`: the origin is enumerated in `dev_cors_origins`, so a silent
fallback to another port would produce a CORS failure that reads as a broken server. `VITE_PORT`
moves it deliberately, and `run_demo.sh` writes whichever port it is given into the deployments it
generates.

## Regenerating the two generated artifacts

**`viewer/presets.json` — per fixture.** The term dictionary differs between bundles, so presets
measured against `2m4` are meaningless against `1e8`. Each preset's `visible` is measured with a
`zoom = 0` full-extent call, whose `visible` *is* that principal's visible-set size.

```bash
TESSERA_SESSION_CRED=… node scripts/measure-principals.mjs --terms 0..200
```

**`core/test/fixtures/*` — per wire change.** A decoder test passing against a stale golden is
worse than no test.

```bash
TESSERA_SESSION_CRED=… node scripts/capture-golden.mjs --terms 0
```

Capture against the **wide** fixture (`data/scaled/attrs/schema-wide.toml`, nineteen columns and
twelve Arrow types), not against a demo bundle: `decode.test.ts` walks the captured `meta.json` and
checks every declared column decodes at its declared type, so a six-column capture keeps the test
passing while quietly dropping two thirds of the types it covers. The other two are captured with
`layers: []` so they keep pinning the no-artifacts-frame case whatever the server holds.

**`viewport-artifacts.bin` and `viewport-membership.bin` are the exception, and may be captured
against any corpus that carries a layer** — `--artifacts-only` recaptures the pair alone, leaving
the wide goldens as they are. The first is the annotation channel's own shape, `k = 0` and no
points frame; the worked decodes in `reference/examples` and `wire-example` pin its bytes, so it
is captured as `--terms 0` every time. **Both are r44 captures and are due a recapture** — they
carry `hull_x`/`hull_y`, the names the shape columns had before `polygon-membership.md` §7.1 made
them `shape_x`/`shape_y` three lists deep (contracts §3.2 r45), and the decoder refuses those names
outright rather than reading a hull body as shapeless. Until they are recaptured the tests strip
the old columns (`core/test/old-shape-columns.ts`) to keep the row-set and membership claims
against real bytes, and `viewport-artifacts-pre-r40.bin` beside them is a real body from before
the rings change, kept deliberately and never recaptured: both are what the refusal is tested
against. The second names the layer with points, so the body carries
the artifacts frame **and** a points frame with the per-point membership column (D12); the wide
fixture's breadth, a property of the declared columns, has nothing to contribute to either. What
it must carry is several artifacts with genuinely different geometry — a layer declaring
`centroid`, `box` and `hull` over clusters that occupy different parts of the map — and at least
one point the column names a member, so it is captured as a principal broad enough to be served
several (the demo's *medium* preset). Clusters cut from runs of consecutive ids do **not** qualify on a
synthetic corpus whose positions are a modular sequence — every such run samples the whole extent,
so every centroid lands in the middle and a decoder reading row 0 for every row would pass.

**Every golden also predates the tiles frame's `highlighted` column** (`highlight-and-hierarchy.md`
§2, fifth after `served` and always present), so `liftTilesHighlighted` in the same module gives
each one that column with each tile's `matched` in it — which is exactly what the server serves for
a request carrying no `highlight`, and these captures carried none. `liftGolden` is both lifts
together. Both are rewrites of stale recordings, **in test code only**: the decoder keeps no shim
(decision 0048) and refuses a body without the column. They go when the goldens are recaptured
against a server serving the highlight columns.

## Annotation layers, and the number beside a cluster

The viewer draws whatever annotation layers `/v1/meta` says this principal reaches. There are none
until something publishes one:

```bash
TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node scripts/publish-clusters.mjs \
  --presets ../../tessera-demo/presets/2m4.json --clusters 24 --layer clusters/kmeans-v1
# and again, for the same clusters under an existence criterion:
… --layer clusters/kmeans-v1-min1000 --min-visible 1000
```

That registers a layer and publishes a k-means clustering of the corpus's own points into it. The
clustering is deliberately unremarkable — **what it exists to show is the masking**. Pick the layer
in the left column, then switch principal and watch the *Clusters in view* panel:

| principal | visible items | clusters served | `c-0013` |
|---|---|---|---|
| narrow — term 14 | 243 | 5 of 24 | 4 |
| sparse — 1.9% | 35,138 | 17 of 24 | 485 |
| medium — 19% | 360,239 | 24 of 24 | 1,962 |
| heavy — 50% | 929,811 | 24 of 24 | 4,138 |
| full — top 4096 terms | 1,856,276 | 24 of 24 | 8,380 |

*(2m4, `clusters/kmeans-v2`, measured 2026-08-16. The cluster holds 11,008 members; no principal is
told that, and the broadest one here sees 8,380 of them.)*

Three things about that panel are worth knowing before reading a number off it:

- **The count does not change as you pan.** It is over the whole cluster, not the viewport — a
  per-viewport count would let two boxes be differenced for the members between them. Only *whether*
  a cluster appears depends on where you are looking.
- **A cluster that is absent gives no reason.** Below its layer's criterion, in a layer this
  principal cannot reach, suppressed, never published — one answer, indistinguishable. Under
  `--min-visible 1000` the same clustering serves 0, 0, 8, 20 and 24 clusters to the five principals
  above.
- **Every shape and colour on the map is the wire's.** A cluster's outline is the `hull` or `box`
  the service derived for *this* principal from the members they can see; its name sits at the
  derived `centroid`; a point wears a cluster's colour only because the response's membership
  column named it a member (decision 0099, exact only). Nothing is placed from a publisher-side
  file, and no shape is contoured from the held marks — the sidecar the demo once used is gone.

The annotation channel makes its **own** request (`k = 0`, the layers that are on) rather than
reading the artifacts off the point path's responses: the replica elides tiles it already holds,
and an elided tile carries no artifacts, so clusters would thin out as the cache warmed. The
point path names the same layers, which is what puts the membership column on each band.

### Labels attached to those clusters, and who is served which description

```bash
TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node scripts/publish-clusters.mjs \
  --presets ../../tessera-demo/presets/stage3.json --clusters 24 --layer centroids/kmeans-2026-08 \
  --labels topics/ctfidf-2026-08 --label-term 46
TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node scripts/check-labels.mjs \
  --presets ../../tessera-demo/presets/stage3.json
```

The first publishes a second layer of labels **attached** to those clusters, each carrying two
ranked descriptions: one generated from the cluster's whole membership, one from the part of it a
`--label-term` principal can see. The second reads them back and **exits non-zero** if the answers
stop depending on the principal — a table alone prints just as happily when they do.

What it demonstrates, measured on 2m4: a principal seeing 15,188 items is served the description,
and one seeing 181,900 is served **none**, because containment is not a coverage fraction — what
decides is *which* documents the description was generated from. And suppressing a **cluster** stops
its label serving on the identifier route as well as in the viewport, which is the route that
traverses no edge and would otherwise go on describing what was just hidden.

### The write cycle: what a deletion does to a description

```bash
TESSERA_SESSION_CRED=… TESSERA_OPERATOR_CRED=… node scripts/write-cycle-demo.mjs \
  --presets ../../tessera-demo/presets/stage3.json            # add --dry-run to see the plan first
```

The two above are read-only about the corpus. This one changes it: it publishes a small cluster and
one label over six documents **it picks itself**, deletes one of the three the label was written
from, and runs a fold. The label goes at the ack — containment is evaluated per request, so nothing
had to be stored to withdraw it — and the assertion is that it is **still gone after the fold**,
with the cluster one member lighter. An earlier draft of the design had the fold re-base generating
sets into row space, which brought the withheld label back, served on a set that no longer named
what its text was derived from.

It publishes its own pair rather than driving the labels above **because it cannot see a generating
set** — no client can, which is the design working. Driving somebody else's labels would mean
deleting documents at random until one landed in a set; publishing its own means exactly one
document dies.

It leaves two things behind: a deleted document (a delete is not a suppress) and a new prefix, the
fold having reclaimed the one it replaced — so the bundle needs free disc of about its own size. The
fold also writes `reports/fold-<prefix>.json` in the bundle root: which artifact lost how many
members, out of how many it was published with, and which description lost a source. That is the
notice the publisher is owed, and it is written before anything retires.

Run it with the viewer open over the cluster: the label disappears at step 2, and step 3 changes
nothing a viewer can see, which is what a fold is supposed to look like from outside.

## Filter, highlight, and a layer that draws nothing

**A filter narrows the map; a highlight keeps every point and lights the matched ones.** They are
two fields of one `POST /v1/viewport` — `filters` and `highlight`, in the same grammar — and the
client holds them as one draft: a control carries a `verb`, and moving a clause between the two
positions is that field changing and nothing else, so a predicate is never re-entered. The chips on
`<tessera-filter-panel>` carry the word and move the clause when it is clicked; the words in the
interface are **filter**, **highlight** and **matched** throughout.

Under a highlight the marks that satisfy it draw lit and the rest at a fifth of their alpha — the
map does not move and nothing is removed, which is the whole difference — and the density wash
switches to the per-tile `highlighted` count, which is what shows the members the mark budget did
not draw. `<tessera-status>` gains a fourth cell, *the highlight matched N*, and it is drawn only
where a highlight was asked: the wire's `highlighted` equals `matched` where none was, so a cell
drawn always would repeat a number.

**A `member_of` clause names one artifact of one layer** and asks for its membership. It is what
*Filter to this*, *Highlight this* and *Outside this* on `<tessera-artifact-card>` send, and what a
node of `<tessera-hierarchy>` sends; it replaces the `region`-by-published-artifact spelling for
that use, the drawn-region spelling staying for a region drawn by hand.

**A layer declaring no computed content is a filter layer, and a filter layer is still a layer.**
It is in `/v1/meta`'s roster and in `<tessera-layer-picker>`, in its own group with no draw toggle;
it is never named in a viewport request's `layers`, so nothing draws it, nothing labels it and it
is absent from *In view*. It is reached through `<tessera-hierarchy>` and applied as a clause. A
MeSH descriptor's members are spread over the whole layout, which is what the rule is for: its hull
would be the map's outline.

**`<tessera-hierarchy>` does not depend on the viewport.** It walks `POST /v1/artifacts/browse` —
the roots whatever the zoom, children on expansion, *More…* to page, a search box, and, on a `dag`
layer, a node under each of its served parents saying *also under* the others. A click is a
highlight; *filter* is beside it, and *fit* beside that where the layer draws something. Under a
filter it sends the map's own `filters` and shows each row's matched count beside its masked one.

`viewer/.highlight-boards.html` is the harness the element screenshots are taken through: the built
components against a hand-made store, at `/.highlight-boards.html` on the dev server. It needs no
service, which is the point — the states it draws are ones a live corpus reaches rarely.

## Testing

```bash
npm test                       # spike, core, deck, components: every unit suite
cd core && TESSERA_LIVE=1 TESSERA_SESSION_CRED=… npx vitest run test/client.live.test.ts
cd viewer && node smoke.mjs             # drives the page in headless chromium
cd viewer && node smoke-artifacts.mjs   # the same clustering under every principal
cd viewer && node ../harness/harness.mjs  # the §9 claims, through the parts; the measurements
cd harness && node harness.mjs --url http://localhost:5180   # the same claims against examples/plain-html
```

The smoke scripts and the harness read the page **through the components' parts** —
`tessera-status [part="count"]`, `[part="state"]`, `[part="refresh"]` — with Playwright's
shadow-piercing locators, never an id the shadow DOM hides; the mark count and the view's figures
come from the map's probe (`window.__tesseraProbe`, the first map's, published by the demo; on a
page that publishes none the harness reads the explorer's map's own).

`smoke.mjs` reports what the page actually did — requests and their statuses, the counts each
principal reported, whether marks accumulate on zoom, lit canvas pixels, console errors — and
writes a screenshot. It is **not** the assertion. The owner looking at the map is; the smoke test
exists so "it builds" can be upgraded to "it ran" without a human in the loop.

`smoke-artifacts.mjs` does the same for the annotation layer, and writes the pair of screenshots
worth putting side by side: one clustering, two principals, different counts on the same clusters
and some of them absent.

Both wait for the mark count to stop moving rather than for a fixed interval — a broad principal on
a large bundle streams bands for tens of seconds, and sampling mid-load reads a still-climbing count
as though the control under test had changed it. `smoke.mjs` turns look-ahead off for its colour
section for the same reason: the anticipation ring issues requests whenever the view is still, which
is exactly when a colour switch is measured.

**Clicking on the canvas is not exercised headless.** deck.gl's `onClick` does not fire under
headless chromium, so neither canvas drill-down — a mark's record or a cluster's count — can be
driven from these scripts. The routes behind them are covered in `core/test/client.live.test.ts`,
the pick resolution in `deck/test`, and the harness fills the item card through the selection
panel's list, which is DOM. Hover picks *do* run under headless input and are slow there — every
`mouse.move` waits on a software-GL pick pass over the marks — so a mouse-up-to-panel time the
harness prints is the input's, and the store's own select-to-counted clock is printed beside it.

Headless chromium needs `npx playwright install chromium-headless-shell` once.

## What this deliberately does not do

Each of these is a recorded decision (design §7), not an oversight:

- **View keys and cross-channel consistency.** The bundle is static for this exercise. One request
  per view makes a single response internally consistent by construction, but nothing ties
  successive responses within one view key.
- **Reconciliation, prefix declarations, the session cursor.**
- **The change signal**, and the refresh affordance client-interaction §4 makes mandatory.
- **`{shown, total}` as an inseparable type.** The discipline is honoured in the panel; it is not
  enforced by the type system, so a future panel can still render a bare sample count.
- **Shown-but-stale**, the fourth display state. The other four — loading, retrying, empty and
  refused — *are* distinct, and only `shown` may display counts.
- **The conformance kit, the obligations list, the tile-addressed GET alias, labels, filters,
  export, and the Python client.**

## Things worth knowing before you read a number

**Depth is chosen for the mark budget, not from the zoom.** §7.2 makes marks-per-*tile*
depth-stable, so marks-on-*screen* is `m_target × tiles-in-view` and the only lever is which
depth's tiles are requested. The request carries `budget / m_target` tiles at every zoom level.
Measured at 1e9: depth climbs 6→10 as you zoom in while the tile count stays ~3,300–4,900 and
marks vary 2.33× — against ~25× for the tile-addressed version.

**`k` is not a viewer control any more.** It is a per-tile cap that θ never reaches (measured:
inert at every depth on every fixture), so the quantity worth setting is the budget. Raise
`serve.theta_target_marks` if you want `k` to bite.

**Calibration only ever goes deeper.** A shallower request returns a strict *subset* of what is
already drawn, so marks would pop *out* while the user did nothing — the lever design §7.2 and §7.3
strike as unsound. It also stops once the principal's whole visible set is served, or a sparse
principal ratchets depth to the tile cap forever to deliver its 1,366 marks.

**The request bbox is a *view* bbox, and is not inset.** `core/src/coords.ts`'s
`tileToRequestBbox` insets to cell centres and exists for addressing a single tile; the viewport
layer deliberately does not use it, because `tile_corners`' inclusive behaviour is what makes the
request cover every tile the view touches.

**`x-tessera-stage-ns` is usually absent.** It needs both the `bench-timing` cargo feature and
`[serve] stage_timing = true`. Absence is a configuration fact, not an error, and the stats panel
says so.

## What the panels do not say

The elements answer and never explain (owner, 2026-08-25): a panel's state is one line — *Nothing
here*, *Refused · 422 contract*, *Corpus updated · Refresh* — and the reasoning behind each number
lives here instead.

- **Shown, matched, visible.** *Shown* is a sample of *matched*; filters narrow *matched* and never
  move *visible*, which is the grant. The strip's shown cell carries its total on `data-total`, so
  the rule *both figures or neither* holds with the total drawn as the visible cell beside it.
- **Starting session…** The first request of a session materialises what this principal may see,
  which can take seconds at 10⁹; the strip says so once and then says *Loading*.
- **Corpus updated.** The numbers were drawn against a corpus that has since moved; the cells are
  blank until *Refresh* redraws them. A number against a stale view is never rendered.
- **Refused · 422.** A `422 contract` refusal is the host page's bug — a request the server would
  never have accepted — not the server's.
- **Nothing here.** An empty answer for this principal — an answer, not a failure — and
  indistinguishable from a region no principal has anything in.
- **A layer's size is never published.** What you reach of a layer is answered artifact by
  artifact, by the viewport; the picker offers a layer with its closure and no count.
- **A cluster's number** is members visible to this principal over the whole artifact — never
  its size, and never over the viewport. It holds steady during a pan. One refusal covers every
  withheld case (a suppressed artifact, one below its criterion, one this principal cannot
  reach, one that never existed) and nothing tells them apart.
- **Colour by cluster is exact only.** A point wears a cluster's colour because the wire named it
  a member; neutral means *not known here yet*, never a guess. The strip's hover reads *colours
  exact* or *refreshing N tiles*.
- **A legend lists the values on screen**, not the whole vocabulary, and a numeric ramp spans the
  marks served, not the corpus. A refused column draws every mark unmapped; every served mark is
  still on the map.
- **A typed category value is submitted, never validated**: values a principal cannot list may
  still be filtered by, and an unresolvable one is an empty answer — the control never says *no
  such value*.
- **A selection's numbers are inexact** where a counted cell is wider than a screen pixel: they
  are exact for the cells asked, not for the shape drawn, and render with `≈`. *Filter to this*,
  *Export* and *Save as artifact* wait on server verbs (D11) and are greyed with the reason on
  hover.
- **A miss is not a fault.** Nothing under the cursor is the ordinary case; a mark whose layer
  carried no identity is a fault in the map and the card says *Layer fault*.

## Why the points sit on a grid

They are on a lattice at high zoom, and it is the fixtures, not the renderer.

`data/scaled/geometry.parquet` carries `entity_id` and **`morton`** — no x/y. Given Morton codes,
the build reconstructs positions by de-interleaving (`tessera-build/src/input.rs`'s
`PointCols::Morton` arm), so stored x/y are **integer cell coordinates**, 0…65535. These fixtures
use an extent of exactly `0…65536`, so one data unit is one cell and every point lands on an
integer. Probed: 65,532 distinct x values across ~1 M served points.

This is faithful rather than broken — **a Morton code does not contain the sub-cell residual**
(design §15: *"what Morton cannot recover is only the residual within a cell"*). A bundle built
from genuine x/y goes down the `PointCols::Xy` path, which passes floats through untouched.

**It became visible only once the mark budget landed.** A cell is `512/65536` world units, i.e.
`2^zoom / 128` pixels — sub-pixel below deck zoom 7, about 8 px at zoom 10. The tile-addressed MVP
never went past depth 3; the budget-driven client reaches depth 10 routinely.

Nothing masked is affected: counts, tile selection and θ all work on Morton codes. Display position
only.

*Open, and deliberately not changed here:* the reconstruction uses `cx as f32`, the cell's
lower-left **origin**, so marks hug cell corners rather than sitting centred — a systematic
half-cell bias. `cx as f32 + 0.5` is the unbiased choice, but it changes stored bytes and the
reference oracle would have to agree.

## A free positional ground-truth check

The synthetic corpus carries **deliberate structures** (`probes/dataset.md` §"Replicas 0–4 pin
deliberate edge cases"), and they are the cheapest correctness check the viewer has: if the
positional pipeline — quantisation, Morton, the request bbox, the data→world transform — were
wrong anywhere, they would smear or move.

What to look for at 1e9 with a broad principal:

- **A sharp vertical line at the extent midpoint.** Confirmed in the data, not a rendering
  artefact: the midpoint column carries 1.37× its neighbouring column and 1.51× the equivalent
  row, with 1,339 of ~1 M sampled points at exactly `x = 32767`.
- **Dense edges at `x`/`y` = 0 and 65535** — the corner-pinned replicas (2 and 3) hitting the
  quantisation clamps. 26,444 sampled points sit at `x = 0`.

If the line renders crisp and vertical and the edges are dense, positions are right. *(Note for
whoever next touches the fixtures: `probes/dataset.md:131` describes replica 4 as "degenerate line
(y collapsed)", which would render **horizontal**. The measured structure is constant-**x**. One of
the two is mislabelled — harmless, but it cost an investigation once.)*

## Measured

_Machine: WSL2. Figures from the stats panel and `smoke.mjs`; re-measure rather than trusting
these._

### 2m4 (2,422,486 items, 109 MB)

Principal presets, measured:

| preset | terms | visible |
|---|---|---|
| narrow — term 14 | 1 | 243 |
| medium — term 112 | 1 | 12,465 |
| broad — term 79 | 1 | 181,900 |
| everything | 176 | 2,422,486 |

At depth 2 over the full extent, principal *everything*: 6 non-empty tiles, `served` 221 of
`visible` 1,994,089, ~1.2 ms server time, ~4.2 KB per tile with a `+3` underlay. Panning and
zooming are immediate.

### 1e8 (4.4 GB)

Presets: 1,205 / 19,327 / 4,273,751 / **53,300,931** (201 terms). All 201 measured in 3.5 s.

Per-tile median over five runs, principal *everything*:

| depth | server | wall | bytes |
|---|---|---|---|
| 0 | 129.4 ms | 131.3 ms | 2,132 |
| 2 | 11.1 ms | 12.7 ms | 2,132 |
| 4 | 0.88 ms | 1.6 ms | 2,324 |
| 6 | 0.29 ms | 0.8 ms | 2,836 |
| 8 | 0.23 ms | 0.9 ms | 5,460 |
| 6, underlay +3 | 0.41 ms | 0.9 ms | 4,252 |

### 1e9 (44 GB)

It runs. Opening the bundle took **35 s**; process RSS stayed ~3 GB with the rest in page cache.
Presets: 1,366 / 21,006 / 42,025,228 / **518,502,081** (201 terms), all measured in 26 s.

| depth | server | wall | bytes |
|---|---|---|---|
| 0 | 1,276 ms | 1,278.7 ms | 2,324 |
| 2 | 88.2 ms | 89.6 ms | 2,324 |
| 4 | 1.85 ms | 2.6 ms | 2,068 |
| 6 | 0.41 ms | 1.1 ms | 2,132 |
| 8 | 0.23 ms | 0.9 ms | 2,324 |
| 6, underlay +3 | 0.53 ms | 1.2 ms | 3,676 |

**Cost falls with depth, and payload is flat.** From depth 0 to depth 8 the server time drops by
four orders of magnitude while the response stays ~2 KB — screen area, not corpus size. The
corpus grew 10× from 1e8 to 1e9 and depth-6-and-below timings did not move.

**The shallow end is the cost, and at 1e9 it sheds.** A depth-0 tile over a 518M-item visible set
takes ~1.3 s, which is long enough that stacking several — switching principal mid-load, say —
saturates the compute-admission gate and the server sheds with `429 backpressure`. The viewer
reports that honestly (*counts unavailable — requests failed*, and the failure listed with its
code) rather than drawing an empty map. **The MVP does not retry**, though the server sends
`Retry-After: 1`; a client with a replica store should. Zoom in one or two levels and it is
immediate again.

At depth 3 over the full extent, principal *everything*: **21 of 518,502,081 shown**.
