# Tile-addressed integration — serving MapLibre, OpenLayers and QGIS

**Date:** 2026-08-01
**Status:** Provisional r2 — under review. Graduated into the corpus 2026-08-01. r2 applies decision 0029: the adapter's cache validity is the **view key** — see §9. **To become normative:** owner sign-off, and reconciliation against `caching.md`'s S5 tier, which is this document's cache.
**Companion to** `client-interaction.md` (§8.3's MVT adapter, §8.6's
tile-addressed alias) and `caching.md`, whose **S5** is this
document's cache.
**Touches:** client-interaction §5, §8.1–8.6, §10; design §7.1–7.3, §11.

---

## 1. The result

Every mainstream visualisation tool is **tile-addressed**; Tessera's verb is
**viewport-addressed**. Adapting the first to the second naively multiplies request count by tile
count — measured: **one browser tab shed 12 of 23 requests with `429 backpressure`** against the
10⁹ fixture.

**Build a stateful, session-scoped, coalescing adapter inside the trust boundary**, and ship §8.6's
tile-addressed alias as a *facade over it* rather than as a direct-to-engine surface. Decline the
multi-tile request shape and the HTTP/2 mitigation; neither addresses the actual cause.

## 2. The reframe the option list depends on

"Request count versus multiplexing" is the wrong axis. The binding resource is **visible-set
touches per interaction**: at 10⁹ a depth-0 tile costs 1,270 ms *because its cost is the
5.2 × 10⁸-item visible set*, while depth 6 over the same extent costs 227 ms wall — cost tracks the
set touched, not the tiles asked for.

**Tile-addressing hurts twice, and the two hurts have different fixes.**

1. **It multiplies evaluations.** Each of `TileLayer`'s six concurrent fetches re-enters the engine
   and re-touches its share of the visible set. *Fix: coalescing.*
2. **It couples requested depth to viewport zoom.** A `z=0` URL demands the intrinsically expensive
   shallow evaluation. *Fix: a depth floor.*

**Coalescing alone is only half a mitigation.** A perfectly batched request for one `z=0` tile still
costs 1.3 s, which busts any interactive budget at 10⁹ however well it batches. Any design that
batches without decoupling depth from URL zoom fails at scale.

## 3. The property that makes a good answer possible

**Nesting** (design §7.2): `served(T)` is a `tessera_id`-order prefix, children superset parents,
and `served` is deterministic in (mask, corpus state, *k*, viewport) (§11).

Therefore **one deep evaluation is a universal answer-store for every shallower tile inside its
extent.** Serving a shallow tile the marks of a depth-*d* evaluation is serving a *superset the
consumer could legitimately have requested* — no pops, no new capability.

This is the fourth place nesting does load-bearing work: §8.2 credits it for making
`best-available` refinement look right, §10's annotation for making depth-decoupling safe, the
caching design for making zoom-out nearly free, and here for letting a tile URL be a read interface
without ever being an evaluation trigger.

## 4. Options, ranked

### 4.1 Viewport-native path — necessary, not sufficient

The TS core's replica store owns depth choice and coalescing (client-interaction §10). Already
built: 9 requests per session instead of 31, 227 ms instead of self-DoS. **Its limit is that it does
not exist for MapLibre, OpenLayers or QGIS**, and §8's owner decision ships the MVT adapter — so a
tile-addressed answer is still owed.

### 4.2 **Build:** a stateful, session-scoped, coalescing adapter inside the trust boundary

The **view key**, which the adapter's cache validity is expressed against, is client-interaction
§6's composite of mask, overlay version, view, *k* and idset — the coordinate within which a
served viewport is stable. **The viewport is not one of its components**, which is exactly what
lets one cached evaluation answer every tile inside its extent.

`{z}/{x}/{y}` — MVT and the raw-Arrow alias alike — terminates at an adapter co-located with
`tessera-server`, which:

- maps the tile to an **effective depth** `d_eff = max(z + s, d_floor(V_total))`, where the floor
  scales with the principal's own visible count, which the session already holds;
