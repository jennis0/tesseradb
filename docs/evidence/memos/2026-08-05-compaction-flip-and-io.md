# The flip and the fold's IO — probes P2 and P3

**Date:** 2026-08-05, P3 extended to the >RAM regime 2026-08-06 · **Status:** Evidence, never
normative
**Subject:** the two measurements `compaction.md` §14 names as gating design choices — **P2**, the
flip's cost against a resident-session population (which decides whether retained-row-space
migration, spec §6.3, is built at all), and **P3**, viewport latency during a corpus-scale
streaming read — which was read as setting the fold's IO throttle rate, spec §6.1, and **which the
r5 review refuted: the fold's inputs are all mappings, so there is no read to throttle.** The harm
this memo measures stands; the mitigation it recommended does not. See recommendation 2.
**Harness:** `crates/tessera-engine/tests/scale.rs`, both `#[ignore]`d so the figures re-run from
the tree. Raw output: `probes/2026-08-05-compaction-flip-and-io/`.

---

## Results

**P2 — the flip is `N` resident entries × a full rebuild, and both terms are now measured.**

| | measured | at |
|---|---|---|
| `derive` — the refresh's per-entry cost on the flush and merge path | **11.1–24.2 ms** | 2.1–2.2×10⁷ rows |
| `cold` — a full rebuild plus a fragment, per entry: **what a fold forces** | **267–352 ms** | the same corpus |
| the ratio between them | **15–24×** | |
| the population term | **linear**: 24.2 ms/entry at N=16, 22.7 ms/entry at N=32 | |

**`compaction.md` §6.2's stated window is confirmed rather than revised.** Scaling `cold` to 10⁹
(48× this corpus) gives ≈12.8 s per entry, against the 10.7 s end-to-end that
`probes/2026-08-04-refresh-ladder/` measured independently for the same work — two routes to the
same figure. At the ~16 entries a 2 GiB projection bound holds, the flip is therefore **≈3 minutes**
for the last session and ≈1.5 for the mean, which is the range §6.2 already states.

**P3 — the risk is real and monotone in read rate, but it is not the orders of magnitude §6.1
feared.** *(The rate arms below were read as choosing a throttle; recommendation 2 records why they
cannot. What they do establish is that streaming the bundle past the page cache costs a concurrent
viewport up to 2.03×, and that the cost falls with the rate at which those bytes move.)* Four runs in the evicting regime — two with a
**45.57 GiB bundle against 36.9–38.2 GiB of RAM** (the real thing, global reclaim) and two with a
**7.84 GiB bundle against a 4 GiB cgroup cap** (a cheaper proxy at a harsher 1.96:1 ratio). The
worst viewport ratio observed at any zoom, per rate:

| read rate | real, 1.24:1 | cgroup, 1.96:1 | reader achieved |
|---|---|---|---|
| unthrottled | 1.24× · **2.03×** | 1.39× · **15.67×** | 1 821–2 150 MiB/s |
| 2 048 MiB/s | 1.04× · 1.29× | — · 2.76× | 1 809–2 048 MiB/s |
| 1 024 MiB/s | 0.97× · 1.10× | — · 1.11× | 1 024 MiB/s |
| 512 MiB/s | 0.98× · **1.48×** | 1.09× · 1.07× | 512 MiB/s |
| **128 MiB/s** | **0.97× · 1.02×** | **1.06× · 0.99×** | 128–129 MiB/s |
| 32 MiB/s | 0.99× · 1.13× | 1.06× · 0.99× | 32–33 MiB/s |
| *noise floor* (a second quiet sweep at the end of each run) | 0.98× · 1.13× | 1.07× · 0.99× | — |

**128 MiB/s is the only rate whose worst observation across all four runs is inside every run's own
noise floor.** 512 and 1 024 are usually quiet and occasionally not; 2 048 and unthrottled are above
noise in every run that reached them.

**The 15.7× must not be quoted as a fold's cost.** It came from a cgroup-capped run, where a hard
`memory.max` puts the allocating task into *direct* reclaim rather than leaving it to kswapd — a
stall the real regime does not reproduce. Both real runs peak at 2.03×. The cgroup proxy is
faithful about direction and unreliable about magnitude, which is worth writing down because it is
the cheap way to reach this regime and it will be reached that way again.

**Neither regime is as harsh as a real deployment's.** Bundle:cache was 1.24:1 real and 1.96:1
capped; a 47 GB bundle on a 16 GB machine is ~3:1, and the effect grows with the ratio — the
cgroup pair, at the harsher ratio, is where the excursions are. The measurement bounds the harm at
the ratios tested; it does not bound it in general.

**The resident-regime run is kept below as the control**: at 0.90 GiB against 44 GiB of cache
nothing is ever evicted, every rate sits inside a 0.79–1.19× band, and the measurement says almost
nothing about a fold. Same probe, same code, no signal — which is why it now prints the regime it
ran in.

