# Tessera — Concurrency and Lifecycle Design

**Status:** Draft r4 — r3 plus group-commit allocation, flush's standing as the ingest-visibility mechanism, and the write-latency budget extended to deny dispositions (Appendix R)
**Owns:** the mechanism level of the lifecycle: thread and state ownership, the generation/pin machinery, the fragment-epoch and deny-retirement ledgers, merge-versus-snapshot interaction, the WAL, and the router/worker protocol. This is the area the plan singles out ("the bugs will be in merge-versus-snapshot races"). Everything here is engine-internal — none of it is contract (contracts §6) — but it is *invariant-bearing* internal, so it gets design-and-review treatment.

**The simplicity rule applied here:** one mutation discipline — **immutable artifacts, atomic pointer swaps, refcounted pins, and a single writer thread per partition** — with every surviving subtlety given a named ledger and an explicit rule. r1's review found four fail-open paths in the first draft of those rules; r2's rules are the corrected ones, and each carries a conformance test.

---

## 1. The state model: generations

### 1.1 One mutable root per partition

All of a partition's serving state hangs off a single atomically-swappable pointer (arc-swap) to an immutable **Generation**:

```
Generation {
  prefix, segments_version: n, watermark: W,
  segments: Arc<[SegmentRef]>,   // each SegmentRef holds per-FILE Arc<Mmap>s
  postings: PostingsView,        // base + delta tiers + tombstones
  overlay_version: v,
  overlay: Arc<Overlay>,         // evaluate + deny entries (§3)
}
```

**The request ordering invariant (load-bearing, tested):** a request thread loads the generation pointer **exactly once, at request start, before acquiring any fragment or cache entry**, and works from that `Arc` throughout. Overlay resolution *happens-before* fragment acquisition. This ordering is what makes fragment eviction safe while a request still holds a fragment `Arc` — the request's own overlay still carries any deny the ledger has since retired — and an implementation that refreshes a fragment mid-request or fetches one before resolving the generation leaks in the eviction→retire window. It is an invariant, not folklore: the conformance suite drives the interleaving explicitly.

### 1.2 Two version axes, deliberately not one

`segments_version` moves on flush and compaction (row-space and postings shape); `overlay_version` moves on every accepted change batch (security state). They match §8.5's cache keys and have opposite pinning semantics (§2.3). Every mutation builds a new Generation sharing unchanged parts by `Arc` and swaps the pointer; a change-only generation is two small allocations.

### 1.3 The single writer, minimally loaded

One **lifecycle thread** per partition owns all *mutation decisions*, but performs only cheap operations itself: command-queue drain, WAL append and fsync (group commit permitted), and pointer swaps. **All file and object IO — segment writes, digests, side-manifest and object-store publication, merge execution — runs on a background pool**, submitting a completed, immutable result back to the lifecycle thread for a swap-only publication step. The command queue has a **priority lane for deny-disposition changes**, so a compaction publish or a stalled object-store PUT can never queue a suppression behind unbounded IO — deny visibility latency is bounded by (queue-front + fsync), nothing else. *(r4: with the write-latency budget of design §3 extended to deny dispositions, a deny may share §5.1's commit window, so the lane's guarantee is best read as **never starved beyond one window** rather than as a millisecond target. What it still forbids is exactly what it always forbade: a deny queued behind work of unbounded duration.)*

Publication-by-rebase resolves the merge-versus-flush race by construction: whatever completed work arrives, the lifecycle thread rebases it on the then-current generation, so concurrently flushed segments are carried forward automatically (§5).

## 2. Pins and retirement

### 2.1 The pin manager is Arc plus a drain list — with one ordering rule

A pin is an `Arc<Generation>`. Superseded generations sit on a drain list. **Reclaim ordering:** the lifecycle thread first *removes* the entry from the drain list, then verifies the strong count is one, then reclaims (closes exclusive mmaps, deletes retired-prefix files past their retention). A racing session-pin resolution that misses the removed entry gets `410 pin-expired` — correct — rather than cloning an Arc mid-reclaim. Verify-then-remove is the use-after-free the review caught; remove-then-verify is the fix, and it costs nothing. File handles are per-file `Arc<Mmap>`s held by every `SegmentRef` referencing them, so a file shared across generations survives either generation's reclaim (the case Lucene's `SnapshotDeletionPolicy` exists for; read it and `SearcherLifetimeManager` before writing this module regardless).

