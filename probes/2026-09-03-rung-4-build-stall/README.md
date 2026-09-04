# Rung 4's raw output — three builds, four drives, and one stall

**Status:** Raw measurement. The campaign's reading of it is
[`../../docs/ingest-campaign.md`](../../docs/ingest-campaign.md) §4a and the rung's is
[`../../test_corpora/paperseek/README.md`](../../test_corpora/paperseek/README.md); neither is
restated here. This directory is the files those two were written from, kept because the headline is
a **negative** result and a negative result that cannot be re-read is an assertion.

Measured 2026-09-02/03 on this box (WSL2, 12 cores, 47 GB, RTX 3080, local NVMe), binary
`cargo build --release -p tessera-cli` from `.claude/worktrees/rung-4-vectors`.

⊘ **The box was not quiet for the stall.** Another track's 3.2×10⁷-row MedCPT base build ran on the
same disk from 03:48 and two serve batteries were driving cgroup `memory.reclaim` eviction beside
it, for most of the 03:13–06:25 window. The mechanism is not in doubt; the fault rates, the PSI
figures and the wall in `build-full.*` **include contention** and are not that build's cost alone.

| | |
|---|---|
| `stage-full.log` | the one pass off the share — 53 chunks, 164.7 minutes |
| `routes.json` | the fit-size measurement at 1024 dimensions that set `FIT_ROWS = 1,500,000` |
| `prepare-1m.log`, `prepare-full.log` | `prepare.py` at 10⁶ and at the whole 1.02×10⁸ |
| `build-1m.*` | the 10⁶ build — 23.3 s, 744.3 MB. `.rss.csv` and `.stages.csv` are `sample_rss.py`'s two streams |
| `build-full.*` | **the stall.** `.log` ends at `layers`; `.rss.csv` runs to the kill; `.faults.csv` is major faults, `utime`/`stime` ticks and the abstract spill count every 30 s; `.stall.txt` is the thread states, PSI and arena sizes sampled four hours in and again at the kill |
| `build-10m.*` | the 10⁷ prefix that brackets it — 545 s, 7.44 GB, `text_index` **178 s** |
| `drive-1m.json` | the 10⁶ bundle driven on 8131 |
| `drive-10m-nocap.json`, `drive-10m-24g.json` | the 10⁷ bundle, uncapped and under `systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0`. 85 of 85 count-bearing responses agree between them |

**Reading `build-full.faults.csv`.** Columns are `t,majflt,utime_ticks,stime_ticks,spills` — seconds
since the sampler started (not since the build), the process's cumulative major faults, its user and
system CPU in 100 Hz ticks, and how many `text-run-*.spill` files the abstract column had written.
The first four rows have six fields rather than five: a duplicate sampler ran briefly and was killed.
Between t=0 and t=2249 the spill count went 33 → 44, which is the evidence that the stage was
progressing and would not have finished.

**The arena is the finding.** `build-full.stall.txt` records `.build-tmp/column-13.arena` at
**137,438,953,472 bytes — 128 GiB exactly** — the abstract column's preallocated, mapped arena,
against 47 GB of RAM. Nothing in this directory was produced with a `--memory-budget` arm, a trimmed
declaration or the abstracts dropped: the brief was to report the build rather than patch it.
