# Where a bundle's payload goes at rest, and what its floor is

**Status:** Fact-find, 2026-09-10, for the disk-use campaign. **Not normative, and nothing here is
decided.** Produced by a subagent reading main at 0c0b60b3 as one of eight parallel investigations;
it is that agent's read of the code, not a reviewed design. The owner's session independently
verified the `zoom > 16` refusal and `split32`'s halves — the Morton cell is the high 16 bits an axis and the residual the low 16, so at the deepest served zoom one tile is one cell and the residual is never a tiling input; the record blob's 0.634 blocks-to-characters on GeoNames is independently corroborated by the forecast fact-find's 0.567–0.750 on synthetic high-entropy values, so "half the characters" is not a ceiling. **Every other claim here is unverified** — re-check a figure
or a citation before relying on it.

Measurement tooling for figures the report marks as taken in its own session is in
[`probes/2026-09-10-disk-survey/`](../../../probes/2026-09-10-disk-survey/). The campaign's starting
point is [`2026-09-10-build-disk-weight.md`](2026-09-10-build-disk-weight.md), which these supersede
in the places they contradict it. The other fact-finds in this set are the sibling
`2026-09-10-disk-*.md` memos in this directory.


**Date** 2026-09-10. **Repo** `/home/joe/code/tessera` at main `0c0b60b3`, read-only.
**Box** WSL2, local NVMe-backed VHDX.

Every figure below is **measured** on a bundle in `data/` unless it says **modelled** or
**assumed**. Sizes are `st_size` (apparent) per file walked with `os.walk`; allocated blocks
(`st_blocks × 512`) were taken beside them and differ by **under 0.08% on every bundle**, so nothing
here is sparse and nothing is lost to block rounding. `du` on the same trees reads about 86 KB
higher because it counts directory inodes.

Companion to `docs/evidence/memos/2026-09-10-build-disk-weight.md`, which measured the build's
scratch. This measures what the bundle keeps, which is what serving pays for the life of the
deployment.

## 0. The bundles that exist

Seven built bundles under `data/`, five schemas:

| bundle | corpus | items | bytes | GB | B/item | `bundle_format` |
|---|---|---|---|---|---|---|
| `data/ladder/gbif-64p/bundle` | GBIF, 64 of 8,369 parts, spread | 25,846,007 | 2,123,831,429 | 2.12 | **82.17** | 8 |
| `data/ladder/geonames/bundle-final` | GeoNames | 13,463,857 | 1,397,884,892 | 1.40 | **103.82** | 3 |
| `data/ladder/treeoflife-1m/bundle` | TreeOfLife, 10⁶ prefix, two views | 1,000,000 | 187,779,713 | 0.19 | **187.78** | 8 |
| `data/ladder/medcpt-1m/bundle-probe` | MedCPT, no abstracts | 1,000,000 | 332,724,070 | 0.33 | **332.72** | 5 |
| `data/ladder/medcpt-1m-abs/bundle-probe` | MedCPT with abstracts | 1,000,000 | 798,433,620 | 0.80 | **798.43** | 5 |
| `data/ladder/medcpt-10m-abs/bundle-auto` | MedCPT with abstracts | 10,000,000 | 7,705,567,013 | 7.71 | **770.56** | 5 |
| `data/rung4-run/bundle` | PaperSeek, whole rung 4 | 102,117,343 | 70,783,071,941 | 70.78 | **693.15** | 5 |

`data/ladder/geonames/bundle-hull` and `bundle-names2` are two more GeoNames builds at the same
size and are not decomposed separately. **No Overture bundle exists on this box**; only its
`points.parquet` staging does. Rung 5 (TreeOfLife, 233×10⁶) and rung 6 (GBIF, 3.65×10⁹) have no
bundle here either, so the 10⁶ prefix and the 64-part spread stand for them.

⊘ **Three of the seven are at a stale `bundle_format`** (3 and 5 against the current 8) and a
current reader refuses to open them. The bumps 5→8 changed the *artifact* record blob's fields, the
segments manifest's runtime-declaration lists and a view descriptor
(`contracts.md` §0.3). None of them changed the per-item record blob's row format, the token index,
the geometry columns or `columns.arrow`, so the per-item figures below are comparable across the
three formats. Two consequences of age are visible and are called out where they matter: the labels
transpose `entities/terms/` post-dates the GeoNames build, and the external-id sidecar is a
build-time flag that two of the seven were given.

## 1. The measured decomposition

`residual` and `tessera_id` are separated out of `columns.arrow` by reading its Arrow buffers; the
remainder of that file is the declared render columns plus its per-column validity bitmaps.

### gbif, 64-part spread — 25,846,007 items, 82.17 B/item

| kind | bytes | B/item | % |
|---|---|---|---|
| external-id sidecar (build flag) | 523,382,622 | 20.25 | 24.6% |
| record blob | 296,560,497 | 11.47 | 14.0% |
| layers (members, row-column, containment, tile index) | 285,192,300 | 11.03 | 13.4% |
| view maps (permutation, row-entity) | 206,935,004 | 8.01 | 9.7% |
| labels (`entities/terms`) | 206,773,644 | 8.00 | 9.7% |
| wire identity (`tessera_id`) | 206,768,056 | 8.00 | 9.7% |
| geometry: Morton cell | 103,384,028 | 4.00 | 4.9% |
| geometry: residual | 103,384,028 | 4.00 | 4.9% |
| keyword dict + ordinals | 101,392,523 | 3.92 | 4.8% |
| scalar values, presence, category postings | 54,253,496 | 2.10 | 2.6% |
| render columns | 35,539,294 | 1.38 | 1.7% |
| access terms, oracle pairs | 230,450 | 0.01 | 0.0% |
| manifest, reports, segments json, global dict | 35,487 | 0.00 | 0.0% |
| **total** | **2,123,831,429** | **82.17** | |

