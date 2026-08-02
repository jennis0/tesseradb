# Independent review — flush-and-merge design r1

**Date:** 2026-08-02 · **Verdict:** needs-rework · **Reviewer:** independent agent, no stake in the
design

Preserved verbatim because Appendix R records the substance of only twelve of the eighteen findings,
which would leave the closure of the other six unauditable by anyone, now or later. The second
reviewer raised exactly that.

Section references are to r1 of [`../../design/flush-and-merge.md`](../../design/flush-and-merge.md);
numbering shifted in r2
and r3.

---

The single-publisher/rebase/one-cadence architecture survives review. What does not survive is the
spec's treatment of the WAL as reclaimable and the side-manifest as flush-only state: the two
together contain at least one outright fail-open path and two contract-inconsistent ones.

## Blocking findings

**B1 — §7.3's WAL truncation deletes acked denies. Fail-open, the exact class the corpus treats as
non-negotiable.** The overlay's only durable home is the WAL: recovery is `replay(&records, …)`
(`tessera-engine/src/write.rs`, `tessera-lifecycle/src/overlay.rs`), and nothing persists overlay
state anywhere else — side-manifest `deny`/`tombstones` are written by nothing and honoured by
nothing (`tessera-store/src/manifest.rs`, `HONOURED_STATE = &[]`; the spec adds only `"deltas"`).
Sequence: suppress item X (`Change` record, fsynced, 200) → several flushes pass → §7.3 deletes
"files wholly below `wal_pos`" → restart → replay starts at the checkpoint → no `Change` record →
overlay empty → **X is visible**. Same for deletions. `Flush{n, wal_pos}` bounds *buffer* replay
correctly, but `Change` records are not rows and are captured by no segment; suppressions retire
*only on unsuppress* (lifecycle §3.1), so their records must outlive every checkpoint. Fix: rotation
may reclaim only files containing no live `Change` records; or `Change` records are carried forward
at rotation; or the overlay gets a durable home. As drafted, §7.1's "replay cannot lose a row at any
crash point" is true and beside the point.

**B2 — What flush writes into `deny`/`tombstones`, and how the node reopens its own manifest, is
unspecified — and every resolution the spec permits is broken.** Contracts §2.3: each
`SEGMENTS-<n>.json` is complete for its partition, and any accepted deny change triggers immediate
publication. If flush's manifest carries the current suppression set: `Honourability::Unready` — the
node **cannot reopen its own bundle** after a restart once one suppression exists, since the spec
adds only `"deltas"` to `HONOURED_STATE`. If it omits the set: the manifest is complete-state
interchange data that reconstructs a suppressed-item-visible state on any replica — SA §6.2's
fail-open at the interchange layer, and a decision-0013 violation since flush would publish
manifests silently claiming an empty deny state.

**B3 — §7.3's "a node in `WalPoisoned` may still publish a completed flush" durably encodes
un-durable denies.** Under the apply-anyway rule (lifecycle §4), a delete whose durability failed is
applied in-memory and answered 500; contracts §3.1's stated residual is "the item is visible again
after a restart". Sequence: item ingested and acked → WAL degrades, delete arrives, applied
in-memory, 500 → flush runs per §3.4, skips writing the entity, advances W past it, publishes →
crash → replay discards the un-durable delete record; the item is < W, not in the buffer, not in the
segment. The un-acked delete's effect is now **permanent**. Fix: a flush's §3.4 decisions may act
only on WAL-durable dispositions, or a `WalPoisoned` node does not publish flushes.

**B4 — Flushed entities lose their `external_id` on the drill-down direction after truncation.**
Flush publishes `external_id_extents` (external_id → entity) only. The reverse direction is served
"live map first, locator second" (contracts §2.4); the live map is rebuilt from WAL replay, and
`ext-locator.u32` is one file of build-time length. After truncation and restart, `/v1/items` on a
visible flushed item misses the live map and runs off the end of the locator — a typed error,
forever, for every post-build item.

