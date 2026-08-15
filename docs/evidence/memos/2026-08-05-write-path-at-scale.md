# The write path at scale — flush, merge and coalesce over 10M rows

**Status:** Evidence — measurement, never normative. Campaign:
[`probes/2026-08-05-write-path-at-scale/`](../../../probes/2026-08-05-write-path-at-scale/); the
harness is `crates/tessera-engine/tests/scale.rs`, so every figure re-runs from the tree rather
than from a probe binary that has drifted from the engine. WSL2, 12 cores, 39 GB, NVMe — **not a
latency-certification environment**; read the shapes, not the milliseconds.

Commissioned on the owner's question: run several rounds of ingest over millions of rows into a
meaningful build and prove the data becomes correctly queryable — then, how long until an ingested
row is visible, how the bundle grows, and what a viewport costs at a range of zooms.

## Results

**Correctness holds at ten million.** 2,000,000-item base plus 32 rounds of 250,000 = 10,000,000,
across 9 merges and 4 coalesces and a restart: every masked total exact after every round, every
external id resolving both ways, every planted probe's Morton code byte-identical, and a sparse
principal's masked total equal to independently computed ground truth. The seven properties and
what each would catch are in the harness's module doc.

**Four figures worth carrying forward:**

1. **Visibility is ~99% tick.** The two mechanical terms are ~0.5 s and tens of milliseconds; the
   wait is `ingest.flush_max_age_secs`, default **90 s**.
2. **On-disc bytes are ~1.3–1.6× the live working set, and only grow.** Nothing reclaimed a
   merge's consumed segments in this campaign, there being no compaction fold when it ran.
   Provision for the on-disc number. **The fold has since been built** and reclamation is its
   §6; these figures are the unreclaimed shape and are not a measurement of what a deployment
   running folds holds.
3. **The per-(tile × segment) cost model holds** — arch §11.3's prediction, confirmed: at z8 it
   *falls* from 6.9 µs to 1.4 µs as the 5M run's corpus grows 4×, and sits at 1.6 µs at 10M.
   Constant per pair; the request cost is the product.
4. **One full projection build across the whole run**, at both sizes — decision 0044's D1, which
   `soak.rs` asserts at 64 items and this confirms at 10⁷.

## 1. How long until an ingested row is visible

Three intervals, because "visible" has three different answers depending on who is asking.

| interval | state at the end of it | 250k rows, 5M corpus | 250k rows, 10M corpus |
|---|---|---|---|
| `ack` | **durable and invisible** — WAL append + fsync + buffer swap | 1.9 → 4.6 s | 1.8 → 4.5 s |
| `publish` | flushed; visible to a session authorised **after** this point | **0.44 → 0.52 s** | **0.42 → 0.58 s** |
| `refresh` | live sessions' row projections brought forward; visible to sessions that **already existed** | **36 → 61 ms** | **5 → 142 ms** |

`ack` is a property of the caller's batch size rather than of the corpus. **It is not
fsync-dominated** — an earlier revision of this memo said it was, and the arithmetic refutes it: 25
sub-batches at the measured ~3.2 ms fsync floor is ~80 ms of a 1.8–5.0 s round, **2–4%**.
`crates/tessera-engine/tests/ingest_shape.rs` splits the term; ~56% of it is `apply_window` and the
remainder is unattributed. `publish` and `refresh` are the write path's own costs.

**The dominant term is none of the three.** `publish` above is a *forced* flush
(`Engine::request_flush`). In a deployment the wait is the tick: `ingest.flush_max_age_secs`,
default **90 s**, which `tessera-server`'s config calls "the bound on how stale an acknowledged
item's absence may be" and argues for at its own site — a shorter tick synchronises projection
rebuild across the whole session population, and a rebuild is a measured 10.7 s at 10⁹.

So the end-to-end answer is **up to the tick + ~0.5 s flush + tens of ms refresh**, and at the
shipped default the tick is ~99% of it. The other two terms are what a shorter tick would expose.

