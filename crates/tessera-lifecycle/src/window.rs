//! The commit window: many ingest submissions, **one signature-sorted allocation run** (Phase 2
//! stage 2.1, Task 7a).
//!
//! ## What the window is for, and what it is not
//!
//! Design §11.1 spends the entity-ID ordering on posting compression, and the sort's scope is
//! whatever set of items is allocated together. Until this task that scope was **whatever chunk a
//! client happened to POST**. Lifecycle §5.1 moves it to the server: "hold arriving requests open in
//! a commit window bounded by size or age; at close, signature-sort **the whole window**, allocate
//! from the high-water, append and fsync once, swap, then acknowledge every held request with its
//! rows' IDs." Amortising the fsync and the generation swap is a welcome side effect; the allocation
//! scope is the point.
//!
//! **I9 is untouched, and this is the site that has to say so** — "the window allocates" reads like
//! an allocator change and is not one. IDs are still issued monotonically from the high-water by one
//! [`Allocator::allocate`] call, still never reused, still assigned in `(signature, external_id)`
//! order by the unchanged [`assign_sorted`]. The window changes only *how many* are assigned in one
//! sorted run (lifecycle §5.1: "the window changes only *how many* are assigned in one sorted run").
//!
//! ## Calibrate the win honestly
//!
//! The probes' 8.9–36.7× posting compression was measured under a **full-corpus** signature sort.
//!
//! **`run ≈ B × p` is an upper bound, not the expected run length**, and the distinction is worth one
//! to two orders of magnitude. [`assign_sorted`] sorts on an item's whole sorted, deduplicated term
//! list — its **signature** — not on any single term. A term's ids are contiguous only across items
//! whose *entire* signature is equal, so `B × p` is attained only where the term is effectively the
//! signature: a corpus of one term per item, which is exactly how this crate's
//! `the_window_run_ratio_against_the_full_sort_ceiling` builds its corpus, so that test cannot fail
//! the bound and must not be read as evidence for it. Where a term co-occurs with others, its
//! postings split across every signature carrying it.
//!
//! **What the measured corpus implies.** Probes results §3, categories-subclass at ε=0: **54,791
//! distinct signatures over 2.42 M items**, mean group 44, group size by rank 4,213 (rank 100) → 444
//! (rank 500) → 158 (rank 1,000) → 42 (rank 2,500), top 500 groups covering 82.4%. A 10 000-row
//! window drawn from that distribution holds ≈ 17 rows of the rank-100 group and ≈ 0.65 of the
//! rank-1,000 one — runs of order 10¹ and 1, not ~200. **The ~200 figure is the ceiling for a
//! leading term and not a forecast for the median one**, and an operator sizing this knob from it
//! should expect one to two orders of magnitude less at 10⁹, having paid the window latency and the
//! resident rows in full. Re-measuring this against a per-window permutation of the probe corpus is
//! the open item (Task 7a fix round 1, F2 — the measurement was not run, no probe exists for it).
//!
//! **And the cost that grows with `B`.** [`assign_sorted`] is `n log n` over the window's rows with a
//! `Vec<u32>` sort key per item and a lexicographic compare per comparison. The per-item key
//! allocation is per-row either way, but the comparison count is not: 100 rows to 10 000 is
//! log₂ 10⁴ / log₂ 10² = **twice the comparison work per row**. Raising the bound buys run length
//! sub-linearly (the groups it reaches are smaller) and costs sort work, window latency and
//! residency; it is not a free dial.
//!
//! And it is a fraction in a specific, nameable way. Design §11.1's container model gives the
//! benefit available to a term of density *p* at sort scope *B* as `max(1, 2¹⁶/(p·B))`, and notes
//! that `p·B < 2¹⁶` for every `p ≤ 1` once `B ≲ 6·10⁴`. Every window size this deployment's heap
//! budget permits is below that. So a window collects the **posting-storage** (run-encoding) win and
//! **none of the container-count** win — and container count is what a union costs
//! (`bitmap operations cost O(containers touched), not O(cardinality)`). Nothing here is wrong about
//! that; it is simply not what this lever reaches.
//!
//! ## The window carries ingest, and deny dispositions stay out of it
//!
//! Lifecycle §5.1 *permits* deny dispositions to share the window — "**may** share", a permission
//! rather than a requirement. **The permission is declined**, and the reason is that it buys almost
//! nothing and is paid for in the machinery that keeps denies fail-closed. A reader who thinks the
//! mixed window is the obvious next step should read this before building it.
//!
//! **What it would buy.** One fsync, and only for the deny. A deny concurrent with ingest pays its
//! own append and fsync today; folded into a window it would ride the window's single fsync. The
//! *ingest* path saves nothing at all — a change record joins an fsync that was going to happen
//! anyway, so the write path's fsync count per ingested row is unchanged. Against measured deny
//! acknowledgement latency (`docs/evidence/memos/2026-08-01-deny-ack-baseline.md`) the saving is
//! zero on a quiescent node, because there is no concurrent ingest to share a window with, and a few
//! per cent under sustained ingest, where the wait is dominated by the in-flight work item.
//!
//! **What it would cost.** Three things, and each is larger than the fsync.
//!
//! 1. **A per-entry durability fold.** §4's apply-anyway rule — a deny whose durability write failed
//!    is applied regardless, because a refusal that leaves an item visible is worse than an
//!    under-durable hide — is scoped to `Delete` and `Suppress`. An `Unsuppress` applied without
//!    durability re-exposes an item that replay still hides. A mixed window's failure handler must
//!    therefore answer for every operation at once, per entry, over a log whose poisoning makes
//!    "which entries reached the file" a position question rather than a batch one.
//! 2. **An intra-window ordering hazard that does not exist otherwise.** A live `suppress` followed
//!    by its `unsuppress` must not replay inverted. Both ride one first-in-first-out lane and are
//!    each executed to completion with their own append, fsync, apply and swap, so submission order
//!    is apply order at every step and there is no sequence to reorder. A window is what would
//!    introduce one.
//! 3. **A type that stops saying what is true.** [`WindowEntry`] is a struct. An unconstructed
//!    `Change` variant would assert in the type that deny dispositions are windowed when they are
//!    not, and leave a reader to guess which failure rule covers it.
//!
//! So this type carries ingest entries only, and the deny lane keeps its own path in the executor:
//! drained to empty before each window is filled, so a deny waits at most for the window in front of
//! it. That bound comes from the executor yielding at every window close, not from the order of the
//! two drains — see `Executor::run_work_pass`.

