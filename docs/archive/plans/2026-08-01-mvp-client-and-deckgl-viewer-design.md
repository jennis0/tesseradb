> **ARCHIVED 2026-08-01 — SUPERSEDED by the epic model. Approved by the owner 2026-08-01 and implemented; remaining work is tracked in GitHub issues.**
>
> Kept for its reasoning and its record, not as an instruction. Plans are no longer a
> maintained artifact in this repo: design rationale lives in `docs/design/`, decisions in
> `docs/decisions/`, and work status in GitHub issues. Do not execute this document.

# MVP client and deck.gl viewer — an artifact you can point at a running database

**Date:** 2026-08-01
**Status:** design, approved by the owner 2026-08-01
**Parent:** `client-interaction.md`, which owns the client
architecture. This document is the first slice of it, cut to one purpose.
**Touches:** client-interaction §8.2, §9, §10, §13, §15; contracts §3.2, §5; SA §7.

---

## 1. What this is for, and what it is not

**Purpose, stated by the owner 2026-08-01:** *"The goal here is not security, conformance,
or an actual client that we ship — it's to have an artifact we can actually run against a
running DB in order for me to validate that it works."*

So the deliverable is an **instrument**, not a product. It earns its place by making four
things visible on a screen, which are the owner's own success criteria:

1. **Marks land where they should.** The tile index arithmetic is right — `OrthographicView`
   plus `TileLayer`, y-axis orientation, zoom→depth mapping, non-square extents.
2. **Counts are visibly honest.** Masked `visible`/`matched`/`served` come from the number
   channel and are displayed against the drawn sample, never derived from it.
3. **Masking is visibly real.** Switching principal changes the map, by eye.
4. **It feels interactive at scale.** Pan and zoom against a large fixture without stalling.

It **will** become the real client — that is why §2's package split exists rather than a
single throwaway app — but nothing here claims conformance, and §7 enumerates what is
deliberately missing so that absence is a recorded decision rather than an oversight.

This slice also discharges the two items client-interaction §13 places *outside* its mode
gradient because they gate rather than deliver: the orthographic spike (§8.2) is Task 1
here, and the arithmetic it settles is client-interaction §15's first open question.

## 2. Shape

```
clients/ts/
  core/     @tessera/client — the four verbs, the framed-Arrow decoder,
            the interleave pass, tile↔bbox arithmetic. No state.
  viewer/   Vite + TypeScript + deck.gl. All UI, all state.
crates/tessera-server/   one dev-only CORS configuration key
```

Two packages, not one and not three (owner decision, 2026-08-01). One would make
client-interaction §10's load-bearing seam a later refactor; three would mean building the
replica store — epochs, reconciliation, prefix declarations — which is exactly the
machinery an instrument should not carry.

**`core` holds what a stranger gets wrong**, which §8.2 measured: the framed Arrow payload
(`u32` LE length prefix, tile stream, points stream, then the sub-cell stream **absent
rather than empty** when the underlay was not requested — see `tessera-wire`'s `payload`
module doc), and the x/y→interleaved `Float32Array` pass that `getPosition` requires
because we ship separate `x` and `y` columns. It exposes:

| Call | Route |
|---|---|
| `authorise(terms: string[])` | `POST /session/authorise`, body `{"auth_data": "<base64 of {\"terms\": [...]}>"}` — the passthrough plugin's shape — under the session-credential bearer |
| `meta()` | `GET /v1/meta` |
| `viewport(req)` | `POST /v1/viewport` |
| `item(tesseraId)` | `POST /v1/items/{tessera_id}` |

It holds no cache, no epoch, no session lifetime, and no replica state. The replica store of
client-interaction §10 is **inserted between** the two packages later.

**`viewer` owns everything stateful**, including the tile cache — which it gets from
`TileLayer` for free, session-scoped by construction (§8.2), so no cross-viewer fault line
arises.

## 3. The server change, held to one key

`tessera.toml` gains one key under `[serve]`:

```toml
dev_cors_origins = ["http://localhost:5173"]
```

It installs a `tower-http` `CorsLayer` on the **viewer and session** listeners: allows the
`Authorization` request header, and exposes `x-tessera-pin` and `x-tessera-stage-ns` (the
stats readout of §5 reads the latter). The control plane is untouched.

Three rules on it:

- **Absent means off.** No default, no environment variable, no wildcard origin accepted.
  This follows `config.rs`'s established discipline — refuse rather than clamp, and never
  let a typo silently enable a mechanism.