**The `refresh` term is a real visibility boundary, not bookkeeping.** Decision 0044's D1 keeps a
flush off a live session's request thread, so an established session serves from its existing
projection until the refresh replaces it. Measured directly while building the harness: immediately
after a flush an established session read **1,000,000** where 1,250,000 had been ingested, while a
session authorised after the flush read **1,250,000**. Both are correct; they are answering at
different generations. Any client-facing statement about ingest latency has to name which it means.

### Where ingest time goes, attributed

`WriteStage` (added 2026-08-05, the write-path counterpart to `timing.rs`) partitions a commit
window's close. Measured at `bench-timing`, µs per ingested row; `submit→receipt` is the caller's
whole wait and *overlaps* the rest rather than partitioning beside it.

| stage | 1M base, 1 descriptor/row | 250M base, 3 descriptors/row |
|---|---|---|
| **`buffer_clone`** | 2.60 (**37%**) | **5.99 (42%)** |
| `record_batch` | 0.59 | 1.80 |
| `apply_rows` | 1.24 | 1.33 |
| `allocate` | 0.30 | 0.96 |
| `admit` | 0.77 | 0.87 |
| `wal_fsync` | 0.38 | 0.49 |
| `wal_append` | 0.04 | 0.38 |
| `swap` | 0.00 | 0.00 |
| unattributed | ~1.2 | 2.42 |
| **`submit→receipt`** | **7.20** | **14.24** |

**The buffer clone (F3) is the dominant ingest cost at both scales**, and it worsens with term
density: `admit` and `apply_rows` are flat between the two columns, while `buffer_clone`,
`allocate` and `wal_append` roughly triple — and those three are exactly what copies or compares a
row's `Vec<TermId>`. Both columns buffer ~250,000 rows between flushes at a 10,000-row window, so
copies per row is 11.5 in both; what moved is the per-copy cost, 291 → ~520 ns. **The 2× is term
density, not corpus size.**

Two smaller results worth carrying: `record_batch`'s per-batch `Vec<EntityId>` clone into
`accepted_batches`, **which is never pruned in-process**; and `allocate`'s tripling is
`assign_sorted` comparing longer signatures — the `n log n` term `config.rs` warns about at
`DEFAULT_COMMIT_WINDOW_MAX_ITEMS`, surfacing as a term-density effect rather than a window-size one.

> **Superseded 2026-08-05 for `record_batch`.** Its 1.80 µs/row here was allocator pressure from
> its neighbour, not its own work — the dedicated run below confirmed the coupling, and measures
> that clone at **~1.8 ns per row**, linear in batch rows, three orders of magnitude smaller.
> `accepted_batches` going unpruned remains a memory argument and is not a throughput one.
> [`2026-08-05-ingest-rate.md`](2026-08-05-ingest-rate.md) §5.

### The fix, and why the chunked buffer is shelved

A `BufferedItem` behind an `Arc` — so the per-close map clone copies a pointer
instead of the item's four heap allocations. Measured, µs per ingested row:

| | 1M base, 1 desc | 250M base, 3 desc |
|---|---|---|
| `buffer_clone` | 2.60 → **0.24** | 5.99 → **0.22** |
| `submit→receipt` | 7.15 → **2.11** | 14.24 → **2.63** |
| `ack` per 250,000-row round | | ~4.5 s → **731 ms** |

**It transfers to scale, and the hasher fix did not.** 3.4× at 1M and 5.4× at
250M, against 5.5× and nothing. The difference is what each targets: the clone's
operand is the row's term vector, so its cost rises with term density — and term
density is what separates a fixture from a deployment.

**An allocator-pressure coupling, and it is the more useful finding.** Four
stages the `Arc` change does not touch fell with it: `record_batch` 1.80 → ~0,
`admit` 0.87 → 0.20, `wal_append` 0.38 → 0.09, `allocate` 0.96 → 0.35. The deep
copy was millions of allocations per round, and it was taxing every other
allocation in the executor. **Consequence for reading the pre-Arc attribution:
those stages were never as expensive as they measured** — they were being slowed
by their neighbour.

