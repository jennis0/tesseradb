# Bounded assembly: memory, writes and faults per stage

Whether a build's memory stays under its budget and its writes stay close to the bytes it adds
to the bundle, as the row count grows. This is the measurement the bounded-assembly design's
acceptance turns on ([`2026-09-12-bounded-assembly-design.md`](../../docs/evidence/memos/2026-09-12-bounded-assembly-design.md)
§8): write bytes within 1.5× of bundle growth in every stage, no major faults, anonymous RSS
under the budget throughout.

## What it measures

Four prefixes of `data/ladder/gbif` are built with two binaries each, one before the change and
one after, at a 24 GB budget. A prefix is `--limit N` over the points together with the
membership file cut to the same prefix (`limit_members.py`), because `--limit` limits the points
alone and a membership row naming an entity past it is refused by the layer publication.

Beside each build, a sampler reads `/proc/<pid>` every two seconds and walks the bundle root:

| | |
|---|---|
| `status` | `RssAnon`, `RssFile`, `VmSwap`: what the process holds anonymously, in page cache charged to it, and in swap |
| `stat` | `utime + stime`, `minflt`, `majflt`: a major fault is a page read from disk, so a stage that faults is one whose working set the cache is not holding |
| `io` | `read_bytes`, `write_bytes`: bytes the process moved through the block layer, which is what write amplification is counted in |
| the bundle root | allocated bytes, `st_blocks` × 512, so a sparse or fallocated file is charged what it occupies rather than what it spans |

`report.py` differences those counters across each stage's `started_at` and `ended_at` from
`--stage-timings-json`, and prints per stage: wall time, peak anonymous RSS, write bytes against
bundle growth and their ratio, major faults, and bytes read. Then one row per binary for the
whole build: wall time, peak anonymous RSS, peak allocated disk.

Every figure the probe prints is measured. The acceptance thresholds it is read against are the
design's, and the 1.5× ratio is a chosen bound, not a measurement.

## What it cannot attribute

- **A stage boundary is a wall-clock timestamp, and a sample is two seconds wide.** A counter
  is differenced at the last sample at or before each boundary, so up to two seconds of one
  stage's work is charged to its neighbour. A stage shorter than a few seconds is not
  measurable this way.
- **Four stages do not own their interval.** `text_index`, `record_blob`, `column_release` and
  `filter_postings` are charged from inside one column loop: the duration is measured, but the
  interval it is reported at is not when the stage ran. Their rows carry the counters of
  whatever ran over that interval, and the report marks them.
- **The counters are the whole process's.** Concurrent stages, and any thread pool, are summed
  together. The probe says which stage the process was in, never which of its threads wrote.
- **`write_bytes` is what the process sent to the block layer.** Page-cache writeback of a
  mapped file is accounted to the process that dirtied the page, but a bundle file's growth is
  seen only when the sampler walks the tree, so a file written and unlinked between two samples
  contributes writes with no growth.
- **Growth can be negative.** A stage that unlinks scratch shrinks the bundle root; the ratio
  is left blank rather than signed.

## How to run

`WORK` wants room for the largest single bundle, on the same filesystem as the builds: each
bundle is deleted as soon as its run has been sampled, so only one exists at a time.

```bash
BEFORE=/path/to/tessera-before AFTER=/path/to/tessera-after \
LADDER=data/ladder WORK=/scratch/bounded-assembly \
LIMITS="16299326 30104813 64657133 125789091" \
bash probes/2026-09-12-bounded-assembly/run.sh
```

`LIMITS="2000000"` is a short run for checking the scripts. `BUDGET` overrides the 24 GB
`--memory-budget`. The corpus's `.env` is sourced for the identity key. Each build runs in its
own session under `setsid` and is killed only by the pid this script started; nothing matches a
process by name.

The cut membership files are kept in `$WORK` and reused across binaries and runs. A report per
limit is written to `$WORK/report-<limit>.md` and printed; per run, `$WORK/<before or
after>/<limit>/` keeps `proc.tsv`, `stages.json` and `build.log`.

## Results

Not yet run.
