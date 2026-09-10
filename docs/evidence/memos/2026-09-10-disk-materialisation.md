# What decides that a build structure is materialised, and what a consumer-driven rule would have to read

**Status:** Fact-find, 2026-09-10, for the disk-use campaign. **Not normative, and nothing here is
decided.** Produced by a subagent reading main at 0c0b60b3 as one of eight parallel investigations;
it is that agent's read of the code, not a reviewed design. The owner's session independently
verified the bodies of `postings_are_owed` and `blob_resident`, and the single `ScalarType::Text` branch at `pipeline.rs:2079` that the report identifies as the whole inconsistency. **Every other claim here is unverified** — re-check a
figure or a citation before relying on it.

Measurement tooling for figures the report marks as taken in its own session is in
[`probes/2026-09-10-disk-survey/`](../../../probes/2026-09-10-disk-survey/). The campaign's
starting point is [`2026-09-10-build-disk-weight.md`](2026-09-10-build-disk-weight.md), which
these supersede in the places they contradict it.

The other fact-finds in this set are the sibling `2026-09-10-disk-*.md` memos in this
directory.

**Date** 2026-09-10. **Repo** `/home/joe/code/tessera` at `0c0b60b3` (main). Read-only survey; nothing
was changed and nothing was built.

Every per-item figure below is **measured** — allocated blocks from
`probes/2026-09-10-build-disk/200m-after.files.tsv`, divided by that run's 125,789,091 items. Every
rung-6 figure is **modelled**: that per-item figure times 3,495,729,729, with no allowance for the
whole corpus's 2% longer `scientificname` values. Consumer sets are read off the code and are
**measured** in the sense that they are the call sites, not an estimate.

## The result, first

1. **`postings_are_owed` is not a consumer-graph rule.** It is a two-clause disjunction over
   *declared flags* — `index`, and `visibility = "derived"` on the vocabulary the column names —
   where each clause's doc comment names the consumer that clause stands for. It never reads the
   declared type. That is the template: **a named predicate whose body is one clause per consumer,
   with the consumer named at the clause.** Generalising it does not mean computing a graph; it
   means writing more predicates of that shape and calling them where the code currently branches
   on `ScalarType`.

2. **The build has four distinct bounding mechanisms**, not one, and they are not
   interchangeable. Naming them precisely (§3): *spill-and-merge under a budget*; *extents written
   while decoding*; *page-cache substitution* (a `MappedArray`, which bounds memory and not disk);
   *release at the last reader*. Only the first two remove bytes from the disk. The third and
   fourth are what the 2026-09-09/10 work applied to `source_ids` and the ordinal→entity map, which
   is why the memo's phrase "given a bounded form" covers two different things.

3. **No reader of any string column in a build is random-by-entity.** Both readers — the keyword
   dictionary's chunk pass and the record blob's merge — walk entity space ascending, once. The
   8 B/item `at` array exists because the *arena* is in arrival order, not because a consumer wants
   random access. That is the single largest consumer-versus-type mismatch in the build.

4. **Ranked by bytes at rung 6, the structures a mechanism from §3 would already satisfy** (§4):
   a blob-resident `keyword` column's arena and offsets, **155.2 GB** (in flight, do not take it);
   an indexed `keyword` column's arena and offsets, **65.7 GB**; the source-id array on a contiguous
   corpus, **28.0 GB**; each view's ordinal geometry, **28.0 GB per view**. The first two are a
   `ScalarType::Text` branch and nothing else. The third and fourth are not type branches — they
   are producer-order problems, and their obstacle is data (item 5).

5. **The memo groups the ordinal→entity map with the ordinal geometry, and the code does not.** The
   geometry has no random reader at all; the map has two (the layer publication's per-member-entry
   resolve, and the attribute join). Only the geometry is a candidate. Of the four candidates, two
   are not decidable at plan time (§5): contiguity of the source ids, and whether a points or
   attribute source arrives in entity order. The build already has two runtime materialisation
   tests of exactly this kind (`anchor_fallback_reaches_an_item`, and the contiguity subtraction
   test at the layer publication) and one data-driven *deferral* (`Presence`, which materialises a
   bitmap only at the first absence). A consumer-driven rule cannot replace a type-driven one where
   the answer needs a pass over the data that the structure itself is the product of.

---

## 1. `postings_are_owed`, exactly

`crates/tessera-build/src/pipeline.rs:4774`

```rust
pub(crate) fn postings_are_owed(schema: &Schema, attribute: &Attribute) -> bool {
    if attribute.index { return true; }
    attribute.vocabulary.as_ref()
        .and_then(|name| schema.vocabularies.get(name))
        .is_some_and(|v| v.visibility == Visibility::Derived)
}
```

**Inputs.** Two declared facts, one of them on another object:

| input | where from | the consumer it stands for |
|---|---|---|
| `attribute.index` | the column's own declaration | the filter surface: `eq`/`in` inside a latency budget (filter-index §2.3) |
| `schema.vocabularies[attribute.vocabulary].visibility == Derived` | the *vocabulary's* declaration | `/v1/categories`: value visibility is membership-derived (per-point-attributes §3.3), and the member sets are these postings |