use std::hash::{Hash, Hasher};
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};
use tessera_types::{EntityId, TermId};

use crate::alloc::{assign_sorted, AllocError, Allocator, PendingItem};
use crate::command::UnallocatedRow;
use crate::wal::{WalRecord, WalRow};

/// One admitted `/control/ingest` submission, held open until the window closes.
///
/// `waiters` is a `Vec` and not one responder because **Task 8's join** ([`CommitWindow::join`])
/// appends a byte-identical retry's responder to an entry already held here, so both callers
/// receive the same ids off one allocation. Task 8 is what makes it hold more than one; before it,
/// a retry forced the window to close and was answered from the durable index instead.
///
/// Generic in the waiter type, and that is forced rather than stylistic: the engine's `Responder`
/// is `pub(crate)` inside a private module (`tessera-engine`'s `write.rs`, `mod ack`), whose private
/// field is the whole of the ack-ordering guarantee — a successful receipt cannot be constructed
/// without proof that the generation carrying it is live. It must not become nameable from here.
/// The window never *does* anything to a waiter, so it needs to know nothing about one.
pub struct WindowEntry<W> {
    pub rows: Vec<UnallocatedRow>,
    pub batch_id: String,
    pub body_hash: [u8; 32],
    pub waiters: Vec<W>,
}

