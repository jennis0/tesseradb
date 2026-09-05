# What one row of growth into a large level costs the next request — rung 3 (MedCPT, 36M points)

**Status:** measurement, 2026-09-03. Taken to settle one question raised by
`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`: is the 94–113 s rebuild the
ingest cycle saw caused by the flush, or by the change to the level's records that preceded it?

**It is the record change.** No flush happened here — the flush counter stayed at 0 throughout —
and the next request over the level still rebuilt its row form from scratch and was shed.

## The run

`growth_trigger.py`, against the rung's all-in bundle (`data/ladder/.measure/medcpt36/allin`),
served alone on spare ports with the page cache warm. Same host as `../2026-09-02-cold-start/`.

1. Open, with the warm at open: `mesh/descriptors` built **by transposing the fold-written
   column** (`adopted=false transposed=true`, 16 s inside a 30 s open).
2. Zoom-0 whole-extent viewport, `layers: "all"`: **0.92 s** server time, then **0.14 s**.
3. One ingest batch of one row carrying three `mesh/descriptors` keys (the ingest driver's own
   layer probe), then a `delete` of that row. No flush.
4. The same viewport: **shed at 176.7 s**. The server log shows the level's row form rebuilt
   with `adopted=false transposed=false` — the fold-written column refused because the level's
   version had moved, so the full route: project 1.66×10⁹ memberships through the permutation
   and compose the row-major list column from the result.
5. The same viewport again: served, **0.67 s**.

`result.json` is the driver's record; `serve-log-excerpt.txt` the four log lines that matter.

## What it says

- The window the memo describes opens at **any change to a large level's records** — one
  growth, one publish — and closes at the first request that finishes the rebuild, or at the
  fold. The flush is not the trigger; it is where the ingested rows go missing from the form,
  which is a different defect (the memo's §"The fix", (c)).
- The rebuild here was 177 s against the campaign's 94–113 s. Same bundle, same host; the
  difference is not separated (this run had no second server, but a fuller page cache from
  the campaign that preceded it). Both are far over the 60 s stream deadline.
- The version-keyed refusal of the fold-written column is correct (I11: a persisted structure
  is never adapted). What is wrong is that the *held* form is discarded with it rather than
  brought forward by the one change the store applied.