**Not inputs.** The declared type. `attribute.render`. Anything about the request surface at run
time, the item count, or the data.

**Shape.** A disjunction with one clause per consumer, each clause a fact stated in the declaration,
and each clause's justification written at the clause. The doc comment says the second clause "is
*not* optional" and explains why: everywhere else the postings are an accelerator, and here a
disclosure control depends on their existing. That is what makes the predicate readable as a
consumer rule rather than as a flag test.

**Call sites** (all in the build, all asking the same question of a different pass):

- `pipeline.rs:2921` — `write_filter_postings_banded` skips a column that owes none.
- `pipeline.rs:3130` — inside `blob_resident`: a column with no entity-space home is blob-resident.
- `residency.rs:637` — a `text` column earns a text index exactly where it is owed postings.
- `residency.rs:832` — the disk model charges a value column and a keyword dictionary's scratch.

### Sibling predicates of the same shape

| predicate | where | reads | decides |
|---|---|---|---|
| `scoped_postings_are_owed` | `pipeline.rs:2678` | `vocabulary.is_some()` and `ScopedScalar::licence_of(ty, vocab, index, render)` | a group-scoped category family's keyed postings |
| `ScopedScalar::licence_of` | `tessera-store/src/manifest.rs` (~line 640) | type, vocabulary-presence, `index`, `render` | `index \|\| (render && (vocabulary \|\| ty != Text))` — one body called from the build and the engine, because `check-layers.sh` denies the build the engine |
| `ScopedScalar::has_value_column` | `manifest.rs` (~658) | vocabulary-presence, type | every family but `text` gets `values.arrow` + `presence.roaring` per view |
| `blob_resident` | `pipeline.rs:3125` | type (`Text`), `render`, `postings_are_owed` | which columns the record blob reads, and when a column's storage may be released |
| `owes_value_column` / `owes_postings` / `blob_resident` | `tessera-engine/src/filter.rs` ~1539, ~1584, ~1568 | the manifest's `DeclaredScalar` | the engine's mirrors of the above; the doc at 1532 says they "must stay one" |
| `Attribute::index` / `Attribute::render` | `config.rs` ~2070–2084 | — | the flags the predicates above are built from; `render` is described there as "load-bearing, not informational" |
| `column.text_index` | `residency.rs:636` | `ty == Text && postings_are_owed(..)` | whether the model charges a text index's runs |

Four further predicates are **data-driven** rather than declaration-driven, and they are the
existing precedent for anything §5 proposes:

| predicate | where | reads | decides |
|---|---|---|---|
| `anchor_fallback_reaches_an_item` | `pipeline.rs` ~1119 (`geometry[args.anchor].rows != n`) | pass one's per-view row counts | whether `x-anchor.u32`/`y-anchor.u32` are written at all |
| the contiguity test | `pipeline.rs:1632` (`ids_last - ids_first + 1 == ordinals`) | the sorted source ids' extrema and length | whether the layer publication resolves by subtraction or by binary search — and therefore whether `source-ids.u64` is released before the publication or after it |
| `Presence` | `pipeline.rs` ~3395 | the first gap in the ascending present set | whether a presence bitmap is materialised at all; a fully-present column writes none |
| `layout.is_row_major()` + `composable` | `artifact_pass.rs` ~403–412 | the chosen serving layout and the layer's `MembershipSource` | whether a row-column (`.tslb`) or a tile index is written for a `(view, layer, level)` |

---

## 2. Every structure a build materialises

Grouped by phase. `n` is the item count, `p` the pair rows, `V` the view count, `m` the member
entries. "B/item" is measured at 125,789,091 GBIF occurrences from the probe's `after` listing;
"rung 6" is that figure times 3,495,729,729 (modelled). Files not exercised by that corpus carry
"—" and are marked below the table.

### 2a. Pass one and the dictionary

| structure | file(s) | written | size | consumers (fn, file:line) | access each needs | streamed/bounded form? |
|---|---|---|---|---|---|---|
| **sorted source ids** | `.build-tmp/source-ids.u64` | `read_source_ids_union` `pipeline.rs:5118`, pass one | `8n` (allocated at the pre-dedup length) — 8.00 B/item, **28.0 GB** | `build_dictionary` :5226 (merge sweep); `resolve_pairs_chunk` :390 → `join_chunk` :354; geometry `join_chunk` :1032; external-id write :1506 (`source_ids[ordinal]`); `read_one_attribute_source` :2246; `read_scoped_column` :2794; `layers::publish` resolve closure :1650/:1662; the label-disagreement message :1307 | **sequential** in every consumer but one: `join_chunk` sorts its chunk and sweeps. The one random reader is `publish`'s `binary_search` on the sparse path :1662 | Yes on a contiguous corpus — every use reduces to `id − ids_first` and `ids_first + ordinal`. **But contiguity is not known until the array is sorted** (§5). The per-view sort is also what the duplicate check :5134 rides on |
| **term-key / term-id arrays** | none (heap) | `build_dictionary` :5226 | `12T` where T = distinct terms | `resolve_pairs_chunk` :390, term-sorted sweep | sequential | already bounded by T, not n; dropped at :1157 |
| **pre-dedup `row_counts`** | none (heap) | `build_dictionary` | `8T` | `plan_build` :604 | sequential | as above; dropped at :1159 |
| **ordinal histogram** | none (heap) | `build_dictionary` :5237 | `8 × 2^16`, independent of n | `plan_build`'s `worst_pairs` :671 | sequential | already constant |
| **term dictionary** | `v*/dictionary/terms-0.dict` | `DictStreamWriter`, `build_dictionary` :5226 | T records — 1,530 B here | serving | sequential at open | **already streamed**; never held whole |

