//! The commit window: many ingest submissions, **one signature-sorted allocation run**.
//!
//! ## What the window is for, and what it is not
//!
//! Design §11.1 spends the entity-ID ordering on posting compression, and the sort's scope is
//! whatever set of items is allocated together. Without a window that scope is **whatever chunk a
//! client happened to POST**. Lifecycle §5.1 moves it to the server: "hold arriving requests open in
//! a commit window bounded by size or age; at close, signature-sort **the whole window**, allocate
//! from the high-water, append and fsync once, swap, then acknowledge every held request with its
//! rows' IDs." Amortising the fsync and the generation swap is a welcome side effect; the allocation
//! scope is the point.
//!
//! "The window allocates" reads like an allocator change and is not one. IDs are issued by one
//! [`Allocator::allocate`] call, freed ids first and then from the high-water, and assigned in
//! signature order by [`assign_sorted`]. The window changes only *how many* are
//! assigned in one sorted run.
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
//! resident rows in full. **Not confirmed by measurement, and do not claim it is**: no probe runs a
//! per-window permutation of the probe corpus, so the figure above is modelled from the measured
//! signature distribution rather than observed.
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
//! rather than a requirement. **The permission is declined**
//! (docs/decisions/0033-both-lanes-group-commit.md), and the reason is that it buys almost
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
//!
//! ## The fragmentation tally
//!
//! [`CommitWindow::allocate`] also measures the assignment it just made, which is what
//! `/control/status`'s `fragmentation` reports (contracts §3.4). See [`FragmentationTally`] for what
//! the numbers mean, what they deliberately do not, and why this is the site that has the
//! information.

use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};
use mosaica_types::{EntityId, TermId};

use crate::alloc::{assign_sorted, AllocError, Allocator, PendingItem};
use crate::command::{UnallocatedEdit, UnallocatedRow};
use crate::wal::{RowOutcome, RowReceipt, WalEdit, WalRecord, WalRow};

/// One admitted `/control/ingest` submission, held open until the window closes.
///
/// `waiters` is a `Vec` and not one responder because **the join** ([`CommitWindow::join`])
/// appends a byte-identical retry's responder to an entry already held here, so both callers
/// receive the same ids off one allocation. The alternative — forcing the window to close so the
/// durable idempotency index can answer — costs a close per retry and allocates nothing extra.
///
/// Generic in the waiter type, and that is forced rather than stylistic: the engine's `Responder`
/// is `pub(crate)` inside a private module (`mosaica-engine`'s `write.rs`, `mod ack`), whose private
/// field is the whole of the ack-ordering guarantee — a successful receipt cannot be constructed
/// without proof that the generation carrying it is live. It must not become nameable from here.
/// The window never *does* anything to a waiter, so it needs to know nothing about one.
pub struct WindowEntry<W> {
    /// The rows this batch writes.
    pub rows: Vec<UnallocatedRow>,
    /// The items this batch moves to new entities.
    pub edits: Vec<UnallocatedEdit>,
    /// One per row of the request, in request order: what the row became.
    pub slots: Vec<Slot>,
    /// The request rows creating an item indexed under more than
    /// [`mosaica_authz::MAX_KEYS_PER_ITEM`] keys.
    pub over_bound: Vec<u32>,
    pub batch_id: String,
    pub body_hash: [u8; 32],
    /// The artifacts this batch's rows join, each with the **ordinal its key resolved to at
    /// admission** and the row positions that named it (`artifacts-from-points.md` §6.2). Empty for
    /// a batch carrying no membership column, which is every batch that names no layer.
    pub memberships: Vec<ResolvedMembership>,
    /// The parent edges this batch's list column declared **that the close has to settle**: the ones
    /// whose child it is about to mint, and the ones whose child exists and holds no parent. An edge
    /// the layer already holds agreed at admission and is gone by here. A minted child takes its
    /// parent from these; an existing one is filled with it.
    pub edges: Vec<crate::command::BatchEdge>,
    pub waiters: Vec<W>,
}

/// What one row of an ingest request became, as the window's close answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The row writes the entry's row at this position: it creates an item, or adds the item
    /// whose `tessera_id` it carries to a view.
    Written { row: u32, tessera_id: Option<u64> },
    /// The row named this item and changed nothing, so it writes nothing.
    Unchanged { entity: EntityId, tessera_id: u64 },
    /// The row places this item in artifacts that do not hold it and changes nothing else: a
    /// change to the artifacts, not the item, which keeps its entity and is answered unchanged.
    Joined { entity: EntityId, tessera_id: u64 },
    /// The row moves its item to a new entity as the entry's edit at this position.
    Edited { edit: u32, tessera_id: u64 },
    /// The identity rule refused the row, which writes nothing.
    Refused(crate::resolve::Reason),
}

/// What an entry's rows touch that a later entry's rows must not touch in the same window: the
/// existing items its rows add to a view, edit or place in artifacts, and the unique values its
/// rows give items. Each is decided against state the close writes, so a later row touching one is
/// admitted only after the window holding it has closed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowClaims {
    pub entities: Vec<EntityId>,
    /// Unique values, as `(declared position, key widened)`.
    pub keys: Vec<(u16, u128)>,
}

impl WindowClaims {
    /// The claims of `rows` and of the existing items `named` that an entry's edits and joins in
    /// place touch, with `keys` the unique values the rows set.
    pub fn of(
        rows: &[UnallocatedRow],
        named: impl IntoIterator<Item = EntityId>,
        keys: Vec<(u16, u128)>,
    ) -> Self {
        WindowClaims {
            entities: rows
                .iter()
                .filter_map(|row| row.join)
                .chain(named)
                .collect(),
            keys,
        }
    }
}

