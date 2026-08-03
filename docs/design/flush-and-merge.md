# Flush and merge — design

**Date:** 2026-08-02
**Status:** Provisional — under review, and **not approved**. The rest of the corpus governs where
they disagree. **To become normative:** owner sign-off on the design, and §16's amendments folded
into `architecture.md`, `contracts.md` and `concurrency-lifecycle.md` as the code lands — the ⊘
markers those documents carry are the gate, and clearing them ahead of the code would read as an
assurance (decision 0013).
**Reads against:** architecture §4, §5.1, §11.1–§11.3, Appendix C; contracts §2.1–§2.6, §3.1, §3.4;
concurrency-lifecycle §1–§5, §7, §8; system-architecture §6.2, §9.
**Citation convention:** unprefixed §n is the architecture design, per CLAUDE.md; `lifecycle §n` is
concurrency-lifecycle, `contracts §n` contracts, `SA §n` system-architecture. This document's own
sections are cited as **spec §n**.

**Owns:** the row-space segment lifecycle. Flush turns WAL-durable buffered items into a published
segment, which is what makes an ingested item visible at all; merge bounds the segment and
delta-tier counts flush would otherwise grow without limit. Both are **invariant-neutral**: neither
folds authorisation state, neither retires an overlay entry, neither can re-expose anything.

**Does not own:** compaction and its fold, the deletion stamp ledger, the retirement floor, the
evaluate-entry fold. Those are the invariant-bearing half of the write journey, specified separately
after this lands, because their inputs are artefacts this document creates.

---

## 0. What this closes, and the one thing it is not

Epic #3 states the condition plainly: ingest is durable and invisible. An acknowledgement is a
durability receipt, and the gap between it and visibility is unbounded, because a buffered item has
no row in any segment and every map verb asks a row-space question (§11.2).

Flush ends that condition. Merge is not a separate ambition — it is the cost control without which
flush is a serving cliff on **two** axes: a tile resolves to one contiguous range per live segment
(§11.3), and a fragment build unions across every live delta postings tier. A 90 s flush period
produces roughly a thousand of each per day.

**What this is not:** the mechanism that lets anything retire. After this lands, deletion denies
still never retire, evaluate entries are still immortal, and the overlay still grows monotonically
under deletion and predicate churn. Fail-closed, and not the specified mechanism. Compaction owns it.

**And one thing this makes load-bearing.** Under §11, `tessera build` is initial-load only, so
compaction becomes the *only* route to re-quantisation, the fold, re-ranking and a batch-grid change
— the deployment's sole reorganisation path rather than merely a desirable one.

## 1. Publication

### 1.1 One publisher

Flush and merge execute on the background pool over immutable inputs and submit a completed,
immutable result to the write executor for a **swap-only** publication step (lifecycle §1.3).

This closes #59. Geometry publication is today callable from any thread, which `write.rs` records at
the publication site along with the reason it matters — *"a flush would be precisely a second
publisher"*. So geometry publication becomes a command rather than a method any caller can reach:
the engine keeps exactly one non-atomic `.store(`, and `scripts/check-layers.sh` goes on policing it.

Completed units arrive on the **work** lane, never the deny lane. The loop's existing discipline —
drain deny to empty before touching work — keeps a suppression from queueing behind a flush's IO.

**At most one flush and one merge are in flight.** A tick arriving while a flush runs is skipped, not
queued: two concurrent flushes would double-consume the buffer range. Skips are counted and alarmed,
because a flush persistently slower than the tick is a visibility-latency breach that
`flush_max_age_secs` would otherwise silently miss.

### 1.2 Publication by rebase, over a shared bundle

A completed unit names what it consumed and the `segments_version` it snapshotted; the executor
applies it to the **then-current** generation rather than to the one it was planned against.

- A **flush** removes exactly the entity range it consumed from the buffer, whatever arrived while
  it ran, and appends its segment.
- A **merge** publishes only if every input `seg_id` is still present in the current generation.
  ABA-safe because `seg_id`s are never reused, across merges or prefixes (contracts §2.1). A merge
  whose inputs are gone is discarded; its outputs are orphans nothing references.
- A tick with an empty buffer **still publishes a pending merge**, or a completed merge on an idle
  deployment waits indefinitely.

**Generations are constructed incrementally, never by reopening the bundle.** A published generation
shares the previous one's base `Arc<Bundle>` and adds (flush) or substitutes (merge) segment entries
and permutation extents. This is required rather than an optimisation: `open_bundle` maps every file
afresh and `Permutation::load` re-pays an O(bound) `validate_rows`, so re-opening would cost more
than the flush it followed. §1.4's memory claim rests on it.

### 1.3 Two kinds of publication, and only one is on a cadence

The distinction is what lifecycle §2.2's sizing obligation is actually about, and the two are easy to
conflate because both write a side-manifest:

- A **geometry publication** supersedes geometry, so it bumps `segments_version` and runs the
  row-projection retention pass. Flush and merge are the only ones.
- An **overlay publication** — a side-manifest written because a deny disposition was accepted
  (contracts §2.3) — changes no geometry. Nothing is superseded and **`segments_version` does not
  move**, so no session's row projection is invalidated by a deny. Contracts §2.3's
  rule that an accepted deny publishes **immediately, never deferred to the next flush**, stands.

