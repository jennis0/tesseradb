# Part 5 facts — ingest, buffer/watermark/overlay, segments/merge/compaction, generations/pins, retirement rules, router/worker protocol

## Ingest: what it may and may not touch

- Target ingest visibility latency is seconds to minutes — source: design §11
- Ingest is not coupled to any credential cadence, because under §2.2 there isn't one — source: design §11
- Entity space is append-only (invariant I9), so ingest appends entity IDs and appends postings to the term index — source: design §11.1
- Ingest never reorders, never rewrites, and never invalidates anything expressed in entity space — masks, node memberships and generating sets all remain valid, merely incomplete — source: design §11.1
- New term descriptors are interned per §6.1 — source: design §11.1
- Row space is where the churn lives; new items interleave arbitrarily into the existing Morton ranking, so a correct in-place insert would renumber a large fraction of the slice — this is why the permutation exists (invariant I4) — source: design §11.1
- Entity IDs within a batch are assigned sorted by term signature so term postings form long runs inside each batch's ID range and head terms encode as run containers; this costs nothing, does not weaken I9, and is safe only because of I10 — source: design §11.1
- Entity IDs must NOT be assigned in Morton order — the tempting alternative because it would make new segments permutation-free — because that would give permission-space IDs spatial meaning, turning the ID gap between two visible points into an estimate of how much unauthorised data lies between them (leak register C6) — source: design §11.1
- Measured (r18): created-order assignment yields run lengths of 1.00–1.26 against a 1.000 random baseline (nothing), while signature-sorted assignment buys 8.9–36.7x on posting storage and up to 130x on union cost at equal coverage — source: design §11.1
- Because I9 makes assignment permanent, signature-sorted assignment ships in the Phase 1 allocator or not at all — source: design §11.1
- Deletions are tombstones: remove the entity from the term index, add it to the overlay with deny disposition, notify the caller of affected labels (§2.5), and drop the row at the next compaction; the ID is never recycled (I9) — source: design §11.3

## The buffer, the watermark, and the overlay