/// One artifact a batch's rows join, resolved: the address the store gave its key, and which rows
/// named it.
///
/// **The ordinal is resolved once, at admission, and carried rather than re-derived.** That is the
/// rule the growth record already follows (`crate::membership::ArtifactStore::apply`) — what is
/// applied is what was decided — and between admission and the close nothing can move an existing
/// ordinal: a publication appends, and the two operations that remove an artifact (the fold, and a
/// deletion on the deny lane) both close the open window before they run. An ordinal whose record
/// has gone by then adds nothing, which is `ArtifactStore::grow`'s stated behaviour and the right
/// one: a growth may not resurrect an artifact a fold retired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMembership {
    pub layer: String,
    pub level: u32,
    /// As on [`crate::command::BatchMembership::view`].
    pub view: Option<String>,
    /// The key the caller's column carried, kept beside the ordinal because a key with no ordinal
    /// is the whole of what minting has to work from.
    pub key: String,
    /// The ordinal the key resolved to at admission — `None` where the layer's `value_set` is
    /// **open** and no live artifact held the key, which is the case the close mints
    /// (`artifacts-from-points.md` §6.3).
    ///
    /// **An open layer's unknown key resolves at the close and not here**, deliberately: an
    /// ordinal cannot be claimed at admission, because the record that would make the claim durable
    /// is not appended until the window closes and a publication executing in between would take
    /// the same one. So the key travels and the resolution is made once, on the executor, where
    /// nothing can interleave with it.
    pub ordinal: Option<u32>,
    /// Indices into this entry's `rows`, then into its `edits` counted on from the rows' end,
    /// then into its [`Slot::Joined`] items in slot order, counted on from the edits' end.
    pub rows: Vec<u32>,
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
    /// The resolved term set of each edit's first row, in the record's edit order.
    pub edit_terms: Vec<Vec<TermId>>,
    /// The assigned ids, in the entry's row order, then each edit's new entity in edit order, then
    /// each [`Slot::Joined`] item's entity in slot order: a membership names them by position.
    pub entity_ids: Vec<EntityId>,
    /// This entry's memberships, carried through the allocation unchanged: the ids the joins name
    /// are `entity_ids[row]`, which is why the two travel together.
    ///
    /// **Mutable after the close, at exactly one site**: the mint pass resolves the keys that had
    /// no ordinal at admission, so that what follows it sees one shape rather than two.
    pub memberships: Vec<ResolvedMembership>,
    /// The edges this entry's close has to settle, carried through the allocation unchanged.
    pub edges: Vec<crate::command::BatchEdge>,
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

    /// The record's edits, for the apply.
    pub fn edits(&self) -> &[WalEdit] {
        match &self.record {
            WalRecord::IngestBatch { edits, .. } => edits,
            _ => unreachable!("a ClosedEntry's record is always an IngestBatch"),
        }
    }

    /// The record's rows and edits, mutably, before the append: a novel category key is resolved
    /// to its code in place, and an edit is told whether its old entity is suppressed.
    pub fn rows_and_edits_mut(&mut self) -> (&mut [WalRow], &mut [WalEdit]) {
        match &mut self.record {
            WalRecord::IngestBatch { rows, edits, .. } => (rows, edits),
            _ => unreachable!("a ClosedEntry's record is always an IngestBatch"),
        }
    }

    /// The record's rows, mutably — for the executor to resolve a novel category key to its code
    /// **in place, before this entry's record is appended** (per-point-attributes §3.4). Minting
    /// has to land here, between allocation and the append loop: the row still carries the key
    /// [`crate::window`]'s caller admitted, and this is the last point it can be rewritten before
    /// the WAL frames it durably. Same `unreachable!` as [`Self::rows`], for the same reason.
    pub fn rows_mut(&mut self) -> &mut [WalRow] {
        match &mut self.record {
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

    /// One receipt per row of the request, in request order.
    pub fn receipt(&self) -> &[RowReceipt] {
        match &self.record {
            WalRecord::IngestBatch { receipt, .. } => receipt,
            _ => unreachable!("a ClosedEntry's record is always an IngestBatch"),
        }
    }
}

/// What one allocation run collected, in **entity space** — the operator figure behind
/// `/control/status`'s `fragmentation` (contracts §3.4).
///
/// # The entity-space quantity, not the row-space one
///
/// Contracts §3.4 warns about exactly one confusion, and it is worth repeating where the numbers
/// are produced: this is **posting run length in entity space** (the probes' results §2), **not**
/// the row-space mask run ratio of their §5. The two normalise the same way over different sets and
/// are not comparable. `mosaica_bench::metrics::run_ratio` is the row-space one — it takes a Roaring
/// bitmap of *row* ids and a *row* universe, and its callers pass masks and fragments. It is
/// deliberately not reused here; sharing one function between the two would be the fastest route to
/// quoting one as the other.
///
/// # What is measured, and where
///
/// Design §11.1 spends entity-ID ordering on posting compression, and [`CommitWindow::allocate`] is
/// where that ordering is decided. Immediately after [`assign_sorted`] returns, every row carries
/// its assigned id and its resolved term list, so posting runs are fully determined *before a
/// posting byte is written* — which matters, because nothing in the serving process writes postings
/// at all. **⊘ Specified, not implemented:** there is no flush, so there is no flush-time or
/// fold-time statistic to take instead.
/// The build pipeline does write postings, but its sort is global by construction and would show no
/// window effect whatever.
///
/// So these counters cover the ingest stream this process has allocated, and nothing else. A
/// bundle's own postings are not in them.
///
/// # `run_ratio` is within-window sort quality, and that is a real limit
///
/// For one term with `k` postings among a window's `W` rows, the expected number of runs of a
/// uniformly random `k`-subset of a `W`-universe is `k·(W − k + 1)/W`. Its mean run length is
/// `W/(W − k + 1)`, which is the probes' `1/(1 − p)` baseline up to the `+1` — and the `+1` is what
/// makes `k = W` give one run rather than a division by zero.
///
/// Because both the measurement and its baseline are taken at window scope, **the ratio reports how
/// much run length one allocation run collected relative to a random assignment of that same
/// window** — not how fragmented the stream is overall. Two consequences a reader must have:
///
/// * At `W = 1` (group commit disabled) the ratio is **identically 1.0** for every corpus. That is
///   a fact about the formula, not a measurement — and it is the right reading, since a window of
///   one row collects nothing.
/// * Fragmentation *between* allocation runs is invisible here by construction. That is the part
///   §11.1 records as permanent: compaction leaves the entity axis untouched and ids are stable
///   across rebuilds, so nothing repairs it.
///
/// This is therefore **not** a lower bound on stream-scope fragmentation and must not be reported as
/// one. What it does do is scale with the scope actually achieved — a contiguous term reports
/// ≈ `k(1 − p)` — so a deployment whose windows are tiny (clients trickling, or
/// [`CommitWindow::conflicts`] forcing early closes) reports ≈ 1.0 while one with fat
/// windows reports hundreds. The raw counters are published beside the ratios so that
/// `postings / runs` — mean run length with no window-local normalisation — is available to whoever
/// wants it.
///
/// # `postings_per_container` reduces to something smaller than its name
///
/// A container is a 2¹⁶ block of entity ids, and this module's header has the arithmetic: design
/// §11.1's container model gives `p·B < 2¹⁶` for every `p ≤ 1` once `B ≲ 6·10⁴`, and every window
/// size a heap budget permits is below that. So a window spans **one** container, or two when it
/// straddles a boundary, and `postings / containers` is in practice mean postings per term per
/// window. It is emitted because contracts §3.4 specifies it; it must not be read as the
/// container-count figure the union cost model is about, because a window collects none of that win.
///
/// # Aggregation
///
/// Summing over terms and over windows, `run_ratio = (Σpostings/Σruns) / (Σpostings/Σbaseline)` =
/// **`Σbaseline / Σruns`** — so no per-term state survives a window, and no weighting scheme needs
/// arguing separately. `baseline_runs_milli` is that sum in thousandths, because the per-term
/// expectation is fractional while the counters it folds into are integers; the reported ratio
/// carries two significant figures, against which a rounding of 0.0005 runs per window is nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FragmentationTally {
    /// Distinct `(row, term)` pairs. A term repeated within one row is one posting.
    pub postings: u64,
    /// Maximal ascending consecutive id sequences within one term's postings.
    pub runs: u64,
    /// Distinct 2¹⁶ blocks of entity id touched, per term.
    pub containers: u64,
    /// `Σ_t k_t·(W − k_t + 1)/W` over the terms in this window, in thousandths of a run.
    pub baseline_runs_milli: u64,
    /// Rows in the window — the universe the baseline is taken over.
    pub rows: u64,
}

