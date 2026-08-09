# Session materialisation at 10⁹ is seconds per session, and nothing shares it

**Status:** Evidence. Measured 2026-08-10 against the rebuilt `data/bench-fixtures/1e9`
fixture (10⁹ items, 47,968 terms), local socket, cold OS page cache not controlled.

## Result

`/session/authorise` returns in 1–2 ms at every principal size. The cost surfaces in the
**first viewport request of the session**, where the visible set actually materialises, and it
scales with the principal's coverage:

| principal | terms | visible | first viewport | warm viewport |
|---|---|---|---|---|
| narrow | 1 | 1,366 | 11 ms | 7 ms |
| sparse ~2% | 2 | 1.5 × 10⁷ | 180 ms | 49 ms |
| medium ~11% | 2 | 8.1 × 10⁷ | 1,135 ms | 150 ms |
| heavy ~51% | 17 | 3.9 × 10⁸ | 5,160 ms | 228 ms |
| full (top 4,096 terms) | 4,096 | 7.7 × 10⁸ | 9,877 ms | 285 ms |

**A second session for the identical principal re-pays the whole cost** — 10,480 ms for the
full principal, measured immediately after the first session's 9,877 ms. Nothing is shared
across sessions today: `caching.md`'s S1/S2 idset-keyed shared caches are specified and NOT
implemented, and this is the workload that prices them. Per tab, per refresh, per principal
switch, a broad principal at this scale waits ~10 s.

The user-facing effect measured ~30 s for a first page load, which serialises several
partially-cold requests (visible box, margin, anticipatory ring) behind the materialisation.
The viewer now labels the wait for what it is (`sessionWarm` in the counts panel), which
changes perception and nothing else.

## What this does and does not establish

- The lazy-materialisation shape means the session plane's own latency figures say nothing
  about time-to-first-pixel at scale; the first *viewport* carries the setup.
- NOT established: where inside the first request the time goes (posting-list page-in vs
  bitmap union vs count). Stage timings were not captured for these runs.
- NOT established: behaviour under concurrent cold sessions — the measurements were serial.

## The decision this prices

Whether to build S1/S2 (idset-keyed shared visible-set cache) or to materialise eagerly at
`authorise` with progress reporting — or both. Owner call; the client can only relabel the
wait.
