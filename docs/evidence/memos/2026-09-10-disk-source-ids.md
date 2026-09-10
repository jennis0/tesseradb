# Is the sort owed? `source-ids.u64`, its consumers and its sortedness

**Status:** Fact-find, 2026-09-10, for the disk-use campaign. **Not normative, and nothing here is
decided.** Produced by a subagent reading main at 0c0b60b3 as one of eight parallel investigations;
it is that agent's read of the code, not a reviewed design. The owner's session independently
verified the contiguity branch and that the array is already dropped before `publish`; that `residency.rs` charges `8 * n` while the file is allocated at the pre-dedup per-view total; `SortRec::order`'s ordinal tiebreak; and the duplicate check's spelling. **Every other claim here is unverified** — re-check a
figure or a citation before relying on it.

Measurement tooling for figures the report marks as taken in its own session is in
[`probes/2026-09-10-disk-survey/`](../../../probes/2026-09-10-disk-survey/). The campaign's
starting point is [`2026-09-10-build-disk-weight.md`](2026-09-10-build-disk-weight.md), which
these supersede in the places they contradict it.

The other fact-finds in this set are the sibling `2026-09-10-disk-*.md` memos in this
directory.

**Date** 2026-09-10. Read-only fact-find against `main` at 0c0b60b3. Nothing changed.

## The answer

**No. The sort is not owed for the duplicate check.**

The duplicate refusal is a **distinctness** test over one view's ids
(`crates/tessera-build/src/pipeline.rs:5133-5142`). It is *spelled* as `windows(2).any(|w| w[0] ==
w[1])` on a segment that has just been sorted, but distinctness is not an ordered property: a
presence bitmap or a hash set decides it exactly. There is no global duplicate check — across views a
repeated id is the ordinary case and is collapsed by `dedup_sorted`
(`pipeline.rs:5146`, `views.md` §4 and §7), not refused. So nothing about the duplicate rule requires
`n log n` over the union, and nothing about it requires the array to survive the check.

**What the sort *is* owed for is the ordinal space itself**, and that is a stronger claim than the
duplicate check would have been. An item's ordinal is its index in the sorted, deduplicated union
(`pipeline.rs:20`, `pipeline.rs:826-830`), and the ordinal is identity-bearing three times over
(§4 below). On a **contiguous** union that index is `id - ids_first` — arithmetic, no array, no
sort. So the memo's own branch resolves the other way:

> "The sort itself may still be owed for the duplicate check — that is the thing to establish first,
> because if it is, the array exists anyway and only its retention is in question."

It is not owed for that. **The array's existence is in question, not only its retention** — and its
retention is already at its floor (unlinked at or before the layer publication, `pipeline.rs:1643`,
`1673`, `1595`; `spill.rs:232-246` unlinks on drop). There is no retention saving left to take. The
whole remaining saving is the **write**.

