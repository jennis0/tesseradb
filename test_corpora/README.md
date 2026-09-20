# test_corpora — the dataset ladder, one directory per rung

Each rung's directory produces one of the corpora the ingest campaign measures against
([`docs/ingest-campaign.md`](../docs/ingest-campaign.md)).

## Layout

| | |
|---|---|
| `<share>/datasets/<name>/<vintage>/` | the publisher's own bytes, read-only |
| `test_corpora/<rung>/` | in git: `prepare.py`, `corpus.toml`, `README.md` |
| `$TESSERA_LADDER/<rung>/` | derived: `points.parquet`, vocabularies, member files, `bundle/` |

Both roots are environment variables read by [`common/paths.py`](common/paths.py):
`TESSERA_STAGED` for the share, `TESSERA_LADDER` for the derived directory, default
`data/ladder`.

## Projection

A geographic rung emits `lon` and `lat` in degrees exactly as its publisher wrote them, and its
declaration names `projection = "web_mercator"` with an `extent` in that box. `tessera build`
transforms and quantises the coordinates. [`common/projection.py`](common/projection.py) is a
second implementation of the same transform: `tessera_spatial::projection` checks its own result
against it over sampled coordinates, and a built corpus is checked by recomputing every point's
position through it from the source degrees.

## One frame per rung

A rung's `extent = "auto"` is fitted to the rows a build sees, so a deployment built from part of
the corpus quantises onto a different grid from a deployment built from all of it. Every
deployment of a rung therefore states the frame of the rung's all-in build:
`MANIFEST.views[].quantisation`, which `state_extent` in
[`common/ingest_cycle/`](common/ingest_cycle/) copies out of that build's manifest into the
measurement's own copy of the declaration. The rung's committed `corpus.toml` is never edited.

## Never build or serve from the share

The share is SMB, so a page fault there is a network round trip. Sources are staged from it once;
a build or a served bundle never reads from it. A build figure names its source medium, so a
figure taken while reading from the share is not compared with one taken from local disk.

## Two Python environments

`~/venvs/ingest` carries DuckDB and PyArrow, for a geographic rung's `prepare.py`: DuckDB does the
row work in bulk, and PyArrow inspects what came out.

`~/venvs/projection` carries cuVS, cuML and CuPy on the GPU, and scikit-learn and SciPy on the
CPU, for an embedding rung's `prepare.py` or `stage.py`.

Neither environment carries `requests`. The drivers under [`common/`](common/) (`workload.py`,
`serve_battery.py`, `ingest_cycle/`) need `requests`, `numpy` and `pyarrow`, and run under the
system `python3`, which has all three.

## Rungs

| Rung | Points |
|---|---|
| `arxiv` | 2,422,486 |
| `geonames` | 13,463,857 |
| `medcpt` | 35,920,666 |
| `overture` | 73,631,092 |
| `paperseek` | 102,117,343 |
| `treeoflife` | 233,055,986 |
| `gbif` | 3,495,729,729 |

Two directories here are not rungs: [`multiview/`](multiview/README.md) is a fixture for
[`docs/guides/views.md`](../docs/guides/views.md), built from the GeoNames rung's output rather
than from a publisher's bytes; `common/` is the transform, the paths and the drivers the rungs
share.
