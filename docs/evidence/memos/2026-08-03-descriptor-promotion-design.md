# Descriptor promotion: what it costs to close, and one fail-open it uncovers

**Status:** Design memo, 2026-08-03 — **D1–D3 ruled the same day (§9); built the same day, §10's
order followed.** Non-normative as evidence; the corpus edits in §8 are applied, and the mechanism
now lives in `flush-and-merge.md` §3.2, `contracts.md` §2.4 and decision 0042. Obligations 1–11
are covered by `tessera-authz`'s `dict::tests`, `tessera-engine`'s `tests/promotion.rs`, and the
`dispatch_rules_tests` and `a_side_manifest_is_never_replaced` unit tests. Fills in the mechanism `flush-and-merge.md` §3.2 states at the
"what" level and `flush::promote` marks ⊘. Found while building §3.3's staleness hint (Task 19),
whose end-to-end path promotion is the missing half of. Revised 2026-08-03 after review: the
publication guard is scoped and gains a dispatch rule, a pre-existing side-manifest collision is
folded in (§2), step 5 publishes from memory, and the up-counting rejection is restated as
construction-versus-discipline (§4). Revised again the same day on verification of the review's
two new hazards — both confirmed in the code: the dispatch rule is simplified to one plan per
dispatch and given an anti-starvation key, the collision is marked latent behind the single-slice
build, `Dict::extended_with` and `hard_link` are named, and two stale ⊘ markers in `write.rs` join
§8.

## 0. The result first

**Promote at flush, from the resolver's extension map.** The descriptor bytes are not lost — the ⊘
at `flush::promote` says the buffer does not retain them, which is true and beside the point.
`DescriptorResolver` holds a process-lifetime `descriptor → extension TermId` map, deduped and
already carried across replay and every live accept precisely so the assignment stays continuous.
Promotion needs the inverse of that map for the ids one flush plan touches, which is a few entries.

Everything else is already built: the extent writer, the manifest field, the loader, the ordinal
preservation rule, and the publication path. What is missing is roughly forty lines in `promote`,
one scoped guard and a one-plan-per-dispatch rule at publication, one loader fix, and a
refuse-to-replace on the side-manifest write — the last two closing a pre-existing commit-point
collision that promotion would otherwise widen (§2).

**The loader fix is not optional, and it is the reason this memo exists.** `Dict::load` and
`Dict::load_extending` disagree about a descriptor an extent repeats, and the disagreement is
between the running process and the same bundle after a restart. Measured (§3), not reasoned:

| | `a` | `b` | `len` |
|---|---|---|---|
| in memory, `load(base).load_extending(ext)` | 0 | 1 | 2 |
| after restart, `load([base, ext])` | **1** | 2 | 3 |

Postings written under ordinal 1 are `b`'s. After the restart a session granted `a` resolves to
ordinal 1 and is served **`b`'s items**. That is fail-open, it is silent, and nothing detects it.
It is unreachable today only because promotion is a no-op — no flush has ever written a second
dict extent. It becomes reachable in the same change that closes this gap.

**What the gap costs while it stands.** An item ingested under a descriptor the dictionary has
never seen is acknowledged, allocated an entity id, flushed, given geometry — and its novel term is
dropped from the delta tier. If that was its only term it has no postings at all, and it is
invisible to **every principal, for ever**, not until its flush. Fail-closed, so nothing leaks; but
a caller whose access labels include a new compartment gets a permanent silent hole rather than an
error.

## 1. What is actually broken

`buffer.rs` mints ids for unknown descriptors counting down from `u32::MAX`, so they are
unsatisfiable by construction: a novel descriptor can buffer an item but can never make it visible.
That is deliberate and correct. §3.2 says the flush is where it ends.

`flush::promote` does not end it. It skips every term at or above `dict.len()` — the extension
range — and returns the dictionary it was given, with no extent. So the item's novel term reaches
no tier, no dictionary and no manifest.

## 2. The mechanism

**Restricted to what the flush carries.** A descriptor the resolver knows but no flushed item
references is not promoted: it has no posting to write, and interning it would put a descriptor
into a durable artefact for no reason.