/// One entry after allocation: the record to append, and everything the apply and the ack need.
pub struct ClosedEntry<W> {
    /// Always [`WalRecord::IngestBatch`], with every row carrying its assigned id — replay reuses
    /// them rather than re-deriving placement (lifecycle §5.1, SA §6.2).
    pub record: WalRecord,
    /// The resolved term set per row, in the record's row order. Carried beside the record because
    /// [`WalRow`] has no `terms` field: the WAL stores raw descriptors, since a term coined between
    /// builds has no durable ordinal.
    pub terms: Vec<Vec<TermId>>,
    /// The assigned ids, **in the caller's submitted row order** — what the ack returns.
    pub entity_ids: Vec<EntityId>,
    pub waiters: Vec<W>,
}

impl<W> ClosedEntry<W> {
    /// The record's rows, for the buffer apply. Panics only if this type is ever built from
    /// something other than an `IngestBatch`, which [`CommitWindow::allocate`] is the sole producer
    /// of.
    pub fn rows(&self) -> &[WalRow] {
        match &self.record {
            WalRecord::IngestBatch { rows, .. } => rows,
            _ => unreachable!("a ClosedEntry's record is always an IngestBatch"),
        }
    }

    /// `(batch_id, body_hash)` — the idempotency key the executor records after the swap.
    pub fn batch_key(&self) -> (&str, [u8; 32]) {
        match &self.record {
            WalRecord::IngestBatch {
                batch_id,
                body_hash,
                ..
            } => (batch_id.as_str(), *body_hash),
            _ => unreachable!("a ClosedEntry's record is always an IngestBatch"),
        }
    }
}

/// The open commit window.
pub struct CommitWindow<W> {
    entries: Vec<WindowEntry<W>>,
    /// **Task 8's join index.** A `batch_id` held here cannot be evaluated against the idempotency
    /// map, because that map is written at *apply*; [`CommitWindow::held`] is what answers for it,
    /// and [`CommitWindow::join`] is what a byte-identical retry does with the answer. Task 7a
    /// used this field to force a window *close* instead, so that the durable check could answer;
    /// Task 8 answers from inside the window and the close is gone.
    by_batch: FxHashMap<String, usize>,
    /// **Hashes** of the external ids held by this window — see [`CommitWindow::holds_external_id_of`]
    /// for why hashes and not ids, and why `None` is absent from it.
    external_ids: FxHashSet<u64>,
    rows: usize,
    /// When this window opened. Read by the executor to time the window's service, and by **Task
    /// 7b**, which owns the age bound. 7a lands no timer.
    opened_at: Instant,
    /// The window's sequence number. **Task 8's `BatchState::Held { window_seq, .. }`** is what this
    /// is for; nothing in 7a reads it beyond diagnostics.
    seq: u64,
}

