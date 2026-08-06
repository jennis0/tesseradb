# Compaction review — memory, performance and user-experience lens

**Status:** Evidence — review transcript, never normative. One of the independent reviews of
`docs/design/compaction.md` (r2, provisional). Commissioned on the owner's priorities: simplicity,
correctness, **memory (the fold must not OOM a box)**, and user experience across the fold and at
the flip. This memo attacks the last two. Nothing here is a ruling; findings are for the owner to
disposition.

Figures are marked **measured**, **modelled** or **assumed** at each use, and the ones that matter
most here come from one file the draft does not cite: the real 10⁹ bundle's manifest
(`probes/2026-07-30-1e9-rebuild/manifest-snapshot/MANIFEST.json`) and the build that produced it
(`probes/2026-07-31-1e9-rebuild/build.log`).

## The measured 10⁹ bundle, which is what the fold rewrites

Every file, from the manifest snapshot — **measured**, and the operand for most of what follows:

| file | bytes |
|---|---|
| `columns.arrow` | 18.500 GB |
| `permutation.bin` | 4.000 GB |
| `ext-locator.u32` | 4.000 GB |
| `morton.u32` | 4.000 GB |
| `external-ids-{0..9}.arrow` | 16.250 GB (10 files) |
| `postings.arrow` | 0.145 GB |
| `pairs.parquet` | 0.121 GB |
| `terms-0.dict` | < 0.001 GB (47,968 terms) |
| **total** | **47.017 GB, 17 files** |

And its build: **10 m 25 s wall, 233% CPU, 27.9 GB peak RSS** (measured, same log). That is the
closest thing the corpus has to a fold, and three things follow immediately: the build is
**CPU-bound, not IO-bound**; the external-id family is **43% of the bundle**, not a footnote; and
the streaming build is *not* an O(1)-memory precedent — it is a budget-planned O(n) one.

---

## Findings, ranked

### 1. "Peak RSS is O(1) in corpus size" (spec §3) is false on the metric the corpus measures

The probe the draft cites for the multiplier it refuses to inherit
(`probes/2026-08-04-maintenance-memory/`) measures `VmHWM` from `/proc/self/status`, and says why:
*"the kernel's own resident-set high-water, which is what a cgroup limit is compared against"*.
Three of the fold's terms scale with the corpus on exactly that metric.

**(a) The two dense entity-indexed mappings.** Spec §3 pass 1: `permutation.bin` *"is written
through a mapping, so it is page cache rather than RSS"*. On Linux, resident pages of a
file-backed mapping are counted in `VmRSS`/`VmHWM` (`RssFile`), and cgroup v2 charges page cache to
`memory.current`. Writing 4 GB through a shared mapping produces 4 GB of dirty page cache that is
resident, charged, and not reclaimable until writeback completes. Pass 3's `ext-locator.u32` is
another 4 GB (both **measured** file sizes above). So **8 GB at 10⁹, on both the metric the corpus
uses and the metric a container enforces.**

The claim that survives is weaker and worth stating instead: *the fold does not OOM, because these
bytes are file-backed and reclaimable under pressure, and because the serving process already maps
`permutation.bin` in full at read (`Permutation::load`) — the fold's marginal cost over steady
state is the dirtiness, not the residency.* That is defensible. "O(1)" is not.

There is a second-order cost the draft does not name: the pass-1 scatter `perm[entity] = row` is
**random** access across the whole 4 GB, so effectively all 10⁶ pages are dirty simultaneously.
Under `vm.dirty_ratio` (20% of RAM by default) that is fine on the 39 GB probe box and is a
writeback stall on a 8–16 GB container.

**(b) `PostingsSpool::offsets` is O(dict.len()), not O(1).** `PostingsSpool` holds
`offsets: Vec<i64>` — one `i64` per record — resident until `finish`. Pass 2 sweeps
`0 .. dict.len()`, so that is **0.94 GB at the FST probe's 1.17×10⁸ terms** and **1.6 GB at the
plugin ABI's declared `max_distinct_terms` of 2×10⁸** (modelled from the code; the term counts are
measured/declared). The 10⁹ fixture's 47,968 terms hides this completely, which is why P1 as
drafted would not catch it.

