# 0142 — The record blob delimits a row by a length the row states

**Date:** 2026-09-13 · **Status:** Settled (owner ruling)

## Context

[Decision 0141](0141-the-record-blob-states-identity-once-per-block.md) moved a row's identity into
the block that holds it and left the row's *extent* to `directory.arrow`: a rank-indexed `u32`
array, one entry per row in the blob, carried as a `LargeList<u32>` beside the block directory.
Its own "Not taken" section names this change and declines it, on the ground that moving the
lengths into the block leaves the block bytes the sole authority on where a row starts, with
nothing for the extent digest to compare them against.

What that section did not have is the rung 6 figure. Over the 3,495,729,729-row GBIF build
(517,992 blocks) `directory.arrow` is **14.4 GB against a 13.9 GB `blocks.bin`**: the addressing
is larger than the thing addressed. The same array is 4 B a row of anonymous memory in the writer
for the whole of the blob stage — `RecordBlobWriter` held **14 GB** of it at that rung, uncharged
by the residency model, which named those bytes as disk. Both figures are measured.

And it buys no read. Reaching any row of a block costs that block's decompress either way, so the
offset saves only the walk from the block's first row to the one wanted: a varint and an addition
apiece over bytes that were decompressed a moment earlier and are in cache.

## The decision

**A row states how many bytes of fields follow it, as a LEB128 varint, and the directory holds
nothing per row.** A block is:

```text
block := row_count u32 LE | first_rank u32 LE | first_entity u32 LE
         | gap × (row_count - 1) | row × row_count
row   := payload_len LEB128 varint | field*
```

`directory.arrow` keeps a row per block and its fifth column becomes the block's row count, four
bytes a block rather than four bytes a row. The count is derivable from the neighbouring first
ranks and is carried anyway, for the reason the compressed lengths are: it lets the has-row
cardinality be checked against the directory at open rather than at the first read of the last
block.

**The extent digest goes with the offsets it covered.** It existed to bind the directory's offset
slice to the bytes the directory addressed, and there is no second file left to disagree.

## What replaces the cross-check

**The tiling walk, made once per block load.** A block's rows are delimited by their own lengths,
so the block is checkable against nothing but itself: stepping over exactly the `row_count` rows
the header states, by the lengths they state, must land on the block's last byte, and no length
may leave the block on the way. `RecordBlob::header_of` walks it when the block is decompressed
and before any row is served, so every read path inherits it — a single-row read, the set read,
the full cursor walk, `self_check`, `tessera verify --deep` and the Python oracle alike.

It costs a varint a row over bytes just decompressed. Measured against the 163 µs a single-row
read cost on the previous format, the walk is in the noise beside the decompress.

Without it the format would be weaker than what it replaced. A length doctored to swallow its
neighbour puts the row after next under this entity's identity — the entity comes from the gaps,
which are untouched — and the read answers with another entity's fields. That is the substitution
`records-and-search.md` §3 (review B6) exists to refuse, and
`a_row_length_that_swallows_its_neighbour_refuses` pins it.

## What is caught, and by what

| failure | caught | by what |
|---|---|---|
| a wrong block — a corrupt directory offset, or `block_of` off by one | yes | the block's `first_rank` against the directory's, its `row_count` against the directory's, and the rank's distance from the first rank against the count |
| a rows section whose rows do not account for the block | yes | the tiling walk, before any row is served |
| a row length that runs past the block | yes | the same walk, on bounds |
| a wrong rank inside the right block | yes | the entity the block's gaps give that row, against the entity the rank resolved to |
| a has-row bitmap that renames a rank | yes | the block's `first_entity` against the bitmap's member at `first_rank`, and every row's gap-derived entity against the bitmap's member at its rank |
| a truncated or corrupt block | yes | zstd against the directory's uncompressed length, the walk against the block's length, the manifest digest over the file |
| a bitmap and a directory that disagree about how many rows there are | yes | the row counts summed against the has-row cardinality, at open |
| a schema/blob disagreement | yes | unchanged: a row is self-describing, so a tag or kind that does not match refuses rather than decoding a wrong value |

## What is not caught

**A rows section rewritten so that it still tiles.** Under 0141 the class left uncaught was a
corrupt *value* inside a row whose framing still fitted the directory's extent — a wrong value for
the right entity. The class here is wider. Someone who rewrites a block's bytes may re-cut the
rows as well as their contents: as long as the walk still visits `row_count` rows and still lands
on the block's last byte, the entities come from the gaps and are unchanged, so a neighbour's
bytes can be presented under an earlier entity's identity. The field walk narrows it — a row's
fields must consume its stated length exactly, and the tags a row carries are checked against the
blob-resident columns by the conformance walk — but it does not close it.

**What stands against it is the manifest's SHA-256 over `blocks.bin`** (records §7: a blob file
that is missing, short, or fails its digest refuses at open). That is the guard against forged
block bytes in general, and it was already the only guard against the 0141 class. What the format
gives up here is the ability to detect one *particular* forgery from a second file's disagreement,
which is an ability it only had while a second file held the offsets.

**Whether this takes a leak-register row is the owner's call, and it is open.** The argument that
it does not is 0141's: a principal receives another entity's record only if `blocks.bin` has been
rewritten, which is outside the artefact's own integrity boundary and refuses at the manifest
digest. The argument that it does is that 0141's own no-register claim rested on the class being
"a wrong value for the right entity", which is no longer the class.

## What it saved, measured

`attrs/record/` built before and after from the same inputs, whole-directory size and the two files
it is made of:

| corpus | `attrs/record/` | `blocks.bin` | `directory.arrow` |
|---|---|---|---|
| `gbif-64p` (64M rows) | **−38.0%** | +18.3% | −99.9% |
| `treeoflife-1m` | **−58.6%** | +5.3% | −99.9% |
| `medcpt-1m` | **−4.7%** | +3.2% | −99.5% |
| `medcpt-10m-abs` (10M rows, abstracts) | **−0.31%** | +0.83% | −97.2% |

The short-row corpora gain most, as under 0141 and for the same reason: the directory's four bytes
were a constant on top of almost nothing. `blocks.bin` rises because a one-byte varint is inside
the block's compression where the four-byte offset was outside it — 0.34 compressed bytes a row on
`gbif-64p` against the 1.67 the directory was spending.

The transient the writer no longer holds is the whole of the point at scale: 4 B a row of anonymous
memory, 14 GB at rung 6, against a 21.5 GB budget.

## Consequences

- **`bundle_format` moves from 9 to 10.** A 9 directory's fifth column is a list of row offsets
  where a 10 directory's is one row count a block, and a 9 block's rows carry no length, so a
  reader at either number frames the other's blocks against the wrong rules.
  [Decision 0048](0048-no-deployments-exist-so-delete-rather-than-support.md) has the bundles
  rebuilt.
- `contracts.md` §2.4 and §0.3, and `records-and-search.md` §3, describe the row form (r94).
- The residency model loses its 4 B/item record-blob directory term, and `RECORD_ROW_BYTES` —
  which still described the pre-0141 row at eight bytes — becomes three: an entity gap and a
  length, both usually one byte, charged at three so a wider corpus is not under-charged.
- The Python oracle transcribes the format and was amended with it; its blob walk is the same
  tiling walk.
- A random single-row read walks the gaps and now the lengths as well, which is two varints and an
  addition per preceding row of the block rather than one. Modelled, not measured for this format;
  the block decompress dominated both before it and after.
- **I10 as corrected by decision 0065 is unchanged.** Entity ids in a block are an index internal
  and cross no boundary.
