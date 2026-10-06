# Serving layers per tile: the interactive bench on gbif-64p and full GBIF

The interactive bench (`test_corpora/common/interactive_bench.py`) on full GBIF at 818122e3, and before that on gbif-64p against the same bundle with two binaries, each driven by the TypeScript core of its own commit:

- **old**: main at e473e126. Artifacts come in `POST /v1/viewport`, and the store promotes a whole level at idle.
- **new**: main at 7b73e9c1. Artifacts come from `POST /v1/artifacts/viewport`, one frame per tile, asked at floor(map zoom) + 2 and at most 558 tiles a request. Every enumerated flat, stacked or tiered level is served from a column.

The first run, against main at 58b215ea, is kept below under "First run". It found the two faults 7b73e9c1 fixes: the store asked at the points' tile depth, and the species level was served as rows.

## Full GBIF: main 818122e3, 2026-10-06

The first-open figures in this section are superseded by `probes/2026-10-06-first-open-fills`, whose fix keeps a zoom-0 open from filling genus and species: a new viewer's level-0 request and tag read now take 1.8, 4.0 and 9.1 s for the 1%, 25% and 100% viewers.

The bench on `data/ladder/gbif/bundle`: 3,495,729,729 occurrences, format 35, 276 GB, built at 818122e3. The binary is 818122e3's release build. The TypeScript core is from e979764a, whose server code is the same as 818122e3's. The old binary cannot read format 35, so the comparison is with `test_corpora/gbif/bench-baseline.json`. That baseline was recorded on 2026-10-04 at be547754, before this campaign, with artifacts in the points viewport and an idle promotion of each whole level. Its viewers were 1%, 7%, 85% and 100%, so its 100% viewer opened after the 85% viewer's fills, where here it opens after the 25% viewer's.

Every server ran under `MemoryMax=24G`, `MemorySwapMax=2G`. Each run used a fresh cache, with the bundle's pages evicted before the server started, then a restart over the kept cache. The restart was not evicted: the reopen figures here ran over whatever of the bundle the first server had left in the page cache. The bench now evicts before the restart too. There were two rounds, each with prefetch off and then on. The viewers see 1%, 25% and 100% of the corpus. GBIF's access field is `countrycode` alone, so every viewer is a set of countries. No year-based or other uncorrelated 1% viewer can be granted on this corpus, and the 1% viewer (5 countries) is spatially clustered.

### Verify

`tessera verify bundle` (not `--deep`) under the 24 GB cap passed in 370 s:

```
OK bundle (v00000): 1 partition(s), 1 view(s), 1 segment(s), 3495729729 rows, entity_id_high_water 3495729729
```

### Headline

1. **At 818122e3, a new viewer's first open settled late, because the read of point tags by identifier filled every level.** `probes/2026-10-06-first-open-fills` has fixed this.
   - For the 100% viewer, settled was 39 s against the baseline's 9.9 s. For the 1% viewer it was 11.7 s against 3.9 s.
   - The points were fast: the last byte of points arrived at 1.1 s for 100%, against the baseline's 9.9 s. The family level arrived at 11.7 s.
   - The read by identifier then took 29.7 s in round 1 and 26.4 s in round 2. It filled the genus and species figures, which a zoom-0 view does not show.
   - At 818122e3 the engine's artifacts read built every level of the layer, whatever level the identifiers named.
2. **Once the figures are filled, the campaign's goals hold.** On reopen, the 100% viewer settles in 1.5 s and the family level arrives in 0.97 s. The read by identifier takes 0.53 s in round 1 and 0.91 s in round 2. All of these are loaded from disk with no fill.
   - Genus and species at zoom 9 arrive in 1.1 s or less (638 ms for the 1% viewer). The baseline's zoom-9 points request, which carried the artifacts, took 2.1 s (the lower of its two regions) and 12.3 s for 100%.
   - Pans at zoom 9 settle in 287 to 568 ms.
   - The zoom out to the world sends no artifact request.
3. **There is no idle whole-level fetch.** With prefetch off the store sends no request on a timer. For the 1% and 100% viewers, which both runs have, the baseline's idle promotions moved 207 MB. The species level for 100% alone was 167 MB in 3.1 s. Its 7% and 85% viewers moved another 133 MB.
   - No request asks for every tile of a depth deeper than the view needs. The opening view's 16 tiles at depth 2 and the parent prefetch's 4 tiles at depth 1 are the whole world, and each is 0.1 MB or less.
   - A run's artifact traffic is 16.2 MB with prefetch off and 30.8 MB with it on, plus 4.3 to 4.5 MB of tags read by identifier.

### First reads, timed per level

`probes/2026-10-06-first-open-fills/first_open.py` (then `gbif_first_reads.py` here, its record kept in `runs/gbif-first-reads.json`) ran on a binary with temporary timers around each fill and each level read; the timers are not committed. On a fresh server it sent, for each viewer in turn:
- what the first open sends for the layer (the world at depth 2, level 0);
- the read by identifier of the artifacts that answered;
- the same read again;
- genus and species at zoom 9.

| viewer | rows in the fill | level 0 request | level 0 fill | tags by identifier, first | of which genus fill | of which species fill | again | zoom 9 after | figures held after |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1% | 34,956,939 | 5,892 ms | 3,027 ms | 9,021 ms | 4,530 ms | 4,450 ms | 2 ms | 2,073 ms | 80 MB |
| 25% | 873,932,430 | 7,331 ms | 5,598 ms | 17,590 ms | 8,526 ms | 8,984 ms | 1 ms | 1,645 ms | 100 MB more |
| 100% | 3,495,729,729 | 9,429 ms | 6,933 ms | 24,905 ms | 10,702 ms | 14,044 ms | 1 ms | 1,395 ms | 160 MB more |