1. **Snapshot, lazily.** `dispatch_flushes` already runs on the executor thread and already holds
   `Arc<LiveState>`. If — and only if — some planned item carries a term at or above `dict.len()`,
   it takes `resolver_state`'s lock and builds the inverse map for those ids. A tick with no novel
   descriptors, which is the steady state, pays one comparison per item and takes no lock.
2. **Resolve each extension id, dictionary first.** `extension id → descriptor →
   ctx.dict.lookup(descriptor)`. A hit means an earlier flush already promoted it: **use that
   ordinal and write nothing to the extent.** This is what keeps §3.2's second consequence exactly
   as stated — an item buffered under a stale extension id stays invisible until *its own* flush,
   and then becomes visible under the ordinal that already exists — and it is the writer half of
   §3's fail-open guard.
3. **Intern the rest**, in a deterministic order, into a `DictStreamWriter` in the flush's own
   segment directory. Ordinals are `ctx.dict.len() + position`, which is what `Dict::load` will
   assign when it walks `dict_extents` in listed order.
4. **Write the tier in final ordinals**, sorted by term. Both branches of step 2/3 yield real
   ordinals, so no extension id ever reaches a durable file — the hazard `promote`'s doc already
   names.
5. **Publish from the same in-memory sequence that named the tier.** The published dictionary is
   the planned one plus the interned descriptors at their assigned ordinals — built directly,
   never `load_extending` over the file just written. Routing the live dictionary through a disk
   round-trip adds nothing (nothing compares, so a bad read would silently *become* the live
   assignment while the tier holds the intended one) and costs a re-read; restart equality is
   obligation 2's to prove, against the file. `Promotion { dict, extent: Some(..) }`; the manifest
   work — the digest, the `dict_extents` push — is already written and already correct.

   **This needs an API that does not exist**, and naming it here is the point: `Dict`'s fields are
   private to `tessera-authz` while `promote` lives in `tessera-engine`, so "built directly" means
   `Dict::extended_with(&self, descriptors: &[Vec<u8>]) -> Dict` — append at `self.len()`, same
   skip-if-present rule as the reader (§3). Without it the implementer reaches for
   `load_extending`, which is the one thing this step forbids.

**An extension id with no descriptor is a failed flush, not a skipped term.** The map is
process-lifetime and only grows, so this is unreachable; silently dropping a term is the bug being
fixed, and it must not survive as the error path.

**A guard at publication, scoped to flushes that wrote an extent.** `publish_flush` discards a
flush whose prefix moved and one whose row space moved. Promotion adds a third: if
`live.dict.len()` is not what a *promoting* flush planned against, the ordinals in its extent are
wrong and it is discarded — same posture, same alarm, next tick re-plans. A non-promoting flush is
exempt: its tier carries only ordinals below the planned length, which append-only extension
preserves, so discarding it would be a needless liveness hole.

The scoping matters because "at most one flush in flight" is a property of *dispatches*, not of
flushes: `dispatch_flushes` builds one context per slice, every context sharing the same planned
dictionary and the same `next_n`, executed sequentially under a single `flush_in_flight`. Two
slices promoting in one dispatch would both plan ordinals from the same `dict.len()`. The guard
still earns its keep without them, through a narrower window: `flush_in_flight` clears only after
the pool's sends, and the executor drains completed flushes *before* it ticks, so a send landing
between the drain and the in-flight check leaves the tick planning against a generation whose
completed flush is not yet published.

**The dispatch rule is simply: at most one plan per dispatch.** The review proposed scoping it to
promoting plans; the simpler rule has the same effect and one fewer concept, because the shared
`next_n` below means only one plan per dispatch can commit anyway. Deferring the rest costs
nothing that refuse-to-replace would not already cost, and saves their segment writes.

**Which plan, though, is a starvation question.** `slices_of` sorts lexicographically, so taking
the first would let a continuously-fed `s0` deny `s1` a flush for ever — and that is inherent in
"one winner per tick", not introduced by this rule: with refuse-to-replace alone the same slice
wins the same race every tick. The plan chosen is therefore **the one holding the oldest unflushed
row** — `items` is ascending by entity id, so `items.first()` is the key, and no cursor state is
needed. That turns an unbounded starvation into a bound: with *s* slices, ack→visibility is
`s × flush_max_age_secs`, which is obligation 11's and §4 relation 1's business to record rather
than something to discover later.