> **Confirmed 2026-08-05** by an A/B differing in that one file, swept over `B/W`:
> at `B/W = 1`, where the clone runs over an empty buffer, the two builds are
> indistinguishable in every stage (0.99× overall); at `B/W = 24` they differ 674×
> in `record_batch` and 4.5× in `admit`. Three of the four reproduce and their real
> costs are the post column. **`wal_append` does not** — it reads level or
> marginally slower after the change, so that item is withdrawn.
> [`2026-08-05-ingest-rate.md`](2026-08-05-ingest-rate.md) §5.

**The chunked buffer is therefore shelved, not queued.** `buffer_clone` is now
**8.4% of ingest at 250M**, so the whole restructure — a public type in
`tessera-lifecycle`, four `remove` call sites, `plan_flush`, `compose`, and a
ruling on `2026-08-05-ingest-buffer-snapshot.md`'s Q2 — has a ceiling of single
digits. Three things would reopen it, and none is pressing: **B/W growing** (the
`B²/2W` term is linear in the flush interval, and 250,000 buffered is modest for
a 90 s tick under load); the **`plan_flush` transient** becoming the binding
memory constraint (≈1×B, which `Arc<Chunk>` retires — a memory argument, not a
throughput one); and **`oldest_wal_pos`**, an unmeasured O(n) fold on the
rotation path that chunking makes O(1).

### The external-id hasher, and the limits of the 5.5× it bought

`WritePath::established` was an `FxHashMap<Vec<u8>, EntityId>`. FxHash is tuned for integer-like
keys; on structured byte strings it clusters. The discriminating measurement was
`established_inverse`, which clones the *same* `Vec<u8>` into an `EntityId`-keyed map: **15.38
µs/row against 0.27, on key type alone.** Switching to SipHash took a 1M-base microbenchmark from
33.50 to 6.12 µs/row.

**That win does not transfer to scale, and the claim must be scoped.** At a 250M base, `ack` per
250,000-row round is unchanged across the fix — 2.0/4.6/4.5/9.6/4.3 s before against
1.8/4.1/4.5/6.4/4.6 s after, within noise. The synthetic fixture's external ids (`g-0`, `g-1`, …)
are FxHash's pathological case, and at realistic term density the buffer clone swamps the
difference. **Quote the 5.5× only with its base size and descriptor count attached.**

The change stands on an argument that does not depend on scale: external ids are **caller-supplied**,
and a weak, non-keyed hash over attacker-chosen keys is a flooding vector against the single
executor thread whose latency the deny lane's bound depends on (lifecycle §1.3). SipHash is randomly
keyed per process.

## 2. Bundle size, and the orphans nothing reclaims

Two numbers, and the gap between them is the finding.

| | 5M run | 250M + 50M run |
|---|---|---|
| live bytes (both manifests) | 47.7 → 185.3 MiB | ~9.4 GiB base + 1.84 GiB written |
| on-disc bytes | 47.7 → 294.8 MiB | 14.85 GiB |
| ratio at the end | **1.59×** | **1.32×** |

> **Corrected 2026-08-05.** An earlier revision of this section reported 2.01× and 2.63×, and
> the 250M run reported ratios up to 1065×. Those were a **metric bug, not a measurement**:
> `live_bytes` summed only the partition's `SEGMENTS-<n>.json` `files` map and omitted the
> bundle-level `MANIFEST.json`, so the entire base build was counted on disc and not in the live
> set. `plan_coalesce` keeps the two maps apart deliberately — it takes `build_files` separately so
> it can refuse to consume a build artefact — which is what made the omission easy. The figures
> above are from `run-5m-corrected.txt`; the 10M and 250M `live` columns in `runs.txt` and
> `run-250m.txt` predate the fix and should not be quoted.