- **single-flights** per (session, view key, covering region at a cohort depth): the first arriving
  tile triggers one internal viewport evaluation over the covering region at `d_eff`; the other five
  concurrent fetches join it. **The machinery exists** — `crates/tessera-authz/src/single_flight.rs`,
  built for mask builds; this is its second use;
- caches the evaluation keyed by **(grant set, overlay version, segments_version, view, content
  version, k)** — the view key spelled out, and §8.3's rule verbatim, mask identity alone being insufficient because suppressions live in
  the overlay — and serves each arriving tile as a **view** of it, which is arithmetic (tiles are
  contiguous Morton ranges), not selection;
- serves counts for the MVT cells layer from per-tile `range_cardinality`, which touches no data
  file (§2.6 step 6).

**Three rules that keep it clean:**

1. **Never derive a parent's served set from its children inside the adapter.** That would
   re-implement §7.2's floor/threshold/cap clauses outside the engine — the invariant-bearing
   arithmetic duplication §15's WASM-kernel question warns about, with silent disagreement against
   the differential oracle. Serve the deeper superset in the shallow tile instead; nesting makes it
   pop-free and I7 holds because it is literally the served set of a request the principal could
   make.
2. **Cache validity is the view key, checked per serve — never a TTL.** Denies publish
   immediately (contracts §2.3), so a content-version bump invalidates the cached evaluation before
   the next tile is served from it. This is what carries *"the server never serves it again"*
   (client-interaction §4, mechanism 1) **through** the adapter. TTLs are hygiene; the view-key
   check is the control.
3. **Fail-closed on abort.** A cancelled internal evaluation caches nothing and joined waiters get
   the error — matching deck.gl's own contract (§8.2: *"on abort… never return incomplete data"*).

### 4.3 **Re-scope, don't drop:** §8.6's GET alias

The §8.2 annotation says *"the alias is the shape that fails"*. More precisely: the **stateless,
zoom-mapped, direct-to-engine** alias fails. Tile-addressing is a fine *read interface over a cached
evaluation* and a self-DoS as an *evaluation trigger*.

Ship the alias only as a front on §4.2's machinery, and **refuse rather than clamp** shallow depths
above a `V_total` threshold when no cached evaluation covers them — the same discipline design §7.3
applies to underlay offsets, and for the same reason: a silently substituted depth hands back cells
the client cannot interpret. The alias keeps its virtues (five-line `getTileData`, no framing parse,
a correct browser-cache URL shape) at the scales where the annotation itself concedes a stranger's
naive path works.

### 4.4 **Decline:** a multi-tile request shape

`POST /v1/viewport` **is** the batch shape — bbox plus depth is a tile list with the list compressed
to a rectangle. A tile-list variant buys non-rectangular coverage only, adds a second selection
surface to keep conformant, and decisively **does not fix the depth coupling**, because
tile-addressed libraries would still populate the list at zoom-mapped depth.

### 4.5 **Decline:** HTTP/2 or HTTP/3 multiplexing

The problem is concurrent expensive evaluations, not connection or header overhead. Multiplexing
*removes* the browser's six-connection cap, so it makes the naive pattern **burstier** and hits the
admission gate harder. The compute-admission gate remains the correct backstop and was doing its job
in the 429 finding.

### 4.6 **Insufficient alone:** documentation-only steering

Necessary for mode 3 — the obligations list, OpenAPI, and stating the naive/coalescing partition
next to §8.1's resident/streaming one. But the MVT adapter is a shipped commitment and QGIS users
will never adopt the TS core, so docs without §4.2 leave a committed seam broken at scale.

## 5. The budget ceiling, which must be documented rather than discovered

From the caching design: at the 1–2 × 10⁶-mark target, a view spans **60–125 k tiles**. A
tile-addressed consumer fetching those individually is the §8.2 self-DoS squared.

