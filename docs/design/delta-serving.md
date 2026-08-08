# Delta serving — what a client may declare, and what that lets the server skip

**Date:** 2026-08-08
**Status:** Provisional r2 — under review. **⊘ Partially implemented.** Built: the two coordinates
of §2, omission by explicit tile list (§3), emptiness caching, the client replica and its
render-provenance rules (§7), the staleness bound (§8), and look-ahead's ring (§13). **Not built:**
the server-side skip and band elision — the `declarations` operand of §6 does not exist, so no
request carries a cut or a count and §5's failure directions describe a path with no traffic on it
— and the next-depth fetch of §13. **To become normative:** owner sign-off on the two coordinates
and on §10's remaining escalations, plus one adversarial review of §4's exactness argument.
**Companion to** `caching.md`, whose §7 mechanism this specifies and whose §14.1 names it the
feasibility item, and to `client-interaction.md` §5, whose prefix declarations it makes concrete.
**Touches:** design §7.2, §8.2, §8.5; contracts §3.2; caching §5–§7, §11; client-interaction §4–§6, §10.

---

## 1. The result

A viewport response is a set of per-tile point bands. Because the selection definition (design
§7.2) serves a **prefix in `tessera_id` order** of each tile's visible set, and because those
prefixes nest across depth, a client that already holds part of a tile's band can say so in a form
the server can act on — and a client that holds all of it need not ask at all.

Three mechanisms, in descending order of what they save:

| | Saves | Decided by |
|---|---|---|
| **Omission** — the tile is not in the request | the whole per-tile pipeline: row ranges, count, selection scan, gather | the client |
| **Skip** — the tile is requested, but provably held | the selection scan and the gather; one count is paid | the server |
| **Elision** — points below a declared cut are not sent | the gather and the wire bytes. **Never the scan** | the server |

Measured shares of one request (`docs/evidence/memos/2026-07-30-viewport-hot-path-and-bundle-size-review.md`):
selection 83–89%, gather 5.1–6.5%, counting 3.4–5.8%, range derivation 2.6–4.4%. So only the first
two make server work scale with novelty rather than with viewport area. Elision is a **wire and
client-decode** mechanism whose server-side value grows with the declared-scalar tail.

Confirmed on a second corpus, by naming tile sets explicitly and reading the server's own timing
(2.4M items, depth 7, `k = 500`, the lean five-column schema, **warm row-projection cache**): a
request for 1 to 164 tiles costs ~170 µs, and one for all 16,384 costs 7.2 ms. **The fixed
per-request prefix is ~170 µs and everything else is per-tile**, so a tile left out of the request
costs essentially nothing, and a view answered entirely from the replica costs nothing at all
because no request is made.

Two limits on that figure, because both change what it means. It is **steady state**: a session
whose projection is not resident pays the build instead, which `refresh.rs` measures at 4,550 ms at
10⁹ — four orders of magnitude above this floor, and dominant whenever it happens. And the per-tile
share grows with schema width, since the gather does; five declared columns is the narrow end.

Throughout: `vis(T)` is a tile's visible set — the session's authorisation mask `M_auth` restricted
to that tile's rows; `served(T)` is the subset design §7.2 serves, of size `m(T)`; θ is that
section's identity threshold, `P_d` at depth *d*.

## 2. Two coordinates

Cache validity and declaration validity are different questions, and conflating them either voids
a cache that is still sound or honours a declaration that is not.

| | Components | Governs |
|---|---|---|
| **Identity key** | idset, auth-data hash, mask-fragment identity in design §8.5's canonical form *(bundle manifest digest, auth-plugin hash, satisfied term set)*, slice | whether a held band may be **rendered at all** |
| **Content key** | the identity key, the **watermark of the geometry actually served**, the overlay version, and a per-process nonce | whether a held band may be **declared** |