**Live bytes grow linearly at ~9.3 MiB per 250,000 rows — ~39 B/row** — with no super-linear term
over 40× of growth. That is the number a capacity model wants.

The ratio rises as merges accumulate consumed inputs and settles near 1.6× at 5M; the 250M run sits
lower at 1.32× because its 9.4 GiB base is never merged and so never orphaned.

**On-disc bytes are what a disc actually holds, and they only ever grow.** A merge's consumed
segments and a coalesce's consumed tiers stay on disc: every side-manifest below the current `n`
still names them, and a step-down serves one of those (contracts §2.3). Reclaiming them belongs to
compaction, and **no compaction fold existed when this ran** — it has since been built, so the
growth below is the unreclaimed shape rather than a steady state. The per-round disc growth is bimodal and
says so plainly: **+9.0–9.4 MiB on an ordinary round, +46.6 MiB on a round a merge lands in** — the
merge writes its output and orphans its inputs, so it costs disc rather than reclaiming it.

The early ratios are an artefact of a small live set beside a fixed build segment; the steady-state
figure is the 1.32–1.59× band above, and it does not trend down. **A deployment provisions for the
on-disc number.** Nothing in the corpus previously recorded this ratio.

## 3. Read latency by zoom

Whole-extent viewport at `k = 500`, sampled after every flush. The raw per-request figure grows for
two reasons at once — more occupied tiles *and* more live segments — so the harness also reports
**µs per (tile × segment)**, which is the quantity arch §11.3 predicts should be constant: a tile
resolves to one contiguous row range per live segment, so a request pays one binary search and one
`range_cardinality` per pair.

5M run, first round against last (1,250,000 → 5,000,000 visible; 2 → 8 segments):

| zoom | tiles | round 0 | round 15 | µs/(tile × segment) |
|---|---|---|---|---|
| 0 | 1 | 780 µs | 2.8 ms | 390 → 353 |
| 2 | 16 | 2.1 ms | 5.3 ms | 65 → 41 |
| 4 | 256 | 12.0 ms | 27.1 ms | 23 → 13 |
| 6 | 1,682 → 3,734 | 21.7 ms | 58.4 ms | 6.5 → **2.0** |
| 8 | 1,982 → 13,064 | 27.3 ms | 146.1 ms | 6.9 → **1.4** |

**The model holds, and better than flat: the per-pair cost falls as the corpus grows.** At 10M
items z8 reaches 243.9 ms over 17,202 tiles and 9 segments — **1.6 µs per pair**, against 3.3 µs at
that run's first round. The request cost is the product of its two factors, and neither factor is
doing anything surprising.

Zoom 0's large µs/(tile × segment) is not a per-pair cost — it is one tile counting the entire
corpus, so the figure is really "the cost of a whole-corpus masked count" (0.78 → 2.8 ms) and the
normalisation is meaningless there. It is reported for completeness, not comparison.

**z8 whole-extent is a stress case, not a client request.** A viewer asks for the tiles on their
screen — the fixed-viewport arm's ~300 — not every occupied tile at depth 8. The sweep exists to
watch the product stay linear while both factors move, which is the claim that would fail silently.

## 4. The two maintenance passes, on their own cadences

| | 5M / 16 rounds | 10M / 32 rounds | 250M + 50M / 200 rounds |
|---|---|---|---|
| merges | 4 | 9 | **62** |
| coalesces | 2 | 4 | **28** |
| segments | 2 → 8, settling at 5 | 2 → 9, settling at 6 | 2 → **18**, settling at **15** |
| delta tiers | 1 → 8 → 2 | 1 → 8 → 2, four times | 1 → 8 → 2, repeatedly; ends at 4 |

### The segment axis is not bounded, and this is the run that shows it

