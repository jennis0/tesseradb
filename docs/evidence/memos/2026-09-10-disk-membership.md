# What membership costs on disk, and where the cost is

**Status:** Fact-find, 2026-09-10, for the disk-use campaign. **Not normative, and nothing here is
decided.** Produced by a subagent reading main at 0c0b60b3 as one of eight parallel investigations;
it is that agent's read of the code, not a reviewed design. The owner's session independently
verified the derived-file collision — `derived_name` puts no view id in a filename (its own doc says so), and `treeoflife-1m`'s three `row-column` files are all 1,520,534 B = 760,259 × 2 + 16, the `geo` view's row count, where the `bioclip` view's 1,000,000 rows would need 2,000,016. **Every other claim here is unverified** — re-check a figure
or a citation before relying on it.

Measurement tooling for figures the report marks as taken in its own session is in
[`probes/2026-09-10-disk-survey/`](../../../probes/2026-09-10-disk-survey/). The campaign's starting
point is [`2026-09-10-build-disk-weight.md`](2026-09-10-build-disk-weight.md), which these supersede
in the places they contradict it. The other fact-finds in this set are the sibling
`2026-09-10-disk-*.md` memos in this directory.


**Date:** 2026-09-10. **Branch** main at 0c0b60b3. **Box** WSL2, Ryzen 9 5900X, 47 GiB.
Read-only fact-finding. Every figure is marked *measured*, *modelled* or *assumed*. Bundle figures
were taken today by decoding the shipped `.tsmb` and `.tslb` files under `data/`; the decoder is
`scratchpad/tsmb.py`.

## The result

**Membership is 7–19% of a bundle, and the row-addressed copy of the same relation beside it is
another 0–29%.** Together they are 8–48% of every bundle on this box. *(Measured.)*

| corpus | rows | bundle | members `.tsmb` | | row-major column | | both |
|---|---:|---:|---:|---:|---:|---:|---:|
| `medcpt-1m` | 1,000,000 | 332.7 MB | 63.1 MB | 18.95% | 96.4 MB | 28.96% | **47.9%** |
| `medcpt-10m-abs` | 10,000,000 | 7.71 GB | 543.9 MB | 7.06% | 963.5 MB | 12.50% | **19.6%** |
| `gbif-64p` | 25,846,007 | 2.12 GB | 179.3 MB | 8.44% | 103.4 MB | 4.87% | **13.3%** |
| `gbif` 200m prefix ⊘ | 125,789,091 | 8.62 GB | 843.7 MB | 9.79% | 1,257.9 MB | 14.60% | **24.4%** |
| `treeoflife-1m` | 1,000,000 | 187.8 MB | 18.6 MB | 9.93% | 4.6 MB | 2.43% | **12.4%** |
| `geonames` | 13,463,857 | 1.40 GB | 103.2 MB | 7.39% | — | 0% | **7.4%** |
| `paperseek` (rung 4) | 102,117,343 | 70.78 GB | 486.6 MB | 0.69% | 204.2 MB | 0.29% | **0.98%** |

⊘ The `gbif` 200m row is from `probes/2026-09-10-build-disk/200m-after.files.tsv` (the files' own
maxima, measured) over the memo's measured 68.5 B/item finished bundle; that bundle is not on the
box. Every other row is a `du` and a decode of files that are.

`paperseek` is the counter-example that shows what the fraction is a fraction *of*: its bundle is
70.8 GB because it carries 100M abstracts as a record blob, so the same 496×10⁶ member entries are
1% rather than 20%.

**Adding the derived artifact family** — tile index, containment partition and the artifact record
extents — moves those totals by under a percentage point everywhere except `geonames`
(8.26% against 7.39%, its five artifact-major levels paying 3.7 MB of tile index). *(Measured.)*

**Per entry, the store costs 0.98–2.40 B.** *(Measured.)*