Immediacy is not a concession but the only workable answer: the deny lane is unbounded and can never
be load-shed, so a sized deny cadence would be unvalidatable under an adversarial deny rate. Two
consequences follow. `segments_version` must **not** move on an overlay publication, or every deny
would rotate the row-projection cache key and cost a full projection rebuild. And the side-manifest
`n` therefore advances faster than `segments_version` — which `read.rs` already tolerates and
documents ("the two agree in every bundle a conforming writer produces, and where they do not it is
the filename that decided").

**Every geometry publication is on one cadence, and a completed merge rides along with the next flush
publication.** One swap, one `segments_version` bump, one drain entry. Sizing flush and merge as
independent publishers would cost a factor of two — forbidding a flush more often than 150 s at the
current defaults — for no benefit a viewer can observe, since a merge changes query cost and nothing
else.

**The three geometry publishers, all on the tick:**

- `flush_max_age_secs` — the tick itself.
- `flush_max_items` — trips under load, and can trip far faster than the tick. **It does not publish
  early**; it marks the buffer flush-ready and publication waits for the next tick. Publishing on
  trip would make the real publication period a function of ingest rate rather than of
  `flush_max_age_secs` — and every publication rotates the row-projection cache key, so that is the
  rate at which every live session pays to bring its projection forward (§9).
- `POST /control/flush` (contracts §3.4) — operator-triggered, accepted at any time, **executed at
  the next tick**. Its 202 already means "accepted, not yet done".

**There is no longer a startup relation to satisfy.** An earlier revision required
`pin_ttl_secs < drain_depth_max × flush_max_age_secs` and made the tick's floor 75 s at the shipped
defaults. Pin retention is deleted (`geometry-pinning.md`), and with it the relation. **What replaces
it is a cost, not a refusal** — §9's projection rebuild — and no configuration is refused for it.

**Deferring `flush_max_items` to the tick requires a bound on the buffer that does not yet exist.**
`ingest_queue_bound` bounds the *command queue* — `sync_channel(queue_bound)`, 32 jobs by default —
and its 429 fires when submission outruns the executor's service rate. The executor drains a job into
the buffer in milliseconds, and nothing in the system compares buffer occupancy to anything, so no
ingest rate produces a 429 by buffer size. This design therefore adds `ingest_buffer_max_items`: a
buffer-occupancy admission bound, checked in the handler before submission, 429 on exceed. It is a
distinct knob from `ingest_queue_bound` because it bounds a distinct thing.

### 1.4 What the tick's period actually costs

The knob's floor used to be the pin relation. With that gone, the binding cost is the
**row-projection cache**: its key carries `segments_version`, so every publication rotates every live
session's key, and the entry has to be brought forward before that session's next viewport is served.

- **Brought forward by a patch**, where the immediately-superseded generation's entry is still
  resident: a union of the new extents' rows onto the old bitmap, which is *equal to* a projection
  over the whole space because a flush appends (§3.4). This is why the cache retains one generation
  back rather than pruning at the swap (lifecycle §2.2).
- **Rebuilt from scratch** otherwise: a *measured* **10.7 s at 10⁹**, per session, and if the patch's
  input is missing then per session *per tick*, synchronised across the whole population. §9 states
  that failure; the retention depth is what prevents it.

So a shorter tick is affordable exactly to the extent that the patch holds. An admin lowering
`flush_max_age_secs` is buying visibility latency with per-tick patch work, not against a validated
floor.

### 1.5 The retention pass runs at the publication

Every geometry publication drops row-projection entries more than one generation behind the one it
just published (lifecycle §2.2). There is nothing periodic left to schedule: retention is a depth on
the cache rather than a clock over a drain list, so it is discharged by the swap that creates the
need for it.

## 2. Row space

### 2.1 Base plus an ordered extent list

I9 makes entity IDs append-only and allocation issues them monotonically from the high-water, so
**each flush segment covers a contiguous, ascending entity range**.

> **Assumption, stated because it is load-bearing and currently unenforced: one slice per
> partition.** `WalRow` carries no slice, and with more than one a commit window's entity range
> interleaves across slices, making a segment's range ascending-with-holes rather than contiguous.
> The design survives that — extents become ascending-with-holes and §5.1's adjacency becomes
> list-adjacency — but this section's arithmetic does not, and nothing today would catch the change.
> **`WalRow` gains a `slice` field in this change**, while the WAL layout is already being revised
> for rotation, because the format is append-only and flush freezes it.

Row IDs remain a flat `u32` space per slice, segment *k* owning
`[row_base_k, row_base_k + row_count_k)`. Beside the built base permutation covering `[0, W_build)`,
a slice carries an ordered list of extents, each `{entity_lo, entity_hi, seg_id, row_base}`:

- **`row_of(e)`** — the base lookup below `W_build`, otherwise a binary search over a short ordered
  list. O(log k).
- **`project(mask)`** — the base projection unioned with a per-extent projection. Only the extent
  part is recomputed on a flush, which is what makes §3.4's patch cheap.
- **tile lookup** — unchanged per segment. Every flush segment is internally Morton-sorted against
  the *same* global `quantisation` (contracts §2.5), so a tile resolves to one contiguous range per
  segment through the existing binary search, and the engine unions across segments.

**The extent dispatch lives inside `tessera-store`'s permutation module.** I4's claim that the
permutation is "the only legal EntityId→RowId path in the codebase" is a claim that module makes
about itself; an engine that learns to select an extent and index a segment falsifies it.

Two bounds stated at the site: total rows per slice stay under 2³², and the extent list is bounded by
the live segment count, which §5 bounds.

### 2.2 Merge is row-count preserving

Dropping rows is a fold, and folds belong to compaction. A merge emits exactly as many rows as it
consumed, so **no later segment's `row_base` ever moves**, and merging *k* adjacent extents collapses
them to one. The extent list grows only under flush and shrinks only under merge.

### 2.3 No Morton re-rank decorator

§11.3 imports from Lucene the idea of re-ranking as a decorator on the merge policy — reorder only
above 2¹⁸ documents, skip rather than fail when memory is short, always reorder on forced merges.

It does not transfer. Lucene reorders for doc-id locality, an optimisation. Here the Morton sort *is*
the tile index: a segment that is not internally sorted breaks `tile_ranges`' binary search outright,
so sorting is not optional and cannot be skipped under memory pressure. And merging two Morton-sorted
code arrays is a linear merge-sort — no extra memory, sorted output unconditionally — so there is
nothing to make conditional on a document count.

*A departure from §11.3, recorded as a decision rather than applied silently.*

## 3. Entity space

### 3.1 What a flush publishes

Per segment: `morton.u32`, `columns.arrow`, a **sparse delta postings file** (term → entities, only
for terms present in the flushed set), a `dict_extents` entry (§3.2), an `external_id_runs` entry
and a **locator extent** giving the entity→external_id direction (§3.6).

Every one is listed in the new `SEGMENTS-<n+1>.json`'s `files` map — where contracts §2.2/§2.3 put
files that appeared after build time. **Flush and merge never touch `MANIFEST.json` or `CURRENT`.**

The manifest also carries `entity_id_high_water`, which §7.3 depends on.

The watermark advances to **`entity_hi + 1`**. Composition treats entities `≥ W` as buffer-resident,
so `W = entity_hi` would leave the highest flushed entity excluded from the fragment *and* absent
from the buffer — invisible.

### 3.2 Novel descriptors become durable at flush

`buffer.rs` allocates term ids for descriptors the dictionary has never seen from the top of the
`u32` range downward, precisely so they are **unsatisfiable**: a novel descriptor can buffer an item
but can never make it visible.

Flush promotes each extension descriptor to a durable dictionary ordinal and publishes the assignment
as a `dict_extents` entry. The collision argument that motivates counting downward is unaffected —
nothing in the promotion path assigns an extension id to a real descriptor, and the extension range
stays reserved.

**The plumbing this requires, named rather than assumed.** `satisfied` is resolved once per session
at authorise against `Engine.dict`, an `Arc<Dict>` loaded at `Engine::open` and threaded into
`WritePath::new` and the `DescriptorResolver`. Promotion requires the dictionary to become
**generation-scoped** — `Arc<Dict>` moves into `Generation` and is republished with each flush —
touching authorise, the write path and the resolver. §16 lists the sites.

**Two fail-closed consequences, neither obvious:**

- A promoted descriptor is satisfiable only by sessions authorised **after** the flush that promoted
  it, because `satisfied` is fixed per session at authorise.
- An item still buffered under an old extension id for an already-promoted descriptor stays invisible
  until *its own* flush, even to a viewer holding the term.

The first is also what makes §3.4's equality hold, and §3.4 relies on it explicitly.

### 3.3 Mask staleness is advertised, never forced

§3.2's first consequence leaves an older session under-seeing with no way to find out. This section
gives it one. **What is specified here is the internal condition; the wire representation and a
client's policy for acting on it are client-facing work.**

**The condition is already computed and discarded.** `Session::satisfied` is built as
`auth_terms.filter_map(|d| dict.lookup(d))`, over a module whose own doc records the design: *"an
unknown descriptor is simply unsatisfied, never an error"*. The descriptors that resolved to `None`
are precisely the session's exposure to promotion.

**It is two integers, and retains nothing new.** A session records `unresolved_count` and
`dict_len_at_authorise`, and is stale iff:

```
unresolved_count > 0  &&  current dict length > dict_len_at_authorise
```

`Dict` is generation-scoped under §3.2, so the current length is `generation.dict.len()` — monotone
across flushes, read from the generation the request already loaded once at its start (lifecycle
§1.1's ordering invariant). Two loads and a branch. **Decision 0020 is untouched: a count and an
integer are not authorisation data.**

**It is a hint, and the client chooses when to act.** The tempting construction is to treat a stale
session as expired and reuse the existing expiry path — no new wire field, and early invalidation is
already contractual under decision 0025. It is rejected on load: re-authorising rebuilds the mask
fragment and the next viewport pays a **measured 10.7 s** row projection at 10⁹, so forcing it on
every affected session at one tick synchronises the most expensive operation in the request path
across the session population. A hint lets each client absorb the cost when it suits and spreads the
same total work over the interval.

**What bounds staleness for a client that ignores the hint already exists**: `token_max_lifetime_secs`
caps every session's life. The hint is the fast path, not the safety net, which is what lets it be
purely advisory.

**Evaluated lazily at request time, never swept** — beside the expiry check the request already
makes. Nothing walks the session registry when a flush promotes a term, which keeps the executor free
of an O(sessions) publication step and is consistent with decision 0035.

**Precise where it matters, over-reporting where it does not.** A session with no unresolved
descriptors is *never* hinted — the common case, and the one that must not regress. A session with one
is hinted whenever any term is promoted, not only its own; the false direction costs one voluntary
re-authorisation. The refinement available later is to compare digests of the unresolved descriptors
against digests of the promoted ones; noted rather than built, because it retains more than a count
does and leaks more (below).

**Three rules that keep this from becoming something it must not be:**

- **It moves in one direction only, and nothing may ever be wired to make a revocation take effect
  through it.** A stale session sees *fewer* items than its principal is entitled to — fail-closed,
  which is what makes an advisory response legitimate at all. Grant changes are not covered and must
  not be made to look as though they are; decision 0025 governs rotation, and this is not a general
  "the mask changed" channel.
- **The only remedy is a new session; `satisfied` is never re-resolved in place.** Re-resolving it
  inside a live session would break §3.4's premise 3 and with it the patch-equals-rebuild equality.
  **This is a rule rather than a structural impossibility** — the rejected expiry construction made
  it structural, and that is the property given up to avoid the load spike. It is the first thing to
  check in any future change to session handling.
- **It needs a leak-register row, scoped wider than the per-session fact.** A hint tells a viewer that
  some descriptor was interned since they authorised. It is also a *clock*: a viewer holding one
  unresolved descriptor observes the flip at its first request after a promoting flush, giving a
  repeating monitor of corpus write activity that colluding sessions can correlate — and decision
  0024 treats timing and cross-session correlation as distinct row kinds. The coarse form leaks
  strictly less than the digest refinement would, which says "a term appeared" where the precise one
  would confirm that *their specific descriptor* now exists.

**Compaction inherits one obligation:** dictionary length is the monotone counter this rests on, so a
compaction that renumbers the dictionary must not reduce it, or must introduce a counter that never
decreases.

### 3.4 The patch equals a rebuild

§11.2 specifies that flush advances `W` by OR-ing in the flushed segment's contribution for the
token's already-known satisfied terms — "a small, monotone patch rather than a rebuild". SA §6.4
claims correctness never depends on patching a fragment, and lifecycle §3.3 requires fragments to be
built from current postings. **These coexist only if the patch produces exactly the value a rebuild
would.** Here it does, on four premises, each a thing this design must maintain rather than a happy
accident:

1. The flushed entity range is contiguous, disjoint from everything below, and entirely at or above
   the pre-flush `W` — from I9's append-only allocation (§2.1).
2. Base postings are untouched by a flush (§5.3's merge/compaction line).
3. **The session's `satisfied` set is fixed at authorise and never re-resolved**, so the terms unioned
   are exactly the terms a rebuild would consult (§3.2). A design that re-resolved `satisfied` per
   request would break the equality, not merely widen it.
4. A flush publishes a *new* `segments_version`, and a fragment build in flight against the old one
   publishes into its own slot under the single-flight sequence-number rule (lifecycle §7.2, rule 2),
   so a concurrent build cannot interleave with a patch.

Given those, `old ∪ (delta ∩ satisfied)` is identical to a rebuild from current postings. The same
argument carries the row projection: the new extent's rows are disjoint from every existing row.

**Asserted by a property test for byte-equality against a full rebuild, not by this paragraph.**

### 3.5 Entities under a deny at flush time

**The rules are relative to the buffer snapshot the flush took.** A delete accepted *after* the
snapshot produces a deleted entity that does have a row, hidden by its standing overlay entry alone —
safe today only because nothing retires, and an obligation the compaction spec inherits.

For dispositions visible at the snapshot, the three behave differently because lifecycle §3.1's
relationship between each and the postings differs:

- **Suppressed → flushed normally.** A suppression never touches postings and retires only on
  unsuppress; a flush that skipped it would leave a later unsuppress with nothing to reveal.
- **Deleted → never written into the segment.** The ID stays burned (I9), no row is created, the deny
  entry stands.
- **Carrying an evaluate entry → the WAL row's terms are written, and the evaluate entry stands.**
  Writing the *entry's* current terms instead would be the fold — invariant-bearing, and compaction's.
  This is the sentence that stops the fold arriving as a simplification.

**A node in `WalPoisoned` publishes nothing.** Under the apply-anyway rule (lifecycle §4) an
under-durable delete is in force in memory and answered 500, and contracts §3.1's residual is that a
restart makes the item visible again. A flush honouring such a delete would skip the entity and
advance `W` past it; replay would then discard the delete record, leaving the item in no segment and
no buffer — the un-acked delete made **permanent**. Not publishing while poisoned costs ingest
visibility during WAL degradation, when nothing new is being made durable anyway.

### 3.6 The external-id directions

`external_id_runs` gives external_id → entity. The **reverse** direction is served
live-map-first, locator-second (contracts §2.4), where the live map is rebuilt by WAL replay and
`ext-locator.u32` is one file of build-time length. Without a durable reverse path for flushed
entities, an item visible on the map would answer `/v1/items` with a typed error forever once its WAL
region is reclaimed. So a flush publishes a **locator extent** for its entity range alongside the
forward extent, and the loader consults base locator then extents.

**The ingest duplicate check must consult flush runs too.** `LiveState::established_collisions`
justifies its bundle-side check as unable to go stale, because the sidecar it reads is immutable —
which ceases to hold once flush publishes new external-id runs and rotation empties the live map
at restart. The failure it guards is the worst one the write path documents: a byte-identical copy of
a suppressed document that no external id names, so no deny can ever reach it.

## 4. Policy and configuration

Admin-configurable, all validated at startup:

| Knob | Governs |
|---|---|
| `flush_max_age_secs` | the tick: visibility latency, and the publication period |
| `flush_max_items` | buffer size at which a flush becomes ready (executed at the next tick) |
| `ingest_buffer_max_items` | buffer-occupancy admission bound (§1.3) |
| `segment_floor_bytes` | below this, segments compare equal for selection |
| `max_merged_segment_bytes` | cap on any single merge |
| `tier_width` | segments per tier before a merge is selected |

One relation is enforced — a configuration **violating** it is refused at startup:

1. `max_merged_segment_bytes < base segment bytes` — §5.3.

*(The pin relation that used to be relation 1 is gone with pin retention; §1.3 and §1.4 record what
replaces it, which is a cost rather than a refusal.)*

Defaults: `flush_max_age_secs` 90 s. **Nothing now forces that number** — it was raised from 60 to
clear the pin relation's 75 s floor, and it is kept because it is what this deployment has run at,
not because anything refuses a shorter one. §1.4 is the cost to weigh before lowering it.

`flush_max_items` and `flush_max_age_secs` are today parsed and asserted **inert** by a test, replaced
by one asserting both bounds are honoured — epic #3's "honoured rather than parsed".

## 5. Merge policy

### 5.1 Tiered, over flush segments, adjacent extents only

`tier_width` segments in a tier selects a merge; `segment_floor_bytes` makes a tail of tiny segments
compare equal so it does not dominate selection; `max_merged_segment_bytes` bounds any single merge.
Selection on the executor against the current generation; execution on the pool over immutable
inputs; publication rebases (§1.2).

**Merge selects only entity-adjacent extents**, so k extents collapse to one and the extent list stays
minimal and ordered. A size-only policy would produce segments covering discontiguous entity sets and
the list would fragment monotonically with nothing but compaction to repair it. The cost is stated
rather than hidden: a large segment can block a merge of its neighbours.

### 5.2 Merge coalesces delta postings, and that is not a fold

Flush produces one delta postings tier per segment, and a fragment build unions across every live
tier. Bounding segments while leaving tiers unbounded moves §0's serving cliff from the tile path to
the authorise path, where it is worse, because a fragment build is a session's first-viewport cost.

**A merge coalesces its inputs' delta postings into one tier**, as a content-preserving re-encode: the
same (term, entity) pairs, concatenated, **deduplicated** and re-sorted, nothing dropped and nothing
rewritten. No tombstone is applied, no evaluate entry's terms are consulted, no overlay entry becomes
retirable. Coalescing is to postings exactly what §2.2's merge-sort is to Morton codes.

The dedup is neither optional nor a fold: `WalRow.descriptors` are not deduplicated on the buffer
path and `encode_posting` hard-fails on any non-strictly-ascending entity list, so
concatenate-and-sort alone specifies an artefact the encoder refuses. Dedup is set semantics, so
invariant-neutrality is untouched.

### 5.2b Merge coalesces external-id runs, for the same reason and by a different rule

Flush publishes one **run** per segment (contracts §2.4) — a file sorted by caller-supplied keys.
Unlike an extent, a run cannot be ordered against its neighbours, because nothing coordinates what
keys a caller supplies. So a lookup cannot select *the* run a key falls in; it must search every run
whose own first/last key could contain it, and that scan is O(runs) on `/control/ingest`'s duplicate
check and `/v1/items`' drill-down.

**A merge coalesces its inputs' runs into one**, by merge-sorting their keys — the same
content-preserving re-encode §5.2 applies to postings, and the same reason: bounding segments while
leaving runs unbounded moves the cost rather than removing it. Nothing is dropped and nothing is
rewritten; a key present in two inputs cannot arise, because an external id names one entity and the
duplicate check refuses a second (contracts §3.1).

This is what makes the run count bounded by the merge policy exactly as the segment count is, and
contracts §2.4's O(runs) scan therefore bounded too. **Stated because it was not**: an earlier
revision specified merge over row-space extents and delta postings tiers and said nothing about
runs, while §2.4 assumed something kept their number down.

⊘ **Not implemented** — merge itself is not built, so nothing coalesces runs today and their number
grows with every flush.

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

There is a sharper form. Base files live in `MANIFEST.files`. A merge consuming base must either leave
them digested there with nothing referencing them, or write a new prefix — at which point it *is*
compaction under another name. Hence relation 2 in §4.

### 5.4 No deletes-percentage trigger

§11.3 lists tombstone reclamation on a deletes-percentage trigger as one of three reasons to merge.
It is not one here: reclaiming tombstoned rows is a fold. Merge's invariant-neutrality is a property
to preserve rather than an accident.

## 6. Coordinates outside the quantisation bounds

Morton codes are computed against `MANIFEST.json`'s `quantisation` (contracts §2.5), fixed at build,
and nothing today validates an ingested item's coordinates against it — which has never mattered,
because a buffered item never acquires geometry. Flush is the moment it does.

`/control/ingest` **refuses** such a row with a typed 4xx naming the bounds, before anything is acked
or WAL-durable. Fail-closed: nothing is silently misplaced, and a clamped item at the boundary would
be indistinguishable from a legitimately edge-located one.

**Rows already WAL-durable when this validation lands are quarantined, not retried forever.** A
pre-existing out-of-bounds row would otherwise fail its flush on every tick, turning §10's "buffer
retained, retried next tick" into a permanent visibility outage for the whole partition. Such rows
are moved to a quarantine list, counted, alarmed and excluded from flush; they remain invisible,
which is the state they were already in.

The stated cost: a deployment whose data drifts outside its declared bounds cannot ingest those items
until it re-quantises, which is compaction's shape and is recorded as a compaction obligation.

## 7. The WAL

### 7.1 Rows: recovery is idempotent by construction

**The buffer is reconstructed at recovery as `{WAL rows with entity_id ≥ the published watermark}`.**

Flushed rows already have geometry, and the `watermark` in the served `SEGMENTS-<n>.json` says
precisely which. Replay therefore cannot duplicate or lose a row at *any* crash point, and
`Flush{n, wal_pos}` is purely a replay-start optimisation and the authority for rotation — never a
correctness device.

### 7.2 The overlay is not rows, and rotation must not treat it as such

The overlay's only durable home is the WAL: recovery is `replay(&records, …)`, and nothing else
persists it — `SegmentsManifest::deny` has exactly one writer in the tree and it writes `Vec::new()`.
`Change` records are captured by no segment, and a suppression retires **only** on unsuppress
(lifecycle §3.1), so a suppression's record must outlive every checkpoint. Reclaiming WAL files below
a flush checkpoint would delete accepted denies and re-expose their items on the next restart — the
row lifecycle §8 says must never exist.

**So each rotation writes a compacted overlay snapshot at the head of the new WAL file**, fsynced
**before** any older file is deleted. The overlay's durable home stays the WAL; no contract changes;
and because dispositions are idempotent, re-writing them is safe by construction. The snapshot is
O(live overlay) — the quantity the overlay soft limit already gauges, which makes the existing gauge
the right alarm for rotation cost too.

**Two details of the snapshot's shape are not free choices.**

- **Entries are keyed by `EntityId`, never by external id.** A `Change`-shaped snapshot would
  re-resolve each external id at replay, and a deleted-at-flush entity has no row and may have no
  extent entry, so `replay` would return `UnknownExternalId` and **the node would refuse to open**.
- **Evaluate entries carry raw descriptors, never `TermId`s.** Extension ids are assigned in replay
  order by `DescriptorResolver`, and rotation changes replay order, so a persisted extension `TermId`
  dangles — pointing at whatever descriptor interns into that slot next. The same hazard `buffer.rs`
  counts downward from `u32::MAX` to avoid, arriving by a different route.

**A node whose live overlay has diverged from its durable WAL publishes nothing and rotates nothing.**
`Wal::discard_undurable` deliberately does not un-apply — "a restart will not carry them" — so after
an in-process recovery the node returns to `Running` while holding dispositions no record backs, and
§3.5's `WalPoisoned` gate no longer covers it because the node is no longer poisoned. Writing the
snapshot or a flush manifest from that overlay would make a 500'd, never-acked deny **permanent**,
contradicting contracts §3.1's residual.

So the executor tracks divergence, and a node that has recovered in-process keeps serving and keeps
applying denies but **publishes no flush and rotates no WAL until restarted**, alarmed throughout.
This keeps lifecycle §4's central argument true without exception — the resulting state is always one
some restart could have produced — at a stated cost: ingest visibility stops until an operator
restarts the node, and the alarm is what makes that an operator's decision rather than a silent
stall. Re-appending the divergent entries to converge the WAL was the alternative, and is rejected
because it produces a state no restart could have produced.

### 7.3 The order, the commit point, and what `wal_pos` means

**`wal_pos` is the offset of the flush's buffer-snapshot point — the position below which every ingest
row has been consumed into a segment.** It is *not* the offset of the `Flush` record, and the
distinction is the whole of this section's safety. Group-commit allocation makes entity order equal
WAL append order, so such a position always exists and is exact.

Under the other reading, rotation deletes rows acked *during* the flush — appended after the snapshot
point, never consumed, carrying entity ids at or above the new watermark — and §7.1 then reconstructs
them from nothing: acked ingest, silently lost at the next restart.

```
pool:     segment files, delta postings, dict extents / external-id runs / locator extents durable
      →   SEGMENTS-<n+1>.json durable                    ← the commit point
executor: generation swap                                 ← the publication event
      →   Flush{n, wal_pos} appended and fsynced          ← optimisation; wal_pos = snapshot point
      →   rotation: overlay snapshot written and fsynced  ← §7.2, before any deletion
      →   WAL files wholly below wal_pos deleted,         ← reclamation, oldest first
          oldest first
```

A crash before the side-manifest leaves orphan files nothing references, and replay re-flushes
deterministically — lifecycle §8's "mid-flush (files, no manifest)" row, as tested behaviour rather
than an inherited obligation.

**Deletion is oldest-first**, because a crash midway through an unordered deletion leaves a gap in the
sequence, and this section fails closed on a gap — turning a benign crash into a permanently
unopenable node. **Steady-state retention is two files**: the file holding the snapshot point is
generally still live above it, so it survives its own rotation.

**Recovery walks every surviving file in sequence order, applying the snapshot at the position it
occupies** — it does not start *at* the snapshot. Those are different algorithms, and the second skips
the surviving older file's post-snapshot-point rows.

Each file carries its own fsync-offset sidecar (decision 0038), and **every lifecycle §4 rule applies
per file, unchanged**: the positional CRC rule ("position decides, not damage"), the three sidecar
guards, and truncate-and-fsync before a handle is issued. The snapshot is an ordinary record under all
of them — a torn snapshot below the fsync point is corruption of acked state and fails closed; above
it, it is discarded with the rest of the undurable tail, and the older file, not yet deleted, still
carries the overlay. A gap mid-sequence fails closed.

**The allocator floor survives rotation via the side-manifest, not the WAL.** `WritePath::reconstruct`
seeds the allocator from `max(manifest high-water, WAL high-water)`, and rotation deletes the `Lease`
and `IngestBatch` records the WAL term derives from. So recovery seeds from the **side-manifest's**
`entity_id_high_water` (§3.1), never from the build `MANIFEST.json`. Without this, leased-but-unwritten
ranges are reallocated — an I9 violation; seeding from the build manifest instead would reallocate
every flushed entity id.

### 7.4 The idempotency horizon is the WAL retention

`accepted_batches` is WAL-replay-derived, and `write.rs` records that a retired WAL segment regresses
old batch ids to `Unknown`. Rotation makes that reachable: after a restart, a byte-identical retry of
a batch older than the retained WAL is no longer recognised as a duplicate, and rows carrying no
`external_id` would be ingested twice (rows that carry one are caught by the live external-id map).

**The idempotency window equals the WAL retention window**, and a client retrying across it must carry
external IDs. This is a client-visible weakening of contracts §3.4's replay rule and needs recording
there as such.

## 8. Side-manifest deny state, and step-down

### 8.1 A flush-published manifest carries deny state, and the reader honours it

Contracts §2.3 makes each `SEGMENTS-<n>.json` **complete current state for its partition, not a
diff**. Flush is the first thing in the system to publish a side-manifest after build, so it inherits
that obligation: its manifest carries `deny` (the active suppression set) and `tombstones` (deleted
entities that already have rows).

Omitting them would publish a manifest silently claiming an empty deny state — a fail-open at the
interchange layer and a decision-0013 violation. Carrying them while `HONOURED_STATE` stays empty
would classify every such manifest `Unready`, so **the node could not reopen its own bundle** once any
suppression existed.

So `HONOURED_STATE` gains `"deltas"`, `"deny"` and `"tombstones"` — **each in the same change as the
code that acts on it**, per that constant's own rule. The loader applies `deny` and `tombstones` to
the initial overlay; WAL replay unions on top. Where the two differ the WAL is the superset and wins;
dispositions are idempotent, so the union is well-defined.

**Honouring a field changes what the reader does with a *valid* manifest, and must not change what it
does with an invalid one.** `unhonourable_state()` filters honoured fields out *before*
`DENY_DISPOSITION_STATE` is consulted, so honouring `"deny"` alone would reclassify a deny-carrying
manifest as `Honourable`, send it to `verify_files`, and let a digest failure `continue` the candidate
walk — **stepping down past accepted denies**, which is the fail-open decision 0018 promoted into
contract, in exactly the damaged-newest case `read.rs` names as most likely. So the branch lands with
the constant: **an honoured deny-carrying candidate that fails verification is `Unready`, never
stepped past.**

### 8.2 Step-down, and where the refusal lives

With §8.1's branch in place a manifest carrying deny state is never stepped past, which is why the
`readyz` freshness gate (#58) is **not** dragged into this epic: step-down past accepted denies is
refused outright rather than time-bounded. #58 remains an availability obligation for a lagging
replica, not a correctness one here, and there are no replicas today (lifecycle §6 is unbuilt
entirely).

A `deltas`-only step-down remains `Steppable`, and for a read-only replica it is still fail-safe
staleness. **For a node that writes it is not**: §7.1 reconstructs the buffer as WAL rows at or above
the *served* watermark, and after rotation the rows between an older manifest's watermark and the
newest one's are gone, so re-flushing from a stepped-down watermark would silently lose them.

Today every node is a writing node — `tessera-server` starts the write executor unconditionally, and
`PartitionData::stepped_down()` exists but is consumed by nothing. So the rule is: **`readyz` fails
while `stepped_down()` is true**, unconditionally, until lifecycle §6 introduces a reader/writer
distinction that makes the qualifier meaningful. Failing closed costs availability on a node whose
newest segment files are damaged, which is the trade SA §9 prescribes.

## 9. Caches

**The row-projection cache** is keyed `(token_id, slice, segments_version)`, and its own doc notes a
multi-segment slice would widen the key. A flush bumps `segments_version` every tick, and a full miss
is a **measured 10.7 s**. So the patch publishes into the **new** key a value derived from the old
entry, which respects "invalidation is key rotation, never mutation" (lifecycle §7.2) because nothing
modifies a cached value.

**Two pieces of machinery have to be built for that to mean anything:**

- **A derive-capable build path.** The cache's only entry point was `get_or_build`, with an
  infallible closure and no way to read another key's entry, so "derived from the old entry" was not
  expressible and would silently degrade to "rebuild from scratch". **Built**: `get_or_derive` reads
  the source entry under the same lock acquisition that claims the target slot, then runs the
  derivation outside it. A source miss falls back to the full build, so the answer is identical
  either way and only the cost differs.
- **Retention of superseded-generation entries.** The pruner used to run inside the publication and
  key on a drain-list reclaim, so with no pins outstanding the old entries were gone at the instant
  of the swap — before any request-driven patch could run. **Built**: retention is now an explicit
  depth on the cache (`KEEP_SUPERSEDED_GENERATIONS = 1`), stated where the cache is bounded rather
  than inherited from a pin lifetime, and the publication drops only what is more than one
  generation behind. Depth one is exactly what the patch needs and depth two is derivable from it.

Without both, the fallback is not an edge case but the steady state: a full 10.7 s projection per
session per tick, synchronised across the session population — the spike §3.3 rejects the expiry
construction to avoid, arriving through the cache instead.

**The authorisation fragment cache is a persistent disk cache, and the watermark lives in the value
rather than the key.** After flush, same-key entries would exist at heterogeneous watermarks, and
`tmp_sibling`'s "both writers wrote byte-identical content" argument would no longer hold. **The
watermark joins the disk cache key.**

One consequence, and one this design asked for and does not get. Every pre-upgrade on-disk entry
becomes unreachable — a leak rather than a fail-open, since new code can never read one. **This
section asked for a format version and an orphan sweep to reclaim them, and both are struck**
(owner ruling, 2026-08-02): nothing is deployed, so there are no pre-upgrade entries anywhere and
the machinery would migrate from a state that has never existed. Pre-alpha, a cache entry an
upgraded binary cannot read is deleted by deleting the cache directory. `WAL_VERSION` is kept, and
the distinction is worth stating: it is a pre-existing tag on a *durable* artefact and it protects
an in-place dev upgrade, where the fragment cache is a derived one that rebuilds itself. And, for
the compaction spec: persisted
fragments surviving a restart at pre-flush stamps would falsify lifecycle §3.2's "the cache restarts
cold" premise, which is what scopes the future retirement floor worker-locally. With the watermark in
the key, a pre-flush fragment is never found by a post-flush lookup and the premise holds.

## 10. Failure handling

The side-manifest being the only commit point makes every failure "nothing happened, retry next tick".

- **Flush task fails, or the disk fills mid-flush** — flush abandoned, buffer retained intact, alarm,
  retried next tick. Orphans are unreferenced and swept. Repeated failure raises the skip alarm (§1.1).
- **Merge fails, or its inputs are gone at publish** — output discarded, inputs still live, retried by
  the next selection.
- **Repeated flush failure** — the buffer grows until `ingest_buffer_max_items` 429s ingest (§1.3).
  Intended backpressure. The WAL headroom rule (lifecycle §4) is untouched, so **denies always have
  room and are never refused for load**.
- **`WalPoisoned`, or an overlay diverged from the WAL** — no flush publishes and no rotation runs
  (§3.5, §7.2).
- **Out-of-bounds coordinates** — refused at ingest; pre-existing ones quarantined (§6).

**One cost this design adds to the deny path, modelled not measured.** The executor's rebase removes
the consumed range from the then-current buffer, O(buffered) on the executor thread — the shape the
deny-ack memo measured as the dominant term at 1 M buffered (165 ms p50). A publication per tick adds
one such stall ahead of the deny lane per tick. The memo's method prices it; it should be re-run
rather than reasoned about.

## 11. `tessera build` is initial-load only

Today `tessera build` is an **overwrite**: it reads a source corpus and writes a complete bundle at a
new prefix, and nothing in it reads the WAL, the buffer or the overlay. That is harmless only while
ingest is invisible — an omitted buffered item has no geometry, so a rebuild that drops it changes
nothing observable, and the WAL replays it into the buffer against the new bundle.

**Once flush exists, an overwrite silently deletes acked, visible items**, whose acknowledgement was a
durability receipt (contracts §3.1). §5.1's "entity IDs stable across rebuilds" tacitly assumes the
rebuild's input contains the same items, which ceases to be true at the first ingest.

**So `tessera build` runs against an empty bundle root and never again.** After that the deployment is
the durable record. Additions enter by ingest → flush. Reorganisation — re-quantisation, the fold,
re-ranking, a batch-grid change — is **compaction**, which reads the deployment rather than the
source. A from-source rebuild remains available as an explicitly identity-breaking migration producing
a *new* deployment, on §12.5's build-under-a-new-prefix-and-flip precedent.

Enforced, not documented: build refuses a bundle root containing a `CURRENT`.

**This breaks nothing, checked rather than assumed.** The refusal already exists in `validate_args`,
and every invocation in the tree complies: `scripts/build_full.sh` builds into a fresh `--out`
(`--carry-id-key-from` reads a *different* root, which is not forbidden),
`scripts/bench_build_fixtures.sh` removes the directory first, the oracle harness `rmtree`s before
rebuilding and conformance rides it, and every Rust test and bench uses a temporary or pre-removed
directory. `run_demo.sh` contains no build. What changes is the *meaning* and the message: the
existing advice "remove it or choose another `--out`" must be reworded, since deleting the root is
catastrophic once the deployment is the durable record.

## 12. Epic #8: how a batch enters an existing bundle

#8 opens with a design decision that must be recorded before code is written: appended as a new
segment, merged, or staged.

**Appended.** A batch enters by the ordinary ingest path and becomes a flush segment — never merged
into base, never staged. Entity IDs remain append-only and never reused (I9), and the batch's sort
scope is the commit window, exactly as for streamed arrivals.

#8's remaining work is therefore not about entry shape: it is the batch-grid identity guard (recording
batch size in the bundle and refusing a rebuild at a different one) and bulk-ingest ergonomics. The
measured caveat carries across and is #8's to answer: §11.1 records that a bulk load chunked into
small requests forfeits the entire signature-sort win **permanently**, so bulk ingest needs large
commit windows.

## 13. Invariant and leak-register interactions

- **I4** — the extent dispatch stays inside the permutation module (§2.1).
- **I2 / §7.2 r24** — θ's anchor `V_total` is counted in row space, so it advances **at flush
  boundaries**, not per arrival. §11.2 already says this; flush makes it the normal steady state of an
  ingesting deployment. Stated as an expected observable so it is not filed as a bug.
- **C4** — drill-down resolves a bit in entity space before looking up a row, so a still-buffered item
  passes the entity-space test and then finds no row, and must return the same *unknown* outcome as an
  identifier naming nothing. Flush **shrinks** that window to `flush_max_age_secs`; it does not close
  it, and C4's timing closure remains a claim about identical outcomes rather than identical work.
- **A new register row for §3.3's staleness hint**, scoped to include its clock and correlation
  properties.
- **I11** — the within-request rule only: a request resolves geometry once and uses it throughout.
  Flush is the first mechanism that moves geometry often enough for that to be exercised. The
  cross-request half, and the retention that served it, are deleted (`geometry-pinning.md`);
  **conformance §4.4 tested I11 through the pin, so I11 moves from covered to uncovered** and that is
  recorded as a negative result rather than left silent.
- **I7, I9** — untouched. Flush allocates no IDs and moves no selection route; the direct-evaluation
  set merely shrinks as items acquire postings.
- **The retirement floor** — does not exist and is not created here. §9's watermark-keyed disk cache is
  what keeps lifecycle §3.2's worker-local scoping argument true across a restart, which is the one
  place this design could have made the floor unimplementable.

## 14. What must be proven

Extending the lifecycle §7.3 fault switchboard rather than building a second beside it.

1. `patch == rebuild`, **byte-equal**, as a property test over random corpora and tokens (§3.4),
   including a concurrent-build-during-flush case for premise 4.
2. Crash-replay idempotence at every ordering point of §7.3: no duplicated or lost row.
3. A suppression accepted before a rotation is still in force after a restart (§7.2).
4. A row acked *during* a flush survives rotation and a restart (§7.3) — the case that pins `wal_pos`'s
   definition rather than its prose.
5. Deleted-at-snapshot acquires no row; suppressed-at-snapshot does, and a later unsuppress reveals it;
   a delete arriving *mid-flush* leaves a row hidden by its overlay entry (§3.5).
6. A `WalPoisoned` node publishes no flush, and an under-durable delete's item is visible again after
   restart (§3.5, contracts §3.1's residual).
7. A node that recovered its WAL in-process publishes no flush and rotates no WAL (§7.2).
8. A deny-carrying manifest whose files fail verification is `Unready`, never stepped past (§8.1).
9. `readyz` fails while `PartitionData::stepped_down()` is true (§8.2).
10. Segment count **and delta-tier count** bounded under sustained ingest; merge keeps both bounded
    (soak).
11. The ack→visibility gap is bounded by `flush_max_age_secs`, including when `flush_max_items` trips
    between ticks (§1.3).
12. Ingest is refused by buffer occupancy, not only by queue depth (§1.3).
13. A flush patches a session's row projection rather than rebuilding it (§9), asserted on the absence
    of a full projection build rather than on timing, and the superseded entry survives long enough to
    be patched.
14. The allocator floor survives rotation: after rotation and restart no entity id is reallocated,
    seeded from the side-manifest's `entity_id_high_water` (§7.3).
15. An evaluate entry round-trips a rotation with its descriptors intact, and a deleted entity with no
    row is recoverable from the snapshot rather than failing the open (§7.2).
16. A flushed item answers `/v1/items` after rotation (§3.6).
17. `seg_id` never reused across flush or merge; `SEGMENTS-<n>` strictly monotonic and unpadded
    (decision 0016).
18. A stamp presented across a flush is answered normally with the staleness signal set — never a refusal — and the response reflects a post-flush deny.
19. §4's merge-size relation refuses a violating configuration at startup.
20. `check-layers.sh`'s one-`.store(` rule still passes — publication stayed single-owner.
21. An out-of-bounds ingest is refused before ack and leaves no WAL record; a pre-existing one is
    quarantined rather than retried (§6).
22. `tessera build` refuses a bundle root containing a `CURRENT`, with the reworded message (§11).
23. A session holding an unresolved descriptor is hinted stale by a promoting flush and re-authorising
    sees the items; a session holding none is **never** hinted; and a hinted session continues to
    serve normally, since no request may fail on an advisory signal (§3.3).

## 15. Out of scope

Compaction and the fold; the deletion stamp ledger, the retirement floor and the evaluate-entry fold;
the `readyz` freshness gate (#58, and §8.2 records why it is not needed for correctness here);
`/control/allocate-ids` (#61); the router/worker protocol (lifecycle §6).

## 16. Corpus updates that land with the code

Present tense about absent machinery reads as an assurance (decision 0013), so the ⊘ markers come out
with the code:

- **concurrency-lifecycle** — §1.1 (generation shape), §1.2 (`segments_version`'s movement), §1.3 (the
  two-publisher marker), §2.1 (reclaim's caller), §4 (the `Flush` record, rotation, the overlay
  snapshot), §5.1 (flush), §5.2 (merge), and the flush and merge rows of §8's crash matrix.
- **architecture** — §11.2 (flush as the visibility mechanism), §11.3 (the re-rank decorator and the
  deletes trigger).
- **contracts** — §2.3 (a flush-published manifest's deny state, §8.1), §2.4 (the locator extent,
  §3.6), §3.2 (the staleness hint's wire representation, when client-facing work specifies it), §3.4
  (`POST /control/flush` executes at the next tick; the idempotency horizon, §7.4).
- **Appendix C** — the staleness-hint row (§3.3, §13).
- **inventory, conformance** — the flush- and merge-dependent markers.
- **`HONOURED_STATE`** — gains `"deltas"`, `"deny"`, `"tombstones"`, each with its acting code (§8.1).
- **`buffer.rs`** — its "until the next build assigns a durable term id" now names flush (§3.2).

Six decision records: the geometry/overlay publication split and the single geometry cadence (§1.3);
dropping the Morton re-rank decorator (§2.3); refusing out-of-bounds coordinates at ingest (§6); the
overlay snapshot as the WAL's rotation rule (§7.2); `tessera build` as initial-load only (§11);
the row-projection retention depth replacing the drain list as what bounds superseded-generation entries (§1.4, §9).

Issues closed or reduced: #3 (flush), #59 (the second publisher), #8's design decision (§12).

## Appendix R — Review record

Reviewed independently twice; both rounds returned needs-rework. Both texts are kept verbatim in
`../evidence/memos/` ([r1](../evidence/memos/2026-08-02-flush-and-merge-r1-review.md),
[r3](../evidence/memos/2026-08-02-flush-and-merge-r3-review.md)), because this trail records the
substance of the findings that changed the design and not the ones it answered without moving.

The architecture survived both — single publisher, publication by rebase, one geometry cadence,
snapshot-at-rotation, `tessera build` as initial-load-only. What did not survive was the treatment of
the WAL as reclaimable: the overlay's only durable home is the WAL and `Change` records are captured
by no segment, so reclaiming below a flush checkpoint deleted accepted denies (§7.2). The second round
found the same class of defect twice more in the fix itself — an undefined `wal_pos` deleting rows
acked during a flush (§7.3), and a `HONOURED_STATE` change that reopened step-down past accepted
denies (§8.1).

Ruled by the owner: the overlay's durable home is the rotation snapshot rather than retention-by-pin
(§7.2); a node whose overlay has diverged from its WAL refuses publication until restart rather than
converging the WAL (§7.2); `tessera build` is initial-load only (§11); contracts §2.3's immediate deny
publication stands, on the geometry/overlay split (§1.3); and the staleness signal is advisory rather
than an expiry (§3.3).

The corrections answering the second round have not themselves been re-reviewed, and are not
queued for it: a third round would be confirming that edits were applied rather than attacking the
design's shape, which the disposition did not change.