impl<W> CommitWindow<W> {
    pub fn new(seq: u64) -> Self {
        CommitWindow {
            entries: Vec::new(),
            by_batch: FxHashMap::default(),
            external_ids: FxHashSet::default(),
            rows: 0,
            opened_at: Instant::now(),
            seq,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries — submissions, not rows.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Rows across every entry. **This is what `commit_window_max_items` bounds** — see the config
    /// key's own doc: its default is sized from `window rows × term_density`, and the heap and the
    /// latency a window costs both scale in rows, not in submissions.
    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn opened_at(&self) -> Instant {
        self.opened_at
    }

    /// **The `Held` half of Task 8's batch-id state machine**: is `batch_id` already an entry of
    /// this open window, and if so, what body hash was it admitted under?
    ///
    /// Returns `(window_seq, body_hash)`. The caller compares the hash: equal means a byte-identical
    /// retry, which [`CommitWindow::join`] adds to the held entry's waiters; unequal is a `409` for
    /// the retry alone (contracts §3.4, and see the engine's `BatchState` for the reading of "the
    /// batch has no effect" that is taken there).
    ///
    /// **This must be consulted before [`CommitWindow::holds_external_id_of`]**, never after. A
    /// retry names its original's external ids by construction, so an external-id-first order would
    /// answer a retry by closing the window — Task 7a's behaviour, which this task exists to
    /// replace — and would do it *even for the byte-identical case that has an exact answer
    /// available*.
    ///
    /// `window_seq` is carried because the join's correctness is a statement about **which** window
    /// the entry sits in. In stage 2.1 that cannot be got wrong: there is exactly one open window,
    /// a local of the executor's work pass, and it is consulted and joined in the same iteration —
    /// so the field is read only by a `debug_assert!` at the join site. It is not decoration and it
    /// is not a live guard either; it is the discriminator a 2.2 executor holding more than one
    /// window would need, written down while the invariant it encodes is still obvious.
    pub fn held(&self, batch_id: &str) -> Option<(u64, [u8; 32])> {
        let index = *self.by_batch.get(batch_id)?;
        Some((self.seq, self.entries[index].body_hash))
    }

    /// **The join** (Task 8): add `waiter` to the entry `batch_id` names, so a byte-identical retry
    /// is answered off the original's single allocation rather than allocating again.
    ///
    /// Nothing else about the entry changes: no rows are added, no external id is registered, the
    /// row count does not move. That is what makes the join safe against Task 3a's security C1 —
    /// the failure C1 describes needs **two** allocations for one external id, and a join performs
    /// zero.
    ///
    /// Returns `false` if `batch_id` is not held, which the executor treats as a programming error:
    /// it calls this only having just seen [`CommitWindow::held`] answer.
    pub fn join(&mut self, batch_id: &str, waiter: W) -> bool {
        let Some(&index) = self.by_batch.get(batch_id) else {
            return false;
        };
        self.entries[index].waiters.push(waiter);
        true
    }

    /// Whether admitting this submission to the **open** window would put two entries in it naming
    /// the same external id.
    ///
    /// # Why this exists — it is a security check, not tidiness
    ///
    /// Both of the executor's admission checks read state that is written at **apply**: the
    /// idempotency index (`accepted_batches`) and the live external-id map (`established`). Task 3a's
    /// security CRITICAL C1 is exactly what happens when a check and its apply are separated — a
    /// client retry under a **fresh** `batch_id` passes the handler's duplicate check twice, gets two
    /// entity ids for one external id, and the second insert overwrites the first, leaving a visible,
    /// byte-identical copy of a suppressed document that **no external id names**, so no deny can
    /// ever reach it. 3a closed it by re-checking on the one thread that also inserts. A window
    /// re-opens it, one window wide, unless the two entries are kept out of the same window.
    ///
    /// The executor's remedy is to **close the window first and re-evaluate**: once the earlier entry
    /// has applied, 3a's existing checks give exactly today's answers (byte-identical replay → the
    /// recorded ids; different bytes → 409; colliding external id → 409). No new failure semantics
    /// and no path only a window can reach.
    ///
    /// **Hashes, not ids.** One `u64` and no allocation per row, against a `Vec<u8>` clone per row.
    /// An admitted row is hashed twice — once here, once in [`CommitWindow::push`] — and that is left
    /// alone deliberately: halving it means threading the digests from this call into that one, and
    /// it is the only per-row work this task *added* against two heap allocations per row and a deep
    /// `wal_rows.clone()` it removed. Net strongly negative; not worth the reviewability.
    /// A hash collision costs a spurious early close — conservative, and the entry is re-evaluated
    /// against the live map either way — never a missed conflict.
    ///
    /// **`external_id: None` is excluded**, on the same argument the live map already makes
    /// (contracts §3.4 r6): an item with no external id is addressable only by its `tessera_id`, is
    /// established in no map, and is a duplicate of nothing. Hashing `None` would make every
    /// second entry of an id-less corpus "conflict" and silently turn group commit off for that
    /// deployment.
    ///
    /// Rows are checked against the window, never against their own entry: an intra-batch duplicate
    /// is the handler's to refuse, and it names them to the caller who supplied them.
    ///
    /// **Task 8 narrowed this to external ids.** It also refused a `batch_id` already held, for a
    /// different reason — so that the *durable* idempotency check could answer once the close had
    /// applied the original. [`CommitWindow::held`] answers that from inside the window now, so the
    /// close is no longer the mechanism and the batch-id clause is gone. What is left is C1 and
    /// only C1: two entries, two batch ids, one external id, **two allocations**. That one still
    /// forces the close, because nothing but an apply can make `established` see the first insert.
    pub fn holds_external_id_of(&self, rows: &[UnallocatedRow]) -> bool {
        rows.iter()
            .filter_map(|r| r.external_id.as_deref())
            .any(|id| self.external_ids.contains(&digest(id)))
    }

    /// Admit an entry. The caller has already established that it does not conflict.
    pub fn push(&mut self, entry: WindowEntry<W>) {
        let index = self.entries.len();
        self.by_batch.insert(entry.batch_id.clone(), index);
        for row in &entry.rows {
            if let Some(id) = row.external_id.as_deref() {
                self.external_ids.insert(digest(id));
            }
        }
        self.rows += entry.rows.len();
        self.entries.push(entry);
    }

    /// **Close the window: one signature-sorted allocation run over every row it holds.**
    ///
    /// Gather every entry's rows into one `Vec<PendingItem>` in `(entry, row)` order, hand it to the
    /// unchanged [`assign_sorted`], and scatter the ids back by position. One
    /// [`Allocator::allocate`] call for the whole window, so ids stay strictly monotone and a window
    /// that cannot allocate has **no effect at all** — `allocate` leaves the high-water mark
    /// unchanged on its error path (I9, plan Important I-1).
    ///
    /// **No per-row clone.** `external_id` and `terms` are *moved* out of each row into its
    /// `PendingItem` and moved back out afterwards, rather than copied ([`UnallocatedRow::take_pending`]
    /// and [`UnallocatedRow::into_wal_row_with`] are an exact inverse pair). Between the two a row is
    /// **hollow** — its `external_id` is `None` and its `terms` empty — and nothing may observe it in
    /// that state: the interval is this function's gather-to-frame, the conflict check above runs at
    /// admission (before it), and the error path drops the entries rather than returning them.
    /// **The error hands the waiters back**, per entry and in entries order, rather than dropping
    /// them with the window: a dropped responder is a lost receipt, which the caller must read as
    /// "this may have been applied in full" (`SubmitError::ReceiptLost`) — the exact opposite of the
    /// truth here, where the high-water mark did not move and nothing was appended. The rows are not
    /// handed back: the batch has no effect, and they are hollow by then.
    #[allow(clippy::type_complexity)]
    pub fn allocate(
        self,
        alloc: &mut Allocator,
    ) -> Result<Vec<ClosedEntry<W>>, (AllocError, Vec<Vec<W>>)> {
        let mut entries = self.entries;

        let mut pending: Vec<PendingItem> = Vec::with_capacity(self.rows);
        for entry in &mut entries {
            for row in &mut entry.rows {
                pending.push(row.take_pending());
            }
        }

        if let Err(e) = assign_sorted(&mut pending, alloc) {
            return Err((e, entries.into_iter().map(|e| e.waiters).collect()));
        }

        let mut scattered = pending.into_iter();
        let mut closed = Vec::with_capacity(entries.len());
        for entry in entries {
            let n = entry.rows.len();
            let mut wal_rows = Vec::with_capacity(n);
            let mut terms = Vec::with_capacity(n);
            let mut entity_ids = Vec::with_capacity(n);
            for row in entry.rows {
                let p = scattered
                    .next()
                    .expect("one PendingItem was gathered per row, in this order");
                let (wal_row, row_terms) = row.into_wal_row_with(p);
                entity_ids.push(wal_row.entity_id);
                wal_rows.push(wal_row);
                terms.push(row_terms);
            }
            closed.push(ClosedEntry {
                record: WalRecord::IngestBatch {
                    batch_id: entry.batch_id,
                    body_hash: entry.body_hash,
                    rows: wal_rows,
                },
                terms,
                entity_ids,
                waiters: entry.waiters,
            });
        }
        Ok(closed)
    }
}

/// A 64-bit digest of an external id, for the intra-window conflict set only. Never durable, never
/// an identity: a collision costs one early window close.
fn digest(external_id: &[u8]) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    external_id.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::WalScalar;

    fn row(external_id: Option<&str>, terms: &[u32]) -> UnallocatedRow {
        UnallocatedRow {
            external_id: external_id.map(|s| s.as_bytes().to_vec()),
            descriptors: vec![b"d".to_vec()],
            x: 1.0,
            y: 2.0,
            scalars: vec![WalScalar::U64(7)],
            terms: terms.iter().map(|t| TermId::new(*t)).collect(),
        }
    }

    fn entry(batch: &str, rows: Vec<UnallocatedRow>) -> WindowEntry<&'static str> {
        WindowEntry {
            rows,
            batch_id: batch.to_string(),
            body_hash: [0u8; 32],
            waiters: vec!["w"],
        }
    }

