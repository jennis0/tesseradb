# Measurement tooling for the 2026-09-10 disk survey

**Status:** Probe tooling, 2026-09-10. Not normative. These are the scripts eight parallel
fact-finding agents used to take the figures their memos mark as measured in their own session —
bundle decompositions, encoder ratios, and the per-file classifications behind them. They were
written to answer one question each and are kept so a figure can be re-run rather than trusted.

The memos are [`docs/evidence/memos/2026-09-10-disk-*.md`](../../docs/evidence/memos/); the
campaign's starting point is
[`2026-09-10-build-disk-weight.md`](../../docs/evidence/memos/2026-09-10-build-disk-weight.md) and
the build measurements it rests on are [`../2026-09-10-build-disk/`](../2026-09-10-build-disk/).

⊘ **Nothing here was run against rung 6**, and several scripts measure an encoder standing alone
(zstd at the blob's block size, pyroaring against croaring's portable format, a pyarrow proxy for
`pairs.parquet`) rather than the shipped writer. Each memo says which of its figures came from a
proxy. Bundles under `data/` at a stale `bundle_format` were measured for their shape; today's
`open_bundle` would refuse them.

`measure.sh` walks a bundle root and emits allocated blocks per file; `classify.py` groups those by
file kind; `decomp.py`/`final.py` produce the per-corpus decompositions (`decomp.txt`, `final.txt`).
The rest each answer one term: `blob*.py` the record blob's compression, `tsmb.py` membership
extents, `post*.py` and `terms.py` the postings, `dict.py` the front-coded dictionary, `geom.py` and
`prec.py` the geometry, `buckets.py` and `cols.py` the build's scratch, `framing.py` the record
framing.