**This makes multi-slice flush safe, not correct.** One slice publishes per tick. The real fix is
one side-manifest per *dispatch*, covering every plan's segment — a flush unit that spans slices
rather than one per slice — which is slices work, not this memo's. Recorded so that "closes a
pre-existing commit-point collision" is not read as "multi-slice flush now works".

**None of it is reachable today.** `tessera build` emits exactly one slice (`BuildArgs::slice_id`),
and a plan for a slice the bundle does not carry is dropped at `partition_data.slices.get(&slice)`,
so `plans` never holds more than one element. The rules above are for the deployment that has more
than one, and the tests must construct the collision directly rather than through a second slice.

**Discard must also be clean at the commit point, and today it is not.** Every plan in a dispatch
writes `SEGMENTS-<next_n>.json` at the same path, and `write_segments_manifest`'s rename replaces:
a flush discarded at publication has already overwritten the *winner's* side-manifest with one
that does not contain the winner's segment or extent. A restart then opens a manifest missing rows
that were acked and published — permanent silent loss, exactly the class of hole this memo exists
to close. (Checked for the leak direction: it stays fail-closed — the WAL persists descriptors,
never `TermId`s, so nothing durable can alias the loser's ordinal assignment into the winner's.)
Worse than the disk: `publish_flush` passes `completed.manifest` — that flush's own clone of the
base plus its own segment — into `with_segment`, so the loser also overwrites the winner in the
**live** bundle, not merely on disk.

The fix is that the side-manifest write **refuses to replace** an existing `SEGMENTS-<n>.json`:
the loser's commit fails loudly and its files are orphans, the same "nothing happened, retry next
tick" posture as every other flush failure. Because the write happens inside `execute_flush`
*before* the unit is submitted, a refused loser never reaches publication at all, which is what
closes the live-bundle half too.

**`std::fs::hard_link`, not `renameat2`.** `std` exposes no `RENAME_NOREPLACE`, and `libc` is a
dependency of `tessera-build` and `tessera-bench` but not of `tessera-engine`; a syscall wrapper is
not worth a new dependency here. `hard_link(tmp, path)` fails with `AlreadyExists` if the target
exists and is atomic, and the `remove_file(tmp)` after it leaves at worst a `.tmp` orphan on a
crash — the same orphan story every other stage already accepts. The directory fsync is unchanged.

The collision is a *code-path* property of two plans in one dispatch, so it is a pre-existing
hazard promotion inherits and closes rather than one it creates — but with one slice per build it
is latent, and the dispatch rule above makes it structurally unreachable. Refuse-to-replace stays
as the guard at the format boundary, on the same belt-and-braces argument §3 makes for the loader:
one rule is a property of a caller, the other of the artefact.

## 3. The fail-open, stated precisely

`Dict::load` inserts every record unconditionally and increments the ordinal counter per record.
`Dict::load_extending` skips a descriptor the base already carries. So an extent containing a
descriptor already in the dictionary produces **different ordinals in the running process and after
a restart**, and not only for the repeated descriptor — every ordinal after it shifts by one.

Two independent fixes, and this design takes both:

- **Writer side (§2 step 2):** the flush never writes a descriptor the dictionary already has.
  Structural, and the property to assert directly in a test rather than inferring from behaviour.
- **Reader side:** `Dict::load` skips a descriptor it already holds, exactly as `load_extending`
  does, making the law `load(a ++ b) ≡ load(a).load_extending(b)` hold by construction. `len`
  becomes the count of distinct descriptors rather than of records, which differs only in the case
  the writer rule forbids.

Belt and braces is warranted here and is not the usual defensive reflex: the writer rule is a
property of one function, the reader rule is a property of the format, and the failure they guard
is a viewer being served another compartment's items with no error anywhere.

## 4. Rejected, with reasons