impl FragmentationTally {
    /// Measure a delta postings tier as encoded: `postings` is `(term, sorted ascending entity
    /// list)` pairs — the shape a flush writes — and `rows` is the flushed item count.
    ///
    /// **This is the scope at which between-window scatter becomes visible.** The per-window tally
    /// below measures one allocation run against a random baseline of that same window, so
    /// fragmentation *between* windows is invisible to it by construction. A tier spans every
    /// commit window the buffer accumulated between two ticks, so its run and container counts
    /// carry exactly the erosion design §11.1 records as permanent — which is what the deferred
    /// index-ordinal split is supposed to trigger on, and what contracts §3.4's figures exist to
    /// make observable.
    ///
    /// The baseline is the same normalisation as [`tally`]'s (`Σ_t k_t·(W − k_t + 1)/W`, W = rows),
    /// so tier-scope and window-scope `run_ratio` values are comparable in construction even
    /// though they answer at different scopes. Entities are deduplicated per term before a tier is
    /// encoded, so `k_t ≤ W` holds by the same argument as the window's rows-not-occurrences rule.
    pub fn of_tier(postings: &[(TermId, Vec<u32>)], rows: u64) -> Self {
        let mut t = FragmentationTally {
            rows,
            ..Default::default()
        };
        if rows == 0 {
            return t;
        }
        let w = rows as f64;
        let mut baseline = 0.0f64;
        for (_, entities) in postings {
            if entities.is_empty() {
                continue;
            }
            t.postings += entities.len() as u64;
            t.runs += 1;
            t.containers += 1;
            for pair in entities.windows(2) {
                // Two INDEPENDENT predicates, exactly as [`tally`]: `65_535 → 65_536` continues a
                // run *and* opens a container.
                if pair[0] + 1 != pair[1] {
                    t.runs += 1;
                }
                if pair[0] >> 16 != pair[1] >> 16 {
                    t.containers += 1;
                }
            }
            let k = entities.len() as f64;
            debug_assert!(
                entities.len() as u64 <= rows,
                "a tier's postings are deduplicated per term, so k_t cannot exceed the row count"
            );
            baseline += k * (w - k + 1.0) / w;
        }
        t.baseline_runs_milli = (baseline * 1000.0).round() as u64;
        t
    }

