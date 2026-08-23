# Geometry pinning — design

**Date:** 2026-08-02
**Status:** Normative. Reviewed three times (invariants, implementability, client contract),
dispositioned and signed off 2026-08-03; the deletion has landed. §14's amendments are folded into
the ten documents it names.
**Reads against:** architecture §2.6, §10.4, §11.2, I11, Appendix C; contracts §3.1, §3.2;
concurrency-lifecycle §2.1–§2.3, §3.2, §6, §7.2; [write-path](write-path.md) §4.1, §4.6, §7.
**Citation convention:** unprefixed §n is the architecture design; `lifecycle §n` is
concurrency-lifecycle, `contracts §n` contracts, `write-path §n` the write path — which absorbed
`flush-and-merge.md` when it was promoted on 2026-08-04, so the `flush §n` citations this document
carried are now write-path's. This document's own sections are cited as **spec §n**.

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

- A drain entry per tick, each pinning a superseded bundle's mappings. Lifecycle §2.2's own sizing
  note puts a *measured* 47.02 GB bundle at a *modelled* ~94 GB of mapped files at depth 2,
  contending for page cache with the live one.
- A startup relation `pin_ttl_secs < drain_depth_max × flush_max_age_secs` (lifecycle §2.2's
  sizing obligation), because publications landing closer together than
  `pin_ttl_secs / drain_depth_max` leave the list permanently at its ceiling: the depth alarm
  saturates, and pins are dropped by the trim rather than by their TTL, so a client is `410`d
  before the lifetime it was promised.
- Roughly 2,000 lines across six crates and three languages, plus two design sections, an error
  code, three config keys and a startup relation.

So the question is whether the retention earns that. This document argues it does not, and that
what a client actually needs is a *staleness signal* rather than a frozen view.

**What the case is not.** An earlier draft led on the startup relation as a *floor of 75 s on
visibility latency*, and that is wrong in both directions. It is not the binding floor: the
row-projection cache keys on `segments_version`, so every flush rotates every session's key, and a
miss is a **measured 1 277 ms** for the projection at 10⁹
(`probes/2026-08-14-project-decomposition/`; it was 4 550 ms against the superseded implementation,
and the end-to-end warm-up viewport that figure sat inside has not been re-measured since). That floor is removed by the background refresh
(write-path §4.6, decision 0044), which is needed whatever happens to pins and is not this
document's to claim. Nor is the relation itself a reason to delete anything:
it is a sizing constraint, and sizing constraints are satisfied by choosing numbers. The honest
case is the two bullets above — mapped bytes and lines of mechanism, both buying something no
client uses.

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

## 4. A flush cannot invalidate a pin — and merge permutes row space, so nothing may key on the prefix

Two claims, and the second is the one a future reader is most likely to get wrong, because an
earlier draft of this document got it wrong.

**A flush appends, so it invalidates nothing.** Flush §2.1 gives segment *k*
`[row_base_k, row_base_k + row_count_k)` and the extent list is ordered and disjoint; an extent is
admissible only where its `row_base` is exactly the view's current row total, and a flush never
rewrites the base `permutation.bin`. So a pin taken before a flush and answered after it returns
**identical answers for every row that existed when the pin was issued**. The retention machinery
is being paid for on the one event that cannot invalidate what it protects.

What a pinned answer *does* differ in is that it omits rows that did not exist at pin time. That
is the frozen view, and spec §6 is about whether it is worth anything.

**Merge does not preserve row identity, only row count — so "a row id is immutable within a
prefix" is false.** A merge is row-count preserving in the sense write-path §7 states: it emits
exactly as many rows as it consumed, so no *later* extent's `row_base` moves. Inside the merged
span the rows are re-sorted into globally sorted output, so entities interleave and a row id
inside that span names a **different entity** after the merge than before it.

Nothing live fails open on this — the row-projection cache keys on `segments_version`, which a
merge advances, so no cached row-space structure survives one. The reason it is stated here, in
the strongest terms available, is that the false version is exactly the sentence a future reader
would rely on to key a *persisted* projection cache on the prefix, which would then serve one
entity's rows under another's mask. **The rule that replaces it: merge permutes row space within
the merged span, so no row-space artefact may key on the prefix.** `segments_version` is the only
safe discriminator, and spec §9 is why it stays one.

## 5. Compaction is the real hazard, and it is prefix-scoped

Compaction rewrites row space: it applies tombstones, reclaims deleted rows' space, folds
snapshot-covered posting deltas into base postings, and may re-quantise. Row ids move. §I11's *"selects arbitrary rows"* is
about exactly this boundary.

**Compaction publishes a new prefix and flips `CURRENT`** (§12.5, write-path §8). So the hazard is
identified by the `prefix` component alone; `segments_version` moves for flushes and merges, which
cannot invalidate anything (spec §4).