Both are opaque to the client: minted server-side, echoed back, compared for equality and nothing
else. A request that echoes a content key the server does not currently compute has **every
declaration ignored and receives a full response**. The server therefore acts on no client claim
about client state — the property that makes the whole mechanism reviewable.

**The content key travels as an entity tag.** The response carries it as `ETag` and a request
echoes it as `If-Match`, because the semantics are exactly HTTP's and contracts §0.2 adopts
published formats rather than inventing. One documented deviation: a mismatch does not produce
`412`, it produces the **full response**, because the request is still perfectly answerable and
refusing it would make the fail-closed path a failure rather than a fallback. The tile-addressed
route will want the same validator for browser caching, which is why this is one spelling of one
coordinate rather than two.

This realises `client-interaction.md` §6.2's tier table. Identity generation voids everything, so
it is the cache partition key; content version voids what is visible, so it gates declarations;
the segment-set version voids nothing, which is why it appears in neither and why the advisory
geometry stamp is not a cache signal.

**A held band survives a content-key rotation.** It becomes renderable-but-stale-marked, not
discarded: an item that was served has been disclosed, and redrawing it to the same principal
discloses nothing further (`client-interaction.md` §4). What lapses is the byte saving, not the
picture — which is what keeps interaction smooth while a corpus is moving.

**Why the watermark and not the segment-set version.** §4's asymmetry means the content coordinate
must catch *rows added*, which is what the watermark counts. A merge or a compaction rotates the
segment-set version and the bundle prefix without adding a row; a declaration is expressed in
identity space; and quantisation is immutable at runtime (decision 0040), so no point changes
tile. Keying on the segment-set version would void every declaration on every background
compaction for no correctness reason.