### 2.2 Session pins, worker restart, and durable retention

The router holds session pin id → per-partition `(n, W)` vector with TTL and a per-session cap. Presenting a pin resolves each partition's entry against that worker's drain list; absent → `410` for the whole request.

**A restarted worker serves no pre-restart pins.** Its drain list is empty and every pin touching it fails `410` — safe under I11 and now *stated*: the tempting alternatives (reloading old side-manifests to reconstruct `(n_old, W)`, or silently reinterpreting to current) are **forbidden**, because half-reconstruction mixes old rows with a fresh fragment cache and replayed overlay in combinations only the §3.3 build-epoch rule keeps safe, and a restart is exactly when that rule's state is coldest.

**Prefix retention is recorded durably — in the local cache, not the bundle.** Deciding a prefix is retired is router-level knowledge (session pins span partitions), so the **router** writes `retired/<prefix>-<timestamp>` under the engine's local cache directory when the last generation referencing a prefix drains; local copies of a non-current prefix are deletable only after (marker + session-pin TTL). The marker deliberately does not live in the bundle: the bundle layout is contract, `CURRENT` is its only mutable file, and node-local retention is a serving concern the contracts spec's out-of-contract list already covers. Deletion of retired prefixes from the *bundle* itself is operator or object-store lifecycle policy, out of scope here. Without the marker, alternating crashes either leak local prefix copies forever or delete one a live pin still references.

### 2.3 Pins fix geometry, never authorisation

A pinned request uses the pinned `segments_version` for tiles, columns and permutation, but composes `M_auth` against the **current** overlay — deny entries apply the moment they are accepted, pinned or not. The composition is coherent under mixed versions: `M_auth` is computed entirely in entity space with the *fragment's* watermark defining the live set, so every entity falls in exactly one of `fragment \ L` or `direct_eval(L)`; projection through the pinned permutation then drops row-absent entities via the sentinel — no gap, no double count. The observable consequence — a pinned drill-down can return fewer items than the viewport before it — is correct behaviour.

**This is a recorded refinement, not a silent divergence** (review finding 12): design §11.2's "a request pins its watermark alongside the segment-set version" and SA §6.4's "binds the triple to the fragment epoch at load" both read as if the pinned `W` participates in composition, when under this rule the effective watermark is the fragment's own and the pin-vector's `W` is advisory (status/debugging). Appendix R raises the amendment against both documents and the contracts' pin-vector description.

## 3. The overlay, fragment epochs, and the two retirement ledgers

### 3.1 Overlay entries, by disposition and cause

| Entry | Carries | Reflected in postings? | Retirement |
|---|---|---|---|
| **deny/deletion** | tombstone epoch *d* | yes — delta-tier tombstone at *d*; folded at compaction | ledger rule, §3.2 |
| **deny/suppression** | — | **never** — suppression does not touch postings | **only by unsuppress.** Non-retirable while active, by construction: no fragment rebuild ever excludes a suppressed entity, so its invisibility rests on the overlay entry for as long as the suppression stands |
| **evaluate** (predicate change) | current term set, inline (§11.2) | **not until compaction folds it** — deltas cover newly flushed entities only, so a change to an existing entity's terms is invisible to postings in both directions | fold epoch rule, §3.4 |

r1 assigned every deny a retirement epoch; for suppressions that is fail-open (any epoch eventually retires the entry and re-exposes the item) and the split above is the fix. Suppression count is a metric — a monotonically growing active-suppression set is a policy signal, not a leak — and `unsuppress` removes the entry and publishes a side-manifest immediately, as all deny-state changes do.

### 3.2 The deletion-retirement ledger

A deletion's deny entry (tombstone epoch *d*) may leave the overlay only when **no servable fragment epoch predates *d***:

- **Scope: all of this is per partition, per worker, in memory.** Epochs are per-partition segments-versions, denies live in their partition's overlay, and fragments never leave their worker — so `epoch_counts` and the floor are worker-local structures, and losing them on restart is safe by construction: the cache restarts cold and §3.3 forces every rebuild from current postings.
- The fragment cache maintains `epoch_counts: BTreeMap<postings_epoch, usize>`; `min_live_epoch()` is its first key (+∞ when empty).
- The cache additionally tracks `retirement_floor` = **the highest epoch of *any* retired overlay entry — a deletion's tombstone epoch *d* or an evaluate entry's fold epoch *f* alike** — and **refuses insertion of any fragment with epoch < retirement_floor**. Without this, a pinned or slow request could rebuild an old-epoch fragment *after* the entries predating it were retired and resurrect a deleted item or a revoked term (finding 2, and the fold-variant of the same race). The floor is deliberately defined over both retirement kinds: a floor raised only on deletion retirements passes the deletion test and still fails open through a pre-fold fragment. Refusal is cheap: the builder retries against current postings (§3.3).
- The lifecycle thread retires the retirable-entry prefix below `min_live_epoch()` after evictions and periodically.
- Compaction may force-refresh all fragments to advance the floor; overlay size is the pressure gauge.

### 3.3 Fragment builds always read current postings

Fragments are built by request threads on miss (single-flight per key) **from the current generation's postings view, never from a pinned one** — consistent with §2.3: pins fix geometry, and a fragment is authorisation state. Together with the insertion floor in §3.2 this closes the epoch-regression path, and the conformance suite carries the interleaving test: delete → retire → pinned request misses cache → assert the rebuilt fragment excludes the item.

### 3.4 Evaluate entries retire at the fold

A predicate change is invisible to postings until **compaction folds it**: compaction rewrites affected entities' postings from the term sets carried in their evaluate entries (an obligation now recorded in SA §6.6). After the fold, the entry carries its fold epoch *f* and retires under §3.2's rule — with *f* participating in the retirement floor exactly as a deletion's *d* does — a fragment predating the fold misreads the entity in both directions (a revoked term still present: fail-open; a granted term absent: wrong counts), so the same min-live-epoch machinery governs it. Before any fold, evaluate entries are immortal, which is why overlay growth under predicate churn schedules compaction, not just fragment refresh.

## 4. The WAL

Per partition, single appender (the lifecycle thread), append-only records (postcard, length-prefixed, CRC per record): `IngestBatch{batch_id, body_hash, rows, allocated entity IDs}`, `Change{external_id, op, term_set | tombstone_epoch}`, `Lease{lo, hi}`, `Flush{n, wal_pos}`.

**Ack ordering, stated fully: WAL fsync → overlay/generation swap → 200.** The swap is nanoseconds and sits *before* the ack so a caller's own next request always observes its accepted change; the crash window "after fsync, before swap" recovers by replay and was never acked — harmless.

**Recovery:** replay from the last `Flush`. **CRC failures distinguish position**: in the unsynced tail (past the last fsync point), truncate — those records were never acked. At or below the last fsync point, a CRC failure is corruption of *acked* state — including possibly denies — and recovery **fails closed**: the worker stays unready and the operator restores from bundle + object store. Truncate-at-first-bad-CRC applied mid-log would silently drop acked denies; the position rule is the difference between crash recovery and data loss.

**Disk-full vs never-429:** the ingest 429 threshold is set strictly below the WAL's hard bound, reserving headroom so change records always have room. If an append genuinely fails, the deny is applied to the in-memory overlay and swapped (visible immediately), the response is **500 with an alarm** — durability is owed and the caller must retry — never a 200 without fsync, never a silent drop, and never a refusal that leaves the item visible. **Side-manifest publication is gated on WAL durability**: the 500 path publishes nothing durable, so a replica can never observe a suppression that a subsequent crash-replay would silently remove — the appear-then-vanish-without-unsuppress state the contracts forbid replicas to reconstruct.

## 5. Flush, merge, compaction

### 5.1 Flush