## Recommendations

1. **Do not build retained-row-space migration yet** (`compaction.md` §6.3), and the case is the
   measurement rather than caution. It is the largest structural change that document proposes —
   two live row spaces, and a discipline across every row-space read path where a fail-open would
   hide — and P2 shows the window it removes is **proportional to a dial the operator already
   sets**. §6.2's own arithmetic (≈ cache byte budget × 37–85 ms/MB) is confirmed: an operator who
   cannot accept a three-minute degraded window can halve the projection cache bound before a fold
   and halve the window, at a cost §6.3 already prices (the dropped sessions rebuild inline on
   their next request). Revisit when a deployment actually runs at 10⁹ with a full cache **and**
   that trade has been refused. This is a recommendation against a decision the owner holds, not a
   ruling.
2. **⊘ WITHDRAWN at the r5 review — this probe cannot set a throttle rate, because the fold has no
   read to throttle.** Every fold input is an `Mmap::map` (`MortonSlice::load`, `ColumnsRef::load`,
   `Permutation::load`, the postings reader, every delta tier), so its byte movement is page faults
   inside load instructions; this probe reads with `File::read` and sleeps between calls. **The harm
   measured above stands** — page-cache displacement is the same either way — but the mitigation
   does not transfer, and the rate arms bound *instantaneous contention at a rate* rather than
   licensing a fold to run at one. A second limitation compounds it: the 128 MiB/s arm displaced
   0.44 GiB of a 45.57 GiB bundle in a ~3.5 s sweep, under 1%, where a real fold at that rate
   displaces all of it over ~13 minutes — so a quiet arm may mean "not enough bytes had moved yet".
   `compaction.md` §6.1 now carries two candidate mechanisms — `posix_fadvise(POSIX_FADV_DONTNEED)`
   behind each cursor, which removes the pollution rather than slowing it, or pacing the three
   shared streaming producers, which are also flush's and merge's — and an owner ruling is owed on
   which. What follows is the reasoning as it stood before the refutation, kept because the
   *measurement* it rests on is unaffected:

   ~~Set the fold's IO limit at 128 MiB/s (`compaction.md` §6.1), not at the knee.~~ 128 is the
   only rate that stayed inside the noise floor in all four evicting runs at every depth, and
   **§6.1's ruling is what makes buying the quiet outright the right move rather than shaving it**:
   *"a slower fold is an acceptable price for a gentler one"*. A fold reads and writes the whole
   bundle, so at the measured 47 GB bundle 128 MiB/s is ≈13 minutes of extra duration — against an
   operation the same document already budgets in minutes-to-hours, on no request path and no
   deadline. Choosing 512 to save eleven of those minutes buys nothing anyone can observe and takes
   a rate that produced a 1.48× excursion.
3. **Whatever mechanism is ruled, make it a configuration key and hand the operator this probe.**
   The harm follows device bandwidth and bundle:cache ratio, neither of which transfers.
   `compaction.md` §14's row moves from **assumed** to **measured** — the assumption was that
   page-cache pollution is benign, and it is not — while a new row records that the *throttle* is
   refuted rather than calibrated.
4. **The harm grows with bundle:cache and neither run reached a deployment-realistic ratio.** 1.24:1
   and 1.96:1 were measured; a 47 GB bundle on a 16 GB machine is ~3:1. A deployment there should
   expect worse than 2.03× unthrottled — which is an argument for *a* mitigation, not for the one
   this memo originally named.

---

## Method, and what each probe is actually measuring

**Neither probe needs a compaction fold, and there is none.** Each measures the term the fold's
cost is made of.

### P2