| corpus | artifacts | entries | entries/row | B/entry, whole file | B/entry, Roaring alone |
|---|---:|---:|---:|---:|---:|
| `paperseek` | 5,054 | 496,443,271 | 4.86 | **0.980** | 0.979 |
| `medcpt-10m-abs` | 30,254 | 471,743,606 | 47.17 | **1.153** | 1.149 |
| `medcpt-1m` | 29,272 | 47,178,538 | 47.18 | **1.337** | 1.298 |
| `geonames` | 465,343 | 68,450,878 | 5.08 | **1.508** | 1.123 |
| `treeoflife-1m` | 70,265 | 7,841,331 | 7.84 | **2.377** | 1.479 |
| `gbif-64p` | 251,907 | 74,626,027 | 2.89 | **2.403** | 2.180 |
| `gbif` 200m ⊘ | 251,907+ | 377,367,273 | 3.00 | **2.236** | — |

The disk memo's `2.24 B` measured figure is the `gbif` 200m row. It is the top of the range and the
build model charges 3 against it.

**Three findings the fractions do not show.**

1. **On a taxonomy the membership file is mostly not membership.** `treeoflife-1m`'s deepest level
   spends **45.3%** of its extent on artifact *key strings* and 36.2% on members; `geonames`' level 4
   spends 28.1% on members and 71.9% on framing. The keys are full paths
   (`Animalia|Arthropoda|Insecta|Diptera|Cecidomyiidae`, 73.7 B mean at that level), and every
   artifact also pays 8 B of offset, 8–10 B of fixed header and 12–27 B of attachment/parent/shape
   tail. *(Measured, §4.)*
2. **The row-major column is a second full copy of the relation, in row order, uncompressed** —
   larger than the membership itself on three of the six corpora. *(Measured.)*
3. **The closure is materialised at every level, and most of it is derivable from the level below
   plus a small stray set.** `gbif`'s level-0 members that have no level-1 artifact are **1.88%**;
   the containment report names **16,068 of 55,977** parents as non-covering. *(Measured, §3.)*

**Shrinking the store on disk would not cost request latency.** The `.tsmb` is read once per
process, at open, sequentially, and never again; requests read an in-memory row form and — for
row-major levels — a mapped row column, one random lookup per visible row. Compressing the *row
column* would cost latency directly; compressing the `.tsmb` would cost open time only. *(Traced,
§5.)*

**The floor is entropy coding, not delta or bitpacking.** GBIF family labels over Morton-ordered
rows have mean run length **1.13** and zeroth-order entropy **0.951 B/row** against a stored
2.00; `xz -6` over the raw label bytes reaches **0.683 B/row**. Run containers would make things
worse: memberships average **1.08–1.22 entries a run** in entity space, and `run_optimize` is
never called. *(Measured, §6.)*

---

## 1. The representation

A membership is **one CRoaring portable-serialised bitmap per artifact, in entity space, packed
many-to-a-file behind one manifest entry**. There is no column, no delta form and no compression.

**The file.** `crates/tessera-store/src/membership.rs:30-45` (module doc) and `:92`:

```text
header  := "TSMB" | u16 version | u16 reserved | u32 count | u32 ordinal_lo   (16 B)
offsets := u64 LE x (count + 1)                                              (8 B/artifact)
payload := count blobs, concatenated in ordinal order
```

One file per `(layer, level)` per publication, addressed by dense ordinal. `MembershipPack::open`
(`membership.rs:245`) maps it and validates the framing; `PackWriter` (`membership.rs:130-232`) is
the streaming writer the build uses so a level's blobs never stand in memory twice.

**The blob.** `tessera_lifecycle::membership::encode_record`
(`crates/tessera-lifecycle/src/membership.rs:2517`):

```text
u16 key_len | key | u16 view_len | view | u16 content_count | u32 members_len | members
            | per content: 32 B digest, u64 cardinality, u32 set_len, set
            | u8 attach_tag [ + u16 layer_len | layer | u32 level | u32 ordinal | u64 entity ]
            | u16 parent_count | per parent: u32 level, u32 ordinal
            | u8 shape_tag [ + 32 B digest | shape bytes ]
```

