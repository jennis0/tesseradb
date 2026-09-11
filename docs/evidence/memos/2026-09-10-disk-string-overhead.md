# What a string value costs, on every route, at build and in the bundle

**Status:** Fact-find, 2026-09-10, for the disk-use campaign. **Not normative, and nothing here is
decided.** Produced by a subagent reading main at 0c0b60b3 as one of eight parallel investigations;
it is that agent's read of the code, not a reviewed design. The owner's session independently
verified that `StringColumn::get` is the only reader of the offset array and the walked-header read is refused for a `keyword`; and that `EntityColumn::prose`'s presence bitmap is never marked while `MappedArray::zeroed` reserves its blocks. **Every other claim here is unverified** — re-check a
figure or a citation before relying on it.

Measurement tooling for figures the report marks as taken in its own session is in
[`probes/2026-09-10-disk-survey/`](../../../probes/2026-09-10-disk-survey/). The campaign's
starting point is [`2026-09-10-build-disk-weight.md`](2026-09-10-build-disk-weight.md), which
these supersede in the places they contradict it.

The other fact-finds in this set are the sibling `2026-09-10-disk-*.md` memos in this
directory.

**Date** 2026-09-10. **Tree** main at 0c0b60b3. Read-only survey; nothing changed.

⊘ **Scope fence.** Routing a blob-resident `keyword` the way a `text` column goes is in flight on
`perf/blob-resident-strings` and is assumed to land. Its route is priced below for completeness and
excluded from the ranking in §7.

Every figure is marked **measured**, **modelled** or **assumed**. Measured build figures come from
`probes/2026-09-10-build-disk/200m-after.files.tsv` at 125,789,091 GBIF occurrences; measured bundle
figures from `du` over `data/rung4-run/bundle/v00000` at 102,117,343 items and from the same probe's
bundle files. Rung 6 is 3,495,729,729 items and **nothing has ever been built at that size**, so
every rung-6 number here is a per-item figure extrapolated.

---

## 1. The result

Fixed cost means everything but the value's own characters. `n` is the item count; "per value" is per
*present* value.

| route | build, over `n` | build, per value | bundle, over `n` | bundle, per value | build floor | gap |
|---|---|---|---|---|---|---|
| `keyword`/`utf8`, no flags, entity scope — **blob-resident** | 8.125 B | 4 B | 4.13 B (blob directory) | 15 B, then zstd | **0** over `n` | 8.125 B/item + 4 B/value *(in flight)* |
| `keyword`/`utf8`, `index = true`, entity scope | 8.125 B | 4 B | 0.085 B (presence) | 4 B ordinal + ~4 B/distinct key | **0** over `n` | **8.125 B/item + 4 B/value** |
| `keyword`, `render = true` | — | — | — | — | — | refused at the declaration |
| `text`, entity scope, `index` either way | 0.125 B | 0 | 4.13 B (blob directory) | 15 B, then zstd | **0** | **0.125 B/item** (a bitmap nothing marks) |
| `keyword`/`utf8`, group-scoped | 8.125 B **per view** | 4 B | 0.085 B per view | 4 B ordinal | **0** over `n` | 8.125 B/item × views + 4 B/value |
| `text`, group-scoped (`index` forced) | 8.125 B **per view** | 8 B | — (no blob row) | — | 0.125 B + a has-row bitmap | 8.000 B/item × views |

The 8.000 B over `n` is the entity-indexed offset array (`column-*.at`); the 0.125 B is the presence
bitmap; the 4 or 8 B per value is the arena record header.

**The bundle already declines everything the build spends.** A keyword's finished form is a `u32`
ordinal per present row and a front-coded dictionary — no offset array, no arena, no per-value
length. The build spends 12.125 B/item to produce a form that spends 4.

**At rung 6, the GBIF schema's two `keyword` columns hold 83.7 GB of fixed overhead against 134.4 GB
of characters** — 38% of the columns' 218 GB is not payload (modelled: the measured per-item figures
extrapolated).

---

## 2. The routes

### 2.1 What is declarable

`utf8` is refused as a declared type (`crates/tessera-build/src/config.rs:4736`); it survives as
`ScalarType::Utf8` internally and as the wire type of a keyword's value. `render = true` is refused
on `text` (`config.rs:4797`) and on `keyword` (`config.rs:4807`), so **no string column ever reaches
a hot row column**. That leaves `keyword` × `index` × scope, and `text` × `index` × scope.