**But removing the array is not "skip the sort".** On the contiguous path the array is still
consulted by five merge sweeps and one ordinal walk before the layer publication ever runs
(§1). The memo's sentence — "the resolver reads only the range's first id and its length — the array
itself is never consulted" — is true of `layers::publish`'s resolver alone. Every other consumer
reads the array's contents today, and on a contiguous corpus every one of them is reading `first +
i`. Removing the array means giving `join_chunk` an id-space argument with a contiguous arm, not
deleting a sort.

---

## 1. Every consumer of `source-ids.u64` and of the sorted order

`SourceIds` is a `MappedArray<u64>` plus a post-dedup length, dereferencing to `&[u64]`
(`pipeline.rs:5049-5060`). Every consumer takes the slice; none knows it is a file.

| # | site | what it reads | array or summary? | needs it **sorted**? |
|---|---|---|---|---|
| 1 | `pipeline.rs:844`, `862` | `.len()` → `n` | **summary** (one integer) | needs the *deduplicated count*, not an order |
| 2 | `pipeline.rs:857` | `.first()`, `.last()` → `ids_first`, `ids_last` | **summary** (two integers) | reads min/max *because* it is sorted; a min/max fold answers it |
| 3 | `pipeline.rs:5237-5238` (`build_dictionary`) | `.len()` for the histogram shift and width | **summary** | no |
| 4 | `pipeline.rs:5249` (`build_dictionary` → `join_chunk`) | whole array, sequential merge sweep | **array** | **yes** — both sides must ascend |
| 5 | `pipeline.rs:400` (`resolve_pairs_chunk` → `join_chunk`), called at `939` | whole array, sequential | **array** | **yes** |
| 6 | `pipeline.rs:1032` (geometry pass → `join_chunk`) | whole array, sequential, once per view | **array** | **yes** |
| 7 | `pipeline.rs:1307` (label-disagreement refusal) | `source_ids[rec.ordinal]` | **array**, random, **error path only** | no; wants ordinal → id |
| 8 | `pipeline.rs:1506` (external-id write, only under `--mint-external-ids`) | `source_ids[ordinal]` for every ordinal | **array**, sequential ordinal walk | no; wants ordinal → id for all ordinals |
| 9 | `pipeline.rs:2246` (`read_one_attribute_source` → `join_chunk`) | whole array, sequential, once per attribute source | **array** | **yes** |
| 10 | `pipeline.rs:2794` (`read_scoped_column` → `join_chunk`) | whole array, sequential, once per (family, view) | **array** | **yes** |
| 11 | `pipeline.rs:1631`, `1653-1659` (layer publication, contiguous arm) | `.len()` and `ids_first`, captured earlier | **summary** | no — the array is dropped at `1643` *before* `publish` |
| 12 | `pipeline.rs:1665-1670` (layer publication, sparse arm) | `binary_search`, once per member entry | **array**, random | **yes** |
| 13 | `pipeline.rs:5068-5080` (`dedup_sorted`) | whole array, in place | internal to pass one | **yes** |
| 14 | `pipeline.rs:5133-5142` (duplicate refusal) | one view's segment | internal to pass one | as written, yes; **as a requirement, no** (§2) |

`join_chunk` itself is `pipeline.rs:354-381`: it sorts the chunk and merges both ascending sides in
one sweep, so its sortedness requirement is structural. The module docs state the contract in one
line (`pipeline.rs:63`): *"What the build establishes — and all it relies on — is that `source_ids`
is sorted and duplicate-free."*

**Nothing outside the build reads the file.** `source-ids.u64` lives under `<out>/.build-tmp/`,
is created by `MappedArray::zeroed` (`spill.rs:135-162`) and unlinked by `Drop` (`spill.rs:232-246`).
Grepping the workspace: the only non-`tessera-build` mentions of `source_ids` are a doc table in
`crates/tessera-bench/src/arms/ingest.rs:165` and test fixtures in `tessera-engine`'s artifact tests
(a different `source_ids`). The engine, the store, the lifecycle crate and the Python oracle never
see it.

### The disk lifetime, and why retention is already at its floor

`probes/2026-09-10-build-disk/README.md` measures `source-ids.u64` as **written in pass one, last
read at the layer publication, unlinked at the layer publication** — the "held for" column is empty,
meaning its lifetime already equals its readers'. `residency.rs:475-490` charges it to the spill,
band and join phases and not to index/blob/assembly, matching that.

---

## 2. The duplicate check

### What it is

```rust
// crates/tessera-build/src/pipeline.rs:5130-5143
let segment = &mut ids.as_mut_slice()[offset..offset + count];
anchors.push(read_source_ids_into(args, view, segment)?);
segment.par_sort_unstable();
if segment.windows(2).any(|w| w[0] == w[1]) {
    return Err(BuildError::Invalid(format!(
        "view '{}': {} contains duplicate entity_id values. A row is unique per (entity, \
         view) — the same entity in several views is the ordinary case and is several \
         files, never several rows of one (views §4)", ...
```

**Per view, over that view's own selected rows.** After every view's segment is checked, the whole
array is sorted and `dedup_sorted` collapses cross-view repeats (`pipeline.rs:5145-5147`).

### Reconciling with the probe's line

`probes/2026-09-10-build-disk/README.md` §"What was changed" 4 says *"a view's ids are distinct —
pass one checks each view's segment as it fills it"*. That is exactly the code above and there is
no tension with the memo: the check is per view, the cross-view collapse is not a check, and **no
global duplicate check exists**. The rule it enforces is `views.md` §7's *"A row is unique per
`(external_id, view)`, and the same entity in two views is the ordinary case rather than a
duplicate"*, and `views.md` §4's ingest arm (*"Known, and already in the named view: 409"*). The
build's per-view refusal is the batch-time spelling of the same rule — which is what decision 0091
("build is ingest into an empty database") requires.

### Is it owed at all, and to whom

Owed, yes — to `views.md` §4/§7 and to the permutation, which is single-valued and cannot hold two
rows for one entity in one view. But it is owed **as a distinctness test**, and the sort is only how
it is currently spelled. Alternatives that answer it exactly:

- **A presence bitmap** over the view's id span: exact, and it answers contiguity in the same pass
  (§5). Cost `span/8`, which is `n/8` only where the span is near `n`.
- **A hash set**: exact, no span dependency, but it is `~16n` bytes of anonymous memory at rung 6 —
  the exact term `probes/2026-09-10-source-ids-memory/` removed. Not viable.
- **The `mix64` anchor already computed** (`pipeline.rs:5008`; `ViewIdAnchor` at `pipeline.rs:5093-5102`): does
  **not** answer distinctness. It is an order-independent sum of a mixer, so `{a, a}` and `{b, c}`
  with `mix64(b) + mix64(c) == 2·mix64(a)` are indistinguishable to it. The memo says this and is
  right.

### The thing the sort is genuinely owed for

Not distinctness — **the ordinal space**. `ordinal = index in the sorted, deduplicated union`, and
the ordinal is identity-bearing (§4). The sort is the construction of the ordinal function, not a
check. On a contiguous union that function is `id - ids_first`, so the sort is *implied* by
non-contiguity and by nothing else.

---

## 3. The resolver

The memo's "resolver" is the `&dyn Fn(u64) -> Option<u64>` closure handed to
`crate::layers::publish` at `pipeline.rs:1646-1680`. There are two of them and the branch is at
`pipeline.rs:1632`:

```rust
let ordinals = source_ids.len() as u64;
if ids_last - ids_first + 1 == ordinals {
    drop(source_ids);                       // 1643 — the array goes BEFORE the publication
    publish(..., &|source| source.checked_sub(ids_first)
                                  .filter(|o| *o < ordinals)
                                  .map(|o| entity_of_ordinal[o as usize] as u64), ...)
} else {
    let published = publish(..., &|source| source_ids.binary_search(&source).ok()
                                  .map(|o| entity_of_ordinal[o] as u64), ...);
    drop(source_ids);                       // 1673 — after
}
```

### Its full contract

`layers.rs:1472-1483`: *"`resolve` maps a **source** entity id to the entity this build assigned
it."* Total function from `u64` to `Option<EntityId>`; `None` is a refusal at every call site, never
a drop. Two call sites and no others:

- `merge_member_runs` (`layers.rs:1767`, applied at `1787`) — **once per member entry**, in place in
  the merge's own buffer. This is the hot one: `pipeline.rs:1629-1630` notes a lineage list per point
  at the Overture rung is 3×10⁸ calls.
- `resolve_artifact` (`layers.rs:2486`, applied at `2503` via `in_place`) — inline memberships and
  generating sets.

`None` refuses the build naming the source id (`layers.rs:1787-1794`, `2495-2501`), because a dropped
member moves both a masked count and the denominator of a proportional criterion.

### Every shape it must handle, what it costs, what it consults

| shape | branch taken | consults | cost per call | correct? |
|---|---|---|---|---|
| contiguous from zero (`ids_first = 0`) | subtraction | `ids_first`, `ordinals` — **two integers**, no element | one `checked_sub` + one compare | yes |
| contiguous from `k` (`ids_first = k`) | subtraction | same two integers | same | yes — the branch tests the span, not the origin |
| sparse | binary search | the whole array, random | ~log₂n dependent cache misses (`pipeline.rs:1628-1630` puts it at ~27 at 74M) | yes |
| duplicated ids | unreachable | — | — | the array is deduplicated at `pipeline.rs:5146`, so `ordinals` is the distinct count and the branch test is over distinct ids |
| multi-segment (several views) | either | the union, after `dedup_sorted` | as above | yes — the union is one sorted array whatever the view count |
| per-view / form B | either | the union | as above | yes — a member names an entity, never a `(entity, view)` row |
| empty (`n = 0`) | sparse (`0 - 0 + 1 = 1 ≠ 0`) | empty slice | O(1) | yes; `pipeline.rs:853-856` argues the placeholder cannot take the wrong branch |

**Why the fast path is exact and not a convention** (`pipeline.rs:1622-1627`): the array is sorted
and duplicate-free, so a range spanning exactly its own length can only be `ids_first + i` at every
`i`. `crates/tessera-build/tests/build_layers.rs:498` builds the same corpus under dense and gapped
id tables and asserts the published memberships are byte-identical;
`build_layers.rs:529` asserts that an id *in a gap* refuses the build — which is the case that would
misresolve if the subtraction were used on a sparse table.

### What the memo's sentence does and does not cover

"The array itself is never consulted" is true **of this resolver only**. On a contiguous corpus the
array is still fully consulted by consumers 4, 5, 6, 8, 9 and 10 in §1's table, and their contents at
those sites are exactly `ids_first + i`. Any change that removes the array has to give those six
sites an arithmetic arm as well.

---

## 4. Sortedness elsewhere — and the identity question, plainly

### Is there an identity / entity-id-assignment dependency on the ordinal? **Yes. Three of them.**

1. **The signature sort's final tiebreak is the ordinal.** `SortRec::order()` is
   `(key_hi, key_lo, morton, ordinal)` — `pipeline.rs:200-202`. Two items with equal signature and
   equal Morton code are separated by their ordinal, and entity id = batch base + position in that
   order. A different ordinal assignment is a different permanent entity-id assignment (I9).
2. **Term ids are ranked by minimum ordinal.** `build_dictionary` keeps `(min ordinal, source_term)`
   per term (`pipeline.rs:5252-5253`) and ranks by it (`pipeline.rs:5290-5294`); the rank *is* the
   term id. A different ordinal order is a different dictionary.
3. **Batches partition ordinal space.** `pipeline.rs:1164-1168`: a batch is a contiguous ordinal
   range of `batch_items` stride, and signature order is per batch (§11.1 r23). Which items share a
   batch is a function of the ordinal.

So sortedness is identity-bearing, through the ordinal, not through the array. **A contiguity fast
path preserves all three exactly**, because `id - ids_first` is the same function as "index in the
sorted array" whenever the union is contiguous. What would break identity is a fast path taken on a
corpus that is *not* contiguous — which is why the test must be exact rather than a hash. The `mix64`
anchor cannot be that test.

### Everywhere else: no dependency

- **The manifest / provenance.** Nothing records ordinals. What is recorded and identity-bearing is
  `batch_items` (`pipeline.rs:815`, `2019`), replayed rather than re-derived.
- **The Python oracle.** `reference/oracle/bundle.py:210-240` reads the points file into
  `qx`/`qy` dicts **keyed by source id**, and reaches an entity through `permutation.bin` and the
  external-id sidecar (`bundle.py:331-337`, `546-560`). No ordinal, no sorted array, no dependency
  on pass one's construction.
- **The conformance suite.** The byte-identity oracle is the *linear* build
  (`crates/tessera-build/src/lib.rs:1100-1104`), which sorts its own points by `source_id` and makes
  its own duplicate check — single-view by refusal (`lib.rs:1051-1069`). It does not read
  `source-ids.u64`; it independently reproduces the same order. That is the contract the sortedness
  actually serves: **ordinal order must be ascending source-id order over the union**, however it is
  computed.
- **The ingest path.** `tessera_lifecycle::assign_sorted` (`crates/tessera-lifecycle/src/alloc.rs:408-433`)
  orders by `(signature_sort_key, external_id)` — no ordinal, no morton, no array. `/control/ingest`'s
  duplicate check resolves per batch against the live map plus the external-id sidecar
  (`crates/tessera-engine/src/session.rs:2981-2995`, `3090`), never a global sort.
- **Delta / flush / compaction.** `flush.rs:447`, `572-576`, `1400-1414` and `compact.rs` order by
  **entity id** throughout. Nothing reads a source-id ordinal.

**Plainly: the only identity dependency is on the ordinal function being "ascending source-id order
over the deduplicated union", and it lives entirely inside one build process.**

---

## 5. The presence-bitmap idea, assessed

### What it would cost

| | bytes | note |
|---|---|---|
| the array at rung 6 (`n = 3,495,729,729`) | 27,965,837,832 = **26,670 MiB** | modelled — arithmetic over the stated `n`; rung 6 is not built |
| a bitmap at `n/8` at rung 6 | 436,966,217 = **416.7 MiB** | modelled; a sixty-fourth, as the memo says |
| the array at 125,789,091 items | 1,006,312,728 = **959.7 MiB** | measured (the probe's own 8.00 B/item) |
| a bitmap at `n/8` there | 15,723,637 = **15.0 MiB** | modelled |

The memo's "26.7 GB" is `26,670 MiB` read as GB; the decimal figure is **27.97 GB** and the binary
one 26.04 GiB. Worth fixing if the number is quoted again.

### What it could answer

Sized over `[min, max]` and filled during the read pass, one bitmap per view:

- **per-view distinctness** — exactly (a bit already set is a duplicate). Replaces the refusal at
  `pipeline.rs:5136`.
- **contiguity of the union** — exactly (`popcount == span` and `span == n`), OR-ing the per-view
  bitmaps into a union bitmap.
- **`n`** — the union bitmap's popcount, which is what `dedup_sorted` currently produces.
- **`ids_first`, `ids_last`** — from the fill's own min/max.

That is the whole of consumers 1, 2, 3, 11 and 14 in §1's table, plus the contiguity decision.

### What it would NOT answer, and which consumers it fails

A plain bit array supports test-and-set. It does **not** support rank or select without an added
superblock index, so on the **sparse** path it answers nothing at all:

- **Fails consumers 4, 5, 6, 9, 10** (`join_chunk` at `pipeline.rs:400`, `1032`, `2246`, `2794`,
  `5249`): each needs `id → ordinal` for arbitrary ids and reads the array sequentially. A bitmap
  answers this only as `rank(id)`, which needs a rank structure and turns a sequential sweep into a
  per-row rank query.
- **Fails consumers 7 and 8** (`pipeline.rs:1307`, `1506`): both need `ordinal → id`, which is
  `select(ordinal)` — a heavier structure again.
- **Fails consumer 12** (`layers.rs:1787` on the sparse arm): same as the join sites.
- **Cannot be sized in general.** The span is `max - min + 1`, not `n`. The unit test at
  `pipeline.rs:6496` uses ids `[5, 90, 1_000_003, u64::MAX - 1]`; a bitmap over that span is 2⁶¹
  bytes. So the construction needs a gate — allocate only where `span ≤ c·n` — and outside the gate
  there is no bitmap and the array is the only route.
- **Needs min/max before it can be allocated.** `count_source_ids` (`pipeline.rs:4955-4973`) reads
  only the parquet footer row count when there is no `--limit` and no selection
  (`input::count_point_rows`, `input.rs:1284-1289`); it scans otherwise. Footer statistics would give
  min/max at no scan cost — `input.rs:1395-1417` already reads the id column's statistics for limit
  pruning, and `statistic_min` at `input.rs:1411` is the existing half. **Measured**: every ladder
  points file stores `entity_id` as physical `INT64`, so `Statistics::Int64` matches and both bounds
  are readable. Caveat: GBIF declares it `uint64` (logical `Int(64, unsigned)`), and
  `u64::try_from(*v)` returns `None` for a value above 2⁶³ — a bound the statistics cannot give, so
  the gate must fall back to "no fast path" rather than to a wrong span.

**So the bitmap is a decision procedure, not a replacement.** It buys the right to *not build* the
array on the contiguous path. It buys nothing on the sparse path and on a sparse corpus it costs more
than the array it fails to replace (§6).

### Writing or retention? Writing. Retention is already gone.

- **Retention** — `MappedArray::Drop` unlinks the file at once (`spill.rs:232-246`), and the drop
  sits at `pipeline.rs:1643` (contiguous, before `publish`), `1673` (sparse, after) or `1595` (no
  layers). `probes/2026-09-10-build-disk/README.md`'s lifetime table shows `source-ids.u64` with an
  empty "held for" column: its lifetime already equals its readers'. **Nothing left to take.**
- **Writing** — `MappedArray::zeroed` `posix_fallocate`s the whole extent up front
  (`spill.rs:149-152`); the fill dirties every page (`read_source_ids_into`, `pipeline.rs:4985-5027`); the per-segment and
  whole-array `par_sort_unstable` dirty them again (`pipeline.rs:5131`, `5145`). `dedup_sorted` is
  the one part that writes nothing on a distinct input (`pipeline.rs:5068-5080`). So the write is
  the entire remaining saving.

### What the write is worth at rung 6 — and it is not 26,670 MiB

The array is charged to the spill, band and join phases only (`residency.rs:487`). Taking
`probes/2026-09-10-build-disk/README.md`'s own modelled rung-6 phases and removing 26,670 MiB from
those three:

| phase | modelled now (MiB) | without the array (MiB) |
|---|---|---|
| spill | 84,011 | 57,341 |
| band | 140,019 | 113,349 |
| **join** | **410,639** ← peak | 383,969 |
| **index** | 388,180 | **388,180** ← new peak |
| blob | 361,135 | 361,135 |
| assembly | 367,687 | 367,687 |

**The forecast peak falls from 410,639 to 388,180 MiB — 22,459 MiB, 23.5 GB, 5.5%** (modelled,
derived from the probe's own modelled table). The index phase becomes the binding one. The other
26,670 MiB of relief lands in phases that were never the peak.

**At the largest measured scale the peak does not move at all.** The probe measures join 12.39 GB
and index 12.70 GB at 125,789,091 items; removing 1.01 GB from the join gives 11.38 against an
unchanged 12.70. The saving there is 1.01 GB of allocated blocks standing through the spill, band
and join phases, plus 1.01 GB of writes, plus the sort — not a smaller peak.

That is the honest shape of the prize: **a fifth of what the memo's headline figure suggests at the
forecast, and none of it at the scale anyone has measured.** It is still real — 28 GB of writes and
one `n log n` over 3.5×10⁹ `u64`s removed — but it should be argued as write volume and pass-one wall
clock, not as peak disk.

⊘ **One more thing the pre-flight cannot do about it**: the disk pre-flight runs inside `plan_build`
(`pipeline.rs:778-806`), which is stage 2b — *after* pass one has already written the array. So the
8n bytes are on the disk before the refusal that would name them can run.

---

## 6. The `multiview` corpus

`data/ladder/multiview/` — ten row spaces over one entity space (`corpus.toml`): `world`,
`world_flat` (same points file, a second frame), `quarter`'s four inline views, and `quarter_alt`'s
four selected out of one file by a `quarter` discriminator (form B).

**Measured today, from the parquet files:**

| view source | rows selected | distinct | min | max | span |
|---|---|---|---|---|---|
| `world.parquet` (×2 views) | 12,816 | 12,816 | 364 | 13,463,610 | 13,463,247 |
| `quarter-2026-Q1..Q4` | 8,251 / 8,190 / 8,231 / 8,161 | all distinct | 364–2,137 | 13,463,610 | ~13.46×10⁶ |
| `quarter-alt.parquet`, per discriminator | 8,251 / 8,190 / 8,231 / 8,161 | all distinct | 364–2,137 | 13,463,610 | ~13.46×10⁶ |
| **union** | **21,300** | 21,300 | 364 | 13,463,610 | **13,463,247** |

The span is **632× the item count**. Every other ladder corpus's union is contiguous from zero —
measured from parquet footer statistics: `arxiv` 2,422,486, `gbif-64p` 25,846,007, `gbif`
3,495,729,729, `geonames` 13,463,857, `medcpt` 35,920,666, `medcpt-1m`/`-1m-abs`/`paperseek-1m`
1,000,000, `medcpt-10m-abs` 10,000,000, `overture` 73,631,092, `treeoflife` 233,055,986. Note that
`treeoflife`'s **second** view (`points-geo.parquet`, 176,899,537 rows over min 10,395,243, max
233,055,985) is sparse, but the *union* with `points.parquet` is contiguous from zero — so the fast
path applies to it. Also measured: `--limit` selects `entity_id < limit` (`input.rs:22`, `181`), so
a gbif prefix stays contiguous.

**What a contiguity fast path would cost `multiview`, and would it be correct:**

- **Correct: yes.** The span test fails (`13,463,247 ≠ 21,300`), so the corpus takes the existing
  sparse path unchanged — which is exactly what it does today. The fast path is an added branch it
  never enters.
- **Cost: the bitmap is bigger than the array it fails to replace.** A bitmap over the union span is
  13,463,247 bits = **1,682,906 bytes**. The array it would be deciding about is 21,300 × 8 =
  **170,400 bytes** of live slice, in a file of 91,298 × 8 = **730,384 bytes** (the array is
  allocated at the *pre-dedup* total, `pipeline.rs:5127`). So the bitmap is **9.9× the slice and 2.3×
  the file** — and after paying it the build still writes the array. Measured (row counts and spans
  from the files today; the byte figures are arithmetic over them).
- Per-view bitmaps make it worse: ten views, each spanning ~13.46×10⁶, is 16.8 MB of bitmap against
  0.73 MB of array, unless one bitmap is cleared and reused ten times — which is ten span-sized
  clears.

This is the case that decides the gate: a fast path predicated on `span ≈ n` must be gated on
`span ≤ c·n` and must pay nothing when the gate fails. A bitmap sized before the span is known
cannot do that.

⊘ One more `multiview` fact worth recording separately: **`residency.rs` charges the source ids
`8 * n` (`residency.rs:486`) but the file is `8 × Σ(per-view selected rows)`.** For `multiview` that
is 730,384 against a charged 170,400 — **4.29×**. For `treeoflife` (233,055,986 + 176,899,537 =
409,955,523 pre-dedup rows against `n` = 233,055,986) it is 3.28 GB against a charged 1.86 GB —
**1.76×**. The model reads low for any corpus whose views overlap. It is only a reporting error
today, since the pre-flight runs after the array is written (§5).

---

## Incidental findings

Neither is in scope; both were read on the way.

1. **`pipeline.rs:1632` can overflow.** `ids_last - ids_first + 1` panics in a debug build for a
   corpus holding both id 0 and id `u64::MAX`. In release it wraps to 0, `n ≥ 1`, and the sparse path
   is taken — correct by accident. `checked_sub(...).and_then(|d| d.checked_add(1))` would make it
   deliberate.
2. **A spelling divergence between `views.md` §7 and the code.** The design says ties within a
   signature group order "by the item's Morton code in the declared anchor view, then by
   `external_id` **bytes**". `SortRec::order()` (`pipeline.rs:200`) ties on the **ordinal**, which is
   ascending *numeric* source-id order. For `u64` ids written little-endian these are different
   orders — `ExternalIdRow` byte-swaps precisely because "byte order is not numeric order"
   (`lib.rs:2341-2343`). Either the code or the sentence is wrong about which order the last tiebreak
   is; the entity-id assignment is what it decides.