### 2b. The pairs relation and the batch loop

| structure | file(s) | written | size | consumers | access | streamed/bounded? |
|---|---|---|---|---|---|---|
| **packed pair buckets** | `.build-tmp/bucket-{k}.u64` | pairs pass :917, only when `!bucket_in_ram` | `8p`, one per batch | `store.load(k)` :1234, then `par_sort_unstable` + `dedup` | whole batch in RAM, sorted | already the plan's own bound; deleted per batch at :1334. **Not exercised** by the GBIF prefix (bucket was in RAM) |
| **`distinct_of_ordinal`** | none (heap) | pairs pass :931 | `4n` — 13,335 MiB at rung 6 | the label-agreement check :1298, inside the assignment walk, only on the per-view route | random by ordinal within a batch | Would fit a `MappedArray`. **Blocked**: `plan_build`'s `loop_fixed = 4*n + …` :691 feeds `feasible` :693 feeds `auto_batch` :698, so removing the term changes the batch stride and hence the permanent entity-id assignment (I9). Owner ruling, not a performance change |
| **`appearances`** | none (heap) | geometry pass :1010 | `4n` | the same check :1298; each view's permutation population | random by ordinal | same coupling is absent (it is not in `loop_fixed`); released at :1349 |
| **term bands** | `.build-tmp/band-{j}.pairs` | assignment walk :1320 | ≤ `6p`; measured 2.02 B/item, **7.1 GB** | the band sweep :1403–1478, once, sequentially; deleted per band :1479 | sequential | already streamed and already deleted at its reader |
| **ordinal→entity map** | `.build-tmp/entity-of-ordinal.u32` | assignment walk :1286, scattered by ordinal | `4n` — 4.00 B/item, **14.0 GB** | external ids :1506 (ascending); `read_attributes_by_entity` :2251 (chunk-ascending); `write_scoped_columns` :2796; `layers::publish` closures :1650/:1665 (**random**, once per member entry); pass two's permute :1794 and membership scatter :1801 (ascending) | mixed: two sequential, two random | **No** for the publication and the attribute join. It is a permutation lookup with genuinely random readers. It is bounded only in the page-cache sense (`MappedArray`) and released at :1991 |
| **postings spool** | `.build-tmp/postings.spool` | band loop :1400 | one record per term | `spool.finish(&postings_path)` :1492, mapped into one Arrow buffer | sequential append, then one map | **already the spool-and-map mechanism**; 28,672 B here |
| **entity→term transpose** | `entities/terms/{offsets.u32,terms.u32,hasrow.roaring}` | the assignment walk itself :1330 | `4(n+1) + 4p + bitmap` — 4.00 + 4.00 B/item, **28.0 GB** | serving: drill-down `labels`, the join rule's label arm (`tessera-store/src/entity_terms.rs` module doc) | random by entity at serving | published output, not an intermediate; already written from the walk with no second pass |
| **postings** | `terms/postings.arrow` | :1492 | ≤ `4p`; **34 KB** measured against 503 MB charged | serving | — | published output |
| **oracle pairs** | `terms/pairs.parquet` | band loop :1470, only when `emit_oracle_pairs` | ≤ `4p`; 1.06 MB | the test-time oracle | sequential | already skippable with `--no-oracle-pairs` |

### 2c. Geometry

| structure | file(s) | written | size | consumers | access | streamed/bounded? |
|---|---|---|---|---|---|---|
| **each view's ordinal geometry** | `.build-tmp/{x,y}-of-ordinal-{v}.u32` | geometry pass :1012, scattered by ordinal from `join_chunk` | `8n` per view — 8.00 B/item, **28.0 GB per view** | the anchor materialisation :1130 (ascending); the signature sort's Morton tiebreak :1257 (ascending within a batch); pass two's permute into entity space :1794 (ascending read, scattered write) | **every read is sequential in the ordinal**; the *write* is the scatter | A streamed form needs the producer to emit in ordinal order, i.e. the points file sorted by `entity_id`. That is a data property (§5). Two reads at different stages would otherwise need two re-reads and re-quantisations of the Parquet, which the design explicitly declined (:1017). Released at each view's permutation :1813 |
| **the anchor's Morton geometry** | `.build-tmp/{x,y}-anchor.u32` | :1122, **only if `geometry[anchor].rows != n`** | `8n` | the sort's tiebreak alone | ascending | **Already consumer-tested at run time.** On every single-view corpus and every multi-view one whose anchor covers the union the file is not written and the view's own arrays are read instead. **Not exercised** by the GBIF prefix |
| **entity-space geometry** | `.build-tmp/{x,y}-of-entity.u32` | pass two :1788, one view at a time | `8n` — 8.00 B/item, **28.0 GB** | the tiler sort's `RowRec` build :1821 (ascending over the view's members); `residual_row` :1885 (**random by entity**, in row order) | random by entity | **No.** Row order is Morton order; the residual gather is a permutation. Dropped at :1932 |