`members` is `serialise_members` = `Bitmap::serialize::<Portable>()`
(`membership.rs:2798`). The store holds it opaquely on purpose: `tessera-lifecycle` owns the
Roaring form and `tessera-store` owns only the addressing.

**Bytes per entry, decomposed.** Roaring's array container is 2 B an entry plus 4 B of descriptive
header and 4 B of offset per 65,536-wide block; a bitset container is a flat 8,192 B. Measured
container mix, all corpora on the box: **array 99.0–99.99%, bitset the rest, run containers zero**.
So the payload floor as built is 2 B an entry wherever a container holds under 4,096 members, and
1 bit an entry wherever it holds more than that.

Measured anatomy of four extents *(one level each)*:

| extent | artifacts | offsets | key | fixed hdr | members | tail |
|---|---:|---:|---:|---:|---:|---:|
| `treeoflife-1m` taxonomy L6 | 51,920 | 8.00 B | **73.67 B** | 10.00 B | 58.92 B | 12.00 B |
| `gbif-64p` taxonomy L2 | 186,464 | 8.00 | 41.48 | 10.00 | **309.49** | 12.00 |
| `geonames` admin L4 | 231,645 | 8.00 | **20.31** | 8.00 | **23.19** | **23.00** |
| `medcpt-10m` mesh L0 | 30,119 | 8.00 | 18.26 | 8.00 | **17,444** | 26.94 |

Per artifact the framing is **56–100 B** across every bundle on the box *(measured)*. As a share of
the membership store: `treeoflife-1m` 37.8%, `geonames` 25.5%, `gbif-64p` 9.3%, `medcpt-1m` 2.9%,
`medcpt-10m` 0.4%, `paperseek` 0.09%. The rule is the mean artifact size: a layer of many small
artifacts pays the framing, a layer of few large ones does not.

**The second representation.** For a level served row-major the fold or build also writes the
membership **addressed by row**, in the same module (`membership.rs:992-1020`):

```text
label := "TSLB" | u16 version | u8 width | u8 reserved | u32 rows | u32 ordinals
         labels: width bytes x rows                       (width 1/2/4, all-ones = no artifact)
list  := "TSLL" | ... | u32 entries
         at:     u32 LE x (rows + 1)
         values: width bytes x entries
```

Verified against the files: `gbif-64p` level 0 is `16 + 25,846,007 x 2 = 51,692,030` exactly;
`medcpt-10m` is `20 + 10,000,001 x 4 + 461,743,606 x 2 = 963,487,236` exactly. So a label column is
`width` B a row and a list column is 4 B a row plus `width` B an entry. *(Measured.)*

Both files are written for a row-major level — the membership extent is the durable record and the
row column is derived per generation — so such a level carries the relation twice.

---

## 2. The spill and the publication

**What is written, in order.**

1. `MemberSpill` (`crates/tessera-build/src/layers.rs:236-330`) accumulates `(artifact, source)`
   pairs in a window of `Vec<u64>` per artifact, charged at 16 B a pair and 80 B an artifact against
   a budget of `memory_budget / 16` clamped to `[64 MiB, 1 GiB]`. Full window → one sorted run
   `member-run-NNNN.spill`, LEB128 delta over ascending sources within each artifact.
2. `cascade_member_runs` (`layers.rs:1824`) reduces the run count to at most
   `MEMBER_MERGE_FAN_IN = 128`, deleting each pass's inputs.
3. `merge_member_runs` (`layers.rs:1764`) k-way merges the runs, and for each artifact in turn:
   collects every source into `MemberRunMerge::sources: Vec<u64>`, **rewrites it in place** to
   entities via `resolve`, `sort_unstable()`s it, and appends it to `member-table.spill`.
4. `MemberTableWriter::push` (`crates/tessera-build/src/spill.rs:2000`) writes
   `varint64(e₀) ‖ varint64(eᵢ − eᵢ₋₁)…` with **no framing between artifacts**; the
   `MemberExtent { offset: u64, bytes: u32, entries: u32, anchor: u64 }` index stays in memory,
   24 B an artifact.
