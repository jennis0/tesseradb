# What an ingest and a compaction cost on the disk

**Status:** Fact-find, 2026-09-10, for the disk-use campaign. **Not normative, and nothing here is
decided.** Produced by a subagent reading main at 0c0b60b3 as one of eight parallel investigations;
it is that agent's read of the code, not a reviewed design. The owner's session independently
verified `statvfs` has exactly two call sites in the workspace; `FOLD_DISC_PERCENT` is 150 and its own doc marks the margin assumed; the fold's `D₀` is `deleted_entities()` alone; and `grown_wal_pos` is set unconditionally at `membership.rs:1616` where `ingest.md` §2.4 says a growth above the high-water is released at the next tail pack. **Every other claim here is unverified** — re-check a
figure or a citation before relying on it.

Measurement tooling for figures the report marks as taken in its own session is in
[`probes/2026-09-10-disk-survey/`](../../../probes/2026-09-10-disk-survey/). The campaign's
starting point is [`2026-09-10-build-disk-weight.md`](2026-09-10-build-disk-weight.md), which
these supersede in the places they contradict it.

The other fact-finds in this set are the sibling `2026-09-10-disk-*.md` memos in this
directory.

**Date** 2026-09-10. **Branch** main at 0c0b60b3. Read-only; nothing was built or run. Figures are
labelled **measured**, **read-from-code**, **modelled** or **assumed**. Two figures are measured
from bundles a previous test run left in `/tmp`; they are labelled as such and the fixture's schema
is stated where it matters.

## The results, first

1. **There is no disk forecast anywhere on the ingest path.** `statvfs` is called from exactly two
   places in the workspace: the build's pre-flight (`crates/tessera-build/src/pipeline.rs:584`) and
   the compaction fold's (`crates/tessera-engine/src/write.rs:5476`). `NoFlush` has four variants and
   none of them is about space (`crates/tessera-engine/src/flush.rs:454-487`). Ingest runs until the
   filesystem refuses, and the code says so in as many words:
   `DEFAULT_WAL_HARD_LIMIT_BYTES` "bounds a startup relation; it does not stop appends … past that
   point the WAL grows until the filesystem refuses, at which point `WalError::Poisoned` makes the
   handle dead" (`crates/tessera-server/src/config.rs:1765-1772`). Read-from-code.

2. **None of the build probe's six changes reaches ingest by shared code.** All six live entirely in
   `tessera-build`. Four do not apply — the constructs they fixed have no ingest counterpart. Two
   have an ingest counterpart that already has the saving. **One has an unfixed counterpart at the
   fold**: `permutation.bin` and `ext-locator.u32`, both written whole over the entity space, are the
   same shape as the ordinal geometry the probe stopped materialising, and compaction §3 already
   names the fix and has not taken it.

3. **A compaction's peak is old prefix + new prefix, and the pre-flight charges 1.5× the manifest-named
   live bytes** (`FOLD_DISC_PERCENT`, `crates/tessera-engine/src/compact.rs:640-647`). Because the old
   prefix on disk is a measured 1.32–2.6× live, the true peak is about **2.3–3.6× the live bundle**.
   **A box that can serve a corpus cannot always compact it**: serving needs live + orphans resident,
   compacting needs a further 1.5× live *free*, and there is no override, no partial fold and no
   resume. The refusal is a `tracing::warn!` and a once-per-crossing alarm
   (`crates/tessera-engine/src/write.rs:7225-7231`).

4. **Every artifact membership growth pins the whole WAL until the next fold** —
   unconditionally, not only growths below a level's high-water as `ingest.md` §2.4 states.
   `grown_wal_pos` is set on every `apply` of a growth record
   (`crates/tessera-lifecycle/src/membership.rs:1614-1618`) and cleared only by
   `mark_growth_packed`, "called only from the fold, and only after its flip"
   (`crates/tessera-lifecycle/src/membership.rs:2354-2372`). This is the ingest path's one unbounded
   disk term and it is the disk cost the probe's question was actually about.

