# Write-path review — fidelity lens

**Status:** Evidence — review transcript, never normative. One of three independent reviews of
`docs/design/write-path.md` (then r2); the others are the
[invariants](2026-08-04-write-path-review-invariants.md) and
[memory](2026-08-04-write-path-review-memory.md) transcripts. **Every finding below was
dispositioned the same day**: write-path r3 applies F2–F12, contracts r16 (amended) applies F1's
correction, and the stale code comments in the incidental note were fixed with them. Kept
verbatim-in-substance because this round is the document's Appendix R record.

**Lens:** the consolidation's one fatal failure mode — a claim that does not match the code,
laundered into the source of truth. Verified against the working tree (which included the
uncommitted r2 corrections — contracts r16, lifecycle r6, CLAUDE.md Rule S/F, `WAL_VERSION` 4,
decisions 0044–0046).

## Findings

**F1 — §4.3's file table listed a per-segment `permutation.bin` that flush has never written —
fatal-to-fidelity.** `store/flush.rs` builds the extent in memory ("Written here rather than as a
`permutation.bin`, whose length is the *bundle's* whole entity space"); the `files` map carries
exactly `morton.u32`, `columns.arrow`, `external-ids.arrow`, `ext-locator.u32` (+ `delta.arrow`,
optional dict extent); at restart the map is rebuilt from the segment's own `tessera_id` column
(`SegmentExtent::rebuild` — "nothing on disk carries it, deliberately"). r2 laundered contracts
§2.6's stale streamed-segment specification into the record as fact — the exact hazard this
review existed to catch; the document had caught the analogous `delta.arrow` discrepancy in the
very next table row and missed this one.

**F2 — §1.3/§10's "disk-full never refuses a deny; the ingest 429 sits below the WAL hard
bound" describes machinery that does not exist — significant (missing ⊘).** `wal_hard_limit_bytes`
is consumed only by a startup relation over the command queue's worst case; no runtime 429 is
keyed on WAL size, nothing bounds the log at runtime, and `config.rs`'s own doc carries the ⊘ the
document omitted. *(Owner subsequently ruled the runtime machinery a nice-to-have; the document
now states the truth.)*

**F3 — mixed idsets in one request are 422 (`ApiError::Contract`), not the 409 §5.7's table
claimed** — significant.

**F4 — the ingest-admission 429's `Retry-After` is derived** (`estimate_retry_after_s`, one work
item's service time, clamped 1–300 s), **not the "fixed 1"** §2.4 and §10 claimed — "fixed 1" is
the compute gate's number, a different subject the code keeps deliberately distinct — significant.

**F5 — no "flushable-items gauge on `/control/status`" existed**: `ExecutorStats` carried the
counters and nothing serialised them; the skip/failure alarms were log lines only — significant.
*(Wired with the disposition: the `write_executor.flush` block.)*

**F6 — §2.2 still described the fragmentation ratios at commit-window scope** after r16 had
rescoped them to delta tiers — significant.

**F7 — the §0 overview diagram still showed the deleted `Flush` WAL record** that the document's
own §1.3/§4.5 said was gone — significant.

**F8 — §2.1's admission steps 8 (coordinates) and 9 (buffer occupancy) were inverted** relative
to the code: occupancy is checked in the handler before the coordinate refusal in
`Engine::accept_ingest` — minor.

**F9 — `Change{external_id}` is written by nothing** (both address forms resolve in the handler
and write `ChangeByEntity`; the variant survives for postcard variant-order stability), and the
document listed it unqualified after deleting `Lease` under exactly the written-by-nothing
criterion — minor.

**F10 — the 125.12 MB projection-entry figure is not in the memo §12 cited**; its measured home
is `probes/results.md` §4.2 — minor (misattribution; classification was right).

**F11 — §5.6 said the buffer is reconstructed "against the served watermark"**, contradicting §9
and the code (the has-a-row predicate) — minor.

**F12 — the tessera-address range check compares the shard half against the manifest's
`shard_id`**, not literally zero — minor.

## Categories attacked that survived

Both lanes' ack orderings end to end (loop order, append ×k → one fsync → apply → one swap →
ack ×k, the `Published` token minted only at the swap, idempotency index updated post-swap,
ingest-fail-applies-nothing vs the deny fold, the two-close deny bound, the conflict-close
yield); the §5.5 deny failure fold exactly (repair-then-fail with 50+200 ms backoff ×3, per-op
asymmetry, applied-anyway entries never marked for publication, the batch fold's 500-over-503);
§9 recovery (seed-before-replay, has-a-row filter, allocator floor, WAL-position stamping,
replayed batch index); §4.5 rotation (seal → snapshot-at-head → fsync → delete wholly-below
oldest-first, `Some(None)` pins, gates); §4.4's five publication steps including the
promoting-only dictionary guard and `hard_link` refuse-to-replace; every §10 default and both
constants, the two deleted keys refused by `deny_unknown_fields`, the prompt `/control/flush`
with `FLUSH_COMPLETION_POLL`; all twelve marked ⊘s true of the code (every failure was in the
*unmarked* direction); every other §12 figure present in its named source at its stated
classification.

**Incidental (outside the document):** three stale "there is no flush"-class comments in
`write.rs` and one dead `Flush{n, wal_pos}` reference in `wal.rs` — the class §13.3 claimed was
swept. *(Fixed with the disposition.)*