This one is cheap to fix and the fix is *smaller* than the thing it replaces: spool the offsets to
a second file and map it as the `OffsetBuffer`'s `ScalarBuffer`, exactly as the values half already
does through `Buffer::from_custom_allocation`. Roughly ten lines, and it makes the O(1) claim true
on this axis.

**(c) The largest term's encode, at ~3 live copies.** See finding 12 for the figure.

**What would change:** state spec §3's rule as a *budget* with named terms and a pre-flight
refusal, on `tessera-build`'s precedent (`pipeline.rs` refuses a build whose plan does not fit the
memory budget, and refuses one whose three phase peaks do not fit the disc). The fold has a
free-space precondition and no memory precondition at all. And P1 must sample `VmHWM`, not heap.

### 2. The pre-swap warm cannot reach 2× the projection cache's budget — the cache prevents it, by evicting the entries still serving

Spec §6: *"At most one new entry per resident entry, so peak residency is 2× the row-projection
cache's own byte budget."* `RowProjectionCache` is a `SingleFlightCache` with a hard byte bound
enforced by an LRU eviction pass **on every publish**. Inserting one new entry per resident entry
does not take residency to 2× the bound; it evicts down to the bound. Two consequences, both
against the draft's intent:

- **The pre-warm's coverage is capped at about half the resident population.** Once the new entries
  fill the bound, each further insert evicts an old one. At the default 2 GiB bound and the
  **measured** 125.12 MB per wide-grant entry at 10⁹ that is ~16 entries total, so ~8 warmed.
- **What it evicts is live.** The evicted entries are old-generation projections that requests are
  *still being served from*, because the fold has not swapped yet. Those sessions fall to rung 3 —
  a full inline build — **during the fold**, which is the opposite of "requests are served from the
  old geometry, correctly, because it is still live". This is the same shape the write-path memory
  review already recorded (finding 6, 2026-08-04: the 0044 refresh's 2×-per-key residency makes LRU
  evict not-yet-refreshed sources).

Getting the draft's stated 2× requires the operator to *double the bound* for the fold's duration —
an unstated memory cost (2 GiB at the default, more at a tuned one) landing on top of finding 1's.
The fragment cache has its own separate budget (`fragment_cache_bytes`, default 1 GiB) and the
pre-warm builds into a second `FragmentCache` under the new identity, so the same doubling applies
there and spec §6's bound omits it entirely.

**What would change:** either name the temporary bound raise and price it, or accept partial
coverage and pair it with finding 3's mechanism.

### 3. Pre-swap warming removes the shedding mechanism, converting a bounded 429 window into an unbounded latency-and-memory herd

Rung 3 of `Engine::session_geometry` sheds with `ProjectionBuilding` (429) **only** when
`refresh_in_flight` is set, and only `RefreshDeps::spawn` sets it. With no post-swap refresh,
nothing arms it. So after the flip every session the pre-warm did not cover — evicted (finding 2),
arrived during the fold, or simply not resident — takes a **full inline build**: a **measured**
4 550 ms for the primitive at 10⁹, 10.7 s end to end. They take it concurrently, up to the 48-way
admission width, at ~125 MB each — the ~6 GB second term `cache.rs` already states in its sizing
note. Rung 2 is unavailable for the reason spec §6 gives.

The draft's *"after the swap the cache is already warm, so there is no window to shed in"* is
therefore true only for the sessions the pre-warm reached, and the fallback for everyone else is
strictly worse than the 429 D2 was avoiding: a 429 with `Retry-After: 1` costs a client one second
and the server nothing; an inline build costs the server seconds of pool time and 125 MB per
concurrent session.

**What would change:** arm `refresh_in_flight` across the swap regardless, and keep a post-swap
pass for the residue. D2's shape survives — the pre-warm still collapses the window for the
sessions it covers — but the shed must not be deleted along with it.

