# The build's disk forecast, its refusal, and what each loose term is worth

**Status:** Fact-find, 2026-09-10, for the disk-use campaign. **Not normative, and nothing here is
decided.** Produced by a subagent reading main at 0c0b60b3 as one of eight parallel investigations;
it is that agent's read of the code, not a reviewed design. The owner's session independently
verified `residency.rs`'s blob-branch `continue` leaves an indexed `text` column's token index and dictionary charged nothing, and the row-column lane term is `4 * n * views * levels` with no dependence on the lane kind. **Every other claim here is unverified** — re-check a
figure or a citation before relying on it.

Measurement tooling for figures the report marks as taken in its own session is in
[`probes/2026-09-10-disk-survey/`](../../../probes/2026-09-10-disk-survey/). The campaign's
starting point is [`2026-09-10-build-disk-weight.md`](2026-09-10-build-disk-weight.md), which
these supersede in the places they contradict it.

The other fact-finds in this set are the sibling `2026-09-10-disk-*.md` memos in this
directory.

**Date** 2026-09-10. **Branch** main at 0c0b60b3. Read-only survey; nothing was changed.
Every figure is marked **measured**, **modelled** or **assumed**. Measurements taken in this
session are marked *(this session)* and name the file they came from.

## The results

**1. The forecast is not a ceiling. Seven of its terms can be exceeded, and five of them are
exceeded by corpora already in the tree.** The memo names three loose ceilings; the model has
twenty-five terms, and the direction of error is not uniform.

| exceeded term | charged | measured actual | over |
|---|---|---|---|
| the external-id sidecar and its locator, 12 B/item | 12.00 B/item | **20.25 B/item** | 1.69× |
| the artifact pass's row-column lanes, 4 B/item a level | 8.00 B/item | **96.35 B/item** | 12.0× |
| `postings.arrow`, 4 B/pair | 4.00 B/pair | **4.91–5.22 B/pair** | 1.23–1.31× |
| the record blob's blocks, half the characters | 0.500 | **0.567–0.750** | 1.13–1.50× |
| `pairs.parquet`, 4 B/pair | 4.00 B/pair | **4.18 B/pair** | 1.04× |
| a published member entry, 3 B | 3.00 B | **5.93–14.0 B** | 2.0–4.7× |
| each view's segment, permutation and row→entity files | 25.00 B/item | **25.375 B/item** | 1.015× |

**2. Whole bundle files carry no term at all.** An indexed `text` column's `postings.arrow` and
`dict.bin`, and an indexed `keyword` column's `dict.bin`, are in the bundle from the index phase
to the end of the build and are charged nothing. Measured on `medcpt-10m-abs` at 10⁷ items:
**176.81 B/item unmodelled**, against a 770.55 B/item partition — 23% of the finished bundle,
before the row-column lane's own 88 B/item of excess. *(this session)*

**3. What holds the ceiling up on the corpus it was fitted to is one over-charge covering
several under-charges.** At 125,789,091 GBIF occurrences the postings term over-charges by
1,005 MB and the external-id term (when minted) under-charges by 1,038 MB. The 1.33× margin is
a net, not a floor.

**4. A wrong refusal costs the operator the pre-flight's own runtime and offers no way out.**
There is no override — no flag, no environment variable, no configuration key; `tessera-build`
reads no environment variable at all. The refusal fires after pass one and the dictionary, which
is 11% of a build's wall clock on the measured corpus (36.9 s of 327 s) and about an hour at
rung 6 (modelled). The band of free space in which a fitting build is refused is 33–68% wide.

**5. A wrong admission costs the whole build and leaves the partial bundle on the disk.** It is
an ordinary `ENOSPC` naming the file, not a signal: `posix_fallocate` reserves blocks up front
(`spill.rs:37`). `.build-tmp/` is swept on `Drop`; `<out>/v00000/` is not, so a retry starts with
less free space than the first attempt had. Rung 6 died at hour three with 93 GB of bundle
written. No `CURRENT` is written, nothing is served, nothing is published.

**6. Ingest has no disk forecast.** The only two `statvfs` pre-flights in the tree are the
build's (`pipeline.rs:584`) and the compaction fold's (`write.rs:5476`, `compact.rs:755`). The
ingest path — `Command::Ingest`, the WAL append, the flush that publishes a segment — has none.

---