    /// Fold another window's tally in. Saturating: a counter that has run out of `u64` stopped being
    /// a useful figure long before, and an operator gauge must not be the thing that panics the
    /// write executor.
    pub fn merge(&mut self, other: FragmentationTally) {
        self.postings = self.postings.saturating_add(other.postings);
        self.runs = self.runs.saturating_add(other.runs);
        self.containers = self.containers.saturating_add(other.containers);
        self.baseline_runs_milli = self
            .baseline_runs_milli
            .saturating_add(other.baseline_runs_milli);
        self.rows = self.rows.saturating_add(other.rows);
    }
}

/// Measure the assignment `pending` has just been given.
///
/// **The cost, stated because it sits on the executor's hot path.** One hash probe per
/// `(row, term)` pair — of order four or five per row — with no allocation per row. It is not free,
/// and it is small against the work already here: [`assign_sorted`] is `n log n` over the same rows
/// with a `Vec<u32>` key allocation per item and a lexicographic compare per comparison. There is
/// deliberately **no configuration gate**: a gate is a second thing to get wrong for a cost that is
/// a fraction of the sort beside it, and a figure that is off by default is a figure nobody has.
///
/// **The per-term state is bounded by the window, not by the corpus.** `last` is a local, keyed only
/// on terms present in this window, and dropped at every close. Its size is bounded by the term
/// occurrences the window is *already* holding — the `Vec<TermId>` per row — so it is a bounded
/// constant factor on memory already paid for, and it disappears with it. That is what keeps this
/// clear of the corpus-cardinality scaling that makes a per-term map over a large descriptor
/// vocabulary a multi-gigabyte structure.
fn tally(pending: &[PendingItem]) -> FragmentationTally {
    let rows = pending.len() as u64;
    if rows == 0 {
        return FragmentationTally::default();
    }
    // Ascending by id. A row that joins an existing entity keeps that entity's id, so a window's
    // ids are not one contiguous block and cannot be addressed as `id - lowest`.
    let id_of = |index: usize| {
        pending[index]
            .entity_id
            .expect("every pending item is assigned an id before the tally runs")
            .raw()
    };
    let mut by_rank: Vec<usize> = (0..pending.len()).collect();
    by_rank.sort_unstable_by_key(|&index| id_of(index));

    // `(last id seen, postings so far)` per term. The second half is `k_t`, and it counts **rows**,
    // never occurrences: a row's term list may hold one term twice, and a `k_t` above `W` would
    // make the baseline below negative.
    let mut last: FxHashMap<TermId, (u64, u64)> = FxHashMap::default();
    let mut t = FragmentationTally {
        rows,
        ..Default::default()
    };

    for &index in &by_rank {
        let item = &pending[index];
        let id = item.entity_id.expect("assigned above").raw();
        for term in &item.terms {
            match last.get_mut(term) {
                // Already counted for this row. Reached by a repeated term whether or not the
                // repetitions are adjacent in the row's term list.
                Some((seen, _)) if *seen == id => {}
                Some((seen, k)) => {
                    // Two INDEPENDENT predicates, deliberately not cascaded. `65_535 → 65_536`
                    // continues a run *and* opens a container, and testing contiguity first would
                    // credit neither — which is the case a long, well-compressed run hits.
                    if *seen + 1 != id {
                        t.runs += 1;
                    }
                    if *seen >> 16 != id >> 16 {
                        t.containers += 1;
                    }
                    t.postings += 1;
                    *k += 1;
                    *seen = id;
                }
                None => {
                    t.postings += 1;
                    t.runs += 1;
                    t.containers += 1;
                    last.insert(*term, (id, 1));
                }
            }
        }
    }

    let w = rows as f64;
    let mut baseline = 0.0f64;
    for (_, k) in last.values() {
        debug_assert!(
            *k <= rows,
            "a term's posting count counts rows, so it can never exceed the window's row count"
        );
        let k = *k as f64;
        baseline += k * (w - k + 1.0) / w;
    }
    t.baseline_runs_milli = (baseline * 1000.0).round() as u64;
    t
}

/// The open commit window.
pub struct CommitWindow<W> {
    entries: Vec<WindowEntry<W>>,
    /// **The join index.** A `batch_id` held here cannot be evaluated against the idempotency map,
    /// because that map is written at *apply*; [`CommitWindow::held`] is what answers for it, and
    /// [`CommitWindow::join`] is what a byte-identical retry does with the answer. Answering from
    /// inside the window is what makes a held batch id cost no window close at all.
    by_batch: FxHashMap<String, usize>,
    /// Everything this window's entries claim: see [`WindowClaims`].
    entities: FxHashSet<EntityId>,
    keys: FxHashSet<(u16, u128)>,
    rows: usize,
    /// When this window opened. Read by the executor to time the window's service, and by nothing
    /// else: **there is no age bound and no timer**. A window closes on its row bound or on the work
    /// queue being observed empty; the age bound the specification mentions is the safety cap on a
    /// linger, and there is no linger
    /// (docs/decisions/0034-the-window-does-not-linger.md).
    opened_at: Instant,
    /// The window's sequence number, which is what the executor's `BatchState::Held { window_seq,
    /// .. }` names. Nothing else reads it beyond diagnostics.
    seq: u64,
}