*Separately, D2's stated cost comparison rests on a false premise.* Spec §13 D2: *"The ordinary
post-swap refresh would have reused `crate::refresh` untouched."* It would not:
`refresh_resident` skips every key whose `prefix != generation.prefix`, and rung 3's own comment
says *"a different base means a different prefix, which is compaction, and nothing carries across
it"*. The post-swap alternative also needed a change. Low severity on its own; recorded because
D2 is ruled and the ruling cites the comparison.

### 4. A fourth seam gap, and it is the one with a data-loss path: `prefix_dir`

Spec §4 names three gaps in `publish_geometry`. There is a fourth. `prefix_dir` is cloned once into
`MaintenanceDeps` at `Engine::start_write_executor` and held on the executor for the process
lifetime. Every flush, merge and coalesce writes under it, and so does
`crate::flush::write_segments_manifest` — the deny-state manifest write. A fold that flips
`CURRENT` to a new prefix without rotating that field leaves the executor writing the next flush's
segment, and **every subsequent `SEGMENTS-<n>.json`**, into the old prefix directory — which spec
§8 then deletes whole.

That is not an omission that shows up as a wrong path. It is a suppression set that exists on disc
nowhere the next restart will look (SA §6.2's fail-open), and a flush whose files vanish under it.
`Engine::prefix_dir` and `Engine::external_index` — a separate `ArcSwap`, not part of the
generation, so not swapped atomically with it — are the same class of process-lifetime binding.

**What would change:** spec §4's gap list, and the ordering in §8 (reclamation must not precede the
executor observing the new prefix).

### 5. The trigger cannot be satisfied when suppressions dominate, and fires a no-op fold for ever

Spec §9's first gauge is `overlay_depth ≥ overlay_soft_limit`, sold as *"un-retired deny entries —
the existing knob, no new number"*. `Overlay::len()` is `|deleted ∪ suppressed ∪ evaluate|`, and
Rule S says a suppression **never** retires at a fold — the draft says so itself in spec §2 and
§11. So a deployment holding ≥ 500,000 live suppressions and few deletions:

1. crosses the gauge permanently,
2. fires a full fold every `compaction_min_interval_secs` (86,400 s) for ever,
3. and each fold changes the gauge by exactly zero.

