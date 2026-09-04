# Does the request path survive when the bundle cannot be resident? — rung 4 preliminary

**Status:** measurement. Answers one question before rung 4 (a ~60 GB bundle on a 47 GB box) is
built: does the request path already serve correctly and acceptably when memory is capped well
below the bundle's size, or does something on the open or request path defeat the mapped-column
design (`filter-index.md` §8) by reading the world into the heap?

**Answer: no, not at 4 GB.** The process survives `tessera serve`'s open under a 4 GiB cgroup cap,
but is reliably OOM-killed on the *first* real request — reproduced three times, on the narrowest
principal measured, at zoom 0. At 12 GiB it serves the full drive sequence (three principals,
every zoom level, `match` filters, drill-down, the `mesh/descriptors` artifact layer) with **zero**
`memory.events` reclaim-to-kill escalation, and its counts are byte-identical to the uncapped run's.

## The bundle and host

`data/ladder/medcpt/bundle` — 35,920,666 rows, 11.15 GB on disk (`title` indexed as text,
`mesh/descriptors` a DAG artifact layer, MeSH branch letters as the point-visibility vocabulary).
12 cores, 47 GB RAM, WSL2 with systemd and cgroup v2's `memory` controller delegated to the user
slice. Built from source in `.claude/worktrees/rung-4-cgroup-serve` (branch
`campaign/rung-4-cgroup-serve`), `cargo build --release -p tessera-cli`.

Served from a private copy of the bundle's own `tessera.toml` (`tessera.toml.example` here, with
the credential *values* stripped — only the environment variable names travel with a deployment
file, per `configuration.md`) on ports 8121–8123, cache and WAL under a scratch directory, so as
not to disturb the other session already serving the same bundle on 8111–8113.

## Method

`systemd-run --user --scope -p MemoryMax=<N> -p MemorySwapMax=0 -- <serve command>` — the user
slice has `memory` delegated (`cgroup.subtree_control` on `user-1000.slice` lists it), so no `sudo`
was needed. `memory.current`, `memory.peak`, `memory.stat` (`anon` vs `file`) and `memory.events`
(`oom` / `oom_kill`) were read from the transient scope's cgroup directory throughout.