impl<W> CommitWindow<W> {
    pub fn new(seq: u64) -> Self {
        CommitWindow {
            entries: Vec::new(),
            by_batch: FxHashMap::default(),
            entities: FxHashSet::default(),
            keys: FxHashSet::default(),
            rows: 0,
            opened_at: Instant::now(),
            seq,
        }
    }

    /// Whether an entry claiming `claims` touches anything an entry of this window claims, so
    /// that it must wait for this window to close.
    ///
    /// A row is resolved against state the close writes: the buffer that holds a join, the live
    /// unique entries a new item's values become. Two entries touching one of them in one window
    /// would each be resolved without seeing the other.
    pub fn conflicts(&self, claims: &WindowClaims) -> bool {
        claims.entities.iter().any(|e| self.entities.contains(e))
            || claims.keys.iter().any(|k| self.keys.contains(k))
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

    /// **The `Held` half of the batch-id state machine**: is `batch_id` already an entry of
    /// this open window, and if so, what body hash was it admitted under?
    ///
    /// Returns `(window_seq, body_hash)`. The caller compares the hash: equal means a byte-identical
    /// retry, which [`CommitWindow::join`] adds to the held entry's waiters; unequal is a `409` for
    /// the retry alone (contracts §3.4, and see the engine's `BatchState` for the reading of "the
    /// batch has no effect" that is taken there).
    ///
    /// **This must be consulted before [`CommitWindow::conflicts`]**, never after. A retry claims
    /// what its original claims by construction, so a claims-first order would answer a retry by
    /// closing the window, even for the byte-identical case that has an exact answer available.
    ///
    /// `window_seq` is carried because the join's correctness is a statement about **which** window
    /// the entry sits in. That cannot be got wrong as the executor stands: there is exactly one open
    /// window, a local of its work pass, and it is consulted and joined in the same iteration — so
    /// the field is read only by a `debug_assert!` at the join site. It is not decoration and it is
    /// not a live guard either; it is the discriminator an executor holding more than one window
    /// would need, written down while the invariant it encodes is still obvious.
    pub fn held(&self, batch_id: &str) -> Option<(u64, [u8; 32])> {
        let index = *self.by_batch.get(batch_id)?;
        Some((self.seq, self.entries[index].body_hash))
    }

    /// **The join**: add `waiter` to the entry `batch_id` names, so a byte-identical retry is
    /// answered off the original's single allocation rather than allocating again.
    ///
    /// Nothing else about the entry changes: no rows are added, nothing is claimed, the row count
    /// does not move. A retry joined this way is answered off the original's one allocation.
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

    /// Admit an entry claiming `claims`. The caller has already established that it does not
    /// conflict.
    pub fn push(&mut self, entry: WindowEntry<W>, claims: WindowClaims) {
        let index = self.entries.len();
        self.by_batch.insert(entry.batch_id.clone(), index);
        self.entities.extend(claims.entities);
        self.keys.extend(claims.keys);
        self.rows += entry.rows.len() + entry.edits.iter().map(|e| e.rows.len()).sum::<usize>();
        self.entries.push(entry);
    }

    /// **Close the window: one signature-sorted allocation run over every row it holds.**
    ///
    /// Gather every entry's rows into one `Vec<PendingItem>` in `(entry, row)` order, hand it to the
    /// unchanged [`assign_sorted`], and scatter the ids back by position. One
    /// [`Allocator::allocate`] call for the whole window, so a window that cannot allocate has **no
    /// effect at all** — `allocate` leaves the allocator unchanged on its error path.
    ///
    /// **No per-row clone.** `terms` are *moved* out of each row into its `PendingItem` and moved
    /// back out afterwards, rather than copied ([`UnallocatedRow::take_pending`] and
    /// [`UnallocatedRow::into_wal_row_with`] are an exact inverse pair). Between the two a row is
    /// **hollow** — its `terms` empty — and nothing may observe it in
    /// that state: the interval is this function's gather-to-frame, the conflict check above runs at
    /// admission (before it), and the error path drops the entries rather than returning them.
    /// **The error hands the waiters back**, per entry and in entries order, rather than dropping
    /// them with the window: a dropped responder is a lost receipt, which the caller must read as
    /// "this may have been applied in full" (`SubmitError::ReceiptLost`) — the exact opposite of the
    /// truth here, where the high-water mark did not move and nothing was appended. The rows are not
    /// handed back: the batch has no effect, and they are hollow by then.
    ///
    /// **Also returns what the assignment collected** ([`FragmentationTally`]), measured here
    /// because this is the only site in the serving process that knows it — see that type for what
    /// the numbers mean and what they do not. The error path returns none: a window that could not
    /// allocate made no assignment to measure, which is the same statement as "no effect at all".
    ///
    /// `tessera_id_of` gives a created item's `tessera_id` for the receipt each record carries; a
    /// row naming an item carries its own.
    #[allow(clippy::type_complexity)]
    pub fn allocate(
        self,
        alloc: &mut Allocator,
        tessera_id_of: impl Fn(EntityId) -> u64,
    ) -> Result<(Vec<ClosedEntry<W>>, FragmentationTally), (AllocError, Vec<Vec<W>>)> {
        let mut entries = self.entries;

        // An edit's first row takes the new entity; its other rows take the same one after.
        let mut pending: Vec<PendingItem> = Vec::with_capacity(self.rows);
        for entry in &mut entries {
            for row in &mut entry.rows {
                pending.push(row.take_pending());
            }
            for edit in &mut entry.edits {
                pending.push(edit.rows[0].take_pending());
            }
        }

        if let Err(e) = assign_sorted(&mut pending, alloc) {
            return Err((e, entries.into_iter().map(|e| e.waiters).collect()));
        }

        // Between the assignment and the scatter is the one moment the whole window's ids and terms
        // are in hand together.
        let tally = tally(&pending);

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
            let mut edits = Vec::with_capacity(entry.edits.len());
            let mut edit_terms = Vec::with_capacity(entry.edits.len());
            for edit in entry.edits {
                let p = scattered
                    .next()
                    .expect("one PendingItem was gathered per edit, in this order");
                let entity = p.entity_id;
                let mut first = Some(p);
                let mut rows = Vec::with_capacity(edit.rows.len());
                for (at, row) in edit.rows.into_iter().enumerate() {
                    let pending = first.take().unwrap_or_else(|| PendingItem {
                        terms: Vec::new(),
                        entity_id: entity,
                    });
                    let (mut wal_row, row_terms) = row.into_wal_row_with(pending);
                    wal_row.join = at > 0;
                    if at == 0 {
                        edit_terms.push(row_terms);
                    }
                    rows.push(wal_row);
                }
                entity_ids.push(entity.expect("assign_sorted assigns every item it is given"));
                edits.push(WalEdit {
                    old: edit.old,
                    number: edit.number,
                    suppressed: false,
                    rows,
                });
            }
            entity_ids.extend(entry.slots.iter().filter_map(|slot| match slot {
                Slot::Joined { entity, .. } => Some(*entity),
                _ => None,
            }));
            let receipt = entry
                .slots
                .iter()
                .enumerate()
                .map(|(i, slot)| match *slot {
                    Slot::Written { row, tessera_id } => {
                        let row: &WalRow = &wal_rows[row as usize];
                        RowReceipt {
                            outcome: if row.join {
                                RowOutcome::Added
                            } else {
                                RowOutcome::Created
                            },
                            tessera_id: Some(
                                tessera_id.unwrap_or_else(|| tessera_id_of(row.entity_id)),
                            ),
                            over_bound: entry.over_bound.contains(&(i as u32)),
                        }
                    }
                    Slot::Unchanged { tessera_id, .. } | Slot::Joined { tessera_id, .. } => {
                        RowReceipt {
                            outcome: RowOutcome::Unchanged,
                            tessera_id: Some(tessera_id),
                            over_bound: false,
                        }
                    }
                    Slot::Edited { tessera_id, .. } => RowReceipt {
                        outcome: RowOutcome::Edited,
                        tessera_id: Some(tessera_id),
                        over_bound: entry.over_bound.contains(&(i as u32)),
                    },
                    Slot::Refused(reason) => RowReceipt {
                        outcome: RowOutcome::Refused(reason),
                        tessera_id: None,
                        over_bound: false,
                    },
                })
                .collect();
            closed.push(ClosedEntry {
                record: WalRecord::IngestBatch {
                    batch_id: entry.batch_id,
                    body_hash: entry.body_hash,
                    rows: wal_rows,
                    edits,
                    receipt,
                },
                terms,
                edit_terms,
                entity_ids,
                memberships: entry.memberships,
                edges: entry.edges,
                waiters: entry.waiters,
            });
        }
        Ok((closed, tally))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::WalScalar;

    /// Admit `entry` claiming what its rows claim, with no unique values.
    fn push<W>(w: &mut CommitWindow<W>, entry: WindowEntry<W>) {
        let claims = WindowClaims::of(
            &entry.rows,
            entry.edits.iter().map(|edit| edit.old),
            Vec::new(),
        );
        w.push(entry, claims);
    }

    fn claims(rows: &[UnallocatedRow]) -> WindowClaims {
        WindowClaims::of(rows, [], Vec::new())
    }

    fn row(terms: &[u32]) -> UnallocatedRow {
        UnallocatedRow {
            view: "default".to_string(),
            join: None,
            descriptors: vec![b"d".to_vec()],
            x: 1.0,
            y: 2.0,
            scalars: vec![WalScalar::U64(7)],
            scoped: Vec::new(),
            terms: terms.iter().map(|t| TermId::new(*t)).collect(),
        }
    }

    fn entry(batch: &str, rows: Vec<UnallocatedRow>) -> WindowEntry<&'static str> {
        WindowEntry {
            slots: (0..rows.len() as u32)
                .map(|row| Slot::Written {
                    row,
                    tessera_id: None,
                })
                .collect(),
            over_bound: Vec::new(),
            rows,
            edits: Vec::new(),
            batch_id: batch.to_string(),
            body_hash: [0u8; 32],
            memberships: Vec::new(),
            edges: Vec::new(),
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
            push(&mut split, entry(
                &format!("b{b}"),
                vec![
                    row(&[9]),
                    row(&[1]),
                ],
            ));
        }
        let mut alloc_split = Allocator::new(100);
        let (closed, _) = split.allocate(&mut alloc_split, |e| e.raw()).unwrap();
        let split_ids: Vec<u64> = closed
            .iter()
            .flat_map(|e| e.entity_ids.iter().map(|i| i.raw()))
            .collect();

        let mut whole = CommitWindow::new(0);
        let mut rows = Vec::new();
        for _ in 0..4u32 {
            rows.push(row(&[9]));
            rows.push(row(&[1]));
        }
        push(&mut whole, entry("one", rows));
        let mut alloc_whole = Allocator::new(100);
        let whole_ids: Vec<u64> = whole.allocate(&mut alloc_whole, |e| e.raw()).unwrap().0[0]
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
        push(&mut w, entry(
            "a",
            vec![row(&[5]), row(&[0])],
        ));
        push(&mut w, entry("b", vec![row(&[3])]));
        let (closed, _) = w.allocate(&mut Allocator::new(0), |e| e.raw()).unwrap();

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

        // The rows survived the hollow interval: geometry is back where it was.
        assert_eq!(closed[0].rows()[0].descriptors, vec![b"d".to_vec()]);
        assert_eq!(closed[0].rows()[1].x, 1.0);
        assert_eq!(closed[0].terms[1], vec![TermId::new(0)]);
    }