Declares a `u8` render category, an indexed `keyword`, an indexed `u16`, a stored-only `keyword`,
one view and one tiered layer of three levels. **No text index**: the owner ruled out a `locality`
column at rung 6.

### geonames — 13,463,857 items, 103.82 B/item

| kind | bytes | B/item | % |
|---|---|---|---|
| render columns | 351,745,798 | 26.13 | 25.2% |
| scalar values, presence, category postings | 241,308,574 | 17.92 | 17.3% |
| record blob | 176,296,728 | 13.09 | 12.6% |
| text index (dict + postings) | 138,527,453 | 10.29 | 9.9% |
| layers | 109,747,735 | 8.15 | 7.9% |
| view maps | 108,667,248 | 8.07 | 7.8% |
| wire identity | 107,710,856 | 8.00 | 7.7% |
| **`MANIFEST.json` and reports** | 56,033,647 | 4.16 | 4.0% |
| geometry: Morton cell | 53,855,428 | 4.00 | 3.9% |
| geometry: residual | 53,855,428 | 4.00 | 3.9% |
| access terms, oracle pairs, segments json | 135,997 | 0.01 | 0.0% |
| **total** | **1,397,884,892** | **103.82** | |

Thirteen declared columns, eight vocabularies, one `text` column, two tiered layers, one view. The
`MANIFEST.json` is 56.0 MB because it inlines every vocabulary; that is a constant, not a per-item
cost, and it is 4.16 B/item only because the corpus is small. No `entities/terms` (the labels
transpose post-dates this build).

### treeoflife-1m — 1,000,000 items, two views, 187.78 B/item

| kind | bytes | B/item | % |
|---|---|---|---|
| keyword dict + ordinals | 43,430,212 | 43.43 | 23.1% |
| render columns (two views) | 31,030,960 | 31.03 | 16.5% |
| layers | 23,700,162 | 23.70 | 12.6% |
| external-id sidecar (build flag) | 20,251,010 | 20.25 | 10.8% |
| view maps (two views) | 15,437,836 | 15.44 | 8.2% |
| wire identity (two views) | 14,082,072 | 14.08 | 7.5% |
| labels | 8,000,234 | 8.00 | 4.3% |
| geometry: Morton cell (two views) | 7,041,036 | 7.04 | 3.7% |
| geometry: residual (two views) | 7,041,036 | 7.04 | 3.7% |
| record blob | 6,072,110 | 6.07 | 3.2% |
| `MANIFEST.json` and reports | 5,088,002 | 5.09 | 2.7% |
| text index | 3,278,322 | 3.28 | 1.7% |
| scalar values, presence, category postings | 3,270,504 | 3.27 | 1.7% |
| access terms, segments json, global dict | 56,217 | 0.06 | 0.0% |
| **total** | **187,779,713** | **187.78** | |

The `bioclip` view holds every row, the `geo` view 760,259 (76.0%). Everything per view is paid
twice at that ratio. `uuid`'s dictionary alone is **34.45 B/item**, 18.4% of the bundle.

### medcpt-1m (no abstracts) — 1,000,000 items, 332.72 B/item

| kind | bytes | B/item | % |
|---|---|---|---|
| layers | 159,719,176 | 159.72 | 48.0% |
| record blob | 61,124,779 | 61.12 | 18.4% |
| text index | 39,512,087 | 39.51 | 11.9% |
| labels | 22,405,682 | 22.41 | 6.7% |
| keyword dict + ordinals | 9,090,029 | 9.09 | 2.7% |
| render columns | 8,376,130 | 8.38 | 2.5% |
| view maps | 8,201,542 | 8.20 | 2.5% |
| scalar values, presence | 8,122,560 | 8.12 | 2.4% |
| wire identity | 8,000,000 | 8.00 | 2.4% |
| geometry: Morton cell | 4,000,000 | 4.00 | 1.2% |
| geometry: residual | 4,000,000 | 4.00 | 1.2% |
| access terms, manifest, reports | 171,982 | 0.17 | 0.1% |
| **total** | **332,724,070** | **332.72** | |

The MeSH DAG dominates: `row-column-000000-000.tsll` alone is 96.36 B/item and the two `.tsmb`
member files 63.06.

### medcpt-1m-abs — 1,000,000 items, 798.43 B/item

| kind | bytes | B/item | % |
|---|---|---|---|
| record blob | 381,390,881 | 381.39 | 47.8% |
| text index | 185,026,710 | 185.03 | 23.2% |
| layers | 159,647,472 | 159.65 | 20.0% |
| labels | 22,405,682 | 22.41 | 2.8% |
| keyword dict + ordinals | 9,090,029 | 9.09 | 1.1% |
| render columns | 8,376,130 | 8.38 | 1.0% |
| view maps | 8,201,542 | 8.20 | 1.0% |
| scalar values, presence | 8,122,580 | 8.12 | 1.0% |
| wire identity | 8,000,000 | 8.00 | 1.0% |
| geometry: Morton cell | 4,000,000 | 4.00 | 0.5% |
| geometry: residual | 4,000,000 | 4.00 | 0.5% |
| access terms, manifest, reports | 172,594 | 0.17 | 0.0% |
| **total** | **798,433,620** | **798.43** | |

