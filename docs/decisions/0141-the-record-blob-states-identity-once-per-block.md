# 0141 — The record blob states a row's identity once per block

**Date:** 2026-09-11 · **Status:** Settled (owner ruling)

## Context

Every row of the record blob carried an eight-byte header: the entity the row belonged to, and the
row's payload length. On a one-`utf8`-field row that header plus the field's own tag, kind and
length is 15 bytes of framing around the value, and compressed it is
**42.6% of `blocks.bin` on `gbif-64p`, 43.4% on `geonames`, 54.7% on `treeoflife-1m`** and 3.8% on
PaperSeek (measured, [`2026-09-10-disk-bundle-payload.md`](../evidence/memos/2026-09-10-disk-bundle-payload.md)
§2). Modelled forward, that is about 11.4 GB at rung 6.

Most of it was not waste. Reaching a row is three derived indirections — the has-row rank, the
block that holds that rank, the offset within that block — and any one of them can be off by one
while every byte on disc matches its digest. The result is a different entity's record served out
of a block that also holds entities the principal cannot see, with no symptom. The entity in the
row header is what refused that, and `records-and-search.md` §3 (review B6) requires the refusal.

Every storage engine writes that check. They write it **per page**: InnoDB puts the page number in
a 38-byte header on a 16 KB page, about 0.3%. Tessera was putting the entity in a 15-byte header on
a 30-byte row.

## The options

**A. Keep the per-row entity.** No change; the check is where it is and costs what it costs.

**B. Move the identity to the block and keep nothing per row.** Blocks are already addressed by
first rank, so a block can state its first rank and first entity and nothing else. This loses the
wrong-rank-inside-the-right-block case entirely, which a corrupt has-row bitmap produces.

**C. Move the identity to the block and keep a one-byte check digit per row** — the entity's low
eight bits, catching 255 of 256 wrong-rank reads at a quarter of the cost.

**D. Move the identity to the block, as a column.** The block states its first entity once and then
one LEB128 varint per row holding that row's entity as a distance from its predecessor, less the
one that strict ascent already gives. The identity of every row is still in the block that holds
it, still checked at every read, and still exact.

## What was measured

Re-encoding every block of four built bundles under each candidate and compressing at the shipped
level, block cut for block cut (the control reproduces the shipped `blocks.bin` byte for byte):

| framing | gbif-64p | geonames | treeoflife-1m | medcpt-1m |
|---|---|---|---|---|
| as shipped, per-row entity + length | 7.028 B/row | 8.217 | 4.254 | 58.226 |
| C: one-byte check digit | **+0.4%** | −0.1% | **+8.3%** | −2.0% |
| interleaved varint gap per row | +4.1% | +2.7% | +4.9% | −0.5% |
| B: no identity, keep the row length | −18.9% | −16.9% | −25.6% | −4.6% |
| B: no identity, no row length | −37.4% | −26.0% | −43.9% | −7.8% |
| **D: columnar varint gaps, no row length** | **−37.4%** | **−26.0%** | **−39.6%** | **−7.8%** |
| no framing at all, values only | −43.9% | −44.8% | −54.7% | −16.7% |

Two results decided it.

**The cheap middle is not cheap.** A one-byte check digit costs *more* compressed than the full
four-byte entity on the two corpora where the framing share is worst. Ascending entity ids are a
near-perfect prediction and zstd charges almost nothing for them; the low byte alone is the one
part that carries entropy, and stripping the three predictable bytes around it removes matches the
compressor was using. Option C buys a weaker check at a higher price.

**Columnar identity is free.** Against the same encoding with no identity at all, D costs 0.002
B/row on `gbif-64p`, 0.004 on `medcpt-1m`, nothing measurable on `geonames`, and 0.18 B/row on
`treeoflife-1m`, where 29% of entities have no row so the gaps are not all one. Full per-row
identity at the price of no identity is not a trade.

## What it saved, measured on the rebuilt bundles

`blocks.bin`, built before and after from the same inputs. The block cut moves with the row size,
so a corpus repacks slightly more rows per block and the realised saving is a little larger than
the same-cut table above:

| corpus | before | after | saving | B/item before → after |
|---|---|---|---|---|
| `gbif-64p` (25,846,007 items) | 189,787,007 | 117,546,801 | **−38.1%** | 7.34 → 4.55 |
| `treeoflife-1m` (1,000,000 items) | 3,013,406 | 1,764,264 | **−41.5%** | 3.01 → 1.76 |
| `medcpt-1m` (1,000,000 items) | 56,563,227 | 51,150,397 | **−9.6%** | 56.56 → 51.15 |
| `multiview` | no per-item blob | | | |

Whole bundles fall by 3.4%, 0.7% and 1.3%. `directory.arrow` is unchanged: the addressing it
carries did not move.

**The prose corpora gain little and that is the expected shape.** Framing is a constant per row and
a MedCPT row carries 149 characters against GBIF's 34, so the same bytes are a sixth of the share.
The ruling's target was the short-row corpora, where the framing was a constant on top of almost
nothing.

## The decision

**D, and the per-row payload length goes with the entity.** A block is:

```text
block := row_count u32 LE | first_rank u32 LE | first_entity u32 LE | extent_digest u64 LE
         | gap × (row_count - 1) | row × row_count
```

(The extent digest and the directory's offsets left the format two days later; see the section at
the end.)

A row is its fields and nothing else, delimited by the directory's rank-indexed offsets, and its
field walk must consume that extent exactly.

