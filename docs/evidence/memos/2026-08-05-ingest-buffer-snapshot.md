# What ingest costs per row — the buffer clone is not the answer

**Status:** Evidence — analysis and design, never normative. Sweeps run by the owner 2026-08-05,
twice, reproducing: `crates/tessera-engine/tests/ingest_shape.rs`, release, WSL2 (the Phase 0
caveat applies — shapes, not milliseconds). Commissioned as a design for a chunked ingest buffer;
the premise was refuted mid-drafting and this is what replaced it. No code was changed and nothing
was re-measured here.

## Results

**1. The per-window buffer clone is not the dominant term, and the "24× gap = 25× B/W ratio"
agreement was a coincidence.** Two uncontrolled harnesses were being compared. Refuted; do not
carry the arithmetic forward.

**2. But the sweep did not refute the mechanism either — it lacked the resolution to see it.** The
clone's own cost has been measured three separate times at **160–330 ns per buffered item per
window close**, and at that constant the sweep's four cells differ by only 1.8–3.8 µs/row on a
~34 µs/row base: **5–11%, under the scatter**. A least-squares line through the four points has a
slope of 0.35 µs per predicted item-copy — the same order as those three measurements — but with
R² 0.28 over four points it cannot be distinguished from zero, so it corroborates nothing on its
own. The honest reading is a **bound**, not a refutation: at B ≤ 240,000 the clone is ≲11% of
per-row cost.

**3. The floor is not attributable by reading the code.** Enumerating every per-row step from
admission to ack, at generous cache-miss constants, accounts for **~7.9 µs/row of the measured
~26** (§2). The largest single identified term — 5.7 µs/row — is `assign_sorted`'s tie-break, and
it is **inflated by the harness**: every row carries one descriptor, so every comparison ties and
pays the expensive branch. **~70% of the floor is unattributed.**

**4. Nothing on the ingest path is O(corpus).** Every step is O(rows in this window),
O(buffered), or O(1) (§3). So the `scale.rs` (17.6 µs/row at a 250M base) versus `ingest_shape`
(26–39 µs/row at 1M) discrepancy is **not** a corpus effect; the leading candidate is row shape —
`scale.rs` rows carry two distinct signatures, `ingest_shape` rows one, which is exactly the input
that decides how often the tie-break above runs.

**5. Recommendation: instrument, do not optimise.** Three of the four measurements needed cost one
`eprintln!` each against counters that already exist (§6). Building anything against a 70%
unattributed cost is building against noise. **The chunked buffer is still worth doing** — for
`oldest_wal_pos`, for the measured deny-lane head-of-line stall, and for the flush plan's memory
transient — but on those merits, at depths this sweep did not reach, and **not as an ingest-
throughput fix** (§4).

---

## 1. What the two sweeps establish, and why the clone did not show

`Executor::apply_window` builds each generation's buffer with a deep clone of the previous one.
With `B` rows buffered between flushes and a close every `W` rows, the copies sum to `B²/2W` per
flush interval, so per-row cost ∝ `B/W`. That reading of the code is correct and unchanged.

What is new is the constant. Three independent measurements of it, none of them new work:

| source | depth | per buffered item, per close |
|---|---|---|
| `probes/2026-07-31-ingest-baseline/ingest_batch.jsonl`, cell `…/b10000`, `samples_in_order_ns` = 7.76 → 9.76 → 12.14 ms at depths 0 / 10k / 20k | 10⁴ | **200–240 ns** |
| deny-ack baseline memo — 165 ms p50 | 10⁶ | **165 ns** |
| `apply_nanos_max` 210–437 ms (write-path §12) | 1.34×10⁶ | **157–326 ns** |

The first row is a **new result from data already on disc**: `arms::ingest` emits
`samples_in_order_ns` precisely so that "a rising sample sequence within one cell is F3's
O(buffer) clone showing up directly", and the arm's own note reads only `min`, which is the
depth-0 sample. Read in order, the series is clean and monotone. **F3 is confirmed as a
mechanism** — at a magnitude that makes it a minor term at these depths, which is what the arm's
"NOT confirmed by measurement" was right to refuse to overclaim.

At 160–330 ns, the BUFFERED sweep's predicted spread across `B/W` = 1 → 24 (0 → 11.5 copies/row)
is **1.8–3.8 µs/row on a 33 µs base**. The observed non-monotonicity spans ~10%. The signal was
smaller than the noise.

**Two things would explain the discrepancy beyond effect size, and the second is a confound in the
sweep itself:**

