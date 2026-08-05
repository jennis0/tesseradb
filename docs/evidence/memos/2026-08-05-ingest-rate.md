# The ingest rate, and the three properties that set it

**Status:** Evidence — measurement, never normative. Campaign:
[`probes/2026-08-05-ingest-rate/`](../../../probes/2026-08-05-ingest-rate/); the harness is
`tessera-bench ingest-rate`, so every figure re-runs from the tree. WSL2, 12 cores, 47 GB, NVMe —
**not a latency-certification environment**; read the shapes, not the milliseconds.

Commissioned because the corpus had no ingest rate in it. `arms::ingest`'s `batch` mode carried
"~1.37M items/s at batch=10,000", which was `min` over a rising series at a term density and a
buffer depth where the costs that dominate a deployment are invisible. It is withdrawn.

## Results

**There is no single ingest rate, and the spread across plausible deployment shapes is 4.8×.** A
bulk loader — one caller, maximal batches — against a 2,422,486-item base sustains **250,000–465,000
rows/s** of submission, and **150,000–270,000 rows/s** once the flush that makes those rows visible
is counted. A single caller submitting small batches into a deep buffer sustains **97,000**. Which
end you get is decided by three properties nobody was sweeping and not at all by the one everybody
was.

1. **`B/W` has an optimum, and it is not "as deep as possible".** Including the flush, buffering
   four commit windows between publications is best at every density. Flushing every window is
   **30–42% worse** — the flush's fixed cost spread over too few rows. Buffering twenty-four is
   20–36% worse, which clears the resolution bar at three and eight descriptors per row and not at
   one.
2. **Term density adds up to 43% to per-row cost, and only when the buffer is deep.** At
   `B/W = 24`, eight descriptors per row cost 43% more than one; at `B/W = 1` they cost 6% more,
   which is below this campaign's noise floor. Where that cost lands has also changed — see §2.
3. **Concurrency is worth 3× — but only to the caller the commit window exists for.** Eight
   concurrent callers submitting 1,000-row batches go from 97,000 to 286,000 rows/s. Eight
   submitting *maximal* 10,000-row batches gain nothing measurable, because
   `ingest.commit_window_max_items` equals `ingest.ingest_max_batch_rows` by design and one maximal
   batch fills a window — an exact counter, not a timing, says so. Raising that ceiling makes the
   mechanism work but does not buy resolvable throughput; **concurrency is not a multiplier on the
   bulk-loader figure**, which is what it was suspected of being.
4. **fsync is 11–24% of a serial caller's per-row cost, against `apply_window`'s 38–50%.** The
   "ingest throughput is an fsync amortisation story" reading is withdrawn: it described a batch of
   one, and the 2% figure that replaced it was a share of a total 15× larger than today's.

Two things the campaign was asked to verify, and did:

5. **`record_batch`'s lap is correctly placed and its true cost is ~1.8 ns per row.** The 1.80
   µs/row it read before the `Arc<BufferedItem>` change was three orders of magnitude above its own
   work.
6. **The allocator-pressure coupling is confirmed**, by an A/B whose only difference is that one
   file. **Consequence: `record_batch`, `admit` and `allocate` were never as expensive as the
   pre-`Arc` attribution measured them** — see §5, and re-derive anything built on those figures.
   One item of that hypothesis, `wal_append` 0.38 → 0.09, does **not** reproduce.

**Noise floor, measured rather than assumed.** Every cell is `min` over five complete steady
regions; the median max/min spread *within* a cell is 52%. What matters is the spread *between*
runs, and four separate invocations happen to contain the identical cell (`d=3`, `B/W=12`, one
caller, at the defaults): **3.15, 3.09, 2.66 and 2.47 µs/row**, a **28% span**, with the two fastest
being the two run alone rather than inside a 48-cell campaign. So the resolution here is ~30%, and
nothing below that is claimed. Comparisons between cells measured back to back inside one campaign
are better than that — they share whatever the campaign-length drift is — but by an unquantified
amount, so every claim in this memo is held to the 30% bar and the ones that do not clear it say
so.

## 1. The table

2,422,486-item `categories-subclass` base, one caller, 10,000 rows per `accept_ingest` call, a
10,000-row commit-window ceiling — the server's own defaults. `B/W` is rows buffered between
flushes over rows per commit-window close; at these settings it is also the number of submissions
per flush cycle. µs per ingested row, and rows/s:

| `B/W` | 1 descriptor/row | 3 | 8 |
|---|---|---|---|
| 1 | 2.53 (395k) | 2.63 (381k) | 2.68 (374k) |
| 4 | 2.30 (434k) | **2.15 (465k)** | 2.41 (416k) |
| 12 | 2.27 (440k) | 3.15 (318k) | 3.10 (323k) |
| 24 | 2.79 (359k) | 3.50 (286k) | **4.00 (250k)** |

That is submission alone — what a `/control/ingest` caller waits on. A deployment also waits for the
flush, whose cost is fixed per publication and therefore falls per row as `B` grows, exactly against
the clone. Same cells, rows/s including the publication:

| `B/W` | 1 descriptor/row | 3 | 8 |
|---|---|---|---|
| 1 | 177k | 156k | 151k |
| 4 | **259k** | **270k** | **217k** |
| 12 | 246k | 208k | 182k |
| 24 | 206k | 174k | 154k |

**The curve has an interior optimum, and `B/W = 4` is at or next to it at every density** — the two
costs trade either side: at `B/W = 1` the flush costs 3.1 µs/row against 1.1 at `B/W = 4`, and by
`B/W = 24` the clone and the sort have taken back more than the flush gave up. `B/W = 4` and `12`
are not separable at one descriptor per row (259k against 246k); everything else in the column is.

**This is a knob a deployment does not currently have.** Flush cadence is `flush_max_age_secs`, a
*time*, so `B` is the arrival rate times the tick and is not set directly. Arithmetic from the
measured rate: a loader sustaining ~380,000 rows/s across the shipped 90 s tick buffers ~34,000,000
rows between publications — `B/W` of 3,400, more than a hundred times the right-hand end of this
table. What the table says about that regime is that per-row cost keeps climbing; what it cannot say
is how far, because the campaign stops at 240,000 and the `B²/2W` law has no ceiling in it.

## 2. Where the time goes

`WriteStage` laps, differenced across the measured region, at three descriptors per row and one
caller. µs per ingested row:

| stage | `B/W` 1 | 4 | 12 | 24 |
|---|---|---|---|---|
| `apply_rows` | 1.17 | 1.03 | 1.28 | 1.41 |
| `allocate` | 0.44 | 0.46 | 0.68 | 0.78 |
| `wal_fsync` | 0.40 | 0.39 | 0.51 | 0.40 |
| **`buffer_clone`** | 0.001 | 0.023 | 0.109 | **0.325** |
| `admit` | 0.25 | 0.21 | 0.24 | 0.23 |
| `wal_append` | 0.10 | 0.12 | 0.19 | 0.21 |
| `record_batch` | 0.001 | 0.001 | 0.003 | 0.002 |
| **total** | **2.63** | **2.15** | **3.15** | **3.50** |

**`buffer_clone` is proportional to `B/W`, as the `B²/2W` law predicts** — 0.001 → 0.023 → 0.109 →
0.325 against a ratio of 1 → 4 → 12 → 24, i.e. a constant ~13 ns per unit of `B/W`. At `B/W = 1` the
buffer is empty at every close and the clone is exactly zero; that is what makes this axis worth
sweeping and what a shallow-buffer benchmark cannot see.

**But the clone is no longer the term to chase.** At its worst here it is 9% of the total, against
37–42% before `BufferedItem` went behind an `Arc`. Across every serial cell of campaign A,
`apply_rows` is **38–50%**, `allocate` **12–27%**, `wal_fsync` **11–24%** and `buffer_clone`
**0–9%**.

**`allocate` rises with `B/W` too, and it should not.** The window it sorts is 10,000 rows whatever
the buffer depth: 0.44 → 0.78 is a 77% rise in a stage whose input never changed. The residual
allocator pressure of §5 is the available explanation and it is a **hypothesis, not a
measurement** — the A/B there shows the effect exists and is proportional to buffer churn, and does
not isolate it inside `allocate`.

### Term density lands in `allocate` and `wal_append`, not in the clone

