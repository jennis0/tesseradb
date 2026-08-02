# Geometry pinning — design

**Date:** 2026-08-02
**Status:** Provisional — under review, and **not approved**. The rest of the corpus governs where
they disagree. **To become normative:** owner sign-off, and §12's amendments folded into
`architecture.md` (I11), `concurrency-lifecycle.md` (§2.1–§2.3) and `contracts.md` (§3.1's code
list, §3.4's pin header) as the code lands.
**Reads against:** architecture §2.6, §10.4, §11.2, I11, Appendix C; contracts §3.1, §3.4;
concurrency-lifecycle §2.1–§2.3, §3.2, §6, §7.2; flush-and-merge §1.3, §2.1, §2.2.
**Citation convention:** unprefixed §n is the architecture design; `lifecycle §n` is
concurrency-lifecycle, `contracts §n` contracts, `flush §n` flush-and-merge. This document's own
sections are cited as **spec §n**.

**Owns:** what a pin is for, and what the server retains on its behalf.

**Does not own:** I11's within-request rule, which is unchanged and is the half of the invariant
that carries its weight. Nor compaction, whose interaction with this is stated in spec §5 and
must be settled by the compaction spec rather than inherited from it.

---

## 0. The question, and what changed to make it worth asking

A pin is a `(prefix, segments_version)` pair returned on every viewport response and presented on
the next request. Presenting one causes the request to be answered against **that** geometry
rather than the live one, which requires the server to keep superseded generations alive: a drain
list of up to `DRAIN_DEPTH_MAX` entries, each holding an `Arc<Bundle>` and therefore its
memory-mapped segment files, for up to `pin_ttl_secs`.

That was affordable while geometry moved rarely — a rebuild, a compaction. **Flush makes it move
every tick**, and the cost stops being incidental:

- A drain entry per tick, each pinning a superseded bundle's mappings.
- A startup relation `pin_ttl_secs < drain_depth_max × flush_max_age_secs` (lifecycle §2.2's
  sizing obligation), because publications landing closer together than
  `pin_ttl_secs / drain_depth_max` leave the list permanently at its ceiling: the depth alarm
  saturates, and pins are dropped by the trim rather than by their TTL, so a client is `410`d
  before the lifetime it was promised.
- Therefore a **floor on visibility latency of 75 s** at the shipped 300 s TTL and depth 4 —
  which is why `flush_max_age_secs` defaults to 90 s rather than to something a user would notice
  less.

So the question is whether the retention earns that. This document argues it does not, and that
what a client actually needs is a *staleness signal* rather than a frozen view.

## 1. I11 is two claims, and only one of them requires retention

§I11 reads: *"Any cached structure expressed in row IDs carries the (segment-set version,
watermark) pin it was built against, and a request resolves the segment-set version once and uses
it throughout."*

**The within-request claim.** A request resolves its generation once and uses it for tile ranges,
columns, permutation and mask alike. Mixing a row-space mask from one generation with a
permutation from another selects arbitrary rows — §I11's own words, *"not stale-restrictive but
simply wrong"*.

This is unarguable and it is **free**. A request loads the generation once at its start and holds
the `Arc` for its duration, so every file it reads stays mapped by refcount. §10.4's *"in-flight
requests complete against the old prefix, retained until drained"* describes that refcount, not
the drain list. Nothing in this document changes it.

**The cross-request claim.** A *later* request presenting a pin is answered against an *earlier*
generation. This is what the drain list, the TTL and the per-session cap exist for, and it is the
only thing they exist for.

The two are separable, and this document keeps the first and drops the second.

## 2. The retention was imported with a justification that does not transfer

§10.4 states the provenance: *"This is the standard session-pinning pattern from mature search
engines."* Lucene's `SearcherLifetimeManager` is the reference, and in a search engine session
pinning has one canonical purpose: **pagination**. A query returns page 1 from one searcher, and
pages 2..n must come from that same searcher or results shift, duplicate or vanish between pages.

**This API has no pagination.** A viewport request takes a bbox and a zoom and returns every
covered tile's counts plus the selected marks in one response. There is no cursor and no page 2.
Each response is self-contained and internally consistent whichever generation produced it.

Lucene's other use of the pattern — `SnapshotDeletionPolicy`, keeping files alive for backup and
replication — is a different problem this project does not have here.

## 3. Nothing client-facing needs old geometry to be resolvable

The remaining intuition for retention is that a client holds identifiers which only mean something
in the geometry that issued them. It does not hold any such identifier.

- **A tile is a Morton prefix and a depth.** Codes are computed against `MANIFEST.json`'s
  `quantisation`, fixed at build (contracts §2.5), so a prefix names a region of the grid and not
  a set of rows. `tile_ranges` resolves the same prefix against any segment of any generation by
  binary search over that segment's own sorted codes.
- **An item is a `tessera_id`.** `IdentityKey::invert` recovers `(shard, entity)` — a keyed
  permutation, independent of geometry — and the entity resolves to a row through the current
  `RowSpace`. Drill-down already answers in entity space *before* looking up a row, which is what
  closes C4's timing channel; the row lookup is the last step, not the addressing.