The same corpus with one more declared `text` column adds **465.7 B/item**: 320.2 in the blob and
145.5 in the index (§4). `medcpt-10m-abs` reproduces the shape at 770.56 B/item with the layer
constant unchanged and the blob at 375.90.

### paperseek, rung 4 whole — 102,117,343 items, 693.15 B/item

| kind | bytes | B/item | % |
|---|---|---|---|
| **record blob** | 44,774,476,706 | **438.46** | **63.3%** |
| **text index (dict + postings)** | 20,111,130,522 | **196.94** | **28.4%** |
| keyword dict + ordinals (`openalex_id`) | 916,000,684 | 8.97 | 1.3% |
| view maps | 835,790,306 | 8.18 | 1.2% |
| labels | 816,960,773 | 8.00 | 1.2% |
| wire identity | 816,938,744 | 8.00 | 1.2% |
| layers | 690,863,692 | 6.77 | 1.0% |
| render columns | 587,176,054 | 5.75 | 0.8% |
| scalar values, presence | 415,876,845 | 4.07 | 0.6% |
| geometry: Morton cell | 408,469,372 | 4.00 | 0.6% |
| geometry: residual | 408,469,372 | 4.00 | 0.6% |
| access terms, manifest, reports, segments json | 918,871 | 0.01 | 0.0% |
| **total** | **70,783,071,941** | **693.15** | |

**Prose is 91.7% of this bundle.** The `abstract` column alone is 397.97 B/item in the blob
(measured, §2) plus 172.43 in the index: **570.4 B/item, 82.3% of the whole bundle**. `title` adds
another 67.3.

### The one-line comparison

| | gbif 64p | geonames | treeoflife-1m | medcpt-1m | medcpt-1m-abs | paperseek |
|---|---|---|---|---|---|---|
| record blob | 11.47 | 13.09 | 6.07 | 61.12 | 381.39 | **438.46** |
| text index | — | 10.29 | 3.28 | 39.51 | 185.03 | **196.94** |
| geometry (cell + residual) | 8.00 | 8.00 | 14.08 | 8.00 | 8.00 | 8.00 |
| wire identity | 8.00 | 8.00 | 14.08 | 8.00 | 8.00 | 8.00 |
| view maps | 8.01 | 8.07 | 15.44 | 8.20 | 8.20 | 8.18 |
| labels | 8.00 | — | 8.00 | 22.41 | 22.41 | 8.00 |
| layers | 11.03 | 8.15 | 23.70 | 159.72 | 159.65 | 6.77 |
| render + scalar + keyword | 7.40 | 44.05 | 77.73 | 25.59 | 25.59 | 18.79 |
| external-id sidecar | 20.25 | — | 20.25 | — | — | — |
| **total B/item** | **82.17** | **103.82** | **187.78** | **332.72** | **798.43** | **693.15** |

The 10× spread the campaign records is **entirely** the schema. A corpus with prose pays the blob
and the index; a corpus without pays 30 to 80 B/item of geometry, identity, addressing and
membership that barely moves with the schema at all.

## 2. The record blob

`attrs/record/{blocks.bin, directory.arrow, hasrow.roaring}`, plus a small per-artifact extent set
under `attrs/record/extents/`.

**Content.** Every declared field that neither renders nor indexes, and every `text` field's
values whatever its flags (`records-and-search.md` §3). One self-describing row per entity, rows
concatenated in ascending entity order.

**Format** as measured. ⊘ **The per-row framing below was replaced on 2026-09-11** by a block
header stating the row count, the first rank, the first entity and a digest of the directory's row
offsets, with one varint per row carrying its entity as a gap
([decision 0141](../../decisions/0141-the-record-blob-states-identity-once-per-block.md)); §2's
framing figures are what that ruling rests on, and the row shape here is the one they were taken
over.


```
row     := entity u32 LE | payload_len u32 LE | payload
field   := tag u16 LE | kind u8 | value      -- kind 12 utf8 adds byte_len u32 LE
```

**Codec.** zstd at level 3, `crates/tessera-filter-write/src/record.rs:63` (`ZSTD_LEVEL`), one
frame per block, blocks concatenated. Block target **256 KiB uncompressed**,
`crates/tessera-filter/src/record.rs:93` (`RECORD_BLOCK_TARGET`); a block seals when the next row
would pass it, so a row never splits (`record.rs:139-141`) and an oversized row gets a block of its
own.

**Confirmed independently**: recompressing a decompressed block at zstd level 3 through pyarrow
reproduces the shipped compressed length **to the byte** on 300-block samples of both
`medcpt-1m-abs` and `paperseek`.

### Compression, measured

Characters are the source column's value bytes, recovered by decoding every row of the blob (the
decoder reproduces the source Parquet's character totals exactly on all four corpora where both
were computed).

