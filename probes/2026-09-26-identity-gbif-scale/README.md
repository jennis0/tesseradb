# GBIF rung with a unique `gbifid`, 2026-09-26 and 27

The whole GBIF rung (3,495,729,729 placed rows) prepared, built, verified and served on main
`22ba6444`, with GBIF's own key `gbifid` added as a `unique` `u64`, and then the identity
measurements on the served bundle. Every figure is in [`results.json`](results.json); the logs are
beside it.

The timings belong to this box (WSL2, 12 cores, 47 GiB), which other sessions shared throughout,
building and testing Rust.

The bundle at `data/ladder/gbif/bundle` carries the 1,000,000-row ingest below, and does not reopen
under a 30G cap: the open is killed for memory while it rebuilds the taxonomy's third level (see
"Restart after the ingest").

## What ran

| step | how | result |
|---|---|---|
| prepare | `prepare.py --reuse-taxonomy --holdout 1000000 --duplicates 10000`, `MemoryMax=26G` | 48 m 31 s, 2.4 GiB peak RSS; `points.parquet` 66.5 GB |
| build | `--memory-budget 26g --no-oracle-pairs`, `MemoryMax=30G` | 3 h 26 m 31 s; bundle 269.1 GB (251 GiB) |
| verify --deep | `MemoryMax=24G` | stopped after 5 h 49 m, see below |
| serve and battery | `serve_battery.py --view geo --zooms 0,6,12 --deciles 9 --candidates 40 --samples 10 --cold-samples 3 --text-samples 0`, `MemoryMax=24G` | open 214 s at 6.2 GiB anonymous; six principals in 4 h 03 m, `oom_kill` 0, peak 7.6 GiB anonymous |
| identity | [`probe.py`](probe.py) | below |

The taxonomy's member file and vocabulary were reused from 2026-09-10. A 16-part prefix of a
freshly written member file equals the kept file's first 3,843,932 rows in Arrow.

## Build

**At 22g and 25g the build refused before assigning ids**, 9 minutes in each time. The unique
index adds a 4,096 MiB term to the entity-order model, and the publication batch's term is a
quarter of the budget, so the model needs about 19.2 GiB plus a quarter of the budget: 24,848 MiB
against 22,528, then 25,616 against 25,600. 26g fits. Without `gbifid` the same model fits 24g.

| stage | 2026-09-14 | today |
|---|---|---|
| batch loop (sorts + assignments) | 1,255 s | 1,497 s (213 + 1,283) |
| `attribute_tail` | 1,331 s | 1,832 s |
| `layers` | 1,881 s | 2,001 s |
| `unique_indexes` | — | **606 s** |
| `filter_postings` | 1,076 s | 1,061 s |
| `record_blob` | 806 s | 1,156 s |
| `segment_write` | 577 s | 615 s |
| `artifact_pass` | 1,815 s | 1,988 s |
| wall | 2 h 52 m 33 s | 3 h 26 m 31 s |
| bundle | 196 GiB | 251 GiB |

The unique index is 53 run files, 42.08 GB, 12 B an entry. The rest of the growth is not broken
down here. Peak VmRSS was 29.1 GiB. Anonymous memory was not sampled during the build, only VmRSS
and the scope's `memory.current`, so it cannot be compared with September's 12.1 GB. Free disc fell
from 452 to 116 GiB at the least, other sessions included.

## Verify --deep

Stopped by hand after 5 h 49 m (September: 15 m 08 s). It had read 424 GB from disc (311 GB
through `read`) and written 51.2 GB of spill scratch, with peak anonymous memory 0.52 GiB. Its reads
had stopped hours earlier, and it ran single-threaded at 100% CPU. A 60-second sample
([`ipsample.py`](ipsample.py), [`verify-profile.txt`](verify-profile.txt)) puts 98.3% of the time
in croaring's `roaring_bitmap_rank` (59.6%) and `run_container_cardinality` (38.7%). That is
`RecordBlob::for_each_row_in` (`tessera-filter/src/record.rs`), which calls `hasrow.rank(entity)`
once for each entity, and croaring's rank sums every container before the entity's. Over
3.5×10⁹ entities in about 53,000 containers the walk is quadratic. `check_unique_indexes` reaches
it by reading every row's `gbifid` from the record blob.

## Serve