- **The BUFFERED sweep varies `B` by varying flush frequency.** The `B/W` = 1 cell runs 24 flushes
  during the timed region's lifetime; the `B/W` = 24 cell runs one. Each flush publication puts a
  segment write, an O(buffered) rebase clone, a WAL rotation, a background projection refresh and
  possibly a merge or coalesce onto the pool, competing for CPU with the `accept_ingest` calls
  being timed. **That background work is anti-correlated with buffer depth**, so it cancels part
  of the very signal the sweep is looking for. A depth sweep that holds flush frequency fixed —
  pre-load the depth once, then time a fixed number of identical windows on top — does not have
  this problem.
- **The WINDOW sweep cannot separate the clone from per-call overhead at all.** Over its four
  cells, copies/row and call count are both ∝ 1/W: perfectly collinear. Its 1.49× is therefore
  consistent with either reading in isolation. What licenses the per-call reading is the BUFFERED
  sweep — which holds calls fixed at 24 and finds little depth effect — so the two sweeps together
  support the conclusion and neither does alone. Worth stating, because the conclusion is right
  and the single-sweep argument for it is not.

One more thing the WINDOW sweep shows: the 1.49× **saturates at ~120,000 rows per call**
(26.52 → 26.28). That is what an O(log W) sort with a working set outgrowing L3 looks like fighting
a 1/W amortisation — see §2.

## 2. The ~26 µs/row floor — what is on the path, and what it should cost

Per row, one call of 240,000 rows, one window, one fsync, a clone of an empty buffer. **Every cost
below is modelled** from the data structures, at ~80 ns for a DRAM access and ~50 ns for a
small-allocation malloc/free pair; nothing in this table is measured.

| step | per-row work | modelled |
|---|---|---|
| `LiveState::established_collisions` | one hash probe of the live external-id map | ~0.1 µs |
| window conflict set | one `FxHasher` digest of the id bytes, one set probe | <0.1 µs |
| `signature_sort_key` | one `Vec<u32>` allocation, sorted and deduped | ~0.1 µs |
| **`assign_sorted`'s sort** | log₂W ≈ 17.9 comparisons. **Every one ties here** (one descriptor for every row), so each falls through to `items[*a].external_id.cmp(…)`: two random reads of a 15 MB `PendingItem` array plus two of the external-id heap | **~5.7 µs** |
| WAL row framing | **moves, no clone** — `take_pending`/`into_wal_row_with` are an exact inverse pair, verified | ~0 |
| `postcard::to_allocvec` | ~10 serde fields; the output buffer doubles to ~20 MB | ~0.9 µs |
| `write_all` ×3, one `fsync` | per record, not per row | ~0 |
| **`apply_window`** | **three `external_id` clones** (`established`, `established_inverse`, the buffered item), one `slice` `String` clone, three hash inserts, one hash lookup for `set_wal_pos` | **~1.0 µs** |
| buffer clone | `(B/2W)` × 160–330 ns; `B = W` here, so zero | 0 |
| ack, receipt, `record_accepted_batch` | per call | ~0 |
| **identified** | | **~7.9 µs** |
| **measured** | | **~26 µs** |
| **unattributed** | | **~18 µs — 70%** |

Two readings follow, and they point in opposite directions:

**The largest identified term is workload-dependent, and the harness sits at its worst case.**
`assign_sorted` sorts by `(signature, external_id)`; the tie-break only runs when signatures are
equal. `ingest_shape`'s rows carry one descriptor, so *every* comparison ties; `scale.rs`'s carry
two distinct signatures per round, so roughly half do. **It is not merely an artefact, though** —
ties are what signature-sorted allocation exists to produce (§11.1: equal signatures land in a
contiguous id run, which is what makes their postings compress as runs, measured at 8.9–36.7×
under full-corpus sort). A real corpus ties far less than always and far more than never, so this
cost is real and its size is a property of the workload. Falsifiable for free: give `build_rows` a
descriptor drawn from, say, `i % 64` and re-run the WINDOW sweep. Predicted: several µs/row off
the large-`W` cells, near-nothing off the small ones. If the floor does not move, this line is
wrong and the residual is ~90% rather than 70%.

**Is something O(n)-ish hiding? Not O(corpus) — §3 rules that out by reading.** What is left is a
bad constant at this working-set size, and I can name only two candidates: the sort's random
access into a 15 MB `PendingItem` array (priced above, and the only one code reading can price),
and allocator and page-fault behaviour under ~40 MB of allocate-and-free churn per window, which
it cannot. The rest of the path is a handful of allocations, a handful of hash operations and one
sort — single-digit microseconds at any plausible constant. **So the honest answer at this point
is instrumentation, not analysis** — §6.