Two consequences:

- A staleness signal that distinguishes prefixes distinguishes the only boundary that matters.
- **A Morton prefix is a permanently stable address.** An earlier draft carried re-quantisation as
  the one case where it stops being one; [decision 0040](../decisions/0040-quantisation-is-slice-scoped-index-config.md)
  removes the case. Quantisation is view-scoped index configuration, immutable at runtime, and
  compaction carries each view's forward byte-for-byte — re-quantisation is not one of the
  reorganisations compaction does. So a client never has to be told to discard cell identifiers,
  and spec §12's superseded-prefix obligation is a freshness question rather than a correctness
  one.

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
- **The fragment-insertion floor lifecycle §3.2 then specified.** It refused a fragment whose
  stamp was below the highest retired entry's, and its stated trigger was *"a pinned or slow
  request"* — the **slow** half surviving pins entirely, so the floor was never pin-dependent and
  this document left it untouched. *(It has since been **deleted from the spec** with the stamp
  ledger it belonged to — owner-ruled 2026-08-03. Rule F replaces both: a deletion retires only at
  the compaction fold that executes it, and the safety property is an identity match rather than a
  stamp ordering, the fold's new prefix rotating the fragment identity so no pre-fold fragment is
  reachable by key. Nothing in this section's pin argument depended on the floor.)*

## 10. What this buys

- **~94 GB of mapped files stop being retained** at lifecycle §2.2's own depth-2 sizing, against a
  *measured* 47.02 GB live bundle — page cache spent holding geometry no client resolves.
- **`flush_max_age_secs` loses one of its two constraints.** The startup relation goes, so the 90 s
  default stops being forced by the TTL. **It does not become a free choice**: the binding floor is
  the row-projection rebuild, a *measured* 1 277 ms at 10⁹ synchronised across the session population
  at every tick, and only taking that off the request path removes it. Claiming the tick as this
  document's win was an error in an earlier draft and is corrected here rather than quietly
  dropped. *(Decision 0044 is what took it off: a background refresh at each publication, with
  stale-serve in front of it — write-path §4.6. The floor is now one refresh round, which is a
  different and much smaller number.)*
- **Cache pruning simplifies.** The licence to prune a superseded generation's projections was a
  `Reclaimed` value produced by a drain-list reclaim, and `prune_generation` ran synchronously
  inside the publication. With no drain list, retention became an explicit N-generations-back
  policy on the cache, stated where the cache is bounded rather than inherited from a pin lifetime
  — and that depth is now what both feeds the background refresh and bounds how long a
  stale-served session can lag (write-path §4.6).
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
2. **The prefix-retention bound loses its operand, and the replacement is not free.** Lifecycle
   §2.2 makes a local prefix copy deletable only after `marker + session-pin TTL`. With no TTL the
   bound must become "no in-flight request holds it", which is an `Arc` strong count rather than a
   clock. **Nothing can observe a strong count reaching zero.** `Arc::strong_count` is a sample,
   not an event, so the mechanism is a registry of `Weak` handles and something that polls it —
   which is the drain list again, wearing a different type. Roughly 100 of the deleted lines come
   back the day prefix deletion lands. That is a real cost of this decision and it is recorded here
   rather than discovered then; it is still an order less than what is removed, and it is scoped to
   prefixes (compactions) rather than to every flush.
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
6. ~~Lifecycle §3.2's retirement floor still refuses a below-floor fragment from a slow
   request.~~ *(Superseded 2026-08-03: the floor is deleted from the spec with the stamp ledger,
   and Rule F's identity match replaces it — write-path §5.4. The obligation this list was
   checking, that deleting pins costs no retirement safety, is unaffected: neither mechanism was
   pin-dependent.)*

## 13. Out of scope

Cell-granular invalidation (spec §7); compaction and re-quantisation's client contract (spec §5);
the multi-partition router protocol (lifecycle §6); the prefix-retention mechanism (spec §11.2).

## 14. Corpus updates that land with the code

The draft listed five documents. The real set is ten, and the five it missed are named first
because a missed document is how a corpus goes stale silently.

- **conformance** — §4.4 tests I11 **through the pin**, calling it *"the presentable proxy"*.
  Removing pins moves I11 from covered to uncovered. **That must be stated as a negative result,
  not left silent**: the within-request rule (spec §1) is what survives, and it has no test until
  one is written against generation resolution directly.
- **client-interaction** — §6.1's per-session staleness scoping (now an efficiency argument, not a
  security one — see the ruling below) and §6.2's tier table, whose "pin/segment-set version" tier
  assumes `segments_version` moves only on compaction. Flush made it move every tick, and that
  inconsistency is real independently of pins.