**Retain the descriptors on `BufferedItem`** — what `promote`'s ⊘ proposes. It stores per *item*
what the resolver already stores per *descriptor*: a million buffered items carrying one novel
descriptor would hold a million copies. It is also a change to the buffer's shape, which §2.1
deliberately trimmed to "exactly the rows without geometry". The resolver's map is the same data,
deduped, already durable through replay, and already there. It is also a cost on every clone, not
a one-off: the accept path builds each next generation's buffer by cloning the current one, and
`publish_flush` clones it again to remove consumed rows — per-item descriptor bytes would be
recopied on all of them.

**Mint durable ordinals at accept instead of extension ids** — count *up* from `dict.len()` and
publish the dictionary extension with the buffer swap the executor already performs each window.
Tempting: it deletes promotion entirely and makes an item visible at ack rather than at flush.
Rejected — and the honest reason is construction versus discipline, not a reachable leak. No
today-route exists: nothing durable ever holds a `TermId` — `Change` records and
`OverlaySnapshotEntry` persist raw descriptors, sessions and fragments die with the process, and a
flush freezes its assignments in an extent. What up-counting destroys is the *shape* of the
safety: "extension ids are unsatisfiable" stops being a property of the id range and becomes a
global negative — no site may ever persist a `TermId` outside an extent-frozen artefact — that
nothing checks and any future persistence site can silently break. `buffer.rs`'s own doc records
the previous upward scheme rotting exactly that way, prose-only and broken by growth; and
composition compares evaluate-entry terms directly against `satisfied`, so the first slip is
cross-compartment serving. The benefit it buys is visibility at ack rather than within
`flush_max_age_secs`, and architecture §3's write-latency budget (seconds to minutes, r23) says
that is not a requirement.

**Refuse an unknown descriptor at `/control/ingest`.** The simplest thing that is honest: no
promotion, no extents, no guard, and the silent hole becomes a typed error. Rejected as the primary
answer because a corpus whose access labels are open-ended is the premise of the system — an ingest
carrying a new compartment would fail until an operator rebuilt the bundle. Worth keeping in mind as
the fallback if promotion is ever ruled too much machinery: it is strictly better than the status
quo, which is the same refusal made silently and after the ack.

## 5. Costs

- **Steady state (no novel descriptors): one comparison per planned item.** No lock, no inversion,
  no extent, no dictionary clone. This is the case that must stay free and does.
- **A promoting flush clones the lookup map to build the published dictionary** — O(dictionary),
  the same cost `load_extending` would pay, minus the file re-read (§2 step 5). At the 10⁵–10⁶
  terms a real deployment carries this is tens of milliseconds on the pool thread, off the
  request path, and the obviously-correct construction to land first. **Not measured; modelled from
  the map's size.** If promoting flushes become steady state at a large dictionary, the exit is to
  layer rather than clone: `Dict` becomes an immutable shared base plus a small extension probed on
  base *miss*, so the extra probe is paid only for promoted-since-open or unknown descriptors —
  rare by this memo's own premise — never per lookup on `authorise` or ingest. Layering also
  removes the transient second copy and the full-copy-per-retained-generation multiplication a
  clone leaves behind. Not built; the trigger is promotion frequency × dictionary size.