## 3. What is *not* on this path

Ruled out by reading, and worth recording because it is what a corpus-size hypothesis would need:

- `established_collisions` probes the **in-memory** external-id map only. No bundle extent lookup,
  no binary search over corpus-sized state.
- `Wal::append` is `postcard` plus three `write_all`s; O(bytes in this record), three syscalls per
  record.
- `Executor::publish_arc` re-derives the deny mask **under `debug_assert!` only** — free in
  release, which is how these sweeps were run.
- `apply_window` is O(buffered) + O(window). `publish_flush` and `apply_changes` are O(buffered).
  Nothing is O(corpus).

So ingest cost **should** be flat in corpus size, which is what `scale.rs` observes across 5M, 10M
and 250M. The remaining harness-to-harness gap is row shape, machine state, or a confound — not
the corpus.

**One thing found while reading, stated and not pursued:** `accepted_batches` is never pruned
in-process. It holds one entry per accepted batch, each carrying that batch's whole `Vec<EntityId>`,
for the executor's lifetime — 5,000 entries × 10,000 ids ≈ 400 MB over `scale.rs`'s 200 rounds. A
restart rebuilds it from the retained WAL, so it is bounded across restarts and unbounded within
one run. It is not in the memory review's table.

## 4. The chunked buffer, on its actual merits

**Proposed, not built.** An append-only chain of sealed, `Arc`-shared chunks would replace
`IngestBuffer`'s `FxHashMap`: a generation's buffer is a `Vec<Arc<Chunk>>`, and cloning it costs a
handful of refcount bumps rather than a deep copy of every row.

**What it buys, and the class of each claim:**

- **`oldest_wal_pos` becomes O(1) instead of an O(n) fold over every buffered item, on the
  rotation path.** Structural, not modelled: WAL positions ascend with window order, so the chain
  minimum is the oldest chunk's cached minimum. Today's fold runs once per rotation over up to
  10⁶ items.
- **It removes the *ingest window's* contribution to the deny lane's head-of-line stall, which is
  measured.** A deny waits behind at most one window close, and that close's dominant term is this
  clone: **165 ms p50 at 10⁶ buffered, 210–437 ms max at 1.34×10⁶** (deny-ack baseline;
  write-path §5.7 records it as a cost this design adds). A deny is a security operation with a
  bound in lifecycle §1.3. The worst case is unchanged — a deny that deletes a buffered row still
  pays O(buffered) under either removal rule in Q2 — so this is a p50 argument, not a p99 one. It
  is the strongest argument for the change, and it does not depend on anything in §1 or §2.
- **The flush plan's ≈1×B memory transient can go away.** `plan_flush` deep-clones every matching
  buffered item; with sealed chunks it can hold `Arc<Chunk>`s and filter at write time. Modelled;
  the memory review priced the transient at 0.25–0.35 GB at the 10⁶ bound.