Lifecycle thread decides; the pool executes: tiler → segment files under temp names → rename → delta files → side-manifest write; the lifecycle thread then swaps. A crash before the manifest write leaves orphans no reader references; replay re-flushes deterministically.

**Flush is what makes ingested items visible at all, not merely what bounds segment count** *(r4; design §11.2 and SA §6.4 carry the same correction)*. A buffered item has no row in any segment, and every viewer verb asks a row-space question, so the composition resolves its verdict and has nowhere to put it. Whoever implements this section is implementing ingest visibility; the flush policy's size-or-age knobs are therefore a *visibility-latency* control as much as a segment-count one, and `flush_max_age` in particular is the bound on how stale an acknowledged item's absence may be.

**Group-commit allocation** *(r4; owner decision on the write-latency budget, 2026-07-30 — design §3)*. Design §11.1 spends the entity-ID ordering on posting compression, and the sort's scope is whatever set of items is allocated together. Contracts §3.4 acknowledges `/control/ingest` with a per-row `tessera_id`, a bijection of the entity ID, so allocation must **precede the acknowledgement** — but §3's budget permits the acknowledgement itself to wait seconds. That is the whole latitude needed, and this document already permits the mechanism that uses it: §1.3's lifecycle thread does "WAL append and fsync (**group commit permitted**)".

So: hold arriving requests open in a commit window bounded by size or age; at close, signature-sort **the whole window**, allocate from the high-water, append and fsync once, swap, then acknowledge every held request with its rows' IDs.

- **The effective sort scope becomes the commit window**, across every request in it, regardless of how the client chose to chunk its upload — which closes at the server the failure mode design §11.1 warns about, rather than delegating it to a client convention.
- **Nothing about the ordering rules moves.** §1.3's ack ordering (fsync → swap → 200) holds per window instead of per request, so a caller still observes its own accepted change on its next request, and the crash window "after fsync, before swap" still recovers by replay having never been acknowledged.
- **I9 is untouched** — IDs are issued monotonically from the high-water exactly as now; the window changes only *how many* are assigned in one sorted run.
- **Nothing crosses the boundary.** The caller receives the same per-row `tessera_id` in the same 200, later.
- **Replay is unaffected**: WAL rows carry their allocated IDs (§4), so replay reuses them and never re-derives placement.

This supersedes an arena-based sketch carried earlier in r4's drafting, which bought the same scope by leasing a contiguous range and abandoning unfilled positions as never-issued holes. Group commit dominates it on every axis and is recorded in its place: **no ID slack** against design §16's thin `u32` budget (it issues precisely what it allocates), **no sub-chunk sizing problem** (the window's signature mix is known before a single ID is assigned, where the arena had to guess at lease time and degenerated under the near-singleton signature policies the probes measure), and **no conflict with §6's allocator arbitration** (the maximum ID actually written stays authoritative, because nothing is consumed without being written).

**Deny dispositions may share the window** *(owner decision; design §3)*. The write budget covers them, and a bounded configured delay is not the fail-open the deny rules exist to prevent. Two rules keep it that way and neither is negotiable. **The acknowledgement stays coupled to the application** — a deny's 200 is held until its entry is fsync'd and swapped, so nothing is ever acknowledged that is not yet in force; §4's "never a 200 without fsync" survives verbatim, and the priority lane's guarantee changes from *fast* to *never starved beyond one window*, which is what it should be measured on. And **changes are still never load-shed** (§4, contracts §3.1): batching a security operation for latency is acceptable, refusing one for load is not, and those are different things.

What remains ordinary design work: the window's size-and-age policy and its relation to `flush_max_items`; and **idempotency across a held window** — contracts §3.4's batch-id-to-body-hash check currently distinguishes *accepted* from *unknown*, and a window introduces a third state, *held but not yet acknowledged*, which a client retry can land in. A retry must join the open window rather than allocate a second time.

### 5.2 Merge

Tiered policy (§11.3 parameters), Morton re-rank decorator above 2¹⁸ rows and on forced merges. Selection on the lifecycle thread; execution on the pool over immutable inputs; publication rebases. Abandonment check at publication: all input segments still present in the current generation — ABA-safe because **`seg_id`s are never reused**, across compactions or prefixes (now stated in contracts §2.1).

### 5.3 Compaction, with the full carry-forward rule

Compaction snapshots a generation, emits the partition-slice's single segment, folds **snapshot-covered** posting deltas, tombstones and evaluate entries into base postings, rewrites the permutation, and publishes a new prefix. **Carried forward verbatim, not folded:** segments and deltas flushed after the snapshot, **tombstones accepted after the snapshot** (r1's rule covered only segments/deltas — folding away a post-snapshot tombstone while the entity survives in the folded base is fail-open, finding 5), the active suppression set, and all unfolded overlay entries. The new prefix's first side-manifest lists all of it; `n` continues; old prefix retention per §2.2.