Two predicates decide the rest:

- `pipeline.rs:4774` `postings_are_owed` — `index = true`, or a `derived` vocabulary.
- `pipeline.rs:3125` `blob_resident` — `text`, or (not rendered and no postings owed).

### 2.2 The build's string column

`crates/tessera-build/src/column.rs:276` `EntityColumn::filled` creates three files for a string
column, all sized over `n` at creation:

| file | bytes | what it is |
|---|---|---|
| `column-<k>.at` | `8 × n` | `MappedArray<u64>`, entity-indexed offset into the arena |
| `column-<k>.arena` | payload + header × present + one growth step | `MappedArena`, records in **arrival** order |
| `column-<k>.present` | `n/8` | `MappedArray<u64>` presence bits |

Every one is `posix_fallocate`d, not left sparse (`spill.rs:57` `reserve`), so the blocks are disk
the build has taken. Measured at 125,789,091 items: `.at` 8.001 B/item, `.present` 0.125 B/item.

The record layout is `column.rs:218`/`:231`:

- `RECORD_HEADER_WALKED = 8` — entity `u32` then length `u32`. `text` only.
- `RECORD_HEADER_INDEXED = 4` — length `u32` alone. `keyword`, `utf8`.

`record_header` (`column.rs:234`) reads the declared type, so the two cannot drift per call site.

A **`text` column entity-scoped** gets `EntityColumn::prose` instead (`column.rs:323`): no arena, no
offset array, a presence bitmap and nothing else. Its prose is spilled as record-blob extents by the
join (`build-column-extents.md`).

A **group-scoped** column is one `EntityColumn::filled(…, n)` **per view of the group**
(`pipeline.rs:2780`), so the per-`n` structures multiply by the view count.

### 2.3 What each part is read by

| part | reader | access pattern | citation |
|---|---|---|---|
| `.at` | `StringColumn::get(entity)`, through `str_at`/`value_at` | random by entity as written; **every production caller iterates entity ascending** | `column.rs:731` |
| `.at` | `for_each_record_in`'s liveness check | genuinely random by entity | `column.rs:496` |
| record length | `StringColumn::get` | at `offset + header − 4` | `column.rs:732` |
| record entity | `for_each_record_in` | walked route only | `column.rs:485` |
| `.present` | `present_entities`, `is_present`, the `Presence` sweep | sequential | `column.rs:426`, `pipeline.rs:3462` |

Production callers of a **`keyword`/`utf8`** column, exhaustively:

1. `for_each_keyword` — the keyword dictionary pass. `present_entities()` ascending, then
   `str_at(entity)`. One pass. `pipeline.rs:3453`, called from `write_keyword_column`
   (`pipeline.rs:3733`).
2. `ColumnRows::next_row` — the record blob merge. `self.entity` ascending from 0.
   `pipeline.rs:3146`, into `record_value_of` (`pipeline.rs:3181`).
3. `take_from` — the attribute join's move from the staging buffer. Write side.
   `pipeline.rs:2314`, `column.rs:524`.

A column is exactly one of (1) or (2): `blob_resident` and `postings_are_owed` are complementary for
this family. After its reader it is released — a non-blob column at the end of the filter postings
(`pipeline.rs:1719`), a blob column at the release stage (`pipeline.rs:1759`).

The **walked** route (`for_each_record_in`, `column.rs:460`) refuses a column whose header is
`RECORD_HEADER_INDEXED`, and `TextValues::Arena` is constructed at exactly one production site —
`pipeline.rs:2540`, the group-scoped text index. Everything else at `TextValues::Arena` is a test.

---

## 3. The offset array

**Where it is written.** `StringColumn::set` (`column.rs:726`): `self.at.as_mut_slice()[entity] =
offset`. The join resolves a source chunk to entities, sorts the chunk ascending
(`build-column-extents.md` §2), then scatters — so writes ascend within a chunk but every chunk
touches the whole array.

**Where it is read.** `StringColumn::get` (`column.rs:731`) and nowhere else, for `keyword`/`utf8`.
`for_each_record_in`'s liveness check (`column.rs:496`) is the one random-by-entity read and it is
unreachable for this family.

