# Independent review — flush-and-merge design r3

**Date:** 2026-08-02 · **Verdict:** needs-rework · **Reviewer:** independent agent, no stake in the
design

Second round. The reviewer was asked to do two things: verify each r1 finding was closed **by a
mechanism** rather than by prose, and attack the material nobody had reviewed (r2's new sections and
r3's §3.3), weighted at §7.2 — the fix for r1's fail-open, where a defect reintroduces the exact
failure the rework existed to close.

Section references are to r3; numbering shifted slightly in r4.

---

## R1 findings

- **B1** (WAL truncation deleted acked denies) — **PARTIALLY CLOSED.** The rotation snapshot is a
  real mechanism, fsynced before deletion, with a test — for *denies*. But §7.2/§7.3 never define
  `wal_pos`, and under its natural reading rotation deletes **acked mid-flush ingest rows** (NB1).
  The snapshot's provenance is unspecified, so it can durably persist dispositions the caller was
  told were not durable (NB2).
- **B2** (side-manifest completeness) — **PARTIALLY CLOSED.** Deny/tombstones are carried and
  honoured, but the specified `HONOURED_STATE` change *removes* the existing refusal it depends on
  (NB3), and §8.1 silently contradicts contracts §2.3's immediate-publication rule (NB6).
- **B3** (`WalPoisoned` publication) — **PARTIALLY CLOSED.** "No flush while poisoned" is a
  mechanism; "acts only on WAL-durable dispositions" has none, and the divergence survives
  `discard_undurable` recovery (NB2).
- **B4** (entity→external_id) — **CLOSED.** Locator extent published per flush, loader order stated,
  tested.
- **B5** (delta tiers unbounded) — **CLOSED.** Per-merge coalescing bounds tiers with segments;
  invariant-neutrality verified against code. One wording defect (NN8).
- **B6** (pin relation) — **CLOSED.** All three publishers on the tick, relation validated at
  startup. Note: the *code's* current `DEFAULT_FLUSH_MAX_AGE_SECS = 60` violates relation 1 — the
  default must move in the same change or a default-config server refuses to start.
- **B7** (dict promotion) — **CLOSED.** Generation-scoped `Dict` named with its touch-sites, both
  visibility consequences stated, §3.3 gives the under-seeing session a remedy.
- **B8** (step-down + rotation) — **PARTIALLY CLOSED.** The rule is correct; the mechanism is a
  sentence. No writer/reader distinction exists anywhere, `PartitionData::stepped_down()` is consumed
  by nothing, and the spec never names where the refusal lives.
- **N1** (caches) — **PARTIALLY CLOSED.** The fragment-cache fix is right and verified (the watermark
  is in the *value*, not the key). But the row-projection patch is **not expressible** under the
  current cache (NB5), and the key change orphans every pre-upgrade disk entry with no GC (NN5).
- **N2** (watermark off-by-one) — **CLOSED.** `entity_hi + 1` verified against `compose.rs`.
- **N6** (`WalRow` slice) — **CLOSED.** Field added now, with the append-only-format reasoning at the
  site.
- **N7** (Arc-sharing) — **CLOSED.** §1.2 names incremental construction required; §1.4 marks the
  memory claim modelled-not-measured.
- **N3, N4, N5, N8, N9, N10** — **UNVERIFIABLE.** No r1 review document existed in the repository, so
  closure of six findings could not be checked by anyone (NN10). *(Closed in r4 by committing the r1
  review alongside this one.)*

## Verdict: needs-rework

The architecture again survives — single cadence, publication by rebase, snapshot-at-rotation, and
build as initial-load-only are all right, and several sections verified clean against code (§2, §3.3,
§3.4's premises, §5.2, §11). But §7.2, the fix for r1's fail-open, has two defects of the same class
it exists to close, and §8.1's mechanism deletes an existing fail-open guard.

## New blocking findings

**NB1 — `wal_pos` is undefined, and the natural reading deletes acked ingest at rotation.** Rows R1
acked → tick T: flush snapshots the buffer at WAL offset P → rows R2 acked mid-flush (appended after
P, before the `Flush` record) → publication; `Flush{n, wal_pos}` appended at offset Q > P → rotation
deletes files "wholly below `wal_pos`". If `wal_pos = Q` — the natural reading, and the only one the
diagram suggests — R2's records are deleted; R2's entity ids are ≥ the new watermark, so §7.1
reconstructs them from nothing: **acked ingest silently lost at the next restart**. Group commit
makes entity order equal WAL append order, so the correct definition exists: `wal_pos` is the offset
of the flush's buffer-snapshot point. Relatedly, §7.2's "recovery reads the snapshot, then the
records after it" and §7.3's "recovery walks the sequence in order" are two different algorithms —
the first skips the surviving old file's post-snapshot-point rows, losing them via the recovery path.

**NB2 — Snapshot and manifest deny state have unspecified provenance, and the live overlay is not the
durable overlay.** Under lifecycle §4's apply-anyway rule a failed deny is applied in memory and
answered 500; `Wal::discard_undurable` recovers the handle and *deliberately does not un-apply*.
After recovery the node is `Running` again, so §3.5's `WalPoisoned` gate no longer protects anything
— yet the live overlay permanently contains dispositions the durable WAL lacks. If the rotation
snapshot or the flush manifest is written from the live overlay (the spec never says what it is
written from), an un-acked, 500'd delete becomes permanent — contradicting contracts §3.1's residual
and lifecycle §4's own rule that side-manifest publication is gated on WAL durability. §3.5's "a
flush acts only on WAL-durable dispositions" is a sentence: `OverlayEntry` carries no durability
marker and nothing else distinguishes the two.

**NB3 — §8.1's `HONOURED_STATE` change removes the refusal §8.2 relies on.** `unhonourable_state()`
filters out honoured fields *before* `DENY_DISPOSITION_STATE` is consulted. The moment
`"deny"`/`"tombstones"` join `HONOURED_STATE`, a deny-carrying manifest classifies `Honourable`,
proceeds to `verify_files`, and on a digest failure the walk does `continue` — **stepping down past
accepted denies**, the exact fail-open decision 0018 promoted into contract, in exactly the
mid-sync/damaged-newest case `read.rs`'s own doc names as most likely. The claim that
"`Honourability`'s existing classification already refuses that" is false after the spec's own
change, and the pinned test loses its protection.

**NB4 — The backpressure story is refuted by the code.** `ingest_queue_bound` bounds the command
queue — `sync_channel(queue_bound)`, default 32 jobs. The 429 keys on `try_send` full, i.e.
submission rate exceeding executor service rate; nothing anywhere compares `IngestBuffer` occupancy
to anything. Since the executor drains jobs into the buffer in milliseconds, there is **no ingest
rate at which buffer size produces a 429** — between ticks the buffer is bounded by nothing, and "the
excess is 429'd exactly as today" describes a mechanism that does not exist.

**NB5 — §9's row-projection patch has no expressible mechanism, and its failure mode is the load
spike §3.3 was written to avoid.** The cache's only entry point is `get_or_build` with an infallible
closure; there is no peek/read API, so "a value derived from the old entry" can only be expressed as
"possibly rebuild the old entry". Worse, `prune_generation` runs *synchronously inside*
`publish_geometry`: with no pins outstanding, old-generation entries are pruned at the instant of the
swap — **before** any lazy per-session patch can run. "Prune runs only after the patch publishes" is
unsequenceable with today's callers, because patching is request-driven and unbounded in time. The
fallback is then the norm: a full 10.7 s projection per session per tick.

**NB6 — §8.1/§1.3 contradict contracts §2.3's deny-publication rule, silently.** Contracts §2.3:
"any accepted deny-disposition change… triggers **immediate** publication of a new side-manifest, not
deferral to the next flush — a syncing replica must never reconstruct a state in which a suppressed
item is visible." The spec puts every publication on one tick and §16's contracts updates do not
touch this rule. Either deny changes remain an off-tick publisher (then "exactly one cadence" is
false and the interaction with `n`-monotonicity and `segments_version`-keyed caches is unexamined),
or the contract rule is being revoked for the single-node stage (then §16 must say so). Owner ruling
required.

## New non-blocking findings

1. **§7.2's snapshot format is load-bearing and unspecified.** Evaluate entries must be snapshotted
   as raw *descriptors*, never `TermId`s — extension ids are assigned in replay order and rotation
   changes replay order, so persisted extension `TermId`s dangle. And if snapshot entries are
   `Change`-shaped (external-id-keyed), a deleted-at-flush entity — which gets no row and possibly no
   extent entry — resolves nowhere after rotation: `replay` returns `UnknownExternalId` and the node
   refuses to open.
2. **The allocator floor after rotation is never named.** `WritePath::reconstruct` seeds from
   `max(manifest_high_water, WAL high-water)`; after rotation deletes `Lease` and `IngestBatch`
   records the WAL term regresses, and only the side-manifest's `entity_id_high_water` can hold the
   floor. If the implementer wires the *build* `MANIFEST` high-water instead, flushed entity ids get
   reallocated — catastrophic.
3. **The ingest duplicate check must consult flush extents.** `LiveState::established_collisions`'
   doc says the bundle-side check "cannot go stale" because the sidecar is immutable — false once
   flush publishes new extents and rotation empties the live map at restart. The failure it guards is
   the documented worst one.
4. **Old-file deletion order unspecified.** Non-oldest-first deletion plus a crash leaves a
   mid-sequence gap, which §7.3 fails closed on — a permanently unopenable node from a benign crash.
   Also "retention returns to one file" is wrong under the corrected `wal_pos`; steady state is two.
5. **Fragment disk cache: watermark-in-key orphans every pre-upgrade entry.** No cache format version
   exists and nothing ever deletes disk entries. Old entries are unreachable (leak, not fail-open —
   confirmed), but the orphans need a sweep.
6. **§3.3's leak-register row is under-scoped.** The hint is also a clock: a viewer holding one
   unresolved descriptor observes the flip at the first request after a promoting flush — a repeating
   corpus-write-activity monitor, correlatable across colluding sessions. Decision 0024 treats timing
   and cross-session correlation as distinct row kinds.
7. **`drain_depth_max` is a compile-time constant today**, and `pins.rs` records that as "a constant
   rather than a config key deliberately". §4 makes it a knob without acknowledging the reversal.
8. **§5.2's "concatenated and re-sorted" must say "and deduplicated".** `WalRow.descriptors` are not
   deduped on the buffer path, and `encode_posting` hard-fails on any non-strictly-ascending list.
   Dedup is set-semantics content-preserving, so invariant-neutrality survives; the sentence as
   written specifies an encoder-refused artefact.
9. **§11's refusal already exists** (`validate_args`) but is untested, and the error message's advice
   "remove it or choose another `--out`" becomes actively dangerous once the deployment is the
   durable record.
10. **The review record does not preserve r1.** Six of eighteen findings exist nowhere in the
    repository; closure is unauditable by anyone. Attach or commit the r1 review text.
11. **§8.2's refusal needs a site.** Today every node is the writing node, so the rule is "refuse
    readiness when `PartitionData::stepped_down()`", unconditionally, until lifecycle §6 exists.

## What §11 breaks

**Nothing — checked concretely, and the worry did not survive.** The refusal is already implemented
and every invocation in the repo already complies: `scripts/build_full.sh` (fresh `$OUT`;
`--carry-id-key-from` reads a *different* root, not forbidden), `scripts/bench_build_fixtures.sh`
(`rm -rf` before build), `run_demo.sh` (contains no build at all), `reference/oracle/harness.py`
(`rmtree` before rebuild; conformance rides it), all Rust tests and benches (tempdirs or pre-removed
dirs). Build does write into the root it checks, so rebuild-over-root is already refused today — the
ruling changes semantics and messaging, not behaviour.

## Questions escalated to the owner

1. **NB6:** does contracts §2.3's "immediate side-manifest publication on every deny-disposition
   change" stand, or is it revoked for the single-node stage?
2. **NB2:** the remedy for post-recovery overlay divergence — re-append divergent denies at recovery
   (makes 500'd dispositions durable deliberately, softening contracts §3.1's residual), or refuse
   flush/rotation until restart (availability cost). Both are fail-closed; they give the contract
   different meanings.
3. **§7.4:** "the idempotency window equals the WAL retention window" is a client-visible weakening of
   contracts §3.4's replay rule, presented as an observable rather than a ruled change.
4. **Non-blocking 7:** making `DRAIN_DEPTH_MAX` admin-configurable reverses a recorded deliberate
   choice.

## Confirmed clean, for the record

Checked and did not survive as worries: §2.1/§2.2's row-space arithmetic given the stated one-slice
assumption; §3.4's four premises against `session.rs`, lifecycle §7.2 rule 2 and `compose.rs`; §3.3
at every code claim including 0020/0024/0035 consistency; §5.2's no-entity-in-two-tiers claim
(structural, via monotone allocation); the `check-layers.sh` rules cited all exist; the 10.7 s figure
is genuinely measured; `seg_id` non-reuse is contract; decision 0018/0020 compatibility of carrying
and honouring deny state as such.