The first read by identifier is the genus and species level reads, to within 10 ms by the timers around each level read. The fill timers inside them account for all but 41 to 159 ms of it. Each fill walks the level's column over the viewer's rows, cold from disk. The full taxonomy's figures for one viewer take 12 s to fill for 1%, 23 s for 25% and 32 s for 100%. The level-0 request takes 1.7 to 2.9 s more than its fill. I did not split that time further.

### The owed measurements

- **Fill time and memory per viewer:** as in the table above. The figures for the three viewers hold 340 MB in memory and on disk. Anonymous memory stayed at 5 to 6.6 GB throughout, and resident memory at the cap, 24 GB, at the server's open.
- **Status counters:** fills 9 per run, one per viewer per level. On reopen: loads 9, fills 0. `not_admitted`, `reserve_spent`, `labels_rows_read` and `exact` are 0, since the bench makes no deny. `disk_bytes` under the figures directory is 339,891,297 after the run and after the reopen.
- **Persisted write volume:** the cache directory held 340,497,989 bytes after a run, 339,899,489 of them figures. A reopen wrote nothing more. Nothing was written to the bundle or the write-ahead log. This kernel has no `/proc/<pid>/io` and the user's cgroups have no I/O controller, so the process's own write bytes could not be read. The bench now records the cache directory's size at each start and after each phase.
- **Tagging nested or DAG layers with no `artifact_budget`:** not measured. GBIF's only layer, `taxonomy/tree`, is tiered.
- **The first tag read by identifier:** at 818122e3 it paid for the genus and species fills, as above.
- **A flat layer published at runtime:** skipped. A copy of the 276 GB bundle does not fit in the 172 GB free, and a runtime publication writes into the bundle it serves. The gbif-64p figure is in "Second run".

### Tables: prefetch off against on

Each cell is the lower middle value and the largest over the two rounds, with two regions per round for a map step: the smaller and the larger of two values at a first open or a reopen. The baseline column is be547754's prefetch-off run, where it has the same viewer and measure; its settled includes no promotion at a first open. The full tables are in `summary-gbif.md`.

These runs predate the bench's `--reference`, so their prefetch-on figures for points and layers include the prefetch's own requests: a points request of the margin or the next depth, or an artifacts request of the ring or the parent depth, can be the last byte. The prefetch-on columns are marked so. A run with `--reference` keeps that work out.

#### First open at zoom 0, a viewer new to the server

| viewer | measure | prefetch off | prefetch on, prefetch work included | baseline |
|---|---|---:|---:|---:|
| 100% | counts | 154 / 190 | 156 / 160 | 137 |
| 100% | first points | 896 / 1,101 | 607 / 752 | 1,046 |
| 100% | last byte of points | 1,091 / 1,475 | 10,997 / 13,406 | 9,853 |
| 100% | last byte of layers | 11,704 / 12,824 | 21,261 / 21,434 | in the points |
| 100% | settled | 39,245 / 41,441 | 45,109 / 46,019 | 9,853 |
| 25% | first points | 519 / 642 | 472 / 496 | |
| 25% | last byte of layers | 9,061 / 10,801 | 10,761 / 13,134 | |
| 25% | settled | 24,106 / 33,506 | 25,963 / 26,488 | |
| 1% | first points | 810 / 861 | 1,012 / 1,016 | 983 |
| 1% | last byte of points | 4,650 / 5,712 | 4,948 / 5,798 | 3,867 |
| 1% | last byte of layers | 6,460 / 6,799 | 8,670 / 10,016 | in the points |
| 1% | settled | 11,660 / 15,344 | 14,727 / 15,001 | 3,867 |

"Points beside layer work" equals the last byte of points here: every points request of the open ran while the level-0 request was filling.

#### Reopen: the server restarted over its cache

| viewer | measure | prefetch off | prefetch on, prefetch work included |
|---|---|---:|---:|
| 100% | first points | 485 / 925 | 421 / 464 |
| 100% | last byte of layers | 971 / 1,595 | 1,977 / 2,173 |
| 100% | settled | 1,513 / 2,518 | 1,977 / 2,173 |
| 25% | last byte of layers | 1,267 / 1,802 | 2,399 / 3,464 |
| 25% | settled | 6,545 / 6,962 | 2,963 / 4,671 |
| 1% | last byte of layers | 2,383 / 2,933 | 3,798 / 6,024 |
| 1% | settled | 2,629 / 3,194 | 3,798 / 6,024 |

The server took 199 to 260 s to open each time.

#### Map steps

| viewer | step | measure | prefetch off | prefetch on, prefetch work included | baseline |
|---|---|---|---:|---:|---:|
| 100% | zoom 9 | last byte of points | 834 / 1,163 | 1,339 / 1,497 | 2,109 / 12,257 |
| 100% | zoom 9 | last byte of layers | 1,139 / 2,442 | 5,800 / 9,134 | in the points |
| 25% | zoom 9 | last byte of layers | 1,145 / 1,267 | 4,773 / 6,176 | |
| 1% | zoom 9 | last byte of points | 460 / 518 | 493 / 698 | 391 / 1,501 |
| 1% | zoom 9 | last byte of layers | 638 / 1,893 | 3,789 / 4,999 | in the points |
| 100% | pan at zoom 9 | settled | 354 / 386 | 3,477 / 3,824 | |
| 25% | pan at zoom 9 | settled | 568 / 1,116 | 3,726 / 4,101 | |
| 1% | pan at zoom 9 | settled | 287 / 325 | 3,478 / 3,624 | |
| 100% | pan at zoom 6 | settled | 346 / 920 | 3,596 / 3,895 | |
| 100% | zoom out to the world | settled | 289 / 295 | 297 / 769 | |
| 1% | zoom out to the world | settled | 272 / 289 | 271 / 294 | |

