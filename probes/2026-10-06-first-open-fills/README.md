# A new viewer's first open on full GBIF

What a viewer whose grant is new to a fresh server waits for on the taxonomy layer, before and after branch `perf/first-open-fills`, and where the time goes.

The corpus is `data/ladder/gbif/bundle`: 3,495,729,729 occurrences, format 35. The server ran under `MemoryMax=24G`, `MemorySwapMax=2G`, with the bundle's pages evicted before it started. The viewers are the serving-layers bench's: 1% (5 countries), 25% (7 countries) and 100% (253 countries). The layer is `taxonomy/tree`, tiered, with family, genus and species levels, each served from a label column, and it declares `centroid` and `box`, so every fill accumulates positions as well as counts.

## The probe

`first_open.py` starts a binary on a fresh cache, then for each viewer in turn sends:

1. the world at depth 2, level 0, as the store's first open does;
2. the read by identifier of the artifacts that answered, with `level`, `parents` and `centroid`, as the store reads its points' tags;
3. the same read again;
4. genus and species at zoom 9 over the viewer's densest region.

With `--reopen` it restarts the server over the kept cache and sends the same steps. Beside each step it records the bytes the disk read (from `/proc/diskstats`, so another process's reads count too), the scope's major faults, the one-minute load, the figures counters, and the `PROBE` lines a binary with timers writes. The timers around each fill and each stage of the level-0 request were in the binaries measured here and are not committed.

```bash
cd data/ladder/gbif && set -a && . ./.env && set +a && cd -
python3 probes/2026-10-06-first-open-fills/first_open.py <tessera> \
  data/ladder/gbif/bench-runs-2026-10-06/gbif-new-off-1.json <out.json> --reopen
```

`runs/gbif-before.json` is main at bc546c08, `runs/gbif-after.json` is this branch at 8d8ad7b0. Both ran back to back with the one-minute load between 1.7 and 8.1 (before) and 2.1 and 7.4 (after). Each figure is one run.

## Before and after

### Each request, fresh server (ms)

| viewer | step | before | after |
|---|---|---:|---:|
| 1% | world, level 0 | 6,778 | 1,775 |
| 1% | tags by identifier, first | 10,297 | 1.2 |
| 1% | zoom 9, genus and species | 2,067 | 2,667 |
| 25% | world, level 0 | 8,618 | 4,009 |
| 25% | tags by identifier, first | 16,961 | 1.6 |
| 25% | zoom 9, genus and species | 1,648 | 6,172 |
| 100% | world, level 0 | 10,498 | 9,070 |
| 100% | tags by identifier, first | 25,513 | 1.1 |
| 100% | zoom 9, genus and species | 1,408 | 24,476 |

The first open at zoom 0, the level-0 request and the tag read together, takes 17.1, 25.6 and 36.0 s before and 1.8, 4.0 and 9.1 s after. The genus and species fills have not gone: a viewer pays them at the first request that shows genus or species, which in this sequence is the zoom-9 step. All four steps together take 19.1, 27.2 and 37.4 s before and 4.4, 10.2 and 33.5 s after.

### Each level's fill, fresh server (ms)

| viewer | rows in the fill | family before | family after | genus before | genus after | species before | species after |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1% | 34,956,939 | 3,276 | 290 | 4,048 | 241 | 6,212 | 1,147 |
| 25% | 873,932,430 | 6,841 | 3,560 | 8,248 | 2,165 | 8,638 | 3,245 |
| 100% | 3,495,729,729 | 8,120 | 8,815 | 9,859 | 10,732 | 15,500 | 12,480 |

### The level-0 request besides its fill (ms)

| viewer | before | after |
|---|---:|---:|
| 1% | 3,325 | 1,315 |
| 25% | 1,634 | 420 |
| 100% | 2,283 | 250 |

### After a restart over the cache (ms)

| viewer | step | before | after |
|---|---|---:|---:|
| 1% | world, level 0 | 3,514 | 2,173 |
| 1% | tags by identifier, first | 287 | 1.2 |
| 1% | zoom 9 | 2,771 | 1,502 |
| 25% | world, level 0 | 2,186 | 347 |
| 25% | tags by identifier, first | 339 | 1.1 |
| 25% | zoom 9 | 2,195 | 951 |
| 100% | world, level 0 | 1,218 | 180 |
| 100% | tags by identifier, first | 597 | 1.0 |
| 100% | zoom 9 | 1,026 | 1,439 |

Every reopen loads its figures from the cache and fills none, before and after.

The 100% viewer's zoom 9 after a restart was slower after than before in that one run. Before, the tag read loads the genus and species figures from the cache and zoom 9 finds them held; after, zoom 9 loads them. Two more rounds of the 100% viewer alone, alternating main at df5b7057 and this branch at its head, with the one-minute load between 2.0 and 2.3 at each reopen (`runs/gbif-100-*.json`):

| step after a restart | main, round 1 | branch, round 1 | main, round 2 | branch, round 2 |
|---|---:|---:|---:|---:|
| world, level 0 | 2,898 | 411 | 2,129 | 410 |
| tags by identifier, first | 719 | 1.1 | 534 | 0.9 |
| zoom 9 | 2,880 | 2,074 | 2,325 | 2,058 |
| the three together | 6,497 | 2,486 | 4,988 | 2,469 |

Zoom 9 read 1.6 to 1.7 GB from disk on main and 0.21 to 0.24 GB on the branch, with 3,335 major faults against 214: each fault on a member bitmap now reads its page and not 8 MB around it.

## What the profile showed

A level's fill is one parallel walk of the fragment's base rows on the 12-thread count pool. For each visible row it reads the row's label from the level's label column (2 bytes a row for family, 4 for genus and species) and, because the layer declares a centroid and a box, the row's position from `morton.u32` and the residual column (8 bytes a row). It adds the row to its artifact's count, placed count, position sums and box, and offers it to the artifact's reserve of extreme rows. It intersects no member bitmaps. Building the dense counts from the walk took under 110 ms, and the cache's write to disk runs on its worker after the request.

**Disk.** Before, the 1% viewer's family fill read 13.3 GB from disk for 35 million rows. Its rows are scattered over the row space, and each fault on a mapped page brought in the kernel's 8 MB read-ahead window. At 25% and 100% the rows are dense, and the walk reads the label column and both position columns end to end: about 28 GB for the family level and 36 GB for genus or species at 100%, at about 3 GB/s. Under the 24 GB cap the position columns do not stay in the page cache between levels, so each level reads them again.

**The workers' figures.** Each worker kept a per-ordinal array of counts, placed counts, sums, boxes and a full reserve, initialised whole: 296 bytes an ordinal, so 415 MB a worker and 5 GB across 12 workers for the 1.4 million species. In a stand-alone run of the species walk over 3.4 billion rows (an example program over the bundle's own files, not committed), allocating those arrays took 2 to 4 s of thread time and merging them 0.4 to 4 s of wall time, of a 12 to 14 s fill.