| `B/W` | density | total | `apply_rows` | `allocate` | `wal_append` | `buffer_clone` |
|---|---|---|---|---|---|---|
| 1 | 1 | 2.53 | 1.08 | 0.30 | 0.06 | 0.000 |
| 1 | 3 | 2.63 | 1.17 | 0.44 | 0.10 | 0.001 |
| 1 | 8 | 2.68 | 1.21 | 0.50 | 0.16 | 0.000 |
| 24 | 1 | 2.79 | 1.26 | 0.60 | 0.14 | 0.225 |
| 24 | 3 | 3.50 | 1.41 | 0.78 | 0.21 | 0.325 |
| 24 | 8 | 4.00 | 1.53 | 1.02 | 0.33 | 0.247 |

**This corrects the mechanism the write-path memo gives for density.** That memo attributes
density's cost to "`buffer_clone`, `allocate` and `wal_append` — exactly what copies or compares a
row's `Vec<TermId>`". Two of the three still do: `allocate` is `assign_sorted` comparing longer
signatures (0.30 → 1.02, 3.4×) and `wal_append` is the raw descriptor bytes reaching the record
(0.06 → 0.33, 5.5×). The clone no longer does, because behind an `Arc` its operand is a pointer:
across the density axis at `B/W = 24` it reads 0.225 / 0.325 / 0.247, which is flat within this
campaign's noise.

## 3. Concurrency: worth 3× to a small-batch caller and nothing to a bulk loader

`accept_ingest` blocks on its receipt, so a serial caller leaves the executor's work queue empty at
every close and a window holds exactly one entry. Under concurrent callers the window can gather.
Whether it does depends entirely on the caller's batch size, and the two cases answer differently.

### Maximal batches: no gain, by design

`ingest.commit_window_max_items` and `ingest.ingest_max_batch_rows` both default to 10,000, so one
maximal submission fills a window and closes it on its row bound before a second can be admitted.
`entries/window` — `wal_appends / wal_closes`, an exact counter — is pinned at exactly **1.00 in all
forty-eight cells of campaign A**, and throughput is flat to falling across the concurrency axis
(three descriptors per row):

| `B/W` | `commit_window_max_items` | c=1 | c=2 | c=4 | c=8 | entries/window at c=8 |
|---|---|---|---|---|---|---|
| 4 | 10,000 (shipped) | 465k | 452k | 419k | 394k | **1.00** |
| 4 | 240,000 | 335k | 356k | 406k | 392k | 4.00 |
| 12 | 10,000 (shipped) | 318k | 371k | 368k | 360k | **1.00** |
| 12 | 240,000 | 355k | 373k | 443k | 412k | 6.00 |
| 24 | 10,000 (shipped) | 286k | 323k | 325k | 323k | **1.00** |
| 24 | 240,000 | 312k | 365k | 344k | 357k | 8.00 |

**This is the documented intent, not a defect**, and it is worth saying because a 1.00 in a
group-commit counter looks like a bug. `config.rs` argues the equality at its own site: one maximal
batch is one maximal window, so no client can define the window's size by picking a chunk size, and
"at the defaults a maximal batch commits alone and gains nothing from grouping — it is the *small*
batches the window collects". What was missing was the price of that, and it is now measured:
raising the ceiling to 240,000 makes the mechanism work exactly — `entries/window` tracks the
submitter count 1 → 2 → 4 → 6 → 8, `buffer_clone` falls 0.371 → 0.020 µs/row (18×, because
gathering raises `W` and the law is keyed on `B/W`) and `wal_fsync` falls 0.87 → 0.16 — and buys
throughput of **+14% to +25%, which does not clear this campaign's 30% resolution bar**. The
mechanism is not in doubt (those two stage figures are 18× and 5×, and `entries/window` is an exact
counter); the *throughput* gain is not resolved, and the reason it is small is visible in the
stages: the two costs group commit amortises were already under 1.3 µs/row together, and
`assign_sorted` grows `n log n` against them (0.73 → 0.94 µs/row at `B/W = 12`). **Read this as "not
a multiplier", not as "+20%."**

### Small batches: 2.2–3.0×, and it is the largest single effect measured

The case the window exists for. 1,000 rows per call, the shipped 10,000-row ceiling, three
descriptors per row, at the same two buffer depths campaign A reaches:

| rows buffered between flushes | c=1 | c=2 | c=4 | c=8 | gain | entries/window at c=8 |
|---|---|---|---|---|---|---|
| 40,000 | 161k | 213k | 254k | **358k** | **2.2×** | 1.90 |
| 240,000 | 97k | 156k | 223k | **286k** | **3.0×** | 2.23 |

**Eight concurrent small-batch callers recover most of the gap to a bulk loader** — 358k against the
465k a single caller reaches with maximal batches — and `fsync` per row falls 3.51 → 1.77 while
`buffer_clone` falls 2.57 → 0.91 at the deeper buffer, which is the amortisation doing exactly what
it is for.

**`entries/window` saturates near 2, not near 8**, and that is decision 0034's no-linger rule
showing: a window also closes the moment the work queue is observed empty, and the executor drains
faster than eight callers can refill it. `w_observed` reaches 1,899 and 2,235 rows against a 10,000
ceiling — so the ceiling is not what bounds gathering here, arrival rate is. Raising
`commit_window_max_items` would not help this case at all.

**No ruling is asked for.** The default equality has an argument at its own site and this campaign
supports it: the configuration it disadvantages is a bulk loader submitting maximal batches, which
is already the fastest shape measured, and the configuration it serves — many small concurrent
writers — gets its 3× without any knob being moved.

## 4. The two axes that turned out not to matter

**Base corpus size.** One shape (`d=3`, `B/W=12`, one caller) at 250,000 and at 2,422,486 items:
**3.71 and 3.09 µs/row**. A 10× base change moves per-row cost by 20% — the run-to-run bar — with
the *smaller* base the slower of the two, so there is no base effect this campaign can resolve and
no monotone one to extrapolate. This is a weaker control than the write-path memo's
1M-against-250M comparison and does not replace it; it justifies the arm fixing the base and
spending its cells elsewhere.

**Batch size.** `d=3`, `B/W=12`, one caller, with the window ceiling tracking the batch:

| rows per call | µs/row | `wal_fsync` | `allocate` |
|---|---|---|---|
| 1,000 | 5.95 | 4.83 | 0.51 |
| 10,000 | **2.66** | 0.47 | 0.69 |
| 40,000 | 3.14 | 0.24 | 0.95 |

The knee is the same one `crates/tessera-engine/tests/ingest_shape.rs` describes — fsync
amortisation trading against `assign_sorted`'s `n log n`, visible here as `wal_fsync` falling 20×
while `allocate` nearly doubles — and 1,000 rows per call is unambiguously the wrong side of it.
**Between 10,000 and 40,000 this campaign cannot choose**: 2.66 against 3.14 is 18%, inside the
run-to-run bar. That test's "the best of those is ~40,000" was measured before the
`Arc<BufferedItem>` change lowered everything the sort competes against, and is no longer supported;
what survives is the shape, and 10,000 — the shipped `ingest_max_batch_rows` — is not on the wrong
side of it.

## 5. The allocator-pressure coupling, confirmed

The write-path memo observed that four stages the `Arc<BufferedItem>` change does not touch fell
with it, and asked for a dedicated run before anything was built on it. This is that run: the same
binary, the same fixture, back to back, differing in exactly one file
(`crates/tessera-lifecycle/src/buffer.rs`, at `78759b1^` against `78759b1`). Three descriptors per
row, one caller. µs per ingested row:

| stage | `B/W` 1 pre → post | `B/W` 12 pre → post | `B/W` 24 pre → post |
|---|---|---|---|
| **total** | 2.46 → 2.49 (**0.99×**) | 6.69 → 2.42 (2.8×) | 12.43 → 2.91 (4.3×) |
| `buffer_clone` | 0.001 → 0.000 | 3.006 → 0.102 (30×) | 6.236 → 0.271 (23×) |
| `record_batch` | 0.001 → 0.001 (1.00×) | 0.804 → 0.002 (349×) | 1.759 → 0.003 (674×) |
| `admit` | 0.168 → 0.188 (0.89×) | 0.711 → 0.211 (3.4×) | 0.942 → 0.211 (4.5×) |
| `allocate` | 0.319 → 0.347 (0.92×) | 0.582 → 0.661 (0.88×) | 1.159 → 0.656 (1.8×) |
| `wal_append` | 0.081 → 0.083 (0.98×) | 0.105 → 0.184 (0.57×) | 0.155 → 0.173 (0.90×) |
| `apply_rows` | 1.092 → 1.109 (0.98×) | 1.170 → 1.140 (1.03×) | 1.198 → 1.173 (1.02×) |
| `wal_fsync` | 0.367 → 0.365 (1.01×) | 0.351 → 0.363 (0.97×) | 0.443 → 0.353 (1.26×) |