So a re-issued request re-locates everything it needs. There is no identifier a client can hold
that a fresh generation cannot resolve.

## 4. A flush cannot invalidate a pin

This is the load-bearing claim, and it is a property of the design rather than of an
implementation detail.

**Within a prefix, a row id addresses the same entity for the prefix's whole life.**

- A flush **appends**: flush §2.1 gives segment *k* `[row_base_k, row_base_k + row_count_k)` and
  the extent list is ordered and disjoint. An extent is admissible only where its `row_base` is
  exactly the slice's current row total.
- A merge is **row-count preserving**: flush §2.2 — *"A merge emits exactly as many rows as it
  consumed, so no later segment's `row_base` ever moves"*. Merging *k* adjacent extents collapses
  them to one at the same base.
- Neither rewrites the base `permutation.bin`.

So a pin taken before a flush and answered after it would return **identical answers for every row
that existed when the pin was issued**. The retention machinery is being paid for on the one event
that cannot invalidate what it protects.

What a pinned answer *does* differ in is that it omits rows that did not exist at pin time. That
is the frozen view, and spec §6 is about whether it is worth anything.

## 5. Compaction is the real hazard, and it is prefix-scoped

Compaction rewrites row space: it applies tombstones, reclaims deleted rows' space, folds evaluate
entries into postings, and may re-quantise. Row ids move. §I11's *"selects arbitrary rows"* is
about exactly this boundary.

**Compaction publishes a new prefix and flips `CURRENT`** (§12.5, flush §5.3). So the hazard is
identified by the `prefix` component alone; `segments_version` moves for flushes and merges, which
cannot invalidate anything (spec §4).

Two consequences:

- A staleness signal that distinguishes prefixes distinguishes the only boundary that matters.
- **Re-quantisation is the one case where a Morton prefix stops being a stable address**, because
  the grid it names changes. A client holding pre-compaction cell identifiers must be told to
  discard them, which is the same shape as `idset`'s answer for identifiers (contracts §2.2). The
  compaction spec must settle this; this document does not.

## 6. What the frozen view is worth

Stripping out what spec §3 and §4 dispose of, one benefit remains: while a client pans and zooms,
counts and drawn marks do not shift underneath it because of newly ingested items. θ is anchored
on the session's total visible count (§7.2), so as that grows the threshold moves and the same tile
at the same depth can select a different subset of marks.

Three things weigh against it, and the first is decisive:

**The design already surrendered half of it, deliberately.** Lifecycle §2.3: a pinned request uses
pinned geometry but composes against the **current** overlay, so *"the observable consequence — a
pinned drill-down can return fewer items than the viewport before it — is correct behaviour"*. A
pin does not give a client what it saw; it gives what it saw minus anything denied since. The
stability argument is already accepted as partial.

**Under flush, counts only grow.** A live map arguably should show data arriving, and a child tile
summing higher than the parent a moment ago is explicable in a way that "the numbers are wrong" is
not.

**The price is stated in spec §0**: a floor of 75 s on visibility latency, plus superseded bundles
held resident — lifecycle §2.2's own sizing note puts a *measured* 47.02 GB bundle at a *modelled*
~94 GB of mapped files at depth 2, contending for page cache.

**This is the one judgement in this document that is not derivable from the corpus.** Whether the
shimmer matters is a property of the map's feel. The recommendation is that it does not, and that
if it does the remedy is client-side — hold the last frame's marks while new counts settle — rather
than a server retaining geometry for five minutes.

## 7. What replaces it: a staleness stamp, and the shape a delta protocol needs

The round trip is kept and its meaning changed.

- The response carries the generation stamp it was answered from, as now.
- The request carries one back — **advisory only**. It never selects geometry, never expires, and
  never produces a `410`. The server compares it against the live generation and reports whether
  anything moved.

That is the smallest thing delivering the value the frozen view was reaching for: a client learns
its view is out of date and decides what to do, at the moment it asks rather than five minutes
later.

**It is also the input a cell-granular protocol needs**, which is why the field is kept rather
than dropped. The natural evolution is for the response to carry *which Morton prefixes changed*
rather than a boolean, so a client redraws the affected cells instead of the view. The data is
already in the right shape: a flush segment covers a contiguous entity range and its codes are
sorted, so the cells a generation touched are a run of prefixes readable from the segment's own
code array rather than a scan. **Not specified here** — it wants its own design, including how the
prefix depth is chosen and what a client is promised about coalescing. What this document commits
to is not closing the door: dropping the request field now would mean re-adding it then.

## 8. What is deleted

- `PinManager`, `PinnedGeometry`, `DrainEntry`, `Reclaimed`, `PinStats`, and the reclaim, retire
  and drained-resolve paths.
- `pin_ttl_secs`, `pins_per_session_max`, `drain_depth_max`, `DRAIN_DEPTH_ALARM`, and the startup
  relation `pin_ttl_secs < drain_depth_max × flush_max_age_secs`.
- `EngineError::PinExpired` (→ 410) and `EngineError::PinCapExceeded` (→ 422), with their server
  mappings.
