# The same probe after the row form is brought forward — rung 3 (MedCPT, 36M points)

**Status:** measurement, 2026-09-03. `growth_trigger.py` one directory up, run against the branch
that applies a write's delta to the held row form instead of letting the next request project the
level again (`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`).

## The runs

Both binaries on the same host inside one hour, each against **its own copy** of the all-in bundle
— see *the bundle is not reusable* below. `TESSERA_BUNDLE` was added to the driver to point it at a
copy; nothing else about it changed.

| | binary | open | growth returns | request after the growth |
|---|---|---|---|---|
| `run-main-clean` | 882e46cb | 28.1 s | 0.01 s | **shed at 134.3 s** |
| `run-branch-timed` | 882e46cb (a second baseline) | 27.1 s | 0.23 s | **shed at 100.7 s** |
| `run-branch-instr` | branch, before the row form was made cheap to copy | 27.5 s | 2.99 s | served, 264 ms |
| `run-branch-final` | branch | 24.6 s | 0.04 s | served, **125 ms** |

`run-branch-timed` is labelled as a baseline because it is one: it was launched believing it held
the branch binary and did not — `target/release/tessera` was the base commit's at that moment. It
is kept because two independent baselines agreeing that the request is shed is worth more than one.

## What the server said

`run-branch-instr` is why the last row is the last row. It carries the amendment's own log line:

```
a level's held row form took a write's delta layer=mesh/descriptors level=0 view=knn
  rows_added=0 cloned=true cloned_ms=2576 elapsed_ms=2581
```

The amendment itself is 5 ms. The other 2 576 is `Arc::make_mut` copying the form because a request
was still reading it — 1.66×10⁹ membership entries. With one `Arc` per artifact's bitmap the same
copy is 30,217 pointers, and `run-branch-final` says so:

```
  rows_added=0 cloned=true cloned_ms=9 elapsed_ms=15
```

`rows_added=0` is correct and worth stating: the probe's growth names an entity whose row is still
in the commit buffer, so it joins the membership and labels no row yet. What the run measures is
the **cost** of a growth into a large level, not the arithmetic of one.

## The bundle is not reusable

A growth moves the level's version. The side manifest the next tick writes carries forward only the
derived extents whose version still matches (`Executor::artifact_coordinates`), and the fold-written
row column is not one of them — correctly, under I11. So the *second* run against a given bundle
opens with `row_columns_named=0`, projects `mesh/descriptors` whole instead of transposing it, and
takes about 147 s to open, **on either binary**.

`data/ladder/.measure/medcpt36/allin/bundle` had already been through prior runs when this campaign
started: its latest side manifest names no row column and records `mesh/descriptors` at version 3,
while the column file on disk is version 1. Every run above is therefore against a hardlinked copy
with the extra `SEGMENTS-*.json` removed, which is the state the parent directory's own
`result.json` was taken at. The original was not written to and is unchanged.
