# Why the species level is slow per tile on `POST /v1/artifacts/viewport`

Measured on gbif-64p (25,846,007 rows, `taxonomy/tree` with family, genus and species) on
2026-10-06 between 00:20 and 00:31 BST, with main at 8dd0e723. The GBIF build had stopped by
then, but other agents' work kept the load average at 6 to 7. The counts below do not depend on
load. Rerun the times on a quiet machine before quoting them.

## What was found

The species level is not served from a row column. The build serves it as rows, one bitmap per
artifact (`served rows` in the build log). The layout rule (`tessera_store::derived::choose`)
picks a column only where at least a quarter of a level's artifacts span more than the tile
index's coarsest node. Species is at 0.164, genus at 0.312 and family at 0.501. A level served as
rows has no members or coverings and no figures, so the tile walk takes `by_index`. That path
asks the tile index for candidates, probes every candidate against the mask, computes a verdict
for every one present, and sorts the whole lot.

The tile index returns 50,353 species candidates for every tile, whatever the tile's size and
whoever the viewer. They are the index's `everywhere` set (30,580 artifacts) plus the artifacts
placed at coarse nodes that every small tile cuts. Each candidate pays a masked probe of about
240 ns. That comes to 12 to 14 ms per tile, 85% of the request. Between 10 and 917 of them
are present in a tile.

With the layer pinned to `layout = "column"`, species takes the same walk as genus: covering
candidates, a heap by figures count, and lazy verdicts and presence. It answers with the same
number of artifacts per request in all 18 cases measured. I did not compare identities.

Per walked tile, median of three warm repeats, `per_tile` 50:

| viewer | map zoom / depth | species as rows: ms per tile, candidates | species as column: ms per tile, candidates | genus: ms per tile |
|---|---|---|---|---|
| 1% | 9 / 11 | 16.6, 51,939 | 0.19, 487 | 0.21 |
| 1% | 9 / 13 | 15.9, 51,938 | 0.20, 476 | 0.21 |
| 25% | 9 / 13 | 15.9, 51,053 | 0.26, 972 | 0.23 |
| 100% | 9 / 11 | 14.3, 50,356 | 0.24, 1,713 | 0.23 |
| 100% | 9 / 13 | 13.8, 50,353 | 0.29, 1,562 | 0.25 |
| 100% | 12 / 14 | 13.7, 50,353 | 0.43, 1,555 | — |

Whole request, species only:

| viewer | depth, tiles | rows | column |
|---|---|---:|---:|
| 1% | 13, 510 | 2,724 ms | 36 ms |
| 25% | 13, 510 | 7,807 ms | 130 ms |
| 100% | 11, 40 | 572 ms | 12 ms |
| 100% | 13, 510 | 6,854 ms | 148 ms |
| 25% | 16, 510 | 1,702 ms | 38 ms |

As a column, the first request of each viewer pays a figures fill of 323 to 415 ms, once. The
bundle grows from 1.9 to 2.0 GB, with 74 MB of species members and coverings.

## Files

- The counters were a temporary patch around each phase of the tile walk and of the covering
  index, printing one `TPROF` line per request to the server's log. It is not kept here; it is
  in the history at a12c2824 (`git show a12c2824:probes/2026-10-06-species-tiles/tprof.patch`).
- `tile_phases.py`: starts the server on the bench deployment under `MemoryMax=24G`, asks the
  route for each viewer and level, and records each request's counters. `DEPLOYMENT` picks the
  deployment, `REPEATS` and `PER_TILE` change the asks.
- `runs/species-rows.json`: the stage 5 bundle as built.
- `runs/species-column.json`: the same corpus with `layout = "column"` on `taxonomy/tree`.

## Commands