- **At a large dictionary the clone is not the binding constraint anyway: the base map is.**
  Measured (`probes/2026-08-03-dict-fst/`, n = 1.17×10⁸, the surnames fixture's own cardinality):
  the `FxHashMap` base is 7.1 GB resident and 40–53 s of `Engine::open` rebuild on every restart,
  against 0.78 GB mmapped in microseconds for an FST over the same descriptors (uncorrelated-
  ordinal ceiling ~6.7 B/key; adversarial random keys 4.1 GB vs 9.0 GB; lookups ~1.3 µs vs
  ~0.43 µs, irrelevant at their consumers). The exit is a derived, digest-gated FST index beside
  the canonical `terms-<k>.dict` extents, which stay the format truth — the derived-artifact-gating
  frame, with a conformance obligation that index lookup ≡ walking the records. Promotion composes
  unchanged: it appends record extents and never touches the base index.
- **One dict extent per promoting flush**, not per flush. `Engine::open` reads all of them.
  Dictionary extents are **extents** in the `1a94c2e` sense — positional, ordinal order is listed
  order — so a merge may coalesce a contiguous run of them into one file with no renumbering,
  provided order is preserved and the reader rule of §3 holds. Unlike external-id runs, they need no
  key merge; unlike the base locator, they need no repair. **Task 21 inherits this**, and it is the
  cheapest of its three coalescing obligations.

## 6. Invariants, bounds and the register

- **I5** — build-time and runtime resolution both go through `Dict`; promotion only appends. The
  test-only reference oracle knows nothing of ordinals a flush assigned, which is a gap in the
  oracle's coverage of promoted terms rather than in the mechanism.
- **Ordinals are append-only and never renumbered**, the term-space analogue of I9 and the thing
  §3's fail-open breaks. `load_extending` preserves them structurally; the reader fix makes reload
  preserve them too.
- **Invariant-neutral** in the plan's sense: promotion folds no authorisation state, retires no
  overlay entry and drops no row. It makes an item visible that was invisible, which is the grantor's
  own descriptor taking effect.
- **A bound is now reachable by a caller.** An ingesting caller can grow the dictionary, one term
  per novel descriptor. `DeclaredBounds::max_distinct_terms` is 200,000,000 and
  `EXTENSION_ID_START > max_distinct_terms` is a compile-time assertion the extension range's
  safety rests on. A promotion that would carry `dict.len()` past the declared bound must **fail the
  flush** — alarm, buffer retained, ingest sheds at `ingest_buffer_max_items`, which is the intended
  backpressure and the same posture as every other flush failure.
- **Leak register: no new row.** Promotion is server-side; the dictionary is a bundle artefact no
  viewer can enumerate. The one viewer-visible effect is that a caller's ingest can flip §3.3's
  staleness hint, which is **C21** exactly as written — "corpus write activity, not content".

## 7. Conformance obligations

1. A novel descriptor ingested, flushed, is **visible to a session authorised after the flush and
   invisible to one authorised before** — §3.2's two consequences, exercised for the first time.
2. **Restart equality.** After a promoting flush, a reopened bundle resolves every descriptor to the
   ordinal the publishing process assigned. The §3 fail-open, as a test.
3. `load(a ++ b) ≡ load(a).load_extending(b)`, including when `b` repeats a descriptor in `a`.
4. The extent written by a flush **contains no descriptor the dictionary already holds** — asserted
   on the file, not inferred from lookups.
5. Two promoting flushes: the first's ordinals are unchanged by the second.
6. A descriptor promoted by flush 1, carried by an item flushed in flush 2, gets flush 1's ordinal
   and produces no second extent record.
7. Promotion past `max_distinct_terms` fails the flush and retains the buffer.
8. **Obligation 23 becomes end-to-end.** §3.3's staleness hint is currently tested through a
   hand-driven publication because no ingest can flip it; with promotion the ingest→tick→hint path
   is testable as specified.
9. A dispatch offered several plans dispatches exactly one, and it is the plan holding the oldest
   unflushed row — asserted on the dispatch, since no build produces a second slice to drive it.
10. A non-promoting flush whose planned dictionary length no longer matches is published, not
    discarded — the guard is scoped to extent-carrying flushes.
11. `SEGMENTS-<n>.json` is never replaced: a second write at the same `n` fails and leaves the
    first intact, asserted at the filesystem operation.

## 8. What this changes in the corpus

- **`flush-and-merge.md` §3.2** gains the mechanism: the resolver's map as the source of the bytes,
  dictionary-first resolution, the deterministic intern order, the publication guard and the
  `max_distinct_terms` refusal. Its two "fail-closed consequences" are unchanged — the design
  preserves both rather than closing either.
- **§5.2b / Task 21** gains dict-extent coalescing, stated as the cheap one (§5).
- **`contracts.md` §2.4** — dictionary extents need the same "positional, listed order, never
  reordered" statement the terminology split gave row-space and locator extents, and the no-duplicate
  rule is a *format* rule, not an implementation detail.
- **Four ⊘ markers are deleted by the change that lands this**, and decision 0013's rule cuts both
  ways: `flush::promote`'s and `Session::is_stale`'s second one, which this closes — and the two in
  `write.rs`'s `tick_if_due`, which claim "the flush itself" and "the segment write and the
  publication" are unimplemented. Both landed in Task 13 and
  `a_published_flush_is_a_bundle_a_restart_opens` passes against them; they are stale, they sit in
  the file the dispatch rule edits, and `inventory.md` counts them.
- **`flush-and-merge.md` §4 / obligation 11** records that ack→visibility is
  `slices × flush_max_age_secs` while a flush unit is per slice, and that the per-dispatch
  side-manifest is what collapses it back to one tick.
- **A decision record** for the reader-side `Dict::load` fix, because it changes what a published
  format means in a case that currently has an answer — the wrong one.
- **The plugin contract's "declarations, not limits" sentence gains a carve-out.** Promotion makes
  `max_distinct_terms` the one bound that is *enforced* (D3's flush refusal), because
  `EXTENSION_ID_START > max_distinct_terms` is what keeps a dictionary ordinal from ever aliasing
  a live extension id — a dictionary allowed to grow far enough past the declaration is satisfiable
  aliasing, an actual fail-open. Per-item and per-token bounds stay declarations.

## 9. Decisions

All three **ruled 2026-08-03, as recommended.**

**D1 — promote at flush, or refuse unknown descriptors at ingest?** Ruled: promote (§0, §4). Cost
if wrong: a mechanism carried for a capability nobody uses; the refusal remains available and is a
smaller change than this one, not a larger one.

**D2 — take both halves of the §3 fix, or only the writer rule?** Ruled: both. The writer rule
alone leaves a published format with a case whose two readers disagree, and the next writer of an
extent — a merge coalescing them, a compaction — has to rediscover the rule from prose.

**D3 — is a caller-driven dictionary bound acceptable at the declared ceiling?** Ruled: refuse the
flush at `max_distinct_terms` and alarm. The alternative — a lower, configurable ceiling — is a
knob with no measurement behind it, and the extension-range assertion is what the ABI bound
already protects.

## 10. Implementation order

For the implementing agent. Each step keeps the gate green on its own; the order closes the
fail-open before the path that could reach it exists.

1. **Reader rule** — `Dict::load` skips a descriptor it already holds; `len` becomes the distinct
   count, and `Dict::extended_with` arrives beside it for §2 step 5 (`tessera-authz`'s `dict.rs`),
   with obligation 3 as a test alongside.
2. **Promotion** — `flush::promote` per §2: the lazy resolver snapshot threaded from
   `dispatch_flushes` into `FlushContext`, dictionary-first resolution, `DictStreamWriter` into
   the flush's own segment directory, the `max_distinct_terms` refusal (§6), publication from the
   in-memory sequence (§2 step 5). Obligations 1, 2, 4–8.
3. **Guard and dispatch rule** — the extent-scoped `dict.len()` guard in `publish_flush`; one plan
   per dispatch in `dispatch_flushes`, chosen by oldest unflushed row (`tessera-engine`'s
   `write.rs`). Obligations 9–10.
4. **Side-manifest refuse-to-replace** — `write_segments_manifest` hard-links rather than renames
   over an existing `SEGMENTS-<n>.json` (`tessera-engine`'s `flush.rs`). Obligation 11.
   Pre-existing hazard; lands here because promotion widens it (§2).
5. **Corpus and markers** — the §8 edits, the four ⊘ deletions, and the decision record for the
   reader rule.

Out of scope, recorded not built: the layered dictionary and the FST base (§5 states the triggers
and the measurement); dict-extent coalescing is Task 21's.

**One thing this uncovered while being built, and fixed with it.** Obligation 1 asserts a flushed
item is visible through its promoted descriptor, and reaching for `/v1/items` to say so panicked:
`Engine::item` took `slice_data.segments.first()` and indexed it with a **slice**-space row, so a
drill-down on any flushed item read past the build segment's end. Pre-existing and live since the
flush landed — the drill-down had no flushed-item test — and unrelated to promotion beyond being
what the obligation walked into. The segment/`row_base` pairing `Engine::viewport` already did
inline is now `segments_with_row_bases`, used by both; a second copy is how the two came to
disagree in the first place.

Gate: `cargo test --workspace` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`bash scripts/check-layers.sh` · `python3 scripts/check-doc-links.py`.
