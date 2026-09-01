# View switching — one store, several views, and two drop-downs

**Date:** 2026-09-01
**Status:** Provisional r2 — the client-side design, ready to build; the one wire change it names
(§5's `positions` verb) waits on an owner ruling (§10) and is not a precondition for anything
else here. It follows from [`views.md`](views.md) (normative: what a view and a view group are)
and sits under [`client-components.md`](client-components.md) (the client stack it extends);
where it names a wire change it defers to [`contracts.md`](contracts.md), which does not yet
carry it. Owner direction 2026-09-01, recorded because the design turns on it: **users will
switch views regularly; within a group, a slider moved rapidly is a first-class behaviour;
entities drawn in both views should animate between their positions on a switch; there is no
precaching on slider movement; and the picker is two drop-downs — one choosing between view
groups, one moving between the views of a group.**

**Reads with:** [`views.md`](views.md) §2–§5, [`client-architecture.md`](client-architecture.md)
(the replica and its bands), [`client-components.md`](client-components.md) §5.3 (the
catalogue), §5.5 (the explorer's regions) and §7 (the demo and the notebook),
[decision 0040](../decisions/0040-quantisation-is-slice-scoped-index-config.md) (the extent is
the view's), [decision 0113](../decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md)
(a key is the only address, and creation order is the only order).

---

## 1. Summary

A deployment may declare several views over one entity space — two projections of the same
papers, or a group of quarters — and the server serves each with its own frame, gate and
artifacts (`views.md`). The client does not: its store binds one view when it is built and has no
way to change it, and the viewer offers no choice. This document says how the client holds and
switches between views, in six claims.

1. **A switch is a pointer change, never a rebuild.** The store holds the machinery of several
   views at once — a replica, a presenter and an artifact channel per view id — and *current
   view* is a field. Everything that is not geometry is shared and untouched by a switch:
   session, mask, `meta`, filters, colour, layer choice, item details. Components subscribe once.
2. **One byte budget over every view's bands**, evicted least-recently-drawn across views. There
   is no separate "cache of views" and no prefetch — a view is warm because it was shown, and
   **a view that is not current has no request in flight.**
3. **The active view has the whole point budget.** It is never split to keep another view warm.
4. **Within a group, a switch keeps the camera, the depth rule and the selection.** Every view of
   a group shares one frame, so the same tiles at the same depth are the request in the next
   view. A switch between frames refits the camera, drops the selection and re-decides the
   basemap.
5. **The picker is two drop-downs.** `<tessera-view-picker>` chooses the *layout* — a plain view
   or a group; `<tessera-key-picker>` moves along the current group's roster in creation order,
   with previous and next beside it. Both draw nothing for the one-view corpus that every demo
   corpus is today.
6. **Animation is a client-side join over what the server served**, and it needs one new wire
   verb to be honest when zoomed in: *positions in view B for these ids*, gate-filtered per view
   like the item detail's `views` list already is. It is the one part of this design that waits
   on a ruling.

## 2. What is already in place

The wire and the core types are multi-view; this document adds nothing to what `/v1/meta` says.
Each `views[]` entry carries its own `quantisation`, `projection`, `worldAspect` and `tileScheme`
— the field that decides whether a basemap may be drawn — and, for a group view, its `roster`
(group, key, typed metadata). `groups[]` lists each group's views in creation order, with a
`title` where the deployment declared one and `membersOf` for a second layout over another
group's keys. Every viewer verb names its view. `ItemDetail.views` lists the views an item is in
*that this session may reach*, each with that view's own grid coordinates; `ItemDetail.scoped`
carries group-scoped values by family and key; `ItemDetail.labels` the satisfied labels.

The store's frame accessors are per view (decision 0040). `@tesseradb/client` already exports
`viewsOfGroup(meta, group)` and `stepView(meta, id, ±1)` — the roster walk in creation order,
non-wrapping — with tests; nothing calls them. The notebook widget takes a `view` trait and on
`change:view` disposes and rebuilds every store — the blunt form of a switch, correct and
unusable under a slider.

**What is absent.** A way to change the store's view after construction; any choice of view in
the viewer, which takes `meta.views[0]` at three sites (`viewer/src/main.ts`, at activation and
at both basemap calls); any rendering of `ItemDetail.views`, `scoped` or `labels` on the item
card; a `scopedScalars` field on the client's `Meta` (a doc comment names it, the type does not
declare it) — though the filter draft already composes a pinned leaf `family@key` verbatim, so a
scoped family under another view is a naming question and not a machinery one; and a view on the
client's `/v1/categories` request, which the server has been view-addressed for since
2026-08-31 (contracts §3.2) — **built at V1**: the optional `view` query parameter, passed at
both store call sites. The value cache behind it is per column with no view axis, which is
harmless until a group-scoped family is served (§6.8).

## 3. The store holds several views

```
TesseraStore
  session, mask, meta, filters, colourBy, layer choice, budget, palette,
  item details, described records, the session artifact table      — shared, one of each
  current: view id                                                  — projections.view.id
  perView: Map<view id, { replica, presenter, channel }>            — built on first visit
  bands: one byte budget across every view's replica, LRU by last drawn
```

**`setCurrentView(id)`** is the operation. It changes `current`, publishes the `view` projection
with the new id, and the next frame is answered from `perView.get(id)` — cold on first visit,
warm on return. A component that draws reads the current view's replica through the same
accessor it reads today; a component that lists artifacts reads the current view's channel.
Nothing re-subscribes. Before `meta` has arrived the call is queued, as `setView` is, and applied
at warm-up in place of `options.view`; `options.view` remains the initial view and
`meta.views[0]` the fallback.

**An id that is not in `meta.views` is ignored and reported** through `onTrace('view-switch',
{refused: 1, id})` — never a throw and never a guess at a neighbour. The pickers are built from
`meta.views`, so they cannot produce one; the viewer's URL state can, and the viewer's fallback is
its own (§6.5). This is the report-and-continue rule of the project's strictness note: a wrong
view id discloses nothing and costs a rerun.

**What is per view and why.** A replica's bands are geometry quantised under one frame, a
presenter's composition is one view's drawn frame and its counts, and an artifact channel's
served set and payloads are per row space (`views.md` §2), so all three are per view by
construction. The camera is per view only across *frames*: within a group it is shared (§4).

**What is shared and why.** The mask is the principal's, not the view's; a filter is an
expression over entity-space attributes and means the same thing in every view (a group-scoped
attribute is the exception, §6.8); colour, palette, layer choice and the point budget are the
user's; an item's detail already carries every reachable view's position, so one fetch serves
every view; the described-record cache is per id. **The session artifact table is one**, not
one per view: it is the ordinal registry behind the map's single lookup texture, keyed
`(layer, tessera_id)`, and an ordinal indexes a colour rather than a position — an artifact
identity served in two views takes one ordinal and one colour, which is right. (r1 said one table
per view; that was the channel, corrected here.)

**Shared state reaches a view when it becomes current.** `setLayers`, `setColourBy`, `setBudget`
and `setPalette` write the shared field and apply it to the current view's machinery, as today. A
held, non-current view is brought up to date at the switch that makes it current — its channel
told the layers, its presenter the budget — rather than being told at every change, because a
change applied to a non-current view would have it *ask*, and rule 2 forbids that. **When a view
stops being current its presenter and channel are cancelled**: whatever was in flight is dropped,
and nothing is issued until it is current again. A slider stepping through five views therefore
issues requests for the fifth alone (§4).

**The cache.** Every view's replica holds its own bands; what changes is that the byte budget
is one figure for the store and the victim is chosen least-recently-drawn across every view, with
the protected rectangle applying to the evicting view alone. A view the user left an hour ago
yields its bytes to the view they are in, and a view they flick back to is as warm as their last
visit left it. Two constructions satisfy this: a view axis on the band key inside one `BandCache`,
or one `BandCache` per replica sharing a budget accountant that can evict from any of them. The
second is preferred — the per-depth indexes and every band-key site stay as they are, and the
accountant is a small object — but either is acceptable if it keeps the two invariants: one
number bounds the total, and eviction never touches the current view's drawn rectangle. No
neighbour pinning and no velocity model: those buy latency hiding at the cost of a heuristic that
is wrong whenever the user does something other than what it guessed, and the transition (§5)
hides the same latency without guessing.

**What a switch publishes, immediately.** The `view` projection with the new id, empty marks and
tiles, the incoming view's own artifact state (empty for a cold view, its held served set for a
warm one — never the outgoing view's), and `status` at `loading`, so a component never draws view
A's marks or artifacts under view B's frame. Where the incoming view's replica already holds the
current camera's tiles, that composition follows **on the next scheduler tick** — a redraw from
the held bands, no request — rather than synchronously, because the presenter coalesces per tick
and a switch that landed a frame inside its caller would be the one place it did not. The old marks stay on screen only by the transition (§5), which is not built; until it is,
a cold switch draws nothing for the length of one request, and the status strip says so.

**The `replica` projection** reports the whole cache's bytes (the figure the budget bounds), the
current view's points and bands, and `views`, the number of views holding any band — which is
what a default budget will be measured against (§7).

> **Built 2026-09-01** (V1, `client/view-switching-store`, refereed): everything in this section,
> with two things the building settled — the re-schedule at a within-group switch sits behind a
> store-level settle (§4), and a warm view's held composition is published one scheduler tick
> after the switch, from its own bands and with no request. The budget is one `BandBudget`
> shared by one `BandCache` per replica; eviction offers every non-current view's bands before
> the current view's, then the existing order within each — view-first rather than one merged
> cross-view ordering, which is a choice recorded at the code. The prefetch ring now measures its
> fullness against the whole store's held bytes, a consequence of one budget that §7 did not
> state. A filter or selection change resets every held view's replica as well as the current
> one's, since their bands answer the previous question.

## 4. A switch, in the two cases that differ

**Within a group — the slider.** Every view of a group shares one frame (`views.md` §3.1), so the
Morton addresses, the depth and the camera are the same in view *k* and view *k+1*. A step is:
set the current view; re-schedule the last camera on the new view's presenter **behind a
store-level settle of the driver's own debounce length**; draw what the cache holds meanwhile.
The settle is the store's and not the driver's because the driver fires a leading-edge request
for a camera arriving from stillness — which every stepped-to view is — so scheduling the incoming
presenter directly would put one request on the wire per step (found at V1). The basemap does not
change, the camera does not move, the selection stays — a shape in the shared frame means the
same region — and its counts are re-asked under the new view. A slider moved rapidly issues one
request per settle, not one per step: each passed-over view's presenter is scheduled and then
cancelled before its debounce fires, so a step the slider passes over costs nothing, by the
no-precaching rule.

**Between frames — a plain view, or a group whose layout differs.** The arXiv rung's `knn` and
`pca64` views quantise against different extents; a geographic view against a Mercator one
differs in kind; a `members` group is a second frame over the same keys (`views.md` §3.3). A
switch drops the selection — a selection is a shape in one frame's data coordinates — and
publishes the new view with no camera. The map, which owns the camera, sees the frame change
under `view.id`, refits to the new view's extent and issues the `setView` that asks for the
viewport the refitted camera covers; the host re-decides the basemap from the new view's
`tileScheme` (a scheme to `null` means the basemap goes). Hover does not survive. An open item
does — its detail already holds its position in the new view — and its card says which view it
is being looked at in (§6.4).

**The depth across a group switch — decided at r2: the budget chooses per view, as it does
today, and the depth is not pinned.** r1 left this open on the worry that a bbox sparse in one
quarter and dense in the next would draw deep then shallow with the camera fixed, and read as a
fault. It does not, for three reasons. The drawn count is bounded by the point budget in both
views, so what the user sees is a comparable number of marks either way; the depth is the tile
granularity, which is never on screen (the grid is never shown — owner direction, 2026-08-24);
and the sample in the next view is a different set of rows at *any* depth, because the rows in a
tile are the rows the next view placed there, so pinning would buy no continuity of marks. It
would cost a driver knob and, in the denser view, fewer marks than the budget allows. Declined.

**Gate-filtered views.** `meta.views` is already per principal, so a picker built from it is right
for whoever holds the session. A principal switch can remove the current view; the fallback is
explicit — the first view of the same group, else `meta.views[0]` — and never an accident, and
`clear()` resets every held view rather than the current one alone, since a mask change
invalidates all of them.

## 5. Animation

**The join.** Two held replicas both carry a `tessera_id` per mark, so the marks drawn in both
views are a hash match over the two id sets: those animate from one position to the other, those
in one alone fade in or out. Nothing crosses the trust boundary that the server did not serve under
the same principal in both views — an entity the gate withholds in one view is simply absent from
that replica. Within a group the two frames are identical, so the interpolation is in one data
space; between frames it runs in screen space after the refit.

**Why the join is not enough zoomed in.** At the top level the two held sets are the world, and
the join is complete. Zoomed in, the point of a group is that an entity is *somewhere else* in the
next view — usually off-screen — so the intersection of two viewports' contents is whatever
happened to stay nearby, and everything else pops rather than moves. Honest animation needs the
destination of every mark currently drawn, wherever it lands.

**The verb.** `POST /v1/positions` — *for these ids, where does view B put them* — taking the
current view's drawn ids (bounded by the point budget) and answering each id's grid position in B
or its absence, gate-filtered per view exactly as `ItemDetail.views` is (contracts §3.2). It is
that predicate at scale rather than a new one: an id the caller names is one it already holds
from a viewport under this session, and the answer for a view the principal cannot reach is
absence, indistinguishable from an entity not in that view. **It wants a leak-register entry**
(architecture Appendix C), and the natural form is a widening of **C30** — the row that already
covers an item's reachable views and their positions on the drill-down — from one id to a list,
on the same argument: every position served is the quantity a viewport of that view already ships
for a point the principal is already being served. It sits behind the same compute-admission gate
as any viewer verb. The request is small — ids and nothing else — so it returns before the
destination viewport does, and the marks are moving while the arrivals are still in flight.

**⊘ Specified, not implemented, and waiting on §10 (a).** No such verb exists; today the only
bulk answer is a viewport in the other view, which says nothing about where the *drawn* marks
went. The deck layer draws from band buffers and has no per-id interpolation; the transition
needs positions keyed by entity across the two datasets, which the join supplies and the layer
does not yet take.

**Under a fast slider**, animation runs only between the view last settled on and the one settling
now; if the slider outruns the requests, the drawn frame snaps.

## 6. The two pickers, and what a group asks of the rest of the client

Owner direction 2026-09-01: two drop-downs, one between view groups and one between the views of
a group. r1's slider is replaced by a `<select>` with previous and next beside it, which is the
slider's behaviour under a held arrow key and a list's when opened.

### 6.1 `<tessera-view-picker>` — which layout

One entry per **plain view** (its `displayName`) and one per **group** (`title`, else `name`),
in `/v1/meta`'s serving order — plain views first, then the groups. An owner group and the
`members` group laid over its keys are two entries, because they are two layouts (an embedding and
a map of the same quarters), and choosing between them is the *layout toggle* r1 asked for.

Choosing a plain view calls `setCurrentView(view)`. Choosing a group calls it with the group's
view **under the key the user is already on**, where the current view is in a group sharing that
key set (`membersOf` either way — creating a key on the owner creates it on every sharer,
`views.md` §3.3); else the key this picker last left that group on, held in the element; else the
group's first view in creation order. The user's key survives a layout toggle, and a group is
entered where it was left.

Hidden — it renders nothing, not an empty select — when the bundle offers exactly one entry:
one plain view and no groups, which is every demo corpus today. The explorer then looks exactly as
it does now.

### 6.2 `<tessera-key-picker>` — which view of the group

For the current view's group: a `<select>` over the group's views **in creation order, never by
interpreting keys** (`views.md` §3.2, decision 0113), and two buttons, previous and next, that
are `stepView(meta, current, ∓1)` — disabled at the ends, never wrapping. Left and right arrow
keys on the focused select are the native behaviour and are the slider: holding one steps through
the roster at key-repeat rate, and §4 makes that cost one request.

**A view's label** is its roster metadata where the group declared any, else its key. The rule
reads metadata *names* by convention, which is the only interpretation available to a client, and
is stated so two hosts agree: a text-typed value named `label` or `title`; else a
`timestamp_us`-typed `starts`, drawn as a date and, with an `ends`, as a range; else the key.
Where a label is drawn the key follows it, muted, as the filter's value rows draw a title and its
key — the key is the address a link or a request will carry, and a user should be able to read it
off the screen. A `members` group's views carry no metadata of their own (`views.md` §3.3); their
labels are the owning group's, found through `membersOf`.

Hidden when the current view is plain — a plain view is in no group and has no neighbours.
Its caption is the group's **`name`** — the key's namespace, `quarter` in `quarter:2026-Q3` —
not its title: the layout picker above already shows the title, and the same words twice read
as a fault on the first screenshots (ruled 2026-09-01).

**The rules live in the client, not the element** (§6.7): the label rule, the entry list and its
order, which view a group is entered on, and whether the view set is trivial are pure functions
in `@tesseradb/client` beside `viewsOfGroup` and `stepView`, and the elements render their
answers. A host building its own picker, and the React wrapper, get the same behaviour without
re-deriving it.

### 6.3 Where they sit

Both go at the top of the explorer's **`toolbar`** slot, above *Colour by* and *Layers*, in
docked and overlay layouts alike, and in the narrow layout's *Layers* sheet, which is where the
toolbar already goes. The board is `Views.dc.html` in the client-components canvas generator
(`docs/evidence/mockups/client-components/build.py`, added at r2): the toolbar panel with the two
selects and the step buttons, drawn once for a grouped corpus and once for two plain views, in the
boards' own select and button chrome. The pickers are accepted against it, side by side.

Both emit **`tessera-viewswitch`** `{from, to, sameFrame}` (ids as strings), bubbling and composed
as every event of §5.7 is, so a host that owns the URL or a basemap can react without reading the
store. The map does not listen for it: the map reacts to the `view` projection, so a host calling
`setCurrentView` directly gets the same refit.

### 6.4 The item card follows an item

`<tessera-item-card>` renders **`ItemDetail.views`** as a row of chips under the title, one per
reachable view, the current one marked. Clicking another switches to it and centres the camera on
the item's position there — `x`, `y` are that view's own grid units, dequantised under that
view's frame — so *follow this item into the other view* is one click and needs no request: the
detail already holds every position. It emits `tessera-viewfollow` `{view, x, y}` (data
coordinates) and the explorer handles it by switching, waiting for the frame, and centring. The
card also draws `labels` as chips where the array is non-empty — captioned as the labels the
viewer *holds*, never as the item's label set (decision 0114) — and `scoped` values as a second
group of rows headed by the key, in key order as served. Across frames a follow costs two viewport
requests, the map's refit and then the centring; within a group it costs one.

### 6.5 The viewer: URL state and the basemap

The current view is URL state — `?view=<id>` beside `?dataset=` — read at activation and written
on every switch with `history.replaceState`, so a link means what it showed. An id the bundle does
not declare falls back to `meta.views[0]` and is reported in the failures panel. The basemap is
decided **per switch** from the new view's `tileScheme`: `installBasemap` and the settle-following
recomposition are rebound to the current view, and the three `meta.views[0]` sites go.

### 6.6 The widget

`change:view` calls `setCurrentView` on every mounted view's store instead of disposing and
rebuilding them, and the current view id is up-synced with the other traitlets at the settle, so
`m.view` reads what the map shows. `Map(url, view=…)` keeps its meaning as the initial view.

### 6.7 Reuse and restyling

Owner direction 2026-09-01: a consuming system must be able to take these up easily and restyle
them. The design meets that on the client-components ladder (§5.6 there) and nowhere else:

- **Headless first.** The switch is `setCurrentView` on the store and the current view is
  `view.id`, so a host with its own controls, a React application or the notebook switches
  without our elements. The four rules the pickers embody — `viewLabel`, `viewPickerEntries`,
  `enterGroup`, `isTrivialViewSet` — are exported from `@tesseradb/client` as pure functions with
  their own tests; the elements are thin renderers over them.
- **Placement.** Both pickers are Tier 2 panels: they read the store by context, sit in the
  explorer's `toolbar` slot as default content, and are replaced by putting anything else in that
  slot. Both are wrapped in `@tesseradb/react` as every other element is.
- **Restyling.** Every element exposes parts in the existing convention — `title`, `select`,
  `step` (with `data-direction`), `label`, `key`, `entry` — and uses only the existing
  `--tessera-*` tokens, no new ones, so a host that has restyled the legend's `::part(select)`
  gets these for free.
- **The select is the platform's.** A native `<select>` is keyboard-native, accessible, and its
  held arrow key is the slider for free; the price is that the open list is the browser's and
  only the closed trigger restyles. A host that needs a styled list replaces the element through
  the slot and calls the same four functions. A custom listbox was considered and not taken —
  it is a component to own and keep accessible, for a gain in styling alone. Owner leaning, not
  ruled.
- **Routing stays the host's.** `?view=` is the viewer's, not the components'; a host that owns
  its URL listens for `tessera-viewswitch` and writes what it likes.

### 6.8 Group-scoped attributes — ⊘ not needed by any corpus in the tree

A filter over a scoped family means *this view's column*: under a view of the family's group the
leaf goes bare and the request's view decides; under any other view it must pin a key
(`family@key`) or the family is hidden. The client's draft already composes a pinned leaf
verbatim (`filters.ts`, `scopedFilters.test.ts`); what is missing is the panel's rule for when to
offer a key and the `/v1/categories` request naming the view it asks under. None of this is
needed for a two-view corpus with entity-scoped attributes alone, which is every corpus in the
tree today; it is needed the day a group with a scoped family is served, and `test_corpora/multiview`
already declares four such families to build against.

## 7. Cost

Per warm view: a replica at the point budget, a presenter and a channel. The cache budget bounds
the total bytes, and the active view's share is the whole point budget by rule; what the budget
buys in warm views is measurable from the `replica` projection's `bytes` and `views`.

A switch within a group at the top level: one request, the same tiles at the same depth. A switch
across frames: a refit and the viewport it covers — the same cost as opening the view.

The animation join is a map over the drawn ids — the point budget's size — built once per switch,
not per frame. The `positions` request is ids only: at 500,000 drawn marks it is a few megabytes
up and the same down, small beside the viewport it runs ahead of.

⊘ Neither figure is measured. The default byte budget stays at the store's 512 MiB, now for every
view together; measure a coarse group switch and a zoomed one on `test_corpora/multiview` and the
two-view arXiv rung before choosing another.

## 8. What this design does not do

- **No prefetching of views**, on slider movement or otherwise (owner direction). Warmth is what
  the user did, not what a model guessed.
- **No request from a view that is not current.** A held view is inert until it is shown again.
- **No cross-frame selection.** A selection is a shape in one frame; a switch across frames drops
  it. Within a group it is kept.
- **No cross-view artifact identity in the client.** A layer named on several views serves its
  artifacts per row space; the client reads each view's channel and does not try to match them.
- **No change to what a viewport carries.** Positions in another view come from the `positions`
  verb, not from a second column on the viewport.
- **No interpretation of keys.** Order is creation order; a label is metadata or the key.

## 9. What lands, and what proves it

Three steps, tracked in [`../client-delivery.md`](../client-delivery.md) as the client work is.
The first two are independent of §10 and build together — the components against the fake store,
the store against its tests — and integrate on the multi-view fixture.

| step | what lands | proved by |
|---|---|---|
| V1 | `@tesseradb/client`: `setCurrentView`, `view.id`, the per-view replica/presenter/channel with the shared table, the one byte budget with cross-view LRU, cancel-on-leave, the immediate publish, the queued call before meta, `clear()` over every view, `replica.views`; the `/v1/categories` request naming the current view | store tests: a switch within a group reuses the camera and keeps the selection; across frames publishes no camera and drops it; a passed-over view issues no request; a non-current view issues none on `setLayers`; eviction crosses views under one budget and never the current view's rectangle; an unknown id is ignored and traced |
| V2 | `@tesseradb/components`: the two pickers, the explorer's toolbar, the map's refit on a frame change, the item card's views/labels/scoped, `tessera-viewswitch` and `tessera-viewfollow`; the viewer's `?view=` and per-switch basemap; the widget's `change:view` | component tests against the fake store (entries and order, hidden-when-trivial, the label rule, the step buttons at the ends, the key kept across a layout toggle, the chips); side-by-side screenshots against `Views.dc.html`; a smoke on `test_corpora/multiview` served locally — every view of every group reachable from the pickers, a switch within `quarter` keeping the camera, a switch to `world` refitting |
| V3 | the transition: the join, per-id interpolation in the deck layer, and — under §10 (a) — the `positions` verb with its register entry | the join's tests; a recorded switch on the two-view arXiv rung |

## 10. Decisions for the owner

- **(a) The `positions` verb.** Add `POST /v1/positions` as §5 describes, with C30 widened from
  one id to a list on the same argument; **or (b)** no verb — animate the join alone, complete at
  the top level and fading where a mark leaves the viewport; **or (c)** defer the transition
  until the layer is free of the in-flight hover and halo work. V1 and V2 do not wait on this.
  Recommended: (a), taken up with V3.
- **The depth across a group switch** — decided at r2 as *not pinned* (§4), for reasons stated
  there; recorded here so it can be overruled in a letter rather than rediscovered.

## Appendix R — review trail

- **r1, 2026-09-01.** Drafted from an investigation of the client's handling of views, under the
  owner's four directions in the status line. Unreviewed. Open: the depth rule across a group
  switch (§4); the `positions` verb's leak-register row (§5); the cache budget's default (§7).
- **r2, 2026-09-01.** Fleshed out to build against, under a fifth owner direction: the picker is
  two drop-downs. The store's operation, projections and rules are named (§3): a non-current view
  has no request in flight; shared state reaches a view when it becomes current; the session
  artifact table is one, correcting r1's *one table per view*. The depth question is decided —
  not pinned — with its reasons (§4). The pickers are specified with their entries, order, label
  rule, hiding rule and events (§6.1–§6.3), and the item card, viewer and widget follow (§6.4–§6.6).
  Found while reading the code: `viewsOfGroup` and `stepView` already exist, tested and uncalled;
  `scopedScalars` is named in a comment and declared nowhere; the pinned leaf already composes.
  The steps are laid out (§9) and one ruling is put (§10). The board `Views.dc.html` is added to
  the canvas generator in the same change. Unreviewed beyond the author. **Same day, owner
  question on reuse and restyling:** §6.7 added — the rules move into `@tesseradb/client` as pure
  functions, the parts and the token rule are named, React wrappers owed, the native select kept
  as a leaning.