## 1. The model, read out

Written by `residency::disk` (`crates/tessera-build/src/residency.rs:678`), which starts from
the mapped terms of `residency::model` → `entity_order_residency` (`residency.rs:468`) and adds
everything outside that window. `plan_build` takes the largest of six phases
(`pipeline.rs:783–784`). `n` = items, `p` = `pair_rows` (the access relation's **pre-dedup** row
count), `views` = declared views, `levels` = declared levels summed over layers.

### Terms carried from `entity_order_residency`

| # | term | charge | denominator | phases | basis |
|---|---|---|---|---|---|
| 1 | the sorted source ids (`residency.rs:486`) | `8 × n` | items | spill, band, join | **exact** — one `u64` an item. Measured `source-ids.u64` = 1,006,312,728 = 8 × 125,789,091 |
| 2 | the ordinal→entity map (`:496`) | `4 × n` | items | band onwards | **exact** — one `u32` an item. Measured `entity-of-ordinal.u32` = 503,156,364 = 4n |
| 3a | a fixed-width declared column (`:521–525`, push `:527`) | `width × n + n/8` | items | its last reader's phase (`:638–645`) | **exact** for the slot; the presence bit is a **ceiling** (a Roaring bitmap's own worst case). Measured `column-5.col` = 251,578,182 = 2n for a `u16` |
| 3b | a `keyword`/`utf8` declared column (`:509–520`) | `8n + n/8 + arena_capacity(payload + 4n)` | items and sampled characters | as above | **modelled**: `payload` is sampled (§"Free space" below); `arena_capacity` (`:372`) adds one `ARENA_GROWTH_STEP` = 256 MiB (`spill.rs:310`), a **stated over-charge** bounded at 256 MiB a column |
| 3c | a `text` declared column (`:508`) | `n/8 + payload / EXTENT_SHARE` | sampled characters | as above | **stated ceiling.** `EXTENT_SHARE = 2` (`:347`). Measured 0.345 on PaperSeek prose; **exceeded** at 0.567–0.750 on high-entropy values *(this session)* |
| 4 | the text index's sorted runs (`:560–571`) | the column's own storage (its `payload` for a `text` column) | as the column | index | **stated ceiling, marked loose in the code**: 555 MB measured against 2.58 GB charged on 7.4×10⁷ Overture names |
| 5 | the published memberships, packed extent (`:585–594`) | `3 × member_entries` | **key values** in the member file's footer | join onwards | **stated ceiling** (`MAPPED_BYTES_PER_MEMBER_ENTRY`, `:425`). Measured 2.24 (GBIF), 1.15 (MedCPT); **exceeded** at 5.93–14.0 for a scattered membership *(this session)* |
| 6 | the member spill's runs and its merged table (`:595–603`) | `4 × member_entries` | key values | join | **stated ceiling** (`SPILLED_BYTES_PER_MEMBER_ENTRY`, `:438`). Measured 2.9 (GBIF), 2.2 (GeoNames). LEB128 over source-id deltas, so it rises with a sparse `u64` id space |

Two terms of `entity_order_residency` are anonymous and reach the memory budget only, not the
disk: the publication's own Roaring at `4 × level_entries` (`:575–584`,
`BYTES_PER_MEMBER_ENTRY = 4`, `:407`, denominator the member file's **rows**), and `SLACK` at
64 MiB (`:114`, `:605–610`).

### Terms added by `disk`