- **system-architecture** §4.2 — the pin in the three-plane API surface.
- **tile-addressed-integration** — the per-tile request pattern reads against the pin.
- **caching** — the pin as a cache key component.
- **inventory** — the I11 row.
- **architecture** — I11 restated as the within-request rule; §10.4's session-pinning paragraph and
  §11.2's pin-vector language; §12.5's compaction/drain interaction. **Appendix C's C15 mitigation
  column is false in code** and is corrected rather than carried: it claims *"Pin values are
  per-session scrambled so no cross-session correlation"*, and `PinId` is a plaintext
  `(prefix, segments_version)`, identical for every principal. Harmless under the ruling below, but
  a leak register that describes a mitigation the code does not implement is worse than one that
  describes the exposure.
- **concurrency-lifecycle** — §2.1 (the drain list), §2.2 (session pins, the two bounds and the
  sizing obligation, the prefix-retention marker), §2.3 (pinned geometry against current overlay).
  §3.2's retirement floor is unchanged by *this* document and should say so. *(It was
  subsequently deleted with the stamp ledger — see spec §11's note above.)*
- **contracts** — §3.1's closed code list loses 410 `pin-expired`; **§3.1 and §3.2** carry the
  `x-tessera-pin` header, which changes meaning from a selector to an advisory stamp. (The draft
  and the Status line both cited §3.4 for this; §3.4 is the external-id contract.)
- **flush-and-merge** — §1.3 and §4's relation 1 and its default; §13's I11 line. *(That document
  was deleted on 2026-08-04; `write-path.md` carries all three, and §4's merge-size relation is no
  longer load-bearing — merge selection runs over the extent list, so the base segment is excluded
  structurally.)*
- One decision record: pins become a staleness stamp rather than retained geometry.

### The owner's leak ruling, recorded

**Knowing that data has been ingested is not a security leak.** Ruled 2026-08-02, resolving a
finding the invariants and client-contract reviewers raised independently: the staleness signal in
spec §7 may be the **broadcast** form — the live generation stamp, with no per-session mask
intersection. `client-interaction.md` §6.1 argues for per-session scoping; under this ruling that
argument is an efficiency one (do not wake clients whose visible set did not change), not a
security one, and it is recorded there as such.

This is what makes spec §7 implementable as one comparison against the live generation rather than
as a per-session diff, and it is why C15 needs no mitigation beyond an accurate description.

## Appendix R — Review record

**r1 (2026-08-02) — drafted.**

**r2 (2026-08-02) — three independent reviews: invariants, implementability, client contract.**
The central argument survived all three. `client-interaction.md` §6.2 in fact states the conclusion
more definitively than the draft did — its tier table has the pin advancing on compaction and
voiding *"nothing"* for a client.

**r3 (2026-08-03) — disposition and sign-off.** Six corrections, all to the document rather than to
the design:

1. §4's mechanism was false — row ids are *not* immutable within a prefix, because merge permutes
   row space within the merged span. Replaced and inverted into the rule that no row-space artefact
   may key on the prefix.
2. §10's headline benefit was false. The binding floor on `flush_max_age_secs` is the
   row-projection rebuild, not the pin TTL. §0's motivation rewritten to the honest case: mapped
   bytes and lines of mechanism.
3. §5's re-quantisation caveat is gone under decision 0040, and §12's superseded-prefix obligation
   with it.
4. §14 listed five documents; the real set is ten. Five added, and the `x-tessera-pin` citation
   corrected from contracts §3.4 to §3.1/§3.2.
5. The owner's leak ruling recorded in §14 and here.
6. C15's mitigation column corrected — `PinId` is plaintext, identical for every principal.

**One finding verified and rejected.** The client reviewer argued the real cross-request dependency
is frame assembly: a tile-addressed client issues one request per tile, and spec §2 obliges it to
*"render only responses sharing one view key"*. Checked against `client-interaction.md`: §6.2 makes
content version (flush) and pin/segment-set version (compaction) **separate tiers**, and §6.1
assigns frame assembly to a client-side replica store — *"Fetch behind the current display and flip
atomically when the visible tiles and their counts are complete"* — with rule 2 going further, that
*"auto-flipping is the wrong default"*. So the pin is not the mechanism frame assembly rests on,
and removing it does not remove one. The §6.2 inconsistency the finding surfaced on the way (the
tier table assumes `segments_version` moves only on compaction; flush moves it every tick) is real
and is fixed there.

**The fallback that was not taken, named so it is not rediscovered as novel.** The reviewer offered
a **frame-window retention** — prefix-scoped, depth 1–2, seconds rather than 300 s — as a middle
path keeping most of the stability benefit at a fraction of the cost. It was put to the owner
alongside full deletion and full deletion was chosen. If spec §11's objections turn out to bite,
this is the cheaper thing to reintroduce, and it is cheaper than what was removed.