That is a trigger whose condition its own action cannot clear — the oscillation the brief asked
about, and it is real rather than hypothetical: the suppression-heavy deployment is the natural
shape for a permission-masked service. The other two gauges do collapse at the flip (dead bytes to
the new prefix's orphans, tombstoned rows to zero), so this is the only self-retriggering one.

**What would change:** obligation 1's gauge must be the **deletion** count, which is what Rule F
retires — and that costs the "no new number" economy the draft claims for it, since a deletion
count needs its own threshold.

### 6. Pass 3 rewrites 43% of the bundle in three lines, and the existing precedent is a 64 GB trap

The external-id family is **20.25 GB of the measured 47 GB bundle** (16.25 GB of runs plus the
4 GB locator). Spec §3 gives it a paragraph, and spec §10 lands it as *"the external-id run merger
and the locator scatter"* in `tessera-store` — where a merger already exists and is exactly what
must not be used:

- `coalesce::read_runs` returns `Vec<(Vec<u8>, u32)>` — one heap allocation per key. At 10⁹ keys
  that is **~50–70 GB of anonymous RSS** (modelled: 24 B `Vec` header + a ≥32 B allocation for the
  key bytes + 4 B entity + padding), before `write_coalesced_run`'s `sort_by` over it.
- `write_coalesced_run` then allocates `vec![ROW_ABSENT; span]` for the locator — the full 4 GB, as
  anonymous memory, the same shape as `write_permutation_iter`'s `vec![PERMUTATION_ABSENT; bound]`.

`execute_merge` already calls both, so the precedent points straight at the trap, and the draft
does not forbid it. Note also that `write_permutation_iter` — the *only* permutation writer that
exists — materialises the whole slot array as an anonymous `Vec` and then streams it through a
`BufWriter`. Pass 1's "written through a mapping" is not something any existing code does.

**What would change:** spec §3 pass 3 should state the streaming shape and say explicitly that
`coalesce_external_id_runs` is not it; spec §10 should say the same about `write_permutation_iter`.

### 7. The wall clock is CPU-bound and single-threaded, and the suspension it forces is a serving cost the draft does not price

Spec §14 carries the fold's wall clock as *"modelled — IO-bound, minutes"*. The nearest measurement
contradicts the "IO-bound" half: the 10⁹ build produced this same 47 GB in **10 m 25 s at 233%
CPU** — ~1,460 CPU-seconds against ~230 GB of IO, which NVMe serves in a small fraction of that.
The fold does at least: a k-way merge plus 10⁹ random scatters into a 4 GB mapping; 1.72×10⁹
postings re-encoded; 10⁹ external-id keys merged and 10⁹ locator slots scattered; `pairs.parquet`
re-emitted over 1.72×10⁹ pairs; and ~47 GB of SHA-256 (**~50–90 s alone** at typical single-core
rates, modelled). On **one thread**.

**Modelled estimate at 10⁹: 20–45 minutes, and hours is not excluded.** That is the number spec §9's
operator surface and spec §1's suspension should be reasoned from.

And the suspension is not free. Spec §1 prices it as *"waste, not hazard"*, but flush continues and
publishes one segment and one delta tier per `flush_max_age_secs` (default 90 s), with **nothing
bounding either axis for the fold's whole duration**. Arch §11.3's per-(tile × segment) model is
*confirmed by measurement* at 10⁷ (1.6 µs per pair), so a viewport's cost is linear in live segment
count: a 20-minute fold adds ~13 segments, an hour adds ~40, and they cannot be merged until it
finishes. The tier axis is the cheaper one (fragment build measured flat in tier count, P2), but
the segment axis is not. This is a slowdown *during* the fold that spec §6 — whose subject is the
flip — does not cover, and it is the second-most-likely way the design disappoints on "minimal
slowdown during the fold" after page-cache pollution.

**What would change:** either let merge run during the fold and accept its output being orphaned
(the draft already says the safety argument does not rest on the suspension), or state the segment
growth as a cost and bound the fold's duration by giving passes 1 and 3 a small dedicated
`rayon::ThreadPool` rather than one raw thread. Decision 0043 forbids *occupying request-serving
workers*; a 2–4 thread pool of its own answers that as well as one thread does, and buys back the
parallel primitives these crates already have (`Permutation::project`, `par_sort_unstable`).

### 8. Opening the new prefix is not lazy, and spec §4 step 5 names the wrong cost

*"Mapping the new files is lazy; what costs anything is rebuilding the carried-forward extents' row
maps."* Three things cost more:

- **`PostingsReader::open` validates every record eagerly.** `validate_records` walks all
  `array.len()` records and, for every tag-1 payload, round-trips it through
  `Bitmap::try_deserialize::<Portable>` plus `get_serialized_size_in_bytes`. That is a full pass
  over `postings.arrow` (a full read into page cache) plus an owned-bitmap allocate-and-drop per
  head term — at 1.17×10⁸ terms, a serial cost at the swap measured by nothing.
- **Digest verification is a whole-bundle read, or it is skipped.** `open_bundle` → `verify_files`
  reads and SHA-256s every manifest-named file except the sidecar family. If the fold's in-process
  open uses it, the flip pays ~30 GB of read plus SHA-256 (~30–60 s, modelled). If it does not, the
  new prefix is served unverified — a deviation from contracts §2.3's reader protocol that the
  design must state rather than leave to the implementer. The draft says "not `Engine::open`" and
  stops there.
- **`derive_denied` is re-run on the executor**, per slice, doing `row_space.row_of()` for every
  entity in `deleted ∪ suppressed`. At `overlay_soft_limit` that is 500,000 random probes into a
  4 GB mapping. Warm it is tens of milliseconds; against a permutation the fold has just pushed out
  of page cache it is seconds. (`Permutation::validate_rows`, if called, is worse: an O(bound) scan
  plus a `vec![false; row_count]` — 1 GB at 10⁹.)

The draft is right that this is *bounded* work. It is wrong that it is negligible, and it is the
serial section the whole flip sits inside.

### 9. Peak disc is ~3.5–4.1× live, not 2×, and §8 contradicts §0

Spec §8: *"Peak disc is old prefix + new prefix — roughly 2× live bytes."* Spec §0 obligation 2, in
the same document: on-disc bytes are a **measured 2.0–2.6× the bytes the manifest names, and only
grow**. The old prefix is the on-disc figure, not the live one. Adding the spools — spool-then-
assemble means `columns.arrow` (18.5 GB) and `morton.u32` (4 GB) exist twice while pass 1 runs,
plus the postings spool — gives:

| term | at 10⁹, from measured file sizes |
|---|---|
| old prefix (2.0–2.6× live) | 94–122 GB |
| new prefix | ~47 GB |
| spools | ~23 GB |
| **peak** | **164–192 GB, i.e. 3.5–4.1× live** |

An operator provisioning from "2×" under-provisions by roughly half, and the fold's free-space
precondition (*"its estimated output plus a margin"*) does not obviously include the spools.

On the object store the architecture names (§ "Data is held in an object store"), carried-forward
files are copies rather than links, adding those bytes again. The draft flags that the estimate
"has to say which it is" and then does not say.

### 10. Pass 1's "one writer, two producers" cannot be built against the writer that exists

`write_segment` — the one writer spec §3 says pass 1 feeds — takes `&[TilerItem]`, which is the
4.4–4.9× materialisation the fold exists to avoid. The only bounded route is
`write_columns_from_parts`, which takes mmap-backed `Buffer`s and is written for exactly this
handover — but it has **no caller anywhere in the tree** (only tests), and it supports **only the
two fixed columns**. A bundle with declared scalars has no bounded write path at all, and a `Utf8`
scalar needs an offsets buffer spooled alongside its values.

So spec §3's *"one writer, two producers"* is a promise about code that does not exist yet, and the
design should say which it is: `write_columns_from_parts` widens to carry the declared scalar
schema, or the fold refuses a bundle that declares scalars. Silence here is how the second writer
write-path §7 warns about gets built by accident.

### 11. The gauges are not cheap, and one of them does not measure what it says

- **`tombstoned_rows / live_rows`.** `Overlay`'s `deleted` is entity-space, and a deleted entity may
  never have had a row — `MergePolicy::select`'s own doc says so (*"a deleted entity acquires no
  row"*). Counting rows means `row_of` per deleted entity: up to 500,000 random probes into the
  4 GB permutation mapping, **on the executor, every flush tick**. And the only accessor that
  exists, `Overlay::deleted_entities()`, materialises a `Vec<u64>` of the whole set (4 MB per call
  at the soft limit).
- **`dead_bytes` (on-disc minus manifest-named)** requires a directory walk and a `stat` per file
  per tick.

Both want a maintained counter instead of a per-tick scan: the deletion path already performs the
`row_of` lookup once when it adds the row to the deny mask, and every publication already knows
exactly which files it orphaned. That is strictly less work and less code than either scan.

### 12. Pass 2's sizing figure is wrong, and the corpus has the measured one

Spec §3 pass 2: *"The largest term plausibly covers 25–50% of all points (§3), which at 10⁹ is 2 GB
as `u32`s and ~62 MB as the portable Roaring."*

The corpus measured this. A 25%-coverage mask over a 10⁹ universe serialises to **125.12 MB — "the
dense bound for a 10⁹ universe"** (`probes/results.md` §4.2, on a real 10⁹ bundle; reproduced in
`probes/2026-08-04-refresh-ladder/1e9.txt`). It is still 125.12 MB at 41.6%, 72% and 100% coverage,
so the 50% end of §3's range is the same number. The draft's figure is understated ~2×.

