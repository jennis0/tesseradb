# What the disk forecast reads against a measured peak, after the terms were corrected

**Date** 2026-09-11. **Binaries** main at `00de804c` ("before") and `perf/disk-forecast` on top of
it ("after"), both `--release`. **Box** WSL2, AMD Ryzen 9 5900X, 12 cores, 47 GiB, local
NVMe-backed VHDX, 213 GB free on the output filesystem. **Corpora** four prefixes of
`data/ladder/gbif` (16.3×10⁶ to 125.8×10⁶ occurrences) for the forecast, `treeoflife-1m` and
`medcpt-1m` for the bundle comparison.

    BEFORE=/tmp/w/tessera-before AFTER=/tmp/w/tessera-after LADDER=data/ladder WORK=/tmp/w \
      bash probes/2026-09-11-disk-forecast/run.sh

Every figure below is **measured** unless it says otherwise. The peaks are not re-measured here:
they are `probes/2026-09-10-build-disk/`'s, sampled over the whole bundle root every 0.5 s as
allocated blocks, at the same four row counts.

## The forecast against the peak

| items | measured peak | before | | after | |
|---|---|---|---|---|---|
| 16,299,326 | 1.96 GB | 3.30 GB | 1.68× | **3.26 GB** | **1.66×** |
| 30,104,813 | 3.31 | 5.00 | 1.51 | **4.93** | **1.49** |
| 64,657,133 | 6.61 | 9.36 | 1.42 | **9.21** | **1.39** |
| 125,789,091 | 12.70 | 16.92 | 1.33 | **16.96** | **1.34** |

The before column reproduces the 2026-09-10 figures to the megabyte, which is what says the two
runs are comparable. **No row count reads below 1.0**, and the margin still falls with the corpus
because what is left of it is constants.

⊘ **The peaks are the older binary's, and main has moved.** Two changes since touch what a build
holds: the label-agreement tally is a file rather than anonymous memory, and a string column may
spill its characters as record-blob extents instead of filling an arena. Neither reaches these
four peaks. The tally is 4 B/item and goes back at the assignment, two phases before the join and
index the peak is in; and at these row counts the route keeps the arena the measurement saw, which
the forecast line says for itself — `0 of 1 string column(s) spill`. Reasoned from the phase each
term stands in, not re-measured.

The GBIF schema moves the forecast in two directions at once, which is why the ratio barely
changes.

The band phase **falls** — 715 MiB to 677 at 16.3×10⁶, 5,518 to 5,213 at 125.8×10⁶ — because
`postings.arrow` is charged a record a term at that term's own row count instead of 4 B a pair.
This relation is the shape that costs least under the new term: 253 country terms whose entities
are contiguous in Morton order, and the file measures **33,714 bytes** at 125.8×10⁶ occurrences
(`probes/2026-09-10-build-disk/200m-after.peak.tsv`) against the hundreds of megabytes either
charge gives it. The join phase carries the band terms, so it falls with them at the three smaller
row counts.

The index phase **rises** by the terms that were missing — the keyword dictionary the pass leaves
behind, and the spool `values.arrow` is assembled from — by 194 MiB at 16.3×10⁶ and 1,534 at
125.8×10⁶. At 125.8×10⁶ that is enough to move the peak phase from the join to the index, which is
where the last row's ratio rising rather than falling comes from.

## What this corpus does not exercise

GBIF declares no `text` column, mints no external id and draws its taxonomy on one view, so three
of the corrected terms are checked elsewhere.

- **The external-id sidecar**, corrected from 12 B/item to a measured 20.25. `medcpt-1m` built with
  `--mint-external-ids` writes 16,251,010 B of `external-ids-0.arrow` and 4,000,000 of
  `ext-locator.u32` over 10⁶ items. The sidecar's three buffers are 16,000,004 of that — a 4 B
  offset a row and one more at the end, an 8 B id and a 4 B entity — the locator is the other
  4,000,000, and the remaining quarter byte an item is Arrow framing. The same 20.25 holds at
  25,846,007 items on `gbif-64p` (`docs/evidence/memos/2026-09-10-disk-bundle-payload.md` §1).
- **The token index and `dict.bin`**, unit-tested against the shapes in
  `crates/tessera-build/src/residency.rs`.
- **A layer on a view group**, likewise. `views` holds declared names and one name can be a group,
  so a layer on a ten-view group writes ten lanes a level and declares one name.

## The bundle

Byte-identical either binary, on both corpora: `diff -rq` reports no differing file outside
`MANIFEST.json` and `CURRENT`, and the only manifest field that differs is `created_at`
(`docs/ingest-campaign.md` §4c). 68 files in `treeoflife-1m`'s bundle, 36 in `medcpt-1m`'s.

That is not free by construction. The forecast itself is printed and read by nothing that writes a
byte, but one build decision does read the model — the extent route, which compares the
entity-order tail against the free space — and the row framing is a term of that tail. So a change
to it can move a column's route, and the comparison above is what says it did not here.