    /// An entry adding an item to a view, or setting a unique value, claims it: a later entry
    /// touching either conflicts, and one touching neither does not.
    #[test]
    fn an_item_a_row_adds_and_a_value_a_row_sets_are_claimed() {
        let joining = UnallocatedRow {
            join: Some(EntityId::new(7)),
            ..row(&[1])
        };
        let mut w: CommitWindow<&'static str> = CommitWindow::new(0);
        let first = entry("b1", vec![joining.clone(), row(&[1])]);
        let claims = WindowClaims::of(&first.rows, [], vec![(2, 99)]);
        w.push(first, claims);

        assert!(w.conflicts(&WindowClaims::of(&[joining], [], Vec::new())));
        assert!(w.conflicts(&WindowClaims::of(&[row(&[1])], [], vec![(2, 99)])));
        assert!(!w.conflicts(&WindowClaims::of(
            &[row(&[1])],
            [],
            vec![(2, 98), (3, 99)]
        )));
    }

    /// The `Held` lookup: the batch id, the window's own sequence number, and **the hash the
    /// entry was admitted under** — which is what decides join versus 409.
    #[test]
    fn a_held_batch_id_is_found_with_the_hash_it_was_admitted_under() {
        let mut w: CommitWindow<&'static str> = CommitWindow::new(7);
        push(
            &mut w,
            WindowEntry {
                slots: vec![Slot::Written {
                    row: 0,
                    tessera_id: None,
                }],
                over_bound: Vec::new(),
                rows: vec![row(&[1])],
                edits: Vec::new(),
                batch_id: "b1".to_string(),
                body_hash: [3u8; 32],
                memberships: Vec::new(),
                edges: Vec::new(),
                waiters: vec!["w"],
            },
        );

        assert_eq!(w.held("b1"), Some((7, [3u8; 32])));
        assert_eq!(w.held("b2"), None, "an unheld batch id is not held");
        // And it is found however the retry's rows differ: the hash decides, not the rows.
        assert_eq!(w.held("b1").map(|(_, h)| h), Some([3u8; 32]));
    }