The *live* peak is higher again, because `encode_posting` builds the record in three stages: the
unioned `Bitmap` (~125 MB of containers), the `Vec<u8>` that `serialize::<Portable>()` returns
(~125 MB), and the `record` `Vec` it is then copied into (~125 MB, up to 250 MB while doubling).
**~375–500 MB transient for one term** — bounded and affordable, but eight times what the draft
budgets. The bitmap-shaped `encode_posting` sibling the draft calls for should serialise straight
into the spool (tag byte, then `serialize_into` the writer) rather than build a record.

### 13. Simplicity: what does not earn its place

- **Four digest implementations.** `tessera_store::flush::digest_of` and
  `tessera_engine::coalesce::digest_of` both do `fs::read(path)` — a whole-file slurp, which at
  18.5 GB is a loaded gun with `execute_merge` already holding it; `read.rs::verify_files` streams
  in 1 MiB chunks; and spec §3 pass 5 adds a fourth (digest-as-written). One streaming digest
  writer used by all four is *less* code than what is there now, and removes the slurp.
- **The offsets spool** (finding 1b): ten lines that delete the largest O(corpus) term in pass 2.
- **Maintained counters instead of gauge scans** (finding 11).
- **Three gauges may be two.** The OR is justified as three independent obligations, but
  obligation 1's gauge is wrong (finding 5) and obligation 2's `dead_bytes` is largely a derived
  quantity of obligation 3 plus merge activity. Deletions + dead bytes, floored by the interval,
  covers all three with one fewer number to calibrate. Offered as an observation; the OR's
  reasoning is otherwise sound and the draft argues it well.