At 5M and 10M the merge appears to bound segments. **It does not.** `MergePolicy::select`'s rule 3
refuses any window whose *total* exceeds `max_merged_segment_bytes` (256 MiB shipped,
`tier_width` 4). A 250,000-row extent is ~9.3 MiB, so:

| tier | segment | 4 × total | selectable |
|---|---|---|---|
| extent | ~9.3 MiB | 37 MiB | yes |
| 1 | ~37 MiB | 149 MiB | yes |
| 2 | ~149 MiB | **595 MiB** | **no — over cap** |

So merging **saturates at ~149 MiB**: a tier-2 segment can never be merged again, and the count
grows linearly thereafter at roughly **one segment per 4M rows ingested**. Measured: 2 → 18 live
segments over 200 flushes while merges fired throughout (62 of them), settling at 15 once ingest
stopped. The coalesce, by contrast, bounds its three axes repeatedly and indefinitely.

At a 90 s tick and 250k rows per flush that is **+1 live segment every ~24 minutes**, ~60/day. A
viewport pays one binary search and one `range_cardinality` per segment per tile, and §3 measures
that per-pair cost as flat — so request cost tracks segment count linearly and without bound.

**Design §16's "how many live segments before per-tile fan-out is noticeable" is still open**, and
now has a rate attached rather than only a question. The cap is raisable — write-path §7 requires
only that it sit strictly below the base segment's size, and a 250M base leaves ~9.4 GiB of room —
but merge peak memory is a measured 4.4–4.9× the inputs' file bytes, so the knob buys segment count
with pool transient. Ruling needed; nothing here changes a default.

The coalesce fires roughly every seven rounds once the merge is consuming segments — at rounds 8,
15, 22 and 29 in the 32-round run — and each firing takes the tier axis from 8 back to 2. **Both
axes are bounded, repeatedly, rather than bounded once.**

`soak.rs` established this shape at 64 items and 40 flushes. What this adds is that it survives
segments of a quarter-million rows, an entity space spanning many Roaring containers, and merges
whose row-space permutation is non-trivial.

## Caveats

- **WSL2**, per the Phase 0 memo's standing note: fine for mmap and page-cache fidelity, not
  trusted for latency certification. Shapes, not absolutes.
- **The fixture's base occupies exactly 1,000 distinct positions, whatever `n` is.** `common`'s
  corpus places item `e` at `(e·37 mod 1000, e·53 mod 1000)` — **both axes are functions of
  `e mod 1000`**, so the position set does not grow with the corpus at all: 1,000 positions at 1M
  items and 1,000 at 250M, carrying 1,000 and 250,000 co-located items respectively. The ingested
  rows spread further (`position_of` shifts by round, ~1,000 fresh positions per round), which is
  where the occupied-tile counts in §3 come from.

  This is a severe departure from any real distribution, and it cuts two ways. It is a *harder*
  test of the Morton ordering, which must sort a quarter-million exact ties per position, and of
  the count path, which is exercised over maximally dense ranges. It makes the **latency** figures
  in §3 unrepresentative: real geometry spreads over far more tiles with far fewer rows in each,
  which changes both factors of the product. Re-measure against the Phase 0 corpus
  (`data/scaled/`) before quoting §3 as deployment figures; §1, §2 and §4 do not depend on the
  spatial distribution.

  It also broke the harness once, which is worth recording because the failure mode was silent
  until 250M: a probe row sharing a base position sits in a tile of 250,001 rows, §7.2's cap serves
  the first `k`, and the probe is simply not in the answer. The counts were all exact; only the
  by-position lookup failed. Probes are now placed half a unit off the integer grid and read back
  at zoom 16, which isolates them at any corpus size.
- **In-process.** The harness drives `tessera-engine` directly. Nothing here goes through the HTTP
  server or the differential oracle, so this is not conformance evidence.
- **No crash coverage.** Nothing here kills the process mid-flush or mid-merge; the pause sites
  that would allow it (`PauseSite`) exist only on the deny/ingest command path.
