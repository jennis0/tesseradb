# What the first request of a fresh server process pays — rung 3 (MedCPT, 36M points)

**Status:** measurement. Numbers here are this host's, and were taken to settle one question: the
demo's first page load after a restart was tens of seconds, and the suspicion was that a level's
artifact-projection build was being paid lazily, on whichever request arrived first.

**It was, and it is now paid at open.** The change is `Engine::warm_artifact_projections`
(`crates/tessera-engine/src/viewport.rs`), called at the end of `Engine::open` — so it completes
before `tessera_server::run` binds a listener, and nothing can reach `/readyz`, let alone a
request, until it has.

## The bundle and the host

`data/ladder/medcpt` — 35,920,666 articles, one view (`knn`), two layers: `clusters/kmeans`
(253 artifacts, artifact-major) and `mesh/descriptors` (30,217 artifacts, a DAG, row-major, a
1.66×10⁹-row membership). 12 cores, 47 GB. A second `tessera serve` over the same bundle was
running throughout, and the page cache was warm — a cold-cache open of the mesh membership
measured 42 s rather than 23 s, so treat the build figures as warm-cache ones.

Driven by `measure.py` (three fresh processes, one per first-request kind), `computed.py` (where
the residue lives) and `twosession.py` (per-process against per-session), each against a copy of
the rung's own `tessera.toml` on spare ports.

## Before — the build lands on whichever request is first

| | first request of a fresh process |
|---|---|
| time to `/readyz` 200 | **10.0 s** (9.9 / 10.1 / 10.1 / 10.2 / 10.3) |
| viewport, `layers=[clusters/kmeans]` | **2.6 s** (2.65 / 2.67) |
| browse of `mesh/descriptors` | **23.3 s** (23.2 / 23.4) |
| viewport with a `member_of` highlight over mesh | **25.8 s** (25.7 / 25.8) |
| viewport, `layers=[mesh/descriptors]` | **26.5 s** (26.1 / 27.0) |

The same request again, in the same process, is 4–50 ms. The cost is **once per process, per
`(view, layer, level)`** — the mesh figure is `ArtifactRows::build_over` over the 1.66×10⁹-row
membership, and the kmeans one the same construction over 168 MB. A viewport naming no layer at
all was 0.17 s, so nothing else about the request path is expensive-cold.

## After — the same builds, at open

| | first request of a fresh process |
|---|---|
| time to `/readyz` 200 | **32–37 s** (32.1 / 33.1 / 33.7 / 35.1 / 36.5 / 36.8) |
| viewport, `layers=[clusters/kmeans]` | 2.2–2.9 s |
| browse of `mesh/descriptors` | **0.45 s** |
| viewport with a `member_of` highlight over mesh | **2.7 s** |
| viewport, `layers=[mesh/descriptors]` | 2.6–3.6 s |

The open's own log line reports it: `levels=2 elapsed_ms=24390`.

**The cost did not disappear; it moved.** What changed is who waits: `run_demo.sh` and any
readiness gate wait for `/readyz`, and a viewer that connects after it never sees a build.

## What is left, and why it cannot move

Everything above is per **process** and principal-independent. What remains on the first request
is per **session**, and `twosession.py` says so: a second session in the same warm process pays
the same 2.2 s a first one did. It is I2 work — computed inside `M_auth`, so there is no
principal-independent form of it to precompute, and materialising one per token over the artifact
population is what decision 0093 forbids.

`computed.py` puts it in one place. A fresh session, whole-map viewport over `clusters/kmeans`:

| `computed` | seconds |
|---|---|
| `[]` | 0.17–0.19 |
| `["centroid"]` | 0.39–0.41 |
| `["centroid", "box"]` | 0.33–0.37 |
| `["shape"]` | 2.9–3.1 |
| absent (every declared property) | 3.1–3.6 |

**It is the hull.** 253 concave hulls over the visible members of a 36M-point corpus, once per
session. The viewer asks for `computed: ['centroid', 'box']`
(`clients/ts/core/src/artifactChannel.ts`), so the demo's actual first page load pays ~0.35 s of
this, not 3 s — the 3 s figure is what a client that asked for hulls would see.

The row-major layer has a per-session cost of its own: a fresh session's first mesh viewport is
~4.5 s against ~2.8 s warm, the difference being the masked histogram over 36M rows under that
session's mask. The ~2.8 s that remains is per *request* and inherent — the gate and a masked
probe over 30,217 artifacts, 98% of which are `everywhere`.

## The open itself, once the mesh level stopped projecting (2026-09-02, later the same day)

The 24 s above is one build: `mesh/descriptors` is served **row-major**, and its column — the very
membership, addressed by row — was already mapped from the prefix, while the artifact-major form
beside it was being reached by projecting all 30,217 memberships through the permutation a second
time. It is now **transposed out of the column** instead (`RowColumn::transpose`,
`ArtifactRows::build_from_column`). Same host, same bundle, warm cache, a second `tessera serve`
running throughout, `open.py` — three clean runs each, interleaved:

| | projected | **transposed** |
|---|---:|---:|
| `mesh/descriptors` row form | 23.6 / 25.1 s | **18.9 / 19.4 / 19.0 s** |
| `clusters/kmeans` row form *(artifact-major, unchanged)* | 0.21 / 0.20 s | 0.25 / 0.20 / 0.21 s |
| the open's own `elapsed_ms` | 23.2 / 24.6 s | **18.5 / 18.9 / 18.5 s** |
| time to `/readyz` 200 | 33.7 / 32.8 / 34.9 s | **29.8 / 30.0 / 29.1 s** |
| peak RSS | 9.07 GB | 9.20 GB |

**A fifth off, and not more.** The projection is not where most of the time was: transposing costs
0.9 s to count the block's entries, 5.9 s to place them and **11.6 s in croaring's inserts**, which
is 1.66×10⁹ of them whichever address the members arrive in. Two negative results beside it:
appending each row to its ordinals' bitmaps as the walk reaches it — no counting sort — measured
**68 s**, three times the projection; and blocks of 2¹⁸, 2²⁰ and 2²² rows instead of 2¹⁶ all
measured slightly *worse*, the placing pass's scatter losing what the inserts gain.

**The extra 130 MB of peak RSS is the column being read.** Adopted and never walked, its 3.46 GB of
mapped bytes were touched only by the requests that scanned it; the transposition reads all of it.

## Re-running

```bash
cp data/ladder/medcpt/tessera.toml <work>/tessera.toml   # absolute [bundle].path, spare ports,
cp data/ladder/medcpt/.env         <work>/.env           # its own cache and WAL
cd <work> && set -a && . ./.env && set +a
COLD_START_WORK=$PWD python3 <repo>/probes/2026-09-02-cold-start/measure.py kmeans browse highlight
```

`open.py <binary> <tag>` is the one the table above was taken with: it starts one fresh process,
times `/readyz`, samples `VmHWM`, stops it, and prints the open's own log lines — so a before/after
is two binaries over one bundle.

The scripts hard-code `127.0.0.1:8211/8212`, the `knn` view and the rung's `branch-terms.txt`.
`measure.py` starts a throwaway process first, both to resolve a mesh artifact for the highlight
and to leave the page cache warm; without it the first process measured pays the disc as well.
