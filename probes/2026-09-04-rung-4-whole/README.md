# Rung 4, whole: the build completes

**Date** 2026-09-04. **Branch** `campaign/rung-4-build`, binary from `main` at `604ec296`.
**Box** WSL2, 12 cores, 47 GB, local NVMe-backed VHDX, 448 GB free at the start.
**Corpus** `$TESSERA_LADDER/paperseek` — 102,117,343 OpenAlex works, 118.9 GB of abstracts
indexed as text, nothing trimmed.

**This is the raw output. The reading of it is [`docs/ingest-campaign.md`](../../docs/ingest-campaign.md)
§4b**, and the rung's own summary is [`test_corpora/paperseek/README.md`](../../test_corpora/paperseek/README.md).
Every figure here is also in `test_corpora/paperseek/measurements.json` on the committed schema.

## The finding, in one table

| | before (§4a, §4b's own "before" column) | this run |
|---|---|---|
| `tessera build` over 1.02×10⁸ rows | ⊘ never finished — the text index stalled, and when that was fixed the record blob did | **10,578.4 s — 2 h 56 m 18 s**, exit 0 |
| `record_blob` | ⊘ over four hours making no progress, 52 MB at 56 KB/s, ~144 major faults a second | **905.4 s at 0 major faults a second**, 133.6 MB/s read |
| `attribute_tail` | 704.7 s | **6,369.1 s** — the second decode and the entity-order scatter, which is what the blob was bought with |
| bundle | ⊘ projected ~76 GB from a 10⁷ prefix | **70,783,029,628 B — 70.78 GB** |
| `verify --deep` | ⊘ no bundle | **clean in 74.24 s** |
| served under `MemoryMax=24G` | ⊘ no bundle | **`oom_kill` 0**, `memory.peak` at the cap, open 98.9 s |

⊘ **The box carried rung 5's share passes and GPU work throughout.** No other `tessera build` and
no serve battery ran, so the disk was this build's alone; the CPU was not.

⊘ **The arm most likely to beat this one was not run** — an arrival-order arena with the ascending
scatter, which is what made `record_blob` finish at 10⁷ under a 4 GB cap
(`probes/2026-09-03-entity-ordered-arena/` §3). At 10⁸ the entity fill costs 9× the join where at
10⁷ it costs 2.04×, and the extra is the scatter rather than the decode: the join's chunk buffer
does not grow with the corpus, so the arena is written as ~54 interleaved ascending runs here
against six at 10⁷. One more three-hour build settles it.

## What is here

| | |
|---|---|
| `build.rss.csv` | `sample_rss.py` at 100 ms over the whole 2 h 56 m — `RssAnon`, `RssFile`, `majflt`, `read_bytes` and PSI io. 105,000 samples |
| `build.stages.csv` | the build's own stderr, timestamped on the same clock |
| `stage-timings.json` | `--stage-timings-json`, the machine-readable stage record |
| `serve-cap24g.json` | the battery's own output, before collation |
| `verify.log`, `breakdown.txt` | `verify --deep` and the bundle's size by family |

Re-read the RSS trace against the stages with:

```bash
python3 probes/2026-09-03-text-arena-streaming/sample_rss.py --report --out build
```

The bundle itself is not committed and is not kept: 70.78 GB under
`$TESSERA_LADDER/paperseek/`'s volume, rebuilt by

```bash
cd $TESSERA_LADDER/paperseek && set -a && . ./.env && set +a
tessera build --stage-timings --stage-timings-json stage-timings.json --arena-order auto --out <out>
```