    /// The join adds a waiter and **nothing else**: no rows, no claim, no row count. That is
    /// the whole reason it is safe where a second entry would not be — the unreachable duplicate
    /// needs two allocations, and a join performs none.
    #[test]
    fn joining_adds_a_waiter_and_changes_nothing_else() {
        let mut w = CommitWindow::new(1);
        push(&mut w, entry("b1", vec![row(&[1])]));
        let rows_before = w.rows();
        let entries_before = w.len();

        assert!(w.join("b1", "retry"), "a held batch id joins");
        assert!(!w.join("nope", "retry"), "an unheld one does not");

        assert_eq!(w.rows(), rows_before, "a join adds no rows");
        assert_eq!(w.len(), entries_before, "a join adds no entry");
        assert!(
            !w.conflicts(&claims(&[row(&[1])])),
            "a join claims nothing"
        );

        let (closed, _) = w.allocate(&mut Allocator::new(0), |e| e.raw()).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(
            closed[0].waiters,
            vec!["w", "retry"],
            "both callers are owed the same ids off the one allocation"
        );
        assert_eq!(closed[0].entity_ids.len(), 1, "one row, one id — not two");
    }

    /// **The tally, on a layout whose every counter is worked out by hand below.**
    ///
    /// The arithmetic is written out rather than recomputed, because a test that re-implements the
    /// loop it is testing agrees with any bug that loop has.
    ///
    /// Six rows in one window, allocated from id 0. `assign_sorted` orders on the sorted,
    /// deduplicated signature, ties in batch order:
    ///
    /// | signature | rows | ids |
    /// |---|---|---|
    /// | `[1]`     | `a`, `b` | 0, 1 |
    /// | `[1, 2]`  | `c`, `d` | 2, 3 |
    /// | `[2]`     | `e`, `f` | 4, 5 |
    ///
    /// So the postings are `term 1 → {0,1,2,3}` and `term 2 → {2,3,4,5}`, and by hand:
    ///
    /// * `postings` = 4 + 4 = **8**
    /// * `runs` = 1 + 1 = **2** (both lists are contiguous)
    /// * `containers` = 1 + 1 = **2** (`W = 6`, so everything is in block 0)
    /// * baseline, `k·(W − k + 1)/W` with `W = 6, k = 4`: `4·3/6 = 2` per term → **4.000**
    /// * `run_ratio` = 4.000 / 2 = **2.0**
    ///
    /// The corpus is deliberately **two** terms with `k < W`. A one-term-per-row corpus makes
    /// `k = W`, where the baseline is `W·1/W = 1` and a perfect run is 1, so the ratio pins at 1.0
    /// whatever the assignment does — an arrangement in which every assertion below holds vacuously.
    #[test]
    fn the_tally_matches_the_arithmetic_worked_out_by_hand() {
        let mut w: CommitWindow<&'static str> = CommitWindow::new(0);
        push(&mut w, entry(
            "b1",
            vec![
                row(&[2]),
                row(&[1, 2]),
                row(&[1]),
                row(&[2]),
                row(&[1, 2]),
                row(&[1]),
            ],
        ));
        let (_, t) = w.allocate(&mut Allocator::new(0), |e| e.raw()).unwrap();

        assert_eq!(t.rows, 6);
        assert_eq!(t.postings, 8, "four postings for each of the two terms");
        assert_eq!(t.runs, 2, "each term's ids are one contiguous block");
        assert_eq!(t.containers, 2, "six ids all sit in entity-id block 0");
        assert_eq!(t.baseline_runs_milli, 4_000, "4·3/6 = 2 runs per term");
        // And the ratio the endpoint publishes, from those two numbers alone.
        assert_eq!(t.baseline_runs_milli as f64 / 1000.0 / t.runs as f64, 2.0);
    }