The open took 214 s at 6.2 GiB anonymous (September: 197 to 202 s, 6.5 to 6.7 GB). The battery's
request has changed since September: it now asks at depth 9 with k = 5,000, so zoom 0 is not
comparable with §4d. Hot zoom 0 took 0.49 s at 1% and 1.3 to 1.8 s at 5%. At 10% it took 3.4 to
27.5 s. At 25% it took 65 to 71 s and was shed on 9 and 10 of 16 samples; at 50% and 100% every
sample was shed. Cold zoom 6 and 12 took 5.0 s at 1%, 7.1 to 7.4 s at 5%, 10.8 to 12.3 s at 10%,
14.2 to 15.9 s at 25%, 18.5 to 21.7 s at 50% and 21.9 to 49.3 s at 100%. Hot zoom 6 and 12 took
7 to 202 ms.

## Identity

Principals: all 253 terms, and `MX` alone (34,702,298 rows, 0.99%). A value set is 10⁶ rows sampled
from 50 row groups spread over `points.parquet`. "Cold" is a fresh server with the index's 42 GB
dropped from the page cache. Every count and every set of rows returned matched the sample
exactly.

| request, through `/v1/items` | all, cold | all, warm | MX, cold | MX, warm |
|---|---|---|---|---|
| `in` 10³, no fields (map order) | 188 s | 193 s | 67.5 s | 41.0 s |
| `in` 10⁵, no fields | 220 s | 222 s | 72.9 s | 72.2 s |
| `in` 10³ / 10⁵ with `count` and `gbifid` (stored order) | 547 / 418 s | — | 4.2 / 4.3 s | — |
| `eq`, one value | 427 s | 479 s | 4.9 s (a holder MX cannot see; answered as absent) | 4.3 s |
| `in` 10⁶ | 413: the 11.9 MB body is over the viewer plane's 2 MiB limit | | | |

A request's time grows with the rows in the viewer's visible set and hardly with the number of
values named. On this binary a read through `/v1/items` paged through the viewer's whole visible
set and tested each row against the rows the index matched. A 3.8×10⁶-row prefix answered the same
`in` 10³ in 0.17 s, about 44 ns a row, and the whole corpus takes about that per row. Main has since
changed the route to read only the matching rows the viewer can see when a unique index bounds the
filter; these figures were not re-measured after that change.

**Ingest.** 1,000,000 held-out rows (real GBIF rows with no coordinate, each given a placed row's
coordinate) in 100 batches of 10,000: all accepted, 69.4 s, **14,411 rows/s**, peak 6.96 GiB
anonymous. The executor flushed and merged the ingested rows on its own ticks while it ran.

**Duplicates.** 10,000 rows, each a copy of a placed row and so carrying the `gbifid` of a live
item, sent one row a request. On this binary the ingest route refused a batch in which a row set
a unique value that another live or suppressed item held. All 10,000 answered 409 and wrote
nothing, each naming the holder's `tessera_id`, in a median 0.68 ms and a p99 of 1.68 ms a request.
The figure is the cost of a request that looks one value up in the unique index and refuses.
On main an ingest row now names the item that holds its unique value: a row carrying what that
item stores answers 200 and counts as `unchanged`, and a row that differs edits the item. These
rows were not re-sent under that rule.

**Restart after the ingest.** The next open adopted the build's derived structures but rebuilt
the taxonomy levels' row forms (`adopted=false`): level 0 took about 8 minutes and level 1 about
10. During level 2 the server was killed for memory at 23.37 GiB anonymous under `MemoryMax=24G`,
and again at 29.4 GiB under `MemoryMax=30G`. Before the ingest, the same bundle opened in 200 to
214 s at 6.29 GiB anonymous. So the check that the held-out rows are served after a restart,
and the explicit flush timing, were not measured.

**Not run.** The fold: its own pre-flight needs 150% of live bytes, about 404 GB, against 125 to
165 GB free. The runtime `unique` declaration and `occurrenceid`: an `occurrenceid` build would add
about 140 GB, which the disc did not have.

## Files

- [`measure.py`](measure.py) runs a command under a cap and samples its memory and the free disc.
- [`probe.py`](probe.py) is the identity driver. It serves the rung through
  `test_corpora/common/deployment.py`, on ports and a scratch cache and WAL of its own.
- The serve battery is `test_corpora/common/serve_battery.py`; with `--boot-rung data/ladder/gbif
  --cap-bytes <bytes>` it serves the rung under a cap and runs against it. `results.json` holds its
  cells in summary.
- [`ipsample.py`](ipsample.py) samples a process's instruction pointers where no profiler is installed.
- `build.log`, `build-stages.json`, `build-refused-25g.log` (the 22g refusal's figures are in
  `results.json`), `prepare.log`, `verify-profile.txt`, `verify-at-stop.txt`.