With prefetch on, a run sends 115 more artifact requests than with it off (14.6 MB more) and 86 more points requests (333 MB more). Matched one by one against the prefetch-off run of the same round, 142 artifact requests and 113 points requests sent something prefetch off did not. That count is higher because what a prefetch holds also changes what a view asks for. The slowest of those is 5.0 s for artifacts and 11.3 s for points, over both rounds (`summarise.py --beyond`). Settled then marks the end of that background work, which includes two 1.5 s idle timers after each move; it is not a wait the viewer sees. The baseline's settled has no counterpart for the map steps, because its idle promotions ran inside them.

### Machine load

The one-minute load average at each run's start and end:
- verify: 0.8 then 1.3;
- round 1: prefetch off 1.7 then 3.6, prefetch on 3.5 then 4.4;
- round 2: prefetch off 1.7 then 16.3, prefetch on 18.4 then 2.1, while another session's work overlapped;
- first-reads probe: 3.9 then 8.0.

Round 2's figures are within the spread of round 1's, apart from the 25% viewer's first open, whose largest value (33.5 s) came from round 2. The files are `runs/gbif-*.load`.

### Commands

From the repository root, with a release build at a commit that reads format 35 and the TypeScript core built:

```bash
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
cargo build --release -p tessera-cli
npm --prefix clients/ts ci && npm --prefix clients/ts run build -w @tesseradb/client
B=$PWD/target/release/tessera P=probes/2026-10-05-serving-layers-bench G=data/ladder/gbif

(cd $G && set -a && . ./.env && set +a && \
  systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G -- $B verify bundle)
DEPLOYMENT=$G SIDES=new bash $P/run.sh - - $B 2 1 gbif-
python3 probes/2026-10-06-first-open-fills/first_open.py --deployment $G --binary $B \
  --run $P/runs/gbif-new-off-1.json --out $P/runs/gbif-first-reads.json
python3 $P/summarise.py $P/runs --old gbif-new-off-1 gbif-new-off-2 --new gbif-new-on-1 gbif-new-on-2 \
  --labels "prefetch off,prefetch on" --beyond > $P/summary-gbif.md
```

The runs recorded here used 818122e3's release build, the run script of that day (without `--reference`, and evicting only before the first start), and a binary with timers for the first reads; the timers are not committed. The run files are in `data/ladder/gbif/bench-runs-2026-10-06/`.

### Ready

The bench runs full GBIF in 12 to 16 minutes a run with the reopen. The read by identifier now fills only what it shows (`probes/2026-10-06-first-open-fills`), so the next GBIF run should find the first open at 1.8 to 9.1 s. A prefetch-on run now takes its round's prefetch-off run as `--reference` and keeps the prefetch's work out of points and layers; a smoke of that on gbif-64p is below.

A smoke of the bench at b06d526b on gbif-64p (`data/ladder/gbif-64p/first-open-fills`, format 35, one round, `runs/smoke-new-off-1` and `runs/smoke-new-on-1`, run files in `data/ladder/gbif-64p/bench-stage5/runs/`) recorded the cache directory's size: 746 bytes at the server's start, 44,507,016 after the run, and the same after the reopen. With prefetch on and the prefetch-off run as reference, 290 requests were marked as the prefetch's work, and the 100% viewer's first-open points and layers came to 207 and 262 ms against 148 and 258 ms with prefetch off, while settled, which keeps that work, came to 1,781 ms. No request was counted as whole-level.

## Second run: main 7b73e9c1, 2026-10-06

Three rounds, each on a fresh server with the bundle's pages evicted, then a restart over the kept cache, which was not evicted. Each round ran old and new with the store's prefetch off, then with it on. The viewers see 1%, 25% and 100% of the corpus. Each cell is the lower middle value and the largest over the three rounds, with two regions per round for a map step. The prefetch-on figures include the prefetch's own requests.

The bundle is `data/ladder/gbif-64p/bundle-stage5b`, format 34, built at 7b73e9c1 (85 s, 2.11 GB). The build log shows all three taxonomy levels as `served column`. The old binary opened it.

### Headline

1. **The faults the first run found are gone.** A zoom out to the world sends no artifact request, since the depth-2 tiles of the opening view are held. The first run sent 711 to 838 MB there, taking 8 to 18 s. A run's artifact traffic is 11.8 MB, plus 1.7 MB of tags read by identifier. The old store moved 44.7 MB and the first run 4,681 MB.
2. **Zoom 9 and pans are as fast as the old code or faster.** The last byte of genus and species at zoom 9 takes 222 ms for every viewer. The old code took 373 to 454 ms. Settled on a zoom-9 pan is 255 to 273 ms in both.
3. **Asked directly, the species level is 34 to 76 times faster.** At map zoom 9, depth 13 (510 tiles), warm, it takes 48, 182 and 220 ms for the 1%, 25% and 100% viewers. The first run took 2.2, 6.1 and 16.7 s.
4. **A new viewer's first open settles later than in the old code.** Settled is 727 to 1,048 ms against 260 to 333 ms. The points and the layers arrive about as before. The cause is the store's read of point tags by identifier (`POST /v1/artifacts` with `ids`, 561 to 3,325 ids). It is sent after the first frames and takes 370 to 950 ms on a fresh server. The same read takes 2 to 7 ms for the same viewer's second open, and 20 to 33 ms after a restart. Not investigated: what that first read pays for.
5. **Reopen loads every figure from disk.** New loads 9 entries and fills none. Old loads 3. Settled on reopen is 410 ms for 100% (old 620), and 312 to 317 ms for the others (old 254 to 260).