| # | term | charge | denominator | phases | basis |
|---|---|---|---|---|---|
| 7 | the packed pair buckets (`:706–712`) | `8 × p`, only when `!bucket_in_ram` | relation rows | spill | **exact** — `ordinal << 32 \| term_id` as a `u64` |
| 8 | the first term band beside the buckets (`:714–718`) | `6p / batches` | relation rows | spill | **stated ceiling** (varint pair). Measured 2.02 B/pair (`band-0.pairs`, 254,325,481 over 125,789,091) |
| 9 | the term bands (`:719–723`) | `6 × p` | relation rows | band | same ceiling, same measurement |
| 10 | each view's geometry by ordinal (`:728–734`) | `8 × n × views` | items × views | spill onwards | **exact** — two `u32` an item a view. Measured `x-of-ordinal-0.u32` = `y-…` = 503,156,364 |
| 11 | the anchor view's Morton geometry (`:740–746`) | `8 × n`, only when `views > 1` | items | spill, band | **exact**, and conditional: a single-view build writes none (decision 0112's fallback reaches nothing) |
| 12 | the row space's geometry in entity order (`:748–753`) | `8 × n` | items | assembly | **exact** — one view at a time |
| 13 | the row-order render tail (`:754–758`, `render_tail_bytes` `:451`) | `Σ width × n` over render columns; a `bool` at `n + n/8` | items | assembly | **exact** |
| 14 | the attribute join's staging buffer, per source (`:788–796`) | `Σ (fixed_width × staged + staged/8) + payload(staged) + 256 MiB` per string column; `staged` from `pipeline::staging_rows` against `JOIN_STAGE_BYTES` = 256 MiB (`pipeline.rs:312`) | chunk rows | join | **modelled.** Measured 405 MB, flat in the corpus |
| 15 | the entity→term transpose (`:800–804`) | `4(n+1) + 4p` | items and relation rows | band onwards | **exact.** Measured `offsets.u32` = 503,156,368 = 4(n+1) and `terms.u32` = 503,156,364 = 4p |
| 16 | `postings.arrow` (`:805–809`) | `4 × p` | relation rows | band onwards | **stated ceiling — exceeded.** Measured 0.00027 B/pair on GBIF; **4.91–5.22 B/pair** on a 10⁷-term relation *(this session)* |
| 17 | `pairs.parquet` (`:811–818`), only when `emit_oracle_pairs` | `4 × p` | relation rows | band onwards | **stated ceiling — exceeded.** Measured 0.0084 B/pair on GBIF; **4.18 B/pair** on a scattered relation over a 4×10⁹ entity space *(this session, pyarrow proxy)* |
| 18 | the external-id sidecar and its locator (`:820–824`), only when `mint_external_ids` | `12 × n` | items | band onwards | **assumed — exceeded.** Measured **20.25 B/item** on both bundles that mint them: the sidecar's `external_id` is an Arrow `binary` column (4 B offset + 8 B payload) beside a `u32` entity, 16.25 B/item, plus a 4 B/item locator *(this session)* |
| 19 | an attribute's value column and presence bitmap (`:842–849`), where postings are owed and the column is not blob-resident | `width × n + n/8`; `width` = 4 for the string families, the declared width otherwise | items | index onwards | **exact** for the slot, **ceiling** for the presence bit. Measured `values.arrow` 3.82–4.13 B/item |
| 20 | the keyword dictionary's row ordinals and sorted runs (`:855–864`) | `4n + payload / EXTENT_SHARE` | items and sampled characters | index | **stated ceiling.** Measured 5.3 B/item against 8 charged |
| 21 | the record blob (`:867–877`) | `4n + blob_payload / EXTENT_SHARE` | items and sampled characters of blob-resident columns | blob onwards | **stated ceiling — exceeded.** Directory measured 4.13–4.26 B/item against 4 charged; blocks measured 0.251–0.26 of characters on GBIF, **0.567–0.750 on high-entropy values** *(this session)* |
| 22 | each view's segment, permutation and row→entity files (`:887–891`) | `(4+4+4+4+8+Σrender_width) × n × views` | items × views | assembly | **assumed — exceeded.** The charge omits `columns.arrow`'s per-column validity bitmaps, `n/8` a column. Measured 25.375 B/item against 25.000 charged, at 25,846,007 items (`gbif-64p`) and 125,789,091 (the probe) — the same 0.375 = 3 × ⅛ both times |
| 23 | the artifact pass's row-column lanes (`:898–901`) | `4 × n × views × levels` | items × views × levels | assembly | **assumed — exceeded.** Holds for a `RowMajorLabel` lane (`.tslb`, measured 1,258 MB against 1,509 MB charged). **Fails for `RowMajorList`** (`.tsll`, one list of artifact ordinals a row): measured **963,487,236 bytes = 96.35 B/item** at 10⁷ MedCPT items against 8 charged *(this session)* |

### Terms that do not exist

Measured on `medcpt-10m-abs/bundle-auto` at n = 10⁷ *(this session)*. All are bundle files,
written in the index phase and never released.