**B5 — Nothing bounds delta postings tiers; the spec's own cost argument for merge ignores the
authorise path.** §0 justifies merge by tile cost per live segment, but §5.2 scopes merge to "row
space only", so `terms/deltas-<n>.arrow` files are neither folded nor coalesced. At ~1,000
segments/day a fragment rebuild unions across an unboundedly growing tier list until a compaction
that is out of scope and unbuilt. Either merge coalesces its inputs' delta files — a
content-preserving re-encode that retires nothing — or the growth is stated as an accepted cost with
compaction named as its only bound.

**B6 — `flush_max_items` breaks the pin relation the spec claims is refused at startup.** §1.3
validates `pin_ttl_secs < drain_depth_max × flush_max_age_secs`, but the actual publication period
under load is set by `flush_max_items`, which can trip far faster. Lifecycle §2.2's obligation
constrains the publication period whatever moves it. Additionally `POST /control/flush` (contracts
§3.4) is an out-of-cadence publisher the spec never mentions.

**B7 — §3.2's promotion is unimplementable against the current dictionary plumbing, and its
session-visibility consequence is unstated.** `satisfied` is resolved once at authorise against
`Engine.dict`, an `Arc<Dict>` loaded at `Engine::open` and threaded into `WritePath::new` and the
`DescriptorResolver`. Promotion requires the dictionary to become generation-scoped, touching
authorise, the write path and the resolver — none of which appears in §13. Two consequences must be
stated: a promoted descriptor is satisfiable only by sessions authorised **after** the flush (which
is also what saves §3.3's byte-equality, an argument the spec nowhere makes); and items still
buffered under the old extension id stay invisible until their own flush. (The downward-counting
collision argument survives promotion — checked against `buffer.rs`; no finding there.)

**B8 — Adding `"deltas"` to servable state plus §7.3 truncation turns step-down from staleness into
silent permanent loss.** Flush publishes `SEGMENTS-<n+1>`, checkpoint fsynced, WAL files below
`wal_pos` deleted → a data file of segment n+1 is later damaged → reader steps down to n
(deltas-only is `Steppable`) → §7.1 reconstructs the buffer as WAL rows ≥ W_n, **which no longer
exist**. The rows are gone, silently, and the freshness gate that would bound the stepped-down state
is unbuilt.

## Non-blocking findings

**N1 — The cache story is absent entirely.** The row-projection cache is keyed
`(token_id, slice, segments_version)` and its own doc says a multi-segment slice "would widen this
with `seg_id`"; a flush bumps it every 90 s and a full projection miss is a measured 10.7 s. §3.3's
"only the extent part is recomputed" needs a stated shape against the single-flight discipline
("invalidation is key rotation, never mutation" — a patch that reads the evicted old entry has no
source). The authz fragment cache is a **persistent disk cache** whose key excludes the watermark:
post-flush, same-key entries exist at heterogeneous watermarks, `tmp_sibling`'s byte-identical
argument dies, and persisted fragments surviving restart at pre-flush stamps falsify lifecycle
§3.2's "the cache restarts cold" premise that the future retirement floor's worker-local scoping
rests on.

**N2 — Watermark off-by-one.** §3.1 says "W advances to the flush's `entity_hi`", but composition
treats entities ≥ W as buffered. At `W = entity_hi` the highest flushed entity is excluded from the
fragment and absent from the buffer — invisible. W must be `entity_hi + 1`.

**N3 — §3.4's rules are snapshot-relative and the spec states them as absolute.** A delete arriving
after the flush snapshotted the buffer produces a deleted entity **with** a row, hidden only by the
standing overlay entry — safe today solely because nothing retires. State the cut, and add the
mid-flush-delete interleaving to the proof list; the obligation as written is timing-dependent.

**N4 — Evaluate-on-buffered at flush is unspecified.** For a buffered entity carrying an evaluate
entry, the spec must say flush writes the **WAL row's** terms and the evaluate entry stands. Writing
the entry's current terms is a fold — invariant-bearing, and compaction's. One sentence closes it;
its absence is how the fold sneaks in "to simplify".

**N5 — Idempotency horizon.** `accepted_batches` is WAL-replay-derived, and `write.rs` already
records that a retired WAL segment regresses old batch ids to `Unknown`. Truncation makes this
reachable for the first time: a byte-identical retry after restart duplicates rows that carry no
`external_id`. The spec should own the horizon as a stated observable.

**N6 — Multi-slice premise.** `WalRow` carries no slice, and with more than one slice entity IDs
interleave across slices, so §2.1's "contiguous, ascending entity range" is false. One slice exists
today — state the assumption, and consider adding `slice` to `WalRow` now, since the WAL format is
append-only and flush freezes the layout.

**N7 — §1.4's Arc-sharing is asserted in the present tense about machinery that will not exist.**
Today `open_bundle` maps every file freshly per call and a `Generation` holds one `Arc<Bundle>`; the
"marginal cost ≈ one flush segment" claim requires an incremental `Bundle` construction the spec
neither owns nor names. The O(bound) `validate_rows` at open is the cost a naive re-open pays per
flush and is worth a sentence.

**N8 — Unstated liveness/concurrency rules:** one flush in flight at a time; whether an empty-buffer
tick still publishes a pending merge (as written, a completed merge on an idle system waits
indefinitely); and out-of-extent rows *already WAL-durable* when §6's validation lands poison every
future flush of their range, making "retried next tick" a permanent visibility outage with no
quarantine rule.

**N9 — Executor rebase cost.** Removing the consumed range from the then-current buffer is
O(buffered) on the executor; the deny-ack memo measured the O(buffered) clone as the dominant
deny-latency term at 1 M buffered. A flush publication per 90 s adds another such stall ahead of the
deny lane. Modelled, not measured.

**N10 — Wording.** §4's "Two relations are refused" — the *violations* are refused. §4's "90–120 s,
which is the floor" — the floor is 75 s. Future deny-triggered side-manifest publications will share
the `n`-space with flush; reserve it.

## Things the spec gets right that a rewriter might undo

- Publication-by-rebase with `seg_id` ABA-safety — leans on contracts §2.1's never-reused rule; do
  not replace with pointer-equality checks.
- The suppressed-flushed / deleted-skipped split — a "simplifying" uniform rule is fail-open on
  unsuppress; this is the three-way retirement split's flush-time face.
- Merge is row-count-preserving and folds nothing — dropping the deletes-percentage trigger is
  correct, not an omission.
- Refusing out-of-extent coordinates rather than clamping, with the rebuild cost stated.
- The watermark-partition recovery rule — idempotence by construction rather than by ordering is the
  right shape; B1/B8 are about what else the WAL holds, not about this rule for rows.
- Dropping the Morton re-rank decorator as a recorded decision — the argument that the sort is the
  index, not an optimisation, is correct against `tile_ranges`.
- Keeping downward-counting extension ids through promotion — verified against `buffer.rs`.
- One cadence — the factor-of-two arithmetic is right; B6 is a hole in its enforcement, not in its
  choice.

## Questions escalated to the owner

1. **Where does the overlay live durably once the WAL is truncatable?** (B1/B2 collapse into this.)
   WAL `Change`-record carry-forward; side-manifest deny lists honoured at open (which drags the
   deny-writer + freshness-gate "one unit" obligation into this epic); or no truncation of
   `Change`-bearing files until compaction exists. This sets a security invariant.
2. **May an offline `tessera build` run against a deployment that has flushed?** A rebuild re-interns
   the dictionary, re-derives postings from source, and swaps a bundle whose watermark and dictionary
   disagree with live overlay/buffer term ids. Flush makes this collision reachable.
3. **WAL retention depth vs step-down** (B8): how many published versions must the WAL be able to
   re-cover?
4. **Is `pin_ttl` 300 s / depth 4 the thing to keep, or is visibility latency the thing to keep?**