| corpus | blob-resident columns | chars B/item | framed B/item | `blocks.bin` B/item | **blocks ÷ chars** | framed ÷ compressed | rows/block |
|---|---|---|---|---|---|---|---|
| gbif 64p | `scientificname` | 32.041 | 47.041 | 7.343 | **0.229** | 6.41× | 5,571 |
| geonames | `name` | 13.469 | 28.469 | 8.536 | **0.634** | 3.34× | 9,203 |
| treeoflife-1m | `common_name` | 10.890 | 21.515 | 3.013 | **0.277** | 7.14× | 8,534 |
| medcpt-1m | `title`, `mesh_major` | 144.138 | 165.039 | 56.563 | **0.392** | 2.92× | 1,586 |
| medcpt-1m-abs | + `abstract` | 1058.251 | 1083.976 | 376.714 | **0.356** | 2.88× | 241 |
| medcpt-10m-abs | + `abstract` | 1057.657 | 1083.387 | 371.603 | **0.351** | 2.92× | 241 |
| paperseek | `title`, `abstract` | 1247.466 | 1269.481 | 434.176 | **0.348** | 2.92× | 205 |

The probe's **0.26 of characters is corpus-dependent and not a constant**. It holds within a factor
on short-string corpora (gbif 0.229, treeoflife 0.277) and is wrong by 2.4× on GeoNames (0.634) and
by 1.3–1.4× on the prose corpora (0.348–0.392). GeoNames is the case the build's forecast should
worry about: a 13.5-character name carries 15 bytes of row and field framing, so the blob's blocks
are **two-thirds of the characters** rather than a quarter of them.

The all-corpora range is **0.229 to 0.634**. The memo's stated ceiling of "half the characters"
is exceeded by GeoNames, so as a build-forecast ceiling it is not one.

### What the framing costs, measured

Compressing each block's values with the framing stripped, at the same level and the same block
cut, gives the framing's compressed cost directly:

| corpus | `blocks.bin` B/item | values only, compressed | **framing, compressed** | framing as % of blocks |
|---|---|---|---|---|
| gbif 64p | 7.343 | 4.216 | **3.127** | **42.6%** |
| geonames | 8.536 | 4.829 | **3.707** | **43.4%** |
| treeoflife-1m | 3.013 | 1.365 | **1.648** | **54.7%** |
| medcpt-1m | 56.563 | 47.161 | **9.402** | 16.6% |
| medcpt-1m-abs | 376.714 | 359.314 | **17.400** | 4.6% |
| paperseek | 434.176 | 417.792 | **16.384** | 3.8% |

Raw framing is 15 B for a one-`utf8`-field row (8 row header + 3 tag/kind + 4 length), 22 for
PaperSeek's two-field row and 25.7 for MedCPT's three-field row. Four of those bytes are the
entity discriminant, which review B6 requires as the fail-closed check against serving a
neighbour's record.

### Addressing

`directory.arrow` is one Arrow row per block carrying `(compressed_offset, compressed_len,
uncompressed_len, first_rank, row_offsets: LargeList<u32>)`. The flattened child is one `u32` per
has-row entity, so the file is essentially **4 B per entity with a row**:

| corpus | directory B/item | B per has-row entity | blocks | rows with a blob row |
|---|---|---|---|---|
| gbif 64p | 4.131 | 4.131 | 4,639 | 25,846,007 |
| geonames | 4.129 | 4.129 | 1,463 | 13,463,857 |
| treeoflife-1m | 2.926 | 4.314 | 83 | 708,333 |
| medcpt-1m-abs | 4.260 | 4.263 | 4,149 | 999,456 |
| paperseek | 4.284 | 4.284 | 497,318 | 102,117,343 |

`hasrow.roaring` is 290 B to 129 KB, so under 0.0002 B/item everywhere except treeoflife-1m (0.129,
where 29% of entities have no row).

### The read

`RecordBlob::fields_of` (`crates/tessera-filter/src/record.rs:729-737`): a has-row `contains`, a
rank, a binary search of the block directory, **one block read and one decompress**, then a
bounds-checked row decode with the entity discriminant verified. **One block per call, nothing held
between calls.** Its own doc quotes 163 µs per artifact name on GeoNames, and records §3 quotes
236–270 µs per random single-row read on the built writer.

`for_each_row_in(&Bitmap, ..)` amortises a set over the blocks it touches, one decompress per
block, which is what an artifact level's contiguous entity run takes.

So a drill-down decompresses **one 256 KiB block** to return one row. On PaperSeek that block holds
205 rows and 260,670 uncompressed bytes; on gbif it holds 5,571 rows.

## 3. The geometry

**Stored per item per view, 8 bytes:**

- `views/<v>/segments/seg-N/morton.u32` — the 32-bit Morton cell code, **16 bits per axis**, sorted.
  4.000 B/item, measured exactly on every bundle.
- `residual`, a `u32` column inside `columns.arrow` — the sub-cell interleave, **the low 16 bits per
  axis**. 4.000 B/item, measured exactly on every bundle.

Concatenated they are the 64-bit interleave of two 32-bit fixed-point axes:
**32 bits per axis** (`hot-row-geometry.md` §2; `crates/tessera-spatial/src/morton.rs:103-113`,
`split32`). The wire carries the concatenation as one `code: uint64` beside `tessera_id: uint64`
(`crates/tessera-wire/src/payload.rs:743`), so 16 B per point plus render scalars.

Beside them, per view per item: `permutation.bin` 4 B (entity → row), `row-entity.u32` 4 B
(row → entity), `tessera_id` 8 B inside `columns.arrow`.

**Total per view per item: 8 B of position, 8 B of identity, 8 B of entity↔row map.**
`treeoflife-1m` pays 1.76× that because it has two views.