| unmodelled file | bytes | B/item |
|---|---|---|
| `attrs/abstract/postings.arrow` | 1,347,576,882 | 134.76 |
| `attrs/title/postings.arrow` | 245,693,554 | 24.57 |
| `attrs/mesh_major/postings.arrow` | 109,683,634 | 10.97 |
| `attrs/pmid/dict.bin` | 41,575,643 | 4.16 |
| `attrs/abstract/dict.bin` | 17,477,969 | 1.75 |
| `attrs/title/dict.bin` | 5,353,861 | 0.54 |
| `attrs/mesh_major/dict.bin` | 155,188 | 0.02 |
| `containment/*.tscp` | 573,140 | 0.06 |
| **total** | **1,768,089,871** | **176.81** |

The cause is `residency.rs:828–833`: a `text` column is `blob_resident` for every schema
(`pipeline.rs:3125`), so the loop `continue`s at the blob branch and never reaches the value
column or the dictionary push. A `text` column's token index and dictionary are in the bundle
whatever its blob row costs. An indexed `keyword` column reaches the push but gets a term for its
value column alone; its `dict.bin` has none.

Two smaller unmodelled scratch files appear in the probe's own listing:
`attrs/specieskey/values.arrow.spool` at 473,983,152 bytes (3.77 B/item, alive 26 s) and
`attrs/year/values.arrow.spool` at 243,206,776. Also unmodelled: `tile-index/*.tsti`
(1.49 MB on `gbif-64p`) and the artifact record extents.

---

## 2. The three loose ceilings, quantified

### `postings.arrow` at 4 B/pair — not a ceiling

`postings.arrow` is an Arrow IPC file holding one `LargeBinaryArray` row per term
(`tessera-authz/src/postings.rs:1–52`). Its size is
**`8(T+1)` offset bytes + `Σ_t (1 + payload_t)` + framing**, where a term of cardinality
`c ≤ 32` (`SMALL_TERM_THRESHOLD_DEFAULT`) writes `4c` raw `u32`s and a larger one writes
run-optimised portable Roaring.

So a tag-0 relation costs **`4 + 9/c` bytes a pair**, which exceeds 4 for every finite `c` and
reaches 13 B/pair at `c = 1`. The charge is `4p` with no term for `T`.

**Measured** *(this session)*, `probes/…/probe-suggest/{uniform,zipf}/postings.arrow`, both
10⁷ terms over 10⁸ pairs:

| relation | terms | pairs | file | B/pair | against a `4p` charge |
|---|---|---|---|---|---|
| uniform, 10 entities a term | 10,000,000 | 100,000,000 | 491,250,674 | **4.913** | **1.23×** |
| zipf | 10,000,000 | 100,000,000 | 522,288,050 | **5.223** | **1.31×** |

The arithmetic reproduces the uniform file to 0.25%: `8 × 10,000,001 + 10,000,000 × 41 =
490,000,008` against 491,250,674 measured. The zipf file's 169,039 Roaring terms cost
**4.42 B/pair** on their own — Roaring is not bounded at 4 B an element either, because a
container header is 8 bytes and a scattered term spends one per element.

GBIF is the other end of the same term: 253 access terms over 125,789,091 pairs encode to
33,714 bytes — **0.00027 B/pair, 14,900× under the charge**. That is what makes this term the
loosest in the model and, on a different relation, the one that under-reads.

**A high-cardinality access relation.** At GBIF's 3,495,729,729 items with one term an item and a
mean term cardinality of 10, the charge is 14.0 GB and the file is ~17.2 GB (modelled from the
measured 4.91 B/pair). At a mean cardinality of 1 — a per-record ACL — the charge is 14.0 GB and
the file is ~45.5 GB (modelled). `pairs.parquet` doubles both when the oracle copy is written.
The two are small against a 430 GB forecast, so this term's error does not by itself sink a
build; its significance is that a stated ceiling is stated wrongly.

### `pairs.parquet` at 4 B/pair — a ceiling that is just reached

`(entity_id u64, term_id u32)`, `DELTA_BINARY_PACKED` on both columns, Snappy, no dictionary
(`tessera-store/src/pairs.rs:57–67`). Entities ascend within a term and reset at each term
boundary, so a short-term relation makes every mini-block pay for its outlier.

**Measured** *(this session, a pyarrow proxy for the Rust writer — same encoding, compression and
schema, not the same row-group sizing; treat as an order-of-magnitude figure)*, 10⁷ pairs:

| shape | entity space | B/pair | against 4 |
|---|---|---|---|
| 1 term, 10⁷ entities | 10⁸ | 0.785 | 0.20× |
| 1,000 terms × 10,000 | 10⁸ | 2.067 | 0.52× |
| 10⁶ terms × 10 | 10⁸ | 3.486 | 0.87× |
| 10⁷ terms × 1 | 10⁸ | 3.546 | 0.89× |
| 10⁶ terms × 10 | 4×10⁹ | 4.185 | **1.05×** |
| 10⁷ terms × 1 | 4×10⁹ | 4.176 | **1.04×** |

The delta is bounded by the entity space, which is bounded by I9's 2³² ceiling, so this term
saturates at about 4.2 B/pair. It is loose (0.2×) on the shapes the ladder builds and marginally
exceeded at the corner.

### The record blob's blocks at half the characters — not a ceiling

`EXTENT_SHARE = 2` charges the blocks at half the source characters. The blocks are zstd at the
library default over a 256 KiB uncompressed target (`tessera-filter-write/src/record.rs:60`,
`tessera-filter/src/record.rs:93`). A compression ratio is not bounded below 0.5.

**Measured** *(this session, zstd level 3, 256 KiB blocks, characters only)*:

| content | share of characters | against the charged 0.500 |
|---|---|---|
| repetitive taxonomy names | 0.080 | 0.16× |
| random lowercase, 12 chars | 0.596 | **1.19×** |
| random hex, 32 chars | 0.567 | **1.13×** |
| random base64, 32 chars | 0.750 | **1.50×** |

Against the build's own figures: 0.251 measured on GBIF's `scientificname` (blocks.bin
1,031,654,690 over 3,927 MB of characters) and 0.345 on PaperSeek prose. The shape that exceeds
the ceiling is a blob-resident column of high-entropy short identifiers — a DOI, a UUID, a hash,
a base64 key. Two further under-charges compound it: the row framing the model does not count
(8 bytes a row, plus 3 bytes a field and 4 more for a `utf8` length — `tessera-filter/src/record.rs:13–21`),
which for an 8-character value is more than the value; and the directory, charged 4 B/item and
measured 4.13–4.26. The worst case is bounded — zstd does not expand incompressible input — at
about 2× the charge on characters, plus framing.

### A member entry at 3 B — not a ceiling

The packed extent is 8 bytes of offset an artifact plus one opaque blob, and the blob is portable
Roaring over the artifact's entity set (`tessera-store/src/membership.rs:30–36`).

**Measured** *(this session, pyroaring, the same portable format croaring writes)*:

| membership | span | B/entry incl. the 8 B offset | against 3 |
|---|---|---|---|
| 10⁷ members, dense over 1.2×10⁷ | 1.2×10⁷ | 0.22 | 0.07× |
| 1,000 members over 10⁶ | 10⁶ | 2.14 | 0.71× |
| 10⁷ members scattered over 4×10⁹ | 4×10⁹ | 2.05 | 0.68× |
| 10⁵ members scattered over 4×10⁹ | 4×10⁹ | **5.93** | **1.98×** |
| 1,000 members scattered over 4×10⁹ | 4×10⁹ | **9.98** | **3.33×** |
| 64 members scattered over 4×10⁹ | 4×10⁹ | **10.25** | **3.42×** |
| 4 members scattered over 4×10⁹ | 4×10⁹ | **14.00** | **4.67×** |

The build's own figures are 2.24 (GBIF, 843.8 MB over 377,367,273 entries) and 1.15 (MedCPT,
543,884,239 over 471,778,374) — both under, because entity space is Morton-ordered and both
corpora's artifacts are spatially or topically clustered, so their bitmaps come out as runs. The
shape that exceeds the charge is **many small artifacts whose members are uncorrelated with the
entity order**: a fine-grained taxonomy over a large corpus, an owner-per-record layer, a layer
over an embedding view where entity order is a k-NN layout rather than the layer's own axis. At
rung-6 scale one level of 10⁷ artifacts with 350 members each spends 3.5×10⁹ entries at, say,
6 B/entry (modelled) against 10.5 GB charged — 10.5 GB under-read on one level.