**The level-0 request's extra time.** It was in the per-tile choice of artifacts, which probes each candidate's member bitmap against the tile's visible rows. The member file was mapped with the kernel's default read-ahead, so each probe's fault on a large bitmap read 8 MB around it.

**Persistence.** The written figures were 340 MB for the three viewers' nine fills. Writing them is off the request path, and reading them back after a restart made no fill.

## What changed

1. **A read by identifier builds only the levels it needs.** `POST /v1/artifacts` built every level of the layer, and every level of each layer it hangs from, whatever it read. It now builds the levels its identifiers name, the levels their parents sit at where `parents` is asked for, the requested parent's level, and only the target levels its artifacts hang from where a target or a filter needs them. A paged read with no identifiers builds the levels it walks. An identifier's level comes from the artifact store's existing entity lookup (`locate_artifact`), as the read already did. Test: `artifacts_read::a_read_by_identifier_reads_only_the_levels_it_names` reads a tiered layer by identifiers and checks the fills counter and that the rows equal the read of every level.
2. **The walk asks for scattered pages.** Before a chunk of a million rows is walked, where its visible rows average at least 64 rows apart, the walk asks for exactly the pages of the label column and the position columns under them (`MADV_WILLNEED`). The scan that proposes a tile's candidates does the same. The 1% viewer's level-0 request read 1.1 GB from disk in place of 13.3 GB.
3. **Each worker's figures are one zeroed cache line an ordinal.** Pages are written only where a worker meets an ordinal, a reserve is kept only for the ordinals a worker offers a row to, a row is offered to a side of the reserve only while it can enter it, and the workers' figures are merged ordinal by ordinal over the pool. The figures are the same; `row_column::one_pass_is_a_row_at_a_time_reading` checks counts, sums, boxes and reserves against a row-by-row reading that shares no code with the walk, over chunk sizes from 1 to a million rows.
4. **Member bitmaps are read without read-ahead.** The member file's payload is mapped with `MADV_RANDOM`, the tail of a bitmap a probe opens is asked for, and a reader of a whole bitmap asks for all of it.