The flip's window is `N` resident entries × the per-entry refresh, run by a **serial** loop
(`refresh_resident`), with every request for a key the pass has not reached shed 429 for its
duration (decision 0044's rung 3). Two of those terms are measurable against an ordinary
publication; the third — which rung a fold forces — is settled by construction rather than by
measurement: a fold rewrites `permutation.bin`, so `RowProjection::extends_to` and
`can_rebase_extents` both refuse and every entry takes the full rebuild.

So the probe warms `N` resident entries (`authorise` + one whole-extent viewport each — the refresh
is O(cache residency), so a session that has never asked contributes nothing), publishes a flush,
and times the pass. `cold` is then sampled directly: a freshly authorised session's first
whole-extent viewport is a fragment build plus `RowProjection::new`, which is exactly the work a
fold's refresh does per entry.

**What it asserts is that the mechanism ran** — every resident entry refreshed, and every request
fired into the window shed-then-satisfied rather than failed. A latency bound in a test that runs
on developer machines is a flake generator, so the figures are printed and their home is here.

**The observed shed was zero at every size run.** At a 2×10⁷ corpus the whole pass is 178–725 ms and
the concurrent requests waited 31–80 ms without ever meeting a refusal. That is a statement about
this corpus, not about the mechanism: the ladder's rung 3 is exercised by
`tests/merge.rs` and by decision 0044's own tests, and at 10⁹ the pass is three orders longer while
the request rate is unchanged.

### P3

A background thread reads every file under the bundle root through the page cache, in 1 MiB
buffered reads, looped for the duration of a viewport sweep — the faithful shape of what a fold
does, since the fold streams its inputs through cursors and what a concurrent viewport feels is the
cache filling with bytes it does not want. The sweep is `ZOOM_SWEEP`, taken quiet, then at each of
unthrottled / 2048 / 1024 / 512 / 128 / 32 MiB/s, then quiet again.

**Two ways to reach the evicting regime, and they do not agree about magnitude.** The honest one is
size: `TESSERA_P3_BASE=1200000000` builds a 45.57 GiB bundle against this machine's 36.9–38.2 GiB
of available RAM, so global reclaim evicts as it would on a deployment. It costs **833 s per run**,
almost all of it the base build, and it is where every number quoted above comes from.

The cheap one is a cgroup cap — cgroup v2 charges page cache and reclaims against `memory.max`:

```text
systemd-run --user --scope -p MemoryMax=4G -- <the test binary> --ignored --nocapture <name>
```

which reaches the same *kind* of pressure at a 200M-row corpus in a tenth of the time, at a harsher
1.96:1 bundle:cache. It is the right tool for finding the shape of the curve and the wrong one for
quoting a number: a hard `memory.max` puts the allocating task into **direct** reclaim rather than
leaving it to kswapd, and the 15.7× excursion appears only there. Use the cap to explore, the real
build to publish.

Either way the probe reads its own cgroup's `memory.max` and `MemAvailable`, takes the smaller, and
**prints which regime it ran in** — `RESIDENT` or `EVICTING` — because a ratio from the first says
nothing about a fold and a reader cannot tell the two apart from the latencies. The distinction is
not a formality: the same probe reported 0.79–1.19× resident and up to 15.7× capped.

**The trailing quiet sweep is the noise floor and it is not decoration.** The first version of this
probe took its baseline as the run's first sweep and reported *every* ratio below 1.0 — which reads
as "streaming makes viewports faster" and is really "the first sweep of a run is 1.4–1.8× the
fifth", the first touch of freshly published mappings plus whatever the frequency governor is doing
after a minute of fixture building. The probe now discards a warm-up sweep and prints both
baselines, so a reader deciding a throttle rate sees the error bar rather than having it silently
subtracted.

The throttle itself is a running-average sleep rather than a token bucket, and it holds: achieved
rates were within 1% of target at every setting from 2 048 MiB/s down. **The unthrottled arm is not
a seventh rate**, though — on this device it achieved 1 821–2 150 MiB/s, so it and the 2 048 MiB/s
arm are the same workload with the throttle not binding, which is what makes the two of them an
internal control on the run's noise. In the first large run they disagreed by ~20% (1.02–1.24×
against 0.96–1.04×); in the second they agreed (1.25–2.03× against 1.18–1.29×). That disagreement
is why two samples were taken rather than one.

## What is measured, modelled and assumed

| Figure | Class |
|---|---|
| `derive` 11.1–24.2 ms/entry, `cold` 267–352 ms/entry, at 2.1–2.2×10⁷ | **measured**, this memo |
| the population term is linear in `N` | **measured** (per-entry flat across N=16 and N=32) |
| the flip at 10⁹ ≈ 12.8 s/entry, ≈3 min at 16 entries | **modelled** — linear extrapolation, corroborated by `probes/2026-08-04-refresh-ladder/`'s independently measured 10.7 s |
| unthrottled costs a viewport up to **2.03×** at a 45.57 GiB bundle against 36.9–38.2 GiB of RAM | **measured**, two runs, real regime, global reclaim |
| **128 MiB/s is indistinguishable from quiet** — worst 1.06× across four evicting runs, inside every one's noise floor | **measured** |
| the **15.7×** excursion | **measured, and discounted** — cgroup-capped only, a plausible direct-reclaim artefact, not reproduced in either real run |
| viewport ratios in the **resident** regime: 0.79–1.19× at every rate | **measured**, and kept only as the control — it understates a fold by the whole eviction term |
| that 128 MiB/s transfers to another device | **not measured**, and it plainly does not — the knee follows device bandwidth, which is why the rate is a key and the probe is the instruction |
| what a deployment at ~3:1 bundle:cache pays | **not measured** — worse than 2.03×, direction only |
| the fold's own wall clock at 10⁹ | **modelled** — unchanged by this memo |