### Prefetch off against on

With prefetch on, the new store sends 112 more artifact requests per run than with it off (10.2 MB more). Matched one by one against the prefetch-off run of the same round, 139 artifact requests (13.1 MB) sent something prefetch off did not, the slowest 23 ms. The count is higher because what a prefetch holds also changes what a view asks for. The old store's 6 promotions per run (37.4 MB, the largest 340 ms) run with prefetch off as well. With prefetch on, both stores also prefetch points: 287 points requests against 197.

Settled with prefetch on is the end of all background work, not a wait the viewer sees. In the new code it is 1.72 to 1.82 s at open and 3.22 to 3.40 s after a move. That is two prefetch steps, each started after 1.5 s of quiet (`PREFETCH_IDLE_MS`), and each costs milliseconds. The old code's settled with prefetch on is 259 to 628 ms. The view's own requests are the same as with prefetch off.

The new store makes no idle whole-level fetch: no request asks for every tile of a depth deeper than its view needs. The opening view's 16 tiles at depth 2 and, with prefetch on, the parent prefetch's 4 tiles at depth 1 (6 ms) are the whole world, which is what a view at zoom 0 shows.

### Tables, prefetch off

The last column is the first run's new binary (58b215ea, two rounds) where it measured the same thing.

#### First open at zoom 0, a viewer new to the server

| viewer | measure | old p50 / max ms | new p50 / max ms | first run, 58b215ea |
|---|---|---:|---:|---:|
| 100% | first points | 46 / 46 | 72 / 128 | 62 / 117 |
| 100% | last byte of points | 252 / 255 | 213 / 313 | 185 / 563 |
| 100% | last byte of layers | 257 / 267 | 288 / 320 | 272 / 371 |
| 100% | settled | 333 / 345 | 1,048 / 1,496 | 502 / 991 |
| 100% | MB received | 8.24 | 6.60 | 6.46 |
| 25% | last byte of layers | 210 / 212 | 256 / 285 | 237 / 312 |
| 25% | settled | 264 / 284 | 752 / 939 | 386 / 506 |
| 1% | last byte of layers | 210 / 210 | 246 / 266 | 231 / 351 |
| 1% | settled | 260 / 261 | 727 / 803 | 338 / 780 |

#### Zoom 9: genus and species

| viewer | measure | old p50 / max ms | new p50 / max ms | first run, 58b215ea |
|---|---|---:|---:|---:|
| 100% | last byte of points | 109 / 612 | 74 / 162 | 84 / 178 |
| 100% | last byte of layers | 454 / 461 | 222 / 325 | 690 / 2,653 |
| 100% | settled | 256 / 612 | 255 / 325 | 690 / 2,653 |
| 25% | last byte of layers | 373 / 385 | 228 / 265 | 854 / 15,271 |
| 25% | settled | 377 / 448 | 313 / 387 | 929 / 15,365 |
| 1% | last byte of layers | 452 / 517 | 222 / 359 | 10,908 / 23,239 |
| 1% | settled | 255 / 517 | 287 / 393 | 10,987 / 23,239 |

#### Pans and the zoom out

| viewer | step | old settled | new settled | first run, 58b215ea | old MB | new MB |
|---|---|---:|---:|---:|---:|---:|
| 100% | pan at zoom 6 | 261 / 432 | 269 / 452 | 620 / 1,031 | 0.56 | 0.27 |
| 25% | pan at zoom 6 | 255 / 662 | 254 / 607 | 673 / 3,717 | 0.00 | 0.04 |
| 1% | pan at zoom 6 | 257 / 621 | 304 / 1,704 | 5,879 / 11,118 | 0.00 | 0.03 |
| 100% | pan at zoom 9 | 256 / 441 | 255 / 343 | 18,970 / 23,861 | 1.41 | 1.21 |
| 25% | pan at zoom 9 | 253 / 281 | 259 / 314 | 7,179 / 32,499 | 0.02 | 0.08 |
| 1% | pan at zoom 9 | 254 / 257 | 273 / 288 | 11,322 / 15,052 | 0.00 | 0.23 |
| 100% | zoom out to the world | 260 / 315 | 260 / 262 | 11,425 / 18,615 | 0.00 | 0.00 |
| 25% | zoom out to the world | 256 / 259 | 256 / 262 | 10,879 / 17,871 | 0.00 | 0.00 |
| 1% | zoom out to the world | 255 / 264 | 257 / 259 | 258 / 15,807 | 0.00 | 0.00 |

#### Reopen: the server restarted over its cache

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | last byte of points | 620 / 625 | 179 / 392 |
| 100% | last byte of layers | 242 / 245 | 212 / 222 |
| 100% | settled | 620 / 625 | 410 / 703 |
| 25% | settled | 254 / 603 | 317 / 323 |
| 1% | settled | 260 / 410 | 312 / 315 |

#### The figures counters on `/control/status`

The same in all three rounds:

| binary | phase | fills | loads | not_admitted | reserve_spent | labels_rows_read | exact | disk bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| old | run | 9 | 0 | 0 | 0 | 0 | 0 | 44,501,549 |
| old | reopen | 0 | 3 | 0 | 0 | 0 | 0 | 44,501,549 |
| new | run | 9 | 0 | 0 | 0 | 0 | 0 | 44,501,549 |
| new | reopen | 0 | 9 | 0 | 0 | 0 | 0 | 44,501,549 |

The full tables are in `summary-prefetch-off.md`, `summary-prefetch-on.md` and `summary-new-off-on.md`.

### The route asked directly

`route_depths.py` against the new binary, as in the first run, with the first run's warm figures beside. The full table is in `runs/route-depths-b.txt`.

| viewer | map zoom | tile depth | levels | tiles | first ms | warm p50 / max ms | first run warm p50 |
|---|---:|---:|---|---:|---:|---:|---:|
| 100% | 6 | 8 | [1] | 45 | 99 | 12 / 13 | 46 |
| 100% | 9 | 11 | [1, 2] | 40 | 408 | 18 / 19 | 822 |
| 100% | 9 | 13 | [1, 2] | 510 | 221 | 220 / 221 | 16,739 |
| 100% | 12 | 14 | [2] | 40 | 10 | 10 / 10 | 1,105 |
| 100% | 12 | 16 | [2] | 510 | 20 | 22 / 22 | 1,508 |
| 25% | 9 | 11 | [1, 2] | 40 | 346 | 15 / 15 | 488 |
| 25% | 9 | 13 | [1, 2] | 510 | 182 | 182 / 184 | 6,130 |
| 1% | 9 | 11 | [1, 2] | 40 | 393 | 7 / 7 | 244 |
| 1% | 9 | 13 | [1, 2] | 510 | 48 | 48 / 50 | 2,228 |

The first request at zoom 9 takes 346 to 408 ms. It is the viewer's first touch of the genus and species levels, so it is probably their fill; the probe does not separate the two. Repeats take 7 to 220 ms.

### A flat layer published at runtime

`runtime_layer.py` registers `genus-flat`, the taxonomy's genus level declared again as a flat layer. It publishes the layer's 56,893 artifacts and 25,088,942 members by `gbifid` in 9 requests, which took 17.9 s. It then asks the artifacts viewport for the world at depth 2 (16 tiles). The server serves a copy of the bundle, because a publication writes membership segments into the bundle it serves.

| read | genus-flat, first | genus-flat, warm p50 | built-in genus level, first | built-in, warm p50 |
|---|---:|---:|---:|---:|
| 1% viewer, the layer's first read | 4,342 ms | 18 ms | 335 ms | 33 ms |
| 100% viewer, after it | 576 ms | 11 ms | 128 ms | 12 ms |
| 1% viewer at zoom 9, depth 11 | 10 ms | 15 ms | 15 ms | 6 ms |

Most of the first read is spent composing a list column. At 7b73e9c1 a runtime registration started every column-served level as a list column, although this level's memberships are disjoint, until a fold re-chose the layout. A level's first publication now chooses its layout by the build's rule, and this level's first read composes a label column (`probes/2026-10-06-first-open-fills`). A second run with a temporary timer around the two steps (not committed) split a 4,232 ms first read into:

- 1,138 ms building the level's row form and tile index from its 25 million members;
- 3,034 ms composing the list column;
- about 60 ms for the rest, including the viewer's fill.

The 100% viewer's first read (576 ms) is its own fill over the form already built.

### The machine for the second run

AMD Ryzen 9 5900X, 12 CPUs, 47 GiB, WSL2. Other sessions were active, and the machine never stayed under a load average of 2. `run.sh` waited before each round for the one-minute load to fall under 2. Round 1 ran under 5 to 10. Round 2 started at 1.9 and rose to 5 during its runs. Round 3 ran under 0.3 to 3, apart from 9.4 at the start of new-on-3. Part of the load during a run is the bench's own server and client. Each run's start and end load is in `runs/*-off-*.load` and `runs/*-on-*.load`. The runtime-layer and route probes ran under 3 to 11.

### Commands for the second run

From the repository root, with the old binary kept at `data/ladder/gbif-64p/bench-stage5/bin/tessera-e473e126`:

```bash
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
cargo build --release -p tessera-cli
npm --prefix clients/ts ci && npm --prefix clients/ts run build -w @tesseradb/client
mkdir -p target/old-src && git archive e473e126 clients/ts | tar -x -C target/old-src
npm --prefix target/old-src/clients/ts ci && npm --prefix target/old-src/clients/ts run build -w @tesseradb/client
B=$PWD/target/release/tessera OLD_CORE=$PWD/target/old-src/clients/ts/core/dist/index.js
D=data/ladder/gbif-64p/bench-stage5 P=probes/2026-10-05-serving-layers-bench

# the bundle: bench-stage5/tessera.toml names ../bundle-stage5b
(cd $D && set -a && . ../.env && set +a && \
  systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G -- $B build --deployment tessera.toml)

DEPLOYMENT=$D bash $P/run.sh $D/bin/tessera-e473e126 $OLD_CORE $B 3
python3 $P/route_depths.py --deployment $D --binary $B --run $P/runs/new-off-1.json --out $P/runs/route-depths-b.json
python3 $P/runtime_layer.py --deployment $D --binary $B --run $P/runs/new-1.json --out $P/runs/runtime-layer.json
python3 $P/summarise.py $P/runs --old old-off-1 old-off-2 old-off-3 --new new-off-1 new-off-2 new-off-3 > $P/summary-prefetch-off.md
python3 $P/summarise.py $P/runs --old old-on-1 old-on-2 old-on-3 --new new-on-1 new-on-2 new-on-3 > $P/summary-prefetch-on.md
python3 $P/summarise.py $P/runs --old new-off-1 new-off-2 new-off-3 --new new-on-1 new-on-2 new-on-3 \
  --labels "new off,new on" --beyond > $P/summary-new-off-on.md
```