The figures' keys, the persisted format and the lookup the aggregate route reads through (`Engine::figures`) are unchanged.

## Candidates tested and not taken

- **A grant that covers every base row.** Its counts are the members' counts over the base rows, which the column already derives when it is opened. This layer declares a centroid and a box, and those need every row's position, so the walk reads the same pages with or without the counts. Taking it would need the build to store each level's whole-base positions, a format change. Not done.
- **A tiered level as the sum of its children.** In five samples of 20 million rows each, 3% to 14% of rows carry no species label against 0% to 10% with no genus label, so some occurrences have a genus and no species. A genus's figures are not the sum of its species', and nothing in the declaration or the stored data guarantees it for another corpus. Not done.
- **Threads.** The walk already runs on the 12-thread count pool. At 25% and 100% it is bound by reading the columns. Not changed.

## What is left

- At 100% each level's fill reads 28 to 36 GB and takes 9 to 12 s. A request that shows genus and species fills both in turn, reading the position columns twice. One walk filling every level a request needs would read them once, saving about 9 s at the 100% viewer's zoom 9. It needs the figures cache to fill several keys from one walk. Not built.
- The 1% viewer's level-0 request still spends 1.3 s in one tile, probing large member bitmaps for a sparse viewer before it switches to scanning the tile's rows. Not investigated further.

## A flat layer published at runtime

A layer registered at a running service recorded every level served from a column as a list column, until a fold observed its memberships. A level's first publication now chooses its layout by the rule a build applies (`tessera_types::layer::choose`, moved there from `tessera_store::derived`), over the memberships the publication carries, before the store applies it (`LayerRegistry::settle_layout`). Replay calls the same function at the same record, so a restart records the same layout. The cost is one union of the publication's own memberships. A registration's initial layouts are the same rule over a level that may overlap. The memberships are observed in entity space; their spread over rows is not known there, so a level not served from a column stays artifact-major until a fold, as before.

A later publication or growth that gives an item a second artifact leaves the record to the next fold, and the level is served from a list column meanwhile: a label column the memberships no longer fit is composed again in the list form, at the form's build and when a held form is amended, wherever the layout is the automatic pick. Under a pin the level is served artifact-major, as before. Checking every record against the level's existing artifacts instead would cost one intersection per artifact of the level per record, under the locks reads take, since the artifact store has no lookup from an item to the artifacts holding it; a union of each label level's members would hold about 0.4 GB a level at full GBIF.

`runtime_layer.py` is the serving-layers bench's probe pointed at a bundle built at this branch (`data/ladder/gbif-64p/bundle-first-open-fills`, format 35, 2.1 GB, built in 74 s), since the bench's own gbif-64p bundle is format 34. It registers `genus-flat`, the genus level declared again as a flat layer, publishes its 56,893 artifacts and 25,088,942 members in 9 requests, and reads it.

| read | main bc546c08 | this branch |
|---|---:|---:|
| layout of `genus-flat` level 0 after publication | list | label |
| 1% viewer, the layer's first read | 3,881 ms | 3,403 ms |
| 100% viewer, after it | 468 ms | 498 ms |

A run with timers split the branch's first read: 1,011 ms building the level's row form from its members, 1,324 ms composing the label column (`project_row_column`), and 859 ms writing the column's member bitmaps. The list column it replaces took 3,034 ms to compose on the earlier run. Most of the first read is still the composition, so the saving at full GBIF would be about a quarter of that cost, not all of it.

```bash
python3 probes/2026-10-06-first-open-fills/runtime_layer.py <tessera> \
  .claude/worktrees/stage5-bench/probes/2026-10-05-serving-layers-bench/runs/new-1.json <out.json>
```

`runs/runtime-layer-before.json` and `runs/runtime-layer-after.json` are the two runs, at a load of 2.1 to 3.1.
