# Compaction review — invariants and fail-open lens

**Status:** Evidence — review transcript, never normative. An independent adversarial review of
`docs/design/compaction.md` (r2, provisional, 2026-08-05), against architecture §4 / §10.2 /
§11.1–§11.3 / Appendix C, write-path §4–§9 (normative for the write path), contracts §2.1–§2.6,
SA §6.6–§6.7, and decisions 0040–0048. Not dispositioned. Findings are ranked; the
attacks-that-failed section is kept in full so they are not re-run.

The draft survived most of what was thrown at it. **F1 is a fail-open that serves an acknowledged
deletion**, and it is not closed by anything in the document; F2, F3 and F5 are holes of the same
family — the draft's obligation lists are each one item short. The rest are corrections to claims
the body makes.

---

## Findings

### F1 — Rule F retires on the *plan's* tombstone set, not on what the fold removed; a deletion accepted before the snapshot whose row a flush in flight then writes is retired with its row and postings intact (fail-open, serves a deleted item, permanent)

**What the draft says.** Spec §2's table: `deleted` is *"**executed**: rows dropped, postings
dropped, entries retired"*, and `tombstones` published is *"`live deleted − executed`"*. Spec §7:
*"Deny during the fold: … A delete is post-snapshot, so its entity keeps its row in the folded
base and its tombstone is carried forward."* Spec §12's obligation 2 tests exactly and only the
post-snapshot case. Appendix R r1 claims write-path §8's obligation *"the deletion accepted after
a flush snapshot whose row exists (spec §4.2)"* is answered by spec §2 — it is not. Spec §2
answers deletions accepted after the **fold's** snapshot. write-path §4.2's obligation is about
deletions accepted after a **flush's** snapshot, which is a different set, and the draft never
addresses it.

**The interleaving.**

1. Tick *N*: `plan_flush` snapshots the buffer for slice S. Entity E is buffered and not deleted,
   so E is in the plan. Dispatched to the pool.
2. The pool run spans the tick: the segment write is queued behind a merge or a coalesce
   (write-path §7 — *"Nothing bounds the **sum** when a flush, a merge and a coalesce overlap on
   the pool"*), or it is simply large.
3. A delete for E is accepted and applied to the live generation. `deleted ∋ E`. The flush's plan
   is a value taken from an earlier generation and cannot see it — pinned by
   `flush.rs::a_delete_arriving_after_the_plan_does_not_unwrite_the_row`, whose own doc says this
   is *"an obligation the compaction spec inherits."*
4. Tick *N+1*: the flush is still in flight, so the flush tick skips. The fold trigger fires. The
   fold plans: it names the files in the **live manifest** — which does not contain the in-flight
   flush's segment — and clones `deleted`, so `D₀ ∋ E`.
5. The flush completes and publishes by rebase into the old prefix. It writes **E's row** into its
   segment and **E's postings** into its `delta.arrow` tier.
6. The fold publishes. Its executed set is `D₀ ∋ E`, so E leaves `deleted` (spec §4.6) and E is
   absent from `tombstones` (`live deleted − executed`). Its segment and tier are *post-snapshot*
   and are **carried forward verbatim** (spec §2), so E's row and E's postings survive.
   `denied[slice]` is re-derived from `deleted ∪ suppressed` — after retirement — so E is not in
   it either.

**Result.** E has a live row, live postings, no overlay entry, no tombstone and no deny-mask bit.
It is drawn in every viewport, counted in every tile, returned by drill-down and served to every
principal authorised for its terms — permanently, and across restarts, since the new manifest is
the durable record. An acknowledged deletion is silently undone. This is the exact failure Rule F's
safety argument exists to prevent (write-path §5.4, architecture §11.3 r33), reached by a route
the identity match cannot see: no fragment is stale here, and rotating the fragment identity does
nothing, because the entity genuinely *is* in the post-fold postings.

The same root cause has a second instance: any member of `D₀` whose row lives in a file the fold
did not consume. Today the in-flight flush is the only producer of one; the rule as written admits
any future one.

**Why the draft's arithmetic cannot close it.** `executed` is defined by the *plan* (a clone of
`deleted`) while the removal is defined by the *files*. The two sets differ whenever a row for a
planned tombstone lands in a file the plan did not name.

**What would close it.** Define retirement by what the fold demonstrably removed, checked against
the **published** row space:

```
executed = { e ∈ D₀ : row_of_new(e) is None }
tombstones = live deleted − executed
```