Against that, three things in the draft do earn their place and should not be cut in a
simplification pass: the failure-discards-the-fold rule (spec §3 pass 5), the refusal to
re-quantise or renumber (spec §0), and the tombstones set-difference (spec §2). Each removes
mechanism rather than adding it.

---

## Attacks that failed

Recorded so they are not re-run.

- **The k-way merge's read side really is O(k).** `MortonSlice::load` and `ColumnsRef::load` both
  mmap and decode zero-copy (`decode_single_batch` over a `Buffer::from_custom_allocation`), and
  the column accessors hand back slices into the mapping. A cursor is an index. Spec §3's first
  bullet holds exactly as written.
- **`PostingsSpool` really does what spec §3 claims of it.** Records are appended to a temp file
  and `finish` maps that file as the `LargeBinaryArray`'s values buffer, through the same
  `write_posting_array` the buffered path uses, so the output is byte-identical. Only the offsets
  half of the structure is unbounded (finding 1b); the values half is exactly the discipline the
  draft describes.
- **The hard-link "storm" is not one.** The measured 10⁹ bundle has **17 files**, and dictionary
  extents are bounded by the entity-space coalesce, which does select `dict_extents` under the same
  size-tiered window as tiers and runs. Carry-forward is tens of links. Pass 4's "an axis
  `coalesce` already bounds" is correct.
- **There is no cheaper row-space transform for a cached projection.** If the fold merely deleted
  rows in place, `newrow(r) = r − rank_dropped(r)` would transform every cached projection at
  O(containers touched) and make spec §6 nearly free. It does not: pass 1 globally re-sorts by
  `(morton, tessera_id)` across the base *and* the appended extents, so surviving rows do not keep
  their relative order — and preserving the order would leave the output not Morton-sorted, which
  breaks `tile_ranges`' binary search outright. The alternative (emit an `old_row → new_row` array
  and gather) costs O(grant cardinality), no better than the measured 4 550 ms. **Spec §6's "every
  resident session needs a full projection build" is correct and unavoidable.**
- **The fold is not self-retriggering through gauges 2 and 3.** Both collapse at the flip. Only the
  overlay gauge re-fires (finding 5).
- **Nothing a client holds breaks at the flip**, as spec §6 claims: the entity axis is untouched,
  `tessera_id` is stable, a tile is a Morton prefix, and `idset` does not advance. Checked against
  `RowProjectionKey`'s prefix field and the identity construction; no counterexample found.
