# test_corpora — the dataset ladder, one directory per rung

**Status:** Working code, never normative. What is here produces the corpora the ingest campaign
measures against; nothing in it decides anything about Tessera. The campaign is
[`../docs/evidence/memos/2026-08-27-ingest-campaign-plan.md`](../docs/evidence/memos/2026-08-27-ingest-campaign-plan.md)
and the survey behind it is
[`../docs/evidence/memos/2026-08-26-dataset-ladder.md`](../docs/evidence/memos/2026-08-26-dataset-ladder.md).

## Nothing here projects

A geographic rung emits `lon` and `lat` in degrees exactly as its publisher wrote them, and its
declaration names `projection = "web_mercator"` with an `extent` written as a longitude/latitude
box. The transform runs inside the build, at the boundary, in the same place for a build and for
an ingest (`../docs/design/projections.md` §3), so the frame, the snap, the clamp count and the
clip count are all the build's report and none of them is this tree's.

Both rungs were built the other way first — projected here, with the declaration stating the frame
the projected numbers lived in — and rebuilt on the declared projection when it landed. The rebuild
was a rerun of a script rather than a lost artifact, a map projection being a pure function, which
is exactly what `data/geometry.parquet` is not and why that file is hashed rather than seeded
(`../probes/dataset.md` §3).

[`common/projection.py`](common/projection.py) stays, as the **second implementation** the engine's
arithmetic is checked against: `tessera_spatial::projection` runs it over 100,000 sampled
coordinates and requires the same stored position, its `TEST_VECTORS` and `TILE_VECTORS` are data in
a file both languages read, and a built corpus is checked by recomputing every point's expected
position through it from the source degrees. Both rungs agree exactly, over 87 million points.

## Layout

| | |
|---|---|
| `<share>/datasets/<name>/<vintage>/` | the publisher's own bytes, read-only — never built or served from |
| `test_corpora/<rung>/` | in git: `prepare.py`, `corpus.toml`, `README.md` |
| `$TESSERA_LADDER/<rung>/` | derived — `points.parquet`, vocabularies, member files, `bundle/`. Default `data/ladder/<rung>` |

**One rung's source is derived rather than staged.** `arxiv` reads `data/` in this checkout, which
`probes/build_corpus.py` and `probes/build_embeddings.py` produced; the share's
`arxiv-tessera/2026-07-27/` is a mirror of that directory and a backup, not a publisher's bytes. It
is the only rung where the acquisition is a rerun rather than a download.

The declaration is in git because it is what gets reviewed and what `tessera check --payloads`
reads. The derived files are not, because they are regenerable and large. Both roots are
environment variables ([`common/paths.py`](common/paths.py)): the ladder's top two rungs do not fit
on this machine's root volume, and when a second one appears it is one value that moves.

## Every deployment of a rung shares the frame its first all-in build recorded

**Owner ruling, 2026-09-03.** A rung's `extent = "auto"` is fitted to the rows the build saw, so a
deployment built from part of the corpus quantises onto a slightly different grid and its cells do
not line up with the whole corpus's. Every later deployment of a rung therefore states the frame
the rung's first all-in build recorded — `MANIFEST.views[].quantisation`, which
[`common/ingest_cycle.py`](common/ingest_cycle.py)'s `state_extent` copies out of that manifest and
writes into the measurement's own copy of the declaration. The rung's committed `corpus.toml` is
never edited; what changes is the copy the measurement builds from, and the run records the frame it
stated.

The same ruling is what makes **100% online ingest** possible: a deployment may start from a bundle
with no points at all, with its frame stated, and take the whole corpus in through
`/control/ingest` (decision 0091 — the build's zero-item refusal is gone). `auto` still cannot be
fitted to no rows, and that refusal names the remedy.

## Two rules inherited from the staging tier

1. **Never build or serve a bundle from the share.** SMB at ~67 MB/s measured 2026-08-27 — a page
   fault is a network round trip, so any residency figure taken there measures the network.
2. **A build figure names its source medium.** Anything read from the share while building is a
   *network-source* figure and is not comparable with the published local-NVMe numbers.

## Two environments, because the two kinds of rung need different things

`~/venvs/ingest` — DuckDB and PyArrow — is the **geographic** rungs'. DuckDB does the row work so
that 10^7-row passes never enter Python; PyArrow is for inspecting what came out. The `spatial`
extension is not installed until rung 2 needs it for Overture's point-in-polygon join.

`~/venvs/projection` — cuVS, cuML and CuPy on the GPU, scikit-learn and SciPy on the CPU, and
optionally toponymy and a CPU torch — is the **embedding** rung's, and it is separate because none
of the geographic rungs want any of it. Its `requirements.txt` sits beside the rung, and it is
named for the projection experiment that rung's two views came out of.

```bash
python3 -m test_corpora.common.projection                        # the transform's own checks
~/venvs/ingest/bin/python     -m test_corpora.geonames.prepare    # one geographic rung
~/venvs/projection/bin/python -m test_corpora.arxiv.prepare       # an embedding rung
~/venvs/projection/bin/python -m test_corpora.medcpt.stage        # the largest one: stage once,
~/venvs/projection/bin/python -m test_corpora.medcpt.prepare      # then build from the local copy
```

**One rung stages.** `medcpt` reads 163 GB of publisher bytes over SMB, which is forty minutes a
pass, so `stage.py` makes exactly one and writes what every later run reads —
`$TESSERA_LADDER/medcpt/staging/`: one parquet per chunk and one flat float16 memmap of
35,920,666 x 768. It is resumable per chunk and refuses a partial matrix rather than reading the
zeros of a sparse file.

## Two directories here are not rungs

They measure nothing about a dataset and are outside the ingest campaign's sizing.

| | |
|---|---|
| [`multiview/`](multiview/README.md) | the fixture [`docs/design/views.md`](../docs/design/views.md) is pointed at — ten row spaces over one entity space, both view-group forms, a shape layer spanning two frames. Synthetic but real-derived: it reads the GeoNames rung's built output and un-projects it |
| `common/` | the transform, the paths and the timer the rungs share |

## Rungs

| Rung | Points | Bundle | State |
|---|---|---|---|
| `medcpt` | 35,920,666 | 11.15 GB | the ladder's largest embedding rung and its first **`dag`** layer — MeSH's 30,217 descriptors with members over 41,321 edges, membership closed upward to **1.66×10⁹ entries**, 3.27× rung 2's spill. Built in 12 m 10 s at 16.03 GB, verified, served; the only rung with a staging step, and the only one to need a layout fitted on a sample and the rest placed against it, 2026-09-02 |
| `arxiv` | 2,422,486 | 1.4 GB | the corpus the artifact catalogue is exercised against, and the ladder's only **two-view** rung — one embedding laid out twice, every layer drawn in both, 2026-09-01 |
| `geonames` | 13,463,857 | 1.34 GB | built and verified on a declared `web_mercator` projection, 2026-08-30 |
| `overture` | 73,631,092 | 12.57 GB | built and verified on the same declared projection, with its division polygons declared in longitude and latitude, 2026-08-30 |