```bash
W=$PWD   # the worktree
git show a12c2824:probes/2026-10-06-species-tiles/tprof.patch | git apply
export CARGO_TARGET_DIR=$W/target CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_RELEASE_DEBUG=0
cargo build --release -p tessera-cli
RUN=$W/../stage5-bench/probes/2026-10-05-serving-layers-bench/runs/smoke-new.json

# species as rows (the stage 5 bundle)
python3 probes/2026-10-06-species-tiles/tile_phases.py $W/target/release/tessera $RUN rows.json

# species as column: the corpus with `layout = "column"` added to the layer, built beside it
cd /home/joe/code/tessera/data/ladder/gbif-64p
sed 's/^membership                = "enumerated"$/&\nlayout                    = "column"/' \
  corpus.toml > corpus-species-column.toml
# bench-species-column/tessera.toml is bench-stage5's with ../bundle-species-column and
# ../corpus-species-column.toml
cd bench-species-column && set -a && . ../.env && set +a
systemd-run --user --scope --collect -p MemoryMax=24G -p MemorySwapMax=2G -- \
  $W/target/release/tessera build --deployment tessera.toml
cd $W && DEPLOYMENT=/home/joe/code/tessera/data/ladder/gbif-64p/bench-species-column \
  python3 probes/2026-10-06-species-tiles/tile_phases.py $W/target/release/tessera $RUN column.json
git checkout -- crates && rm crates/tessera-engine/src/tprof.rs
```

## The other routes that read the species level

`other_routes.py`, with one binary (a12c2824, no counters) on both bundles. The server starts on a
fresh cache with the bundle's pages evicted. "First" is each viewer's first ask of the route, and
"warm" is the median of five more. `artifact by id x20` is twenty requests in a row. Measured at
00:40 BST with the load average at 6 to 8.

| viewer | route | rows: first / warm ms | column: first / warm ms |
|---|---|---:|---:|
| 1% | browse search, page 1 | 105 / 53 | 12 / 13 |
| 1% | browse search, filtered | 99 / 72 | 42 / 33 |
| 1% | browse children, filtered | 58 / 53 | 23 / 25 |
| 1% | aggregate by species | 35 / 36 | 9 / 9 |
| 1% | artifact by id x20 | 14 / 13 | 396 / 14 |
| 25% | browse search, page 1 | 102 / 99 | 68 / 66 |
| 25% | browse search, filtered | 214 / 216 | 130 / 122 |
| 25% | aggregate by species | 99 / 90 | 68 / 68 |
| 25% | artifact by id x20 | 42 / 15 | 369 / 14 |
| 25% | bulk read 100 ids | 243 / 60 | 87 / 1 |
| 100% | browse search, page 1 | 265 / 296 | 251 / 246 |
| 100% | browse search, filtered | 806 / 752 | 567 / 513 |
| 100% | browse children, filtered | 538 / 505 | 233 / 218 |
| 100% | aggregate by species | 178 / 190 | 256 / 268 |
| 100% | aggregate by species, filtered | 124 / 115 | 68 / 69 |
| 100% | viewport z11 tagged | 10 / 5 | 9 / 5 |
| 100% | artifact by id x20 | 151 / 13 | 409 / 15 |
| 100% | bulk read 100 ids | 523 / 371 | 139 / 1 |

The full set is in `runs/other-routes-rows.json` and `runs/other-routes-column.json`.

Two are slower with the column:

- **The first `/v1/artifacts/{id}` of each viewer** takes 324 to 431 ms against 1 to 21. That
  request fills the species figures, and the `fills` counter on `/control/status` goes up by one.
  The fill happens once per grant and is kept on disk. Later requests take 0.6 to 1.1 ms, against
  5 to 10 for the 100% viewer as rows. The tile route pays the same fill if it gets there first.
- **Aggregate by species, unfiltered, for the 100% viewer** takes 254 to 268 ms against 170 to
  190, which is 1.4 to 1.5 times as long. A level with a column counts groups by scanning the
  set's rows and reading their labels (`filtered_counts`), one thread over 25.8 million rows. A
  level stored as rows intersects each artifact's members on the pool instead. The 1% and 25%
  viewers, and every filtered aggregate, are faster with the column.
