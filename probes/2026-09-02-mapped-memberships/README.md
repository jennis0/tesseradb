# The build's published memberships, mapped — and the transient the artifact pass was hiding

**Date** 2026-09-02. **Binaries** built in the `build/mapped-memberships` worktree from its base
`1724ce63` (*before*) and from the change (*after*), `cargo build --release -p tessera-cli`, run
from that worktree's own `target/`. **Corpora** `medcpt-1m` and `medcpt-10m-abs` built with
`corpus-noabs.toml` — the 10⁷ **control** arm of `probes/2026-09-02-text-peak-split/`, 471,778,374
closed MeSH member rows over 30,954 descriptors. **Box** WSL2, 12 cores, 47 GB, local NVMe.
⊘ Two other sessions held long jobs on the box throughout; see §6.

## The finding

**Two structures, 3.1 GB between them, and neither of them had to exist.** At 10⁷ rows the build's
anonymous high-water was 4,445 MiB, all of it arriving in the `manifests` window. It is now
**2,682 MiB** — a **40% cut**, measured, on a control whose bundle is byte-identical.

| 10⁷ control | before | after | |
|---|---|---|---|
| anonymous high-water | 4,445 MiB | **2,682 MiB** | −1,763 MiB, −39.7% |
| file-backed high-water | 1,909 MiB | 2,383 MiB | +474 MiB — the mapped extent |
| `VmHWM` | 4,682 MiB | 4,179 MiB | −503 MiB |
| whole run | 236.19 s | 230.25 s | −2.5% |
| bundle | 3,163,528,625 B | 3,163,528,625 B | byte-identical (§4) |

The two structures, both attributed by measurement rather than by reading (§2, §3):

- **The store's published memberships** — one heap Roaring bitmap per artifact, live from the
  layers stage to the artifact pass four stages later. **~1.2 GB**, 2.7 B a closed member row.
  They are now a view over the packed extent `layers.rs` writes and fsyncs a moment later
  (`Members::mapped`): one write, no second format, and what stays on the heap is a container
  descriptor.
- **The row-major list column's `Vec<u32>` of values** — 471,778,374 entries at 4 bytes, **1.9 GB**,
  standing beside the 0.9 GB of column it was about to be narrowed into. The composer now frames
  the column first and writes each entry straight into it at its stored width
  (`membership::ListColumnWriter`).

**The saving is less than the sum, and that is the allocator rather than an error.** 1.2 + 1.9 is
3.1 GB against 1.76 GB observed, because freeing the memberships does not return their arena to
the kernel — glibc keeps it, and the (now smaller) column composition is served out of it. What
changed is the *peak*, which is the figure `--memory-budget` and an OOM kill are both about.

⊘ **Neither number is a prediction for rung 4.** Both scale with this corpus's hierarchy, and
rung 4's layers are not MedCPT's. What transfers is the shape: a build that publishes a
corpus-sized membership no longer carries it, and a level served row-major as a list no longer
materialises its entries twice.

## 1. What was measured, and how