- Arrivals land in an in-memory buffer — source: design §11.2
- A flush policy — size or age, whichever trips first — turns the buffer into an immutable on-disk segment, so segment count is governed by the flush interval rather than the arrival rate — source: design §11.2
- The mask carries an entity high-water mark: a mask fragment built at watermark W is authoritative below W; entities at or above W are new and not yet folded in — source: design §11.2
- Flushing advances W by OR-ing in the flushed segment's contribution for the token's already-known satisfied terms — a small, monotone patch rather than a rebuild — source: design §11.2
- The overlay holds items in flux below W: those whose predicate changed, those deleted, and those administratively suppressed — source: design §11.2
- Each overlay entry carries a disposition — evaluate (test its current term set against the token's) or deny (invisible regardless); the disposition is what lets one mechanism cover both a predicate change and an administrative suppression; entries carry their own term sets inline — source: design §11.2
- The live set L = overlay ∪ {entities ≥ W}, and I1's composition follows — source: design §11.2
- Direct evaluation needs no index: an item's term set is a handful of IDs, so visibility is a set intersection against the token's satisfied terms — microseconds for tens of thousands of items — source: design §11.2
- L is bounded by change rate times the interval before masks are naturally rebuilt — source: design §11.2
- A request resolves its segment-set version pin once and uses it throughout (invariant I11); the watermark governing I1's composition is the one belonging to the mask fragment actually used, and is never pinned across requests — pins fix row-space geometry, not authorisation state (a suppression applies to a pinned request the moment it is accepted; see the concurrency and lifecycle design) — source: design §11.2
- Generation struct fields include segments_version: n and watermark: W alongside segments, postings (PostingsView: base + delta tiers + tombstones), overlay_version: v, and overlay (Arc<Overlay>: evaluate + deny entries) — source: lifecycle §1.1
- segments_version moves on flush and compaction (row-space and postings shape); overlay_version moves on every accepted change batch (security state) — source: lifecycle §1.2
- Every mutation builds a new Generation sharing unchanged parts by Arc and swaps the pointer; a change-only generation is two small allocations — source: lifecycle §1.2

## Segments, merging, compaction; merge-vs-snapshot

- A tile resolves to one contiguous range per live segment, so cost is linear in segment count and it must be bounded — source: design §11.3
- Merge policy is structured on established lines rather than as a scheduled job: a floor size (below which segments are treated as equally small so a tail of tiny segments does not dominate decisions), a maximum merged-segment size (preventing any merge from becoming an unbounded rewrite), and separating the reasons to merge — natural tiering, forced compaction, and tombstone reclamation on a deletes-percentage trigger — source: design §11.3
- Re-ranking is a decorator on the merge policy: reorder only merges above a minimum document count, skip rather than fail when memory is short, and always reorder on forced merges — the Morton re-rank becomes a continuous property of large merges rather than a scheduled cliff — source: design §11.3
- Reference points from a widely-deployed policy: ten segments per tier, a 5 GB maximum merged segment, a 2 MB floor, a 20% deletes threshold, reordering above 2^18 documents — source: design §11.3
- A compaction rewrites the permutation, tile table, candidate lists and columns, publishes them under a new segment-set version, and lets in-flight requests drain (invariant I11); at single-node scale it does not invalidate the term index, masks or generating sets — source: design §11.3
- Flush (lifecycle mechanics): lifecycle thread decides, pool executes: tiler → segment files under temp names → rename → delta files → side-manifest write; the lifecycle thread then swaps; a crash before the manifest write leaves orphans no reader references, and replay re-flushes deterministically — source: lifecycle §5.1
- Merge: tiered policy (§11.3 parameters), Morton re-rank decorator above 2^18 rows and on forced merges; selection on the lifecycle thread, execution on the pool over immutable inputs, publication rebases — source: lifecycle §5.2
- Abandonment check at publication: all input segments still present in the current generation — ABA-safe because seg_ids are never reused, across compactions or prefixes (stated in contracts §2.1) — source: lifecycle §5.2
- Compaction snapshots a generation, emits the partition-slice's single segment, folds snapshot-covered posting deltas, tombstones and evaluate entries into base postings, rewrites the permutation, and publishes a new prefix — source: lifecycle §5.3
- Carried forward verbatim, not folded: segments and deltas flushed after the snapshot, tombstones accepted after the snapshot (r1's rule covered only segments/deltas — folding away a post-snapshot tombstone while the entity survives in the folded base is fail-open, finding 5), the active suppression set, and all unfolded overlay entries — source: lifecycle §5.3
- The new prefix's first side-manifest lists all of it; n continues; old prefix retention per §2.2 — source: lifecycle §5.3
- Publication-by-rebase resolves the merge-versus-flush race by construction: whatever completed work arrives, the lifecycle thread rebases it on the then-current generation, so concurrently flushed segments are carried forward automatically (§5) — source: lifecycle §1.3
- Compaction may force-refresh all fragments to advance the retirement floor; overlay size is the pressure gauge — source: lifecycle §3.2
- Compaction's fold obligation extends to evaluate entries and the carry-forward rule to post-snapshot tombstones and the suppression set (action raised against SA §6.6, applied 2026-07-28) — source: lifecycle Appendix R

## Generations and pins

- All of a partition's serving state hangs off a single atomically-swappable pointer (arc-swap) to an immutable Generation — source: lifecycle §1.1
- The request ordering invariant (load-bearing, tested): a request thread loads the generation pointer exactly once, at request start, before acquiring any fragment or cache entry, and works from that Arc throughout; overlay resolution happens-before fragment acquisition — source: lifecycle §1.1
- This ordering is what makes fragment eviction safe while a request still holds a fragment Arc — the request's own overlay still carries any deny the ledger has since retired; an implementation that refreshes a fragment mid-request or fetches one before resolving the generation leaks in the eviction→retire window — source: lifecycle §1.1
- A pin is an Arc<Generation>; superseded generations sit on a drain list — source: lifecycle §2.1
- Reclaim ordering: the lifecycle thread first removes the entry from the drain list, then verifies the strong count is one, then reclaims (closes exclusive mmaps, deletes retired-prefix files past their retention); verify-then-remove is the use-after-free the review caught, remove-then-verify is the fix, and it costs nothing — source: lifecycle §2.1
- A racing session-pin resolution that misses the removed entry gets 410 pin-expired — correct — rather than cloning an Arc mid-reclaim — source: lifecycle §2.1
- The router holds session pin id → per-partition (n, W) vector with TTL and a per-session cap; presenting a pin resolves each partition's entry against that worker's drain list; absent → 410 for the whole request — source: lifecycle §2.2
- A restarted worker serves no pre-restart pins; its drain list is empty and every pin touching it fails 410 — safe under I11 — source: lifecycle §2.2
- **Pin rule, quoted verbatim (lifecycle §2.3 heading and body):** "### 2.3 Pins fix geometry, never authorisation / A pinned request uses the pinned segments_version for tiles, columns and permutation, but composes M_auth against the current overlay — deny entries apply the moment they are accepted, pinned or not." — source: lifecycle §2.3
- "The observable consequence — a pinned drill-down can return fewer items than the viewport before it — is correct behaviour." — source: lifecycle §2.3
- The composition is coherent under mixed versions: M_auth is computed entirely in entity space with the fragment's watermark defining the live set, so every entity falls in exactly one of fragment \ L or direct_eval(L); projection through the pinned permutation then drops row-absent entities via the sentinel — no gap, no double count — source: lifecycle §2.3
- This is a recorded refinement, not a silent divergence (review finding 12): design §11.2's "a request pins its watermark alongside the segment-set version" and SA §6.4's "binds the triple to the fragment epoch at load" both read as if the pinned W participates in composition, when under this rule the effective watermark is the fragment's own and the pin-vector's W is advisory (status/debugging) — source: lifecycle §2.3
- Design §11.2 states the same rule in the architecture spec: "pins fix row-space geometry, not authorisation state (a suppression applies to a pinned request the moment it is accepted; see the concurrency and lifecycle design)" — source: design §11.2
- Fragments are built by request threads on miss (single-flight per key) from the current generation's postings view, never from a pinned one — consistent with §2.3: pins fix geometry, and a fragment is authorisation state — source: lifecycle §3.3
- The conformance suite carries the interleaving test: delete → retire → pinned request misses cache → assert the rebuilt fragment excludes the item — source: lifecycle §3.3

## The three retirement rules

- **Retirement rules, quoted verbatim (lifecycle Decision 5, §9):** "Three retirement rules, not one (amended r2): deletion denies by the epoch ledger with an insertion floor; suppressions only by unsuppress; evaluate entries at their compaction fold. r1's single rule was fail-open for two of the three." — source: lifecycle §9
- **Table form, quoted verbatim (lifecycle §3.1 table rows):**
  - "deny/deletion | tombstone epoch d | yes — delta-tier tombstone at d; folded at compaction | ledger rule, §3.2"
  - "deny/suppression | — | never — suppression does not touch postings | only by unsuppress. Non-retirable while active, by construction: no fragment rebuild ever excludes a suppressed entity, so its invisibility rests on the overlay entry for as long as the suppression stands"
  - "evaluate (predicate change) | current term set, inline (§11.2) | not until compaction folds it — deltas cover newly flushed entities only, so a change to an existing entity's terms is invisible to postings in both directions | fold epoch rule, §3.4"
  — source: lifecycle §3.1
- r1 assigned every deny a retirement epoch; for suppressions that is fail-open (any epoch eventually retires the entry and re-exposes the item) and the disposition split is the fix — source: lifecycle §3.1
- Suppression count is a metric — a monotonically growing active-suppression set is a policy signal, not a leak — and unsuppress removes the entry and publishes a side-manifest immediately, as all deny-state changes do — source: lifecycle §3.1

### Deletion-deny retirement mechanism (epoch ledger)

- A deletion's deny entry (tombstone epoch d) may leave the overlay only when no servable fragment epoch predates d — source: lifecycle §3.2
- Scope: all of this is per partition, per worker, in memory — epoch_counts and the floor are worker-local structures, and losing them on restart is safe by construction: the cache restarts cold and §3.3 forces every rebuild from current postings — source: lifecycle §3.2
- The fragment cache maintains epoch_counts: BTreeMap<postings_epoch, usize>; min_live_epoch() is its first key (+infinity when empty) — source: lifecycle §3.2
- The cache additionally tracks retirement_floor = the highest epoch of any retired overlay entry — a deletion's tombstone epoch d or an evaluate entry's fold epoch f alike — and refuses insertion of any fragment with epoch < retirement_floor — source: lifecycle §3.2
- Without the floor, a pinned or slow request could rebuild an old-epoch fragment after the entries predating it were retired and resurrect a deleted item or a revoked term (finding 2, and the fold-variant of the same race) — source: lifecycle §3.2
- The floor is deliberately defined over both retirement kinds: a floor raised only on deletion retirements passes the deletion test and still fails open through a pre-fold fragment — source: lifecycle §3.2
- Refusal is cheap: the builder retries against current postings (§3.3) — source: lifecycle §3.2
- The lifecycle thread retires the retirable-entry prefix below min_live_epoch() after evictions and periodically — source: lifecycle §3.2

### Evaluate-entry fold retirement mechanism

- A predicate change is invisible to postings until compaction folds it: compaction rewrites affected entities' postings from the term sets carried in their evaluate entries (an obligation recorded in SA §6.6) — source: lifecycle §3.4
- After the fold, the entry carries its fold epoch f and retires under §3.2's rule — with f participating in the retirement floor exactly as a deletion's d does — source: lifecycle §3.4
- A fragment predating the fold misreads the entity in both directions (a revoked term still present: fail-open; a granted term absent: wrong counts), so the same min-live-epoch machinery governs it — source: lifecycle §3.4
- Before any fold, evaluate entries are immortal, which is why overlay growth under predicate churn schedules compaction, not just fragment refresh — source: lifecycle §3.4

### What starts/retires each rule, and what conflating them would break

- Deletion-deny: started by the deletion (deny entry stamped with tombstone epoch d, delta-tier tombstone applied immediately); retired only when the fragment cache's min_live_epoch() rises above d (no servable fragment epoch predates d), enforced by the retirement_floor refusing insertion of any fragment with epoch < floor — source: lifecycle §3.1, §3.2
- Suppression: started by an administrative suppress action (deny entry added, never touches postings); retired only by an explicit unsuppress action removing the entry and publishing a side-manifest immediately — no epoch, no fragment-cache condition, no compaction event retires it — source: lifecycle §3.1
- Evaluate/predicate-change: started by a predicate change (entry carries the current term set inline, immortal until folded); retired only at the entity's next compaction fold, which rewrites postings from the entry's term set and stamps a fold epoch f that then participates in the same epoch-ledger floor as a deletion's d — source: lifecycle §3.1, §3.4
- What goes wrong under a single mechanism: r1's single retirement rule assigned every deny a retirement epoch, which is fail-open for suppressions specifically — "any epoch eventually retires the entry and re-exposes the item" — because a suppression carries no postings-side signal (no tombstone, no fold) to re-derive from, so an epoch-based timer expiring it resurrects the suppressed item with no unsuppress ever having occurred — source: lifecycle §3.1
- Relatedly, a retirement floor raised only on deletion retirements (i.e., treating deletion and evaluate-fold as one mechanism and ignoring the other) "passes the deletion test and still fails open through a pre-fold fragment" — a fragment built before an evaluate entry's fold could survive past when the entry logically retires, misreading the entity in both directions — source: lifecycle §3.2
- The row that must never exist, stated generally: "any path that loses or re-exposes an acked deny"; §4's WAL ordering, §3.1's suppression rule, §3.2's insertion floor and §5.3's tombstone carry-forward each close one such path found in review; all four carry conformance tests — source: lifecycle §8

## The WAL and its ack contract

- Per partition, single appender (the lifecycle thread), append-only records (postcard, length-prefixed, CRC per record): IngestBatch{batch_id, body_hash, rows, allocated entity IDs}, Change{external_id, op, term_set | tombstone_epoch}, Lease{lo, hi}, Flush{n, wal_pos} — source: lifecycle §4
- Ack ordering, stated fully: WAL fsync → overlay/generation swap → 200; the swap is nanoseconds and sits before the ack so a caller's own next request always observes its accepted change; the crash window "after fsync, before swap" recovers by replay and was never acked — harmless — source: lifecycle §4
- Recovery: replay from the last Flush; CRC failures distinguish position — in the unsynced tail (past the last fsync point), truncate, since those records were never acked — source: lifecycle §4
- At or below the last fsync point, a CRC failure is corruption of acked state — including possibly denies — and recovery fails closed: the worker stays unready and the operator restores from bundle + object store; truncate-at-first-bad-CRC applied mid-log would silently drop acked denies — source: lifecycle §4
- Disk-full vs never-429: the ingest 429 threshold is set strictly below the WAL's hard bound, reserving headroom so change records always have room — source: lifecycle §4
- If an append genuinely fails, the deny is applied to the in-memory overlay and swapped (visible immediately), the response is 500 with an alarm — durability is owed and the caller must retry — never a 200 without fsync, never a silent drop, and never a refusal that leaves the item visible — source: lifecycle §4
- Side-manifest publication is gated on WAL durability: the 500 path publishes nothing durable, so a replica can never observe a suppression that a subsequent crash-replay would silently remove — source: lifecycle §4
- One lifecycle thread per partition owns all mutation decisions but performs only cheap operations itself: command-queue drain, WAL append and fsync (group commit permitted), and pointer swaps — source: lifecycle §1.3
- All file and object IO — segment writes, digests, side-manifest and object-store publication, merge execution — runs on a background pool, submitting a completed, immutable result back to the lifecycle thread for a swap-only publication step — source: lifecycle §1.3
- The command queue has a priority lane for deny-disposition changes, so a compaction publish or a stalled object-store PUT can never queue a suppression behind seconds of IO — deny visibility latency is bounded by (queue-front + fsync), nothing else — source: lifecycle §1.3

## The router/worker protocol

- Internal, versioned by the binary; postcard frames over unix socketpairs; per-request deadlines; heartbeats — source: lifecycle §6
- Messages as r1 (Hello/Ready, BuildFragment, Query, Changes/IngestRows, Publish, Heartbeat) with one addition from review — source: lifecycle §6
- Hello carries the worker's allocation high-water, and the worker's WAL always wins; on (re)connect the router advances its allocator journal to max(journal, every reported high-water) before granting any lease — source: lifecycle §6
- This arbitrates divergent replay — a router journal restored from an older backup would otherwise re-grant ranges workers already consumed, an I9 violation with I5-scale blast radius; the worker's WAL is authoritative because it records IDs actually written — source: lifecycle §6
- Failure semantics: worker timeout fails the request (I13's outage asymmetry — never empty); respawn with backoff, rebuild from bundle + WAL, fragment cache cold and rebuilt on demand from router-retained auth data; pre-restart session pins fail 410 (§2.2); router exit kills workers via the supervision-pipe watchdog — source: lifecycle §6
- Prefix retention is recorded durably in the local cache, not the bundle: deciding a prefix is retired is router-level knowledge (session pins span partitions), so the router writes retired/<prefix>-<timestamp> under the engine's local cache directory when the last generation referencing a prefix drains — source: lifecycle §2.2
- Local copies of a non-current prefix are deletable only after (marker + session-pin TTL); the marker deliberately does not live in the bundle — source: lifecycle §2.2
- Threading model: tokio for HTTP and sockets; one lifecycle thread per partition; rayon for CPU-heavy request work (unions, gathers, selection); the engine's public API is sync and owns no executor (embeddability, SA §1) — source: lifecycle §7
- Caches are concurrent maps with single-flight build; entries immutable, keyed by §8.5's keys verbatim; invalidation is key rotation, never mutation — source: lifecycle §7

## Crash matrix (selected entries relevant to durability/authorisation)

- Worker crash: respawn; bundle + WAL; pins on it 410 (pins affected, by design) — source: lifecycle §8
- Router crash: watchdog kills workers; supervisor restarts; allocator re-arbitrated from worker high-waters (§6) (sessions affected, by design) — source: lifecycle §8
- Mid-log WAL corruption (below fsync point): fail closed; restore from bundle + object store (availability affected, never denies) — source: lifecycle §8

## Expected-but-absent claims

- None. Every requested topic (ingest boundaries, WAL/ack, buffer/watermark/overlay, segments/merge/compaction and merge-vs-snapshot, generations/pins with the exact geometry-not-authorisation rule, all three retirement rules verbatim with their starting/retiring events, and the router/worker protocol) was found explicitly stated in the two source documents.