`SPILLED_BYTES_PER_MEMBER_ENTRY = 4` has the same structure and a second exposure: the runs are
LEB128 deltas over **source ids**, not entity ordinals (`spill.rs:1690–1712`), and a `u64` delta
is up to 10 bytes. GBIF's ids are a contiguous range so its deltas are one byte and the pair of
files measures 2.9 B/entry. A corpus whose source ids are hashes spread over the `u64` space
spends 9–10 bytes on every first delta of a record and on every scattered one after it —
modelled at 2–5× the charge, for the runs and the merged table both, which stand together.

---

## 3. The refusal's behaviour

`pipeline.rs:782–806`, inside `plan_build`, after the batch plan and the band boundaries.

```
let disk = crate::residency::disk(args, n, p, batches, bucket_in_ram, &payloads, &tail);
let (phase, disk_need) = disk.peak();
eprintln!("disk: ~{} MiB at peak, in the {} phase{}", …);        // always
if let Some(free) = available_disk(&args.out) {
    if free < disk_need { return Err(BuildError::Invalid(…)); }  // hard
}
```

**It is a hard refusal.** `BuildError::Invalid`, non-zero exit, no bundle. There is no warning
band (the memory model beside it has one at `pipeline.rs:650–664`: over budget refuses, within
2× warns and proceeds). There is no headroom factor either — the comparison is `free < need`,
where the fold's counterpart asks for 150% of its inputs (`compact.rs:640`).

**There is no override.** No CLI flag (`tessera-cli/src/main.rs:70–95` has
`--mint-external-ids`, `--no-oracle-pairs`, `--batch-items`, `--memory-budget` and nothing for
the disk), no configuration key, and `tessera-build` reads no environment variable anywhere. The
operator's levers are indirect: free disk, `--limit`, `--no-oracle-pairs` (removes one 4 B/pair
term), leave `--mint-external-ids` off (removes 12 B/item), drop a member source, narrow the
schema.

**It is not documented outside the code.** `docs/design/build-column-extents.md` §5 describes what
the model charges for a `text` column and says the pre-flight "is what decides whether that build
starts". No document states the refusal, its lack of an override, or the operator's remedies.

**What the operator sees.** Always, whether or not free space can be read:

```
disk: ~410639 MiB at peak, in the join phase (spill 84011 MiB) (band 140019 MiB)
      (join 410639 MiB) (index 388180 MiB) (blob 361135 MiB) (assembly 367687 MiB)
```

and on a refusal, the bytes needed, the phase, `n`, `p`, `batches`, the free bytes, then the peak
phase's mapped terms one a line, largest first, in MiB, with their own descriptions
(`Residency::describe_phase`, `residency.rs:268`). The descriptions carry the coefficient and the
denominator in words — "the sorted source ids, 8 B/item", "postings.arrow, at a ceiling of
4 B/pair", "the record blob: N MiB of directory at 4 B/item, and its blocks modelled at half the
M MiB of characters its columns carry".

**What it does not show.** Which terms are exact and which are stated ceilings; any ratio against
a measured peak; any phase but the peak one's breakdown. An operator refused for GBIF's postings
term cannot tell from the output that the term is 14,900× over on their corpus.

**Against the house rule.** `CLAUDE.md` §"How strict to be": refuse only where a change leaks or
is irreversible; everything else is recoverable, so report the numbers and let the operator
decide. A disk forecast leaks nothing — no aggregate, no mask, nothing a principal can observe —
and is not irreversible: a build that runs out of disk writes no `CURRENT`, publishes no
identity, mints no entity or term id that survives. The failure it prevents is the loss of a
build's wall clock, which is the definition of recoverable.

**So the code today is stricter than the rule.** The rule's shape for this case is the one the
memory model already uses ten lines above: refuse on what is certain, warn with the numbers on
what is modelled. What weighs the other way is the asymmetry in *when* the two errors are
discovered: a wrong refusal is known in the first minutes and a wrong admission at hour three,
and the fold's own pre-flight refuses hard on the same reasoning. **Not decided here.**

### What each error costs today

**A wrong refusal.** The build stops after pass one and the dictionary — measured at 36.9 s of a
327 s build at 125,789,091 items (11%), modelled at about an hour at rung 6. The output directory
keeps `<out>/v00000/dictionary/`; `.build-tmp/` is swept. The operator has no way to say "I
accept the risk", so the outcome is a schema change, a smaller corpus, or a bigger box. The band
in which this happens is the model's own margin: **1.33–1.68× the measured peak** at four row
counts, so a build whose real peak is X is refused on any box between X and 1.33–1.68 X. At
rung 6 the forecast is 430.6 GB and the fit puts the real peak near 360 GB (modelled), so a
380 GB box is refused for a build with 20 GB to spare.