`probes/2026-09-02-text-peak-split/sample_rss.py`, unchanged but for an `--interval` flag added
here (its `RssAnon` / `RssFile` split and its stage alignment are that probe's §1). Sampled at
**50 ms**, half that probe's interval, because the stages this is about are seconds rather than
minutes.

```bash
export TESSERA_LADDER=/home/user/code/tessera/data/ladder
cargo build --release -p tessera-cli                                  # in this worktree

cd $TESSERA_LADDER/medcpt-1m && set -a && . ./.env && set +a
python3 probes/2026-09-02-text-peak-split/sample_rss.py --interval 0.05 --out …/1m-after -- \
    …/tessera build --deployment ./tessera.toml --out ./bundle-mm-after --stage-timings

cd $TESSERA_LADDER/medcpt-10m-abs && set -a && . ./.env && set +a
python3 probes/2026-09-02-text-peak-split/sample_rss.py --interval 0.05 --out …/10m-after -- \
    …/tessera build --deployment ./tessera.toml --out ./bundle-mm-after --stage-timings \
                    --config ./corpus-noabs.toml
```

**The attribution runs (§2) carry temporary markers and the shipped binaries do not.** Ten
`eprintln!`s inside `layers::publish` and `artifact_pass::run`, printed in the build's own
`--stage-timings` format so `--report` reads them as stages, taken on a binary built for the
measurement and reverted after it. They are named `lay_*` and `ap_*` in the tables below, and
`layers` and `manifests` in a marked run are the *remainder* of those stages rather than the whole.

Raw CSVs beside this file — `<tag>.rss.csv` and `<tag>.stages.csv` for `1m-before`, `1m-after`,
`10m-before`, `10m-after`, `10m-marked` (before, instrumented) and `10m-after-marked`.

## 2. Where the 2.6 GB was: `ap_column_project`, and nothing else

`probes/2026-09-02-text-peak-split/` §4.1 put the build's anonymous high-water in `manifests` and
said what it was *about* — the artifact layout over the MeSH DAG — without saying which structure.
The marked run answers it. Anonymous MiB, 10⁷ control, before the change:

| marker | s | anon MiB | what it covers |
|---|---|---|---|
| `segment_write` | 0.73 | 1,699 | the stage boundary the pass starts from |
| `ap_resolve` | 0.01 | 1,699 | the spatial levels' resolution — none here |
| `ap_observe` | 7.12 | 1,699 | `observe_shape`, every membership projected one at a time |
| `ap_tileidx_project` | 0.06 | 1,699 | the tile indexes — the k-means level only |
| **`ap_column_project`** | **35.93** | **4,367** | **`project_row_column` over the MeSH level** |
| `ap_column_file` | 1.16 | 2,618 | the column written; what is left is the column's own bytes |
| `ap_contain` | 0.03 | 1,699 | the containment partitions |
| `manifests` | 44.87 | 1,661 | the digest re-read |

**One marker owns the whole of it.** `+2,668 MiB` inside `project_row_column`, of which `919 MiB`
survives the call as the composed column and the rest goes at `ap_column_file`. Two things to say
about the rest of the table:

- **`ap_observe` is flat**, over the same 30,954 memberships and the same 7 seconds. The
  projection buffers are shared across the pass and a membership is projected one at a time, which
  is exactly what that loop's comment claims and is now measured.
- **`manifests` proper is 1,661 MiB and 45 seconds of SHA-256.** The build's peak was never the
  manifest write; it was the pass that runs inside the same window.

The arithmetic behind `ap_column_project`, all of it in the `RowMajorList` arm:

| | bytes | at 10⁷ |
|---|---|---|
| `at`, the offsets | 4 × (rows + 1) | 40 MiB |
| `cursor`, its clone | 4 × (rows + 1) | 40 MiB |
| **`values`, one `u32` an entry** | **4 × 471,778,374** | **1,800 MiB** |
| the packed column, 2 B an entry | 2 × 471,778,374 + offsets | 940 MiB |

`values` is a `u32` per entry only so that `pack_list_column` can narrow it to the level's width a
moment later — the level holds 30,954 ordinals, so the stored width is 2 bytes. Framing the column
first and writing into it removes the vector and nothing else:

| marker | before | after | |
|---|---|---|---|
| `ap_column_project` | 4,367 MiB / 35.93 s | **2,617 MiB** / 36.34 s | −1,750 MiB, +1.1% wall |
| `ap_column_file` | 2,618 MiB / 1.16 s | 2,617 MiB / 2.97 s | the column is the same bytes |

## 3. Where the 1.2 GB was: carried from `lay_publish` to the end

The same marked run, over the layers stage:

| marker | s | anon MiB | file MiB |
|---|---|---|---|
| `attribute_tail` | 5.51 | 421 | 1,460 |
| `lay_merge` | 17.03 | 544 | 832 |
| `lay_verify` | 8.89 | 601 | 808 |
| **`lay_publish`** | **5.73** | **1,630** | 808 |
| `lay_extents` | 1.10 | 1,630 | 808 |
| … through to `segment_write` | | 1,699 | |

The publication adds **~1.0 GB** and none of it leaves: `text_index`, `record_blob`, `tiler_sort`
and `segment_write` all sit at 1,760 MiB in the unmarked before run, and the artifact pass's
transient is stacked on top of that. The extent the store could have read instead was already on
the disk from `lay_extents` onwards — **543,884,239 bytes** in two `.tsmb` files, 1.15 B a member
row, well under the 2.7 B a row the heap form costs because a descriptor whose members are dense
comes out as a run or a bitset container.

After the change, the same window in the after-marked run:

| marker | anon MiB | file MiB |
|---|---|---|
| `lay_publish` | 1,619 | 15 |
| `lay_extents` | 1,619 | **521** |
| `lay_content` | 1,619 | 534 |

**The anonymous figure does not fall here, and that is expected.** The publication still builds
each membership as a heap bitmap — the incoming bitmaps, the durable record's bytes and the
store's decoded copy, which is what `residency.rs`'s `BYTES_PER_MEMBER_ROW` charges — and freeing
them at the rehousing leaves the arena with glibc. What the file-backed column shows is the extent
arriving as page cache, and what the whole-run figure shows (§0) is that everything after this
point now allocates into the space the memberships used to hold rather than above it.

⊘ **`lay_extents` costs 0.18 s more** — 1.10 → 1.28 s — for mapping two files and putting every
blob through the *checked* Roaring deserialiser before a view is taken over it. `BitmapView` is
unchecked in croaring; that check is what stands between a mis-framed blob and a membership read
as containers at whatever the header claimed.

## 4. The bundles are byte-identical

Both corpora, before against after, every file digested:

```
cd $TESSERA_LADDER/medcpt-1m
diff <(cd bundle-mm-before && find . -type f | sort | xargs sha256sum) \
     <(cd bundle-mm-after  && find . -type f | sort | xargs sha256sum)
```

| | files differing | what differs |
|---|---|---|
| `medcpt-1m` (332,688,528 B) | `MANIFEST.json`, `CURRENT` | `created_at`, and the digest over it |
| `medcpt-10m-abs`, `corpus-noabs.toml` (3,163,528,625 B) | `MANIFEST.json`, `CURRENT` | the same |

`diff` over the two manifests, formatted, is **one line**: the build's own timestamp. Every
membership extent, every derived structure, every column and every segment file is the same bytes.

## 5. The whole tables

Anonymous MiB / file-backed MiB, each stage's own high-water over its samples.

**10⁶ (`medcpt-1m`)** — before 22.36 s, after 20.95 s:

| stage | before | after |
|---|---|---|
| source_ids | 13 / 14 | 11 / 15 |
| dictionary | 108 / 15 | 112 / 15 |
| geometry_read | 186 / 29 | 186 / 15 |
| pairs_pack | 89 / 29 * | 89 / 30 * |
| signature_sort | 146 / 30 | 145 / 30 |
| assignment | 134 / 30 | 133 / 30 |
| assignment | 62 / 22 | 97 / 23 |
| postings_write | 81 / 23 | 98 / 24 |
| external_ids | 63 / 23 * | 100 / 24 * |
| attribute_tail | 97 / 284 | 111 / 286 |
| layers | 538 / 107 | 552 / 167 |
| text_index | 362 / 212 | 463 / 271 |
| filter_postings | 362 / 212 * | 463 / 271 * |
| record_blob | 362 / 212 | 463 / 271 |
| column_release | 362 / 41 * | 463 / 101 * |
| tiler_sort | 362 / 41 | 463 / 101 |
| segment_write | 362 / 41 | 463 / 103 |
| **manifests** | **593** / 42 | **555** / 103 |
| **whole run** | **593** / 284 | **555** / 286 |

⊘ **At 10⁶ the change is 38 MiB and the noise is comparable to it.** This corpus's memberships are
a tenth of the 10⁷ one's and the list column is 47 million entries rather than 472 million; the
row is here because the brief asks for both scales, not because it decides anything. The
file-backed column rises by the extent's own 60 MiB, which is the effect showing up on the side it
should.

**10⁷ (`medcpt-10m-abs` with `corpus-noabs.toml`)** — before 236.19 s, after 230.25 s:

| stage | before | after |
|---|---|---|
| source_ids | 108 / 14 | 126 / 15 |
| dictionary | 827 / 14 | 829 / 15 |
| geometry_read | 1,889 / 107 | 1,889 / 162 |
| pairs_pack | 597 / 167 * | 600 / 162 * |
| signature_sort | 1,117 / 167 | 1,140 / 168 |
| assignment | 863 / 167 | 870 / 168 |
| assignment | 321 / 91 | 342 / 168 |
| postings_write | 501 / 92 | 507 / 93 |
| external_ids | 326 / 92 * | 334 / 93 * |
| attribute_tail | 403 / 1,456 | 409 / 1,470 |
| layers | 1,628 / 864 | 1,634 / 1,342 |
| text_index | 1,760 / 1,909 | 1,761 / 2,381 |
| filter_postings | 1,760 / 1,898 * | 1,761 / 2,381 * |
| record_blob | 1,760 / 1,899 | 1,761 / 2,383 |
| column_release | 1,760 / 1,466 | 1,761 / 2,306 |
| tiler_sort | 1,764 / 246 | 1,763 / 699 |
| segment_write | 1,764 / 324 | 1,763 / 776 |
| **manifests** | **4,445** / 247 | **2,682** / 766 |
| **whole run** | **4,445** / 1,909 | **2,682** / 2,383 |

**Wall time per stage**, the two stages the change touches:

| stage | before | after | marked before | marked after |
|---|---|---|---|---|
| layers | 110.31 s | 113.69 s (+3.1%) | 112.13 s | 110.28 s (−1.6%) |
| manifests | 48.54 s | 43.36 s (−10.7%) | 44.87 s | 48.61 s (+8.3%) |
| whole run | 236.19 s | 230.25 s (−2.5%) | 234.70 s | 238.07 s (+1.4%) |

⊘ **Nothing here is a wall-time result.** The two pairs disagree about the sign of both stages, at
3–11% either way, on a box that was never idle; what the marked pair *does* pin is the one figure
that is a property of the change rather than of the load — `ap_column_project` at 35.93 s against
36.34 s, the same work at the same speed writing into a narrower place. The mapped read is not
free and it is not visible either: `lay_extents` is the only marker that moved for it, by 0.18 s.

## 6. Load, and what this does not answer

⊘ **The box was not idle.** Two other sessions held long GPU and share jobs throughout, and a
MedCPT server was up for part of the campaign. Anonymous high-waters are the build's own `/proc`
figures and a neighbour does not move them; file-backed ones are page cache, so a busier box shows
fewer of them resident — those are an upper bound on what this build got. Wall times are
indicative for the same reason, which §5 says again where it matters.

- **The publication's own window.** `lay_publish` still builds every membership on the heap three
  times over — the incoming bitmap, the WAL record's bytes and the store's decode. At this rung
  that window is 1.0 GB and it is not the peak; on a corpus whose largest level is bigger than its
  artifact pass's column it would be. Nothing here measures that corner.
- **The column's own bytes.** 940 MiB of composed column stands from `ap_column_project` to
  `ap_column_file` because `Filed` carries bytes rather than a path. That is the file, so it is a
  floor rather than a transient — but it is a floor a streaming writer would not pay.
- **Whether the mapping costs anything under pressure.** These runs had 47 GB for a 4 GB anonymous
  set, so the extent stayed resident and no membership was ever re-read from the disk. A build
  whose mapped files exceed the box will page, and this probe says nothing about what that costs —
  the same gap `probes/2026-09-02-text-peak-split/` §7 leaves.
