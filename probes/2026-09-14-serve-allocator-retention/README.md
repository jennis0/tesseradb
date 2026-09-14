# Allocator retention on the serve path: what glibc holds, and what a trim returns

Status: observation taken 2026-09-14 on the rung 6 server at main `488e43e5`, battery taken the
same day on branch `serve/allocator-retention`. **Not normative.** No design document is amended by
it; `docs/design/caching.md` §4's sizing rule is untouched. Re-take a figure before relying on it —
see *What was not measured*.

## What was asked

A serving node's caches are bounded and accounted for. Whether the memory those caches release
goes back to the kernel is a separate question, and nothing on the serve path was asking it: there
was no `malloc_trim` anywhere in `tessera-engine`, `tessera-server`, `tessera-store` or
`tessera-authz`, and no `MALLOC_ARENA_MAX`. The build has trimmed at every stage boundary since the
whole-corpus build held 34 GB without it.

## The rung 6 observation

One `tessera serve` over the 196 GiB GBIF bundle, `MemoryMax=24G MemorySwapMax=0`, 39 threads
(12 tokio workers and 25 pool), glibc 2.35, after a six-principal battery.

| | |
|---|---|
| `RssAnon` | 14.96 GiB |
| of which inside glibc | 14.15 GiB |
| non-main arena heaps | 373, holding 8.87 GiB resident of 23.31 GiB of address space |
| main arena | 5.28 GiB |
| the server's own cache accounting | 127 MB |
| `memory.events max` | 19,915,452 |
| `workingset_refault_file` | 1.2 × 10⁹ |
| read in 104 minutes | 8 TB |

Two readings follow. The growth is a **ratchet that saturates**: zero over the last 21 minutes
under the heaviest load, a seventh principal cost 9.5 MB, and nine further requests cost nothing.
And on this node the retention was not free — the cgroup sat 86 KB under its cap, so every byte the
allocator held was a byte of the bundle's page cache evicted, and the bundle is what the next
request reads.

## The 64p battery

`data/ladder/gbif-64p` rebuilt into a scratch path (25,846,007 rows, format 8,
`tessera build --memory-budget 4g --no-oracle-pairs`), served under
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0` on ports 8241–8243, with
`RssAnon`/`RssFile` sampled every second. One row per binary; "battery" is the reduced battery
below, and a run of three batteries repeats it in one process.

| run | RssAnon open / peak / end | trims | trim cost → bytes returned |
|---|---|---|---|
| main `488e43e5`, 1 battery | 379.7 / 657.0 / 657.0 MiB | — | — |
| this branch, 1 battery | 373.6 / 629.3 / 629.3 MiB | 0 (growth 255.8 MiB, under the 256 MiB threshold) | — |
| main, 3 batteries | 378.5 / 884.8 / 884.8 MiB | — | — |
| this branch, 3 batteries | 376.6 / 872.3 / 859.8 MiB | 2 | 7.4 ms → 67.7 MB; 11.1 ms → 100.1 MB |
| this branch at a 16 MiB threshold, 2 batteries | 372.1 / 763.1 / 741.9 MiB | 102 | ~2.1 ms each, 7.6–18.4 MB apiece |

At equal work — two batteries — `RssAnon` was 848 MiB on main, 787 MiB with the cadence at its
shipped threshold, and 742 MiB at 16 MiB. A trim's cost tracks what it returns rather than how
often the walk runs, so a smaller threshold does not cost more in total; it returns memory sooner.

**Most of this scale's anonymous memory is live cache, not retention.** After two batteries the
masked-count cache alone held 268 MB of about 900 MB of `RssAnon`, which is why the trims returned
7–11% rather than the rung 6 fraction. That is the reading the `/control/status` cache blocks
beside `heap` exist to make available.

### The arena cap

One battery each, same trim threshold, the only difference being `mallopt(M_ARENA_MAX)`: the
shipped cap, which is the compute pool's width, against the same binary allowed 4,096.

| | uncapped | capped at 12 |
|---|---|---|
| arena-shaped anonymous regions in `/proc/pid/maps` | 44 | 15 |
| anonymous address space | 3,013 MiB | 1,157 MiB |
| `RssAnon` at end | 604.0 MiB | 619.3 MiB |
| hot p50, zoom 0, 100% principal | 25.96 / 25.97 ms | 26.04 / 26.29 ms |
| hot p50, zoom 6, 100% principal | 2.73 / 3.04 ms | 2.58 / 2.76 ms |
| hot p50, zoom 12, 100% principal | 12.80 / 29.58 ms | 13.78 / 29.49 ms |
| one trim | 6.96 ms → 47.9 MB | 6.79 ms → 63.1 MB |

Two cells per zoom, both given. The latency differences are inside the scatter between repeats of
one binary: three batteries in one capped process gave 14.53, 14.69 and 15.46 ms at the first
zoom 12 cell. So the cap costs nothing measurable at this concurrency and takes 1.9 GiB off the
address space. An arena-shaped region is counted as an anonymous mapping of at most 64 MiB at a
64 MiB-aligned address, which is how glibc maps a non-main arena's heap; it is a proxy for the
arena count, not a reading of it.

### The served answers are identical

Every run and every pass produced the same `(target, zoom, cell, visible)` table — one SHA-256
digest, `e5eeaaaf35ebaee1` — and the same `measured_visible` per principal (258,459 / 1,292,304 /
2,584,596 / 6,461,499 / 12,923,003 / 25,846,007 against 25,846,007 rows). The change is memory
behaviour and nothing else.

## Commands

```
tessera build --deployment <scratch>/tessera.toml --memory-budget 4g --no-oracle-pairs

python -m test_corpora.common.serve_battery \
    --viewer http://127.0.0.1:8241 --session http://127.0.0.1:8242 \
    --session-cred "$CRED" --bundle <scratch>/bundle --cache <scratch>/cache \
    --ranks data/ladder/gbif-64p/country-ranks.json \
    --server-pid <pid> --cgroup <scope> --cap-bytes 8589934592 \
    --zooms 0,6,12 --deciles 9 --candidates 40 --samples 10 --cold-samples 3 \
    --text-samples 0 --out <label>.battery.json
```

The server was booted by `test_corpora/common/deployment.py`'s `Deployment` inside the transient
scope above and stopped by pid. `RssAnon`/`RssFile` came from `/proc/<pid>/status` once a second;
the trim's cost and bytes returned came from `/control/status`' `heap` block, which this branch
adds; the arena-shaped region counts came from `/proc/<pid>/maps` taken before the server was
stopped.

## What was not measured

**The cap under a saturated blocking pool.** The battery is sequential: one request in flight,
against a gate admitting far more, so the threads that would contend for a shared arena — up to
`4 × compute_threads` blocking threads plus `ingest_admission` plus the pool's 32 — were never all
allocating at once. Lock sharing is where the cap can cost, and this run does not reach it.

**A node whose ratchet has saturated.** The 64p run climbs throughout, so the cadence's
self-extinguishing property — trims stop when growth stops — is argued from the rung 6 observation
and not exercised here.

**Retention created below the baseline while RSS is flat.** The cadence keys on growth, so a
process that frees and re-allocates at a steady `RssAnon` never trims. Rung 6's retention arrived
as growth; whether a steady-state workload accumulates free chunks without growing is unmeasured.

**The trim's effect on request latency.** It runs on a blocking thread and the battery is
sequential, so no sample overlapped a trim by construction.

**Anything above 25.8 × 10⁶ rows on this branch.** The rung 6 figures are main's, taken before the
change; no capped-and-trimming run over the 196 GiB bundle has been taken.