- **It logs loudly at startup** when enabled, naming itself dev-only.
- **That is the entire diff.** Nothing in the engine, the request path, the wire format or
  the selection machinery moves for this document.

**The owner chose browser-direct over a Vite dev proxy** (2026-08-01), having been told the
cost: `POST /session/authorise` is gated by the operator-configured *session credential*, so
browser-direct authorisation puts that credential in the browser. That is accepted for a
dev instrument, and the off-by-default key is what stops the shape riding into a deployment.
It is **not** the recommended integration topology — client-interaction §7 makes T2 with
verified assertions the documented default, and nothing here revises that.

## 4. The map

### 4.1 Cell space is the world coordinate system

The engine's world is the 2¹⁶ × 2¹⁶ Morton cell grid, and §2.5 quantises each axis
**independently** onto it. A tile is therefore square in cell space and rectangular in data
space. The viewer works in **cell space** as its deck.gl world coordinates and converts to a
data-space bbox only at the request boundary, using the `quantisation` extent from
`GET /v1/meta`. That is simultaneously the answer to client-interaction §8.2's non-square-extent
item — `TileLayer` takes a scalar `tileSize`, and in cell space it does not need an
anisotropic one.

### 4.2 Task 1 is the spike, and it contains no Tessera

Per §8.2, the smallest discharging experiment is ~50 lines: `OrthographicView` plus
`TileLayer` with a synthetic `getTileData` that draws each tile's own index and bbox.
It asserts:

- y-axis direction, and the zoom→`z` mapping;
- index arithmetic at z 0–16 against the 2¹⁶ grid;
- abort on fast pan, and that aborted tiles do not enter the cache;
- cache behaviour and eviction under a viewport sweep.

It runs **first and alone**, because a failure changes everything downstream and would
reopen a decision the visualisation architecture treats as settled. Non-geographic tiled
deck.gl is not exotic — Viv and Vitessce ship it daily at gigapixel scale — but they use
their own multiscale layer rather than `TileLayer`, so what is unverified is our arithmetic
against *that* layer.

### 4.3 Per-tile fetch

`getTileData({index: {x, y, z}, bbox, signal})` → `POST /v1/viewport` with
`{slice, zoom: z, bbox: <data space>, k, underlay_offset}`. The `signal` is forwarded to
`fetch`, and an abort **throws** so nothing incomplete is cached — deck.gl's cancellation
contract is explicitly fail-closed and it costs nothing to honour.

`refinementStrategy: 'best-available'` is correct for us rather than merely tolerable:
§7.2's nesting makes every child a superset of its parent's marks, so showing a parent while
children load is add-only and does not pop. The rejected pre-r22 rank-position sampler would
have popped on every refinement.

### 4.4 Two sublayers, one TileLayer

- **Underlay:** sub-cell counts rasterised into a 2^`offset` × 2^`offset` image drawn by a
  `BitmapLayer` over the tile's bbox, coloured client-side by **histogram equalisation**
  (datashader's `eq_hist`, which client-interaction §9 recommends over a fixed log transfer
  and which is safely client-side because it is computed from counts the principal was
  served).
- **Marks:** a `ScatterplotLayer` over binary attributes — `data.attributes.getPosition`
  fed the interleaved `Float32Array` from `core`.

Both live in **one** `TileLayer`, with a translucent underlay and depth test off. That is
§8.2's own mitigation for arbitrary cross-tile draw order, and it halves the request count
against two parallel `TileLayer`s.

### 4.5 Picking

`ScatterplotLayer` picking returns a **positional index**, resolved against that tile's
`BigUint64Array` of `tessera_id`s — so identity never enters the render path, and there is
no u64 problem to solve here (§8.2). Because `TileLayer` overrides its sublayers'
`highlightedObjectIndex`, the selection highlight is a **separate overlay layer**, not a
highlight prop on the scatterplot.

### 4.6 Invalidation, crudely

`k`, the term set, the slice and the underlay offset compose the `TileLayer`'s identity.
Changing any of them mints a new layer id, which drops the whole cache. Crude and correct;
it is precisely the place the replica store lands later.

## 5. The panels

**Counts.** Summed `visible` / `matched` / `served` over loaded tiles at the **current
integer zoom** whose bbox intersects the viewport. Parent tiles retained for
`best-available` refinement are excluded, or they double-count against their own children.
Presented as *served shown of visible*, never a bare drawn-mark count — the sample must not
be able to read as the set (P2).