computed on the executor at publication, after the carry-forwards are rebased. It costs |D₀|
permutation probes, it subsumes the post-snapshot case the draft already handles, and it is
robust to any future carry-forward category, because it asks the row space rather than the plan.
An entity with a row in a carried segment necessarily has its postings in that segment's carried
tier, so one condition covers both halves of r33's *"both halves, or neither"*. Spec §12's
obligation 1 should then be run with a flush deliberately held in flight across the fold's
snapshot, which is the case obligation 2 does not reach.

---

### F2 — the enumeration of pre-fold fragment holders is incomplete; `freshest_fragment` is prefix-blind, and the row-projection cache holds fragments outside `FragmentCache`

**What the draft says.** Spec §4: *"Swapping the whole cache — rather than adding identity to its
key — additionally makes a pre-fold in-memory fragment **unreachable by construction**"*, and gap
2 is stated as *"A **session's** cached fragment is valid only when that identity matches the
generation's."*

**Why it is false as stated.** A `FrozenFragment` is reachable from three places, not one:

- `FragmentCache`'s slots (the draft's target);
- `Session::fragment`, an `Arc` the *server* holds per session (gap 2's target);
- **`SessionGeometry::fragment`**, an `Arc` carried inside every row-projection cache entry, which
  `Engine::item` reads through `RowProjectionCache::freshest_fragment(token_id)` — and that
  function takes `max_by_key(segments_version)` over the token's entries **ignoring
  `key.prefix`**.

Post-fold, `prune_generations_below(live − 1)` keeps entries at the pre-fold version, so a token
whose pre-warm did not land (a session established after the pre-warm pass; an entry the pass
skipped on `FragmentCacheError::Building`; a fragment build that failed and `continue`d) has its
pre-fold entry as the freshest, and the drill-down composes a **pre-fold fragment** against the
post-fold generation, with the deletion already retired.

**How far it bites today, stated honestly.** For the row-space verbs it is bounded: rung 1 and
rung 2 of `session_geometry` both key on `prefix` (`RowProjectionKey`'s fact 3, kept by decision
0041 for exactly this), and a folded-away entity's `perm[e]` is the sentinel, so projecting a
superset fragment yields the same rows. For `Engine::item` the pre-fold fragment says *visible*
and the row lookup then returns `Ok(None)` — the same 404. **Both of those bounds are the
permutation, and F1 is the case where the permutation still holds a row.** With F1 open, the
prefix-blind read is a second route to the same disclosure. And neither bound exists for the
entity-space verbs the draft's own §11 defers: §7.5's cluster visibility and §7.6's label gate
answer from `verdict` and `fragment.contains` with **no row lookup at all**, so Phase 3 acquires a
straightforward fail-open the moment it lands.

**What would close it.** Two things the draft should name rather than one: (a) the identity check
belongs on the *fragment* (or on every holder of one), not on `Session::fragment`'s watermark
comparison alone — `freshest_fragment` must reject an entry whose generation identity is not the
live one, and the cheapest expression is to give it the prefix/identity filter the refresh already
applies; (b) spec §12's obligation 3 must name all three holders — it currently names the in-memory
memo, the persisted files and a session's held fragment, and omits the row-projection cache's.

---

### F3 — the fold's publication does not re-check the poisoned / diverged / stepped-down gates, and a prefix flip is the one publication a step-down cannot be walked back from

**What the draft says.** Spec §9 gates the **trigger**: *"refused while the WAL is poisoned, while
the overlay is diverged, and while any partition is stepped down."* Spec §4's publication sequence
checks the manifest presence relation and the merge-size relation, and nothing else.

**Why that is not enough.** write-path §4.2's rule is *"Two gates, checked before any work **and
again at publication**"*, and the code honours it (`write.rs`'s flush publication and
`publish_overlay_state` both re-test `wal.is_poisoned()`, `overlay_diverged` and
`stepped_down()`). A flush's plan-to-publish window is seconds; the fold's is *minutes to hours*
(spec §14), so the probability that the node's posture changes across it is not comparable, and
the fold's publication does strictly more than a flush's: it writes a MANIFEST **and** a
SEGMENTS-*n*, **and** it mutates the overlay.

Three concrete outcomes:

- **Diverged.** Publishing a SEGMENTS-*n* whose `deny`/`tombstones` come from an overlay holding
  dispositions no record backs makes a 500'd, never-acked deny permanent on every restore —
  write-path §4.2's stated reason for the gate, unchanged.