**So every reader is sequential in entity order.** The array is not serving random access. It is
serving a *reorder*: the arena is in arrival order (source-file order), entity order is
signature-then-Morton, and `at` is the permutation between them. The random access it pays for is on
the **arena** side — `get(entity)` reaches a random offset per value — which is exactly the access
pattern the text route was rewritten to remove.

**Which the memo suspects it is.** It is the same materialisation the `text` route declines. The
text route's join already sorts each chunk by entity and writes it as an extent; the record blob then
k-way-merges the extents (`build-column-extents.md` §2), and the token index reads block ranges. Both
consumers get `(entity, value)` in the order they want with **no per-`n` structure at all**. The
keyword routes' consumers want the same thing and have the same producer.

Two facts strengthen it:

- **The flush already does this.** `write_filter_extents` (`crates/tessera-engine/src/flush.rs:1517`)
  builds a keyword extent from `Vec<&str>` over one bounded batch — no arena, no offset array,
  nothing indexed by entity. Under decision 0091 the build is ingest into an empty database, and here
  the build is the outlier.
- **The bundle already does this.** The finished keyword column is a `u32` ordinal per present row
  and a dictionary. There is no offset array in the artefact.

**The counter-case.** A group-scoped `text` column is walked in arena order and needs `at[entity]` at
random to test whether a record is still live (`column.rs:496`). That route owes the array as written.
The extent route answers the same question with per-extent has-row bitmap arithmetic
(`build-column-extents.md`, "An entity written twice"), so even there it is a choice rather than a
necessity.

**One unmodelled term the offset array's sibling carries.** `MappedArena` pushes a record-start mark
every 32 MiB *or every 64 records*, whichever comes first (`spill.rs:294`, `:326`, `:374`). On short
values the record rule always fires, so a keyword column holds `8 × present/64` bytes of heap `Vec`:
**840 MB across the GBIF rung's two columns** (modelled). The marks feed `windows()`, which only the
walked route consumes. `residency.rs` carries no term for them.

---

## 4. The record header's length

**Is the length derivable from the offsets?** No, as the arena stands. `at` is indexed by entity and
the arena is in arrival order, so `at[e]` and `at[e+1]` name records that are not adjacent. Deriving a
length from a neighbouring offset needs the offsets sorted in *arena* order, which is a second array.

**What reads it.** `StringColumn::get` (`column.rs:732`), at `offset + header − 4`, on every
`str_at`/`value_at`. It is reachable **only through the offset array** for `keyword`/`utf8`: the
arena walk that would read it from a scan is refused for that header (`column.rs:469`), and
`arena_windows` is consumed at one production site, the group-scoped text index (`pipeline.rs:2540`).

**Is a 0-byte header possible for a route that has offsets? Yes, and cheaply.** Pack the length into
the offset word — 40 bits of offset and 24 of length in the existing `u64` — and the header
disappears with no change to the arena's order and no new file.

- Ceilings: 1 TiB of arena, 16 MiB per value. `scientificname`'s arena at rung 6 is 111 GB
  (modelled); the `u32` length refusal at `column.rs:706` already caps a value at 4 GiB and would
  tighten to 16 MiB.
- What it breaks: nothing on the indexed route. `get` is the only reader. `for_each_record_in` and
  `windows` are already refused or unreached for it.
- What it does **not** reach: the walked (`text`) route, whose header must stay 8 bytes — the walk
  discovers a record's entity and length from the record itself, having no index to consult.
- Worth: **4 B per present value**, 26.9 GB at rung 6 across the two GBIF keyword columns (modelled).

The other route to a 0-byte header is an entity-ordered arena with a prefix-sum offset array, so
`at[next present] − at[e]` is the length. That is the two-pass fill withdrawn on 2026-09-04
(`build-column-extents.md` §6) and it costs a second decode of the source.

---

## 5. The presence bitmap

**Cost.** `n/8` bytes — 0.125 B/item, measured. Allocated at `column.rs:305` over
`n.div_ceil(64)` `u64` words, `posix_fallocate`d like the rest. At rung 6, **437 MB per column**
(modelled).

**Is it derivable from the offset array?** In principle, with a reserved sentinel offset — offset 0
is a legal record offset today, so the arena would have to burn a byte. Three reasons it is the wrong
thing to remove:

1. **It is 64× cheaper to scan.** `present_entities` skips an absent run 64 entities at a time over
   0.125 B/item; the same sweep over `at` reads 8 B/item. The bitmap is the structure that would let
   `at` go, not the other way round.