### What the render path can show

The grid is 2¹⁶ × 2¹⁶ and a tile prefix is meaningless past depth 16
(`crates/tessera-spatial/src/morton.rs:197`, `frame.rs:30`). The server **refuses `zoom > 16`**
(`crates/tessera-server/src/viewer.rs:1804`). At zoom 16 one tile is exactly one Morton cell, so:

- the **cell code is fully used** at every zoom the service serves;
- the **residual is never a tiling input at all**. It is a within-cell position for drawing.

Drawing one whole cell across a 2,000-pixel viewport resolves about **11 bits per axis** of the 16
the residual carries (arithmetic). Reaching all 16 needs a client that keeps zooming inside one
tile until a single cell spans 65,536 pixels, which the server neither bounds nor helps with (the
bbox may be arbitrarily small at zoom 16).

### Where the stored precision exceeds what the corpus knows

| corpus / view | rows | distinct cells (16 b/axis) | cells ÷ rows | distinct residuals | log₂ | distinct x, log₂ | distinct y, log₂ |
|---|---|---|---|---|---|---|---|
| gbif 64p / `geo` | 25,846,007 | 3,508,005 | 0.136 | 5,522,773 | 22.4 | 22.2 | 22.1 |
| geonames / `world` | 13,463,857 | 11,543,951 | 0.857 | **621,260** | **19.2** | 22.7 | 22.4 |
| treeoflife-1m / `bioclip` | 1,000,000 | 997,722 | 0.998 | 999,875 | 20.0 | 19.9 | 19.9 |
| treeoflife-1m / `geo` | 760,259 | 452,388 | 0.595 | 621,008 | 19.2 | 19.2 | 19.2 |
| medcpt-1m-abs / `knn` | 1,000,000 | 997,159 | 0.997 | 999,872 | 20.0 | 19.9 | 19.9 |

**GeoNames is the clear case.** Its 13.46M points take only **621,260 distinct residual values out
of 2³²** (2^19.2), because the source is a five-decimal-degree grid. Under the declared
`web_mercator` frame the stored step is 40,075 km ÷ 2³² = **9.3 mm** against a source resolution of
about **1.1 m**: the stored grid is roughly **118× finer per axis than the data**, so about 6.9 bits
per axis carry nothing at any zoom. TreeOfLife's `geo` view, which is GBIF coordinates, reads the
same way at 2^19.2.

The greatest common divisor of the axis steps is 1 on every corpus, so the unused precision is not a
clean low-bit run: the f64 quantiser scatters a decimal grid across the `u32` range. It is
information the corpus does not have, not bits that are structurally zero.

Cost of the residual alone: **4.00 B/item/view**. 408.5 MB on PaperSeek, 53.9 MB on GeoNames,
14.6 GB at rung 6 (modelled: 4.00 × 3.654×10⁹).

## 4. The text and token index

Two files per declared `text` column, in entity space:
`attrs/<col>/postings.arrow` and `attrs/<col>/dict.bin`.

**Postings** (`crates/tessera-authz/src/postings.rs:1-8`): one Arrow IPC file, one
`LargeBinary` record per term, row ordinal = term id. A record is `u8 tag ‖ payload`: **tag 0** is a
sorted `u32` LE entity array when the term's count is at or below `small_term_threshold` (32 here),
**tag 1** is portable Roaring bytes.

**Dictionary** (`crates/tessera-filter/src/dict.rs:34-43`): front-coded blocks of K = 16 with a
`u64` restart offset per block, so 0.50 B/key of restart table plus the front-coded suffixes.

### Measured, per column

| corpus | column | terms | postings | postings/item | file B/item | **B per posting** | dict B/item | dict B/key |
|---|---|---|---|---|---|---|---|---|
| paperseek | `abstract` | 48,961,099 | 10,118,888,527 | 99.09 | **169.000** | **1.706** | 3.426 | 6.65 |
| paperseek | `title` | 8,676,778 | 1,165,771,974 | 11.42 | 23.973 | 2.100 | 0.542 | 5.88 |
| medcpt-10m-abs | `abstract` | 3,080,307 | 779,365,357 | 77.94 | 134.758 | 1.729 | 1.748 | 5.17 |
| medcpt-10m-abs | `title` | 883,683 | 120,493,428 | 12.05 | 24.569 | 2.039 | 0.535 | 5.56 |
| medcpt-10m-abs | `mesh_major` | 20,906 | 52,470,919 | 5.25 | 10.968 | 2.090 | 0.016 | 6.92 |
| medcpt-1m-abs | `abstract` | 747,202 | 77,981,405 | 77.98 | 141.308 | 1.812 | 4.225 | 5.15 |
| medcpt-1m-abs | `title` | 251,044 | 12,052,151 | 12.05 | 26.213 | 2.175 | 1.546 | 5.66 |
| geonames | `name` | 4,226,541 | 26,194,503 | 1.95 | 8.390 | 4.312 | 1.899 | 5.55 |
| treeoflife-1m | `common_name` | 11,815 | — | — | 3.192 | — | 0.087 | 6.83 |

**The unit is the posting, not the token.** PaperSeek's abstract index is 1.706 B per
(token, document) pair and its per-item cost, 169 B, is 99.09 postings × 1.706.

Split by encoding, PaperSeek `abstract`:

| | terms | postings | payload bytes | B/posting |
|---|---|---|---|---|
| tag 0, small lists | 47,565,081 (97.1%) | 108,565,814 (1.07%) | 481,828,337 | 4.44 |
| tag 1, Roaring | 1,396,018 (2.9%) | 10,010,322,713 (98.9%) | 16,378,168,359 | **1.636** |
| Arrow offsets, 8 B/term | — | — | 391,688,800 | — |

The vocabulary's long tail is real but not the cost: 97.1% of terms hold 1.07% of postings and take
873.5 MB with their Arrow offsets, 5.1% of the file, plus 349.9 MB of dictionary.

### Is the text index the largest file?

**No, on both corpora.** The record blob's `blocks.bin` is the largest single file in every bundle
that has prose:

| bundle | largest file | second |
|---|---|---|
| paperseek | `attrs/record/blocks.bin` 44.34 GB | `attrs/abstract/postings.arrow` 17.26 GB |
| medcpt-10m-abs | `attrs/record/blocks.bin` 3.72 GB | `attrs/abstract/postings.arrow` 1.35 GB |
| medcpt-1m-abs | `attrs/record/blocks.bin` 376.7 MB | `attrs/abstract/postings.arrow` 141.3 MB |
| gbif 64p | `entities/external-ids-0.arrow` 420.0 MB | `views/geo/.../columns.arrow` 345.7 MB |
| geonames | `views/world/.../columns.arrow` 513.3 MB | `attrs/record/blocks.bin` 114.9 MB |
| treeoflife-1m | `attrs/uuid/dict.bin` 34.45 MB | `views/bioclip/.../columns.arrow` 29.63 MB |

The `text_index` stage being PaperSeek's slowest at 28 m 08 s is a build-time fact about tokenising
119 GB of prose, not about the file it writes.

### A `text` column is stored twice, by design

Each declared `text` column pays the blob (for `entity → value` at drill-down and for `phrase`
verification) and the index (for `match`). Measured, with the blob share taken by compressing each
field's bytes alone at level 3 in the same block cut (sum of the parts exceeds the whole by 1.3% on
medcpt-1m-abs and 0.4% on paperseek, the credit joint compression takes):

| corpus | column | chars B/item | in the blob | in the index | **total B/item** |
|---|---|---|---|---|---|
| paperseek | `abstract` | 1160.51 | 397.97 | 172.43 | **570.40** |
| paperseek | `title` | 86.96 | 42.80 | 24.52 | **67.32** |
| medcpt-1m-abs | `abstract` | 914.11 | 320.19 | 145.53 | **465.72** |
| medcpt-1m-abs | `title` | 92.46 | 42.37 | 27.76 | **70.13** |
| medcpt-1m-abs | `mesh_major` | 51.68 | 18.81 | 11.74 | **30.55** |
| geonames | `name` | 13.47 | 8.54 | 10.29 | **18.83** |

GeoNames is the ratio to notice: a 13.5-character name costs **18.83 B/item**, 1.40× its own
characters, to be both returnable and searchable.

## 5. Vectors and embeddings

**Nothing of an embedding reaches a bundle. Tessera stores 2-D geometry and never a vector.**

- No file kind in any of the seven bundles carries one. The complete file inventory is the eight
  directories in §1's tables.