- **Poisoned.** The fold's retirement is applied in memory and the WAL cannot record anything; the
  rotation spec §5 relies on for durability is itself gated, so the node ends with a published
  manifest whose `tombstones` omit entries the WAL still holds and no route to reconcile.
- **Stepped down — the worst of the three.** Step-down works by walking `SEGMENTS-<n>` candidates
  **within the current prefix** (contracts §2.3). A fold flips `CURRENT` to a new prefix
  containing exactly one side-manifest, so there is nothing to step to and nothing to step past:
  the fold does not merely publish from a shadowed state, it destroys the only recovery material
  the step-down mechanism has. `readyz` fails unconditionally while any partition is stepped down
  (write-path §5.6), so this is reachable whenever a step-down arises *during* a multi-hour fold.

**What would close it.** Spec §4 gains a step 0: re-check poisoned, diverged and stepped-down on
the executor immediately before the hard-link/MANIFEST sequence, and discard the fold if any
holds — the same discard the presence check already produces, and the same alarm shape. Add it to
spec §7's interleaving list and to spec §12 as an obligation.

---

### F4 — the pre-swap refresh is invalidated by any flush between the pre-warm and the swap, because the fragment key carries the watermark; the published pair is then write-path §4.6's F5 defect, structurally prevented there and reintroduced here

**What the draft says.** Spec §6: the candidate generation is *"constructed but not published, the
refresh runs against it, and the generation pointer and the filled cache are published together …
after the swap the cache is already warm, so there is no window to shed in"*, with one stated
mid-flight answer: *"an extent that landed meanwhile is unioned into each pre-warmed projection at
publication — the measured 0.24 ms rung-2 primitive."*

**Three things break it, and the first is certain rather than possible.**

1. **The fragment cache key carries the watermark.** `canonical_key` hashes
   `bundle_identity ‖ auth_plugin_hash ‖ watermark ‖ sorted term ids`. A pre-warm at watermark
   *W₁* produces fragments keyed at *W₁*. Any flush between the pre-warm and the swap advances the
   watermark to *W₂* and publishes a tier the pre-warmed fragment does not union. At a 90 s tick
   and a fold measured in minutes-to-hours, at least one such flush is **certain**. Unioning the
   new extents into the projection does not repair this: `RowProjection::extend` projects the
   *same* fragment over the new extents, and the fragment does not contain the newly flushed
   entities, so the new rows contribute nothing.
2. **The published pair is a stale fragment under a live key.** Rung 1 of `session_geometry`
   serves the cached pair *without* any watermark test, so every session is served `(F@W₁, P)`
   until the next geometry publication's refresh. That is exactly the defect write-path §4.6
   records as review finding F5 — *"a projection derived from a stale fragment but inserted under
   the new `segments_version` key would pin the session's freshly flushed items invisible until
   the next publication, silently falsifying §4.1's ack→visibility bound"* — and the pair-in-one-
   entry design was adopted to make it *unexpressible*. The fold's pre-warm expresses it again, by
   building both halves early. Fail-**closed**, bounded at one tick; but the corpus treats this
   one as structural rather than tolerable, so the draft should not reintroduce it silently.
3. **The projection key's `segments_version` is not knowable at pre-warm time.** A flush landing
   between pre-warm and publication takes the version the fold predicted, so the fold's own is one
   higher and the pre-warmed entries sit at *version − 1*. They are then reachable only through
   rung 2's stale-serve, which is the same F5 outcome by another route. This is the plan-time-`n`
   hazard write-path §4.4 exists to close, in a new guise: **nothing about a publication may be
   decided at plan time.**

Taken together, D2's justification does not survive its own timeline: the pre-warm's 4 550 ms per
resident entry is largely spent on entries that are stale before they are published.

**What would close it.** State that the pre-warm's keys are assigned at publication, not at
pre-warm; and make the last step before the swap a fragment rebuild at the **live** watermark
(a measured ~200 ms per credential, flat in tier count — bounded, unlike the projection) with the
projection then extended over the mid-flight extents from *that* fragment. That is the only
arrangement in which the pair the swap publishes is internally consistent. If the flush tick can
land inside even that shorter window, the fold must either hold the tick for its duration or fall
back to 0044's ordinary post-swap refresh with `refresh_in_flight` armed before the swap — which
is the bounded-429 residual the ruling already permits, and is preferable to a silently stale pair.

