# Caching architecture — where data rests, and what that costs

**Date:** 2026-08-01
**Status:** Provisional r2 — under review. Graduated into the corpus 2026-08-01. r2 applies decision 0029: every cache key formerly written against an "epoch" is written against the **view key** — see §15. Already load-bearing: it supersedes Phase 2 of the archived underlay plan. **To become normative:** owner sign-off on the S1–S5/C1 tier model, and the eviction rules reconciled against `engine/src/single_flight.rs`, which implements a cache this document does not yet describe.
**Companion to** `client-interaction.md`, whose §3, §5, §5.1, §6.2
and §10 this makes concrete and in two places corrects.
**Touches:** design §7.1–7.3, §8.5, §10.4, §11; client-interaction §3–§6, §8.1–8.6, §10; SA §7, §13.

---

## 1. The one-paragraph result

**At the target operating point, caching is not an optimisation layered on a working system — it
is the feasibility mechanism.** A 1–2 × 10⁶-mark view at 10⁹ costs a measured **8–16 s of CPU** and
**16–32 MB of wire**. Recomputing that per interaction supports roughly **five concurrently active
broad principals per box**. Serving deltas against what the client already holds lifts that to
~10–40 and turns a 1.3–2.6 s transfer into a fraction of one. Everything below follows from that
inversion: the architecture is **delta-native**, and the full self-contained response is demoted to
bootstrap, view-key refresh and long-range jumps.

The naive path stays correct (P6) — it simply operates at a lower mark budget.

**The view key**, which every key below is expressed against, is client-interaction §6's
composite of **(mask, overlay version, slice, *k*, idset)** — the coordinate within which a served
viewport is stable. **The viewport is not one of its components**, so one view key covers every pan
and zoom a session performs; that is what makes it usable as a cache key at all.

## 2. Why this document exists

Caching decisions were being made piecemeal inside a narrower design (a per-session density map).
The owner stopped that: caching spans client, wire, engine and deployment sizing, and wants
deciding once.

**Two goals, in the owner's words:** reduce server load; reduce client latency on interaction.

## 3. The operating point, stated once

Every number below is against this, and it is the thing to change first if any of it is wrong.

| | |
|---|---|
| Target draw | **1–2 × 10⁶ marks on screen** |
| Corpus | 10⁹ items; a broad principal sees 5.2 × 10⁸ |
| Server | 32 GB RAM, ~24 GB indexes, **~10 GB for caching**, 14 compute threads |
| Users | 100–1000, and **grant sets must be assumed near-unique** |
| Per view | ~16–32 MB wire (16 B/point), 8–16 s CPU, 0.8–1.6 s wall |
| Session | ~600 settled views in 30 minutes |

Measured sources: `probes/2026-08-02-viewport-and-underlay/viewport_cost.md` (depth 8 at 10⁹
gathered 1,040,729 points in 820 ms wall / 8,372 ms CPU; depth 6 saturates at ~31 req/s from two
clients onward), `underlay_route.md`, and `config.rs`'s `MEASURED_PROJECTION_BYTES_AT_1E9`.

## 4. The sizing rule: active visible-set mass, not users

The instinct to divide 10 GB by user count is wrong, and the correction matters because it changes
what a deployment is sized against.

The dominant per-principal server memory is the **row projection**, and `config.rs` records it at
**125 MB per entry at 10⁹** — over any per-user budget before a single new cache exists. But that
is the *dense* bound. A Roaring projection is roughly **2 B × visible count**, so a principal
seeing 10⁶ costs ~2 MB and one seeing 10⁷ costs ~20 MB. Only broad principals approach the cap.

> **Σ over actively-querying principals of min(≈2 B × V_total, 125 MB) ≤ ~7 GB**
>
> where *actively querying* means "queried within the LRU residency window", not "has a session
> open" — an idle session needs no resident projection.

And there is a second term, which the CPU measurement forces:

> **active principals ≤ (compute threads) ÷ (CPU-seconds per interaction)**
>
> ≈ 5 at full redraw, ≈ 10–40 delta-native, on 14 threads at one view per 3 s.

**Under these, the owner's stated worst case is honestly answered:** 1000 unique users at median
10⁶–10⁷ visible fits comfortably. **100 concurrently active unique *broad* principals at 10⁹ does
not** — 12.5 GB of projections against 10 GB, and `config.rs`'s cache-bound note says this regime
*collapses* rather than degrades (miss/hit ratio 10⁵–10⁷, single-flight 429 storm). It is
unservable on memory and on CPU simultaneously, so no cache rescues it.