- **The `x-tessera-stale` broadcast is already covered** by the 2026-08-02 C15 ruling; nothing new
  here.

---

## Page-cache pollution (P3): how bad, and does it need a mechanism

The draft calls this its weakest assumption. It is, and the honest range is wider than "a note"
suggests.

A fold streams ~47 GB of reads and ~70 GB of writes (with spools) past the page cache in one pass.
Everything the serving path relies on being warm — `columns.arrow`'s touched tiles, `morton.u32`,
and above all `permutation.bin`, which every projection build walks — is a clean reclaim candidate
in front of that stream. The measured cost model (1.6 µs per tile × segment at 10⁷) is a
**warm-cache** figure; a cold major fault on NVMe is ~100 µs. So the plausible worst case for a
concurrent viewport is not a percentage, it is **two to three orders of magnitude**, and it lasts
for the fold's whole 20–45 minutes rather than for the flip.

Two interactions the draft does not name:

- The **pre-swap warm runs at the end of the fold**, and full projection builds walk the 4 GB
  permutation mapping. Whether the new `permutation.bin` (written in pass 1) is still resident
  after passes 2, 3 and 5 have streamed ~25 GB past it decides whether spec §6's 4 550 ms figure
  applies at all, or whether every pre-warm entry pays 4 GB of major faults first. Unknown; P3
  should measure it, and it is the difference between a pre-warm that costs 0.7 s of pool time and
  one that costs minutes.
- `verify_files` at the *next restart* is already a whole-bundle sequential read, so the fold's
  stream is not a new class of behaviour — but it is the first one that happens while serving.

**It needs a mechanism, and a one-line one exists.** `posix_fadvise(POSIX_FADV_SEQUENTIAL)` on the
fold's inputs while reading and `POSIX_FADV_DONTNEED` on ranges as they are consumed and on outputs
as they are written keeps the fold's stream from evicting the serving set. That is a handful of
syscalls per pass, not a mechanism in the design sense — but it must be *named* in spec §3, because
"a note, pending P3" is how the design ships with the property the owner ranked fourth being the
one it breaks.

---

## Probe recommendations

**P1, amended.** As drafted it would miss finding 1 entirely. It must (a) sample `VmHWM`, the
metric `probes/2026-08-04-maintenance-memory/` used and a cgroup compares against, not heap;
(b) report the terms separately — resident pages of the two dense mappings, the offsets vector, the
per-term encode peak — because a single number cannot distinguish reclaimable file pages from
anonymous ones and the operator conclusion differs; and (c) **add a term-count axis**. The
`scale.rs` fixture and the 10⁹ bundle both carry ~48,000 terms; the offsets term and the
largest-term encode only appear at 10⁷–10⁸ terms, so a run at fixed row count and 10⁸ terms is the
one that decides whether the central claim holds.

**P2, amended.** As drafted it does not say the projection cache is *at its bound*, which is the
only configuration in which findings 2 and 3 exist. It must run with `row_projection_cache_bytes`
and `fragment_cache_bytes` at their defaults, a populated cache, and live viewport load across the
pre-warm and the flip, reporting `evictions`, `young_evictions`, `full_projection_builds`,
`stale_serves` and request latency. Without those it will report that pre-swap beats post-swap and
tell us nothing we did not model.

**P4, new.** The fold's wall clock and the suspension's cost, together: run a fold at 10⁷ on the
`scale.rs` harness with ingest continuing, and report the segment and tier count at the flip
alongside the z6/z8 viewport latency through the fold. That is what turns finding 7 from a model
into a number, and it re-runs from the tree.

**P3, as drafted**, plus the pre-warm interaction above.

## What this review did not cover

The invariants, the leak register, Rule F/Rule S, and I2's forward obligation — another lens owns
those. Where a finding here touches one (finding 4's lost deny state; finding 5's gauge, which is a
Rule S/Rule F conflation showing up as a trigger bug) it is flagged as cross-lens rather than
adjudicated.