---

### F5 — spec §3's re-ingestibility claim is inverted: the fold's own retirement makes a folded-away external id **less** re-ingestible, not more

**What the draft says.** Spec §3, pass 3: *"Dropping a folded-away entity's key is what finally
makes a deleted external id re-ingestible without a forgotten holder in the way, and it is sound
for the same reason the duplicate check exempts a deleted holder."*

**Why it is wrong.** The duplicate check is `established_collisions(&rows, |e|
generation.overlay.is_deleted(e))` — it exempts a holder **only while the overlay says the holder
is deleted** (decision 0047). The fold's retirement removes exactly that condition. The live
`established` map is replay-derived and nothing ever removes an entry from it, so after the fold:

- before the fold, re-ingesting external id X (held by deleted E) was **accepted** — the exemption
  fired;
- after the fold, E is no longer deleted, `established[X] = E` still stands, and the same
  re-ingest is **refused 409** — for the process's lifetime.

Dropping X from the sidecar run does not help: the live map is consulted first, and it is the map
that holds the stale binding. This refuses a user's write, which decision 0047 rules out in terms
(*"it never refuses a user's write"*), and it is the fold that causes it.

**What would close it.** The swap must remove the retired entities from `established` and
`established_inverse` — a fourth thing the widened publication signature carries, beside the
external-id index. It is exact (the fold has the entity set) and it is the point at which the
sidecar drop and the live map agree.

---

### F6 — spec §8's reclamation is written as description, but the mechanism it waits on was deleted by decision 0041 and is unbuilt, unmarked and unallocated

**What the draft says.** Spec §6: *"Prefix deletion waits on that (lifecycle §2)."* Spec §8: *"once
no request holds a mapping of it the tree is deleted in one operation."* Spec §10 gives it no row.

**What the corpus says.** lifecycle §2.3 carries a ⊘ marker: *"There is no router, no `RETIRED`
marker is written, and no local prefix copy is ever deleted. That is safe **only** while nothing
deletes a file — the moment a stage adds prefix deletion, the marker and its `Weak` registry must
land in the same change."* Decision 0041 says the same and prices it (*"Roughly 100 of the deleted
lines come back the day prefix deletion lands. Scoped to prefixes (compactions)"*). This is that
stage.

**Why it matters more than a marking nit.** Deleting a prefix whose files a live request still has
mmapped is not a stale answer, it is a `SIGBUS` or a torn read on the serving path —
`Arc::strong_count` is a sample, not an event, so there is no cheap correct version of "wait until
nobody holds it". Reclamation is obligation 2 of spec §0, so the fold cannot claim to discharge it
without owning this. Decision 0013 also applies: the paragraph reads as an assurance about
machinery that does not exist.

**What would close it.** Mark it ⊘ at the claim, give the `Weak` registry, the poller and the
`RETIRED` marker a row in spec §10, and add spec §12 an obligation asserting that no file the old
prefix names is unlinked while a request holds a mapping of it.

---

### F7 — spec §5's "harmless and self-healing" understates what the resurrected entry is doing; it is load-bearing, and two unstated properties are what make the fold safe at restart

**What the draft says.** Spec §5: a restart before the post-fold rotation *"resurrects the retired
entries"*, and *"That is harmless and it self-heals: a resurrected entry names an entity with no
row and no postings, so `verdict` denies something nothing can reach."* Spec §12's obligation 15
asserts the resurrection is harmless.

**What actually happens.** The fold turns every folded-away entity into a **candidate for buffer
reconstruction**. `WritePath::reconstruct` rebuilds the buffer as *"the replayed rows whose entity
has no row in any segment"* (write-path §9's exact predicate, deliberately not the watermark
proxy). Pre-fold, a deleted-then-flushed entity E has a row and is filtered out. Post-fold, E has
**no** row, so replay re-buffers it. The only thing that then removes it is
`overlay::drop_deleted`, which fires because the resurrected `Delete(E)` put E back in `deleted`.
So the resurrection is not a harmless residual — it is the mechanism keeping E out of the live
buffer, and if it were absent E would be buffered as a live item and the next flush would give it
a fresh row. The manifest cannot help: the fold's `tombstones` deliberately omits E.

**Why it is nevertheless safe today** — two properties, neither stated anywhere:

1. `IngestBatch(E)` is necessarily **older** than `Delete(E)`, rotation deletes members
   oldest-first and only *wholly below* the reclaim bound, so the delete record can never be
   reclaimed while the ingest record survives.