5. **A suppressed item's disk is permanent.** The fold's `D₀` is `deleted_entities()` alone
   (`crates/tessera-engine/src/compact.rs:774-779`); no pass touches a suppressed entity. Its row, its
   postings, its values, its blob row and its text terms stay for the life of the corpus, and it costs
   about **70 bytes of pretty-printed JSON in every side-manifest written thereafter** — every one of
   which is retained until a fold.

6. **The per-segment fixed cost is dominated by the filesystem block quantum, not by headers.** A
   flush writes a fixed file set that is a function of the *schema*, not the data — every declared
   filter column gets an extent every flush, empty or not
   (`crates/tessera-filter/src/extent.rs:56-59`). An empty extent is ~442 B of content and one 4 KiB
   block. For a 13-column schema that is ~37 files ≈ **148 KiB per flush before a row is stored**,
   which matches the data at about **4,000 rows a flush** (modelled from 39 B/row measured).

---

## 1. The ingest path's disk peak

### 1.1 What a batch writes, in order

| Stage | File | Size | Basis |
|---|---|---|---|
| window close | one `IngestBatch` record per batch, appended to the active WAL member, one fsync per window | the whole row: `external_id`, `entity_id`, the **view name as a string per row**, `join`, raw descriptor bytes, `x`/`y` as two `f64`, every declared scalar including text, and the group-scoped list (`crates/tessera-lifecycle/src/wal.rs:188-221`) | read-from-code |
| window close | `CategoryCode` mints, ahead of the batches they colour, inside the same fsync | kilobytes | read-from-code |
| flush | `segments/<seg_id>/morton.u32` | 4 B/row | read-from-code; **measured** 160,000 B / 40,000 rows |
| flush | `segments/<seg_id>/columns.arrow` | `tessera_id` u64 + `residual` u32 + the render tail | **measured** 12.27 B/row on a fixture with no render scalars |
| flush | `segments/<seg_id>/delta.arrow` | the flush's terms only | **measured** 1,082 B at 40,000 rows |
| flush | `segments/<seg_id>/external-ids.arrow` | key bytes + u32 entity, sorted by key (`crates/tessera-store/src/flush.rs:249-250`) | **measured** 16.28 B/row on 8–10 B keys |
| flush | `segments/<seg_id>/ext-locator.u32` | **4 B × the entity *span*, not the row count** (`crates/tessera-store/src/flush.rs:252-257`) | read-from-code |
| flush | `segments/<seg_id>/render-presence/<col>.roaring` | one per render column with any absence | read-from-code |
| flush | `attrs/<col>/extents/<seg_id>.{arrow,roaring[,dict]}` | one set **per declared filter column, always, even empty** | read-from-code |
| flush | `entities/terms/extents/<seg_id>.{hasrow.roaring,offsets.u32,terms.u32}` | 4 B/entity + 4 B/(entity,term) pair | read-from-code; **measured** 4.00 + 4.00 B/row at one term an entity |
| flush | `attrs/record/extents/<seg_id>.{blocks.bin,hasrow.roaring,directory.arrow}` | the blob layer, if any record column | read-from-code |
| flush | one text extent per indexed text column, three files | read-from-code |
| flush | `segments/<seg_id>/terms-0.dict` | only on a promoting flush | read-from-code |
| flush | `<seg_id>.spool.<i>` beside each `columns.arrow` / `external-ids.arrow` | transiently **doubles** the column bytes; unlinked by `SpoolGuard` on every exit path (`crates/tessera-store/src/write.rs:346-360`) | read-from-code |
| publication | `partitions/<p>/SEGMENTS-<n>.json` | **complete state, refuse-to-replace, superseded ones retained** (`crates/tessera-store/src/manifest_write.rs:200-258`) | read-from-code |

No permutation file is written at a flush: the entity→row extent is built in memory and rebuilt at
open from the segment's own `tessera_id` column (write-path §4.3;
`crates/tessera-store/src/flush.rs:222-231`).

### 1.2 Per item ingested

**Measured, from a bundle a test run left at `/tmp/.tmprHH5XV`** — segment `flush-9-8`, 40,000 rows,
one view, no declared filter columns, no record blob, no text index, one term an entity, short
external ids:

| file | bytes | B/row |
|---|---|---|
| `morton.u32` | 160,000 | 4.00 |
| `columns.arrow` | 490,882 | 12.27 |
| `ext-locator.u32` | 160,000 | 4.00 |
| `external-ids.arrow` | 651,010 | 16.28 |
| `delta.arrow` | 1,082 | 0.03 |
| `terms/…offsets.u32` | 160,004 | 4.00 |
| `terms/…terms.u32` | 160,000 | 4.00 |
| `terms/…hasrow.roaring` | 15 | — |
| | **1,782,993** | **44.6** |

⊘ That fixture is a test artefact, not a campaign: I read it off the disk and did not run the test
that made it. It agrees with the independent campaign figure below to within 15%, which is the
external ids' length.

**Measured, `probes/2026-08-05-write-path-at-scale/`**: live bytes grow at **~39 B/row**, "~9.3 MiB
per 250,000 rows — with no super-linear term over 40× of growth". That is the number a capacity model
wants.

Add to that, per item, whatever the schema costs: the filter columns' own values, the record blob's
compressed extents, the text index's postings. Those are the payload and they are schema-shaped, not
constant.

**Two per-item terms that are not per-row**, both read-from-code and neither previously written down:

- **`ext-locator.u32` is 4 B × the entity *span* the segment covers**, and the span is not the row
  count. A commit window interleaving views, or a deletion between flushes, leaves holes
  (write-path §4.2: "ascending-with-holes where deletes struck or where a commit window interleaved
  views"). On a corpus of *V* views round-robining, each view's segment spans roughly *V* times its
  own rows, so the locator costs about **4V B a row**. Modelled from the construction; not measured.
- **The WAL carries the view name as a string on every row** (`WalRow::view`,
  `crates/tessera-lifecycle/src/wal.rs:191`). Read-from-code.

### 1.3 Per resident item

Nothing on the ingest path is proportional to the resident corpus at all. That is the shape
difference from the build: a flush's cost is proportional to *its own batch*, bounded by
`flush_max_items` (40,000, `crates/tessera-server/src/config.rs:1810`) or by
`ingest_buffer_max_items` (10⁶, at which ingest sheds 429). One flush's whole transient is at most
about 39 MB of output and the same again of spools. **Modelled** from the two figures above.

What *is* proportional to the resident corpus is the fold — §3 — and everything that waits for it.

### 1.4 What stands at the ingest peak

There is no peak in the build's sense. Disk on the ingest path is **monotonically increasing between
folds**, and the peak of the whole cycle is the fold's. The terms that accumulate:

| term | released by | bound |
|---|---|---|
| the live bundle | never | 39 B/row (measured) |
| merged-away segments, consumed tiers, runs, locators, attr/record/text/transpose extents | **only the fold** | measured 1.32–1.59× live at 5M–250M (`2026-08-05-write-path-at-scale.md` §2); the compaction docs quote a 2.0–2.6× no-compaction steady state |
| superseded `SEGMENTS-<n>.json`, one per publication | **only the fold** | ~158 B per live file per manifest (**measured**: 8,227 B over 52 entries) |
| orphan files from a failed or discarded flush, merge or coalesce | **only the fold** | unbounded under sustained failure; each retry takes a fresh `seg_id` attempt number |
| the WAL | rotation at each publication, **unless pinned** | steady state two members; **unbounded once any artifact growth is outstanding** |

`reclaim_prefix` is the only thing in the tree that deletes bundle data
(`crates/tessera-store/src/reclaim.rs`, module doc: "Nothing else in this codebase deletes a file"),
and it deletes a whole prefix. `sweep_orphan_prefixes`
(`crates/tessera-engine/src/write.rs:5378`) runs at startup and reclaims *whole orphaned prefixes*
left by a discarded fold; it does not touch orphans inside the live prefix.

### 1.5 Is there a pre-flight?

**No.** Established three ways:

- `statvfs` has two call sites in the workspace, neither on the ingest path
  (`crates/tessera-build/src/pipeline.rs:584`, `crates/tessera-engine/src/write.rs:5476`).
- `NoFlush` is `NothingToFlush | WalPoisoned | OverlayDiverged | SteppedDown`
  (`crates/tessera-engine/src/flush.rs:454-487`). No space variant.
- `wal_hard_limit_bytes` is a **startup** relation only, and its own doc says the WAL then "grows
  until the filesystem refuses" (`crates/tessera-server/src/config.rs:1765-1772`). `Wal` exposes no
  length accessor, so nothing can compare the live log against it.

What happens instead, read-from-code:

- **A flush that hits ENOSPC** fails, the buffer is retained, and it is retried at the next tick with
  a fresh attempt number, leaving the previous attempt's files as orphans
  (`crates/tessera-engine/src/write.rs:9563-9576`). Sustained failure grows the buffer until
  `ingest_buffer_max_items` sheds ingest with a 429 — "the intended backpressure", as the alarm says.
  The orphans it leaves are reclaimable only by a fold.
- **A WAL append that hits ENOSPC** poisons the handle; a deny's window then takes the apply-anyway
  fold behind a 500 and the node goes unready (write-path §1.3).
- **The fold, the one operation that could reclaim, refuses when free space is below 1.5× live.**

So a device filling on the ingest path has no in-process route out: the flush orphans more on every
retry, the fold refuses, and the only reclamation event in the system is the fold.

---

## 2. Do the build probe's six changes reach ingest? (the decision 0091 audit)

All six are in `tessera-build` and nowhere else. `MappedArena`, `ARENA_GROWTH_STEP` and
`MemberTable` appear only in `crates/tessera-build/src/{spill,column,pipeline,residency}.rs`;
`EntityColumn` adds only a build test. **No shared code**, so the question for each is whether the
ingest path has the same construct.

| # | The change | Ingest's counterpart | Verdict |
|---|---|---|---|
| 1 | **Arena grows by a step, not by doubling** (`spill.rs:492-497`, `ARENA_GROWTH_STEP` 256 MiB, `posix_fallocate` over the new range) | None. `posix_fallocate` appears only in `tessera-build`. A string value at ingest travels as a heap `String` in `WalScalar::Utf8`, is written straight into an extent's Arrow values buffer or a blob block, and the flush's spools are plain `BufWriter` appends with no preallocation. | **Does not apply.** No file on the ingest path over-reserves. |
| 2 | **A `keyword`/`utf8` arena record carries no entity** (`column.rs:218-237`, 8 B header for `text`, 4 B otherwise) | None. A keyword at ingest is minted into a per-extent `SortedDict` and stored as a `u32` ordinal against a Roaring presence bitmap (`crates/tessera-engine/src/flush.rs:1595-1640`). There is no arena and no per-value entity. | **Does not apply**, and ingest's form is already the narrower one. |
| 3 | **A column goes back at its last reader, not at the release stage** (`pipeline.rs:1710-1719`) | The flush's columns are in-memory `Vec`s, and every file it writes is a serving artefact that lives until the fold. The only intermediate files are the `SegmentWriter`/`RunWriter` spools, and those already unlink on every exit path through `SpoolGuard` (`crates/tessera-store/src/write.rs:346-360`). | **Already had**, in its only applicable form. |
| 4 | **The anchor's geometry is the anchor view's** (`pipeline.rs:1114-1121`) | None. Ingest allocates entity ids at the commit window and sorts each flush segment against the view's own bounds. There is no anchor pass and no decision-0112 fallback array. | **Does not apply.** |
| 5 | **Each view's ordinal geometry goes at its permutation** (`pipeline.rs:1013`) | At flush, **already stronger**: no permutation file is written at all, the row map being rebuilt at open from the segment's `tessera_id` column (write-path §4.3). **But at the fold it is absent.** Pass 1 writes `permutation.bin` at 4 B × the permutation bound and pass 3 writes `ext-locator.u32` at 4 B × the entity bound, both scattered through a mapping over the *whole* entity space — 4 GB each at 10⁹ (compaction §3's table). Compaction §3 already names the fix and declines to take it: "splitting the base locator into entity-range extents — the mechanism flush segments already use — would drop the second and take it to ~1.5 GB". | **Absent at the fold.** This is the one live 0091 gap of the six. |
| 6 | **The member table unlinks itself** (`spill.rs:2133-2141`) | None on disk. A membership at ingest lives in `ArtifactStore` in memory and in the WAL, and is packed into extents at the tick. **The counterpart is worse than the build's was**: a growth record pins the entire WAL until the fold (§4 below), where the build's spill file was merely held to the end of the build. | **Does not apply**, and the ingest-side cost of the same data is unbounded rather than merely long-lived. |

**Summary for 0091**: four do not apply, one is already had at the flush and absent at the fold, one
does not apply and has a worse counterpart. The principle the memo drew out — *a structure
materialised in entity order because one consumer wanted random access* — reaches the ingest path in
exactly one place, and it is the fold's two mapped arrays.

---

## 3. Compaction's peak

### 3.1 What is on the disk

Read from `crates/tessera-engine/src/compact.rs` and compaction §§3, 8:

| | |
|---|---|
| the old prefix | everything on disc, live + orphans: measured 1.32–1.59× live at 5M–250M, quoted as 2.0–2.6× as the no-compaction steady state |
| the new prefix, written | ≤ the live bytes (deletions dropped, every axis coalesced) |
| the new prefix, carried forward | hard links — directory entries, no bytes (`crates/tessera-store/src/reclaim.rs`, `hard_link_forward`) |
| pass 1 scratch | `SegmentWriter`'s column spools, "corpus-sized (compaction §3, ~12 GB at 10⁹)" — **live at the same moment as the `columns.arrow` assembled from them** |
| pass 1 output | `permutation.bin`, 4 B × permutation bound, laid out over every page of the bound and compacted at the end |
| pass 2 scratch | `postings.spool`, the whole re-encoded postings; deleted at `finish`, and explicitly removed on the failure path (`crates/tessera-engine/src/compact.rs:1253-1257`) |
| pass 2 output | `postings.arrow` and `pairs.parquet`, the latter written unconditionally |
| pass 3 output | `ext-locator.u32`, 4 B × entity bound |
| pass 5 | re-reads every written file to digest it — "~47 GB at 10⁹. Recorded as a cost this design pays rather than as one it avoids" |
| throughout the flight | flushes keep publishing into the **old** prefix, and the WAL keeps growing |

### 3.2 The multiplier, and what the pre-flight charges

```
disc_estimate(live_bytes) = 1.5 × live_bytes,  compared against f_bavail
```
(`crates/tessera-engine/src/compact.rs:640-647`; the check at `:756-771`). `live_bytes` is the sum
of **both** manifests' `files` maps — the bundle `MANIFEST.json` and the partition side-manifest;
summing one alone reported a 1065× orphan ratio in a measured run
(`crates/tessera-engine/src/write.rs:5423-5431`).

The estimate deliberately reads the *live* bytes and not the bytes on disc (compaction §8:
"orphans and merged-away segments are not what the fold is about to write"). But `f_bavail` is
measured against a device that already holds the orphans. So the real requirement is:

**disk needed = on-disc bytes (1.3–2.6× live) + 1.5× live ≈ 2.8–4.1× live.** Modelled, from the
measured on-disc ratio and the constant in the code.

The actual peak, before the reclaim at the end, is **on-disc + the new prefix ≈ 2.3–3.6× live**
(modelled). It is proportional to **the whole corpus**, not to the segments being folded and not to a
bounded window: `plan_fold` takes every live segment of every view
(`crates/tessera-engine/src/compact.rs:683-712`), and pass 1 merges all of them into one new base
per view.

⊘ **The 50% margin does not name the spools.** Compaction §8 says the margin covers "the flushes
that keep publishing into the old prefix, the WAL they append to, and the fragment cache". The
pass-1 spools are corpus-sized by the code's own comment (~12 GB at 10⁹ against a bundle the same
document prices at ~47 GB — about a quarter of live), and they stand at the same instant as the
`columns.arrow` they assemble. `FOLD_DISC_PERCENT`'s own doc calls the 50% **assumed**. So the
margin is roughly half spent on a term nobody listed. Not measured; established by reading the two
constructions against each other.

### 3.3 Can a box that can serve a corpus always compact it?

**No.** Read-from-code:

- Serving needs live + orphans resident. The measured unreclaimed ratio is 1.32–1.59×, and the
  fold's own trigger fires at `dead_bytes/live_bytes ≥ 1.0`, i.e. at 2×.
- Compacting needs a further **1.5× live free**, or `plan_fold` returns
  `NoFold::InsufficientDisc { need, free }`.
- A box provisioned at, say, 2× live is a box that can serve the corpus and its orphans and
  **cannot** fold it.
- There is no override, no `--force`, no partial fold, no resume: "Failure at any point discards the
  fold … there is no resume" (compaction §3 pass 5). The refusal is a `tracing::warn!` and a
  once-per-crossing alarm; `POST /control/compact` still answers 202 and the answer appears only in
  the `compaction` block on `/control/status`.
- Once refused, the situation worsens monotonically: every merge orphans its inputs (measured
  +46.6 MiB on a merge round against +9.0–9.4 MiB on an ordinary one), every publication leaves a
  side-manifest, every failed flush leaves an orphan segment, and nothing but the fold reclaims any
  of it.

⊘ **The sharding design would fix this and is not built.** Under `sharding.md` §3.4 a compaction
names one point shard and hard-links the other shards' directories whole into the new prefix, so the
peak becomes one shard's live bytes plus links. `sharding.md`'s status line: "Nothing in this
document is built."

---

## 4. The deny removal rules against disk

### 4.1 What a suppressed item holds, and for how long

**Everything, for ever.** Read-from-code:

- A suppressed entity is **flushed normally** (write-path §4.2: "A suppression never touches
  postings and retires only on unsuppress; a flush that skipped it would leave a later unsuppress
  with nothing to reveal"). It gets a row, a posting, a value in every filter extent, a blob row and
  its text terms.
- The fold's `D₀` is `generation.overlay.deleted_entities()` and nothing else
  (`crates/tessera-engine/src/compact.rs:774-779`). No pass reads `suppressed`.
- No merge or coalesce axis retires anything (write-path §7: "**No axis retires anything**").

Its continuing cost beyond the row itself:

| | cost | for how long |
|---|---|---|
| one `DenyEntry {entity_id: u64, cause: String}` in the side-manifest (`crates/tessera-store/src/manifest.rs:1272-1275`) | ~70 B + the cause string, pretty-printed JSON (**modelled** from `serde_json::to_vec_pretty`'s array shape at depth 2) | in **every** side-manifest written until the suppression is lifted, and every superseded manifest is retained until the fold |
| one `OverlaySnapshotEntry` at the head of every WAL member (`crates/tessera-lifecycle/src/wal.rs:253-256`, written by `Wal::rotate`) | ~6 B postcard | one per retained member; steady state two, **unbounded while the log is pinned** |

The design already prices the manifest half: write-path §5.6 says per-window publication through a
bulk revocation of *N* entities writes Θ(N²/window) bytes, "modelled ~12–25 GB at N = 10⁶", and the
64-window batching floor is what holds it down.

### 4.2 What a deleted item holds

A deletion accepted **before** a flush's plan snapshot costs nothing: "Deleted → never written"
(write-path §4.2). A deletion of an already-flushed item holds its row, its postings, its values, its
blob row and its text terms **until the fold that executes it** (Rule F), plus one `u64` in
`SegmentsManifest::tombstones` — a plain JSON array, ~15 B per entity per manifest, pretty-printed
(`crates/tessera-store/src/manifest.rs:2026`; **modelled** from the format).

### 4.3 Is there a corpus shape where deletions accumulate faster than compaction reclaims?

**Yes, three of them.** Established, not proposed:

1. **The fold refuses on space.** Deletions add disk (retained rows, plus a tombstone in every
   manifest) and the only thing that removes them needs 1.5× live free. Below that threshold
   deletions accumulate at their arrival rate and reclaim at zero, for ever. This is the shape that
   has no exit.

2. **The 24 h floor against a fold that takes hours.** `compaction_min_interval_secs` is 86,400 and
   the origin is `max(start, end − interval)` (compaction §9). One fold is measured at 330 s over a
   36M-row bundle (`probes/2026-09-04-epoch-shard-fold-decomposition/`) and **modelled** at ~5.5 h at
   a 2³² shard. Deletions accepted during the flight are not in that fold's `D₀` — the plan's
   tombstone clone is taken at dispatch — so the floor under the standing overlay is
   `deletion rate × (fold duration + interval)`. At 5.5 h and a 24 h interval that is a permanent
   ~23% carry, plus a whole interval's arrivals.

3. **`compaction_after_deletions` is an absolute, and the rate can outrun it.** The default is 500,000
   (`overlay_soft_limit`). A deployment deleting more than 500,000 a day is permanently over the
   threshold and is rate-limited by the 24 h floor rather than by the gauge, so the overlay's depth is
   set by the interval and not by the trigger. Overlay depth is a term in deny-ack latency (every deny
   acceptance clones the overlay inside the WAL critical section,
   `crates/tessera-server/src/config.rs:1783-1785`), so this is a latency shape as well as a disk one.

**One more, and it is not about deletions at all**: the fold is also the only thing that releases the
WAL pin a membership growth takes (§1, result 4). A deployment that grows memberships and never
folds accumulates WAL without bound, and every retained member carries a full overlay snapshot — so
`suppressions × retained members` is quadratic on a pinned log. Read-from-code from
`Wal::rotate` (`crates/tessera-lifecycle/src/wal.rs:1594-1640`) and
`ArtifactStore::oldest_wal_pos` (`crates/tessera-lifecycle/src/membership.rs:1179-1208`).

⊘ **`ingest.md` §2.4 and the code disagree.** §2.4 says "a growth of an artifact above the level's
published high-water is released at the next tail pack; only a growth of an artifact below the
high-water waits for the fold". The code sets `grown_wal_pos` on every growth, unconditionally, with
the comment "**Held from here until a whole rewrite covers it, and `mark_published` does not release
it.** The append-only packer starts at the level's high-water and a grown record sits below it"
(`crates/tessera-lifecycle/src/membership.rs:1612-1618`). I did not establish which is intended.

---

## 5. Segment count and per-segment fixed cost

### 5.1 The file set is a function of the schema, not of the data

Per flush, per view (read from `execute_flush_stages`,
`crates/tessera-engine/src/flush.rs:915-1200`):

```
files per flush = 2 (morton, columns)
                + 1 (delta.arrow)
                + 1 (external-ids.arrow)
                + 1 (ext-locator.u32)
                + 1 per render column with any absence   (render-presence/<col>.roaring)
                + 2 or 3 per declared filter column      (values, presence, [dict])
                + 3 (entity→term transpose)
                + 3 if any record column                 (blocks, hasrow, directory)
                + 3 per indexed text column
                + 2 or 3 per group-scoped family per view
                + 1 on a promoting flush                 (terms-0.dict)
                + 1 SEGMENTS-<n>.json
```

"Every declared filter column gets one, including a column no flushed entity carries a value in. The
file set is then a function of the schema rather than of the data … an empty extent costs a few
hundred bytes and composes to nothing" (`crates/tessera-filter/src/extent.rs:56-59`,
`crates/tessera-engine/src/flush.rs:1512-1517`).

**Cross-check, measured** from `/tmp/.tmpjXyg87/…/SEGMENTS-9.json`: 52 file entries over 6 segments,
8 deltas, 6 runs, 5 locators, 8 transpose extents and 1 dict extent — 51 by the formula.

### 5.2 The fixed bytes

| artefact | empty size | basis |
|---|---|---|
| a value extent (`.arrow`) | **434 B** | measured: a one-column empty Arrow IPC file, via pyarrow |
| its presence bitmap (`.roaring`) | 8 B | portable Roaring cookie + container count |
| a keyword dictionary (`.dict`) | 32 B | `MAGIC + FOOTER_LEN`, `crates/tessera-filter/src/dict.rs:183-186`, 376 |
| `hasrow.roaring` on the transpose | **15 B** | measured, `/tmp/.tmprHH5XV` |
| `offsets.u32` | 4 B + 4 B/entity | read-from-code |
| `delta.arrow`, one small tier | **1,082 B** | measured, `/tmp/.tmprHH5XV` |
| a side-manifest base | ~1.1 KB | measured, `SEGMENTS-0.json` |
| a side-manifest, per named file | **158 B** | measured: 8,227 B / 52 entries |

**The block quantum dominates.** Each of those files takes at least one filesystem block (4 KiB on
ext4, no tail packing) and an inode; each segment directory takes another. So the honest fixed cost
is **~4 KiB a file**, not ~450 B.

### 5.3 When the fixed cost becomes material

Modelled, from the 4 KiB block quantum and the measured 39 B/row:

| schema | files a flush | fixed, allocated | matches the data at |
|---|---|---|---|
| minimal (1 view, no filter columns) | ~9 | 36 KiB | ~950 rows a flush |
| 4 declared columns | ~17 | 68 KiB | ~1,800 rows |
| 13 declared columns (`geonames`) | ~37 | 148 KiB | **~3,900 rows** |

At the shipped `flush_max_items` of 40,000 rows (`crates/tessera-server/src/config.rs:1810`) a
13-column schema pays about **9%** in fixed cost. A deployment ingesting a few hundred rows a 90 s
tick pays more in fixed cost than in data. The threshold to state is roughly
**`rows a flush < 100 × files a flush`**.

### 5.4 The other fixed cost: the side-manifest is quadratic in publications

Every publication writes complete state and none is ever removed outside a fold. With *L* live files
and *K* publications since the last fold, the superseded manifests hold about
`158 × L × K` bytes (modelled from the measured 158 B/entry).

At a 90 s tick that is 960 publications a day. With `L ≈ 100` that is ~15 MB a day; with the
`L ≈ 500–1,000` a 10⁹-row corpus would carry it is ~75–150 MB a day. Bounded in practice by
`compaction_min_interval_secs` (86,400) **if a fold happens**, and unbounded if one does not.
Modelled.

### 5.5 Segment count

| | |
|---|---|
| merge saturates at ~149 MiB and the count grows linearly thereafter | **measured**, `2026-08-05-write-path-at-scale.md` §4: 2 → 18 live segments over 200 flushes, settling at 15; ~**1 live segment per 4M rows ingested** |
| at 10⁹ | ~152 segments, ~73 ms on a 300-tile viewport against a 135–164 ms baseline — **modelled**, write-path §7 |
| bounded by the fold trigger at | `compaction_max_segments` = 64 per view, unwindowed; `compaction_window_min_segments` = 8 inside the daily window |
| the coalesce bounds the other axes repeatedly | tiers, runs, dict extents, attr/record/text/transpose extents, `coalesce_width` 8 (measured: 1 → 8 → 2, repeatedly) |

So live segment count is bounded at 64 per view **iff compaction runs**; a deployment whose fold is
refused (space, WAL poisoned, stepped down, or every gauge switched off) grows segments linearly at
~1 per 4M rows with no ceiling, and every viewport pays one binary search and one
`range_cardinality` per segment per tile.

`sharding.md` §3.4 (not built) would make the segment axis and the compaction peak per shard, and the
sealed shard's final form is "one base segment per view, one postings tier, one external-id run".

---

## What I could not establish

- **Whether `ingest.md` §2.4's release rule or the code's unconditional pin is intended.** The two
  disagree about when a membership growth releases the WAL. Owner question.
- **Whether the fold's 50% disc margin covers its own spools.** The margin is documented as covering
  in-flight flushes, the WAL and the fragment cache; the spools are corpus-sized and unlisted. I
  established the sizes and the omission, not what a corrected margin should be.
- **The `ext-locator.u32` span cost at scale.** I read the construction (4 B × entity span, holes
  included) and could not find a measurement of the span-to-rows ratio on any multi-view or
  deletion-churned corpus. No bundle on this box has one.
- **The empty-Arrow-file size under the Rust `arrow` writer.** 434 B is pyarrow's; the Rust writer's
  framing may differ by tens of bytes. It does not move the block-quantum conclusion.
- **A measured ingest-path disk figure at any scale above 250M rows.** The one campaign
  (`2026-08-05-write-path-at-scale`) stops at 250M + 50M and ran before the fold existed, so its
  1.32–1.59× is the unreclaimed shape and not a steady state under compaction. Nobody has measured
  the steady state with folds running.
