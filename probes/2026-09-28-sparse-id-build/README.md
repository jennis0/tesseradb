# Sparse, out-of-order ids at a build, 2026-09-28

Does a build whose one unique id is sparse and out of file order still build within a memory cap
below 8 B an item, in about the time the same corpus takes with its ids in file order? GBIF's
`gbifid` is that shape, and on main such a build held arrays indexed by the id's span and
thrashed (`probes/2026-09-27-gbif-final-main/` on branch `probe/gbif-final-main`). This probe measures branch `identity/no-join-field-a`, where every file names its items
by the unique field through a sort-merge, against itself and against main `f99da0dc`.

Every figure is in [`results.json`](results.json). The box is WSL2 with 12 cores and 47 GiB, and
other sessions were building and testing Rust throughout.

## The corpus

[`probe.py`](probe.py) `make` writes one points file (`id` u64, `x`, `y`) and one members file
(`entity` u64, `key` i32 of 1,000 values), one row per item in each, and a declaration naming the
members by `fields = { id = "entity" }`. The ids have GBIF's spacing: each block of ten spans
eighteen values, with the two gaps of one placed at random, so gaps average 1.8. In the shuffled
corpus the points file holds item `(a·row + c) mod n` at each row, an affine permutation and not a
random one, and the members file another such order; in the file-order corpus both are ascending. `corpus-main.toml` is the same
declaration as main reads it, with `join_field = "id"`.

`probe.py run` builds under `systemd-run --user --scope -p MemoryMax=<cap> -p MemorySwapMax=2G`
with `--memory-budget`, and samples resident and anonymous memory, bytes read and written, and
major faults once a second. [`table.py`](table.py) prints the stages side by side. The per-stage
I/O and faults are 1 Hz samples, so a short stage's figures are rough.

## 10⁹ items, cap 7 GiB (below 8 B × 10⁹), budget 6 GiB

| | shuffled ids | ids in file order |
|---|---|---|
| whole build | **1,842 s**, exit 0 | **1,714 s**, exit 0 |
| identity pass (`source_ids`) | 323 s, 55.7 GB read, 45.9 GB written | 258 s, 50.2 GB read, 49.5 GB written |
| peak anonymous memory | 4,154 MiB (identity pass 1,082) | 3,923 MiB |
| major faults | 56,549 | 55,139 |
| bundle | 48.4 GB | |

Shuffled over file order is 1.07× on the whole build, 128 s more, and 1.25× on the identity pass,
65 s more. No stage but `column_release`, which takes under a second, differs by more than 1.3×. The identity pass's scratch peaked at an estimated 45 GiB of disc.

## 10⁸ items against main, cap 4 GiB, budget 3 GiB

| | this branch, shuffled | this branch, file order | main, shuffled | main, file order |
|---|---|---|---|---|
| whole build | **193 s** | **214 s** | **313 s** | **236 s** |
| `source_ids` | 34.0 s | 36.3 s | 6.2 s | 2.7 s |
| `layers` | 18.0 s | 16.8 s | 108.3 s | 64.9 s |
| `attribute_tail` | 17.5 s | 19.9 s | 31.6 s | 17.8 s |
| `artifact_pass` | 12.7 s | 14.4 s | 20.0 s | 22.6 s |
| peak anonymous | 2,161 MiB | 2,145 MiB | 2,517 MiB | 2,081 MiB |

The branch's identity pass costs about 30 s more than main's id read at 10⁸, and the member join
and attribute reads it replaces cost main 90 to 110 s more, so the whole build is 0.62× main's time
shuffled and 0.91× in file order. (The I/O sampling of main's file-order run read zeros; its
times stand.)

## 10⁸ items under a 640 MiB cap (below 8 B × 10⁸)

Both binaries are refused by the build's own memory model after the identity stage: the
entity-order stages need about 997 MiB at 10⁸ items whatever the ids. Before the refusal:

| | this branch | main |
|---|---|---|
| `source_ids` | 47.6 s, peak anonymous 487 MiB | 7.0 s |
| `dictionary` | 25.8 s, 179,303 major faults | **151.2 s, 3,886,105 major faults** |

Under this cap both binaries page in `dictionary` (1.2 s for the branch and 4.2 s for main at
4 GiB); main faults 22 times as often and takes 5.9 times as long.

## Where the identity pass spends its time

A 30-second sample of the pass at 10⁸ shuffled (`ipsample.py` from
[`2026-09-26-identity-gbif-scale`](../2026-09-26-identity-gbif-scale/)) found the main thread
busy about 55% of the time, the rest waiting on the disc: libc copies and writes 28%, the row
partition's `push_to` 10%, sorting `(key, row)` pairs 13%, the spill writer 6%, loading partition
buckets 5%, and Parquet decoding 6%. The pass is single-threaded and sequential per file.

## The identity pass cut, 2026-09-28

[`identity.py`](identity.py) builds under the same 7 GiB cap and 6 GiB budget and stops the build
once `source_ids` ends. "Before" is the stage A commit `13ad8601`; every other row adds to the one
above it. Figures are in [`identity-results.json`](identity-results.json).

| 10⁹ items, shuffled ids, members in another order | `source_ids` | read | written |
|---|---|---|---|
| before | 350 s | 76.3 GB | 50.0 GB |
| sorts in parallel, a bucket's decisions applied unsorted where a file carries one field | 315 s | 72.3 GB | 49.9 GB |
| spill files not synced | 297 s | 71.6 GB | 51.6 GB |
| held values as key gaps, not whole keys | 286 s | 62.6 GB | 46.5 GB |
| a members file compared row by row with the points (no match here, so it stops) | 273 s | 58.4 GB | 45.1 GB |

| 10⁹ items, one field | before | after |
|---|---|---|
| shuffled ids, members in the points' order, as GBIF's are (`--order aligned`) | 322 s, 52.3 GB written | **133 s**, 20.0 GB written |
| ids and members in file order | 251 s, 61.0 GB written | **120 s**, 20.0 GB written |

The last change is the large one on GBIF's shape. A members file carrying the one field a single
points file set is read in row order beside that file's column, and a row whose value is the one
at its position there names that row's item; only the rows that differ go through the sort and the
merge. After 2²⁰ compared rows a file in which fewer than half matched stops being compared. The
pass is still bound by the disc: the points file's sort spills its `(key, row)` pairs and reads
them back, which is most of what remains.