### 2d. The declared attribute columns (`EntityColumn`, `crates/tessera-build/src/column.rs`)

Ten scratch files for four declared attributes on this corpus (a fixed-width column is `col` +
`present`, a string one `at` + `arena` + `present`), plus ten more for the join's staging buffer,
plus two per render column for the row tail.

| structure | file(s) | written | size | consumers | access | streamed/bounded? |
|---|---|---|---|---|---|---|
| **fixed-width column** (`kingdom` u8 render, `year` u16) | `.build-tmp/column-0.col`, `column-5.col` + `.present` | the attribute join `read_one_attribute_source` :2169, scattered by entity | `width × n + n/8` — 1.00 and 2.00 B/item, **3.5 / 7.0 GB** | `write_column_values` :3298 (ascending); `record_value_of` :3181 (ascending); `render_lane` :4886 (**random by entity**, in row order) | random for a render column, sequential otherwise | a render column: **no**. A non-render one: yes in principle, but the width is already the payload |
| **indexed `keyword` column** (`specieskey`, `index = true`) | `column-2.at`, `column-3.arena`, `column-4.present` | the join, scattered by entity; the arena appended in **arrival** order | `8n` offsets + arena(chars + 4 B header/value) + `n/8` — 8.00 + 10.67 + 0.125 B/item, **65.7 GB** | `for_each_keyword` :3453 → `str_at` (ascending entity, **random into the arena**); released at :1719 when the postings end | one full ascending walk of entity space, once | **Yes.** One consumer, one pass. The pass already spills sorted runs (§3.1); it could take `(key, row)` pairs from the join directly and never place a value at an entity |
| **blob-resident `keyword` column** (`scientificname`, neither flag) | `column-7.at`, `column-8.arena`, `column-9.present` | as above | 8.00 + 36.28 + 0.125 B/item, **155.2 GB** | `ColumnRows::next_row` :3146 → `record_value_of` :3181 (ascending entity, **random into the arena**) | one full ascending walk, once | **Yes — and the mechanism already exists for `text`.** This is the memo's "in flight" item |
| **`text` column** | `column-N.present` only | `EntityColumn::prose` `column.rs:323` | `n/8` | — the slot exists so later passes can index by declaration position | — | **already the extent route** (§3.2). **Not exercised** by GBIF |
| **prose extents** | `.build-tmp/prose-{col}-{serial}.{blocks.bin,hasrow.roaring,directory.arrow}` | `spill_prose_chunk` :2136 → `ProseColumn::push_extent` `prose.rs:68`, one per join chunk | the compressed prose; modelled at half the source's characters (`EXTENT_SHARE`) | `write_text_index` :4223 in block windows; `write_record_blob` :3031 merges them; folded above 128 extents `prose.rs:125` | sequential in both | **is** the streamed form. **Not exercised** by GBIF |
| **the join's staging buffer** | `column-10..19` (`.col`/`.at`/`.arena`/`.present`) | `read_one_attribute_source` :2169, per chunk, `reset` between chunks | `JOIN_STAGE_BYTES` (256 MiB) of fixed-width slots; the arenas hold a whole chunk's characters and are **not** in that budget — measured 405 MB total, flat in n | the scatter into the entity columns :2251 | sequential | already budgeted, though the string arenas escape the budget (`residency.rs:760`) |
| **the row-order render tail** | `column-20.col`, `column-21.present` (one pair per render column per view) | `permute_attribute_tail` :4819 / `render_lane` :4886 | `width × rows` per render column — 1.00 B/item, **3.5 GB** | `write_columns` :1928 | sequential out | the gather is the permutation; **no** |

### 2e. The layers

| structure | file(s) | written | size | consumers | access | streamed/bounded? |
|---|---|---|---|---|---|---|
| **member spill runs** | `.build-tmp/member-run-{seq}.spill` | `MemberSpill::spill` `layers.rs:322`, when the accumulator reaches its budget | ≤ 4 B/entry charged, 2.9 measured; six runs here totalling 4.51 B/item, **15.8 GB** | `merge_member_runs` :1764, k-way, once | sequential | **already the spill-and-merge mechanism** |
| **merged member table** | `.build-tmp/member-table.spill` | `merge_member_runs` :1773 | 4.12 B/item, **14.4 GB** | `verify_hierarchies` :2085 (a parent then its children — **random**); `load_members` :2631 from `publish` :1476 (level in key order — **random**) | random by artifact index | **No**, and `spill.rs:2107` says so explicitly: "Random access, and that is the whole reason this file exists". Unlinked at its last reader by `Drop` `spill.rs:2133` |
| **published membership extents** | `members/members-{n}-{lvl}.tsmb` | `write_membership_extents` :2675 | 3 B/entry charged, 2.24 measured; 1.90 + 2.21 + 2.60 B/item, **23.5 GB** | the store reads its own copies back through the mapping :2675; the artifact pass reads these, not the table | random by artifact | published output; the mapping is what took the memberships off the heap |
| **containment partitions** | `containment/containment-*.tscp` | `artifact_pass::containment` :587 | 0.019 B/item, 2.4 MB | serving | — | published output, composed on first use if absent |
| **row-column lanes** | `row-column-{n}-{lvl}.tslb` | `artifact_pass` :415/:451 | `4n` (or 2) per row-major level per view — 4.00 + 4.00 + 2.00 B/item, **35.0 GB** | serving | random by row | **already consumer-tested**: written only for a level whose chosen layout `is_row_major()` and whose membership is `Enumerated`/`Spatial`, and skipped where the memberships do not partition (:441) |
| **tile indexes** | tile-index extents | `artifact_pass` :358–390 | per non-row-major level | serving | — | **already consumer-tested**: skipped for a row-major level. **Not exercised** here (all three levels were row-major) |
| **shape rows / shape held** | shape-rows and shape-held extents | :459–516 | per spatial level | serving | — | **Not exercised** by GBIF |
| **containment report** | `reports/containment.json` | `write_containment_report` `lib.rs:2450` | 20 KB | the operator | — | a report |