    /// **The headline property, at unit scope**: the sort scope is the window. Four submissions of
    /// two rows each, signatures interleaved, must produce the same assignment as one submission of
    /// all eight rows in the same order.
    #[test]
    fn the_sort_scope_is_the_window_not_the_entry() {
        let mut split = CommitWindow::new(0);
        for b in 0..4u32 {
            split.push(entry(
                &format!("b{b}"),
                vec![
                    row(Some(&format!("x{b}")), &[9]),
                    row(Some(&format!("y{b}")), &[1]),
                ],
            ));
        }
        let mut alloc_split = Allocator::new(100);
        let closed = split.allocate(&mut alloc_split).unwrap();
        let split_ids: Vec<u64> = closed
            .iter()
            .flat_map(|e| e.entity_ids.iter().map(|i| i.raw()))
            .collect();

        let mut whole = CommitWindow::new(0);
        let mut rows = Vec::new();
        for b in 0..4u32 {
            rows.push(row(Some(&format!("x{b}")), &[9]));
            rows.push(row(Some(&format!("y{b}")), &[1]));
        }
        whole.push(entry("one", rows));
        let mut alloc_whole = Allocator::new(100);
        let whole_ids: Vec<u64> = whole.allocate(&mut alloc_whole).unwrap()[0]
            .entity_ids
            .iter()
            .map(|i| i.raw())
            .collect();

        assert_eq!(
            split_ids, whole_ids,
            "four submissions must be allocated exactly as one submission of the same rows — \
             otherwise group commit is decoration"
        );
        // And the sort actually did something: every `[1]`-signature row precedes every `[9]` one.
        let low: Vec<u64> = split_ids.iter().skip(1).step_by(2).copied().collect();
        let high: Vec<u64> = split_ids.iter().step_by(2).copied().collect();
        assert!(low.iter().max() < high.iter().min());
    }

