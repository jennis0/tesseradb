# Flush and Merge — Design

**Status:** Draft r3 — r1 was independently reviewed (verdict: needs-rework; the
publication/rebase/one-cadence architecture survived, the WAL's reclaimability did not); r2 closed
all eighteen findings; r3 adds mask staleness (§3.3). **§3.3 and every r2 section have had no
independent review.** Not yet normative — `docs/design/` wins until this is folded in.

**Owns:** the row-space segment lifecycle. Flush turns WAL-durable buffered items into a published
segment, which is what makes an ingested item visible at all; merge bounds the segment and
delta-tier counts flush would otherwise grow without limit. Both are **invariant-neutral**: neither
folds authorisation state, neither retires an overlay entry, neither can re-expose anything.

**Does not own:** compaction and its fold, the deletion stamp ledger, the retirement floor, the
evaluate-entry fold. Those are the invariant-bearing half of the write journey and are specified
separately, after this lands, because their inputs are artefacts this document creates.

`§n` unprefixed refers to `docs/design/architecture.md`. `lifecycle §n` refers to
`docs/design/concurrency-lifecycle.md`, `contracts §n` to `docs/design/contracts.md`, `SA §n` to
`docs/design/system-architecture.md`.

---

## 0. What this closes, and the one thing it is not

Epic #3 states the condition plainly: ingest is durable and invisible. An acknowledgement is a
durability receipt, and the gap between it and visibility is unbounded, because a buffered item has
no row in any segment and every map verb asks a row-space question (§11.2).

Flush ends that condition. Merge is not a separate ambition — it is the cost control without which
flush is a serving cliff, on **two** axes: a tile resolves to one contiguous range per live segment
(§11.3), and a fragment build unions across every live delta postings tier. A 90 s flush period
produces roughly a thousand of each per day.

**What this is not:** it is not the mechanism that lets anything retire. After this lands, deletion
denies still never retire, evaluate entries are still immortal, and the overlay still grows
monotonically under deletion and predicate churn. That remains fail-closed and remains not the
specified mechanism. Compaction owns it.

**And one thing this makes load-bearing.** Under §11's ruling, `tessera build` is initial-load only.
Compaction therefore becomes the *only* route to re-quantisation, the fold, re-ranking and a
batch-grid change — no longer merely desirable, but the deployment's sole reorganisation path.

## 1. Publication: one owner, one cadence

### 1.1 The single publisher

Flush and merge execute on the background pool over immutable inputs and submit a completed,
immutable result to the write executor for a **swap-only** publication step (lifecycle §1.3).

This is what closes #59. Today there are two publishers: the write executor, and geometry
publication which swaps the pointer from any caller. `crates/tessera-engine/src/write.rs` says so at
the publication site and names the reason — *"a flush would be precisely a second publisher"*. It
would be a third. So geometry publication ceases to be callable off-thread and becomes a command;
the engine keeps exactly one non-atomic `.store(`, and `scripts/check-layers.sh` goes on policing
that.

Completed units arrive on the **work** lane, never the deny lane. The loop's existing discipline —
drain deny to empty before touching work — is what keeps a suppression from queueing behind a
flush's IO, and is unchanged.

**At most one flush and one merge are in flight at a time.** A tick arriving while a flush runs is
skipped, not queued: two concurrent flushes would double-consume the buffer range. Skips are counted
and alarmed, because a flush persistently slower than the tick is a visibility-latency breach the
`flush_max_age_secs` bound would otherwise silently miss.

### 1.2 Publication by rebase, over a shared bundle

A completed unit names what it consumed and the `segments_version` it snapshotted; the executor
applies it to the **then-current** generation rather than to the one it was planned against.

- A **flush** removes exactly the entity range it consumed from the buffer, whatever arrived while
  it ran, and appends its segment.
- A **merge** publishes only if every input `seg_id` is still present in the current generation.
  ABA-safe because `seg_id`s are never reused, across merges or prefixes (contracts §2.1). A merge
  whose inputs are gone is discarded; its outputs are orphans nothing references.
- A tick with an empty buffer **still publishes a pending merge**. Otherwise a completed merge on an
  idle deployment waits indefinitely.

**Generations are constructed incrementally, not by reopening the bundle.** A published generation
shares the previous generation's base `Arc<Bundle>` and adds (flush) or substitutes (merge) segment
entries and permutation extents. This is required, not an optimisation: `open_bundle` maps every
file afresh and `Permutation::load` re-pays an O(bound) `validate_rows`, so re-opening per flush
would cost more than the flush. §1.4's memory claim depends on this being built, and it is named
here so it is not assumed.

### 1.3 One cadence, and the minimum inter-publication interval

**The executor publishes on exactly one cadence, and a completed merge rides along with the next
flush publication.** One swap, one `segments_version` bump, one drain entry.

The reason is lifecycle §2.2's sizing obligation, which constrains the **publication period** and
does not care which publisher moved it. Sizing flush and merge as independent publishers costs a
factor of two, which at the current defaults forbids flushing more often than 150 s for no benefit a
viewer can observe. A merge changes query cost and nothing else, so deferring its effect by up to one
flush period is free.

**Three publishers exist, not one, and all three are governed by a minimum interval.** The age bound
is not by itself the publication period:

- `flush_max_age_secs` — the tick.
- `flush_max_items` — trips under load, and can trip far faster than the tick. **It does not publish
  early.** It marks the buffer flush-ready; publication still waits for the next tick, and if the
  buffer meanwhile exceeds `ingest_queue_bound` the excess is 429'd exactly as today. A bound that
  published on trip would move the real period below the validated one, and lifecycle §2.2's depth
  trim would drop pins before their TTL while the depth alarm saturates — the failure §1.3 exists to
  make unconfigurable.
- `POST /control/flush` (contracts §3.4, 202 accepted) — an operator-triggered publication. Accepted
  at any time; **executed at the next tick**. The 202 already means "accepted, not yet done", so
  nothing in the contract changes.

With every publisher on the tick, the relation is exact over the *actual* minimum period:

```
pin_ttl_secs < drain_depth_max × flush_max_age_secs
```

**Validated at startup; a violating configuration is refused.**

### 1.4 The marginal cost of drain depth, corrected

Lifecycle §2.2 caps the drain list at four superseded generations, and the natural reading is that
raising the cap is expensive because each entry pins an `Arc<Bundle>`.

**That reading is true of the pre-flush system and false after §1.2.** Before flush, consecutive
generations were whole distinct bundles. With incremental construction they share base geometry by
`Arc` and differ by a handful of small segments, so the marginal cost of a drain entry is roughly one
flush segment, not one bundle.

*Modelled, not measured.* The figure to measure before an admin leans on it is resident bytes per
drain entry under sustained flush.

`drain_depth_max` is therefore the cheaper knob, and an admin who wants 15 s visibility raises it
rather than shortening `pin_ttl_secs`.

### 1.5 Reclaim gets its caller

Lifecycle §2.1 records that reclaim has no periodic caller and runs only as a side effect of the
next geometry publication — a liveness gap it assigns to "whichever stage introduces a periodic
publisher". This is that stage. **The flush tick drives reclaim.**

## 2. Row space

### 2.1 Base plus an ordered extent list

I9 makes entity IDs append-only and allocation issues them monotonically from the high-water, so
**each flush segment covers a contiguous, ascending entity range**.

> **Assumption, stated because it is load-bearing and currently unenforced: one slice per
> partition.** `WalRow` carries no slice (`crates/tessera-lifecycle/src/wal.rs`), and with more than
> one slice a commit window's entity range interleaves across slices, making a segment's range
> ascending-with-holes rather than contiguous. The design survives that — extents become
> ascending-with-holes and §5.1's adjacency becomes list-adjacency — but the arithmetic in this
> section does not, and nothing today would catch the change. **`WalRow` gains a `slice` field in
> this change**, while the WAL layout is still being revised for rotation, because the format is
> append-only and flush freezes it.

Row IDs remain a flat `u32` space per slice. Segment *k* owns
`[row_base_k, row_base_k + row_count_k)`. Beside the built base permutation covering `[0, W_build)`,
a slice carries an ordered list of extents, each `{entity_lo, entity_hi, seg_id, row_base}`:

- **`row_of(e)`** — the base lookup below `W_build`, otherwise a binary search over a short ordered
  list. O(log k).
- **`project(mask)`** — the base projection unioned with a per-extent projection. Only the extent
  part is recomputed on a flush, which is what makes §3.4's patch cheap.
- **tile lookup** — unchanged per segment. Every flush segment is internally Morton-sorted against
  the *same* global `quantisation` (contracts §2.5), so a tile still resolves to one contiguous range
  per segment through the existing binary search, and the engine unions across segments.

**The extent dispatch lives inside `tessera-store`'s permutation module.** I4's claim that the
permutation is "the only legal EntityId→RowId path in the codebase" is a claim that module makes
about itself; an engine that learns to select an extent and index a segment falsifies it.

Two bounds stated at the site: total rows per slice stay under 2³², and the extent list is bounded by
the live segment count, which §5 bounds.

### 2.2 Merge is row-count preserving

Dropping rows is a fold, and folds belong to compaction. A merge therefore emits exactly as many
rows as it consumed, so **no later segment's `row_base` ever moves**, and merging *k* adjacent
extents collapses them to one. The extent list grows only under flush and shrinks only under merge.

Merging two Morton-sorted code arrays is a linear merge-sort whose output is unconditionally sorted.

### 2.3 The Morton re-rank decorator is dropped

§11.3 imports from Lucene the idea of re-ranking as a decorator on the merge policy — reorder only
above 2¹⁸ documents, skip rather than fail when memory is short, always reorder on forced merges.

**It does not transfer.** Lucene reorders for doc-id locality, an optimisation. Here the Morton sort
*is* the tile index: a segment that is not internally sorted breaks `tile_ranges`' binary search
outright, so sorting is not optional and cannot be skipped under memory pressure. And §2.2's linear
merge-sort needs no extra memory and produces sorted output unconditionally, so there is nothing to
make conditional on a document count.

*A departure from §11.3, recorded as a decision rather than applied silently.*

## 3. Entity space

### 3.1 What a flush publishes

Per segment: `morton.u32`, `columns.arrow`, a **sparse delta postings file** (term → entities, only
for terms present in the flushed set), a `dict_extents` entry (§3.2), an `external_id_extents` entry
**and a locator extent** giving the entity→external_id direction (§3.6).

Every one is listed in the new `SEGMENTS-<n+1>.json`'s `files` map — which is exactly where contracts
§2.2/§2.3 put files that appeared after build time. **Flush and merge never touch `MANIFEST.json` or
`CURRENT`.**

The watermark advances to **`entity_hi + 1`**. Composition treats entities `≥ W` as buffer-resident
(`compose.rs`; `Generation.watermark`'s own doc: "at or past this value live only in buffer"), so
`W = entity_hi` would leave the highest flushed entity excluded from the fragment *and* absent from
the buffer — invisible.

### 3.2 Novel descriptors become durable at flush

`crates/tessera-lifecycle/src/buffer.rs` allocates term ids for descriptors the dictionary has never
seen from the top of the `u32` range downward, precisely so they are **unsatisfiable**: a novel
descriptor can buffer an item but can never make it visible.

Flush promotes each extension descriptor to a durable dictionary ordinal and publishes the
assignment as a `dict_extents` entry. The downward-counting ids become a *pre-flush stopgap* rather
than a permanent state. The collision argument that motivates counting downward is unaffected —
nothing in the promotion path assigns an extension id to a real descriptor, and the extension range
stays reserved.

**The plumbing this requires, named rather than assumed.** `satisfied` is resolved once per session
at authorise against `Engine.dict`, an `Arc<Dict>` loaded at `Engine::open` and threaded into
`WritePath::new` and the `DescriptorResolver` (`session.rs`). Promotion requires the dictionary to
become **generation-scoped** — `Arc<Dict>` moves into `Generation` and is republished with each
flush — touching authorise, the write path and the resolver. §13 lists the sites.

**Two fail-closed consequences, stated because neither is obvious:**

- A promoted descriptor is satisfiable only by sessions authorised **after** the flush that promoted
  it, because `satisfied` is fixed per session at authorise.
- An item still buffered under an old extension id for an already-promoted descriptor stays
  invisible until *its own* flush, even to a viewer holding the term.

The first of these is also what makes §3.4's equality hold, and §3.4 relies on it explicitly.

### 3.3 Mask staleness is a session-invalidation cause, not a new signal

§3.2's first consequence — a promoted descriptor is satisfiable only by sessions authorised after
its flush — leaves an older session permanently under-seeing with no way to find out. **The remedy
adds no new mechanism and no new wire surface: staleness becomes a third cause of session
invalidation, expressed through the expiry machinery that already exists.**

**The condition is already computed and thrown away.** `Session::satisfied` is built as
`auth_terms.filter_map(|d| dict.lookup(d))` over the plugin's granted descriptors, over a module
whose own doc records the design: *"an unknown descriptor is simply unsatisfied, never an error"*.
The descriptors that resolved to `None` are precisely the session's exposure to promotion.

**The condition is two integers, and retains nothing new.** A session records `unresolved_count`
(how many granted descriptors the dictionary did not know) and `dict_len_at_authorise`. It is stale
iff:

```
unresolved_count > 0  &&  current dict length > dict_len_at_authorise
```

`Dict` is generation-scoped under §3.2, so the current length is `generation.dict.len()` — monotone
across flushes, and read from the generation the request already loaded once at its start
(lifecycle §1.1's ordering invariant). Two loads and a branch. **Decision 0020 is untouched: a count
and an integer are not authorisation data.**

**The effect reuses `Session::expires_at`.** A stale session is treated as expired, so the next
request receives the ordinary expired-token response and the client re-authorises — behaviour every
client must already implement. Three properties follow, and each is why this shape is better than a
new staleness flag:

- **No new wire field.** `expires_at` is already returned at authorise and already published.
- **Early invalidation is already precedented and already contractual.** Decision 0025 makes a key
  rotation a session-invalidation event, so `expires_at` is an upper bound on validity rather than a
  guarantee of it, and a client that assumed otherwise was already wrong.
- **The remedy is structurally a *new session*, never a re-resolution in place.** Re-resolving
  `satisfied` inside a live session would break §3.4's premise 3 and with it the
  patch-equals-rebuild equality. Expressing staleness as expiry makes that impossible rather than
  forbidden.

**Evaluated lazily at request time, never swept.** The check is made where expiry is already
checked; nothing walks the session registry when a flush promotes a term. That keeps the executor
free of an O(sessions) publication step and is consistent with decision 0035 — the session sweep
runs on growth, not on a timer, and this adds neither.

Invalidation is **immediate** rather than graced. The cost is a burst of re-authorisations at a
promoting flush, bounded by how rare promotion is: a novel *descriptor* is rare in a way a novel
*item* is not.

**Precise where it matters, over-reporting where it does not.** A session with no unresolved
descriptors is *never* invalidated — the common case, and the one that must not regress. A session
with one is invalidated whenever any term is promoted, not only its own. The asymmetry is the right
way round: the false direction costs a needless re-authorisation. The refinement available later
without a wire change is to compare digests of the unresolved descriptors against digests of the
promoted ones; noted rather than built, because it retains more than a count does, and because the
coarse form leaks **less** (below).

**Two rules that keep this from becoming something it must not be:**

- **It moves in one direction only, and nothing may ever be wired to make a revocation take effect
  through it.** A stale session sees *fewer* items than its principal is entitled to — fail-closed.
  Grant changes are not covered here and must not be made to look as though they are; decision 0025
  governs rotation, and a future reader must not read this as a general "the mask changed" channel.
- **It needs a leak-register row.** An invalidation tells a viewer that *some* descriptor was
  interned since they authorised — weak corpus-level inference, but not nothing (decision 0024
  scopes the register to viewer inference). The coarse form leaks strictly less than the digest
  refinement would: coarse says "a term appeared", precise would confirm that *their specific
  descriptor* now exists. Cheaper and less disclosive is an unusual pairing, and is the reason to
  prefer it.

**Compaction inherits one obligation:** dictionary length is the monotone counter this rests on, so
a compaction that renumbers the dictionary must not reduce it, or must introduce a counter that
never decreases.

### 3.4 The patch equals a rebuild — the load-bearing claim

§11.2 specifies that flush advances `W` by OR-ing in the flushed segment's contribution for the
token's already-known satisfied terms — "a small, monotone patch rather than a rebuild". SA §6.4
claims correctness never depends on patching a fragment, and lifecycle §3.3 requires fragments to be
built from current postings. **These coexist only if the patch produces the value a rebuild would
produce, exactly.** Here it does, on four premises, each of which is a thing this design must
maintain rather than a happy accident:

1. The flushed entity range is contiguous, disjoint from everything below, and entirely at or above
   the pre-flush `W` — from I9's append-only allocation (§2.1).
2. Base postings are untouched by a flush (§5.2's merge/compaction line).
3. **The session's `satisfied` set is fixed at authorise and never re-resolved**, so the terms
   unioned by the patch are exactly the terms a rebuild would consult (§3.2). A design that
   re-resolved `satisfied` per request would break the equality, not merely widen it.
4. A flush publishes a *new* `segments_version`, and a fragment build in flight against the old one
   publishes into its own slot under the single-flight sequence-number rule (lifecycle §7.2, rule 2)
   — so a concurrent build cannot interleave with a patch.

Given those, `old ∪ (delta ∩ satisfied)` is *identical* to a rebuild from current postings. The same
argument carries the row projection: the new extent's rows are disjoint from every existing row.

**Asserted by a property test for byte-equality against a full rebuild, not by this paragraph.**
Without the equality the alternative is rebuilding both on every flush, which `Permutation::project`
prices at a measured 10.7 s at 10⁹ rows — per session, per flush.

### 3.5 Entities under a deny at flush time

**The rules are relative to the buffer snapshot the flush took**, not absolute, and the cut is stated
because it is where a reader would otherwise assume more than holds: a delete accepted *after* the
snapshot produces a deleted entity that does have a row, hidden by its standing overlay entry alone.
That is safe today only because nothing retires, and it is an obligation the compaction spec
inherits.

For dispositions visible at the snapshot, the two behave differently because lifecycle §3.1's
relationship between each and the postings differs:

- **Suppressed → flushed normally.** A suppression never touches postings and retires only on
  unsuppress. A flush that skipped it would leave a later unsuppress with nothing to reveal.
- **Deleted → never written into the segment.** The ID stays burned (I9), no row is created, the
  deny entry stands.
- **Carrying an evaluate entry → the WAL row's terms are written, and the evaluate entry stands.**
  Writing the *entry's* current terms instead would be the fold — invariant-bearing, and
  compaction's. This is the sentence that stops the fold arriving as a simplification.

**A flush acts only on WAL-durable dispositions, and a node in `WalPoisoned` publishes nothing.**
Under the apply-anyway rule (lifecycle §4) an under-durable delete is in force in memory and answered
500, and contracts §3.1's stated residual is that a restart makes the item visible again. A flush
that honoured such a delete would skip the entity and advance `W` past it; replay would then discard
the delete record, leaving the item in no segment and no buffer — the un-acked delete made
**permanent**, contradicting the contract in the fail-closed direction. Not publishing while
poisoned costs ingest visibility during WAL degradation, when nothing new is being made durable
anyway.

### 3.6 The external-id directions

`external_id_extents` gives external_id → entity. The **reverse** direction is served live-map-first,
locator-second (contracts §2.4), where the live map is rebuilt by WAL replay and `ext-locator.u32` is
one file of build-time length. Without a durable reverse path for flushed entities, an item that is
visible on the map would answer `/v1/items` with a typed error forever once its WAL region is
reclaimed.

So a flush publishes a **locator extent** for its entity range alongside the forward extent, and the
loader consults base locator then extents.

## 4. Policy and configuration

Admin-configurable, all validated at startup:

| Knob | Governs |
|---|---|
| `flush_max_age_secs` | the tick: visibility latency, and the publication period |
| `flush_max_items` | buffer size at which a flush becomes ready (executed at the next tick — §1.3) |
| `pin_ttl_secs` | session pin lifetime |
| `drain_depth_max` | superseded generations retained |
| `segment_floor_bytes` | below this, segments compare equal for selection |
| `max_merged_segment_bytes` | cap on any single merge |
| `tier_width` | segments per tier before a merge is selected |

Two relations are enforced — a configuration **violating** either is refused at startup:

1. `pin_ttl_secs < drain_depth_max × flush_max_age_secs` — §1.3.
2. `max_merged_segment_bytes < base segment bytes` — §5.2.

Defaults: `flush_max_age_secs` 90–120 s. The **floor** the current `pin_ttl_secs` (300 s) and
`drain_depth_max` (4) permit is 75 s; the default sits above it with margin. Ingest visibility
latency is therefore bounded below by the pin relation rather than by an arbitrary choice, and an
admin wanting it lower raises `drain_depth_max` (§1.4).

`flush_max_items` and `flush_max_age_secs` are today parsed and asserted **inert** by a test. That
test is replaced by one asserting both bounds are honoured — epic #3's "honoured rather than
parsed".

## 5. Merge policy

### 5.1 Tiered, over flush segments, adjacent runs only

`tier_width` segments in a tier selects a merge; `segment_floor_bytes` makes a tail of tiny segments
compare equal so it does not dominate selection; `max_merged_segment_bytes` bounds any single merge.
Selection on the executor against the current generation; execution on the pool over immutable
inputs; publication rebases (§1.2).

**Merge selects only entity-adjacent runs**, so k extents collapse to one and the extent list stays
minimal and ordered. A size-only policy would produce segments covering discontiguous entity sets and
the extent list would fragment monotonically with nothing but compaction to repair it. The cost is
stated rather than hidden: a large segment can block a merge of its neighbours.

### 5.2 Merge coalesces delta postings, and that is still not a fold

Flush produces one delta postings tier per segment, and a fragment build unions across every live
tier. Bounding segments while leaving tiers unbounded moves §0's serving cliff from the tile path to
the authorise path — where it is worse, because a fragment build is a session's first-viewport cost.

**A merge coalesces its inputs' delta postings into one tier**, as a content-preserving re-encode:
the same (term, entity) pairs, concatenated and re-sorted, nothing dropped and nothing rewritten. No
tombstone is applied, no evaluate entry's terms are consulted, no overlay entry becomes retirable.
Merge stays invariant-neutral — coalescing is to postings exactly what §2.2's merge-sort is to Morton
codes.

### 5.3 The line between merge and compaction

Both rewrite row space. What separates them is what they **fold**, and the publication difference
follows from that rather than from convention:

| | merge | compaction |
|---|---|---|
| Rewrites | Morton codes, columns, permutation coverage, delta tiers coalesced — **row space and content-preserving re-encodes only** | that, **plus** delta postings folded into base postings, tombstones applied, evaluate entries' term sets written into postings |
| Retires overlay entries | never | the fold **is** the retirement event (lifecycle §3.2, §3.4) |
| Invariant exposure | none — no visibility state changes | the carry-forward rule, three of whose four categories were fail-open as first written |
| Publishes | a new `SEGMENTS-<n>.json` inside the current prefix | a **new prefix** and a `CURRENT` flip, because it rewrites files `MANIFEST.files` digests |

**The base segment is not excluded by a rule; it is excluded by the size bound, and the rule explains
why the bound exists.** A merge that swallowed the base would be a legal row-space-only rewrite. The
objection is that it pays compaction's entire cost — a full permutation rewrite and up to 10⁹ rows of
columns re-emitted — and banks none of compaction's benefit: the overlay still grows, deletion denies
still never retire, evaluate entries stay immortal.

There is a sharper form. Base files live in `MANIFEST.files`. A merge consuming base must either
leave them digested there with nothing referencing them, or write a new prefix — at which point it
*is* compaction under another name. Hence relation 2 in §4.

### 5.4 No deletes-percentage trigger

§11.3 lists tombstone reclamation on a deletes-percentage trigger as one of three reasons to merge.
It is not one here: reclaiming tombstoned rows is a fold. Merge's invariant-neutrality is a property
to preserve rather than an accident.

## 6. Coordinates outside the quantisation extent

Morton codes are computed against `MANIFEST.json`'s `quantisation` (contracts §2.5), fixed at build.
**Nothing today validates an ingested item's coordinates against it** — it has never mattered,
because a buffered item never acquires geometry. Flush is the moment it does.

`/control/ingest` **refuses** such a row with a typed 4xx naming the extent, before anything is acked
or WAL-durable. Fail-closed: nothing is silently misplaced, and a clamped item at the boundary would
be indistinguishable from a legitimately edge-located one.

**Rows already WAL-durable when this validation lands are quarantined, not retried forever.** A
pre-existing out-of-extent row would otherwise fail its flush on every tick, and §8's "buffer
retained, retried next tick" would become a permanent visibility outage for the whole partition.
Such rows are moved to a quarantine list, counted, alarmed, and excluded from flush; they remain
invisible, which is the state they were already in.

The stated cost: a deployment whose data drifts outside its declared extent cannot ingest those items
until it re-quantises, which is compaction's shape and is recorded as a compaction obligation.

## 7. The WAL

### 7.1 Rows: recovery is idempotent by construction

**The buffer is reconstructed at recovery as `{WAL rows with entity_id ≥ the published watermark}`.**

Flushed rows already have geometry, and the `watermark` in the served `SEGMENTS-<n>.json` says
precisely which. Replay therefore cannot duplicate or lose a row at *any* crash point, and
`Flush{n, wal_pos}` is purely a replay-start optimisation and the authority for rotation — never a
correctness device.

### 7.2 The overlay is not rows, and truncation must not treat it as such

**This is the finding that reworked r1, and the rule that replaces it.** The overlay's only durable
home is the WAL: recovery is `replay(&records, …)`, and nothing else persists it —
`SegmentsManifest::deny` has exactly one writer in the tree and it writes `Vec::new()`. `Change`
records are captured by no segment, and a suppression retires **only** on unsuppress (lifecycle
§3.1), so a suppression's record must outlive every checkpoint. Reclaiming WAL files below a flush
checkpoint would therefore delete accepted denies and re-expose their items on the next restart —
the row lifecycle §8 says must never exist.

**So each rotation writes a compacted overlay snapshot at the head of the new WAL file**, carrying
every live suppression, deletion and evaluate entry, and it is fsynced **before** any older file is
deleted. Retention returns to one file; the overlay's durable home stays the WAL; no contract
changes; and because dispositions are idempotent, re-writing them is safe by construction. Recovery
reads the snapshot, then the records after it, and is strictly faster than today's full replay rather
than merely bounded.

The snapshot is O(live overlay) — the quantity the overlay soft limit already gauges, which makes the
existing gauge the right alarm for rotation cost as well.

### 7.3 The order, and its single commit point

```
pool:     segment files, delta postings, dict / external-id / locator extents durable
      →   SEGMENTS-<n+1>.json durable                    ← the commit point
executor: generation swap                                 ← the publication event
      →   Flush{n, wal_pos} appended and fsynced          ← optimisation
      →   rotation: overlay snapshot written and fsynced  ← §7.2, before any deletion
      →   WAL files wholly below wal_pos deleted          ← reclamation
```

A crash before the side-manifest leaves orphan files nothing references, and replay re-flushes
deterministically — lifecycle §8's "mid-flush (files, no manifest)" row, which becomes tested
behaviour rather than an inherited obligation.

The WAL becomes a sequence of `wal-<seq>.log`, each with its own fsync-offset sidecar (decision
0038). **Every §4 rule applies per file, unchanged**: the positional CRC rule ("position decides, not
damage"), the three sidecar guards, and truncate-and-fsync before a handle is issued. Recovery walks
the sequence in order; a gap in the middle is corruption of acked state and fails closed.

### 7.4 The idempotency horizon is the WAL retention, and it is an observable

`accepted_batches` is WAL-replay-derived, and `write.rs` already records that a retired WAL segment
regresses old batch ids to `Unknown`. Rotation makes that reachable for the first time: after a
restart, a byte-identical retry of a batch older than the retained WAL is no longer recognised as a
duplicate, and rows carrying no `external_id` would be ingested twice (rows that carry one are still
caught by the live external-id map).

This is stated as a contract-visible observable rather than fixed here: **the idempotency window
equals the WAL retention window**, and a client retrying across it must carry external IDs.

## 8. Side-manifest deny state, and step-down

### 8.1 A flush-published manifest carries the deny state, and the reader honours it

Contracts §2.3 makes each `SEGMENTS-<n>.json` **complete current state for its partition, not a
diff**. Flush is the first thing in the system to publish a side-manifest after build, so it inherits
that obligation: its manifest carries `deny` (the active suppression set) and `tombstones` (deleted
entities that already have rows).

Omitting them would publish a manifest that silently claims an empty deny state — a fail-open at the
interchange layer and a decision-0013 violation. Carrying them while `HONOURED_STATE` stays empty
would classify every such manifest `Unready` (`manifest.rs`), so **the node could not reopen its own
bundle** once any suppression existed.

So `HONOURED_STATE` gains `"deltas"`, `"deny"` and `"tombstones"` — **each in the same change as the
code that acts on it**, per that constant's own rule. The loader applies `deny` and `tombstones` to
the initial overlay; WAL replay unions on top. The two agree, and where they do not the WAL is the
superset and wins; dispositions are idempotent, so the union is well-defined.

### 8.2 Step-down, and why the writing node fails closed instead

Once deny state is honoured, a manifest carrying it is never stepped past — `Honourability`'s
existing classification already refuses that, which is why the `readyz` freshness gate (#58) is
**not** dragged into this epic: step-down past accepted denies is refused outright rather than
time-bounded. #58 remains an availability obligation for a lagging replica, not a correctness one
here, and there are no replicas today (lifecycle §6 is unbuilt in its entirety).

A `deltas`-only step-down remains classified `Steppable`, and for a read-only replica it is still
fail-safe staleness. **For the writing node it is not, and the writing node therefore refuses to
serve from a stepped-down manifest at all — it stays unready.** The reason: §7.1 reconstructs the
buffer as WAL rows at or above the *served* watermark, and after rotation the rows between an older
manifest's watermark and the newest one's are gone. Re-flushing from a stepped-down watermark would
silently lose them. Failing closed costs availability on a node whose newest segment files are
damaged, which is the correct trade and the one SA §9 already prescribes.

## 9. Caches

r1 omitted this entirely; it is not a detail.

**The row-projection cache** is keyed `(token_id, slice, segments_version)` and its own doc notes a
multi-segment slice would widen the key. A flush bumps `segments_version` every tick, and a full miss
is a **measured 10.7 s**. So the patch publishes into the **new** key a value derived from the old
entry — which respects "invalidation is key rotation, never mutation" (lifecycle §7.2), because
nothing modifies a cached value. Two consequences to hold: the old entry must still be readable at
patch time, so `prune_segments_version` runs only after the patch publishes; and where the old entry
has already been evicted, the patch path falls back to a full build, which is correct by §3.4 and
merely slow.

**The authorisation fragment cache is a persistent disk cache whose key excludes the watermark**
(`tessera-authz/src/fragment.rs`). After flush, same-key entries would exist at heterogeneous
watermarks, and `tmp_sibling`'s "both writers wrote byte-identical content" argument would no longer
hold. **The watermark joins the disk cache key.**

That has a second effect worth recording for the compaction spec: persisted fragments surviving a
restart at pre-flush stamps would falsify lifecycle §3.2's "the cache restarts cold" premise, which
is what scopes the future retirement floor worker-locally. With the watermark in the key, a
pre-flush fragment is never found by a post-flush lookup, and the premise holds.

## 10. Failure handling

The side-manifest being the only commit point makes every failure "nothing happened, retry next
tick".

- **Flush task fails, or the disk fills mid-flush** — flush abandoned, buffer retained intact, alarm,
  retried next tick. Orphans are unreferenced and swept. Repeated failure raises the skip alarm
  (§1.1).
- **Merge fails, or its inputs are gone at publish** — output discarded, inputs still live, retried
  by the next selection.
- **Repeated flush failure** — the buffer grows until `ingest_queue_bound` 429s ingest. Intended
  backpressure, stated rather than discovered. The WAL headroom rule (§4) is untouched, so **denies
  always have room and are never refused for load**.
- **`WalPoisoned`** — no flush publishes (§3.5).
- **Out-of-extent coordinates** — refused at ingest; pre-existing ones quarantined (§6).

**One cost this design adds to the deny path, modelled not measured.** The executor's rebase removes
the consumed range from the then-current buffer, which is O(buffered) on the executor thread — the
same shape the deny-ack memo measured as the dominant term at 1 M buffered (165 ms p50). A publication
per tick therefore adds one such stall ahead of the deny lane per tick. The memo's method prices it;
it should be re-run rather than reasoned about.

## 11. `tessera build` is initial-load only

Flush forces a question build has never had to answer. Today `tessera build` is an **overwrite**: it
reads a source corpus and writes a complete bundle at a new prefix, and nothing in it reads the WAL,
the buffer or the overlay. That is harmless only while ingest is invisible — an omitted buffered item
has no geometry, so a rebuild that drops it changes nothing observable, and the WAL replays it into
the buffer against the new bundle.

**Once flush exists, an overwrite silently deletes acked, visible items**, whose acknowledgement was
a durability receipt (contracts §3.1). §5.1's "entity IDs stable across rebuilds" tacitly assumes the
rebuild's input contains the same items, which stops being true at the first ingest.

**The ruling: `tessera build` runs against an empty bundle root and never again.** After that the
deployment is the durable record. Additions enter by ingest → flush. Reorganisation —
re-quantisation, the fold, re-ranking, a batch-grid change — is **compaction**, which reads the
deployment rather than the source. A from-source rebuild remains available as an explicitly
identity-breaking migration producing a *new* deployment, on §12.5's build-under-a-new-prefix-and-flip
precedent.

Enforced, not documented: build refuses a bundle root containing a `CURRENT`.

This is why §0 records that compaction becomes load-bearing. It also settles the question r1 raised
as an escalation — build never meets a flushed deployment, so there is nothing to fence.

## 12. Epic #8: how a batch enters an existing bundle

#8 opens with a design decision that must be recorded before code is written: appended as a new
segment, merged, or staged.

**Appended.** A batch enters by the ordinary ingest path and becomes a flush segment. Never merged
into base, never staged. Entity IDs remain append-only and never reused (I9), and the batch's sort
scope is the commit window, exactly as for streamed arrivals.

#8's remaining work is therefore not about entry shape: it is the batch-grid identity guard
(recording batch size in the bundle and refusing a rebuild at a different one) and bulk-ingest
ergonomics. The measured caveat carries across and is #8's to answer: §11.1 records that a bulk load
chunked into small requests forfeits the entire signature-sort win **permanently**, so bulk ingest
needs large commit windows.

## 13. Invariant and leak-register interactions

- **I4** — the extent dispatch stays inside the permutation module (§2.1).
- **I2 / §7.2 r24** — θ's anchor `V_total` is counted in row space, so it advances **at flush
  boundaries**, not per arrival. §11.2 already says this; flush makes it the normal steady state of
  an ingesting deployment. Stated as an expected observable so it is not filed as a bug.
- **C4** — drill-down resolves a bit in entity space before looking up a row, so a still-buffered item
  passes the entity-space test and then finds no row, and must return the same *unknown* outcome as an
  identifier naming nothing. Flush **shrinks** that window to `flush_max_age_secs`; it does not close
  it, and C4's timing closure remains a claim about identical outcomes rather than identical work.
- **I11** — a pin taken before a flush serves pre-flush geometry with the current overlay. Already the
  specified behaviour; flush is the first mechanism that exercises it.
- **I7, I9** — untouched. Flush allocates no IDs and moves no selection route; the direct-evaluation
  set merely shrinks as items acquire postings.
- **The retirement floor** — does not exist and is not created here. §9's watermark-keyed disk cache
  is what keeps lifecycle §3.2's worker-local scoping argument true across a restart, which is the
  one place this design could have made the floor unimplementable.

## 14. What must be proven

Extending the lifecycle §7.3 fault switchboard rather than building a second beside it.

1. `patch == rebuild`, **byte-equal**, as a property test over random corpora and tokens (§3.4),
   including a concurrent-build-during-flush case for premise 4.
2. Crash-replay idempotence at every ordering point of §7.3: no duplicated or lost row.
3. **A suppression accepted before a rotation is still in force after a restart** (§7.2). The r1
   fail-open, as a test.
4. Deleted-at-snapshot acquires no row; suppressed-at-snapshot does, and a later unsuppress reveals
   it; a delete arriving *mid-flush* leaves a row hidden by its overlay entry (§3.5) — the timing
   cases, not just the quiescent ones.
5. A `WalPoisoned` node publishes no flush, and an under-durable delete's item is visible again after
   restart (§3.5, contracts §3.1's residual).
6. Segment count **and delta-tier count** bounded under sustained ingest; merge keeps both bounded
   (soak).
7. The ack→visibility gap is bounded by `flush_max_age_secs`, including when `flush_max_items` trips
   between ticks (§1.3).
8. A node whose newest manifest is damaged stays unready rather than re-flushing from a stepped-down
   watermark (§8.2).
9. A flushed item answers `/v1/items` after rotation (§3.6).
10. `seg_id` never reused across flush or merge; `SEGMENTS-<n>` strictly monotonic and unpadded
    (decision 0016).
11. A pin taken across a flush serves pre-flush geometry and applies a post-flush deny.
12. Both §4 relations refuse a violating configuration at startup.
13. `check-layers.sh`'s one-`.store(` rule still passes — publication stayed single-owner.
14. An out-of-extent ingest is refused before ack and leaves no WAL record; a pre-existing one is
    quarantined rather than retried (§6).
15. `tessera build` refuses a bundle root containing a `CURRENT` (§11).
16. A session holding an unresolved descriptor is invalidated by a promoting flush, and
    re-authorising resolves the descriptor and sees the items (§3.3) — **and a session holding none
    is not invalidated by any flush**, which is the half that regresses silently if the condition is
    ever loosened.

## 15. Out of scope

Compaction and the fold; the deletion stamp ledger, the retirement floor and the evaluate-entry fold;
the `readyz` freshness gate (#58, and §8.2 records why it is not needed for correctness here);
`/control/allocate-ids` (#61); the router/worker protocol (lifecycle §6).

## 16. Corpus updates that land with the code

Present tense about absent machinery reads as an assurance (decision 0013), so the ⊘ markers come out
with the code:

- **concurrency-lifecycle** — §1.1 (generation shape), §1.2 (`segments_version` movement), §1.3 (the
  two-publisher marker), §2.1 (reclaim's caller), §4 (the `Flush` record, rotation, the overlay
  snapshot), §5.1 (flush), §5.2 (merge), and the flush and merge rows of §8's crash matrix.
- **architecture** — §11.2 (flush as the visibility mechanism), §11.3 (the re-rank decorator and the
  deletes trigger).
- **contracts** — §2.3 (a flush-published manifest's deny state, §8.1), §2.4 (the locator extent,
  §3.6), §3.4 (`POST /control/flush` executes at the next tick; the idempotency horizon, §7.4).
- **inventory, conformance** — the flush- and merge-dependent markers.
- **`HONOURED_STATE`** — gains `"deltas"`, `"deny"`, `"tombstones"` (§8.1).
- **`buffer.rs`** — its "until the next build assigns a durable term id" now names flush (§3.2).
- **Appendix C** — a leak-register row for the staleness invalidation: a viewer learns that some
  descriptor was interned since they authorised (§3.3).
- **`session.rs` / contracts §3.2** — `expires_at` is an upper bound on validity, not a guarantee of
  it. Decision 0025 already made that true for rotation; staleness is the second cause, and the
  place it is stated should name both rather than either.

Five decision records: the single publication cadence and its minimum interval (§1.3); dropping the
Morton re-rank decorator (§2.3); refusing out-of-extent coordinates at ingest (§6); the overlay
snapshot as the WAL's rotation rule (§7.2); `tessera build` as initial-load only (§11).

Issues closed or reduced: #3 (flush), #59 (the second publisher), #8's design decision (§12).

## Appendix R — Review record

**r1** — drafted 2026-08-02; independently reviewed the same day (verdict: **needs-rework**). The
single-publisher, publication-by-rebase and one-cadence architecture survived; the treatment of the
WAL as reclaimable and of the side-manifest as flush-only state did not.

**r2** closes all eighteen findings. The three that changed the design rather than sharpening it:

1. **WAL truncation deleted acked denies** (B1) — the overlay's only durable home is the WAL and
   `Change` records are captured by no segment. §7.2's rotation snapshot replaces reclamation-by-
   checkpoint. This was a fail-open of the class lifecycle §8 names as the row that must never exist.
2. **A flush-published side-manifest inherits contracts §2.3's completeness obligation** (B2) — so it
   carries deny state and the reader honours it (§8.1), which also makes step-down past accepted
   denies refused rather than time-bounded, keeping #58 out of scope.
3. **`WalPoisoned` publication made an un-durable delete permanent** (B3) — r1 permitted it
   explicitly. §3.5 now forbids it.

Also corrected: the pin relation was validated against the age bound while `flush_max_items` set the
real period (B6, §1.3); delta tiers were unbounded while segments were not (B5, §5.2); flushed
entities had no durable entity→external_id path (B4, §3.6); dictionary promotion needed
generation-scoped `Dict` and had two unstated visibility consequences (B7, §3.2); step-down plus
rotation lost rows silently (B8, §8.2); the watermark was off by one (N2, §3.1); the cache story was
absent (N1, §9); `WalRow` carries no slice (N6, §2.1); and §1.4's Arc-sharing claim depended on
incremental generation construction that nothing owned (N7, §1.2).

Escalated to the owner and ruled: the overlay's durable home under a truncatable WAL → the rotation
snapshot; `tessera build`'s semantics post-initial-load → initial-load only (§11), which also
dissolves r1's rebuild-fencing question.

**r3** adds §3.3, mask staleness, raised by the owner. r2 stated that a promoted descriptor is
satisfiable only by sessions authorised after its flush and then left such a session permanently
under-seeing with no way to find out. The first draft of §3.3 introduced a staleness flag; the owner
observed that the existing session-lifespan machinery already provides the comparison, and it does —
`Session::expires_at` exists, is already published at authorise, and decision 0025 already
established that a session can be invalidated before it. Staleness is therefore a **third
invalidation cause** rather than a new signal, which removes the wire work entirely instead of
deferring it, and makes "the remedy is a new session, never a re-resolution in place" structural
rather than a rule — which is what protects §3.4's patch-equals-rebuild premise 3.

**Unreviewed.** The second review round was not run. §3.3 in particular, and r2's new §5.2, §6, §7.2,
§8, §9 and §11, have had no independent scrutiny; r1's review covered none of them.