**A wrong admission.** `posix_fallocate` fails at the file's creation or growth, and the build
returns `BuildError::io(path, ENOSPC)` naming the file (`spill.rs:37–79` — the reservation exists
so this is an error and not the SIGBUS a sparse mapping would raise). `.build-tmp/` is removed by
`TmpDir::drop`, best effort. **The partial bundle under `<out>/v00000/` is not removed**, so the
retry has less disk than the first attempt unless the operator clears it. Rung 6 died at hour
three of a ~9 h build with 93 GB of bundle written. No `CURRENT`, so nothing is served and no
identity is published; `validate_args` (`lib.rs:996`) allows a rebuild into the same directory
because `CURRENT` is absent.

---

## 4. Free space, measured how

`available_disk` (`pipeline.rs:581–597`): `statvfs` on **`args.out`**, returning
`f_bavail × f_frsize`. `f_bavail` and not `f_bfree`, so a filesystem's root-reserved blocks are
excluded — correct, and the same choice the fold makes (`write.rs:5473`). On a non-Unix target
the function returns `None` unconditionally and the pre-flight does not run.

The path always exists by then: `TmpDir::create` (`pipeline.rs:842`, `spill.rs:729`) does
`create_dir_all` on `<out>/.build-tmp` before pass one, and `plan_build` runs at
`pipeline.rs:896`. So the `None` branch is effectively non-Unix only.

**One filesystem, not two.** Scratch and output are the same tree — `.build-tmp/` is
`<out>/.build-tmp` — so there is nothing to split. Two files do escape it: `keyword-run-*.spill`
and `keyword-ordinals.scratch` live under the bundle tree rather than `.build-tmp/`, still under
`<out>`. Nothing in the model or the pre-flight reads the *inputs'* filesystem, which is a
separate device on any staging setup and is not charged — correct, since the build only reads
them.

**Not accounted for.**

- **Thin provisioning and a growing VHDX.** `statvfs` reports the guest filesystem's own free
  blocks. On the WSL2 box the measurements were taken on, `f_bavail` is the ext4 inside a VHDX
  that grows against a host volume; nothing consults the host. A forecast that fits `f_bavail`
  can still fail at the hypervisor. Same for any thin LVM or overlay.
- **Other tenants.** One reading, at one instant, hours before the peak. Nothing re-checks and
  nothing reserves. A second build, a log, or an `apt` on the same box between the reading and
  the peak is invisible. The probe notes that nothing was measured under disk pressure.
- **Compression or deduplication.** A btrfs or ZFS dataset with compression reports raw free
  blocks; the forecast charges uncompressed bytes for files that will land compressed.
- **The build's own head start is double-counted.** `source-ids.u64` (8 B/item) and the
  dictionary are already written when `statvfs` runs — the probe's sampler first sees
  `source-ids.u64` at 7.0 s and `terms-0.dict` at 36.9 s, and `band-0.pairs` only at 63.3 s,
  after the plan. `free` therefore already excludes those bytes and `disk_need` charges them
  again. 8n is 26.7 GB at rung 6, a conservative error but a real one.
- **A peak that arrives hours in.** Nothing. There is one reading, at one moment, and no
  re-check at any phase boundary. The only later defence is `posix_fallocate` failing at the file
  that cannot be reserved.

---

## 5. What a phase-aware forecast could refuse on instead

**Feasible, and most of the machinery is already there.** `Residency::at(phase)`
(`residency.rs:250`) already gives every phase's total, `describe_phase` (`:268`) already prints
one phase's terms, `Term::phases` (`:228`) already records which windows each term stands
through, and the `disk:` line already prints all six.

**What is missing is one field.** `Term` carries `bytes` and nothing about how that figure was
arrived at. Adding a lower bound — `Term::lower: u64`, or a `basis: Exact | Ceiling { measured }`
that derives one — gives `Residency::lower_at(phase)` for free, and then:

- **refuse** when `free < lower_at(phase)` for any phase: a build that cannot fit even on the
  optimistic reading;