    /// Each entry's ids come back in **its own** submitted row order, and every id the window issued
    /// appears in exactly one framed `WalRow`.
    #[test]
    fn every_entry_gets_its_own_ids_in_its_own_row_order_and_every_id_is_framed() {
        let mut w = CommitWindow::new(3);
        w.push(entry(
            "a",
            vec![row(Some("a0"), &[5]), row(Some("a1"), &[0])],
        ));
        w.push(entry("b", vec![row(Some("b0"), &[3])]));
        let closed = w.allocate(&mut Allocator::new(0)).unwrap();

        let mut framed = Vec::new();
        for e in &closed {
            assert_eq!(
                e.entity_ids,
                e.rows().iter().map(|r| r.entity_id).collect::<Vec<_>>(),
                "the acked ids must be this entry's rows' ids, in this entry's row order"
            );
            for r in e.rows() {
                framed.push(r.entity_id.raw());
            }
        }
        framed.sort_unstable();
        assert_eq!(
            framed,
            vec![0, 1, 2],
            "no id issued that is not in a record"
        );

        // The rows survived the hollow interval: external ids and geometry are back where they were.
        assert_eq!(closed[0].rows()[0].external_id.as_deref(), Some(&b"a0"[..]));
        assert_eq!(closed[0].rows()[0].descriptors, vec![b"d".to_vec()]);
        assert_eq!(closed[0].rows()[1].x, 1.0);
        assert_eq!(closed[0].terms[1], vec![TermId::new(0)]);
    }