2. **The staging buffer needs an out-of-band clear.** `set` with `Null` clears the bit and leaves the
   data alone (`column.rs:365`); `reset_staging` clears the arena and not `at` (`column.rs:617`).
   Sentinel presence would need a write per absent staged slot.
3. **A `text` column has a presence bitmap and no arena at all** (`column.rs:323`), so for that route
   there is nothing to derive it from.

**But route 4 pays for a bitmap nothing uses.** `EntityColumn::prose` allocates `n/8` bytes and
*nothing ever marks a bit* — the join's prose lane writes extents and never touches the home column
(`pipeline.rs:2318`), and `write_record_blob` excludes `text` from `column_tags`
(`pipeline.rs:3070`). It is read only to answer "absent" at every entity. **437 MB per text column at
rung 6, reserved and never written** (modelled; the mechanism is measured at `column-*.present`
0.125 B/item). `residency.rs:509` charges it, so the forecast is right and the disk is wasted.

---

## 6. The bundle

All of §2–§5 is `.build-tmp/` scratch. What serving pays forever is different, and much smaller.

### 6.1 Measured — rung 4, `data/rung4-run/bundle/v00000`, 102,117,343 items

Schema: `publication_year` i32 index+render, `type` category render, `is_oa` bool render,
`openalex_id` **keyword index**, `title` **text index**, `abstract` **text index**.

| file | bytes | B/item | route |
|---|---|---|---|
| `attrs/openalex_id/values.arrow` | 421,234,618 | **4.125** | keyword ordinals, one `u32` per present row |
| `attrs/openalex_id/dict.bin` | 494,766,066 | **4.845** | front-coded dictionary; keys are near-unique here |
| `attrs/openalex_id/presence.roaring` | — | 0 | **not written**: every entity carries a value |
| `attrs/record/blocks.bin` | 44,336,891,609 | 434.176 | title + abstract prose, zstd |
| `attrs/record/directory.arrow` | 437,460,770 | **4.284** | block locator + within-block row offsets |
| `attrs/record/hasrow.roaring` | 22,025 | 0.0002 | one run |
| `attrs/abstract/postings.arrow` | 17,257,806,322 | 169.0 | token index |
| `attrs/abstract/dict.bin` | 349,868,873 | 3.426 | term dictionary |
| `attrs/title/postings.arrow` | 2,448,103,794 | 23.973 | token index |
| `attrs/title/dict.bin` | 55,351,533 | 0.542 | term dictionary |

### 6.2 Measured — the GBIF prefix, 125,789,091 items (probe TSV)

Schema: `kingdom` category render, `specieskey` **keyword index**, `year` u16 index,
`scientificname` **keyword, no flags** (blob-resident).

| file | B/item | route |
|---|---|---|
| `attrs/specieskey/values.arrow` | **3.886** | ordinals, ~4.2 B per present row at 92.1% presence |
| `attrs/specieskey/dict.bin` | 0.018 | 1.4×10⁶ distinct keys over the whole corpus |
| `attrs/specieskey/presence.roaring` | 0.085 | partial presence |
| `attrs/record/blocks.bin` | **8.202** | `scientificname` alone, zstd |
| `attrs/record/directory.arrow` | **4.131** | one `u32` per has-row entity |
| `attrs/record/hasrow.roaring` | 0.0002 | one run |

### 6.3 What the bundle's fixed cost is, per route

- **Indexed keyword.** One `u32` ordinal per present row (`values.arrow`), a front-coded dictionary
  per distinct key (`dict.bin`, format at `crates/tessera-filter/src/dict.rs:36`), and a Roaring
  presence bitmap where presence is partial — omitted entirely where it is universal
  (`Presence::written`, `pipeline.rs:3418`). **4 B/present value + the dictionary.**
- **Blob-resident string.** No `attrs/<column>/` at all. ⊘ The row framing below was replaced on
  2026-09-11 ([decision 0141](../../decisions/0141-the-record-blob-states-identity-once-per-block.md));
  the figure is what the ruling was taken against. In the blob: 8 B row header + 2 B tag +
  1 B kind + 4 B length + characters, all zstd-compressed (`crates/tessera-filter/src/record.rs:200`,
  `:209`); plus **4 B per has-row entity, uncompressed**, in `directory.arrow` — an Arrow IPC file
  written with no compression (`crates/tessera-filter-write/src/record.rs:231`). The 15 B of
  uncompressed row framing collapses with the characters: measured 8.202 B/item of blocks against
  46.2 B/item of framing plus characters, a 5.6× ratio on `scientificname`'s repetitive values, and
  0.26 of the characters alone.