### 2f. The value columns and their indexes

| structure | file(s) | written | size | consumers | access | streamed/bounded? |
|---|---|---|---|---|---|---|
| **value column** | `attrs/{col}/values.arrow` | `write_column_values` :3298, only where `postings_are_owed` | `4n` for a keyword's ordinals, the declared width otherwise — 3.89 and 2.05 B/item, **13.6 / 7.2 GB** | serving scans | rank-addressed | published output; already gated by the predicate |
| **value spool** | `attrs/{col}/values.arrow.spool` | `ValueColumnWriter::create` `tessera-filter/src/values_writer.rs:337` | the column, once more — 3.77 and 1.93 B/item, **13.2 / 6.8 GB** | mapped into the Arrow buffer at `finish` | sequential append, then one map | **is** the spool-and-map mechanism; removed when the column is written, and by `Drop` on an abandoned write. It doubles the column's disk for the length of one stage |
| **presence bitmap** | `attrs/{col}/presence.roaring` | `Presence` :3395 | 0.085 B/item | serving | — | **already deferred**: a fully-present column writes none, decided at the first gap |
| **keyword dictionary** | `attrs/{col}/dict.bin` | `write_keyword_column` :3733 via `SortedDictWriter` | 0.018 B/item | serving | binary search | **already streamed**: the writer holds only its restart table |
| **keyword sorted runs** | `attrs/{col}/keyword-run-{seq}.spill` (+ `keyword-merge-*` on cascade) | `KeywordChunk::spill` :3621 | budget-bounded per run; 1.50 B/item total, **5.2 GB** | `merge_keyword_runs` :3788, once | sequential | **already the spill-and-merge mechanism** |
| **keyword ordinal scratch** | `attrs/{col}/keyword-ordinals.scratch` | the merge :3788, scattered by row | `4 × present rows` — 3.77 B/item, **13.2 GB** | the values-file write, front to back in `VALUE_CHUNK` slices | sequential read, **scattered write** | The doc :3711 states the alternative considered — a second external sort of `(row, ordinal)` — and declines it. Bounded in memory by `MappedArray`, not on disk |
| **text index** | `attrs/{col}/{dict,postings}` + `text-run-*.spill` | `write_text_index` :4223 | per term | serving | — | **already the spill-and-merge mechanism**, per worker under `TEXT_BUDGET_SHARE`. **Not exercised** by GBIF |
| **scoped family columns** | `attrs/{col}/{group}/{key}/…` | `write_scoped_columns` :2449 | one column per view of the group | as the entity-scoped ones | — | **Not exercised** by GBIF; `multiview` covers it, and is the only corpus with a group-scoped `text` column — the one family still read in arena order |

### 2g. The record blob and the row spaces

| structure | file(s) | written | size | consumers | access | streamed/bounded? |
|---|---|---|---|---|---|---|
| **record blob** | `attrs/record/{blocks.bin,hasrow.roaring,directory.arrow}` | `write_record_blob` :3031 → `merge_record_rows` | 8.20 + 4.13 B/item, **43.1 GB** | serving drill-down | random by entity rank | published output; its input is already a merge of ascending row streams |
| **segment columns** | `views/{v}/segments/seg-0/columns.arrow` | `write_columns` :1928 | `4 + 8 + Σ render widths` per row — 13.38 B/item, **46.8 GB** | serving | — | published output |
| **Morton codes** | `.../morton.u32` | :1856 | `4n` — 4.00 B/item, **14.0 GB** | serving | — | published output |
| **permutation** | `views/{v}/permutation.bin` | `write_permutation_iter` :1863 | `4n` — 4.00 B/item, **14.0 GB** | serving; the artifact pass's `RowSpace` | random by entity | published output |
| **row→entity** | `views/{v}/row-entity.u32` | :1885 | `4n` — 4.00 B/item, **14.0 GB** | serving; `permute_attribute_tail` | sequential | published output |
| **render presence** | `.../{col}.presence` per segment | `write_render_presence` :1917 | a bit a row | serving | — | published output |
| **external ids** | `entities/…` extents + locator | :1508–1512, only when `mint_external_ids` | `12n` sort key + `20n` charged | serving `tessera_id` → external id | sorted | **Not exercised** by any ladder corpus in the probe |
| **manifests** | `MANIFEST.json`, `SEGMENTS-0.json`, `CURRENT` | `write_manifests` :2007 | KB | everything | — | published output |