The levers, in order:

1. **Runtime admission control on distinct active masks**, metering both projection residency and
   compute. This is the runtime form of the startup relation `config.rs` already validates, and it
   converts silent collapse into explicit, sized refusal with `Retry-After`.
2. **Publish the sizing formula** as the deployment rule, so a box is sized against Σ active
   visible mass rather than user count.
3. *(Recorded, not proposed.)* A **smaller resident projection for broad masks** — projecting per
   touched Morton range rather than whole-slice. Engine design work against design §10.4; the only
   structural fix.

## 5. The caches

Seven, not one. They differ on every axis that matters, and the two most important — the client
point cache and the server density raster — could not sensibly share a design.

| # | Cache | Home | Key | Bound | Eviction | Invalidated by |
|---|---|---|---|---|---|---|
| S1 | Mask fragment *(exists)* | server | canonical grant set | `fragment_cache_bytes` | LRU | content-addressed; never |
| S2 | Row projection *(exists)* | server | (token, slice, segments_version) | `row_projection_cache_bytes` | LRU + `prune_generation` | compaction |
| S3 | Density raster | server | (grant set, slice, content version, depth) | new knob, ~512 MB | LRU | content version; rebuild async, serve stale-marked |
| S4 | Overview / bootstrap answers | server | (grant set, slice, content version, view, k) | new knob | LRU | content version |
| S5 | Adapter tiles *(class b only)* | server, in boundary | (grant set, overlay version, segments_version, slice, view-key nonce, z/x/y, k, encoding) | new knob, 1–2 GB | LRU + single-flight | content version; view-key-scoped URL self-busts browser copies |
| C1 | **Replica point bands** | client | (view key, tile prefix) → band up to cut *c* | `client_cache_bytes`, 512 MB–1 GB | **truncate cuts, deepest/least-recent/farthest first; never the floor prefix** | identity generation → all; content version → stale-mark, lazy refetch |
| C2 | Density raster | client | (view key, slice), deepest level only | 1–4 MB | replaced whole | content version |

Plus **the session cursor**, which is not a cache: `(tile, view key, cut)` triples, ~64 KB/session,
advisory and safely evictable under P5.