- **Chunks are disjoint and ordered in entity space, by I9.** Entity ids are allocated
  monotonically per window, so chunk *k*'s range lies wholly above chunk *k−1*'s. `get` is
  therefore a binary search over chunk ranges and then one probe — **not** an O(#chunks) scan, and
  the question "what bounds #chunks" mostly dissolves. If each chunk is also sorted internally at
  seal, `plan_flush` gets its ascending order from concatenation and needs no sort at all. This is
  a better shape than the sketch in the brief and the reason is I9, not cleverness.

**What it does not buy, and these must not be claimed:**

- **No ingest-throughput win is supported by any measurement.** At B ≤ 240,000 the term it removes
  is ≲11% of per-row cost (§1), and 70% of that cost is unattributed (§2). Removing a term you
  cannot see is not a throughput fix.
- **No read-path win.** F2's buffer walk is a measured ~10 ns per item over a ~128-byte item —
  near single-thread memory bandwidth already. Chunked iteration is dense rather than a
  hash-table walk, so it should hold level or improve slightly; the only real lever on F2 is
  making `BufferedItem` **smaller**, which is independent of chunking (below).

**Why a chain rather than a persistent map.** An `im`-style HAMT would give the same O(1) clone
for a far smaller diff, and would keep `remove` a real removal, which dissolves Q2 outright. It
loses on two counts: it puts a third-party data structure in the write path's trusted computing
base against "design for audit before performance", and its iteration is a pointer-chasing tree
walk over the item `compose` walks per viewport — the one term here that is already measured and
already near its ceiling. If the owner weights diff size over both, it is the better choice and
this memo does not argue otherwise strongly.

**The independent, non-structural change, which is worth doing on its own merits.**
`BufferedItem` carries a `String` slice id per row when a bundle has one slice, and a
`Vec<TermId>` for a term list that is usually one or two entries. `slice: Arc<str>`,
`external_id: Option<Arc<[u8]>>` and `terms: SmallVec<[TermId; 4]>` take a clone of a typical item
from four heap allocations to zero, and shrink the item the read path walks. Modelled 2–3× on
every one of the five O(buffered) sites — the window clone, the flush plan, the flush publication,
a deny that deletes a buffered row, and `compose`'s per-viewport walk. It changes no exponent, has
no invariant surface, and is the only one of these changes that helps F2.

`wal_pos` can also stop being an `Option`: it is stamped immediately after insert with the
position already in hand on the live path, and the replay path can be given `replayed_positions`
so it stamps at construction too. That makes "unknown position" unrepresentable rather than
fail-safe-by-convention, and deletes `rotate_wal`'s `Some(None) => 0` arm.

## 5. What would falsify this, and what being wrong costs

- **If the instrumentation in §6 attributes most of the floor to the buffer clone after all** —
  i.e. the sweep's confound was hiding a much larger constant — then §1's bound is wrong and the
  chunked buffer *is* the throughput fix, on the original terms. Cost of being wrong this way:
  a delay of one measurement.
- **If the term-diversity experiment does not move the floor**, `assign_sorted`'s tie-break is not
  5.7 µs/row and §2's largest identified line is wrong, leaving ~90% unattributed. Cost: the
  enumeration in §2 is worth less than it looks and the instrumentation is needed sooner.
- **If the deny-lane stall is judged acceptable** — a deny already waits for a 3.2 ms fsync and a
  queue, and lifecycle §1.3's bound is a starvation bound, not a latency target — then the
  strongest remaining argument for the chunked buffer goes with it, and the change is left with
  `oldest_wal_pos` and the flush-plan transient. Those are real but small; on their own they would
  not justify the review surface.
- **The chunk design's own load-bearing assumption is that entity ids are disjoint and ascending
  across chunks.** It follows from I9 and from one window's ids being one contiguous allocation.
  If a future allocator change breaks it, `get` silently returns the wrong item. It must be a
  debug assertion at seal, not a comment.

## 6. The measurements to take, in order

Three of the four cost one `eprintln!` and no new instrumentation.

1. **Split the floor with counters that already exist.** `ingest_shape.rs` already calls
   `engine.write_executor_stats()`. Print the `apply_nanos_total` delta per cell divided by rows,
   alongside `wal_appends` and `wal_fsyncs`. `apply_nanos_total` measures exactly
   [buffer clone + the three external-id clones + the hash inserts + the swap]. This immediately
   splits ~26 µs/row into *apply* and *everything else*, and it settles §1 without re-running
   anything else. **Free.**
2. **Test the tie-break hypothesis.** Change `build_rows` to draw a descriptor from `i % 64` and
   re-run the WINDOW sweep. Predicted: several µs/row off the 120k and 240k cells, near-nothing
   off the 10k cell. **Free.**
3. **Remove the BUFFERED sweep's confound.** Vary depth without varying flush frequency: pre-load
   the buffer to a target depth, then time a fixed number of identical windows on top. This is the
   sweep that can actually measure the clone's constant end-to-end. **One helper.**
4. **The write-path stage timing that does not exist.** `arms::ingest` notes that `StageTimings`
   covers the viewport path only. Closing the residual needs three timers inside `close_window` —
   `with_allocator(window.allocate)`, the append loop, and `fsync` — plus one around
   `apply_window`'s clone alone, accumulated into counters behind the existing `bench-timing`
   feature so nothing ships. **This is the only thing that can attribute the remaining ~18 µs/row,
   and I do not think it can be attributed without it.**

## 7. Owner questions

**Q1 — Is the deny lane's head-of-line stall worth a structural change on its own?**
What the corpus says now: write-path §5.7 and §4.4 record an O(buffered) clone ahead of the deny
lane as a cost this design accepts; the deny-ack baseline measures it at 165 ms p50 and up to
437 ms at 10⁶ buffered. What the code does: every ingest window close clones the whole buffer, and
a deny waits behind at most one such close. Options: **(a)** accept it — lifecycle §1.3's bound is
starvation, not latency, and 165 ms is small beside a bulk-revocation request; **(b)** build the
chunked buffer, which takes the p50 to a handful of refcount bumps and leaves the p99 unchanged
(a delete of a buffered row still folds O(B)). Consequence of (a): the figure grows with whatever
`ingest_buffer_max_items` is set to. Consequence of (b): the review surface in Q2.

**Q2 — How may a buffered row be removed, once chunks are sealed?**
`buffer.remove` is called at **four** sites, not three: flush publication and deny-apply at
runtime, and `WritePath::reconstruct` and `overlay::drop_deleted` at startup. **None is in the hot
loop** — the brief's premise holds, and two of the four are startup-only. Both runtime sites
already pay a full O(buffered) clone today. Two options:

- **(F) Removal by fold.** A removal rebuilds the chain as one chunk minus the removed ids. This
  is **cost-neutral against today** at both runtime sites, because both already clone the whole
  buffer; it resets the chunk count at every flush; it keeps `oldest_wal_pos` exact; and it
  introduces **no new mechanism that hides a row**.
- **(T) A tombstone set.** Cheaper on a deny that deletes a buffered row, and it introduces a
  second structure whose presence hides a row.

**The precise interaction, because this is where the fail-open history lives.** A tombstone does
**not** conflate Rules S and F. No suppression ever removes a buffered row — `apply_changes`
collects only `ChangeOp::Delete`, and `drop_deleted` filters on `overlay.is_deleted` — so Rule S
never touches the buffer at all, and a tombstone fed only from the existing sites cannot give a
suppression a retirement route. Nor does it retire a deletion: the overlay entry stands, and
`compose::verdict` answers `Some(false)` from `is_deleted` before it consults the buffer, which is
the same argument `drop_deleted` already makes for the physical removal.

What it *does* introduce is **a third never-retire rule, of a different kind**, in a system whose
recorded failure mode is confusing the two it has. A tombstone dropped while its row is still in a
chunk re-admits a flushed row to the buffer; the next flush then writes a second geometry for that
entity, and rule 4 answers for an entity the fragment already covers — the invariant
`WritePath::reconstruct` establishes and `compose::verdict` relies on since the watermark gate was
deleted. Separately, a tombstoned row inside the oldest chunk still holds that chunk's cached WAL
minimum down, so a deleted-before-flush row would pin its WAL member — the exact bug
`drop_deleted` exists to prevent, returning by a different route.

So the trade is **review surface, not correctness**: (T) is safe if its monotonicity is never
weakened, and (F) removes the question at no measured cost. This memo's §4 is drafted against (F);
if (T) is ruled, the design acquires a third retirement rule that write-path §5.4 must state
beside S and F.

**Q3 — What ingest rate must the write path sustain?** At the shipped defaults
(`ingest_buffer_max_items` 10⁶, `flush_max_age_secs` 90 s) a deployment can absorb ~11,100 rows/s
before the buffer bound sheds ingest, and the measured path already does several times that. Bulk
load goes through the build pipeline (2.17M rows/s), not through `/control/ingest`. If sustained
ingest is not a target, §2's floor is a curiosity and only Q1 justifies work here; if it is, the
target number decides whether the answer is this path at all or a bulk-ingest route beside it.

**Q4 — Is the `BufferedItem` shrink and the `wal_pos` narrowing (§4, last two paragraphs) worth
doing without waiting for any of this?** It touches no invariant, changes no exponent, helps the
one measured read-path term (F2), and is a modelled 2–3× on five O(buffered) sites. Recommended
yes; it costs a small diff and a re-run of the write-path tests if wrong.

## What this memo corrects

- The brief's "24× gap = 25× `B/W` ratio" agreement: **coincidence**, two uncontrolled harnesses.
  Do not carry it.
- `arms::ingest`'s F3 note: still right that F3 was not confirmed *at the depths it reached*, but
  the arm's own `samples_in_order_ns` at `b10000` does confirm the mechanism at 200–240 ns/item
  when read in rep order. The note reads `min`, which is the depth-0 sample.
- `probes/2026-08-05-write-path-at-scale/README.md` and the scale memo §1 both say `ack` is
  "fsync-dominated". At 250,000 rows per round in 25 sub-batches, 25 fsyncs at the measured
  ~3.2 ms are **~80 ms of a 1.8–4.5 s round — 2–4%**. `ack` is dominated by per-row work, which is
  the whole subject of §2.
- The brief's "three `buffer.remove` call sites": there are **four**, and two are startup-only.
  The substantive claim — none in the hot loop — holds.