**Present in the code and not exercised by the probe's corpora:** the spilled pair buckets
(`bucket-{k}.u64`, needs `!bucket_in_ram`); `x-anchor.u32`/`y-anchor.u32` (needs a multi-view build
whose anchor does not cover the union); prose extents and the text index (`medcpt-1m`,
`multiview`); the scoped column families (`multiview`); the external-id sidecar and its locator
(`mint_external_ids`); tile indexes, shape rows and shape held (`geonames`, `treeoflife-1m`); the
keyword and text run **cascades** (`{family}-merge-{pass}-{group}.spill`, above 128 runs) and the
prose **fold** (`prose-{col}-fold{n}-{group}`, above 128 extents); `MemberTable::empty` for a build
whose layers declare no member source. `ScalarType::Utf8` is retired as a *declared* type
(`config.rs:4736`) but its arms remain live in `column.rs`, `residency.rs` and the engine, reachable
only through a programmatically assembled schema or the ingest plane.

---

## 3. What is already bounded, and by what mechanism

Four mechanisms, and they are not the same thing. Two remove bytes; two do not.

### 3.1 Spill-and-merge under a budget

A pass accumulates into a buffer sized from `--memory-budget`, writes the buffer out as a **sorted
run** when it fills, cascades the runs where there are more than one merge may hold descriptors for
(`RUN_MERGE_FAN_IN`), and merges k-way. The pass's residency is the plan whatever the corpus, and
the **run count** grows instead of the peak. The output is a function of the corpus alone and never
of where a chunk boundary fell — asserted by
`chunking_the_keyword_column_does_not_change_its_bytes` and its text twin.

Instances: the **text index** (`write_text_index` :4223, `TextIndexPlan`, per worker); the **keyword
dictionary** (`write_keyword_column` :3733, `KeywordDictPlan`, one chunk live — this is the
2026-09-09 change the memo means by "bounded by the budget rather than by `n`"); the **member
spill** (`layers.rs:322`, budgeted accumulator → `member-run-*.spill` → `merge_member_runs` :1764).

The keyword pass has a fourth step the text pass does not: the merge yields ordinals in *key* order
and the values file wants them in *row* order, so it scatters into a `u32` array addressed by row
(`keyword-ordinals.scratch`). That array is mechanism 3.3, not 3.1 — it is bounded in memory and not
on disk.

### 3.2 Extents written while decoding

The join writes each chunk of a `text` column as a **record-blob extent** in that chunk's entity
order (`prose.rs:68`) — the same three files, block format and addressing as the base blob. The value
is never placed at an entity index at all. Both consumers read the extents sequentially: the text
index in block windows, the record blob by merging them. An entity written twice is handled by a
**live set** — extent *i*'s has-row bitmap less the union of every later extent's
(`OpenProse::live`) — which is Roaring and costs nothing per value.

This is the only mechanism that removes both the offset array and the arena. It is what the memo
means by "a `text` column has always had one".

### 3.3 Page-cache substitution (`MappedArray` / `MappedArena`)

An `n`-sized array becomes a file under `.build-tmp/` and is used through a mapping. **This bounds
memory, not disk**: the bytes are page cache the kernel may evict, so a corpus larger than the box
gets slower rather than OOM-killed, and `Residency::total` (the figure `--memory-budget` is compared
against) leaves the term out while `Residency::at(phase)` still charges it to the disk.

Instances: every declared column (`column.rs`); the geometry arrays; the ordinal→entity map
(2026-09-10, `6af1fc8b`); the source-id array (2026-09-10, `cfd6e4a3` — pass one went from `16n + 200 MiB`
of anonymous memory to a flat ~230 MiB, measured at three row counts in
`probes/2026-09-10-source-ids-memory/`); the keyword ordinal scratch.

The arena's growth was made **modellable** at the same time: `MappedArena::grow` doubles below
`ARENA_GROWTH_STEP` (256 MiB) and adds a step above it, and `reserve` `posix_fallocate`s the new
range only, so the capacity is within one step of the payload rather than up to twice it.

**This is why "given a bounded form" covers two different things in the memo.** The keyword
dictionary got 3.1 and its disk fell. The source-id array and the ordinal map got 3.3 and 3.4;
their disk did not move at all.

### 3.4 Release at the last reader

Not a form, a lifetime. `MappedArray::Drop` and `MemberTable::Drop` unlink the file as the structure
is released, and the 2026-09-10 audit moved six releases from the end of the build to the pass that
last reads them: each view's ordinal geometry at its permutation (:1813), the member table at the
publication, a non-render non-blob-resident column at the filter postings (:1719), the anchor
geometry at the assignment (:1356), the two label-agreement counters at the assignment (:1349), the
source ids before the publication on the contiguous path (:1643).

### 3.5 A fifth, worth naming separately: spool-and-map