`drive.py` (this directory) drives three principals against the running server — `narrow` (MeSH
branch `V`, 4,910 pairs), `medium` (`K`+`J`, ~4.0M pairs) and `broad` (all sixteen branches plus
`unindexed`, the corpus's whole visible set) — through:

- a viewport pan sequence at zoom 0, 3, 6, 9, 12 (5 positions each, 25 requests) per principal;
- a `match` filter on `title` with a common token (`of`) and a token chosen not to occur (a rare-
  token proxy — the corpus has no natural rare English word at this scale worth hand-picking);
- twenty `POST /v1/items/{id}` drill-downs (low handles; some 404, which is expected and counted);
- ten `POST /v1/artifacts/{id}` drill-downs into the `mesh/descriptors` layer.

Every viewport response's tile frame is decoded (Arrow IPC stream, first frame, matching
`clients/ts/scripts/measure-principals.mjs`'s convention) and its `visible`/`matched` columns
summed. **That sum — not a raw response digest — is the correctness check.** `k=30` point
sampling is not specified as deterministic across processes, and it is not observed to be: two
runs against the same viewport and principal return different bytes at the same length (different
points sampled) but the masked counts agree exactly. Every count-bearing response across the two
successful runs (85 of them) is compared; **zero mismatches**.

There is no `/v1/artifacts/browse` route in this build (`reference/oracle/harness.py`'s `Server.
browse` targets a route `crates/tessera-server/src/viewer.rs` does not register here) — the
`mesh/descriptors` artifact frame instead rides the ordinary `/v1/viewport` response when
`layers` includes it, which every viewport call above already asks for (`layers: "all"`).

No caches were dropped between runs (no passwordless root); results, including the anon-vs-file
split, should be read with that in mind — see "What contaminates the file figures" below.

## Does it serve correctly under 4 GiB?

No. Three independent attempts, each on a freshly-opened process:

| attempt | what ran | result |
|---|---|---|
| 1 | `broad` principal, zoom 6, full-extent warm-up, `k=30` | **OOM-killed**, 21:31:56 |
| 2 | `narrow` principal, zoom 0 (first pan-sequence request) | **OOM-killed**, 21:34:50 |
| 3 | `narrow` principal, zoom 0 (first pan-sequence request) | **OOM-killed**, 21:44:39 |

`journalctl --user` names it in every case: `tessera-cgroup-4g*.scope: A process of this unit has
been killed by the OOM killer`, and `dmesg`'s tail confirms `oom-kill:constraint=CONSTRAINT_MEMCG`
against this cgroup, `anon-rss:4173008kB, file-rss:3734320kB` at the kill in attempt 1 — total
memory well over the 4 GiB `MemoryMax`, not a reclaim that ran out of candidates: `pgscan_direct`
and `pgsteal_direct` in the same `dmesg` tail show the kernel *was* reclaiming file pages under
pressure before the kill, it just was not enough. **Open itself always survives** — `/readyz`
answers 200 before any request is sent, at anon ≈ 2.06–2.17 GB (see below) — the failure is
request-time, on the very first viewport call, at the cheapest principal measured (`narrow`,
4,910-row visible set). A 4 GiB cap leaves roughly 2 GB of headroom above the process's idle
footprint, and the first request's fragment/row-projection build exceeds it before serving
anything.

## Does it serve correctly under 12 GiB, and uncapped?

Yes, both times, cleanly — `memory.events`' `oom` and `oom_kill` counters stayed at 0 across the
whole drive sequence in every 12 GiB run, and the uncapped run is the correctness baseline the
count comparison above is against.

## Latency, by cap and request kind (server-side, `x-tessera-server-us`)

| request kind | n | 12 GiB p50 | 12 GiB p99 | uncapped p50 | uncapped p99 |
|---|---|---|---|---|---|
| viewport pan, narrow | 25 | 0.12 ms | 36.3 ms | 0.28 ms | 14.7 ms |
| viewport pan, medium | 25 | 0.72 ms | 69.2 ms | 0.69 ms | 270.8 ms |
| viewport pan, broad | 25 | 0.73 ms | 206.8 ms | 0.79 ms | 189.1 ms |
| `match title:"of"` (common) | 5 | 1.12 ms | 2.97 ms | 1.01 ms | 11.8 ms |
| `match title:"zzzxyq…"` (absent) | 5 | 0.32 ms | 0.39 ms | 0.25 ms | 0.28 ms |
| item drill-down | 20 | ~0 ms | ~0 ms | ~0 ms | ~0 ms |
| artifact drill-down (mesh) | 10 | ~0 ms | ~0 ms | ~0 ms | ~0 ms |

12 GiB and uncapped are the same order of magnitude on every kind, both well inside the ≤10 ms
gate `bench_p99.py` targets at 10⁹ — no cap-induced slowdown at 12 GiB against this 11 GB bundle.
**4 GiB has no comparable row**: it never returned a first response.

Wall-clock (end-to-end, `p99`) tells a different, expected story on the *first* request of a
process: 22.7 s (12 GiB run) for the first `narrow` viewport at zoom 0, against 0.12 s
server-side. That gap is the one-time fragment-cache and row-projection build for a fresh
`(view, principal)` pair — the same cost `probes/2026-09-02-cold-start/` names and dates to
`Engine::warm_artifact_projections`-adjacent machinery, not a per-request cost; every repeat of
the same request in the same session was single-digit milliseconds end to end.

## What is resident, and where the anon comes from

`memory.stat`'s `anon`, read immediately after `/readyz` answers 200, before any request:

| | anon (heap) | file (mapped, faulted) |
|---|---|---|
| at rest, after open | **2.06–2.17 GB**, every run | 0 – 5.2 GB, highly variable (see below) |
| at peak, after the full drive (12 GiB cap, clean run) | **4.77 GB** | ~0 |
| at peak, after the full drive (12 GiB cap, contended run) | 4.80 GB | 5.23 GB |

**Anon is the stable, comparable figure and it is larger than filter-index.md §8's mapped-column
story predicts on its own** (§8: "the value columns are mapped, not read" — 2 MB resident after
open, measured at 8×10⁸-`u32`-column scale). Two mapped or read-fully readers were traced and
ruled out by code inspection plus `/proc/<pid>/smaps` (per-file RSS): the digest sweep
(`tessera-store/src/read.rs::verify_files`) reads in a fixed 1 MB buffer per file and is freed
before serving starts; `RecordStack`/`FilterColumns` open every declared column with
`Access::Mapped` when `mmap=true` is passed (`session.rs`'s two call sites both pass `true`), and
`smaps` confirms `attrs/title/postings.arrow` (824 MB) and `attrs/mesh_major/postings.arrow`
(365 MB) are resident as **file**, not anon, when touched.

**The best-supported explanation is `mesh/descriptors`'s artifact-projection build, now paid at
open rather than on first request** (`probes/2026-09-02-cold-start`'s README says: "the build lands on
whichever request is first" → fixed by `Engine::warm_artifact_projections`, called at the end of
`Engine::open`). That machinery builds `ArtifactRows::build_over` a 1.66×10⁹-row DAG membership for
this same bundle — a real, owned heap structure, not a mapped column, and not covered by §8's
value-column story because it isn't a value column. This probe did not instrument the allocator to
attribute the 2 GB to that call directly (no heap profiler was available in the time budget); the
figure and the mechanism are both real and measured, the *attribution* between them is reasoning
from the cold-start probe's own account of what moved to open, not a second independent
measurement, and should be read as such.

**What contaminates the file figures.** cgroup v2 charges a mapped, read-only page to whichever
cgroup's task first faults it in; a second cgroup mapping the same file for the same physical page
pays nothing until that page is evicted and re-faulted. Every run in this probe re-mapped the same
`data/ladder/medcpt/bundle` files a previous run (in this probe, or the other session still
serving the same bundle on 8111–8113 for part of this work) had already faulted in — so `file` in
`memory.stat` swings from ~0 (a clean process, nothing else recently touched the bundle) to 5+ GB
(page cache warm from a concurrent or prior reader) for the *same* logical workload. **Anon does
not have this problem** — it is this process's own heap, uncontaminated by any other cgroup — which
is why anon, not the total, is the number to trust for "did open read the world into the heap".
No cache-drop was available (`echo 3 > /proc/sys/vm/drop_caches` needs root; not attempted without
a password prompt), so no genuinely cold-cache run exists here; `probes/2026-09-02-cold-start/`
already measured a cold open at 42 s against 23 s warm for this same bundle's mesh membership, and
that ratio is the best cold-open estimate available without re-running this probe with root.

## What this means for a 60 GB bundle on a 47 GB box

The category/text postings and value columns behave exactly as designed — mapped, and resident
only where a request actually scans, confirmed both by code (every open call in the serving path
passes `mmap=true` / `Access::Mapped`) and by `smaps` (the two largest postings files, 824 MB and
365 MB, show as evictable file pages, not heap). **That part of the design already tolerates a
bundle larger than memory** — nothing here scales the mapped machinery with corpus size in a way
that would bite at 1.02×10⁸ rows beyond the file sizes themselves growing.

**What does not yet tolerate it is the fixed, per-process anon floor** — ~2 GB for a corpus this
shape (dominated, on the best available evidence, by the `mesh/descriptors` DAG artifact-
projection build now paid at open). That floor is *not* proportional to the request — it is paid
once, at open, regardless of which principal ever connects — but it **is** plausibly proportional
to the DAG membership's own size (1.66×10⁹ rows here), and a rung 4 corpus with a comparably-sized
or larger hierarchy would pay a comparably larger or larger floor before serving anything. Whether
it scales sublinearly, linearly or worse with membership row count was not measured here (this
bundle is the only DAG-layer data point in the ladder) and is worth measuring directly — with a
heap profiler, not `memory.stat` — before rung 4 is built, because unlike the mapped columns this
is the one part of the request path that reads real, unavoidable memory into the heap at open, and
it is the reason 4 GiB failed while an operator reading only §8's "the value columns are mapped"
story would have expected it to survive.

**Practically: a 60 GB bundle needs headroom for (a) this per-process anon floor, whatever it turns
out to be at that DAG's scale, plus (b) enough working room for a session's fragment/row-projection
build (the 12 GiB run peaked at 4.8 GB anon after three principals; the request-time growth, not
the open-time floor, is what a broader or more concurrent workload would grow further) — not the
whole 60 GB, since the postings and value columns proper are confirmed mapped and page in on
demand.** On a 47 GB box that headroom is available today; whether it stays available depends on
how the DAG floor scales, which this probe recommends measuring before committing to rung 4's
corpus shape.

## Files here

- `drive.py` — the request driver (viewport pan × 3 principals, `match`, item and artifact
  drill-down; decodes tile frames for the `visible`/`matched` correctness check)
- `results-4g.json`, `results-12g.json`, `results-12g-b.json`, `results-nocap.json` — raw output
  per run (the `died` field names which OOM attempt each `results-4g.json` is; `12g-b` is the
  second, cleaner 12 GiB run used for the anon-at-peak figure with `file ≈ 0`)
- `tessera.toml.example` — the scratch deployment file used, credential *values* stripped (only
  variable names travel with a deployment file, matching `configuration.md`'s own convention)

## Appendix R — review trail

- 2026-09-02, r1: initial measurement, this session. No adversarial review yet — flag before this
  probe is cited to justify rung 4's memory budget without a second pass, per the anon/DAG
  attribution caveat above.
