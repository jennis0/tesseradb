# What a merge costs the next request naming a large level — rung 3 (MedCPT, 36M points)

**Status:** measurement, 2026-09-06 (run started 2026-09-05 23:02 local). Taken to settle the one
arm the held-form fix left on the request path (`docs/design/annotation-write-cycle.md` §4.1,
`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`): after a row-space merge the held
row form no longer covers the row space, is dropped, and the level is projected whole by the next
request that names it. Whether that matters at the fold cadence is what this asks.

**It costs the request that follows the merge 108 s, and it is shed.** Every flush before the
merge cost nothing on the request path.

## The run

`merge_arm.py`, main at b02593eb, against a hardlinked copy of the rung's all-in bundle with only
the build's own side manifest (see "The bundle" below). Warm at open; the zoom-0 whole-extent
viewport with `layers: "all"` timed after each step. Four ingests of 1,000 fresh entities, each
flushed at once (four tiny segments, one tier, `tier_width` 4), then the executor's own merge.

| step | request, server time |
|---|---|
| after open, warm | 250 ms, then 142 ms |
| after flush 1 … 4 (form extended in place) | 168, 158, 150, 133 ms |
| merge published, 84 s after the fourth flush | — |
| **first request after the merge** | **shed at 107.9 s** (the level projected whole: `adopted=false transposed=false` at 107 s after the merge) |
| second request | 161 ms |

Flush publication took 0.5 s each. Open 33 s, of which the mesh level's transpose from the
fold-written column is 17 s. `result.json` is the driver's record; `serve-log-excerpt.txt` the
build lines and the shed.

## What it says

- **The flush arm is closed.** Four flushes, four requests, all under 170 ms, and `visible`
  rising by 1,000 each time: ingested rows count from their flush.
- **The merge arm is open and is the same cost the fix removed elsewhere** — a whole-level
  projection and column composition inside a request, over the 60 s stream deadline at this
  level's size. It fires once per merge, on the first request naming the level. A merge is
  selected whenever `tier_width` same-tier segments accumulate, so under sustained small flushes
  this is every few flushes, not every fold.
- This is the case the owner's ruling reserved for (b), the warm at publication: rebuild on the
  pool when the merge publishes, with the previous form served meanwhile. At 108 s per rebuild
  and a merge every four small flushes, (b) is needed rather than optional wherever ingest is
  continuous.
- Also seen: the `clusters/kmeans` form was rebuilt at the moment the merge published, with no
  request in flight (`serve-log-excerpt.txt`, 23:05:26). Cheap at 256 artifacts and not chased;
  it says something on the merge's publication path builds a form.

## The bundle

A growth or a probe run writes side manifests into the bundle it serves, and a later open
carries forward only the derived extents whose level version still matches — so a bundle a probe
has touched opens without its row column (115 s instead of 33 s, `row_columns_named=0`), on any
binary. `data/ladder/.measure/medcpt36/allin/bundle` is in that state after the 2026-09-03 runs.
This run used `data/ladder/.measure/medcpt36/allin-clean`, a hardlinked copy with
`SEGMENTS-1..3.json` removed; a run dirties the copy in the same way, so it is recreated from the original after each run (`cp -al`, then remove the extra side manifests); it costs no space.

The ingest wire changed on 2026-09-04 (decision 0129): the access column travels as a list and
the external id is the eight-byte entity id. The probe uses the driver's own `encode_batch` so it
does not drift again.