**Why the watermark of the geometry *served*.** A request may be answered from a one-generation-stale
row projection when that projection still covers the row space (lifecycle's geometry ladder). Two
responses under one generation snapshot can therefore have different `vis(T)`, the stale one
lacking rows flushed since. A key minted from the generation would let a fresh response elide
against a cut declared from a stale one, which is a hole the client cannot detect. The key is
minted from the geometry that resolved.

**Why a per-process nonce.** The overlay version is an in-process counter that starts at zero, so
without one a post-restart key can collide with a pre-restart key over different content.
Declarations lapse across a restart; nothing else does.

**The overlay version earns its place on coherence, not safety.** Removals cannot hole a
declaration (§4), but they move the visible and matched counts, which a client must not keep
presenting as current. It also moves on an ingest, before the flush that gives those items rows —
so the key rotates while the row-space visible set is momentarily unchanged. That is
over-rotation, and it is the right direction: over-rotating costs a client bytes it need not have
spent, under-rotating costs it rows.

**Filters.** The retrieval surface carries none today, so matched equals visible. When design
§8.2's filter operands arrive, **declarations are ignored on any request carrying a filter**. A
filter changes which held items match, and having the client decide that is client-derived
membership, which `client-interaction.md` §5 declines.

## 3. Who decides what

### Emptiness — cached first, or none of the rest matters

A response omits a tile whose visible count is zero, so a tile asked for and not returned holds
nothing. A viewport is overwhelmingly such tiles: measured on the 2.4M demo corpus, a settled view
spans 16,524 tiles of which 454 carry any data. A client that caches only the marks re-asks for the
other 16,070 on every view, so it always has a request outstanding and **no revisit is ever free**,
however many marks it holds. Negative entries expire exactly as bands do, since a flush can put
rows in a tile that had none.

### Omission — the client, for a tile it has fetched at this depth

θ is viewport-invariant: `P_d` is a function of the mask, the generation and the slice, never of
the bounding box or the zoom (design §7.2). So at a fixed content key and a fixed depth, `m(T)`
does not move — and the server has already told the client what it is, in the tile stream's
`served` column. Two exact tests:

- **Complete** — the band's point count equals `served`.
- **Complete at a larger *k*** — `served < min(k, k_max_marks)`. The cap was not the binding clause
  of §7.2's definition, so raising *k* cannot grow `m(T)`. Where `served` equals that cap, the cap
  *was* binding and a larger *k* yields more: the tile must be requested.

A tile passing both is not listed in the request, and the server never learns of it.

### Skip — the server, for a tile the client has not fetched at this depth

Zoom-in is this case: the client restricts a parent band to a child and holds *n* of the child's
points, with no `m(child)` to compare against. The server cannot cheaply derive one — §7.2's floor
clause needs to know whether the count below θ reaches `k_min`, and identities are scattered in row
order, so establishing that requires the very scan being avoided.

What the server *can* prove without scanning is `m(T) ≤ min(cap, visible(T))`, and the visible count
is a bitmap operation already performed before selection. So:

> **The server skips a tile when the client's declared count reaches `min(cap, visible(T))`.**

Exact, with no estimate. Two regimes reach it: sparse tiles, where the client holds the whole
visible set; and single-quadrant clusters, where a parent's served prefix restricts almost entirely
to one child, so the declared count reaches the cap exactly when the cap was binding — which is to
say on the *dense* tiles where the skipped scan is largest.

**The estimator is declined.** Approximating the count below θ as `visible · P_d / 2^64` is sound in
expectation, since `tessera_id` is a keyed permutation and identities are uniform over `vis(T)`, and
it would cover every tile. It is refused because it lets a client sit short a few marks at a *fixed*
content key, with no request that would correct it — turning the conformance property from an
equality into an inequality.

### Elision — the server, within a served tile

The request declares a cut: an identity bound *X*, meaning *I hold every member of `vis(T)` with
`tessera_id < X`*. The server evaluates §7.2's definition unchanged and emits `served(T)` minus the
members below *X*, reporting how many it withheld.

**A bound, not a count**, and the choice is load-bearing. The bound is the *same value at every
depth*, so one declaration against an ancestor answers for every tile beneath it — which is exactly
the zoom-in case and `client-interaction.md` §5's parent-granular form. Declarations compose by
taking the maximum bound. Eviction that truncates a band's tail lowers its bound exactly. A count
expresses none of this and means the wrong thing for a band assembled from several responses.

**The elision is a projection of the definition's output, never of the definition.** The selection
scan runs at full coverage, `served` continues to report `m(T)`, and I7 — sampling happens after
masking — is evaluated whole. There is no second selection route here, and decision 0008's single
route is untouched.

## 4. Why elision is exact, and where it would not be

> **Only *additions* to `vis(T)` can open a hole.** `tessera_id` is a keyed Feistel permutation of
> the entity id and is not monotone in it, so a newly ingested entity can land below any bound *X*.
> "The members of `vis(T)` below *X*" is then a set the client does not hold, and eliding it leaves
> a gap the client cannot see. **Removals cannot.** The client holds *every* visible identity below
> *X*, so a shrinking `vis(T)` leaves it holding a superset; and anything a removal newly promotes
> into `served(T)` sits above *X* and is served normally.

Every way `vis(T)` can move, and what covers it:

| Event | Effect on `vis(T)` | Covered by |
|---|---|---|
| Flush | adds rows | the watermark |
| Deny, suppress, unsuppress, delete | removes or restores rows | the overlay version — for coherence; safety does not need it |
| Ingest buffer | no rows exist until flush, so none | nothing needed for safety — but the buffer swap bumps the overlay version, so the key rotates anyway |
| Row-space merge | permutes rows, adds no entity | nothing needed; the declaration is in identity space |
| Entity-space coalesce | touches no row | nothing needed |
| Compaction fold | rotates the prefix and every fragment identity; retires executed deletions — a removal | nothing needed |
| Stale-geometry serve | serves a smaller `vis(T)` than the generation implies | minting the key from the geometry served (§2) |

The last row is the one that must be *keyed on* rather than argued away, and it is the reason §2
specifies the mint site as precisely as it does.

## 5. Failure direction

Three claims a client can get wrong, and what each costs:

- **Understating a cut** — more bytes arrive, never a hole. An evicted band is simply no longer
  declared, which is what makes eviction protocol-free.
- **A stale content key** — a full response arrives. Fail-closed by construction.
- **Overstating a count** — the server may skip a tile it should have served, leaving a hole in the
  **declaring client's own picture**. Nowhere else: `client-interaction.md` §4's posture is that a
  buggy client harms its own user's picture, never another principal's data.

**No client claim can cause the server to serve something it would otherwise withhold.** Declarations
only subtract — the selection heap admits only identities at or above the bound, and the skip only
omits. That is the property the security argument rests on, and it holds by construction rather than
by validation.

A client that declares nothing receives a self-contained response, so the naive path (`caching.md`'s
P6) needs no declaration logic at all.

## 6. Declarations on the wire

A declaration set carries the content key, a list of **cuts** and a list of **counts**.

**Cuts are prefix-granular.** A cut is `(depth, prefix, below)` and applies to every tile beneath
it. At the 1–2 × 10⁶-mark operating point a view spans 60–125 × 10³ tiles, so per-tile cuts would be
0.7–1.5 MB of uplink (`caching.md` §7.2); cuts are θ-derived and near-uniform at one depth, so the
common case compresses to kilobytes with per-tile entries as the exception.

**Counts are per exact tile, and optional.** The skip test compares a count against a specific
tile's visible count, and a count declared against an ancestor cannot be distributed to the tiles
beneath it — it would either mean nothing or overstate every child and, by §5, hole the client's own
picture. So a count is valid only where its `(depth, prefix)` names a tile the request lists, and is
refused elsewhere. Because they do not compress, a client sends counts selectively: they are an
optimisation, and omitting one costs only the scan the skip would have saved.

**Declarations are computed from the cache, never from a cursor.** What a server-held cursor records
is what the server *named*; what the cache holds is what the client *kept*, and eviction is normal
operation rather than an edge case (`caching.md` §7.1).

**Negotiated per request, not per session.** `client-interaction.md` §5 has the additive mode
negotiated per session; per-request declarations are simpler and strictly safer, since a client that
sends none never receives a delta.

## 7. What the client may draw

A band may be rendered under three provenances, and only the first is the served set:

- **Exact** — a band at the requested depth under the current identity key.
- **Ancestor** — a parent band restricted by Morton prefix, drawn while children stream.
- **Descendants** — the union of held child bands, drawn on zoom-out.

The last two are supersets of `served(T)`. They are **presentation, never selection**: a client
choosing what to *draw* is free, a cache that becomes a second sampler is not (I7). Two rules follow.

**No number-channel value is displayed against a non-exact tile.** Superset marks must not be read
as density, and a stale count must not be read as current.

**Every subset a client draws is an identity-order prefix.** This applies to the skipped tile, where
the client draws its own band, and to a *k* decrease, where the drawn length is
`min(cap_new, served_old)` — the band is kept whole and a prefix of it is drawn, so marks never pop
and nothing is discarded. Any other subsetting would make the cache a sampler.

**A response replaces a band; it does not merge into one.** Concatenation is legal only where the
arriving response carries the band's own content key, and the delta's first identity is strictly
above the band's bound. Unioning a full response onto a held band would let a suppressed item the
client holds below the cut survive into a band it now marks fresh — a client-side fail-open, and
against the fail-closed-by-naming property that the server names the complete served set.

**The drawn-count assertion holds over exact tiles only.** Drawn marks equal the sum of `served`
across rendered exact tiles; each of the three carve-outs above is asserted separately rather than
by widening that check, because a widened check is where a real omission would hide.

## 8. The staleness bound must stay reachable

`client-interaction.md` §4 rules that a client answering pans entirely from held tiles makes an
accepted change invisible indefinitely — *don't re-download* is free, *don't re-request* needs a
bound, and the bound is the view key. Omission and look-ahead exist precisely to empty the request,
at which point no rotation is ever observed and cached counts are presented as current forever.

> **A viewport whose tiles are all held still issues its request**, with an empty fetch list, once
> the held answer is older than the revalidation window.

It returns fresh counts and a fresh content key for the cost of the counting stage — 3.4–5.8% of a
request — and that is what keeps the accepted minutes-scale budget bounded. The window is the
bound, and the owner's budget for it is minutes in both directions; inside the window an all-held
viewport touches nothing, which is what makes panning free.

**The revalidation belongs to the replica, not to the scheduler.** The replica owns the content
key, so it is the only layer that can tell a held answer's age; a scheduler that merely declines to
ask would leave the bound unenforced exactly when look-ahead is working best.

**A request with `k = 0` is the counts-only form**, and is named rather than merely tolerated: a
zero cap makes the definition serve nothing, so the response is the tile stream and its validator
alone. Zoom-out uses it to refresh the number channel while marks are drawn from held descendant
bands, which is the ordering `client-interaction.md` §6.2 prescribes — refresh numbers eagerly,
marks lazily.

One consequence a client must not get wrong: at `k = 0` every held band trivially contains the
whole of `served(T)`, because that set is empty. The completeness test of §3 is therefore vacuous
here, and a client applying it would omit every tile and refresh nothing. **A counts-only request
omits nothing**, which is the one place the request planner special-cases `k`.

## 9. Invariants and the leak register

- **I7** (sampling happens after masking). The selection scan runs at full coverage under every
  mechanism here; elision projects its output. Best-available rendering is presentation, and cached
  marks are never re-served upstream.
- **I2** (every aggregate computable from inside `M_auth`). Every band is a masked output; nothing
  cached is an unmasked intermediate gated at serve time.
- **I10** (entity identifiers never cross the trust boundary). Bands hold `tessera_id` only.
- **Deny semantics are not interpreted by any cache.** Retirement stays governed by write-path
  §5.4's two rules; the client's obligation is §7's replace-never-merge, which needs no knowledge of
  them.
- **The withheld count is derivable, so it is not a new number channel.** It is defined over
  `served(T)`, bounded by `m(T)`, and recoverable by one request with a zero bound at the same
  content key — decision 0023's ground. Defining it over `vis(T)` instead would let a client
  binary-search the identity distribution of its own *unserved* visible tail, which is underivable
  and would need a register entry; that definition is refused.
- **⊘ Open — the work channel.** Response time now varies with what the client declares. Design
  §11's standard is work-indistinguishability rather than output-equivalence, and omission removes
  83–89% of the work as a function of client-supplied input, so this needs an Appendix C entry or an
  owner-approved widening of C19 — not an analogy. Escalated (§10).

## 10. Owner decisions

Two are ruled and are stated where they belong — the entity tag in §2, and `k = 0` in §8. Three
remain open, and the first blocks elision:

1. **The work channel above** — a register entry, or a widening of C19.
2. **The relationship to decision 0029.** The identity key deliberately excludes *k* and the overlay
   version, which 0029's view key includes. A declaration's truth depends only on `vis(T)`, which *k*
   does not move, so a client keeps its cache across a *k* change — and raising *k* then fetches only
   the band between the old `m(T)` and the new. Cross-principal safety is unaffected, since the idset,
   the auth-data hash, the fragment identity and the slice together identify what a principal may see.
   0029 is Settled and its warning is aimed at client authors, so this wants a decision file rather
   than a recording.
5. **The budget-form request** (`{slice, bbox, budget, k}`, client-interaction §8.6). Omission is
   structurally incompatible with a server-chosen tile set, though cuts and counts are compatible with
   one. Record the relationship now or the budget form arrives shaped against this contract.

## 11. What this interacts with, and what is unmeasured

**`caching.md` §8's payload shape and this mechanism are in tension.** Making `tessera_id` optional
saves 8 B/point — 8–16 MB per view — but a client that does not hold identities cannot compute a cut,
and cannot lower one by truncating a band. The reconciliation, if that saving is taken, is a per-tile
maximum-served-identity column, which restores declaration without restoring 8 B/point; band
truncation then needs identities at checkpoints rather than per point. Neither is designed here.

**Depth churn costs more than novelty does, early on.** Bands are keyed by depth, and the depth
budget's calibration is one-directional — it may only make the next request deeper — so until
`m_target` settles every view lands at a depth nothing is held at and the replica is cold whatever
its hit rate would be. On the demo corpus that is a handful of interactions; whether it is more at
10⁹, and whether the calibration should be damped harder once a cache exists, is unmeasured.

**The novelty rate remains the load-bearing unmeasured parameter** (`caching.md` §13), assumed at
20–30%. Every figure in §1's table is a share of a request, not a share of a session, and only trace
replay converts them into what a user experiences. Measure, beside it: which of the two skip regimes
fires and how often, declaration uplink size in practice, and the rotation cadence of the content key
under representative ingest — which is only now worth measuring, since the key no longer rotates on a
merge or a compaction.

## 12. What this corrects in `caching.md`

- **§5's C1 key** is the view key and the tile prefix. One coordinate cannot answer both questions
  it is asked: a band whose content has moved is still renderable, and a band whose principal has
  changed is not. §2 splits it.
- **§7.1's mechanism is additions-elide-against-declarations**, which is the third and smallest of
  the three in §1. The two that make server work scale with novelty — omission and the skip — are
  not in that section, and neither is reachable from a declaration alone.
- **§7.2's "parent-granular by default"** holds for cuts and not for counts. A count declared
  against an ancestor cannot be distributed to the tiles beneath it (§6).

## 13. Look-ahead

The replica makes a *revisit* free; it does nothing for the first visit to new ground, which is
most of what panning is. So the client buys a ring beyond the visible box while the view is still,
and the next pan is answered from held bands.

Requesting a tile the viewport does not strictly need is **presentation, never selection** — which
tiles a client asks for is its own business, and §7.2's prefixes nest, so a wider or deeper answer
is a superset and nothing pops when the user arrives. It is never retried and never runs alongside
a foreground request, because anticipation that queues ahead of what the user is waiting for is the
reason a view is slow rather than the cure.

**It buys latency with server work, and does not avoid work.** Measured on the 2.4M corpus
(`probes/2026-08-08-lookahead-contention/`), pans needing no request at all against server CPU per
pan:

| clients | pans free, off | pans free, on | CPU/pan off | CPU/pan on |
|---|---|---|---|---|
| 1 | 5 / 12 | 11 / 12 | 1.61 ms | 1.73 ms |
| 4 | 25 / 48 | 44 / 48 | 1.37 ms | 2.67 ms |
| 16 | 98 / 192 | 176 / 192 | 2.13 ms | 2.58 ms |

Server-side per-request latency is flat in both the arm and the client count — p50 1–7 ms, p95
≤ 27 ms, nothing shed — so anticipation does not make the engine the bottleneck at this scale. What
it costs is CPU: roughly half again per pan, for roughly double the fraction of pans that need
nothing.

**The premium grows with repetitive movement, which inverts the obvious expectation.** At a 0%,
25% and 50% chance of reversing direction per pan, the premium runs +48%, +64%, +71% — because the
replica already makes a revisit free without any anticipation, so turning is exactly where the
*off* arm gets cheap, while a symmetric ring fetches ahead in every direction at once and so costs
the same whether the guess was right or not. Biasing the ring downwind is what would make its cost
depend on prediction; the viewer does that and the measurement does not model it, so that bias's
value is **unmeasured**.

**What look-ahead is for is fast movement, and what bounds it is the pause.** A slow drag outlives
the debounce and is answered *during* the movement, so it never needed anticipation: measured, a
420 px pan at 300 px/s waits for nothing either way, while at 1000 and 2500 px/s the unanticipated
client waits 1.0 s and 2.1 s and the anticipating one waits not at all. But the ring is bought only
after the view has been still, so a continuous drag never triggers one — the binding constraint is
whether a ring fetch fits in the user's pause, not whether the ring is geometrically large enough.

**Anticipation needs its own idempotence guard, and neither equality nor containment provides one.**
A renderer re-emits view-state events continuously — on every property update, and while a drag's
inertia decays — and the values drift by small amounts rather than repeating. An equality check
therefore never fires, and a containment check fails too, because a box shifted by a hair is not
inside the previous one. The foreground is immune only because its covered-view check answers a
repeated view outright; anticipation cannot borrow that check, since its whole purpose is to fetch
what the current view does *not* cover. Left ungated, the ring's own state update re-arms the idle
timer and a fresh ring fires a quarter-second later, indefinitely — measured at seven per idle
pause. The guard that works is **hysteresis**: a ring is bought to cover the next pan, so a drift
of less than a quarter of the viewport does not need another one.

**Look-ahead does not scale to the target operating point as written, and the limit is the
client.** Enumerating a tile set and planning it against the replica are both linear in tile count,
measured at 5.9 ms and 4.1 ms for today's ~4k-tile foreground. At `caching.md` §3's 1–2 × 10⁶-mark
target a view spans 60–125 × 10³ tiles and its ring three times that: **181 ms to enumerate and
56 ms to plan, per idle pause**, on the thread that also draws. That is a visible freeze, and it
arrives before any of the wire or server costs above. Three ways out, none built: enumerate
incrementally, move enumeration off the main thread, or address the ring as a difference from the
foreground rather than as a superset of it. The last is the most promising and the least designed.

**The next-depth fetch is specified and off.** Anticipating a zoom-*in* means requesting depth
`d+1`, which the ring cannot help with — a zoom lands on tiles the replica has never seen. It is
tile-count-neutral (four times the tiles over a quarter of the area) and emphatically not
CPU-neutral: the client by construction holds none of depth `d+1`, so nothing elides and every idle
pause costs a genuine slice of a viewport's selection scan, multiplied by every concurrent user.
Measure before enabling it.

## 14. Sequencing

`caching.md` §14 puts delta-native serving first as the feasibility item and the client band cache
second. Build order inverts them, because the cache is what *computes* a declaration: there is
nothing to declare until bands are held, and the cache alone already delivers revisit-free zoom-out
and best-available rendering with no protocol change at all. The wire-visible half — the tile list,
the declarations, the withheld and skipped columns, the key headers — then lands together, and wants
to land before a published client closes the `api_version = 1` window.

Elision is sequenced behind its own measurement. Its recorded value is policy-dependent — small at
the current mark target, large at big mark budgets — so the mark-budget sweep decides it rather than
an argument.

## Appendix R — Review record

**r2 (2026-08-08) — the built half folded back in.** Three findings from implementing it, each of
which changed the document rather than the code: emptiness must be cached before anything else or a
mostly-empty viewport re-asks forever (§3); the per-request floor is a warm-cache figure and reads
as an assurance without that condition (§1); and anticipation needs hysteresis rather than an
equality or containment guard, because a renderer's view-state events drift rather than repeat
(§13). The last was found by measurement, not review — seven rings per idle pause, in code whose
unit tests all passed.

**r1 (2026-08-08) — drafted**, from `caching.md` §7's mechanism and `client-interaction.md` §5's
declarations, against three independent adversarial reviews of the implementation plan (performance,
invariants, interface). Four of their findings changed the design rather than the prose: the content
coordinate keys on the watermark rather than the segment-set version, so a compaction no longer voids
every declaration; the key is minted from the geometry served rather than the generation snapshot,
closing a hole a stale-geometry serve would otherwise open; cuts and counts are separated, because a
count does not compose parent-granularly; and the reachability of a zero cap removed a proposed
encoding that read a zero served count as a skip marker.