The payload length was redundancy against the directory, and the check it fed — this row ends where
the directory puts the next one — was the only thing comparing the two files. **The extent digest
replaces it and covers more**: FNV-1a over the directory's whole row-offset slice for the block and
the rows section's length, taken by the writer over the offsets it hands the directory and by the
reader over the offsets the directory holds. Where the row length caught a disagreement at the one
row being read, the digest catches any disagreement anywhere in the block, before a row is read.

## What is caught, and by what

| failure | caught | by what |
|---|---|---|
| a wrong block — a corrupt directory offset, or `block_of` off by one | yes | the block's `first_rank` against the directory's, and the rank's distance from it against the row count |
| a directory that disagrees with the bytes it addresses | yes | the extent digest, over every offset of the block and the rows section's length |
| a wrong rank inside the right block | yes | the entity the block's gaps give that row, against the entity the rank resolved to |
| a has-row bitmap that renames a rank | yes | the block's `first_entity` against the bitmap's member at `first_rank`, and every row's gap-derived entity against the bitmap's member at its rank |
| a truncated or corrupt block | yes | zstd against the directory's uncompressed length, the row extents against the rows section, the manifest digest over the file |
| a schema/blob disagreement | yes | unchanged: a row is self-describing, so a tag or kind that does not match refuses rather than decoding a wrong value |

## What is no longer caught

**Corruption inside a row's bytes that still frames as a whole field sequence filling the row's
extent.** Under the old framing the row stated its own length, so a byte pattern that framed
differently from the directory's offsets was refused arithmetically. Now the field walk is what
stands there: an unknown kind byte refuses, a length running past the extent refuses, and a walk
that ends short of the extent refuses, but bytes that happen to frame exactly do not. The manifest's
SHA-256 over `blocks.bin` is the guard behind it, and it is the guard that was always behind
payload corruption anyway — the old check covered a corrupt *length field*, never a corrupt value.

This is not a new disclosure channel and takes no leak-register row: the failure it names is a
wrong value for the right entity, not another entity's record. Every route by which a principal
could receive an entity's record they may not see is closed in the table above.

## Consequences

- **`bundle_format` moves from 8 to 9.** An 8 bundle read at 9 takes the first row's entity id for
  a block's row count; a 9 block read at 8 serves a block header as an entity's fields. The number
  is what stops either, and [decision 0048](0048-no-deployments-exist-so-delete-rather-than-support.md)
  has the bundles rebuilt.
- `contracts.md` §2.4 and §0.3, and `records-and-search.md` §3, describe the block form (r93).
- **I10 as corrected by decision 0065 is unchanged.** Entity ids in a block are an index internal
  and cross no boundary; there is one fewer copy of them on disc than before.
- `tessera verify --deep` walks every block of every record-blob layer and reports the row count.
  Nothing walked the blob's addressing offline before, so a fold or coalesce that corrupted it
  would first have been seen by a viewer receiving another principal's record.
- The 256 KiB block target is measured over a block's rows, the header being addressing.
- **The shared pytest fixtures now rebuild on a format change.** Both are built once per machine at
  a fixed path and reused on a stamped receipt of the *inputs*; the format number is a property of
  the engine, not an input, so a fixture built at 8 was reused at 9 and its blocks decoded under the
  wrong rules. `harness.bundle_format_matches` is the structural gate that stops it, for this bump
  and the next.
- The Python oracle transcribes the format and was amended with it; the conformance suite's blob
  walk checks the block header against the directory and the has-row bitmap as the reader does.
- A random single-row read walks the gaps to the row it wants, which is one byte read per preceding
  row of the block: up to 6,158 of them on the rebuilt `gbif-64p`, about 4% of the read at a byte a
  nanosecond (modelled). Measured end to end, 2,000 random single-row reads on that bundle cost
  **133.5 µs each**, against the 163 µs `fields_of` records for the previous format on GeoNames and
  the 236–270 µs records §3 quotes; the block decompress dominates either way.

## How the content was checked

Every entity's fields must read back exactly as before, and the bytes are expected to differ, so
the comparison is over decoded contents. `gbif-64p`, `treeoflife-1m`, `medcpt-1m` and `multiview`
were built before and after from the same inputs with the same flags, and every row of every layer
decoded by a standalone Python reader of the blob's bytes — not the reader being changed — into
`(entity, [(tag, kind, value)])`, digested in rank order and again as a multiset of field sets.
All four corpora agree entity for entity, 25,846,007 rows on `gbif-64p` and 1,028,678, 708,350 and
30 on the others. `tessera verify --deep` walks all four clean, and a rebuilt bundle served over
the three planes answers `POST /v1/items/{tessera_id}` with values the blob's own bytes hold.

## Not taken here, taken two days later

**The directory's row offsets could move into the block too**, as varint row lengths, and
`directory.arrow` would lose its `LargeList<u32>`. That file is a measured **4.13 B per entity with
a row**, larger than the whole saving above, against a measured 0.71 B/row for the lengths inside
the block. It is the larger prize and it is a different change: it would leave the block bytes the
sole authority on where a row starts, with no second file to disagree with them, and the
cross-check this decision rests on would have nothing to compare.

That is what [decision 0142](0142-the-record-blob-delimits-a-row-by-a-length-the-row-states.md)
rules, on the rung 6 figure this section did not have: `directory.arrow` is 14.4 GB against a
13.9 GB `blocks.bin` there, and the same array is 14 GB of anonymous memory in the writer. What
replaces the cross-check is a tiling walk made once per block load — the rows a block states must
account for the block exactly — and the extent digest below goes with the offsets it covered. The
block form this decision rules is otherwise unchanged; a row now states its own length ahead of
its fields.
