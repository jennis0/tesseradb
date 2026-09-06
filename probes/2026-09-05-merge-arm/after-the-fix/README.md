# The same probe after the merge rebases the held form — rung 3 (MedCPT, 36M points)

**Status:** measurement, 2026-09-06 (run started 02:34 local; the serve log is in UTC).
`merge_arm.py` one directory up, run against the branch that rebases every held row form over the
merged extent at the merge's publication, before the swap, instead of letting the next request
project the level again (`crates/tessera-engine/src/artifacts.rs`,
`ArtifactProjections::rebase_merged`; `docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`).

## The run

The same driver, the same clean hardlinked copy of the all-in bundle (recreated from the original
after each run, as the parent README says), the same four ingests of 1,000 fresh entities each
flushed at once, and the executor's own merge.

| step | before (main at b02593eb) | after |
|---|---|---|
| open | 33 s | 30.7 s |
| after open, warm | 250 ms, then 142 ms | 249 ms, then 131 ms |
| after flush 1 … 4 | 168, 158, 150, 133 ms | 163, 148, 159, 129 ms |
| merge published, after the fourth flush | 84 s | 84.3 s |
| **first request after the merge** | **shed at 107.9 s** | served, **130 ms** |
| second request | 161 ms | 140 ms |

`result.json` is the driver's record; `serve-log-excerpt.txt` the build lines and the two rebase
lines.

## What the server said

The rebase is on the executor thread, where it blocks every ingest and every deny, so its cost is
the number to watch:

```
a level's held row form took a merge's rebase layer=mesh/descriptors level=0 view=knn
  seg_id=merge-4-1 span_rows=4000 rows_relabelled=0 elapsed_ms=27
```

`rows_relabelled=0` is correct: the probe's points are fresh entities in no membership, so the
merged span holds no labelled row. What the run measures is the cost of a merge against a
30,217-artifact level, not the arithmetic of one; the arithmetic is asserted by
`tests/artifact_bring_forward.rs` against a form built from scratch.

A first run of the same binary read `elapsed_ms=3426` for the same line. `MembershipRows::rebase_rows`
made every artifact's bitmap mutable, and the form is still in the cache while the rebase runs, so
every one of the 30,217 bitmaps — 1.66×10⁹ entries — was copied for a merge that changed none of
them. An artifact with no row in the span, before or after, is now left alone
(`range_cardinality` over the span, O(containers)), and the line reads 27 ms.

## The bundle is not reusable

The parent README's reason holds: a run writes side manifests into the copy it serves and a later
open carries forward only the derived extents whose level version still matches, so the copy is
recreated from the original after every run. Nothing here changes that.