**Principal.** A dropdown of pre-baked term sets. A one-off node script authorises each
candidate term and issues a `zoom = 0` full-extent viewport call, whose `visible` is that
principal's exact visible-set size; it emits `presets.json` with narrow / medium / broad
labels derived from **measured** numbers rather than guessed ones. The fixture dictionary is
numeric term ids (`"0"`…~`"170"`) with skewed sizes, which is what makes the spread
available. Selecting a preset re-authorises and remounts the layer.

**Item.** Click a mark → `POST /v1/items/{tessera_id}` → side panel. This exercises the
identity round-trip and the picking arithmetic together.

**Stats.** Per-request wall time and byte count, the `x-tessera-stage-ns` breakdown, tiles
in flight, cache occupancy, and points drawn. Plus a live `k` control — `core` omits `k`
entirely unless the slider is touched, so the deployment's own ceiling is the default
exactly as contracts §3.2 intends.

**Errors.** A failed tile renders as a **marked failure**, never as an empty region. The
full four-state treatment is out of scope (§7), but this one line stays: an empty viewport
and a failed viewport are semantic opposites, zero versus unknown, and collapsing them is
the cheapest way to convert fail-closed into fail-misleading.

## 6. Testing

- **`core`:** unit tests for the framed-Arrow decoder against a golden payload captured from
  the running server (including the underlay-absent case, which must be byte-identical to a
  pre-underlay payload); property tests for tile↔bbox arithmetic round-tripping through cell
  space at every depth 0–16.
- **The spike (Task 1):** its assertions are the test, and they are kept as a runnable
  artifact rather than deleted once green — they are the regression guard for a deck.gl
  upgrade changing tile indexing under us.
- **`tessera-server`:** one test that the CORS key absent means no CORS layer, and one that
  the configured origin round-trips including the exposed headers.
- **`viewer`:** no end-to-end tests. It is the instrument, and the owner reading it is the
  assertion.

## 7. Deliberately absent

Recorded so each absence is a decision rather than a gap:

- **Epochs and cross-channel consistency** (client-interaction §2, §6). The bundle is static
  for this exercise; the counts panel sums responses fetched at different times, which is
  wrong the moment ingest runs and is accepted until it does.
- **Reconciliation, prefix declarations, the session cursor** (§5, §5.1).
- **The change signal** and the refresh affordance §4 makes mandatory in the conformance
  sense.
- **`{shown, total}` as an inseparable type** (P2's mechanical half). The discipline is
  honoured in the panel; it is not yet enforced by the type system, so a future panel can
  still render a bare sample count.
- **The four display states.** Only the failure case survives, per §5.
- **The *k*-non-decreasing rule** (P6). The slider can decrease *k*, which is a real
  deviation and survives only until the replica store owns *k*.
- **The conformance kit, the obligations list, the tile-addressed GET alias, labels,
  filters, export, and the Python client.**

## 8. Sequencing

1. **The orthographic spike** — no Tessera in it. Gates everything.
2. **`serve.dev_cors_origins`** — one key, two tests.
3. **`core`** — four verbs, the framed-Arrow decoder, the interleave pass, cell↔data
   arithmetic, with its unit tests.
4. **The viewer's map** — `OrthographicView`, `TileLayer`, marks only, against the 109 MB
   `2m4` fixture.
5. **Panels** — counts, then principal presets (with the measuring script), then stats
   and `k`.
6. **Underlay and picking** — the two features that are additive once the map is real.
7. **Scale pass** — repoint at `1e8`, then `1e9`, and see whether it still feels
   interactive.

## 9. Provenance

Brainstormed with the owner 2026-08-01, against
`client-interaction.md`. Owner rulings recorded in place: all
four success criteria are in scope (§1); two packages rather than one or three (§2); CORS on
the server rather than a Vite dev proxy, with the session-credential cost stated and accepted
(§3); pre-baked term-set presets rather than pasted tokens or a live credential box (§5); and
underlay, item detail and the `k`/stats readout in, the four display states out (§5, §7).

The server surface this targets was read from the code on 2026-08-01, not from the contracts
document: `crates/tessera-server/src/{session,viewer}.rs` and
`crates/tessera-wire/src/payload.rs`. Where this document and the contracts spec disagree,
the contracts spec governs and this document is wrong.