The bundle is format 34, which the current binary does not read; the commands are the current scripts' form of what ran. The new binary's SHA-256 began `dac0ee80f255b786`. The run files are in `data/ladder/gbif-64p/bench-stage5/runs/`.

### Ready for full GBIF

The bench is ready. It runs both binaries, the new route and the reopen, with prefetch on or off, in about 3 minutes a run on gbif-64p. The client no longer asks for a deep level whole, and the species level is served from a column. Two things to watch on full GBIF:

- the first tag read by identifier, which already costs up to 0.95 s here;
- a runtime-published level's first read, which composed a list column until a fold at 7b73e9c1.

Full GBIF's main `tessera.toml` is valid now, so `--deployment data/ladder/gbif` works as it is.

## First run: main 58b215ea, 2026-10-05

The interactive bench (`test_corpora/common/interactive_bench.py`) run against one bundle with two binaries:

- **old**: main at e473e126, where artifacts come in `POST /v1/viewport` and the TypeScript store promotes a whole level at idle;
- **new**: main at 58b215ea, where artifacts come from `POST /v1/artifacts/viewport`, one frame per tile, and the store asks for the tiles it does not hold.

Each binary is driven by the TypeScript core of its own commit. Both opened the same bundle, which is format 34 and was built at 58b215ea.

Measured on 2026-10-05 between 23:25 and 00:11 BST. The machine was shared with other agents' test runs throughout. A full GBIF build started at 00:08:40 BST. The last 2.5 minutes of run new-2 overlapped it: the 100% viewer's map steps and every reopen in that run. Rerun everything on a quiet machine before relying on a difference under about a factor of two. The differences in the headline below are factors of ten to a thousand, and they come from bytes and request counts, which load does not change.

### Headline

1. **On a move, the new store asks for artifacts at the tile depth the points are drawn at.** That depth is 5 to 9 below the map zoom on this corpus with the demo's 500,000-mark budget. A zoom-3 view asks for 1,881 to 7,410 tiles, and a zoom-6 view for up to 117,192. On the zoom out to the world from zoom 14, the points need nothing new, and the channel asks at depth 9 for all 262,144 tiles: 711 to 838 MB taking 8 to 18 s, once or twice per viewer per run. Over a run the new store received 4,681 MB of artifacts. The old store received 45 MB of artifacts and promotions together. The depth is the store's `projections.view.depth`, sometimes the previous view's (`store.ts:1172`). Only `serve.max_tiles_per_request` caps it, and its default is 262,144.
2. **The species level is slow per tile on the server.** Asked directly at about a screen of 256-pixel tiles (depth = map zoom + 2, 40 tiles), species and genus at zoom 9 take 244, 488 and 822 ms warm for the 1%, 25% and 100% viewers. At depth zoom + 4 (510 tiles) they take 2.2, 6.1 and 16.7 s, and repeated requests are no faster, so the time is per request and not a fill. The family and genus levels cost under 0.5 ms a tile. The species level is the one the build serves as rows (`served rows` in the build log, 186,464 artifacts). For comparison, the old binary answered the same zoom-9 view's artifacts in one request in 250 to 750 ms.
3. **Points are faster without the artifacts frame.** The new binary sends 15 to 35% fewer bytes of points. The last byte of points is faster on most steps: at zoom 9, 84 ms against 245 ms for the 100% viewer and 47 against 304 for 25%. First open for a new viewer is about the same (first points 62 ms on both for 100%), and the points' last byte comes sooner (185 against 352 ms).
4. **Opening with layers is a little slower in the new code.** The last byte of layers at first open is 272 against 216 ms for 100%. Settled is 502 against 352 ms. The new store also reads tags by identifier after the first frame (`/v1/artifacts` with `ids`), which adds one round trip. At first open the new store asks at depth 0 for one tile and draws 50 families. The old store drew every visible family (8,550 for 100%).
5. **Reopen.** Both binaries reload persisted figures without refilling: the new one loads all 6 entries and the old one 3. The new code's reopen settled later (618 against 313 ms for 100%), but the new-2 reopens ran during the GBIF build, so this needs a rerun.

### No idle whole-level fetch, but whole-level fetches on zoom out

With the store's prefetch off (the bench's default, as the viewer's `?prefetch=0`), the new store sent no promotion and no fetch on a timer. The old store sent 6 promotions per run, each a whole level: up to 22 MB and 2.4 s for the species level of the 100% viewer.

The new store made 5 whole-level requests per run, each part of a view's own request after a zoom out (the list is in `summary.md`). Not measured: the new store with prefetch on, which also asks for the ring of tiles and the parent depth at idle.

### Tables

Each cell is the lower middle value and the largest over runs old-1 and old-2 against new-1 and new-2: the smaller and the larger of two at an open, and of four values at a map step, two regions per run. The full set, including the reopened and second opens and the per-kind totals, is in `summary.md`.