2. `Overlay::apply_snapshot` is *"Applied, never assigned"* — a rotation snapshot that omits E
   cannot cancel an earlier `Delete(E)` still in the durable prefix.

Change either — selective reclamation, or a snapshot that becomes an assignment — and the fold is
fail-open at restart.

**What would close it.** Say this at the site rather than calling the state harmless, and restate
obligation 15 as the property that actually needs pinning: *a restart between the fold's swap and
the following rotation must not reconstruct a folded-away entity into the ingest buffer*, with the
two properties above named as its premises.

---

### F8 — spec §6's "2× the row-projection cache's own byte budget" is not something a bounded LRU cache can hold, and the failure mode is decision 0043's forbidden inline build, during the fold

**What the draft says.** *"At most one new entry per resident entry, so peak residency is 2× the
row-projection cache's own byte budget, and its LRU decides which sessions get the smooth path."*

**Why it does not work.** `RowProjectionCache` has a hard byte bound with LRU eviction
(`single_flight`'s rule set); it cannot hold 2× its bound. Inserting the pre-warm into the same
cache means the pre-warm's entries and the **pre-fold entries that are still the only thing
serving live requests** compete for one budget, for the whole of the fold's flight. An evicted
session's next request finds rung 1 and rung 2 empty, `refresh_in_flight` false (no refresh is in
flight — the pre-warm is not one, and if it were armed for hours every racer would 429 for hours),
and takes rung 3 as a **build**: the measured 4 550 ms, on the request thread, caused by
maintenance. That is precisely what decision 0043 forbids, produced by the mechanism chosen to
honour it. The fragment side doubles too, against `FragmentCache`'s separate `set_memory_bound` —
`SessionGeometry`'s weight function deliberately excludes the fragment because it is charged
there.

**What would close it.** Give the pre-warm its own staging structure with its own budget, swapped
in at publication rather than inserted into the live cache; and give the fold a **free-memory**
precondition beside spec §8's free-disc one, since the transient is now two cache budgets plus the
fold's own working set. Name the operator-visible step in spec §9's table.

---

### F9 — `pairs.parquet` re-emission narrows the conformance disagreement rather than closing it (low)

