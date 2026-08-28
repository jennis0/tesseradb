# Client components — four customers, one stack

**Date:** 2026-08-25
**Status:** Provisional r5 — **built through §9's six steps** (2026-08-25; the record is
[`client-delivery.md`](../client-delivery.md), which wins over this document wherever they differ).
Still Provisional because three decisions remain the owner's — D10 (viewer-plane CORS), D4 (the
notebook's proxy arm) and D11 (the selection operand). The fourth is settled: the leak-register row
for D12 (C30, proposed in `evidence/memos/2026-08-25-d12-membership-column.md`) is **not taken** —
architecture Appendix C's preamble names this column as the specimen of the register's ratchet and
gives the inclusion test that excludes it, *data the service serves never qualifying* (owner
direction 2026-08-26). It is Provisional also because the building changed three
things this document said, recorded in Appendix R's r5 entry. r4 was reviewed. r3 was reviewed across three lenses (truthfulness and
corpus fit, embeddability, the four customers; Appendix R) and every finding is dispositioned
here in one pass. **To become normative:** owner rulings on the open decisions in §11, and the
ruled ones written to `docs/decisions/`. Defers to [`client-interaction.md`](client-interaction.md)
for every obligation it restates. **Amends** [`client-architecture.md`](client-architecture.md)
§1 (§8) and its review finding F8 (§4). **Wire changes it asks for and does not depend on:** §3's
idioms (D9), a fetch-model hint (D8), the selection operand (D11), a label's target (D13); and
two it *does* depend on — a viewer-plane CORS surface for production (D10) and a per-point
membership column for colour by cluster (D12, ruled needed, exact only).
**Touches:** client-interaction §4, §6.1–6.2, §7, §8.6, §10, §12, §13, §15; client-architecture
§1, §6, §7 (D3, D4 amended in scope), F8; artifact-system §6; contracts §3.1, §3.2 (amended:
`layers`, `artifact_budget`, `k = 0`); `docs/design/README.md`; `docs/roadmap.md` ([#10],
[#46], [#47]); `clients/ts/README.md`; `run_demo.sh`.

---

## 1. Who this serves

Four customers, and they want different things from the same system. Serving one well at the
expense of another is the failure this document is organised to avoid.

| | who | what they need | what "done" looks like |
|---|---|---|---|
| **C1** | an application developer who wants a **drop-in** | a map and its panels in their page with the fewest lines and no state wiring; restyle it; rearrange it; in the extreme, replace one piece | one tag, two attributes, a working masked map; CSS to restyle; a slot to replace a piece |
| **C2** | someone with an **existing visualisation** switching to Tessera as the backend | our data in their tool — deck.gl, MapLibre, a canvas of their own — with our caching and scheduling doing the asking | a headless store that takes their camera and hands back typed arrays and counts; adapters for the common engines |
| **C3** | a **power user** building their frontend from scratch | a wire interface that is well designed and fully documented; **no dependence on our client** | an OpenAPI description, the framing documented to the byte, the obligations list, and a wire with no idioms you have to be told |
| **C4** | the **in-repo demo**, and the **notebook**, which is the same customer in a different container | out of the box, ready to go, showing everything | `run_demo.sh` and `tesseradb.Map(url, token)` both open on the full experience |

The client is **never responsible for disclosure**. The server decides what a principal may
have before any byte leaves it, whatever the client asked for. What a client can get wrong is
**truthfulness** — presenting a sample as a set, a stale view as current, a refusal as an empty
corpus, a masked count as a size — and the obligations this document carries are all of that
kind (client-interaction §10: *the kit's subject is truthfulness, not secrecy*). Two things are
of a different kind and are treated as such: **credentials** — where a token comes from and where
it may be held (§5.3, §7) — and **the leak register**, which one wire addition this document asks
for must pass (D12). Everything else is ergonomics, and is reported rather than refused.

## 2. The stack, and the rule that keeps it honest

Five layers. **Each is complete for its customer without the one above it.**

| layer | what | for | package |
|---|---|---|---|
| L0 | the **wire**: `/session/*`, `/v1/*`, framed Arrow | C3 | none — documentation |
| L1 | the **store**: verbs, decode, replica, driver, the presented frame, artifacts, filters, selection, the encoding accumulators — headless, no DOM | C2 | `@tesseradb/client` |
| L2 | **adapters**: a deck.gl layer; a tile-shaped adapter; React hooks | C2 | `@tesseradb/deck`, `@tesseradb/react` |
| L3 | **components**: the composite `<tessera-explorer>` and the pieces it is made of | C1 | `@tesseradb/components` |
| L4 | the **demo** and the **widget** | C4 | `@tesseradb/viewer`, `tesseradb[widget]` |

C1 never sees L1 until they replace a piece, at which point the replacement is on C2's surface
for that piece (§5.5). C2 never loads L3 — no Lit, no custom elements, and no deck.gl unless
they asked for the deck adapter (§8 says how the packages keep that true). C3 installs nothing.
C4 is L3 in two containers.

## 3. C3 — the wire is the product

The power user needs nothing built and three things written, all named by client-interaction
§13 for its mode 3 and none existing:

- **An OpenAPI 3.1 description** of the viewer and session planes, generated from the server's
  request and response types so it cannot drift. Errors, the closed code list, `Retry-After`
  on every 429, the three refusal classes (contracts §3.1). It carries what is in the types;
  what is only in doc comments today — `layers`, `artifact_budget` — lands in contracts §3.2
  first, because the description is not the place a contract is first stated. `auth_data` is
  plugin-defined, and the description can say only that it is bytes.
- **The framing, to the byte**, with a worked decode in two languages that share no code with
  ours — Python with `pyarrow`, JavaScript with `apache-arrow` — and a test that keeps the
  document true. contracts §5 has the framing; the worked example and the test are what is
  missing.
- **The client obligations list** — one page, the rules the server cannot enforce because they
  are about presentation, each with what goes wrong on the screen if it is broken:
  - the display states, and only `shown` carries a number;
  - both figures of a sample or neither, never a bare sample count;
  - a stale view is marked, and refresh is reachable;
  - a masked count is a count of what *you* can see and is never a size;
  - an absent artifact has no reason; an absent value in a filter is an empty answer, never
    "no such value" (contracts §3.2 — the one filter rule the server states for clients);
  - the artifact channel asks for itself rather than reading artifacts off a replica's point
    path; held state drops when the identity key or the filter changes;
  - `k` never decreases on zoom (contracts §3.2 — the server cannot enforce it);
  - a `tessera_id` is a `u64`: a decimal string in JSON, a `BigInt` off Arrow, never a JS number;
  - a same-origin proxy in front of the viewer plane forwards the six headers a replica is
    keyed and revalidated by (`etag`, `x-tessera-identity-key`, `x-tessera-stale`,
    `x-tessera-pin`, the two timings);
  - a 401 on a token that previously worked is the session ending, the same as a 403
    (contracts §3.1: the split is best-effort);
  - **depth is the client's choice and is not on the wire**: the request carries
    `budget / m_target` tiles at every zoom (client-interaction §10); the formula and its
    calibration are stated, because no description derives them.

**Pre-release, the wire may still change** (decision 0048), and writing the description is
where its idioms show. Four are visible now, recorded so the pass decides them rather than
documents around them (D9):

- **Artifacts are requested with `k = 0`** — a viewport request that asks for no points is the
  idiom for "just the artifacts". The engine accepts it; contracts §3.2 does not say so.
- **`layers` omitted means every layer**, the expensive default; a client that wants none must
  send `[]`.
- **`artifact_budget`** is accepted and inert on a flat layer, and is documented nowhere a
  stranger reads.
- **The artifacts frame is absent when nothing is served** — and this one is documented, not
  changed: contracts §3.2 makes it absent *deliberately*, so that why nothing was served is not
  distinguishable. A stranger is told; the wire keeps it.

**One topology question decides whether C1 works in production, and it is asked here because
C3 meets it too.** The C1 composite is a browser holding a *token* and calling the viewer plane
— client-interaction §7's T2 with the token handed to the browser — and the notebook's
browser-direct arm (§7) is the same shape. From any origin but Tessera's own, that needs CORS on
the viewer plane, and the only CORS the server has is `dev_cors_origins`, off unless typed and
logged as a development seam. So either the viewer plane gains an enumerated production
`cors_origins` for **token presentation only** — the session plane stays closed to browsers
and the credential never reaches one — or C1 is told that `viewer-url` must be same-origin,
behind a reverse proxy of `/v1/*` that forwards the six headers above. D10. Client-interaction
§15's open item — tokens are bound to the issuing session; *pin this before documenting T2(b)
as the default* — is this shape, and is settled by the same ruling.

## 4. C2 — the store, and getting data out of it

C2 has a visualisation and a camera. What they want from us is: *tell the store where I am
looking, and hand me what to draw, from your cache, with your scheduling.* The store is that
object, and it is `@tesseradb/client`'s main export.

```ts
const store = createStore({viewerUrl, token});           // or authorise: () => Promise<{token, expiresAt}>
store.setView({bbox, width, height});                    // on every camera change; the driver debounces
store.subscribe('marks', (m) => draw(m));                // typed arrays, ids, scalars, counts
store.pick(id).then(showRecord);
```

Three calls to a working integration. Behind them is the machinery client-architecture §3–§5
specifies and has built — the driver (the scheduler that decides when to ask, retry, revalidate
and look ahead; it takes a view and a size and owns every timer), the replica (the cache, keyed
by identity key), and composition (which held bands contribute to the view, at what prefix, on
what authority; in core today). Plus what still lives in the viewer and moves here: the
presented frame's bookkeeping, the artifact channel, item and artifact detail, filter
composition, category resolution, the legend fold, and the encoding accumulators (below). §9
step 0 is that move.

**The store owns no camera.** C2's engine does; the store is told. `setView` takes a bbox in
**data coordinates** — the space of `meta`'s quantisation bounds, the same space `dataXY`
returns — and a pixel size. One function converts: the bbox to world through the quantisation,
the centre to the driver's target, the zoom from the tighter axis (`log2(min(width/bw,
height/bh))`, so a camera whose aspect differs over-covers the other axis, which is safe); a
`setView` before `meta` has arrived is queued. The driver's motion region infers gesture from
the timing of the calls — the built driver has no gesture input; stillness is the gap since the
last call, velocity the target delta — so an engine that reports only `moveend` still works
through the leading edge and loses only the look-ahead's velocity bias. ⊘ A geographic corpus's
CRS is an ingest contract that does not exist (client-interaction §12); until it does, data
coordinates are quantisation-space numbers and nothing here names a projection.

**Projections** — typed, observable, read-only, each replaced immutably so a React
`useSyncExternalStore` snapshot is stable between changes — are the data-out surface:

| projection | carries |
|---|---|
| `meta` | `/v1/meta` as this principal sees it: columns, operands, layers with their dependencies, views, extent |
| `status` | `idle · loading · retrying · shown · empty · refused`; `sessionWarm`; the last refusal; **`stale`** (below) |
| `view` | the drawn region and depth; `visible` and `matched` as `Masked`; `served` as `Count`; provisional marks as a plain mark count — a screen fact, not a masked quantity |
| `marks` | the draw list: `ids` (`BigUint64Array`), world positions (`Float32Array` — what the replica holds; `dataXY` derives data coordinates from it, good to a hundredth of a cell), `scalars` (typed arrays by column), **membership ordinals per layer on** (`Uint32Array`, §5.10), per-tile provenance, and its `Count` |
| `tiles` | the exact per-tile `visible` and `matched` at the drawn depth, and the underlay's sub-cell counts where requested — the number channel, which §5.10's density reads |
| `artifacts` | the layers on; the served set with `centroid`, `box`, `hull`, `content`, `maskedCount` as `Masked`, `parentId`; the tree; **the session artifact table** and each ordinal's resolved colour (§5.10); the channel's status and refusal |
| `selection` | the picked item's record (named fields) or its refusal; the opened artifact or its refusal |
| `region` | the selected box or lasso; its `visible` and `matched` as `Masked`, its `served` as `Count`; the held marks inside it; the depth it was counted at (§5.11) |
| `filters` | operands from meta; the composed `FilterExpr` as sent; per-column value lists and their refusals |
| `legend` | per column: the values marks on screen carry, resolved; the palette ranks by observed frequency and the sticky numeric domains — **the encoding accumulators**, in the store |
| `replica` | bytes, points, bands held; the last plan's held-versus-fetched split; look-ahead spend |

**Verbs** are the write surface: `setView`, `setFilters(expr)`, `setLayers(names)`,
`setColourBy`, `setBudget`, `pick(id)`, `openArtifact(id)`, `select(shape | null)`,
`extentOf(artifactId)` (a data-coordinates bbox — what the map's `fitTo` uses; a C2 host moves
its own camera), `clear()`, `refresh()`, `dispose()`. `dataXY` accepts marks and artifact
geometry alike.

**Numbers are typed by what they are**, so a customer drawing their own panel gets the right
figure by taking the type's word for it:

- `Count = {shown, total, exact}` — a **served sample** of a set: `served`, the draw list, the
  marks inside a region. Show both figures, or neither when `exact` is false or the view is
  stale.
- `Masked = {value, exact}` — a **number-channel scalar** with no sample behind it: `visible`,
  `matched`, an artifact's count, a region's counts. One figure or none.

A formatter in the package renders each correctly; a customer who writes their own has the
types telling them which is which. "12,040 of 12,040" against a cluster is false, not secret,
and the type makes it unnatural to write. Counts of *marks* — provisional marks, marks inside a
region before the server answers — are plain numbers labelled as marks.

**Staleness.** `status.stale` means **the content key observed by the latest response differs
from the presented frame's** — the key the built replica already exposes for this
(`delta-serving.md` §2: identity, served watermark, overlay version; deliberately not merge or
compaction). It is **not** `x-tessera-stale`: that header is the broadcast geometry stamp, which
moves at every flush, merge and compaction whether or not anything this principal can see
changed, and a client wired to it would declare itself stale every tick under continuous
ingest. The signal is observed on responses, so a cache-answered pan surfaces it through the
replica's revalidation and the artifact channel, which are what drive it. On a content bump the
honest ordering is client-interaction §6.2's: the **numbers refresh eagerly** — the
revalidation response carries fresh counts, cheap and exact — while the **marks stay drawn and
are marked as older than the numbers** until `refresh()` redraws them.

**What the store drops, and when.** The replica, held artifacts, selection, per-column value
lists and the encoding accumulators go on identity-key change — a different principal's picture
must not sit under the new one's. Held bands also go on `setFilters`, because the identity key
excludes filters and the server cannot tell the client its holdings no longer match. A held
whole-layer artifact set goes when the content key it was fetched under rotates, and on
`refresh()`. These are staleness rules.

**The encoding accumulators live in the store**, not in the vis layer as client-architecture's
review finding F8 placed them. They are computed from held data — which values the marks on
screen carry, in what order they were first seen, how wide a numeric column has ranged — which
is *what was counted*, and a host on a plain canvas needs them without deck.gl (§8). What stays
vis-side is rank-to-colour: the palette applied.

**Adapters, one per engine family:**

- **`@tesseradb/deck`** — `TesseraLayer`, a deck.gl `CompositeLayer` over `marks`, `tiles` and
  `artifacts`, which is what the viewer's layer construction and GPU slab are today, given a
  class boundary. For the customer who owns a `Deck` already. Peer dependency on `@deck.gl/*`;
  a customer with deck has one copy.
- **A tile-shaped adapter** for MapLibre, OpenLayers and any engine that asks per tile —
  client-architecture D3's ruling: per-tile asks batched into region fetches, each answered
  from the replica, empty distinct from refused per ask. ⊘ Specified there and in
  `tile-addressed-integration.md`; built when a customer with such an engine exists.
- **`@tesseradb/react`** — `useTesseraStore(options)` and `useProjection(store, name)` over
  `useSyncExternalStore`, with `dispose` in the effect cleanup so StrictMode's double mount does
  not leak a driver. Its `/components` entry carries the C1 element wrappers, behind a separate
  entry and an **optional** peer on `@tesseradb/components`, so a hooks-only install pulls
  neither Lit nor deck.gl.

**Two stores, two cameras.** A store owns one view — one driver, one presented frame, one
derivation — so `<tessera-store>`'s sharing (§5.3) is for panels and maps that show *one* view.
An overview beside a detail is two stores.

## 5. C1 — the components

### 5.1 What people do with a masked map

The components follow the tasks, not the panels the instrument happens to have. Nine tasks,
and the one that is Tessera's own is the third:

| task | what it needs on screen |
|---|---|
| **orient** — what is here, how much, what is it called | counts for the region; names from an annotation layer |
| **navigate** — pan, zoom, jump to a thing, come back | the map; fit-to-extent; fit-to-artifact |
| **read the state** — is this current, complete, refused, still loading; how `visible`, `matched` and `shown` relate | a status strip that is always in view, and a refresh control when the corpus moved |
| **inspect a point** — a hint on hover, the record on click, then act on it in the host | a tooltip that costs no request; a card; an event the host handles |
| **narrow** — filter by attribute or text, watch `matched` fall while `visible` holds, clear | one control per operand; the counts beside them |
| **encode** — colour by a column, read what the colours mean | a selector; a legend |
| **see structure** — turn on a layer, read its outlines and names, open an artifact, walk its tree, see what is in view | a layer picker; the overlay; a list of what is served; a card |
| **select a region** — drag a box or a lasso, see how much is in it and what, then do something with it | a select mode on the map with live highlight; a panel with both numbers and the served items inside; actions |
| **refresh** — when told the corpus moved | one control, reachable from anywhere |

The demo adds a tenth — switch dataset and principal, watch the instruments — which is §7's.

### 5.2 What earns a tag

A piece is its own element when at least one of three things is true; otherwise it is a `part`
inside one:

- a host would **place it on its own** — a search box in their header, a legend in a map corner,
  a detail card in their sidebar;
- a host would **replace it** with something of their own — the item card is the obvious one,
  the filter panel the next;
- it is a **truthfulness primitive** a host would otherwise get wrong — a count that knows the
  difference between a sample and a set.

Applied, the rule cuts in both directions. A filter *panel* is not the unit — a host puts the
text search in the header and the category picker in a toolbar, so the unit is one operand's
control, and the panel is their composition. A field row in a card is not a unit — nobody places
one alone — so it is a part with a slot for replacement. A count *is* a unit, because rendering
one is where a host writes "12,040 of 12,040" against a cluster.

### 5.3 The catalogue

Thirteen tags in four tiers. C1 meets the first; a host that wants its own layout meets the
second and third; the fourth is for hosts building their own panels.

**Tier 0 — the experience.**

- **`<tessera-explorer>`** — the map with its status strip, toolbar, legend, layer picker,
  filters, artifact list and the detail card, in a default layout. Constructs its own store
  from `viewer-url` and `token`, or an `authorise` property returning `{token, expiresAt}`
  for renewal; no provider, no wiring. It is itself the context provider for its pieces.
  `layout="docked"` (map beside a sidebar) or `layout="overlay"` (map full-bleed, panels
  floating — the demo's look); the default is D2a's. Under a narrow container the sidebar
  becomes a sheet behind a button, by container query. `panels="filters legend layers
  artifacts detail"` chooses which appear. Every region is a named slot with default content
  (§5.5).

**Where the token comes from** is the host's server. It holds the deployment's *session
credential* — which the browser must never hold — and calls `POST /session/authorise` for the
signed-in user; the package ships that endpoint as a ten-line example beside the page. What
authority that call carries is the auth plugin's business, not the client's: today the only
plugin is `builtin:passthrough`, which takes bare claims, so the host's server is the component
that asserts what its user may see — client-interaction §7's claim-minting proxy, by
construction, until a verified-assertion plugin exists (D14, server-side, asked for). The
document says so where a C1 developer will read it, because a working example that is quietly
that shape is how the anti-pattern ships. **`session-url` is not on any C1 or C2 surface**; it
and the credential belong to the demo's configuration and to the notebook's kernel side (§7).

**Tier 1 — the map.**

- **`<tessera-map>`** — the canvas: points, the artifact overlay, hover, pick, the selection
  highlight, and the display states drawn on the canvas itself, so a refused or expired view
  never reads as an empty corpus. Store precedence, decided synchronously at connection: a
  `.store` property; else a context answer (the request is a one-shot event, so no answer at
  connection means no provider — a provider connected *later* is adopted only if the map has
  not built its own); else its own store from attributes; else detached. Properties:
  `colour-by`, `layers`, `budget`, `palette`, `tooltip-fields` (which of the scalars already on
  the wire the hover hint shows — hover costs no request; the click costs one), `mode` (`pan`,
  `box`, `lasso`; shift-drag is the shortcut in `pan`), and `basemap` — a deck.gl layer drawn
  **under** the points for a geographic corpus. ⊘ A tile basemap lines up with the points only
  when the quantised extent is the basemap's tile grid — the whole Web Mercator square, or a
  2^k-aligned sub-square at an integer zoom offset, with y increasing southward — which is an
  ingest condition nothing states yet (client-interaction §12); a corpus quantised to a tight
  bbox cannot be lined up by a `TileLayer` and is the tile-shaped adapter's case. Methods:
  `fit()`, `fitTo(artifactId)`, `select(shape)`. Four **corner slots** — `top-left`,
  `top-right`, `bottom-left`, `bottom-right` — for anything a host wants over the map, which is
  where the explorer puts the status strip and the legend; the pattern is MapLibre's and
  Leaflet's control corners, and it needs no JS. A `tooltip` slot takes a host's own hint
  renderer.

**Tier 2 — the panels.** Each reads the store by context, takes its data by property instead,
and is placeable and replaceable on its own.

- **`<tessera-status>`** — the state and the three counts. One line — *500 shown · 3,210
  matched · 12,040 visible* — with the state as a badge (loading, retrying, refused with its
  code, expired) and the **refresh control** when `stale`; the detail behind the numbers (the
  drawn region, provisional marks, what the replica holds) is a hover, not a panel. This is the
  view-info panel of the brief, reduced to the strip: on the mock-up boards the expanded card
  added nothing the strip and a hover did not, so the strip is the default and lives on the map
  where it is always in view; `expanded` exists for a host that wants the card. The counts are
  in that order because their relationship is the content — a filter narrows the answer and
  never the grant.
- **`<tessera-filter column="…">`** — one operand, rendered by its type from `meta`: a value
  picker for a category (with a refused enumeration shown as a refusal beside a free-text
  entry), a range for a number or a datetime, words-or-phrase for text, a value for a keyword.
  A typed value is **submitted, never validated against the enumeration**, and an unresolvable
  one is an empty answer: the control never says "no such value". Calls `setFilters` with the
  recomposed expression.
- **`<tessera-filter-panel>`** — every operand `meta` offers, as `<tessera-filter>`s, with
  applied-filter chips and *clear all*. A column absent from `meta` is absent here.
- **`<tessera-legend>`** — what the colours mean: the values marks on screen carry, resolved per
  column and never per vocabulary; a numeric domain as a ramp. `selectable` adds the colour-by
  selector, which the explorer's toolbar uses; without it the legend is a readout for a corner.
- **`<tessera-layer-picker>`** — which annotation layers the map draws, from `meta.layers`.
  **Usually one, and any number when they are different kinds of feature** — a geographic
  corpus with districts, incidents and routes as three layers is the case; stacking label
  layers over one clustering is not (owner direction 2026-08-24). What the picker offers is a
  layer **with its dependents**: a clustering's labels are a second layer that `depends_on`
  it (configuration.md's `[layer.labels]` expands to exactly that, and the demo publishes them
  so), and `meta` publishes the dependency, so one entry names the closure and the store names
  every layer in it in the request — each costs its own pass. Never a count of a layer's
  artifacts, because the wire never carries one.
- **`<tessera-artifact-list>`** — what the layers served for this view, as a list or a tree
  built from `parentId`, each with its content and its `Masked` count; click selects and fits.
- **`<tessera-item-card>`** — the selected point: its fields in declaration order, by name
  (`/v1/items` omits absent fields, so position lies), presented by `type`; a **slot per
  field** so a host renders a title as a link into their application without replacing the
  card; an `open` event for the same purpose. Distinguishes a miss from a broken pick.
- **`<tessera-artifact-card>`** — the selected artifact: its content, layer, key, its `Masked`
  count, its children from the held set. Steady during a pan by construction.
- **`<tessera-selection>`** — the selected region: its three numbers through
  `<tessera-count>`, the served items inside it as a list (title from `tooltip-fields`, click
  picks), and the actions — *clear* now; *filter to this*, *export* and *save as artifact*
  greyed with the reason on hover, as §5.11 says.

**Tier 3 — primitives.**

- **`<tessera-count>`** — renders a `Count` or a `Masked` correctly and nothing else: both
  figures or neither for a sample, one figure or none for a scalar, and neither when the view
  is stale. A host writing its own status line uses it and gets the rule for free.
- **`<tessera-store>`** — the provider, for panels and maps that share one view. Nothing else
  needs it and the documentation does not lead with it.

Two the brief did not name and this design declines: a **search box** (text `match` is one
`<tessera-filter>`, placeable in a header already) and a **principal switcher** (an
application's user is one principal; the demo's is an instrument).

**Selection** is two slots in the store — the picked point and the opened artifact — because a
point inside a cluster makes both meaningful at once. The explorer's `detail` region shows
whichever changed last; a host placing the two cards separately shows both.

### 5.4 The states, uniformly

Every panel renders the same states the same way, through a `part="state"` region, so a host
that restyles one has restyled them all. They map onto the store's `status` — `idle · loading ·
retrying · shown · empty · refused` and the flags — as the third column says:

| state | what the panel shows | from `status` |
|---|---|---|
| **detached** | no store and no data: nothing, and neither "empty" nor "refused", both of which are answers | no store |
| **loading** | a skeleton, no numbers; on the first request of a session, that the session is being established, since that can take seconds at 10⁹ | `idle` or `loading`; `sessionWarm` false |
| **retrying** | the skeleton and that it is retrying — after `Retry-After` on a 429, or with a bounded backoff on a 503 `not-ready`, which the built driver does not yet retry | `retrying` |
| **shown** | the content; the only state in which a number appears | `shown` |
| **empty** | that there is nothing here — an answer, not a blank | `empty` |
| **refused** | the refusal's code and detail; a `422` is the host's bug and says so | `refused` |
| **expired** | the session ended: a prompt, the `authorise` property if the host gave one, a `tessera-expired` event either way | `refused` with `expired-token` (403), **or `bad-credential` (401) on a token this store previously used** — a swept session is indistinguishable from one that never existed (contracts §3.1) — or `expiresAt` passed, which the store watches so renewal runs before the first refusal |
| **stale** | the numbers refreshed; the picture marked as older than them; the refresh control | `shown` with `stale` |

### 5.5 The explorer's regions

Named slots, each with default content, so replacing a piece is putting an element in a slot:

| slot | default | where it sits (docked / overlay) |
|---|---|---|
| `status` | `<tessera-status>` | map bottom-left in both layouts — always in view |
| `toolbar` | colour-by (`<tessera-legend selectable>`), `<tessera-layer-picker>` | sidebar top / map top-left |
| `legend` | `<tessera-legend>` | sidebar / map bottom-right |
| `filters` | `<tessera-filter-panel>` | sidebar |
| `artifacts` | `<tessera-artifact-list>` | sidebar |
| `selection` | `<tessera-selection>`, shown while a region is selected | sidebar, above `detail` |
| `detail` | the item card or the artifact card, whichever changed last | sidebar bottom / floating right |
| `tooltip` | the `tooltip-fields` hint | over the map, following the cursor |

The map's own control cluster — mode and fit — is the map's, not a slot. A replacement that
wants live data reads the store from its context and subscribes — at which point it is on C2's
surface for that piece, and the harness's claims (§9) cover the store-bound path, not what a
host renders from it.

### 5.6 The customisation ladder

Each rung is cheaper than the next, and a host stops at the first that does what they want:

1. **Attributes** — `layout`, `panels`, `colour-by`, `layers`, `tooltip-fields`.
2. **Tokens** — every colour, font, spacing and radius is a `--tessera-*` custom property with a
   neutral default that follows `color-scheme`, so light and dark come from the host page
   unconfigured; `--tessera-density` for compact or comfortable; `--tessera-map-height` for a
   map whose container gives it none. The map's **data palette** is a `palette` property, not a
   token — it encodes data, and brand colours are the wrong thing to encode data in. The default
   palette is distinguishable under the common colour-vision deficiencies.
3. **Parts** — `state`, `count`, `field`, `label`, `value`, `refusal`, `refresh`, `chip`, and
   the structural ones per component, for `::part()` from outside the shadow root. The names are
   the contract; the markup is not.
4. **Slots** — the explorer's regions, the map's corners and tooltip, the card's fields.
5. **Compose the pieces** in the host's own layout, with no explorer.
6. **Bring your own data** — every panel takes its data by property. A card fed from the host's
   own fetch works with no store on the page; what the types can check is all the primitive
   checks, and the harness's claims do not extend to it.
7. **Fork** — the components ship as readable source as well as built ESM
   (client-interaction §12's *editable rather than merely themable*).

### 5.7 Events

Custom events, prefixed, bubbling and composed so a host listens on any ancestor:
`tessera-viewchange`, `tessera-hover`, `tessera-pick` (the id and, once it arrives, the record),
`tessera-artifactopen`, `tessera-selectchange` (the shape, and the counts once they arrive),
`tessera-filterchange` (the composed expression), `tessera-layerchange`,
`tessera-statechange` (each transition of §5.4), `tessera-expired`. Ids in detail payloads are
decimal strings, the wire's own JSON form.

### 5.8 Accessibility

The panels are DOM and are accessible the ordinary way: labelled controls, keyboard operation,
focus order. The map is a canvas: it takes focus, arrows pan and `+`/`-` zoom, and the marks
are not individually focusable — the artifact list and the item card are the accessible route
to what is on it. The status strip is an `aria-live` region, so a refusal, an expiry or a stale
signal is announced. Motion follows `prefers-reduced-motion`; the default tokens meet AA
contrast in both schemes.

### 5.9 Mechanics — decided here, not left to the examples

- **Custom elements, with Lit.** The embedding primitive React, Vue, Svelte, Angular, plain
  HTML and anywidget share; one implementation of §5.4. Lit for the context protocol, reactive
  properties and `@lit/react` — about 5 kB (assumed from its published size). React 19 takes
  custom elements natively; `@tesseradb/react/components` wraps them with typed props and
  events for React 18 and for typing. Tags stay `tessera-*`: the product is Tessera, the scope
  is `@tesseradb` only because `tessera` was taken, and a tag has no registry.
- **Two distributions.** Unbundled ESM with `lit`, `@deck.gl/*` and `@luma.gl/*` as peers for
  anyone with a bundler, with **a subpath entry per element** so a host wanting a count, a
  strip or a legend beside their own map imports no deck.gl; and one self-contained bundle with
  its subresource-integrity hash for a page with no build step, which is also the widget's
  `_esm` (§7). The self-contained bundle **inlines the decode worker** and starts it from a
  Blob URL — a relative worker file does not exist inside a single file, and the built decoder
  falls back to decoding on the main thread silently, at tens of milliseconds per response —
  falling back to inline only where a host's `worker-src` forbids blob workers. Every
  `customElements.define` is guarded with `customElements.get`, because anywidget evaluates the
  module per model and a page may hold both distributions.
- **Client-only import** — Lit and deck.gl touch `window` at import, so a server-rendered page
  imports the package dynamically on the client. **Define before render** — the entry defines
  every element on import; Vue needs `isCustomElement: tag => tag.startsWith('tessera-')`, and
  Vue and Svelte need property bindings for object values. **Context before provider** — the
  protocol's request is a one-shot event, so the package attaches Lit's `ContextRoot` once on
  import to replay a request that arrived before its provider.
- **Per instance, not per module** — a `<tessera-map>` owns its `Deck`, its slab and its trace
  probe (a property; the demo publishes the first map's onto `window` for the smoke scripts).
  It finalises the `Deck` **a settle after** disconnection, cancelled if reconnected first —
  JupyterLab's windowed notebooks and a framework's reorder or keep-alive disconnect and
  reconnect elements routinely, and a synchronous finalize would rebuild the GPU slab on every
  scroll-past — and never disposes the store on disconnect. It is `display: block` with the
  height token because a custom element is inline and heightless and deck.gl sizes its canvas
  from its parent.
- **Standard decorators with `accessor`** (TypeScript ≥ 5.2): under the workspace's ES2022
  target, legacy `@property` on a plain field is shadowed by class-field definition.
- **Controls are not rebuilt under the user** — the instrument learned this by measurement (a
  filter box that could not be clicked into while marks streamed): Lit's keyed rendering keeps
  a control's identity across store ticks, and the panels that move every frame are separate
  elements from the ones a user types into.

### 5.10 How the map draws, and what the client holds per point

The look was settled on the design canvas against DataMapPlot — cluster colours, a density
wash, faint nested outlines, names sized by masked count — and the review found that the wire
carries no per-point membership, so the mock-up's colouring had no source. The owner's ruling
(2026-08-25) is **exact only**: a point wears a cluster's colour only when the wire said it is
a member; there is no geometric guess in between. That makes D12 a prerequisite of colour by
cluster rather than an upgrade, and it makes this section mostly about **the data path** —
what arrives, what is held per point at several million marks, and what happens when the
served artifact set changes under it — with the drawing at the end.

**What arrives.** With D12, a point request that names layers gets, per named layer, one
nullable `u64` column in the points frame: the `tessera_id` of the **deepest served**
artifact the point belongs to in *that response*, `null` if no ancestor is served. Deepest
served rather than the leaf, so the column never names an artifact the response withheld —
naming a finer artifact the principal was not served would say one exists. A point request
therefore names the layers that are on and pays their pass; the response's artifacts frame
comes with it and feeds the table below. The `k = 0` channel is still what enumerates the
artifacts *in view* for the panels, because held tiles are elided from point requests.

**The session artifact table.** Ids are stable across responses (a replacement mints new
identities, an edit keeps them — decision 0081), so the store keeps one append-only table per
session: ordinal → `{tesseraId, layer, parent ordinal where both ends were in one response,
level}` with the reverse map, dropped with the identity key like everything else. Ten
thousand artifacts a layer over a session is a few megabytes. The ordinal — a `u32` — is what
every point carries, and it is assigned in exactly one place:

- **The decode worker cannot assign it** — three lanes decode concurrently and cannot share a
  table without `SharedArrayBuffer`, which needs cross-origin isolation a host page rarely
  has. So the worker does the per-point work and none of the naming: it hashes the column's
  distinct ids (at most the served artifacts, ≤ 10⁴) and emits a **response-local index per
  point** (`Uint16Array` or `Uint32Array`) plus the distinct-id list.
- **The main thread names them**: the distinct list maps to session ordinals through the table
  — a few thousand lookups — and a tight remap loop turns the local indices into session
  ordinals as the band is built: about a tenth of a millisecond per fifty-thousand-point
  response, and no per-point hash on the main thread ever.

**The table and the texture are bounded by what is held, not by the layer.** A layer may hold
10⁶–10⁷ artifacts (the scale campaign's target), and a session over one at a fine cut can be
served a large fraction of them; a table and a lookup texture that only grew would reach tens
of megabytes and, for the texture, the GPU's size limit. So ordinals are **refcounted and
recycled**: each band's distinct-ordinal list is a reference, taken when the band is built and
released when it is evicted, and an ordinal whose count reaches zero returns to a free list.
Live ordinals are therefore bounded by resident marks — at most one per held point per layer,
in practice far fewer — and the texture is sized to the live range, grown in powers of two.
The `u32` itself is not the bound: it indexes four billion, and the flat-layer case (no cut to
bound a view; artifact-system §6's ⊘ wide response) strains the artifact channel's response
size, not the ordinal space.

**What a band holds per point**, at a million marks per column, with the membership added:

| per point | today | with D12 | where |
|---|---|---|---|
| `tessera_id` | 8 B `u64` | 8 B | CPU (picking resolves index → id) |
| position, world `f32` | 8 B | 8 B | CPU and GPU |
| declared scalars | 1–8 B each | same | CPU; GPU as colour when colouring by column |
| colour `RGBA` | 4 B | 4 B | CPU and GPU |
| picking colour | 4 B | 4 B | GPU, written once per growth |
| **membership ordinal**, per layer on | — | **4 B `u32`**, `0` for none | CPU and GPU |

Three million marks with one layer on is 12 MB more on each side; the slab's stated budget
(~20 B a mark CPU-side, 12–16 B GPU-side, six million marks across six retained depths)
grows by a fifth. The byte ledger counts it; eviction is unchanged.

**Colour resolves in the shader, through a lookup texture.** Today colours are computed
CPU-side per point and rewritten across every resident mark when the encoding changes. For
cluster colour that pass is replaced: the membership ordinal is a per-point GPU attribute,
uploaded with the band's slot through the same dirty-span path as positions, and the point
shader reads `colour = lut[ordinal]` from a data texture of one entry per *live* table ordinal (10⁵
entries is 400 KB; a million is 4 MB, still one texture). Everything a user does to the colouring is then **O(artifacts), never
O(points)**: choosing the level to colour at, changing the palette, highlighting the selected
cluster and dimming the rest, switching between cluster colour and column colour (a uniform)
— each is a rewrite of the lookup texture. This is the piece that makes several million marks
interactive; a per-point colour rewrite at that scale is tens of milliseconds and a 12 MB
upload, per interaction.

**Hierarchy is a walk in the table, not on the points.** `lut[o]` is the colour of
`resolve(o)`: walk `o`'s parent links up to the artifact currently served at the chosen level.
A band fetched when the cut served children resolves upward *exactly* — membership in a child
implies membership in its parent — so zooming out never touches a point. The walk fails only
where the edge was never seen (the child served alone, later its parent alone; `parent_id` is
on the wire only when both ends are in one response, deliberately), or where the cut moved
finer, since a walk cannot go down. Those ordinals resolve to **neutral**.

**Colour coverage, and what refetches.** A band is *colour-current* when every distinct
ordinal it carries resolves to something served now — checked per band over its distinct list
(a dozen entries), never per point, whenever the served set changes: 10⁴ bands is a
millisecond. A band that is not, and is in view, is **colour-stale**: its points draw neutral
and the driver's plan treats its tile as a refetch candidate **after** novel ground,
centre-first, exactly as a stale-content band already is. So:

- **The cut moves coarser** (zooming out): nothing refetches for colour; the table walk covers
  it. Neutral appears only for the never-seen-edge case, and is refetched.
- **The cut moves finer** (zooming in): the driver is fetching deeper bands anyway, and they
  arrive with the finer ids; the stand-ins drawn meanwhile resolve upward to the level they
  know, which is exact at that level, or neutral where they cannot.
- **The set changes without a zoom** (`artifact_budget`, a fold re-laying the layer): the
  content key rotates on a fold, which already stale-marks bands; a budget change marks the
  in-view bands colour-stale. Refetch is progressive either way.
- **The user switches layer**: no held band has the new layer's column, so every band in view
  is colour-stale at once. The new layer's hulls, names and counts appear immediately from the
  artifact channel; the points draw neutral and take colour centre-first as bands refetch —
  the same cost as a filter change today, and the honest cost of exact-only. Columns for a
  layer turned off stay on their bands until eviction, so switching back is free.

**Exact only, stated.** A coloured point asserts membership in the served artifact whose
colour it wears, at the level the table resolved it to; neutral means *not known here yet*.
The status strip's hover carries it: *colours exact* when every band on screen is current,
*refreshing N tiles* while any is not. Nothing on screen is a nearest-centroid guess.

**The drawing**, in order of what it reads:

- **Outlines** are the served `hull` or `box` — derived per principal, so exact for this
  viewer — as hairlines, faded; the selected artifact's strong, with a faint fill in its colour
  that is also the only *coloured* area fill. Nested contours from held marks are not drawn:
  they are the density of a per-tile-capped sample, and exact-only applies to shapes too.
- **The density wash** reads the number channel — per-tile `visible`/`matched` at the drawn
  depth, refined by the underlay's sub-cell counts where requested — binned in world space at
  the drawn depth, rebuilt at the settle, drawn as one texture. **Single hue**: colouring it
  by a tile's majority cluster would be a colour chosen from a sample, which is the guess just
  refused.
- **Points** in their membership colour through the lookup texture, or a column's colour, or
  the neutral.
- **Names and counts** at each artifact's `centroid` from its `content` and `maskedCount`, sized
  by masked count within a narrow band, placed by priority into a spatial hash (10⁴ labels is
  O(K) that way; a few hundred fit a viewport and the rest wait for zoom), with a leader line
  when a label moves. A dependent artifact (a clustering's labels) draws its text at its own
  declared `centroid` and shows no count until the wire names its target (D13). Free text is
  deck's `TextLayer` with `characterSet: 'auto'` and an SDF halo.
- **The palette** is positional — hue from the artifact's angle about the corpus extent's
  centre, lightness from its distance; stable under pan, converging when zoomed in — or, at
  the owner's choice, hues spread over the served set at each settle (D12).

**Sizes, modelled** — the render target is multi-million marks on screen and 10⁴-plus
artifacts in a layer (owner, 2026-08-25; the viewer runs at several million today, its
500,000 being only the input's default; the served set per view is bounded by the cut and
`artifact_budget`). Per response: decode as today plus one hash pass over the membership
column in the worker and a remap loop on the main thread. Per settle: a resolve walk over
≤ 10⁵ table entries, a colour-coverage check over held bands' distinct lists, one lookup
texture write, the wash texture, label placement — all O(artifacts) or O(bands), none
O(points). Per frame: nothing new; the shader's texture fetch. 10⁴ hulls × 20 vertices in one
`PathLayer` is a trivial draw. The harness measures every one of these at §9 step 2.

**What the server does** for D12: for each served point and each named layer, the leaf from
the row→artifact inversion the scale design already builds for containment, then a walk up the
edges until it meets the response's served set — a few steps per point — and a `u64` or a
null. Its leak-register pass is short: the column reveals membership of served points in served
artifacts, which the served hull already bounds, and deepest-served keeps finer structure out.

**No grid, and no tile structure**, anywhere: the storage's cells are never shown (owner
direction 2026-08-24). The demo's `clusters.json` sidecar is retired at §9 step 3.

### 5.11 Selection — box and lasso

The wire has no spatial operand: the request's bbox is the only region there is, and
client-interaction §9 rules that selection should become a **content-addressed filter operand**
— composable with other filters, cached like them, with the matched-versus-visible highlight for
free. ⊘ That operand is not built, for a rectangle or a polygon. What follows is what a
selection can do with the wire as it is, what waits on the operand, and how the two are kept
apart on screen.

**With the wire as it is.** A viewport response is per-tile counts plus points. A drawn region
is answered by one counting request (`k = 0`, as the artifact channel asks) in the **`tiles`
form** (contracts §3.2): the store rasterises the shape to Morton prefixes at a depth chosen so
the request stays under a client-side bound of a few thousand tiles — never "as deep as a
pixel", which for a full-screen box is 10⁶ tiles against a `max_tiles_per_request` of 262,144
and a response of megabytes — and asks for exactly those cells, so the sum is the answer to the
question asked rather than a bbox count subsetted afterwards. The sum is licensed by
derivability (client-interaction P3: it equals what the request returns); the live highlight
while dragging is the client's own computation (P1). **The highlight is the shape the user
drew** — the tile grid is a fact about storage and is never shown (owner direction
2026-08-24) — and `region` carries the depth it was counted at: where a counted cell exceeds a
screen pixel at the current view, the region's `Masked`s are **not exact**, and render as such,
because a number that is exact for a cell cover the user cannot see is not exact for the shape
they can. The cell grid is 2¹⁶ per axis, so that is the ordinary case at the overview and the
exception once zoomed in. The served items inside are the held marks whose positions fall in
the shape — a sample, so `served` is a `Count` whose `total` is the region's `matched` (P2: both
numbers, always). One request per settled gesture, debounced like the artifact channel.

**What waits on the operand**, greyed with the reason on hover rather than omitted: *filter to
this* (the selection composed with the other filters, so the counts and the map narrow to it)
is the operand; *export* (the items in the region as a table) is client-interaction §8.1's
bulk-export verb with its refusing threshold; *save as artifact* (a per-analyst selection,
annotations §8.3) is the runtime-artifact path artifact-system §10 lists as not built. Each is a
server-side verb, asked for in D11.

Undrawn items are never painted into the mark layer (client-interaction §9): the region's list
is a panel, and the map highlights only what it already draws.

## 6. Artifacts, at every layer

Artifacts are a data shape, not a customer, and each layer carries them: the wire has
`Artifact.centroid`, `box` and `hull` derived per principal, `content`, `masked_count` and
`parent_id`; the store's `artifacts` projection holds the served set and builds the tree from
`parentId`; `TesseraLayer` draws §5.10; the explorer has a layer picker, an artifact list and
an artifact card.

What every layer states the same way, because the wire gives nothing to fill any other reading
from: the count is over the whole membership and does not move with the viewport, so a card is
steady during a pan; an absent artifact has no reason — no "hidden" rendering, no greyed entry,
and an artifact with no served parent is a root; the layers that are on are named in the
request — with their dependents, since a clustering's labels are a layer of their own — because
omitting the selector pays for every layer and each named one costs its own pass; and the
channel asks for itself with its `k = 0` request per settled view, since the replica elides
held tiles and an elided tile carries no artifacts.

**Fetch model.** artifact-system §6 gives two — small layers fetched whole once and held, large
layers fetched as the budgeted coarse cut and refined on zoom — and records ⊘ that nothing in
`/v1/meta` says which a layer suits. The store picks by observation: it asks the whole map at
the layer's default cut on selection and holds the answer if it is under a configured size,
otherwise fetches per view. ⊘ A wide response over a large flat layer is unbounded today; that
configured size is the only bound this design puts between a customer and one, and streamed
continuation stays owned by the client epic. D8 asks for the hint. **Specified and not built,
and unchanged here:** membership by predicate, runtime artifacts and the edit verb, search over
artifacts, polygon shapes beyond bbox (artifact-system §10).

## 7. C4 — the demo and the notebook

**The demo** is `<tessera-explorer layout="overlay">` plus the instruments — the things that
measure Tessera rather than use it and are deliberately not in the library: the dataset picker
and principal presets (which is where `session-url` and the session credential live, as they do
today), the trace bar (`?trace=1`), the stage-timing readout (from the response trailer's
`stage_ns`), the request-failure list and the replica drawer's spend figures. Each reads a
projection like any piece. `run_demo.sh` keeps its invocation, its scales, its port table and
its `datasets.json`; the smoke scripts keep running against the demo page, reading the probe
from `document.querySelector('tessera-map')` and the panels through shadow-piercing locators
(`page.locator('[part="count"]')`) rather than `#stats` and `#panels`, which shadow DOM hides;
the budget figures they read move to the probe's projections. `publish-clusters.mjs` keeps its
publishing half and loses its sidecar half. `@tesseradb/viewer` keeps its name
(client-architecture D4) and becomes this page.

**The notebook** is the same explorer in an anywidget, shipped as the `widget` extra of
**`tesseradb`** — the one Python package, which later also carries the SDK (#47) and, in a
separate piece of work, an in-process Tessera. The base install has no JavaScript in it; `pip
install tesseradb[widget]` adds anywidget and the bundle, which the wheel's build hook builds
with Node at *wheel-build* time — a user installing from PyPI needs no Node, a developer
installing from a checkout does, and nothing of 1.5 MB is committed on every change to the
components (this replaces the committed bundle §9 first proposed; D6 ruled the package, and
this is the cheaper shape of the same ruling). `_esm` is the self-contained bundle plus a
`render({model, el, signal})` that mounts `<tessera-explorer>` into `el`; `_css` is empty.
anywidget's API is one package across Jupyter, Marimo (`mo.ui.anywidget`), VS Code and Colab;
its **topology** is not (below).

**The entry point is a token.** `tesseradb.Map(url, token=…)` is primary: the analyst holds a
per-principal token their deployment issued them, as any application's user does. The
credential-holding form, `tesseradb.authorise(session_url, credential, terms)`, is separate and
marked **operator-only**: the session credential can mint any principal, and a notebook that
takes it is client-interaction §7's pooled-service-token anti-pattern in a cell — the shape §10
says a practitioner will write if the SDK does not give them the other one. It exists for the
local, single-principal case (mode 1) and for the demo.

What crosses the kernel boundary, as synced traitlets, is **control and selection, never
data**: URLs and view name down; `bbox`, `layers`, `colour_by` and `filters` (the composed
`FilterExpr`, applied without the panel's debounce) both ways, up-synced at the settle and never
per frame — Marimo folds every synced trait into one `.value`, so a cell reading it re-runs per
settle, and the documentation points Marimo users at `observe` on the selection traits;
`selected` and `selected_artifact` (one `tessera_id` each, **as a decimal string** — a `u64` is
not a JS number and a `BigInt` does not serialise) and `region` (the shape and its counts once
they arrive) up. `widget.selected` in the next cell is the picked item; setting `widget.filters`
redraws; the pan path stays browser → Tessera with the kernel never on it (client-interaction
§7). A `to_arrow()` that ships the current `marks` over the comm as Arrow buffers for a
DataFrame is the natural next verb and is not in the first cut.

**Token custody** is client-interaction §15's open item and the notebook's credential concern.
The answer is that **the token is never model state**: the JS side sends `ready` from
`initialize({model})` — once per model, so two views of one widget share it and two widgets do
not — and the kernel answers with the token as a **custom message**, which no route that
serialises widget state can save: not the frontend's opt-in "save widget state", not `nbconvert
--execute`, not papermill, not a headless run where no frontend mounts. Expiry sends
`reauthorise`; a view rendered after a page reload sends `ready` again. What remains is a token
in browser memory for `token_max_lifetime_secs`, which is browser-direct's exposure everywhere.
⊘ The messages are built at §9 step 5, and Marimo's delivery of kernel-side custom messages is
verified there, not assumed.

**Where the widget's requests go** is D10's question again, and it splits the containers.
Browser-direct — the widget's JS calling the viewer plane from the notebook page's origin —
needs the viewer-plane `cors_origins` D10 decides, and can only ever be enumerated for
JupyterLab and Marimo on a known origin: a VS Code notebook renders in a `vscode-webview://`
origin and Colab in a sandboxed iframe, neither of which an origin list can name. For those, and
for a JupyterHub whose browser cannot reach Tessera, the **proxy arm**: the widget's base URL is
a path on the notebook server, and the proxy holds the credential, authorises per principal and
renews — which is more than vanilla `jupyter-server-proxy`'s static header injection, so it is
a small server extension of it. ⊘ Documented, not built, in the first cut; D4 chooses which arm
ships first, conditional on D10. Several `Map` cells each own a `Deck`, and the deferred
finalize of §5.9 on windowed-out cells is what keeps that under the browser's context cap.

## 8. The packages

The npm scope is **`@tesseradb`**, matching the Python package; the repository's two existing
packages, `@tessera/client` and `@tessera/viewer`, take the new scope at §9 step 1 — no
deployment holds the old names (decision 0048). Client-architecture D4 ruled that the viewer
keeps its *name*; what it holds changes here, so D4 is **amended in scope, not kept**.

| package | customer | depends on | contents |
|---|---|---|---|
| `@tesseradb/client` | C2 (and every layer above) | `apache-arrow` | verbs, decode, coordinates, replica, driver, composition, **the store**, the encoding accumulators and the default palettes (colours are data) |
| `@tesseradb/deck` | C2 with deck.gl; C1 through the map | client; peers `@deck.gl/*`, `@luma.gl/*` | `TesseraLayer`, the slab, rank-to-colour, the density texture |
| `@tesseradb/react` | C2 in React; C1 in React | client; peer `react`; `/components` entry with an *optional* peer on `@tesseradb/components` | hooks; element wrappers |
| `@tesseradb/components` | C1 | client, deck (only the map's entry); peer `lit` | `<tessera-explorer>`, the pieces each behind its own subpath, `<tessera-store>`, the formatter, tokens and parts |
| `@tesseradb/viewer` | C4 | components | the demo page and its instruments |
| `tesseradb` (Python) | C4 | `[widget]` extra: anywidget; the bundle built by the wheel's build hook | `Map`, `authorise` (operator-only), the widget class, the mount, the messages; later the SDK and the in-process instance |

This amends client-architecture §1's two-package split: its second package — the deck.gl
binding and its GPU slab, the panels, the trace bar, the DOM wiring — becomes `@tesseradb/deck`
and `@tesseradb/components`, and `@tesseradb/viewer` keeps its name and its instruments and
consumes them. D1a's boundary rule is unchanged and is where `client` stops and `deck` starts;
D3's adapter story is §4's tile-shaped adapter. The encoding accumulators moving to the store
amends client-architecture's F8 (§4).

## 9. What lands, and what proves it

The order, as client-architecture §6 gives its own — the demo working at every step, the smoke
scripts as the net, one worktree per step:

0. **The presented frame moves.** Composition is in core already (`compose.ts`; the viewer's
   `assemble.ts` is buffer concatenation, which is vis) and `ViewState` is already the driver's
   type; what moves is the frame-applying bookkeeping the deck binding does today — the store
   holds the `Composition` as its presented frame and absorbs those writes — and the duplicate
   `ViewState` in the viewer goes. This finishes client-architecture §6 step 3.
1. **The store**, with the projections, `Count` and `Masked`, `status.stale` on the content
   key, `setView`'s conversion, `dataXY`, `extentOf`, `createStore` with a token supplier, the
   encoding accumulators, and the scope rename. The artifact channel, item and artifact
   detail, category resolution, filter composition and the legend fold leave the viewer's
   entry file. The viewer consumes the store; nothing visible changes; the smoke scripts green.
2. **`@tesseradb/deck` and `@tesseradb/components`**: `TesseraLayer`, the density texture from
   the number channel; `<tessera-map>`, `<tessera-status>`, `<tessera-count>`,
   `<tessera-item-card>`, `<tessera-filter>` and `<tessera-filter-panel>`,
   `<tessera-explorer>`, box selection with `<tessera-selection>`. The viewer becomes the
   explorer plus instruments. Smoke green, through shadow-piercing locators.
3. **Artifacts and encoding**: `<tessera-layer-picker>` with dependency closure, wire geometry
   drawn, the membership attribute and lookup texture once D12 serves, the sidecar retired, `<tessera-artifact-list>`, `<tessera-artifact-card>`,
   `<tessera-legend>`; lasso selection in the `tiles` form. `smoke-artifacts.mjs` asserts hull
   and label render under two principals.
4. **C1 and C2 examples in the gate**: a plain-HTML page, a React page using the explorer, a
   page using the store over a plain canvas with none of our rendering, and `@tesseradb/react`.
   New workspaces enter the root `workspaces` array with a `typecheck` script, which is all
   `check-clients.sh` needs. Vue and Svelte documented, not checked.
5. **`tesseradb[widget]`** and the notebook example, run in Jupyter and Marimo by hand once,
   the wheel's build hook in the gate thereafter.
6. **C3's documents**: contracts §3.2 amended, the OpenAPI description generated from the
   server's types, the worked decodes with their test, the obligations list; and whatever §3's
   idioms were ruled to become.

**Acceptance** is the conformance harness client-interaction §10 asked for and has had no
subject: the smoke scripts become DOM-level assertions through the parts — only `shown` renders
a count; both figures render or neither; a refusal renders as one; no count renders against a
stale view and a refresh control is present when one is; a region's count renders as inexact
when its cell exceeds a pixel; an artifact's count does not move across a pan; a switch of
principal empties every card. They run against the demo page and against the C1 example page,
which is the same assertions with none of our layout, and they need a served bundle, ports and
headless Chromium — so they are a target beside the gate, as the smoke scripts are today, not a
step in it. The C2 example is the check that the store is usable with none of our rendering.

## 10. Options considered and rejected

- **React-native components with wrappers for the rest.** Notebooks, Vue and Svelte get the
  worse path, and the demo carries a React runtime to draw its panels.
- **Headless core only, no components.** Serves C2 and C3 and abandons C1 and C4 to assembling
  the demo themselves.
- **Components only, no separately usable store.** Serves C1 and makes C2 adopt Lit and deck.gl
  to get a cache.
- **A provider element required for every page.** One more thing to wire for C1, for a benefit
  — sharing a store — that one customer in a hundred wants.
- **Light DOM with a class contract.** Simpler to restyle from a host stylesheet, and impossible
  to keep from being restyled by accident; D2.
- **Folding the deck adapter into `@tesseradb/client`.** Makes the headless package depend on
  the GPU path, which a customer drawing to their own canvas never wants.
- **Staleness from `x-tessera-stale`.** The broadcast geometry stamp; under continuous ingest
  it would mark every view stale every tick. The content key is the tier that means something
  changed for this principal.
- **Contours and colour from held marks.** A per-tile-capped sample; density from it is flat
  where the data is densest, and a shape around it is the sample-as-set error in geometry.
- **Counting a region at pixel depth.** 10⁶ tiles for a full-screen box; the `tiles` form at a
  bounded depth, with the inexactness stated, is the honest shape.

## 11. Decisions for the owner

**Ruled in conversation, 2026-08-24 — to be written to `docs/decisions/` at promotion:**

- **D6 — one Python package, `tesseradb`**, the widget as its `[widget]` extra; the SDK and the
  in-process instance join it later; sharing no code with `reference/`. The npm scope
  `@tesseradb/*` to match. (The bundle's build mechanics changed in r4 — built by the wheel's
  build hook rather than committed — which is within the ruling.)
- **Layers**: usually one; several only for different kinds of feature; never stacked label
  layers. **The tile grid is never shown.** **The status strip is the default**, the expanded
  card optional. **The map's look follows DataMapPlot** — subject to D12, which decides how much
  of it the wire allows.

**Open:**

- **D1 — custom elements, authored with Lit; React through `@tesseradb/react`.** Recommended.
- **D2 — shadow DOM with tokens, parts and slots**, against light DOM with classes. Recommended:
  shadow.
- **D2a — the explorer's default layout**: `docked` or `overlay`. Recommended: docked — the
  predictable one inside an application; the demo sets overlay.
- **D3 — the package split of §8**, amending client-architecture §1. Recommended as written.
- **D4 — the notebook's first-cut arm**: (a) browser-direct for JupyterLab and Marimo, which
  needs D10's CORS and cannot serve VS Code or Colab; (b) the proxy arm as the general one,
  with its small server extension. Recommended: (b) if D10 declines CORS, (a) first if it
  grants it — the two are one ruling.
- **D5 — instruments stay in the demo.** Recommended.
- **D7 — the first-cut set**: §5.3's thirteen tags, `TesseraLayer`, the hooks. Two declined
  pieces recorded there.
- **D8 — a fetch-model hint in `/v1/meta`** (artifact-system §6's ⊘). Asked for, not depended
  on.
- **D9 — the wire idioms of §3**: `k = 0` (document as accepted), `layers` omitted meaning all
  (change or document), `artifact_budget` (document), the absent artifacts frame (documented,
  kept). Recommended: decide each at the contracts amendment, with 0048 in hand.
- **D10 — a viewer-plane `serve.cors_origins` for token presentation.** Without it C1 works in
  production only behind a same-origin proxy of `/v1/*`, and the notebook's browser-direct arm
  does not exist. Recommended: **yes** — enumerated, viewer plane only, the session plane never
  browser-facing — because it is a token-presentation surface and the token is already
  per-principal; the earlier "no" was argued for T1's credential exposure, which this does not
  create.
- **D11 — the selection operand and its verbs** (§5.11): a rectangle-and-polygon filter operand
  on the wire, and behind it the export verb and the runtime-artifact path. Box and lasso work
  without them; *filter to this*, *export* and *save* wait. Asked for, in that order.
- **D12 — a per-point membership column** in the points frame for the layers that are on:
  the deepest served artifact's id, server-derived per principal, one `null` (§5.10). **Ruled
  needed, exact only** (owner, 2026-08-25): colour by cluster exists only when the wire says so,
  with no geometric guess between. Needs a leak-register pass. Still open in it: the palette's
  centre — the corpus extent (stable under pan, converges when zoomed in) or hues spread over
  the served set at each settle (never converges, recolours when the set changes).
- **D13 — a dependent artifact's target on the wire** (§5.10), so a label can show its
  cluster's count and be placed by its cluster's centroid. Until then label layers declare
  `centroid` and show no count. Asked for.
- **D14 — a verified-assertion auth plugin** (client-interaction §7's documented default),
  without which every C1 host's server is the claim-minting proxy. Server-side, outside this
  design, named here because §5.3 has to tell C1 the truth about the example it ships.

## 12. Provenance

The owner's direction of 2026-08-24, in two parts: the four-point brief (package, split,
restyle, artifacts) and the four-customer re-cut with the correction that the client is never
responsible for disclosure; the same day's rulings on the design canvas. The boundary rule, the
driver and the migration order are client-architecture §1, §3 and §6 (driver built,
`6aa84f7`). The three-layer stack, the projections rule, mode 3's document set and the anywidget
ruling are client-interaction §10, §12 and §13; the stale mark and the refresh affordance are
its §4 and §6.1–6.2; the notebook fork and the two anti-patterns are its §7; the token custody
and binding items are its §15. The two artifact fetch models and the ⊘ hint are artifact-system
§6; the wire's headers, refusal classes and the `tiles` form are contracts §3. anywidget's
`render`, `initialize`, custom messages and Marimo's two-way sync are from its current
documentation.

The design canvas — the explorer in three layouts and every state, the lasso flow, and five host
applications embedding the same pieces — is published as an artifact ("Tessera Client
Components", 2026-08-24); its generator and layout are kept at
`docs/evidence/mockups/client-components/` so the boards can be rebuilt, and the rendered boards
are not committed because they are its output. The boards' rendering assumed per-point
membership, which the wire does not carry; §5.10 records the consequence.

On the roadmap: [#10] becomes four packages; [#46]'s reference viewer becomes §7's demo; the
widget precedes [#47] and does not depend on it, and both live in `tesseradb`. The roadmap
paragraph is updated on promotion.

## Appendix R — review trail

- 2026-08-24: r1 drafted; r2 reviewed across three lenses (disclosure, embeddability, corpus
  fit), twenty-seven findings dispositioned in one pass — the token traitlet unsound (now a
  custom message), the artifact count typed as a sample (now `Masked`), a bare served count via
  `marks.length` (now carried with its `Count`), a held layer that never saw the content key
  rotate, `setFilters` without its drop, the migration's first two steps coupled, colour ranks
  on both sides of the boundary, the change signal absent, "session" colliding with the wire's
  term, the drop-in deck.gl layer missing, the third package amending client-architecture §1
  unstated.
- 2026-08-24: r3 — re-cut on owner direction around four customers, obligations as truthfulness,
  the composite `<tessera-explorer>`, data by property, the store as C2's surface, C3's
  document set, the wire idioms, the CORS question; `tesseradb` and the `@tesseradb` scope;
  box and lasso selection; §5 rewritten from user tasks; the design canvas and the owner's
  rulings on it (several layers for feature kinds, no tile grid, the strip as default).
- 2026-08-25: r4 — r3 reviewed across three lenses (truthfulness and corpus fit, embeddability,
  the four customers), forty-one findings, dispositioned in one pass; every one accepted. The
  ones that changed the shape: **per-point membership is not on the wire**, so the DataMapPlot
  rendering splits into what draws exactly from the number channel and the served hull, what
  the client assigns as a mapping (nearest served centroid), and what a membership column
  would make exact (D12); **`x-tessera-stale` is the broadcast geometry stamp**,
  so staleness keys on the content key with numbers refreshed eagerly and marks stale-marked;
  **counting a region at pixel depth** is 10⁶ tiles, so the `tiles` form at a bounded depth with
  the inexactness typed; **the drop-in's token** comes from the host's server calling authorise
  with the session credential, which under the only plugin is the claim-minting proxy — said
  where C1 reads it, with a verified-assertion plugin asked for (D14) and `session-url` removed
  from every non-demo surface; **browser-direct needs viewer-plane CORS** and cannot serve VS
  Code or Colab, so D4 and D10 are one ruling and the recommendation on D10 flips to yes;
  **labels are a dependent layer** with no target on the wire (D13; the picker names the
  closure); **the notebook entry point takes a token**, with the credential form operator-only;
  `expired` defined against a best-effort 403 and a post-sweep 401; 503 in `retrying`; the
  positional palette's centre; `setView`'s conversion and coordinate space; f32 world positions;
  `extentOf`; the encoding accumulators to the store (amending client-architecture F8);
  optional peers and per-element entries; the inline worker in the single-file bundle; context
  precedence; deferred finalize; shared store as shared camera; decimal ids in traitlets and
  events; the "no such value" filter obligation; provisional marks as a plain count; the state
  mapping; the basemap's alignment condition; by-property data outside the harness's claim; D4
  amended not kept; step 0 rescoped; the bundle built by the wheel's hook; the harness beside
  the gate; the decisions split into ruled and open with the count corrected to thirteen.
  Style: vocabulary aligned with annotations.md, process narration moved here, the §8.6
  misquotation removed, file citations kept to §9.
- 2026-08-25: owner, on r4: colour by cluster is **exact only** — the nearest-centroid mapping is
  dropped and D12 becomes a prerequisite; the render target is multi-million marks and 10⁴-plus
  artifacts a layer; §5.10 rewritten as the data path — the deepest-served column, the session
  artifact table with ordinals named on the main thread from a worker-local index, the
  membership attribute and lookup texture so every colouring interaction is O(artifacts), the
  table walk for hierarchy, colour coverage and what refetches under a cut change or a layer
  switch, and the per-point byte table at several million marks.
- 2026-08-25: r5 — built, steps 0–6 and the server track for D12, one day, one controller and
  seven implementers. The owner ruled three more things mid-build: **D8 declined** (no fetch-model
  hint; the store picks by observation); **D13 as a count, not an identifier** — a dependent
  artifact carries its *target's* masked count, and the target's id stays off the wire; **D9's
  `layers`: omitted or `[]` is none, the string `"all"` is every reachable layer**, `all` refused as
  a layer name. What the building changed: (1) §7's *two views of one widget share one store* does
  not hold — one store has one view input and two explorers over it fought for the camera; the
  store is per view, the token supplier per model, and the active view syncs up. (2) §5.10's
  colour-stale band does not go wholly neutral — zeroing its ordinals would be the per-point pass
  decision 0100 refuses; it keeps drawing the ordinals that still resolve, and stand-ins draw
  neutral. (3) §5.11's box→prefix rasterisation and the exactness rule live in the store, not the
  deck adapter, because the store issues the request. Found at integration, none in the design:
  the artifact channel is built at meta, so a verb before meta must be held as intent; the channel
  must clamp its depth to `max_tiles_per_request` whatever view it is paired with; the density
  wash at a coarse depth was the tile grid, filtered now (decision 0097); under headless swiftshader
  the server sheds a stream the client stops reading — an environment fact the record carries with
  the measurements. Every §5.10 figure is now measured (headed Chromium, 996,488 marks coloured
  through the lookup texture: per settle ≤ 3.5 ms, remap median 4 ms, frame 16.7 ms) except the
  layer-switch refill, which needs a second layer in the demo.