    /// A row that joins an existing entity keeps that entity's id, so a window's ids need not be
    /// one contiguous block: two joins far apart, and a new row allocated above them.
    #[test]
    fn a_window_of_joins_is_tallied_in_id_order_whatever_the_gaps() {
        let joining = |entity: u64, terms: &[u32]| UnallocatedRow {
            join: Some(mosaica_types::EntityId::new(entity)),
            ..row(terms)
        };
        let mut w: CommitWindow<&'static str> = CommitWindow::new(0);
        push(&mut w, entry(
            "b1",
            vec![
                joining(70_000, &[1]),
                joining(3, &[1]),
                row(&[1]),
            ],
        ));
        let (closed, t) = w.allocate(&mut Allocator::new(100_000), |e| e.raw()).unwrap();

        let ids: Vec<u64> = closed[0].entity_ids.iter().map(|e| e.raw()).collect();
        assert_eq!(ids, [70_000, 3, 100_000], "a join keeps its entity; the new row is allocated");
        assert_eq!(t.rows, 3);
        assert_eq!(t.postings, 3);
        assert_eq!(t.runs, 3, "3, 70 000 and 100 000 are three runs");
        assert_eq!(t.containers, 2, "3 is in block 0; 70 000 and 100 000 share block 1");
    }

    /// A term repeated within one row is **one** posting, and it is found however far apart the
    /// repetitions sit in the row's term list.
    ///
    /// A row's term list is not deduplicated on every path that builds one, so it can arrive here
    /// as `[a, b, a]`. Were the repetition
    /// counted, a term's `k` could exceed the window's row count and its baseline
    /// `k·(W − k + 1)/W` would go negative — which, cast to the unsigned counter, saturates to zero
    /// silently rather than failing.
    #[test]
    fn a_term_repeated_within_one_row_is_one_posting() {
        let mut w: CommitWindow<&'static str> = CommitWindow::new(0);
        push(&mut w, entry(
            "b1",
            vec![row(&[7, 9, 7]), row(&[7, 9, 7])],
        ));
        let (_, t) = w.allocate(&mut Allocator::new(0), |e| e.raw()).unwrap();

        assert_eq!(t.rows, 2);
        assert_eq!(t.postings, 4, "two rows x two DISTINCT terms, not six");
        // k = 2 = W for both terms, so each baseline is 2·1/2 = 1.
        assert_eq!(t.baseline_runs_milli, 2_000);
    }

    /// A run that crosses a 2¹⁶ entity-id boundary continues the run **and** opens a container.
    ///
    /// The two predicates are independent, and cascading them — testing `last + 1 == id` first and
    /// treating a hit as "same container" — silently under-counts containers on exactly the long,
    /// well-compressed runs the figure exists to detect. Nothing aligns a window's first id to a
    /// container boundary, so this is an ordinary case rather than a contrived one.
    #[test]
    fn a_run_across_a_container_boundary_opens_a_container() {
        const LO: u64 = 65_534;
        let mut w: CommitWindow<&'static str> = CommitWindow::new(0);
        push(&mut w, entry(
            "b1",
            vec![
                row(&[1]),
                row(&[1]),
                row(&[1]),
                row(&[1]),
            ],
        ));
        let (closed, t) = w.allocate(&mut Allocator::new(LO), |e| e.raw()).unwrap();

        assert_eq!(
            closed[0]
                .entity_ids
                .iter()
                .map(|e| e.raw())
                .collect::<Vec<_>>(),
            vec![65_534, 65_535, 65_536, 65_537],
            "the fixture must actually straddle the boundary, or it tests nothing"
        );
        assert_eq!(t.postings, 4);
        assert_eq!(t.runs, 1, "the ids are consecutive, so it is one run");
        assert_eq!(t.containers, 2, "65_535 -> 65_536 crosses into block 1");
    }

    /// A window that cannot allocate has no effect: the high-water mark does not move (I9).
    #[test]
    fn a_window_that_exhausts_the_id_space_has_no_effect() {
        let mut w = CommitWindow::new(0);
        push(&mut w, entry("a", vec![row(&[1]), row(&[1])]));
        let mut alloc = Allocator::new(u32::MAX as u64 - 1);
        let err = w.allocate(&mut alloc, |e| e.raw());
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