**Tile-addressed consumers cannot reach the drawn-mark target, structurally.** They operate at their
own much lower per-view budget by construction — which is *why* S5's sizing is affordable. The
adapter's documentation must state that ceiling plainly. An integrator who discovers it by
measurement will reasonably conclude the product is slow.

## 6. Invariants and the leak register

- **I2 / I7.** The adapter computes nothing and views only; served sets are the engine's. Serving a
  deeper superset in a shallow tile is derivable under P3 — it is the answer to a request the
  principal could issue.
- **I10.** MVT feature ids are JS numbers in practice (§8.3), so `tessera_id` rides as a string
  property with **per-session ordinals minted inside the boundary** — legitimate precisely because
  the adapter is inside it. Anything carrying per-session handles (Phase 3 labels' `node_handle`)
  must never enter a *shared* cache entry.
- **The two retirement rules** (Rule S and Rule F, write-path §5.4). The adapter holds responses,
  not overlay state, and re-evaluates on any view-key change. It never interprets deny semantics.
- **A geometry stamp is advisory, never authorisation.** The cache key carries overlay version
  independently of the geometry stamp, so a suppression voids cached tiles whatever stamp the
  request carried — and the stamp selects no geometry in any case (`geometry-pinning.md` §7).
- **CDN posture, unchanged from §8.3.** Per-viewer masked tiles are intrinsically CDN-hostile;
  `Cache-Control: private`, view-key-scoped URL segments using a **session nonce and never the bearer
  token** (URLs reach history and proxies), max-age inside the deny-visibility budget.
- **C4 / P3 cost profile.** Cache hit-versus-miss timing discloses the session's own prior activity.
  Where entries are shared across sessions of one grant set, that becomes another session's
  activity, which needs the register entry the caching design's §11 names.

## 7. Judgement calls, flagged as such

- **Over-deep serving weakens mark-count-as-density at shallow MVT zooms.** A `z=0` tile carrying
  depth-6 marks no longer encodes density in its mark count (design §7.3). Acceptable, because the
  underlay is the density carrier at exactly those zooms and no consumer was going to read
  5 × 10⁸ visible items off mark counts at `z=0` — but it differs from a literal per-tile evaluation
  and belongs in the adapter's documentation.
- **`d_floor` as a function of `V_total`** is a new server-side heuristic with no spec home. It
  belongs beside the depth-choice arithmetic in design §7.2's 2026-08-01 annotation, and should be
  **measured rather than reasoned** — the −32%/+14% drift in the marks model across viewport
  fractions says that arithmetic has edges.
- **In-process rather than a separate proxy**, from state-sharing economics: the adapter needs the
  session's mask identity, view-key signal and cache anyway, and a separate process would re-create a
  trust-boundary hop for nothing. The cache key already makes later separation possible.

## 8. What was framed wrongly

Treating §8.6's alias and §8.2's 429 finding as contradicting each other. They do not — they are
about **two different roles of a tile URL**: a read interface, and an evaluation trigger. Nesting is
what lets one URL play the first without ever being permitted the second. The earlier framing
("the cheap adoption path and the scalable path point in opposite directions") was too pessimistic;
they point in the same direction once the alias is a facade rather than a front door.

## 9. Provenance

**r2 (2026-08-01) applies decision 0029.** What §4.2 called an
"epoch" is the **view key** — mask, overlay version, view, *k*, idset. The viewport is not one of
its components, which is what makes §3's answer-store property expressible at all: one cached
evaluation stays valid across every tile and every pan within a view key. §4.2's cache-key list
names the **content version** rather than the whole key, because the list already enumerates the
key's other components. No mechanism changes.

Produced 2026-08-01 by an independent agent briefed on the measured 429 finding, the viewport-cost
probes and the integration seams, with no stake in the existing plan. Its central contributions are
the two-hurts reframe of §2, the answer-store property of §3, and the read-interface/evaluation-
trigger distinction of §8. Nothing here is built; §5's ceiling and §4.2's cache are consistent with
the caching design's **S5** and should be sized with it.