**S2's re-key is hygiene, not a lever.** The projection is pre-overlay — `EffectiveMask { base,
minus, plus }` applies the diff at query time — so it is a pure function of (grant set, slice,
segments_version) and could be shared by content address, exactly as S1 already is. Worth doing;
but under near-unique grant sets it saves nothing, so it must not be presented as the capacity
answer. **One real cost:** `revoke` calls `prune_token`, which works only because the key
*is* the token; sharing needs refcounting or drop-when-unreferenced.

## 6. Client caching, which is the latency mechanism

**Caching is how revisits become free, and exploration is revisit-dominated.** Zoom in then out and
you have thrown away everything you held; the round trip to get it back is the measured 200 ms–1.6 s
floor. A cache converts that to zero. The first visit to genuinely new territory is unavoidable, and
that is what the prefetch margin and covered-view check are for.

**Eviction runs continuously; it is not deferred work.** A 30-minute session accumulates
**2–6 GB** (600 views × 1–2 M × 20–30% novelty), against a client bound of 512 MB–1 GB. So the
cache holds ~8–30 views' worth and the latency win is delivered exactly to the extent the policy
keeps the right bands.

**The unit is the (tile, cut) band, not the point.** Because `served(T)` is a `tessera_id`-order
prefix and prefixes nest, points are not independent entries — a point served at depth 0 is served
again by every deeper fetch covering it. So:

- **Evict by truncating cuts**, deepest / least-recently-touched / farthest-from-viewport first.
- **Never evict a tile's floor prefix**, so overview rendering never blanks.
- This achieves the owner's "top-level points never get bumped" **without any per-point priority
  bookkeeping**, because coarse points are the low-id head of every band. A per-point LRU carries
  metadata over 10⁷ points and fights the nesting the sampler already paid for.

**Best-available rendering in both directions is free and unbuilt.** Zoom-out can draw held deeper
marks immediately (a superset, trimmed when the server answers); zoom-in draws the parent while
children stream. Two guard rails: this is **presentation, never selection** (client-interaction §10's
boundary — a client choosing what to *draw* is fine; a cache that becomes a second sampler is not),
and superset marks must not be read as density.

## 7. Delta-native serving

### 7.1 Split the mechanism by direction

The server-held cursor of client-interaction §5.1 and the client-sent prefix declarations of §5 are
**complements, not substitutes**, and §5.1 over-claimed by proposing the cursor for both halves.

- **Additions elide against per-request client declarations.** The request carries, per region in
  view, the cut it currently holds; the server serves the band between the declared cut and the new
  one.
- **Removals come from the server's own ledger** — the deny set's immediate-publication rule and
  the flush/stamp ledger. No client claim is load-bearing here, which is the security-relevant
  guarantee.

**Why additions cannot use the cursor.** The cursor records what the server *named*; the cache holds
what the client *kept*. A client that evicted a band the server then elides gets a silent hole. P5
does not protect this — it constrains the server ignoring *client* hints, whereas here the server
would be making a claim about client state. And after §6, eviction is *normal operation*, so this is
not an edge case.

**Per-request declarations make eviction protocol-free.** An evicted band is simply no longer
declared. The failure direction is safe by construction: **understating what you hold gets you more
bytes, never a hole**, and declaring nothing yields a self-contained response — so P6 survives
intact and the naive client needs no declaration logic at all.

### 7.2 Declarations are parent-granular by default

At 1–2 M marks a view spans **60–125 k tiles**, so per-tile declarations are **0.7–1.5 MB on the
uplink** — not trivial. Cuts are θ-derived and near-uniform across tiles at one depth, so the
default encoding is *"subtree under ancestor A, held to depth d, cut-uniform, view key E"*, with
per-tile entries as the exception path. That compresses the common case to kilobytes.

### 7.3 What each interaction then costs

| Interaction | Server work |
|---|---|
| Bootstrap / long-range jump | full view: 8–16 s CPU |
| Pan, 10–20% novelty | ~1–3 s CPU |
| Zoom in | the inter-cut band only |
| Zoom out | counts only — the parent's served set is a subset of held children |

## 8. Payload shape

At 1–2 M points, client-interaction §15's two open questions stop being rounding errors. Both said
"decide on P2's numbers"; the numbers are in.

- **`tessera_id` optional when picking is not requested** — 8 B/point, so **8–16 MB per view**.
  Identity fetched lazily per pick, inside the boundary.
- **Interleaved `fixed_size_list<f32,2>` positions** — removes an O(1–2 M) client-side pass per
  view.

Both additive. Both ahead of everything except the delta mechanism itself.

## 9. Density and points cache differently, deliberately

| | Density | Points |
|---|---|---|
| Size | 1–4 MB | 16–32 MB per view |
| Shape | dense, additive, derivable downward | sparse, prefix-structured, nests across depth |
| Scope | viewport-independent once global | per viewport |
| Sharing | across sessions of one grant set | none |
| Eviction | whole-artifact replacement | cut truncation |

Because counts are additive, **the server ships only the deepest level** and the client derives
every shallower level by summing 2×2 blocks — exactly, no approximation, no negotiation.

**Filters (design §8.2) do not break this**, by construction rather than luck: I12 keeps filters off
authorisation, and design §8.5's two-layer rendering makes the **context layer from `M_auth` the
cacheable half**. Every key above uses `M_auth` coordinates and survives any filter. The match layer
from `M_sel` is either small enough to send whole or broad enough that coverage is good again; if a
filtered underlay ever needs caching, its key is the content-addressed filter identity §14 already
establishes.

## 10. Consumer class (b): the ceiling is structural and must be documented

Tile-addressed consumers — MapLibre, OpenLayers, QGIS, the §8.3 MVT adapter — **cannot reach the
1–2 M target at all.** It would be 60–125 k tile fetches per view: the §8.2 self-DoS squared.

They operate at their own, much lower per-view budget by construction. S5's sizing survives
precisely *because* of that. The adapter's documentation must **state the budget ceiling** rather
than let an integrator discover it.

What class (b) needs is **coalescing, not caching**: single-flight per (grant set, view key,
viewport band), evaluate once, split by `served`, hand each `{z}/{x}/{y}` its slice. Posture per §8.3
unchanged — inside the trust boundary, `Cache-Control: private`, view-key-scoped URL segment as a
**session nonce rather than the raw view key** (URLs reach history and proxies), key carrying overlay
version and not mask identity alone.

## 11. Invariants and the leak register

- **I7.** C1's best-available rendering is presentation. The cache must never become a second
  sampler, and cached marks are never re-served upstream.
- **I2.** Every cached artifact is a masked output computed inside `M_auth`. Nothing caches an
  unmasked intermediate that is gated at serve time.
- **I10.** Shared caches hold `tessera_id` only. Anything carrying per-session handles (Phase 3
  labels' `node_handle`) is excluded from shared entries. S1/S2 are entity/row space and never leave
  the server.
- **The three retirement rules.** No cache interprets deny semantics. All invalidation binds to
  §6.2's tiers: identity generation voids everything; content version voids the delta; pin/segment
  version voids nothing client-visible.
- **Pins fix geometry, never authorisation.** S5 keys on overlay version independently of any pin,
  so a suppression voids cached tiles even under a pinned request.
- **New register entry required:** cross-session cache warmth (S3/S5 where grant sets are shared) is
  a timing channel — a hit whose warmth another session caused. Scoped to identical grant sets it
  discloses only "someone with your exact visibility was recently active", which is C15-adjacent and
  the same shape the fragment cache already has. P3/C4's house standard is work-indistinguishability,
  so it needs its own entry rather than an analogy.
- **Staleness is not a control.** Per §4, redrawing a held item to the same principal discloses
  nothing already disclosed. The two mechanisms that carry security are that the server never serves
  it again, and that the cache dies with the principal. Everything else here is coherence.

## 12. What this corrects

- **client-interaction §5.1** over-claimed: the cursor is half a mechanism. Additions belong to
  per-request declarations. (§7.1 above.)
- **client-interaction §5's rejection of client-derived membership** rests on "fail-open on
  newly-suppressed items", which contradicts §4's later ruling. Annotated at that paragraph on
  2026-08-01, re-grounded rather than withdrawn — and now worth revisiting, since a client-derived
  zoom-out is free against 8–16 s of server CPU.
- **design §7.3's "build cost zero"** for the underlay does not survive at screen resolution: 244 ms
  of a 466 ms request at 10⁹.
- **"Cache by cost, inverse to depth"** — proposed during this work and then partially withdrawn.
  It was derived when a shallow answer was kilobytes; with depth decoupled from viewport zoom, the
  overview *is* a 16–32 MB, 8–16 s-CPU answer. S4 survives as an overview/bootstrap cache, demoted.

## 13. Unmeasured, and load-bearing

**The novelty rate is the single most important unmeasured parameter in this document.** It appears
in the client accumulation figure (2–6 GB), the delta cost (~1–3 s CPU per pan), and therefore in
the concurrent-user ceiling. Every headline number moves with it, and it is assumed at 20–30% with
no evidence.

Measure, in order:

1. **Record real exploration traces and replay them** against the C1 policy — gives the novelty rate
   *and* the achieved hit rate under 4–12× cache pressure, which is what decides whether C1 delivers
   its promise or merely its bound.
2. **Distinct-grant-set distribution** for realistic deployments — decides whether the 32 GB shape is
   viable at 10⁹ at all.
3. **S3 build time** at depth 10–11 across principal breadths (the ~1 s figure is extrapolated).
4. **MVT encode cost per tile** — decides whether S5 stores encoded bytes or the Arrow split.
5. **Cursor memory** under real touch patterns before building the removals path.

## 14. Sequencing

1. **Delta-native serving** — per-request parent-granular declarations, plus the narrowed cursor for
   removals. The feasibility item.
2. **C1 with continuous eviction**, and the trace-replay measurement beside it.
3. **Admission control** with the CPU term, and the sizing formula published.
4. **Payload shape** — id-optional, interleaved positions.
5. **S3** density rasters.
6. **S5** with the class-(b) budget ceiling documented.
7. **S4** overview/bootstrap.
8. **S2 re-key** — hygiene.

## 15. Provenance

**r2 (2026-08-01) applies decision [0029](../decisions/0029-view-key.md)** and changes no key's
content. What §5's table and §7's mechanism called an "epoch" is the **view key** — the composite
of mask, overlay version, slice, *k* and idset within which a served viewport is stable. The
viewport is not one of its components, which is exactly why it can key a cache: one entry covers
every pan and zoom a session performs under it. Where a key already enumerates the view key's other
components — S3 and S4 — the remaining term is named the **content version**, which is what
client-interaction §6.2 calls that tier.

Produced 2026-08-01 from building and measuring the MVP client, then two rounds of adversarial
review with an independent agent that had no stake in the conclusions. The owner rejected four of
its first-round positions — that caching is not the latency mechanism, that grant sets may be
assumed shared, that the server-held cursor needs no client coordination, and a client working-set
figure that answered the instantaneous rather than the accumulated question. All four were conceded
on the merits and the arithmetic reworked; §7.1's direction split is the most valuable thing to come
out of that exchange.

A fifth correction came from the owner mid-exchange: the target draw is 1–2 × 10⁶ marks, not the
~66 × 10³ the client is configured to. That changed conclusions rather than numbers, and
§1 is its consequence.