- `architecture.md` §8.3 names a vector sidecar ("cold, large, read in a completely different
  pattern") and Appendix A prices "Source embeddings (768-d float32), **if served**" at 31 GB per
  10⁷ and ~3 TB per 10⁹. The conditional is the whole of it: **no such sidecar is built**, and
  "Vector serving: are source embeddings served at view time?" is still an open question in
  architecture's own list.
- §10.3 states that the per-interaction row and §8.3's vector sidecar are **one slot**, whose only
  and explicitly transitional occupant is the external-ID store.
- The embedding is consumed upstream. TreeOfLife's 346 GB of `float16` were never staged: the
  layout was fitted on 2.5M rows and every row placed in one pass off the share, and what lands in
  the repo is `layout-bioclip.npy` (1.86 GB for 233×10⁶ positions, two `float32` per row) and then
  `points.parquet`'s `x`/`y`. The bundle receives `x`/`y` and quantises them.
- The only place `projection = "none"` appears is a view whose coordinates came from an embedding
  (`crates/tessera-spatial/src/projection.rs:83`), which is a statement about the *coordinates*, not
  about a stored vector.

**Embeddings are not a disk question for the bundle.** They are a disk question for the corpus
pipeline that produces `points.parquet`, and rung 5 already answered it by never staging them.

## 6. The floor, per kind

"Floor" is what the serve-time readers need, given the readers as written. Where a floor depends on
changing what a reader does, that is said.

| kind | spent B/item | floor given the reader | gap | what the floor rests on |
|---|---|---|---|---|
| blob `blocks.bin`, values | 4.22–417.8 | the same | **0** | it is already the compressed characters |
| blob `blocks.bin`, framing | 1.65–17.40 | > 0 | **1.65–17.40** | the row must be self-delimiting; 4 B of it is B6's discriminant |
| blob `directory.arrow` | 2.93–4.28 | 24 B/block | **≈ 4.1** | a row is findable by walking its decompressed block from `first_rank` |
| blob `hasrow.roaring` | ≤ 0.13 | the same | **0** | run-optimised over the entity set |
| token postings | 8.39–169.0 | 0-order entropy (below) | **1.64–1.97×** | Roaring buys random access and container-cost booleans |
| token dictionary | 0.02–4.23 | the same | **0** | front-coded at 3.7–6.9 B/key with 0.5 B of restart |
| geometry `morton.u32` | 4.00/view | 4.00 | **0** | sort key and tile row-range index; the grid is 2¹⁶ |
| geometry `residual` | 4.00/view | ~2.75 at zoom 16 on a 2,000-px viewport | **≈ 1.25** | 16 b/axis stored, ~11 b/axis showable at the deepest tiling served |
| `tessera_id` | 8.00/view | 0 stored, at a cost | **8.00** | a Feistel of `(shard, entity)` under a key the manifest already carries; `row-entity.u32` is present. §7.2's selection reads it **per masked candidate**, not per emitted mark (`select.rs:414, 620`), and rows are stored ascending by it within a Morton leaf, so this is a request-path compute trade and an owner question, not a saving |
| `permutation.bin` + `row-entity.u32` | 8.01–8.20/view | 8.00 | **0** | both directions are O(1) reads on the request path |
| labels `entities/terms` | 8.00–22.41 | 0 stored | **8.00–22.41** | the same relation is `terms/postings.arrow`, 0.0002–0.044 B/item |
| render columns | 1.38–31.03 | the declared widths | **0.125 per column** | every validity bitmap measured all-ones |
| scalar `values.arrow` + presence | 2.10–17.92 | the declared widths | **≈ 0** | Arrow framing is ~0.1 B/item per column |
| keyword ordinals | 4.13 | 4.13 | **0** | 4 B is forced past a 2¹⁶ vocabulary |
| keyword `dict.bin` | 0.03–34.45 | see below | **up to 3.5** on a unique-per-item column | a sorted front-coded dictionary against a scrambled blob row |
| layer members `.tsmb` | 2.40 B/entry | the same | **≈ 0** | the membership relation |
| layer `row-column` | 2.00/level/view | 0 stored | **2.00–96.36** | a transpose held beside the artifact-major form, by the module's own statement |
| external-id sidecar | 20.25 | 0 on a contiguous id range | **20.25** | arithmetic where source ids are a range; opt-in already |
| `MANIFEST.json`, reports | 56 MB, 5.1 MB | a constant | **0 per item** | dominant only at 10⁶ |
| `pairs.parquet` | 0.008–0.093 | 0 in production | **the whole file** | test-only oracle; `--no-oracle-pairs` already skips it |

### The postings floor, measured

Zero-order entropy of each term's posting list over the whole entity space,
Σ_t N·H(c_t/N) bits. This is a lower bound for any coder that stores the same sets and ignores
inter-term correlation. It is **not attainable** while the reader wants random access by term id
and boolean operations costed by containers touched.

| corpus | column | spent B/item | floor B/item | spent B/posting | floor B/posting | ratio |
|---|---|---|---|---|---|---|
| paperseek | `abstract` | 169.000 | **93.937** | 1.706 | 0.948 | **1.80×** |
| paperseek | `title` | 23.973 | **14.644** | 2.100 | 1.283 | **1.64×** |
| medcpt-10m-abs | `abstract` | 134.758 | **71.820** | 1.729 | 0.922 | 1.88× |
| medcpt-10m-abs | `title` | 24.569 | **14.181** | 2.039 | 1.177 | 1.73× |
| medcpt-10m-abs | `mesh_major` | 10.968 | **6.893** | 2.090 | 1.314 | 1.59× |
| medcpt-1m-abs | `abstract` | 141.308 | **71.689** | 1.812 | 0.919 | 1.97× |

### The keyword-dictionary floor on a unique-per-item column

Three corpora declare an indexed `keyword` whose vocabulary is one value per item:
`openalex_id` (8.97 B/item), `pmid` (9.09), `uuid` (38.58). The dictionary is a per-item unique
string; the ordinal is its rank.

Measured on `data/ladder/paperseek-1m/points.parquet`'s 10⁶ `openalex_id` values (10.95 chars mean),
zstd-3 in 256 KiB blocks:

| form | B/item |
|---|---|
| sorted, values only (what `dict.bin` approximates) | 3.99 |
| as-arrived order, values only | 4.83 |
| as-arrived order, framed as a blob row | **5.47** |
| **spent: `dict.bin` 4.845 + ordinals 4.125** | **8.97** |

So a blob-resident form would be about 5.47 B/item against 8.97 spent, on a 10⁶ prefix (⊘ measured
on the prefix, not the 102M corpus, and the prefix's identifiers may compress differently). It
loses `eq` and prefix search on that column, so it is a capability trade rather than a saving.
`records-and-search.md` §3 already records the shape of this finding for a near-sequential
identifier (blob ~4.6 B against DICT+C's 6.1); this is the measurement on real identifiers.

### An indexed-and-rendered column is stored twice

Measured, and it follows directly from §3's three-homes rule (row space for `render`, entity space
for `index`):

| corpus | column | row space | entity space | total |
|---|---|---|---|---|
| paperseek | `publication_year` i32 | 4.000 | 4.073 | 8.07 |
| medcpt-1m-abs | `published` timestamp_us | 8.000 | 8.123 | 16.12 |
| geonames | `country` u16 | 2.000 | 2.125 | 4.13 |
| geonames | `feature_code` u16 | 2.000 | 3.272 | 5.27 |
| geonames | `feature_class` u8 | 1.000 | 1.701 | 2.70 |

The two copies serve per-mark and per-query cadences respectively (§10.3), so the gap is zero
against the design. It is worth naming because it is how GeoNames reaches 44 B/item of columns.

### Ranked gaps at rung 6

⊘ **Modelled**, by scaling the 64-part spread's measured B/item to 3,654,488,638 items. The whole
bundle models to **300.3 GB** with the external-id sidecar and **226.3 GB** without; the campaign's
own independent model is ~219 GB, which the second figure agrees with to 3%.

| rank | gap | B/item | **GB at rung 6** |
|---|---|---|---|
| 1 | external-id sidecar, if minted | 20.25 | **74.0** |
| 2 | `entities/terms` labels transpose | 8.00 | **29.2** |
| 3 | `tessera_id` stored rather than derived | 8.00 | **29.2** |
| 4 | blob per-entity offsets in `directory.arrow` | 4.13 | **15.1** |
| 5 | geometry residual (whole column) | 4.00 | **14.6** |
| 6 | `row-column` transpose beside the members (two of three levels) | 4.00 | **14.6** |
| 7 | blob row framing, compressed | 3.13 | **11.4** |
| 8 | `columns.arrow` all-ones validity bitmaps | 0.375 | **1.37** |
| | **sum, excluding rank 1** | 31.63 | **115.5 of 226.3** |

**Half of a rung-6 bundle is addressing, identity and transposes rather than payload.** GBIF's
declared payload — one `u8`, one `u16`, two `keyword`s, one taxonomy — is 82.17 B/item of bundle,
and 31.63 of it is in the table above.

### Ranked gaps at PaperSeek's shape

Measured, at 102,117,343 items against a 70.78 GB bundle.

| rank | gap | B/item | **GB** |
|---|---|---|---|
| 1 | token postings above the entropy floor (`abstract` 75.06 + `title` 9.33) | 84.39 | **8.62** |
| 2 | blob row framing, compressed | 16.38 | **1.67** |
| 3 | `openalex_id` dict + ordinals against a blob-resident form | 3.50 | **0.36** |
| 4 | `entities/terms` labels transpose | 8.00 | **0.82** |
| 5 | `tessera_id` stored rather than derived | 8.00 | **0.82** |
| 6 | blob per-entity offsets | 4.28 | **0.44** |
| 7 | geometry residual (whole column) | 4.00 | **0.41** |
| 8 | `row-column` transpose | 2.00 | **0.20** |
| 9 | validity bitmaps (5 columns) | 0.625 | **0.06** |
| | **sum** | 131.2 | **13.4 of 70.78 (19%)** |

**The other 81% is prose that both readers want.** `abstract` at 570.4 B/item is 82.3% of the
bundle: 397.97 in the blob because drill-down and `phrase` read it, 172.43 in the index because
`match` reads it. Neither is a gap against the design as written. The only lever on that 82% is the
schema.

### Where the gap is zero

- `morton.u32`. It is the sort key, the tile row-range index and the wire's high half. 4 B is the
  grid.
- `permutation.bin` and `row-entity.u32`. Both directions are read on the request path.
- The token dictionary. Front-coded at 3.66–6.92 B/key with a 0.50 B/key restart table.
- `values.arrow` and its presence bitmap. Declared width plus about 0.1 B/item of Arrow framing.
- Keyword ordinals at 4 B. Forced by a vocabulary past 2¹⁶.
- Layer member entries at 2.40 B (gbif 64p, measured: 179,308,788 B over 74,626,027 membership entries) against the memo's charged 3.
- `blocks.bin`'s value bytes. Already the compressed characters at zstd-3.
- `hasrow.roaring` and the access-relation `terms/postings.arrow`. Both under 0.05 B/item on every
  corpus; gbif's whole access relation for 25.8M items and 253 terms is **12,530 bytes**.

## 7. What is not measured

⊘ **No bundle above 1.02×10⁸ items exists on this box.** Rung 5 is represented by a 10⁶ prefix and
rung 6 by a 64-part spread of 2.58×10⁷. Every rung-6 figure is that spread's per-item cost
extrapolated, and the campaign warns that a fraction of GBIF's parts is not a sample of it: the
spread reads 96.95% coordinate coverage against the whole corpus's 82.5%.

⊘ **Three bundles are at a stale `bundle_format`** and a current binary refuses to open them. The
5→8 bumps do not touch the structures decomposed here, but nothing re-verified that by rebuilding.

⊘ **The per-column blob split is a re-compression, not the shipped bytes.** §4's "in the blob"
column compresses each field's bytes alone at level 3 in the same block cut. The parts sum to 1.3%
(medcpt) and 0.4% (paperseek) above the whole, which is joint compression's credit and is not
attributable to either column.

⊘ **The `openalex_id` blob comparison is on the 10⁶ prefix**, not on the 102M corpus.

⊘ **The entropy floor is a bound on content, not on a usable format.** It assumes an ideal coder
with no random access and no set operations. Nothing here says a coder within 1.8× of it would
serve.

⊘ **The residual's "~11 bits showable" is arithmetic on an assumed 2,000-pixel viewport**, not a
measurement of any client. What is measured is the corpora's own distinct residual counts and the
server's `zoom > 16` refusal.

⊘ **Nothing here was measured under memory pressure or with a cold cache.** These are file sizes.
What serving actually maps and faults is Appendix A's question, not this one.