**`B/W` is the discriminator, and it is what makes this causal rather than correlational.** At
`B/W = 1` the clone runs over an empty buffer, the deep-copy churn does not exist, and the two
builds are *indistinguishable in every stage* — 0.99× overall, and no stage outside 0.89–1.01×. The
same two builds at `B/W = 24`, where the churn is maximal, differ by 674× in `record_batch` and
4.5× in `admit`. A change confined to one struct's ownership cannot make an unrelated `Vec` clone
674 times faster; millions of freed allocations per second stopping is the mechanism that can.

**What this costs the earlier attribution.** The write-path memo's 250M column buffers ~250,000 rows
at a 10,000-row window — `B/W ≈ 25`, this table's right-hand column — and three of its four figures
reproduce almost exactly at a 2.42M base: `record_batch` 1.80 (here 1.759), `admit` 0.87 (0.942),
`allocate` 0.96 (1.159). Those were not those stages' costs. Their costs are the post column:
**`record_batch` 0.003, `admit` 0.211, `allocate` 0.656.**

`allocate` is the one to read carefully: it is 1.8× at `B/W = 24` and *level* at `B/W = 1` and 12,
so only its deepest-buffer figure was inflated. That is consistent with the mechanism — the churn
grows with `B/W` — and it means `assign_sorted`'s own tripling with term density (§2) stands.

Concretely, `record_batch` clones one `Vec<EntityId>` per accepted batch into the idempotency index —
eight bytes per row, once. Campaign D measures it at 1.8 / 14.6 / 70.8 µs per batch at 1,000 /
10,000 / 40,000 rows: **linear in batch rows at ~1.8 ns/row**, which is a memcpy and is what the lap
should read. The lap placement is correct (it brackets the `record_accepted_batch` loop in
`close_window`) and 0.00 µs/row is the true reading. The memo's derived claim — that the un-pruned
`accepted_batches` index costs 1.80 µs/row — should be struck; the index is still never pruned
in-process, which remains a memory argument.

**Negative result: `wal_append` 0.38 → 0.09 does not reproduce.** Here it reads 0.081 → 0.083 at
`B/W = 1` and 0.155 → 0.173 at `B/W = 24` — post-`Arc` is level or marginally *slower*, at three
descriptors per row and a 2.42M base. The write-path memo's figure is at a 250M base; whatever moved
it, this campaign does not reproduce it as an allocator-pressure effect, and it should not be
carried as one.

## Caveats

- **WSL2**, per the Phase 0 memo's standing note. Shapes, not absolutes. Every cell is `min` over
  five steady regions; the median within-cell spread is 52% and the worst is 366% (a
  `concurrency_capped` cell at `B/W = 1`).
- **One base, one label set.** 2,422,486 items of `categories-subclass`, whose dictionary is 176
  terms — so the descriptor pool is 176 and every row's signature is drawn from it. A corpus with a
  large vocabulary would make `assign_sorted`'s comparisons resolve earlier and would move
  `allocate`; that is unmeasured.
- **Term density is descriptors per row, uniform across rows.** A real corpus has a skewed
  distribution (54,791 signatures over 2.42M items, top 500 covering 82.4% — probes results §3),
  and no figure here forecasts what that does to the window sort.
- **Everything is `min` over five regions and held to a 30% resolution bar** (see Results). Where a
  difference does not clear it, the memo says so rather than reporting the number bare.
- **`B` is pinned by flushing and waiting**, which a deployment does not do: its loader keeps
  submitting through the publication. The consequence is that the submission and flush columns are
  reported separately rather than as one number, and a deployment's actual figure sits between them
  — closer to the submission column the more the flush overlaps.
- **In-process.** Nothing here goes through the HTTP server, so `/control/ingest`'s own admission
  check, JSON parse and `spawn_blocking` hop are not counted.
- **No principal is involved.** The arm draws descriptors straight from the bundle dictionary
  rather than from a grant, because it never reads. Nothing here exercises masking, and no figure
  here says anything about the read path.