- **Text.** The same blob row, plus `dict.bin` and `postings.arrow` where indexed. The token index is
  per term occurrence, not per value, and dominates: 169 B/item on abstracts (measured).
- **Group-scoped.** The same files, one set per view under `attrs/<column>/<group>/<key>/`, and
  **no blob row** for any of them (`views.md` §5, `pipeline.rs:2519`).

### 6.4 Modelled — rung 6, the two keyword columns

Per-item figures from §6.2 scaled to 3,495,729,729 items.

| | GB |
|---|---|
| `specieskey` `values.arrow` | 13.58 |
| `specieskey` `presence.roaring` | 0.30 |
| `specieskey` `dict.bin` | ~0.005 |
| `attrs/record/directory.arrow` | 14.44 |
| `attrs/record/blocks.bin` | 28.67 |
| **total** | **57.0**, of which **28.3 is fixed** (8.10 B/item) |

Against the build's 83.7 GB of fixed overhead for the same two columns, plus 134.4 GB of characters
staged in arenas.

---

## 7. The floor, and the gaps at rung 6

`n = 3,495,729,729`. `specieskey` present = 3,221,136,771 (92.14%, from
`data/ladder/gbif/manifest.json`); `scientificname` present = `n`. All rung-6 figures **modelled**.

| rank | gap | bytes at rung 6 | route | is it reducible? |
|---|---|---|---|---|
| 1 | `.at`, `scientificname` | **27.97 GB** | blob-resident keyword | *in flight* — the extent route removes it whole |
| 2 | `.at`, `specieskey` | **27.97 GB** | indexed keyword | yes — the only reader is one ascending pass |
| 3 | arena record header, `scientificname` | **13.98 GB** | blob-resident keyword | *in flight*; otherwise packable into the offset word |
| 4 | arena record header, `specieskey` | **12.88 GB** | indexed keyword | yes — pack 40-bit offset + 24-bit length |
| 5 | arena marks, both columns | **0.84 GB** heap | any arena-backed keyword | yes — only the walked route consumes `windows()`; unmodelled by `residency.rs` |
| 6 | `.present`, both columns | **0.87 GB** | every route | no, and it is what would let `.at` go |
| 7 | a `text` column's never-marked `.present` | 0.44 GB per text column | entity-scoped text | yes, outright — no writer and no reader |

**After the in-flight branch lands, items 2, 4, 5 and 7 remain: 41.7 GB of build disk and 0.42 GB of
heap on the GBIF schema.** Item 2 alone is 27.97 GB — larger than the sorted source-id array the memo
puts first (26.7 GB), and on the same argument.

**Not gaps.** These are floor, and worth stating so nobody re-opens them:

- The bundle's `4 B/present row` keyword ordinal. The masked scan reads it positionally
  (`crates/tessera-filter/src/values.rs`); narrowing it to the 21 bits GBIF's 1.4×10⁶ keys need would
  save 4.7 GB and cost the scan its fixed-width step. Records §4.3 specifies `u32`.
- The blob's `4 B per has-row entity` directory. Rows are variable-length and a drill-down is one
  random row read, so an offset per row is owed. ⊘ It is stored uncompressed and delta-coding it
  within a block would roughly halve it — 7 GB at rung 6 — at the cost of decoding one block's offset
  list per read, which is already dwarfed by the block decompress. **Modelled, unmeasured, and it
  changes a fail-closed reader.** Not proposed.
- The `text` route's per-value blob framing (15 B before zstd). It is the record format, shared with
  every family, and it compresses with the characters.

**The one-line statement of the floor.** For every string route whose readers are sequential in
entity order — which is every route but group-scoped `text` — the irreducible per-item cost over `n`
is **zero**, and the irreducible per-value cost is a self-delimiting length in a chunk-sorted stream.
Today those routes spend **8.125 B/item over `n` and 4 B per value**. The producer that would deliver
the stream already exists: the join sorts every chunk by entity before it scatters, which is the same
sort the extent route writes out.