- **warn, with both figures and the terms responsible** when `lower_at(phase) ≤ free < at(phase)`;
- **admit silently** when `free ≥ at(phase)` for every phase.

The loose terms are enumerable, which is what makes the lower bound writable rather than
guessed: `EXTENT_SHARE` at two sites (terms 3c/21 and 20), `MAPPED_BYTES_PER_MEMBER_ENTRY`,
`SPILLED_BYTES_PER_MEMBER_ENTRY`, the two 4 B/pair terms, the two 6 B/pair terms, the text-index
runs term, and `ARENA_GROWTH_STEP`'s constant over-charge. Everything else in the table is
arithmetic over a fixed width and its own lower bound.

**The question as put — "could a build that exceeds the peak forecast still be admitted if the
excess is in one phase and that phase's terms are the loose ones" — has a per-phase answer that
runs the other way from what the phrasing suggests.** A phase is a position in time. Being
wrong-admitted in the **spill** phase costs minutes; being wrong-admitted in the **assembly**
phase costs the whole build. So a rule that relaxes on late phases relaxes exactly where a
mistake is dearest. The defensible split is by **term basis**, not by phase — and the phase then
tells the operator how long they would run before finding out, which is worth printing beside
the warning.

**A second, cheaper option the phases open up.** The build already has stage boundaries
(`observer.rs`, `BuildStage`) that map onto the six phases. Re-reading `statvfs` at each boundary
and comparing against the *remaining* phases' forecasts turns one guess made before any work into
five progressively better-informed ones — and by the join boundary several of the loose terms are
no longer modelled at all: the arenas' real sizes, the band files' real sizes, the member spill's
real size and the postings' real size are all on the disk and can be `stat`ed. A build admitted
on a loose ceiling would then be stopped at the first boundary where the refined figure says it
cannot finish, having spent one phase rather than five. This needs no new field, only a re-read
and a "phases from here on" variant of `peak()`. Feasibility only; not proposed.

---

## 6. Ingest

**There is no disk forecast on the ingest path.** The whole tree has two `statvfs` calls:
`pipeline.rs:587` (the build) and `write.rs:5479` (the compaction fold, consumed by
`compact.rs:755` against `FOLD_DISC_PERCENT = 150`). `flush.rs` has no disk term of any kind, and
neither does the write path's admission control — `ingest_admission` bounds concurrency, not
bytes.

A full disk reaches ingest as an IO error after the fact: the WAL append fails and gets two
retries with backoff, one of which is there because "the other failure a retry plausibly converts
is a short-lived `ENOSPC`" (`write.rs:4500`); a flush that cannot write its segment fails the
publication.

**Under decision 0091 this is an inconsistency.** "Build is ingest into an empty database. A
feature that works at build and not at ingest is unfinished." The pre-flight is a build feature
with no ingest counterpart. It is not an equal one — a build forecasts a whole corpus from
declared inputs before touching anything, and an ingest sees a batch at a time with no way to
know what the next one brings — so the two cannot share the model. What they could share is the
*terms*: a flush's segment, its value columns, its record-blob extents and its membership extents
are the same files with the same coefficients, over the batch's rows rather than over `n`. Worth
naming as an issue rather than settling here.

---

## What is not established

⊘ **No build was run.** Every figure above comes from reading the code, from the probe's own
`.tsv` listings, from bundles already in the tree, or from a standalone measurement of the
encoder in question. The forecast itself was not re-run against any corpus.

⊘ **The `pairs.parquet` figures are a pyarrow proxy**, not the shipped `PairsParquetWriter`. Same
schema, encoding and compression; different row-group and page sizing. The Roaring figures use
pyroaring against croaring's own portable format, which is exact.

⊘ **The unmodelled-bundle total is one corpus at one scale.** `medcpt-10m-abs` at 10⁷ items with
three indexed `text` columns is the worst case in the tree for that gap; a schema with no `text`
column pays only an indexed `keyword`'s `dict.bin`.

⊘ **Whether the assembly phase's ceiling still holds on a text corpus was not computed exactly.**
A term-by-term estimate for `medcpt-10m-abs` puts the modelled assembly phase at roughly 9.5 GB
against a 7.7 GB partition — the record blob's over-charge covering the 2.7 GB of unmodelled and
under-charged terms. That is arithmetic over sampled payloads, not a measurement, and it is a
coincidence of this schema rather than a property of the model.