#### First open at zoom 0, a viewer new to the server

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | first points | 62 / 102 | 62 / 117 |
| 100% | last byte of points | 352 / 656 | 185 / 563 |
| 100% | last byte of layers | 216 / 292 | 272 / 371 |
| 100% | points beside layer work | 263 / 293 | 357 / 357 |
| 100% | settled | 352 / 656 | 502 / 991 |
| 100% | MB received | 8.23 | 6.46 |
| 25% | first points | 21 / 36 | 24 / 51 |
| 25% | last byte of layers | 209 / 231 | 237 / 312 |
| 25% | points beside layer work | 212 / 212 | 315 / 315 |
| 25% | settled | 256 / 480 | 386 / 506 |
| 25% | MB received | 4.02 | 2.79 |
| 1% | first points | 35 / 150 | 50 / 126 |
| 1% | last byte of layers | 211 / 216 | 231 / 351 |
| 1% | points beside layer work | 270 / 270 | 352 / 352 |
| 1% | settled | 261 / 818 | 338 / 780 |
| 1% | MB received | 0.94 | 0.60 |

"Points beside layer work" is the last byte of a points request that was in flight while an artifacts request of the same view was.

#### Zoom 9: genus and species

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | last byte of points | 245 / 918 | 84 / 178 |
| 100% | last byte of layers | 664 / 754 | 690 / 2,653 |
| 100% | settled | 391 / 918 | 690 / 2,653 |
| 25% | last byte of points | 304 / 691 | 47 / 311 |
| 25% | last byte of layers | 248 / 253 | 854 / 15,271 |
| 25% | settled | 304 / 691 | 929 / 15,365 |
| 1% | last byte of layers | 231 / 299 | 10,908 / 23,239 |
| 1% | settled | 255 / 299 | 10,987 / 23,239 |
| 1% | MB received | 0.00 | 6.83 |

The 1% viewer's points at zoom 9 were already held from earlier steps, so neither binary sent a points request.

#### Pans

| viewer | step | old settled p50 / max ms | new settled p50 / max ms | old MB | new MB |
|---|---|---:|---:|---:|---:|
| 100% | pan at zoom 6 | 283 / 492 | 620 / 1,031 | 0.56 | 15.01 |
| 25% | pan at zoom 6 | 254 / 1,579 | 673 / 3,717 | 0.00 | 2.50 |
| 1% | pan at zoom 6 | 269 / 1,606 | 5,879 / 11,118 | 0.00 | 99.20 |
| 100% | pan at zoom 9 | 447 / 539 | 18,970 / 23,861 | 1.41 | 12.46 |
| 25% | pan at zoom 9 | 260 / 280 | 7,179 / 32,499 | 0.02 | 9.43 |
| 1% | pan at zoom 9 | 255 / 262 | 11,322 / 15,052 | 0.00 | 4.29 |

The old store sends no artifacts request on a pan. It holds each level whole after promoting it. Points on a pan are faster in the new code: the last byte at zoom 9 is 22 against 132 ms for 100%.

#### Zoom out to the world from zoom 14

| viewer | old settled p50 / max ms | new settled p50 / max ms | old MB | new MB |
|---|---:|---:|---:|---:|
| 100% | 260 / 298 | 11,425 / 18,615 | 0.00 | 838.16 |
| 25% | 256 / 267 | 10,879 / 17,871 | 0.00 | 729.73 |
| 1% | 253 / 257 | 258 / 15,807 | 0.00 | 0.32 |

#### Reopen: the server restarted over its cache

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | first points | 51 / 104 | 108 / 116 |
| 100% | last byte of layers | 247 / 250 | 221 / 227 |
| 100% | settled | 313 / 512 | 618 / 783 |
| 25% | settled | 253 / 644 | 434 / 557 |
| 1% | settled | 260 / 653 | 305 / 310 |

Half of the new values (run new-2) were taken during the GBIF build.

#### The figures counters on `/control/status`

`masked_count_cache` after the run and after the reopen. The four runs agree. Run old-1 and new-1:

| binary | phase | fills | loads | not_admitted | reserve_spent | labels_rows_read | exact | disk bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| old | run | 6 | 0 | 0 | 0 | 0 | 0 | 12,952,098 |
| old | reopen | 0 | 3 | 0 | 0 | 0 | 0 | 12,952,098 |
| new | run | 6 | 0 | 0 | 0 | 0 | 0 | 12,952,098 |
| new | reopen | 0 | 6 | 0 | 0 | 0 | 0 | 12,952,098 |

The bench makes no deny, so `reserve_spent` and `labels_rows_read` stay at 0.

#### The route asked directly

`route_depths.py` asks the new binary's `POST /v1/artifacts/viewport` for the screen the bench shows at each map zoom, at a tile depth 2 and 4 below it, with the levels the store would name at that zoom and `per_tile` 50. It used a fresh server with the bundle's pages evicted. Each viewer's first request per level is the cold one; five more give the warm figures. Taken at 23:42 BST, before the GBIF build, beside other agents' tests.