5. The publication reads the table back **one artifact at a time** (`load_members`, `layers.rs:2630`)
   into a reused `scratch: Vec<u8>` and `buf: Vec<u64>`, builds one Roaring bitmap, hands it to the
   store.
6. `write_membership_extents` (`layers.rs:2680`) streams each level's blobs into its `.tsmb` through
   `PackWriter`, fsyncs, then maps the file back and replaces each store record's heap bitmap with a
   view over the mapped bytes (`map_level_memberships`, `layers.rs:2745`). A rehousing that fails is
   reported, not refused.
7. `MemberTable::drop` (`spill.rs:2126`) unlinks `member-table.spill` at the publication.

**Disk peak, measured at 125,789,091 GBIF occurrences** (`200m-after.files.tsv`):

| file | bytes | window (s of 343) |
|---|---:|---|
| `member-run-0000…0005.spill`, 6 files | 566.8 MB total | 156.3 → 206.7 |
| `member-table.spill` | 517.8 MB | 192.7 → 221.2 |
| `members-000000-000/1/2.tsmb` | 843.7 MB total | 215.2 → end |

Runs and table overlap 192.7–206.7 s (**1,084.6 MB together**, which is the memo's figure); table
and extents overlap 218.2–221.2 s (1,361.5 MB). The member terms are **10.7%** of the 12.70 GB build
peak at this row count. *(Measured.)* The model charges runs+table at 4 B an entry (measured 2.9)
and the extents at 3 (measured 2.24) — `crates/tessera-build/src/residency.rs:407-438`.

**The "22.5 GB against family's 1.4 GB" note, precisely.** It is **memory, not disk**, and it is
`MemberRunMerge::sources` — one `Vec<u64>` holding every source id of the single artifact the merge
is currently emitting, because a source id's entity is not a monotone function of it and the
membership has to be in hand to be sorted. Kingdom *Animalia* holds 2.81×10⁹ members, so that vector
is 22.5 GB; the largest family holds about 175×10⁶, so it is 1.4 GB. That is why GBIF's tiered layer
starts at family. *(The 2.81×10⁹ is measured in `probes/2026-09-09-gbif-census/` per
`docs/ingest-campaign.md`; the 8 B/entry is the type.)*

The same bound is paid **twice more**, in two later passes over the same table:

- `verify_hierarchies` (`layers.rs:2086`) decodes a parent's whole membership into
  `HierarchyBuffers::parent_buf: Vec<u64>` to check containment against each child.
- the publication decodes it again into `buf: Vec<u64>` beside `scratch: Vec<u8>` (~1 B/entry) and
  then the Roaring bitmap (~2 B/entry) — about **11 B an entry** for the one largest artifact.

**There is no disk analogue.** The member table is written streaming and read by extent, so the
disk term is linear in *total* entries and indifferent to the distribution. Two hard limits are
per-artifact rather than per-corpus, both in `MemberExtent` (`spill.rs:1965`): `bytes: u32` refuses
an artifact whose delta-varint members exceed 4 GiB (Animalia, at deltas averaging ~1.25 over a
3.5×10⁹ entity space, would be ≈2.9 GB — inside the limit, at 70% of it), and `entries: u32` refuses
one above 4.29×10⁹. *(Modelled from the encoding; not measured, no such build exists.)*

⊘ **The memory model does not name the merge's vector.** `entity_order_residency`
(`residency.rs:466-602`) charges three member terms — `level_entries × 4 B` anonymous,
`member_entries × 3 B` mapped, `member_entries × 4 B` mapped — and none of them is
`largest_artifact × 8 B`. At a level whose largest artifact is a kingdom the pre-flight would read
14 GB where the moment costs ~36 GB. *(Modelled.)*

⊘ **The cascade is unreached today and would fire at rung 6.** The 200m prefix spilled 6 runs at a
1 GiB budget; 10.01×10⁹ entries at 16 B charged a pair is ≈160 runs against a fan-in of 128.
*(Modelled.)* `cascade_member_runs`' own doc says it is unreached by anything measured.

---

## 3. Closure upward

**It is materialised, at every level, and the build does not compute it.**

- **MedCPT.** `mesh.py` in the corpus preparation emits each article's ancestor-closed descriptor
  set; the build reads those rows as plain membership. `docs/design/dag-hierarchies.md` §2 measures
  the explosion on chunk 18: **9.96 explicit descriptors an article against 55.58 closed**, ≈3.1×10⁸
  rows against ≈1.7×10⁹. The ruling (option (a), §8) took the closure knowing the price.
  *(Measured on one chunk, extrapolated to the corpus.)*
- **Tiered layers.** `read_members`' `MemberKeys::Listed` arm (`layers.rs:1183-1216`) walks a row's
  list and calls `attach_member` once per position, each at the level that position names. So a
  seven-level taxonomy is seven member entries a row and a three-level one is three. Nothing sums,
  unions or infers.

Confirmed against the shipped files: every level of every tiered layer carries its own full extent.
*(Measured.)*

| bundle | layer | level entries |
|---|---|---|
| `gbif-64p` | `taxonomy/tree` L0/L1/L2 | 25,570,004 / 25,088,942 / 23,967,081 over 25,846,007 rows |
| `treeoflife-1m` | `taxonomy/tree` L0…L6 | 1,000,000 / 1,000,000 / 993,564 / 991,891 / 988,328 / 966,166 / 901,382 over 1,000,000 rows |
| `paperseek` | `topics/openalex` L0…L3 | 98,581,482 four times over |

**Could an ancestor's be derived from its children's?** Establishing the current behaviour only, as
asked:

- A parent's membership is **not** the union of its children's. `gbif-64p`'s containment report names
  **16,068 non-covering parents of 55,977** (28.7%), and the row columns say the stray entries are
  **481,062 of 25,570,004** at level 0 — **1.88%**. `treeoflife-1m`: 3,243 non-covering of 14,545.
  A stray is a row identified to family but not to genus. *(Measured.)*
- The engine has no notion that a parent contains its children. `docs/design/annotations.md` §6 and
  `dag-hierarchies.md` §7: *"the number beside a served artifact is its own declared membership, and
  roll-up is substitution of a parent for its children and never a sum"*; decision 0080 — every
  artifact is tested on its own masked count with no input from its lineage. `verify_hierarchies`
  *reports* containment; it never fills it in.
- What the read path actually asks of a coarse artifact: (a) `masked_count` = `|rows ∩ M|`
  (`artifacts.rs:1367`), which decides both the number on the wire and the existence criterion;
  (b) computed content — `centroid`, `box`, `hull` over `membership ∩ M_auth`; (c) its span in the
  tile index; (d) the deepest-served membership column, which intersects each served artifact's rows
  with the response's gathered rows. Containment does **not** use the membership — it uses the
  generating sets, which are a different set and are held separately (`MembershipRows::generating`).
- For a `tiered` layer with `prune_children` the memberships within a level are disjoint, so a
  parent's masked count is the sum of its children's plus its strays. That arithmetic is not
  performed anywhere today.
- `artifact-serving-at-scale.md` §7.3 already records the mask-independent alternative for the count
  alone (per-signature counts, ~1.3 MB for the top nine levels of a 10⁶-node tree) and marks it
  scoped and unbuilt. It removes the count's cliff; it does not remove the membership.

---

## 4. In the finished bundle

Measured today. Sizes are `du` byte totals over the named subtrees; entries and Roaring bytes come
from decoding every `.tsmb`.

| corpus | rows | bundle | members | tile index | row column | containment | artifact records | all artifact | % |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `gbif-64p` | 25,846,007 | 2,123,831,429 | 179,308,788 | 1,491,728 | 103,384,060 | 1,007,724 | 0 | 285,192,300 | **13.43%** |
| `treeoflife-1m` | 1,000,000 | 187,779,713 | 18,638,760 | 10,840 | 4,561,602 | 488,960 | 2,802 | 23,702,964 | **12.62%** |
| `medcpt-1m` | 1,000,000 | 332,724,070 | 63,059,916 | 360 | 96,357,100 | 301,800 | 416,200 | 160,135,376 | **48.13%** |
| `medcpt-10m-abs` | 10,000,000 | 7,705,567,013 | 543,884,239 | 1,096 | 963,487,236 | 573,140 | 431,421 | 1,508,377,132 | **19.58%** |
| `paperseek` (rung 4) | 102,117,343 | 70,783,071,941 | 486,589,550 | 4,368 | 204,234,702 | 35,072 | 102,302 | 690,965,994 | **0.98%** |
| `geonames` | 13,463,857 | 1,397,884,892 | 103,233,927 | 3,722,856 | 0 | 2,790,952 | 5,782,252 | 115,529,987 | **8.26%** |

`data/scratch/singleseg-1m` and `multiseg-1m` are `treeoflife-1m` rebuilds and are omitted. **No
`multiview` bundle is on the box** — `data/ladder/multiview/` holds sources only; the build-disk
memo's `multiview` row is a build made and discarded inside that probe.

Per level, the two largest corpora *(measured)*:

| bundle | layer/level | artifacts | entries | `.tsmb` B/entry | row column |
|---|---|---:|---:|---:|---|
| `gbif-64p` | `taxonomy/tree` L0 | 8,550 | 25,570,004 | 1.929 | `TSLB` u16, 51.7 MB |
| | L1 | 56,893 | 25,088,942 | 2.350 | `TSLB` u16, 51.7 MB |
| | L2 | 186,464 | 23,967,081 | 2.964 | — (artifact-major) |
| `medcpt-10m` | `clusters/kmeans` L0 | 135 | 10,000,000 | 1.665 | — |
| | `mesh/descriptors` L0 | 30,119 | 461,743,606 | 1.142 | `TSLL` u16, 963.5 MB |
| `geonames` | `admin/hierarchy` L0 | 254 | 13,463,459 | 0.231 | all artifact-major |
| | L1 | 4,842 | 13,427,070 | 1.091 | |
| | L2 | 51,951 | 9,784,100 | 2.282 | |
| | L3 | 175,963 | 4,119,849 | 5.033 | |
| | L4 | 231,645 | 828,625 | **23.063** | |

The `geonames` L4 row is where the framing dominates: 231,645 artifacts holding 828,625 members
between them — 3.6 members an artifact, and **23.06 B a member** of which 6.48 is the Roaring. That
one level is 19.1 MB, 18.5% of the whole membership store, for 1.2% of its entries.

**Rung-6 projection at the measured constants** *(modelled)*: 10,014,654,968 entries × 2.236 B =
**22.4 GB** of membership extent, plus 3 row-major levels over 3.65×10⁹ rows at 2/4/4 B =
**36.5 GB** of row column, against the campaign's ~219–244 GB modelled bundle — **25–27%**.
The build model's own line charges the extents 30,004 MiB at 3 B an entry.

---

## 5. At serve time

**What a request reads.**

| level layout | candidacy | count | source |
|---|---|---|---|
| artifact-major | tile index walk over row ranges | `mask.count_intersection(rows)` per candidate | `MembershipRows::rows: Vec<Option<Arc<Bitmap>>>` — **anonymous memory, row space** |
| row-major | one scan of `viewport ∩ M_auth`, `RowColumn::candidates` | per-session masked-count histogram | the **mapped** `.tslb`/`.tsll`, one random lookup a visible row |
| spatial | range-versus-tile arithmetic | `count_range` over the artifact's ranges | the level's shapes |

`crates/tessera-engine/src/artifacts.rs:186-199, 1367`; `row_column.rs:96-110, 603-623`.

**Nothing reads the `.tsmb` on a request.** It is opened once, at engine open, in
`Engine::open`'s seeding loop (`crates/tessera-engine/src/write.rs:2909-2950`): the pack is mapped,
every blob is `decode_record`d into an `ArtifactRecord` whose members are `Members::owned` — a
**heap** Roaring bitmap — and the mapping is dropped at the end of that extent's iteration. The
build's rehousing to `Members::mapped` (`layers.rs:2770`) is the build's only; `rehouse_members` has
no other caller.

Then `Engine::warm_artifact_projections` (`viewport.rs:4875`) builds, before the listener binds,
one **row-space** copy per `(view, layer, level)` — `RowSpace::project_base` over the level's whole
membership, or a transpose out of the row column where the prefix holds one. So a live server holds
the relation **twice** for a single-view corpus and **1 + views** times for a multi-view one, both
copies anonymous.

Measured residency at open, before any request *(`docs/ingest-campaign.md` §4b, §4.6,
`probes/2026-09-02-cold-start/`)*:

| bundle | artifacts | entries | views | open | anonymous |
|---|---:|---:|---:|---:|---:|
| `paperseek` rung-4 prefix | 5,054 | 496×10⁶ | 1 | — | **166 MB** |
| `medcpt` 36M | 30,217 | 1.66×10⁹ | 1 | 24.6–33.8 s | **2.06–2.17 GB** |
| `treeoflife` 233M | 1,001,193 | 1.63×10⁹ | 2 | 87.7 s | **15.96 GB** |

Two corpora with the same entry count differ by 7.7×: the cost tracks **containers and artifacts**,
not entries. The residency campaign's constant is 78.5 B a container
(`artifact-serving-at-scale.md` §5.1); `treeoflife` holds a million artifacts across two views where
`medcpt` holds thirty thousand across one, and `medcpt`'s DAG level is row-major so its column is
mapped rather than heap.

**Would shrinking the store on disk cost latency?** Plainly:

- **The `.tsmb`: no.** It is read once, sequentially, at open. Its size is open time and page-cache
  pressure, never request time. A format change that traded bytes for decode work would land wholly
  on `/readyz`, which for `treeoflife` is already 87.7 s.
- **The `.tslb`/`.tsll`: yes.** `RowColumn::candidates` does one random-access read into the mapped
  column per row of `viewport ∩ M_auth`. Any encoding needing more than a shift and a mask at that
  address is paid per visible row per request.
- **The framing (keys, headers, offsets): no, either way.** It is decoded at open and never touched
  again.

---

## 6. Compression and the floor

**Nothing about the membership store is compressed.** `zstd` is in the workspace for the record
blob's 256 KiB block frames (`crates/tessera-filter-write/src/record.rs:169`) and nowhere near
`tessera-store::membership`. `run_optimize` is called in `tessera-filter` and in two bench binaries
and never on a membership — confirmed by the container census: **0 run containers** in every extent
on the box.

**The measured floor, three ways.** GBIF `taxonomy/tree` level 0 — 8,550 families over 25,846,007
Morton-ordered rows, one artifact a row:

| | bits/row | B/row | against stored 2.00 B |
|---|---:|---:|---:|
| uniform over 8,551 labels | 13.06 | 1.633 | 1.22× |
| **zeroth-order entropy** (measured distribution) | **7.607** | **0.951** | **2.10×** |
| run-length model, at 22,872,550 runs | — | 1.686 | 1.19× |
| `gzip -9` over the raw label bytes | — | 0.855 | 2.34× |
| **`xz -6`** over the raw label bytes | — | **0.683** | **2.93×** |

The same for the other row-major levels on the box *(measured)*:

| level | ordinals | mean run | H₀ (B/row) | xz (B/row) | stored |
|---|---:|---:|---:|---:|---:|
| `gbif` family | 8,550 | 1.13 | 0.951 | 0.683 | 2.00 |
| `gbif` genus | 56,893 | 1.06 | 1.258 | 0.965 | 2.00 |
| `treeoflife` L4 | 2,528 | 1.10 | 1.014 | 0.970 | 2.00 |
| `treeoflife` L5 | 14,642 | 1.07 | 1.196 | 1.196 | 2.00 |
| `treeoflife` L6 | 51,920 | 1.07 | 1.436 | 1.314 | 2.00 |

And over the membership extents themselves, `xz -6` on the payload region *(measured)*: `gbif` L0
1.929 → 1.257 B/entry, `gbif` L2 2.964 → 1.864, `treeoflife` L6 9.365 → 2.156.

**What the numbers say.**

1. **Delta and bitpacking buy nothing here, because the ids are not clustered.** Mean run length is
   1.06–1.22 in row space and entries-per-run is 1.08–1.22 in entity space. A taxon is not a place,
   so Morton locality does not carry taxonomy. The 2 B array container is already within 22% of the
   *uniform* bound; a delta-and-bitpack scheme over a sorted 16-bit block cannot beat it by much,
   and a run container would be 2× worse.
2. **The 2× that is available is entropy coding of a skewed label distribution** — a handful of
   families hold most of the corpus. That is what takes 2.00 B to 0.95, and what `xz` beats by
   exploiting higher-order structure the entropy figure ignores.
3. **The barrier is the access pattern, not the encoding.** The row column must answer
   *which artifact is at row r* in constant time, per visible row, per request (§5); the membership
   extent must answer *which entities are in artifact a* as a Roaring bitmap the mask can be
   intersected with without a decode. Both are random-access reads into mapped bytes. Every scheme
   in the table above is a sequential decoder.
4. **The largest redundancy is structural and not a coding question at all.** For a tiered layer,
   `depth − 1` of the `depth` entries a row are a function of the tree and the leaf: GBIF stores
   10.01×10⁹ pairs where the leaf assignment plus the taxonomy is **3.34×10⁹**, and `treeoflife`
   stores 1.63×10⁹ where the leaf is **≈0.23×10⁹**. For MedCPT the closure is 5.58× the explicit
   assignments — 1.7×10⁹ against 3.1×10⁸. *(Measured entry counts; the "function of the tree" claim
   is exact except for the strays, §3, measured at 1.88% at GBIF level 0.)*
5. **On a taxonomy the framing is comparable to the payload and is not entropy-limited at all.**
   `treeoflife-1m`'s deepest level spends 73.67 B an artifact on a pipe-joined path key that is its
   parent's key plus one segment. That is 45.3% of the extent, and the extent is where it is because
   `encode_record`'s doc records that nothing else durable carries the key.


---

## 7. Observed while measuring: the derived files collide across views

Not part of the brief, and checkable in a minute. `derived_name`
(`crates/tessera-store/src/derived.rs:743`) is `"{kind}-{n:06}-{index:03}.{ext}"` where `index` is
the position in that call's `items` vector and `n` is a constant. `artifact_pass::run` is called
**once per view** (`crates/tessera-build/src/pipeline.rs:1943`) and its `items` restart at 0, so the
second view's files are written over the first's while both views' manifest entries are kept.

Measured on `data/ladder/treeoflife-1m/bundle` (two views, `bioclip` 1,000,000 rows and `geo`
760,259):

| | manifest entries | files on disk |
|---|---:|---:|
| `tile-index` | 9 | **5** |
| `row-column` | 6 | **3** |

`tile-index-000000-000.tsti` is named by `(bioclip, clusters/kmeans, 0)` and by
`(geo, taxonomy/tree, 0)`; the 64-byte file on disk is geo's. All three `row-column` files hold
760,259 rows, so the `bioclip` entries name geo's columns.

**No wrong answer results here**, because both adoption guards catch it:
`ArtifactRows::build_from_column` refuses on `column.base_rows() != space.base_rows()`
(`crates/tessera-engine/src/artifacts.rs:772`) and `build_over` refuses a tile index whose
`len()` is not the level's, with a warning. What is lost is the derived family's purpose for the
first view of every multi-view bundle — that view projects every membership at open instead of
adopting. The guard that stands between this and a wrong tile index is an **ordinal count**, so two
levels of two views with the same ordinal count over the same base row count would adopt each
other's index.