    /// The C1 backstop's window half: a second entry naming a held external id conflicts — and
    /// `None` never conflicts with `None`.
    ///
    /// The **held batch id** leg moved to `a_held_batch_id_is_found_with_the_hash_it_was_admitted_under`
    /// when Task 8 replaced that close with the join; it is deliberately not asserted here any
    /// more, because asserting it here would be asserting the thing the join removed.
    #[test]
    fn a_held_external_id_conflicts_and_none_never_does() {
        let mut w: CommitWindow<&'static str> = CommitWindow::new(0);
        w.push(entry("b1", vec![row(Some("k"), &[1]), row(None, &[1])]));

        assert!(
            w.holds_external_id_of(&[row(Some("zzz"), &[1]), row(Some("k"), &[1])]),
            "a held external id conflicts however the batch id differs — Task 3a's C1"
        );
        assert!(
            !w.holds_external_id_of(&[row(None, &[1]), row(None, &[2])]),
            "rows with no external id are duplicates of nothing (contracts §3.4 r6)"
        );
        assert!(!w.holds_external_id_of(&[row(Some("fresh"), &[1])]));
    }

    /// Task 8's `Held` lookup: the batch id, the window's own sequence number, and **the hash the
    /// entry was admitted under** — which is what decides join versus 409.
    #[test]
    fn a_held_batch_id_is_found_with_the_hash_it_was_admitted_under() {
        let mut w: CommitWindow<&'static str> = CommitWindow::new(7);
        w.push(WindowEntry {
            rows: vec![row(Some("k"), &[1])],
            batch_id: "b1".to_string(),
            body_hash: [3u8; 32],
            waiters: vec!["w"],
        });

        assert_eq!(w.held("b1"), Some((7, [3u8; 32])));
        assert_eq!(w.held("b2"), None, "an unheld batch id is not held");
        // And it is found however the retry's rows differ: the hash decides, not the rows.
        assert_eq!(w.held("b1").map(|(_, h)| h), Some([3u8; 32]));
    }

    /// The join adds a waiter and **nothing else**: no rows, no external id, no row count. That is
    /// the whole reason it is safe where a second entry would not be — C1 needs two allocations and
    /// a join performs none.
    #[test]
    fn joining_adds_a_waiter_and_changes_nothing_else() {
        let mut w = CommitWindow::new(1);
        w.push(entry("b1", vec![row(Some("k"), &[1])]));
        let rows_before = w.rows();
        let entries_before = w.len();

        assert!(w.join("b1", "retry"), "a held batch id joins");
        assert!(!w.join("nope", "retry"), "an unheld one does not");

        assert_eq!(w.rows(), rows_before, "a join adds no rows");
        assert_eq!(w.len(), entries_before, "a join adds no entry");
        assert!(
            !w.holds_external_id_of(&[row(Some("fresh"), &[1])]),
            "a join registers no external id"
        );

        let closed = w.allocate(&mut Allocator::new(0)).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(
            closed[0].waiters,
            vec!["w", "retry"],
            "both callers are owed the same ids off the one allocation"
        );
        assert_eq!(closed[0].entity_ids.len(), 1, "one row, one id — not two");
    }

    /// A window that cannot allocate has no effect: the high-water mark does not move (I9).
    #[test]
    fn a_window_that_exhausts_the_id_space_has_no_effect() {
        let mut w = CommitWindow::new(0);
        w.push(entry("a", vec![row(Some("x"), &[1]), row(Some("y"), &[1])]));
        let mut alloc = Allocator::new(u32::MAX as u64 - 1);
        let err = w.allocate(&mut alloc);
        let Err((AllocError::Exhausted { .. }, waiters)) = err else {
            panic!("a window past the u32 ceiling must be refused");
        };
        assert_eq!(alloc.high_water(), u32::MAX as u64 - 1);
        assert_eq!(
            waiters,
            vec![vec!["w"]],
            "the waiters must come back so they can be told the truth: a dropped responder reads \
             as `ReceiptLost` — 'this may be applied in full' — which is the opposite of what \
             happened here"
        );
    }
}