- Lifecycle §2.2's `retired/<prefix>-<timestamp>` marker in its current form — see spec §11 for
  what replaces the bound it provided.

## 9. What is kept, and why

- **I11's within-request rule**, unchanged (spec §1).
- **`check_publishable`** — a publication whose `segments_version` does not strictly increase is
  refused. Its justification changes and does not weaken: today it is *"outstanding pins would
  answer against new geometry"*; afterwards it is *"the row-projection cache keys on
  `segments_version`, so a non-increasing version serves a stale row-space projection against new
  geometry"* — the same mixing hazard, reached from the cache instead of from a pin.
- **The row-projection cache's key**, which already includes `segments_version` and must, because
  a flush genuinely extends row space.
- **Lifecycle §3.2's retirement floor.** It refuses insertion of a fragment whose stamp is below
  the highest retired entry's, and its stated trigger is *"a pinned or slow request"*. The **slow**
  half survives pins entirely: a request that began before a retirement can still rebuild an
  old-stamp fragment. The floor is not pin-dependent and is untouched.

## 10. What this buys

- **`flush_max_age_secs` loses its floor.** The 90 s default exists only to clear 75 s; the tick
  becomes a free choice, and single-digit seconds becomes an ordinary configuration rather than one
  requiring a second knob to be raised first.
- **Cache pruning simplifies.** Today the licence to prune a superseded generation's projections is
  a `Reclaimed` value produced by a drain-list reclaim, and `prune_generation` runs synchronously
  inside the publication — which is why flush §9 needs *"retention of superseded-generation entries
  until patched or drain-expired"* and finds it unsequenceable against today's callers. With no
  drain list, retention becomes an explicit N-generations-back policy on the cache, stated where the
  cache is bounded rather than inherited from a pin lifetime.
- **~1,900 lines of deletion**, and two design sections shrink to the rule that carries their
  weight.

## 11. Where this is weakest

Stated by the drafter so a reviewer spends its effort elsewhere. None of these is believed fatal;
each is a place to attack.

1. **The multi-partition fan-out (lifecycle §6) is unbuilt and unproven here.** A request spanning
   partitions today resolves a per-partition `(n, W)` vector through the router. Without pins, each
   partition answers from its own current generation, so a fanned request can mix generations.
   Entities are disjoint across partitions, so counts still sum; but θ's anchor is required to be
   **session-global** (§12.3 — otherwise "below the cut" means different things in different
   partitions), and a mixed-generation anchor is a mixture. The claim here is that this is a
   difference in freshness rather than in correctness, and that pinning did not solve it either
   (the vector fixes each partition independently, not jointly). **This is the strongest objection
   and the least tested.**
2. **The prefix-retention bound loses its operand.** Lifecycle §2.2 makes a local prefix copy
   deletable only after `marker + session-pin TTL`. With no TTL the bound must become "no in-flight
   request holds it", which is an `Arc` strong count rather than a clock — implementable, but it is
   a different mechanism and this document does not specify it.
3. **The frozen view is a product judgement** (spec §6), not a derivation.
4. **Compaction is undesigned** (spec §5). If it turns out to need cross-request geometry retention
   for a reason not visible from here, this document has removed the machinery it would have used —
   though not the ability to reintroduce it prefix-scoped, which is far cheaper than
   version-scoped.

## 12. What must be proven

1. A request re-issued after a flush returns counts consistent with the flush having happened —
   never a mixture of two generations' row spaces.
2. `check_publishable` still refuses a non-increasing `segments_version`, and the row-projection
   cache never serves an entry built against a different one.
3. A viewport presenting a stamp from a superseded generation is answered **normally**, with the
   staleness signal set — never `410`, never refused.
4. A viewport presenting a stamp from a superseded **prefix** is likewise answered normally with
   the signal set. (What a client should *do* about it is spec §5's open question.)
5. No superseded `Bundle` is retained after the last request holding it completes.
6. Lifecycle §3.2's retirement floor still refuses a below-floor fragment from a slow request.

## 13. Out of scope

Cell-granular invalidation (spec §7); compaction and re-quantisation's client contract (spec §5);
the multi-partition router protocol (lifecycle §6); the prefix-retention mechanism (spec §11.2).

## 14. Corpus updates that land with the code

- **architecture** — I11 restated as the within-request rule; §10.4's session-pinning paragraph and
  §11.2's pin-vector language; §12.5's compaction/drain interaction.
- **concurrency-lifecycle** — §2.1 (the drain list), §2.2 (session pins, the two bounds and the
  sizing obligation, the prefix-retention marker), §2.3 (pinned geometry against current overlay).
  §3.2's retirement floor is unchanged and should say so.
- **contracts** — §3.1's closed code list loses 410 `pin-expired`; §3.4's `x-tessera-pin` changes
  meaning from a selector to an advisory stamp.
- **flush-and-merge** — §1.3 and §4's relation 1 and its default; §13's I11 line.
- **inventory** — the I11 row.
- One decision record: pins become a staleness stamp rather than retained geometry.

## Appendix R — Review record

Not yet reviewed.
