# Every Arrow file in a bundle carries a validity bitmap that says nothing

**Status:** Fact-find with a measured figure and a declined change, 2026-09-11. **Not normative.**
The work exists on the branch `perf/no-allones-validity` (`005ae12a`), which is **deliberately not
merged** — see "Why it was not taken". The figures below are measured on this box; the rung-6 ones
are that per-item rate extrapolated and nothing above 2.58×10⁷ items was built.

## The finding

Every field in every Arrow IPC file a bundle holds is declared **non-nullable**, and every array
reaches the writer with `ArrayData::nulls()` of `None`. The files carry a validity bitmap anyway —
`⌈n/8⌉` bytes per column, every bit set.

**The bitmaps are not written by this repository.** `arrow-ipc`'s `write_array_data`
(`arrow-ipc-59.1.0/src/writer.rs:2098`) synthesises `MutableBuffer::with_bitset(⌈n/8⌉, true)` when
an array has no null buffer, and `IpcWriteOptions` carries no setting that suppresses it. So no
change to how Tessera builds its arrays can remove them; the bytes are added below that boundary.

## What it costs

Measured, building each corpus twice from the same inputs and diffing the bundles.

| corpus | items | Arrow bytes before → after | saved | B/item | share of bundle |
|---|---|---|---|---|---|
| `gbif-64p` | 25,846,007 | 1,024,884,412 → 999,349,244 | 25,535,168 | 0.988 | **1.202%** |
| `treeoflife-1m` | 1,000,000 | 69,810,904 → 66,358,296 | 3,452,608 | 3.453 | **1.992%** |
| `multiview` | 21,300 | 2,503,454 → 2,439,902 | 63,552 | 2.984 | 0.864% |

By file kind, bytes saved and B/item:

| file kind | `gbif-64p` | `treeoflife-1m` | `multiview` |
|---|---|---|---|
| `views/*/segments/*/columns.arrow` | 9,692,352 / 0.375 | 2,861,248 / 2.861 | 43,776 / 2.055 |
| `entities/external-ids-*.arrow` | 6,461,568 / 0.250 | — none minted | — |
| `attrs/*/values.arrow` | 6,147,200 / 0.238 | 500,224 / 0.500 | 16,128 / 0.757 |
| `attrs/record/directory.arrow` | 3,233,984 / 0.125 | 88,896 / 0.089 | — |
| `postings.arrow` (terms and attrs) | 64 | 1,856 | 3,264 |

It is larger than a single `n/8` because it is `n/8` **per column**, and because it reaches every
Arrow file rather than `columns.arrow` alone — 3,230,784 of `directory.arrow`'s 3,233,984 B on
`gbif-64p` is the `large_list` *child* node's bitmap, which spans every row offset.

⊘ **Modelled at rung 6** (3,495,729,729 items), linear in items at a fixed schema: ~3.45 GB on a
`gbif-64p`-shaped schema, ~12.1 GB on a `treeoflife-1m`-shaped one.

⊘ **The same bytes are on the wire.** A `/v1/viewport` response carries them too — measured 58,304
B of a 1,723,909 B response (**3.38%**) on `treeoflife-1m` at zoom 6, *k* = 5000. `tessera-wire`'s
writer was not changed and is not counted above.

⊘ **The delta tier's writer is unmeasured on disk.** None of the three corpora has a delta tier
file, so its share is covered by unit tests alone.

## What removing them would take, and why it was not taken

The branch adds a leaf crate, `tessera-ipc` (1,050 lines), whose `NullFreeWriter` sits **underneath**
`arrow::ipc::writer::FileWriter` as a `Write` adapter: it rewrites each record-batch message's
buffer table so an all-ones validity buffer has length 0, and drops those bytes as the body streams
past. Six writers route through it. It builds no flatbuffer, reads every hand-located value back
through arrow's own accessors before writing, passes an unrecognised type through byte-identically,
and refuses to strip where 64-byte alignment would not hold. Its verification is thorough: decoded
tables compared over all 225 Arrow files of three corpora, `verify --deep` clean, two servers run
side by side over 30 viewports and five filter cases, an ingest-and-flush check for
[decision 0091](../../decisions/0091-build-is-ingest-into-an-empty-database.md), and a framing state
machine driven at chunk sizes 1, 3, 7, 8, 9, 63, 64, 65 and 1024.

**No artifact version bump would be owed.** No discriminant moves; what changes is a buffer entry's
length, from `⌈n/8⌉` to 0, which the Arrow columnar spec permits wherever the field node's
`null_count` is 0. Every reader in the contract dispatches on the field node rather than the buffer —
arrow-rs's `create_primitive_array`, arrow C++/pyarrow's `LoadCommon`, arrow-js's `readNullBitmap` —
so neither shape can be misread by the other.

**The owner declined it on 2026-09-11**, and the reason is the failure mode rather than the size of
the change: a writer that rewrites another library's binary framing fails by emitting **wrong bytes**,
not by failing to build. An `arrow-rs` release that reordered a buffer table or changed its padding
would pass every test here, because the tests assert that today's arrow decodes our output. That is a
silent-corruption dependency on a third party's internals, against `CLAUDE.md`'s rule to prefer the
construction that is obviously correct.

**The fix belongs upstream.** `null_count == 0` writing a length-zero validity buffer is a small
change in the crate that owns the format, correct for every reader, and costs this repository
nothing to carry.

## What needs no action

The disk forecast is **right as it stands**: `residency.rs` charges `arrow_columns × ⌈n/8⌉` because
those bytes are written. Declining the change leaves nothing in the model wrong.

Related: [`2026-09-10-disk-bundle-payload.md`](2026-09-10-disk-bundle-payload.md), whose
`columns.arrow` figure of 13.375 B/item against 13.000 of declared widths this finding explains.