## 6. The router/worker protocol

Internal, versioned by the binary; postcard frames over unix socketpairs; per-request deadlines; heartbeats. Messages as r1 (`Hello/Ready`, `BuildFragment`, `Query`, `Changes/IngestRows`, `Publish`, `Heartbeat`) with one addition from review:

**`Hello` carries the worker's allocation high-water, and the worker's WAL always wins.** On (re)connect the router advances its allocator journal to max(journal, every reported high-water) before granting any lease. This arbitrates divergent replay — a router journal restored from an older backup would otherwise re-grant ranges workers already consumed, an I9 violation with I5-scale blast radius. The worker's WAL is authoritative because it records IDs actually written.

Failure semantics: worker timeout fails the request (I13's outage asymmetry — never empty); respawn with backoff, rebuild from bundle + WAL, fragment cache cold and rebuilt on demand from router-retained auth data; pre-restart session pins fail `410` (§2.2); router exit kills workers via the supervision-pipe watchdog.

## 7. Threading model

tokio for HTTP and sockets; one lifecycle thread per partition (the single writer, minimally loaded per §1.3); rayon for CPU-heavy request work (unions, gathers, selection); the engine's public API is sync and owns no executor (embeddability, SA §1). Caches are concurrent maps with single-flight build; entries immutable, keyed by §8.5's keys verbatim; invalidation is key rotation, never mutation.

## 8. Crash matrix

| Crash point | Recovery | At risk |
|---|---|---|
| Before ingest/change ack | caller retries; idempotent | none |
| After fsync, before swap | replay rebuilds; un-acked | none |
| After ack (fsync + swap done) | replay rebuilds identically | none |
| Mid-flush (files, no manifest) | orphans unreferenced; replay re-flushes | none |
| Mid-merge / mid-compaction (no flip) | outputs orphaned / old prefix authoritative | none |
| Worker crash | respawn; bundle + WAL; pins on it `410` | pins (by design) |
| Router crash | watchdog kills workers; supervisor restarts; allocator re-arbitrated from worker high-waters (§6) | sessions (by design) |
| Mid-log WAL corruption (below fsync point) | **fail closed**; restore from bundle + object store | availability, never denies |

The row that must never exist: any path that loses or re-exposes an acked deny. §4's ordering, §3.1's suppression rule, §3.2's insertion floor and §5.3's tombstone carry-forward each close one such path found in review; all four carry conformance tests.

## 9. Decisions

1. **Single lifecycle writer, minimally loaded** *(amended r2)*: decisions and swaps on the thread, all IO on the pool, deny priority lane — deny visibility latency bounded by fsync, not by compaction IO.
2. **Generations immutable and Arc-shared; drain-list reclaim is remove → verify → reclaim** *(amended r2)*.
3. **Pins fix geometry, never authorisation** — now with the I11/§11.2/SA §6.4 amendment raised rather than implied.
4. **Two version axes** matching §8.5.
5. **Three retirement rules, not one** *(amended r2)*: deletion denies by the epoch ledger with an insertion floor; suppressions only by unsuppress; evaluate entries at their compaction fold. r1's single rule was fail-open for two of the three.
6. **Fragments build from current postings only**, with the cache refusing epochs below the retirement floor *(new r2)*.
7. **Protocol postcard over socketpairs; worker WAL wins lease arbitration** *(amended r2)*.

## Appendix R — Review record

r1 was reviewed independently (verdict: needs-rework — the generation/single-writer/ledger architecture survives; four fail-open paths in the retirement rules did not). r2 closes all fifteen findings: suppression non-retirability (§3.1 — the blocker), the fragment insertion floor and current-postings build rule (§3.2–3.3), the request ordering invariant (§1.1), evaluate-entry fold retirement (§3.4), the extended compaction carry-forward (§5.3), the minimally-loaded lifecycle thread with deny priority lane (§1.3), the fsync→swap→ack order and its crash row (§4, §8), WAL headroom and append-failure semantics (§4), remove-then-verify reclaim (§2.1), lease arbitration by worker high-water (§6), worker-restart pin semantics and durable prefix retention (§2.2), the positional CRC rule (§4), and seg-id non-reuse (§5.2).

**r4** adds to §5.1 from the ingest audit that produced design r23, SA r5 and contracts r8. Two additions, no rule changed. First, **flush is named as the ingest-visibility mechanism**, not merely the bound on segment count: a buffered item has no row, every viewer verb asks a row-space question, so nothing about it reaches an answer until this section runs — which makes the size-and-age knobs a visibility-latency control and makes a phase that ships the buffer without the flush one that has built durability rather than queryable ingest. Second, §5.1 gains **group-commit allocation** for design §11.1's per-batch limit, following the owner's 2026-07-30 decision that the write-latency budget of design §3 is seconds or more **including deny dispositions**. Since contracts §3.4 requires the entity ID only *before the acknowledgement*, and the acknowledgement may wait, requests are held in a commit window and the whole window is signature-sorted, allocated, fsync'd once and acknowledged together — making the sort scope the window rather than the request, at the server rather than by client convention. §1.3 already permitted group commit; the ack ordering, I9, replay and the wire contract are all untouched. **This replaced an arena-based sketch drafted earlier in this revision** (lease a range, place into signature-keyed sub-chunks, seal abandoning holes), which an adversarial review found to carry two unresolved holes — sub-chunk boundaries fixed blind at lease time, degenerating under the near-singleton signature policies the probes measure, which left its 1.3–1.5× slack cap asserted rather than derived; and a conflict with §6's allocator arbitration, since sealing makes the maximum ID *written* understate the range *consumed*. Group commit has neither, needs no slack at all, and is therefore recorded in its place rather than alongside it. §1.3's deny priority lane is re-read as a starvation bound rather than a latency target, and §4's never-a-200-without-fsync and never-load-shed rules are preserved verbatim — a deny's acknowledgement stays coupled to its application, so nothing is acknowledged that is not yet in force.

**Actions raised against companions — all applied 2026-07-28** (design §11.2/§2.6/I11, SA §6.4/§6.6, contracts §2.1/§2.3, plan §10.3); retained as the record:
1. *SA §6.6*: compaction's fold obligation extends to evaluate entries and the carry-forward rule to post-snapshot tombstones and the suppression set. ✔
2. *Design §11.2 + SA §6.4 + contracts §2.3*: the effective watermark in I1 composition is always the fragment's own; the pin-vector `W` is advisory. The verification pass found two further phrasings in the design (§2.6 step 1, I11) still implying the pinned `W` participates; both amended. ✔
3. *Contracts §2.1*: `seg_id`s never reused. ✔
4. *Plan §10*: five lifecycle conformance tests from this document — suppression persistence; the eviction→retire→pinned-miss rebuild excluding deleted items; **its fold-variant** (evaluate entry folds, retires, pre-fold-epoch fragment insertion must be refused — the r3 floor generalisation); post-snapshot tombstone survival; positional CRC fail-closed. ✔

A third-party verification pass confirmed all fifteen r2 resolutions and surfaced the four r3 fixes: the floor generalised over both retirement kinds (§3.2 — the one that could have reintroduced a fail-open path), worker-local scope of the ledger structures stated (§3.2), side-manifest publication gated on WAL durability (§4), and the `RETIRED` marker given a writer (the router) and a home (the local cache, keeping the bundle contract intact) (§2.2).
