# Serving layers per tile: old against new on gbif-64p

The interactive bench (`test_corpora/common/interactive_bench.py`) run against the same bundle with two binaries. Each binary is driven by the TypeScript core of its own commit.

- **old**: main at e473e126. Artifacts come in `POST /v1/viewport`, and the store promotes a whole level at idle.
- **new**: main at 7b73e9c1. Artifacts come from `POST /v1/artifacts/viewport`, one frame per tile, asked at floor(map zoom) + 2 and at most 558 tiles a request. Every enumerated flat, stacked or tiered level is served from a column.

The first run, against main at 58b215ea, is kept below under "First run". It found the two faults 7b73e9c1 fixes: the store asked at the points' tile depth, and the species level was served as rows.

## Second run: main 7b73e9c1, 2026-10-06

Three rounds, each on a fresh server with the bundle's pages evicted, then a restart over the kept cache. Each round ran old and new with the store's prefetch off, then with it on. The viewers see 1%, 25% and 100% of the corpus. Each cell is the median and the largest value over the three rounds, with two regions per round for a map step.

The bundle is `data/ladder/gbif-64p/bundle-stage5b`, format 34, built at 7b73e9c1 (85 s, 2.11 GB). The build log shows all three taxonomy levels as `served column`. The old binary opened it.

### Headline

1. **The faults the first run found are gone.** A zoom out to the world sends no artifact request, since the depth-2 tiles of the opening view are held. The first run sent 711 to 838 MB there, taking 8 to 18 s. A run's artifact traffic is 11.8 MB, plus 1.7 MB of tags read by identifier. The old store moved 44.7 MB and the first run 4,681 MB.
2. **Zoom 9 and pans are as fast as the old code or faster.** The last byte of genus and species at zoom 9 takes 222 ms for every viewer. The old code took 373 to 454 ms. Settled on a zoom-9 pan is 255 to 273 ms in both.
3. **Asked directly, the species level is 34 to 76 times faster.** At map zoom 9, depth 13 (510 tiles), warm, it takes 48, 182 and 220 ms for the 1%, 25% and 100% viewers. The first run took 2.2, 6.1 and 16.7 s.
4. **A new viewer's first open settles later than in the old code.** Settled is 727 to 1,048 ms against 260 to 333 ms. The points and the layers arrive about as before. The cause is the store's read of point tags by identifier (`POST /v1/artifacts` with `ids`, 561 to 3,325 ids). It is sent after the first frames and takes 370 to 950 ms on a fresh server. The same read takes 2 to 7 ms for the same viewer's second open, and 20 to 33 ms after a restart. Not investigated: what that first read pays for.
5. **Reopen loads every figure from disk.** New loads 9 entries and fills none. Old loads 3. Settled on reopen is 410 ms for 100% (old 620), and 312 to 317 ms for the others (old 254 to 260).

### Prefetch off against on

With prefetch on, the new store's idle work is 112 artifact requests per run, 11.1 MB in all, each taking at most 23 ms. The old store's idle work is 6 promotions, 37.4 MB, the largest 340 ms. With prefetch on, both stores also prefetch points: 287 points requests against 197.

Settled with prefetch on is the end of all background work, not a wait the viewer sees. In the new code it is 1.72 to 1.82 s at open and 3.22 to 3.40 s after a move. That is two prefetch steps, each started after 1.5 s of quiet (`PREFETCH_IDLE_MS`), and each costs milliseconds. The old code's settled with prefetch on is 259 to 628 ms. The view's own requests are the same as with prefetch off.

The new store makes no idle whole-level fetch. The bench's whole-level count flags only small fetches. With prefetch off, these are the opening view's 16 tiles at depth 2, which is the whole world at that depth. With prefetch on, the parent prefetch adds the 4 tiles at depth 1 (6 ms).

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