Spec §3's pass 2 argues the file cannot be carried forward because *"it would then disagree with
the base postings about every folded deletion."* True, but the fold's own emission is built from
base ∪ **snapshot** tiers − tombstones, while the published bundle serves base ∪ **carried-forward**
tiers. Every entity flushed during the fold's flight is in the postings and absent from the pairs
relation, so the I1 differential fails on them for the same reason. (The pre-existing form of this
— that no flush has ever emitted pairs — is not the fold's to fix, but the draft should say what
the file's scope is rather than imply the fold restores conformability.)

### F10 — the MANIFEST fields the fold must carry byte-verbatim are unstated (low)

The fold is the first writer of a `MANIFEST.json` that is not a `tessera build`, so nothing else in
the corpus states this. At minimum: `identity` in full — `key`, `shard_id` and above all `idset`,
which contracts §2.2 makes monotone-and-never-reset because both consumers (decision 0025's
`/v1/meta` poll and `/control/changes`' tessera-address guard) are defeated by a reset — plus
`quantisation` (decision 0040 makes it immutable for a slice's life), `data_plugin_hash`,
`declared_bounds`, `declared_scalars` and `small_term_threshold`. Spec §6's *"Nothing a client
holds breaks … `idset` does not advance"* states the consequence without stating the obligation.

### F11 — spec §8's persisted-fragment sweep is not implementable against the current on-disc layout (low)

*"the persisted fragment-cache directory is swept of entries under superseded identities (nothing
else will ever name them)"*. A cache entry is `<hex canonical key>.frag` plus a `.meta` sidecar
carrying `watermark ‖ frozen_len ‖ sha256(frozen)`. The canonical key is a SHA-256 **over**
`bundle_identity`, so nothing on disc records which identity produced a given file, and the sweep
cannot select. Either the layout becomes `<hex identity>/<hex key>.frag` or `.meta` gains the
identity; spec §10 allocates neither. (Sweeping the whole directory at the flip is not the answer
while the pre-warm has just written entries into it under the new identity.)

### F12 — the draft is written to decision 0048, which has not been swept through write-path (low, corpus coherence)

Spec §2 asserts *"There is no evaluate arm, and there is not going to be one"*, but write-path
§5.3's table still lists the `evaluate` store and §5.4's Rule F still reads *"entries leave
`deleted` and `evaluate` only at the compaction fold that **executes** them."* write-path is
normative and the draft must not contradict it. Fail-closed either way (nothing retires), but
promotion needs the 0048 sweep to land first or write-path to be amended in the same change —
otherwise the corpus carries a normative requirement for a fold pass the provisional design
deletes.

---

## Attacks that failed (recorded so they are not re-run)

- **`delete → suppress → unsuppress` across the fold.** No ordering found. `suppressed` is
  serialised fresh from live state at publication (spec §2), the fold gives a suppression no
  retirement route, `derive_denied` re-derives from the union rather than subtracting, and the two
  stores are of two types. An unsuppress landing in the fold's flight, immediately before the
  swap, and immediately after it were each traced: all three end with the correct set.
- **A suppression retired as a side effect of Rule F firing beside it.** No. `denied` is derived
  from `deleted ∪ suppressed` *after* retirement, and the manifest's two fields are taken
  separately (write-path §5.6's rule, unchanged by the fold).
- **A deleted-while-buffered entity resurrected by retirement.** Closed:
  `tessera_lifecycle::overlay::drop_deleted` removes the row from the buffer at the delete's own
  apply, so by the time the fold retires the entry there is nothing left to un-hide. Note that
  `drop_deleted`'s own doc justifies itself partly with *"a deletion never retires (Rule F; the
  fold does not exist)"* — the justification changes with the fold, the conclusion does not.
- **Retirement lost while the entity's ingest record survives, re-buffering it as live at
  restart.** Not reachable: `IngestBatch(E)` is never newer than `Delete(E)`, rotation deletes
  members oldest-first and only wholly below the reclaim bound, and `apply_snapshot` folds rather
  than assigns. Recorded as F7 because the fold now *depends* on all three and none is stated.
- **A pre-fold row projection served across the flip.** Closed by `RowProjectionKey.prefix` —
  rung 1 and rung 2 both carry it, which is `RowProjectionKey`'s fact 3, kept by decision 0041 for
  exactly this case. Rung 2's `extends_to` would also refuse, the base permutation having changed.
- **A pre-fold *fragment* producing rows for folded entities.** Closed by the permutation: a
  folded-away entity's `perm[e]` is the sentinel, so projecting a superset fragment into the new
  row space yields the identical row set. This is why F2 is bounded today and why F1 is what
  unbounds it.
- **A post-fold fragment leaking into the pre-swap window** (`freshest_fragment` picking the
  higher `segments_version` while the old generation still serves). Fail-closed: post-fold
  postings are a subtraction, so the post-fold fragment is a subset, and the deletions it lacks
  are still in `deleted` until the swap — both representations hide the same items, so the
  drill-down and the map do not drift (obligation 27 holds).
- **A merge or coalesce racing the fold's publication.** Closed as the draft says: spec §4.1's
  presence check over `seg_id`s and paths that are never reused (contracts §2.1) makes it
  ABA-safe, and the fold discards. The cost — hours of work thrown away by a maintenance pass that
  was already in flight when the fold was planned — is real but is not a hazard.
- **The staleness stamp or `x-tessera-stale` as a route across the fold.** No: advisory,
  broadcast, never a selector and never a refusal (decision 0041); C15 covers the channel and the
  2026-08-02 ruling accepts it.
- **`dict.len()` decreasing across the fold, breaking `Session::is_stale`'s monotone counter or a
  session's granted `satisfied` ordinals.** Closed by pass 4: extents carried verbatim, never
  renumbered, never shrunk, and the live `Arc<Dict>` carried onto the new generation.
- **C4's identical-outcomes closure across the fold.** Holds: a folded item's `/v1/items` takes the
  same three constant-time entity-space probes as an identifier naming nothing and returns the same
  `Ok(None)`; the row lookup and the sidecar are reached only after the answer is already
  *visible*.
- **I9 across the fold** — `entity_id_high_water` passed through live, no renumbering, a
  folded-away id burned, `permutation.bin`'s `bound` shrinking to max-folded-entity + 1 with
  post-snapshot entities addressed by extents. No route found by which an id is reissued or an
  extent's entity range is misaddressed.
- **`check-layers.sh`'s single-publisher rule.** Unaffected: the fold publishes through the
  executor like every other publication, and adds no second non-atomic generation store.