`PostingsSpool` and `ValueColumnWriter` append a file sequentially and then map it as one Arrow
buffer, so the artefact is never whole in memory. It costs a **second copy on disk** for the length
of one stage — `values.arrow.spool` is 3.77 B/item beside `values.arrow`'s 3.89 — and it is not a
bounding mechanism for disk at all.

---

## 4. The inconsistency, ranked

Structures whose consumer set one of the mechanisms above would already satisfy. Rows 1 and 2 are a
declared-type branch and nothing else; rows 3 to 7 are listed so the ranking is complete. Bytes are
rung-6, modelled from the probe's per-item figures.

| # | structure | rung 6 | consumer set | the mechanism that fits | the branch that prevents it |
|---|---|---|---|---|---|
| 1 | a **blob-resident `keyword` column**'s arena + `at` + presence (`scientificname`) | **155.2 GB** (44.4 B/item: 36.28 arena, 8.00 `at`, 0.125 presence) | exactly one: `ColumnRows::next_row` :3146, one ascending walk of entity space, once | **3.2, extents written while decoding.** The join already has the chunk sorted by entity; `spill_prose_chunk` :2136 is the code | `EntityColumn::prose` vs `filled` is chosen by `ScalarType::Text` alone (`pipeline.rs:2080`). `blob_resident` :3125 is the consumer predicate and already exists. **An agent is on this as of 2026-09-10** |
| 2 | an **indexed `keyword` column**'s arena + `at` + presence (`specieskey`) | **65.7 GB** (18.8 B/item), of which the `at` array alone is **28.0 GB** | exactly one: `for_each_keyword` :3453, one ascending walk, once | **3.1, spill-and-merge** — the dictionary pass *already* spills sorted `(key, rows)` runs. It could take pairs from the join and never build the column | the same `ScalarType::Text` branch. The predicate that would answer it is "is this column read anywhere but the dictionary pass" — i.e. `render \|\| blob_resident` |
| 3 | the **source-id array** on a contiguous corpus | **28.0 GB** (8.00 B/item) | every consumer reduces to `id − ids_first` / `ids_first + ordinal` when the ids are a contiguous range | none needed — the array would not exist | `join_chunk` :354 has no contiguous fast path; the test exists only at the layer publication :1632. **Contiguity is a data property** (§5) |
| 4 | **each view's ordinal geometry** | **28.0 GB per view** (8.00 B/item × V) | three readers, **all ascending in the ordinal**: the anchor build :1130, the sort's tiebreak :1257, the permute :1794 | 3.2 in spirit — extents in producer order — but only where the producer emits in ordinal order | the scatter is needed because the points file is in its own order. **Sortedness of the points file is a data property** (§5), and the two reads are two stages apart |
| 5 | the **ordinal→entity map** | **14.0 GB** (4.00 B/item) | five readers; two ascending, **two random** (the layer publication's per-member-entry resolve :1650, the attribute join :2251) | none. It is a permutation with random readers | **Not** a type branch. This one is materialised for a real reason, and the memo's grouping of it with the geometry does not hold |
| 6 | the **keyword ordinal scratch** | **13.2 GB** (3.77 B/item) | one: the values-file write, front to back | a second external sort of `(row, ordinal)` would remove it; the doc :3711 weighs and declines that | not a type branch — a considered trade |
| 7 | the **values spool** beside every value column | **13.2 GB** for `specieskey`, **6.8 GB** for `year` | one: the map at `finish` | none available; the spool *is* the streaming form | not a type branch |

**Two corrections to the memo's framing, from the code.**

- Its item 3 groups the ordinal geometry with the ordinal→entity map as "both still materialised
  whole". They are not alike. The geometry has **no random reader at all**; the map has two. Only
  the geometry is a candidate.
- Its item 2 ("a string value's fixed overhead — 8 B of offset plus a record header") understates
  the finding. There is no random-by-entity reader of *any* string column in the build: `render` is
  refused on `keyword` (`config.rs:4807`) and on `text` (:4797), and `utf8` is retired as a declared
  type (:4736). Every string column has one consumer and that consumer is a full ascending walk. The
  8 B/item offset array exists to index an arena whose order is the *source file's*, and the arena's
  order is the thing that forces it.

**One thing the code does that the memo does not credit.** `for_each_record_in` `column.rs:460` is
an arena walk that would remove the random access without removing the `at` array — it still reads
`col.at[entity]` to test that a record is live. Only the prose route (3.2) removes both, because it
carries liveness in a Roaring bitmap instead. So "walk the arena" and "spill extents" are different
answers with different savings: the first saves the random reads, the second saves 8 B/item as well.

---

## 5. What a general rule would have to read

### The signature

```
is_materialised_owed(structure, schema, consumers) -> Materialisation
```

where `Materialisation` is not a boolean. The build already distinguishes five outcomes and a
predicate that answered yes/no would collapse them:

- **not written at all** (the anchor geometry where the anchor covers the union),
- **written whole** (the member table, for random readers),
- **written and released at the last reader** (six structures since the audit),
- **streamed as extents in producer order** (a `text` column),
- **deferred until a consumer proves it needed** (`Presence`, materialised at the first gap).

### The inputs

| input | already read by | example |
|---|---|---|
| the column's declared flags — `index`, `render` | `postings_are_owed`, `blob_resident` | the whole of §1 |
| a named object's declared flags — a vocabulary's `visibility` | `postings_are_owed` | the `derived` clause |
| the declared type, **only where the type changes the consumer set** | `ScopedScalar::licence_of`, `owes_value_column` | `text` has a token index and no value column because prose has no per-entity slot |
| the **access shape** each consumer needs — random by entity, random by ordinal, ascending walk, by range | nowhere; it is in the doc comments | `spill.rs:2107`'s "Random access, and that is the whole reason this file exists" |
| the **number of passes** a consumer makes over the structure | nowhere | the geometry's two readers are two stages apart, which is why re-reading the Parquet was declined |
| the **stage** each consumer runs in | `ColumnCost::phases` in `residency.rs:323` | already computed, from `render` / `blob_resident` |
| whether a *second* consumer exists at all | implicitly, in each predicate's disjunction | the `derived` clause exists because a second consumer does |

The first three are declaration facts and are available at plan time. The fourth and fifth are
**properties of the code, not of the schema**, and today they live only in prose. A rule that read
them would need them stated — a consumer registry per structure, naming the pass and the access
shape, which the residency model's `Phases` is already half of.

### Where it is not determinable at plan time

Three cases, and they are the reason a consumer-driven rule cannot simply replace a type-driven one.

1. **Contiguity of the source ids.** The publication's resolve is a subtraction only if
   `ids_last − ids_first + 1 == n`, and that is not known until the ids are read, sorted and
   deduplicated — which is the array. The memo names the exact fix: a presence bitmap at `n/8`
   bytes decides it during the read, at a sixty-fourth of what it removes. The `mix64` anchor
   already in the build does not, being a hash. **Whether the sort is owed anyway** for the
   per-view duplicate check (`read_source_ids_union` :5134) is the thing to settle first: the
   duplicate check is per view and in place, and a presence bitmap over the id range answers it
   too — but only where the range is representable, which is itself a data property.

2. **Whether the anchor covers the union.** Already handled at run time
   (`anchor_fallback_reaches_an_item` :1119), and the disk model charges the term on any multi-view
   declaration because the answer is not known before pass one (`residency.rs:739`). This is the
   working precedent: the model over-charges at plan time and the code decides at run time, and the
   two are allowed to disagree in the safe direction.

3. **Whether an attribute or points source arrives in entity order.** The geometry scatter and the
   arena's arrival order both exist because the source's order is not the build's. GBIF's points
   file happens to ascend with `entity_id`; nothing in the declaration says so, and a build that
   assumed it would give every item after the first out-of-order row another item's position, with
   no error anywhere.

A fourth, weaker case: **whether a membership partitions**. `artifact_pass` :441 discovers at write
time that a level's memberships do not partition and falls back to artifact-major, printing the
reason. That is a consumer test whose answer is data.

### Where a consumer-driven rule cannot replace a type-driven one

- **Where the type *is* the consumer set.** A `text` column has many terms per entity and no
  per-entity slot, so it has a token index and no value column. That is not a proxy for a consumer
  question; it is the answer to one. `owes_value_column`'s `Text` early return is correct as a type
  test.
- **Where the structure is identity-bearing.** `distinct_of_ordinal`'s `4n` is inside
  `plan_build`'s `loop_fixed`, which feeds `auto_batch`, which sets the batch stride, which is the
  permanent entity-id assignment under I9. No consumer argument can be applied here without an
  owner ruling, because the change is to the corpus and not to its cost.
- **Where two random readers exist.** The merged member table and the ordinal→entity map are read
  at scattered indexes by passes whose order the producer cannot emit in. A streamed form does not
  exist for them at any price short of a second sort.
- **Where the mirror must stay a mirror.** `postings_are_owed` and the engine's `owes_value_column`
  must agree or the open demands a file no pass wrote (`filter.rs:1532`). Any new predicate that
  decides a *published* file's existence acquires the same obligation, and the place it can live is
  the record — `ScopedScalar::licence_of` is the precedent, because `check-layers.sh` denies the
  build the engine.

### The shape the rule would actually take

Not a graph walk. The template `postings_are_owed` sets is a **named predicate per structure, whose
body is one clause per consumer, each clause a declared fact, each clause's consumer named in the
doc**. Generalising it means writing four more of them and calling them where `ScalarType` is
matched today:

- `arena_is_owed(schema, attribute)` — false where the column's only reader is the record blob or
  the keyword dictionary, both of which take a stream. Today the answer is `ty != Text`.
- `entity_offsets_are_owed(schema, attribute)` — false where no consumer reaches the column at a
  random entity. Today: always true for a string column.
- `walked_records_are_owed(schema, attribute)` — the `RECORD_HEADER_WALKED` choice, today
  `ty == Text` (`column.rs:234`), which is right only because a bundle-wide `text` column has no
  arena and a scoped one is the sole walker.
- `ordinal_map_is_owed` / `source_ids_are_owed` — which cannot be plan-time predicates, and would
  have to be the runtime tests §5 lists, with the disk model over-charging at plan time exactly as
  it already does for the anchor.