Most of the first read is spent composing a list column. A runtime registration starts every column-served level as a list column (`RegisteredLayer::initial_layouts`), although this level's memberships are disjoint. A fold re-chooses the layout, and would pick a label column here. A second run with a temporary timer around the two steps (not committed) split a 4,232 ms first read into:

- 1,138 ms building the level's row form and tile index from its 25 million members;
- 3,034 ms composing the list column;
- about 60 ms for the rest, including the viewer's fill.

The 100% viewer's first read (576 ms) is its own fill over the form already built.

### The machine for the second run

AMD Ryzen 9 5900X, 12 CPUs, 47 GiB, WSL2. Other sessions were active, and the machine never stayed under a load average of 2. `run.sh` waited before each round for the one-minute load to fall under 2. Round 1 ran under 5 to 10. Round 2 started at 1.9 and rose to 5 during its runs. Round 3 ran under 0.3 to 3, apart from 9.4 at the start of new-on-3. Part of the load during a run is the bench's own server and client. Each run's start and end load is in `runs/*-off-*.load` and `runs/*-on-*.load`. The runtime-layer and route probes ran under 3 to 11.

### Commands for the second run

```bash
git merge main      # 7b73e9c1
export CARGO_TARGET_DIR=$PWD/target CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
cargo build --release -p tessera-cli
npm --prefix clients/ts ci && npm --prefix clients/ts run build -w @tesseradb/client
mkdir -p target/old-src && git archive e473e126 clients/ts | tar -x -C target/old-src
npm --prefix target/old-src/clients/ts ci && npm --prefix target/old-src/clients/ts run build -w @tesseradb/client

# the bundle: bench-stage5/tessera.toml names ../bundle-stage5b
cd data/ladder/gbif-64p/bench-stage5 && set -a && . ../.env && set +a
systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G -- \
  $W/target/release/tessera build --deployment tessera.toml

D=data/ladder/gbif-64p/bench-stage5 P=probes/2026-10-05-serving-layers-bench
bash $P/run.sh $D/bin/tessera-e473e126 $W/target/old-src/clients/ts/core/dist/index.js $W/target/release/tessera 3
python3 $P/route_depths.py $W/target/release/tessera $P/runs/new-off-1.json $P/runs/route-depths-b.json
python3 $P/runtime_layer.py $W/target/release/tessera $P/runs/new-1.json $P/runs/runtime-layer.json
python3 $P/summarise.py old-off-1 old-off-2 old-off-3 -- new-off-1 new-off-2 new-off-3 > $P/summary-prefetch-off.md
python3 $P/summarise.py old-on-1 old-on-2 old-on-3 -- new-on-1 new-on-2 new-on-3 > $P/summary-prefetch-on.md
python3 $P/summarise.py new-off-1 new-off-2 new-off-3 -- new-on-1 new-on-2 new-on-3 \
  --labels "new off,new on" > $P/summary-new-off-on.md
```

The new binary's SHA-256 begins `dac0ee80f255b786`. The run files are in `data/ladder/gbif-64p/bench-stage5/runs/`.

### Ready for full GBIF

The bench is ready. It runs both binaries, the new route and the reopen, with prefetch on or off, in about 3 minutes a run on gbif-64p. The client no longer asks for a deep level whole, and the species level is served from a column. Two things to watch on full GBIF:

- the first tag read by identifier, which already costs up to 0.95 s here;
- a runtime-published level's first read, which composes a list column until a fold.

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

Each cell is the median and the largest value over runs old-1 and old-2 against new-1 and new-2. For a map step there are two regions per run, so four values. The full set, including the reopened and second opens and the per-kind totals, is in `summary.md`.

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

Built in the worktree with debug information off. The old binary and core were built from `git archive e473e126` unpacked under `target/old-src`. Both binaries report the commit 58b215ea, because the old source sat inside the worktree's checkout when `build.rs` asked git, so the hashes tell them apart: new `e7ae4b7f25583bd2…`, old `26b5520addb267f8…`.

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