| viewer | map zoom | tile depth | levels | tiles | MB | first ms | warm p50 / max ms |
|---|---:|---:|---|---:|---:|---:|---:|
| 100% | 0 | 2 | [0] | 16 | 0.15 | 144 | 10 / 10 |
| 100% | 0 | 4 | [0] | 256 | 1.66 | 47 | 55 / 56 |
| 100% | 3 | 5 | [0] | 40 | 0.34 | 11 | 12 / 24 |
| 100% | 6 | 8 | [1] | 45 | 0.42 | 1,266 | 46 / 146 |
| 100% | 6 | 10 | [1] | 510 | 4.19 | 334 | 261 / 351 |
| 100% | 9 | 11 | [1, 2] | 40 | 0.62 | 1,264 | 822 / 919 |
| 100% | 9 | 13 | [1, 2] | 510 | 7.50 | 11,498 | 16,739 / 19,638 |
| 100% | 12 | 14 | [2] | 40 | 0.23 | 906 | 1,105 / 1,217 |
| 100% | 12 | 16 | [2] | 510 | 1.52 | 1,374 | 1,508 / 1,678 |
| 25% | 9 | 11 | [1, 2] | 40 | 0.62 | 547 | 488 / 526 |
| 25% | 9 | 13 | [1, 2] | 510 | 5.49 | 6,175 | 6,130 / 6,619 |
| 25% | 12 | 14 | [2] | 40 | 0.28 | 405 | 419 / 436 |
| 1% | 9 | 11 | [1, 2] | 40 | 0.31 | 318 | 244 / 263 |
| 1% | 9 | 13 | [1, 2] | 510 | 2.13 | 1,774 | 2,228 / 2,349 |
| 1% | 12 | 14 | [2] | 40 | 0.11 | 2 | 5 / 5 |

The rest is in `runs/route-depths.json`. At depth zoom + 2 the family and genus levels cost 8 to 46 ms warm. Wherever the species level is named, the cost grows with the tiles asked for and with the viewer's visible rows in them.

### The corpus and the deployment

`data/ladder/gbif-64p`: 25,846,007 GBIF occurrences from 64 of 8,369 parts, one view (`geo`, Web Mercator), access by `countrycode` (253 terms), and the `taxonomy/tree` layer, tiered, with family (zoom 0 to 5, 8,550 artifacts), genus (4 to 10, 56,893) and species (9 to 16, 186,464). The bundle was built at 58b215ea into `data/ladder/gbif-64p/bundle-stage5` under a 24 GB cap: 97 s, 2.03 GB, log in `data/ladder/gbif-64p/build-stage5.log`.

The corpus's own `tessera.toml` is refused at 58b215ea: `tessera.toml has a [plugin] table, which Tessera does not read`. It also names `session_credential_env`, which main refuses. `data/ladder/gbif-64p/bench-stage5/tessera.toml` is a deployment written from main's template, as the GBIF rebuild's `tessera-2026-10-05.toml` was, with ports 8291 to 8293. The corpus declaration (`corpus.toml`) built unchanged.

The viewers are the bench's greedy term sets for 1%, 25% and 100% of the corpus (`--targets 0.01,0.25,1`): 258,459, 6,461,499 and 25,846,007 visible items.

### Commands

These are the commands as they ran, with the scripts of that day; the scripts' current arguments are in "Commands for the second run". Built in the worktree with debug information off. The old binary and core were built from `git archive e473e126` unpacked under `target/old-src`. Both binaries report the commit 58b215ea, because the old source sat inside the worktree's checkout when `build.rs` asked git, so the hashes tell them apart: new `e7ae4b7f25583bd2…`, old `26b5520addb267f8…`.

```bash
export CARGO_TARGET_DIR=$PWD/target CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
cargo build --release -p tessera-cli
mkdir -p target/old-src && git archive e473e126 | tar -x -C target/old-src
(cd target/old-src && CARGO_TARGET_DIR=$PWD/../old-target cargo build --release -p tessera-cli)
npm --prefix clients/ts ci && npm --prefix clients/ts run build -w @tesseradb/client
npm --prefix target/old-src/clients/ts ci && npm --prefix target/old-src/clients/ts run build -w @tesseradb/client

# the bundle
cd data/ladder/gbif-64p/bench-stage5 && set -a && . ../.env && set +a
systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G -- \
  $W/target/release/tessera build --deployment tessera.toml

# the runs: old and new alternately, each on a fresh server with the bundle's pages evicted,
# then a restart over the kept cache; every server under MemoryMax=24G, MemorySwapMax=2G
bash probes/2026-10-05-serving-layers-bench/run.sh \
  $W/target/old-target/release/tessera $W/target/old-src/clients/ts/core/dist/index.js \
  $W/target/release/tessera 3
python3 probes/2026-10-05-serving-layers-bench/route_depths.py \
  $W/target/release/tessera probes/2026-10-05-serving-layers-bench/runs/smoke-new.json \
  probes/2026-10-05-serving-layers-bench/runs/route-depths.json
python3 probes/2026-10-05-serving-layers-bench/summarise.py old-1 old-2 -- new-1 new-2 \
  --busy-from "$(date -d '2026-10-06 00:08:40 BST' +%s)" > probes/2026-10-05-serving-layers-bench/summary.md
```

`run.sh` was stopped after two of its three rounds when the GBIF build started. The run files are in `data/ladder/gbif-64p/bench-stage5/runs/`. They are 12 MB each for the new binary, so they are not committed. A smoke run of each binary before the rounds (`smoke-old.json`, `smoke-new.json`) gave the same request counts and bytes and timings of the same order. The smoke run's new-binary file predates the final classification of idle requests.

### The machine

AMD Ryzen 9 5900X, 12 CPUs, 47 GiB, WSL2 kernel 6.18.40.1. The bundle's pages were evicted (`posix_fadvise` `DONTNEED`) before each run. The load average was 8 to 26 from other agents' cargo test runs during every run. From 00:08:40 BST a GBIF build held most cores under a 30 GB cap. Each `.load` file in the run directory has the load average at the start and end of its run.
