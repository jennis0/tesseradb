#!/usr/bin/env python3
"""Rank a pairs file's terms by pair count, cached beside the file.

The output feeds `measure-principals.mjs --ranks`: composing a principal that sees ~1%, ~10% or
~50% of a corpus needs to know which terms are big, and at 4.8 x 10^4 terms probing each one over
HTTP is minutes of requests for a number the pairs file already holds. Pair count is not
visible-set size — an item carries ~1.7 terms here, so unions overlap — which is why the measurer
treats this only as an ORDERING and measures every composed principal's real visible set against
the running service.

Streamed in record batches, so the 1.7 x 10^9-row file never materialises: peak memory is one
batch plus the per-term counter.

    reference/.venv/bin/python scripts/rank_terms.py data/scaled/pairs/categories-subclass.pairs.parquet

Writes `<pairs>.term-ranks.json` next to the input — [{term, pairs}] descending — and skips the
work when that file is already newer than the input.
"""
import collections
import json
import sys
import time
from pathlib import Path

import pyarrow.parquet as pq


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} <pairs.parquet>")
    pairs = Path(sys.argv[1])
    out = pairs.with_suffix(pairs.suffix + ".term-ranks.json")
    if out.exists() and out.stat().st_mtime > pairs.stat().st_mtime:
        print(f"up to date: {out}")
        return

    reader = pq.ParquetFile(pairs)
    term_column = next(
        (name for name in reader.schema_arrow.names if "term" in name.lower()),
        reader.schema_arrow.names[-1],
    )
    counts: collections.Counter = collections.Counter()
    rows = 0
    started = time.monotonic()
    for batch in reader.iter_batches(columns=[term_column], batch_size=1 << 22):
        values = batch.column(0)
        counted = values.value_counts()
        ids = counted.field("values").to_pylist()
        ns = counted.field("counts").to_pylist()
        for i, n in zip(ids, ns):
            counts[i] += n
        rows += len(batch)

    ranked = [{"term": str(term), "pairs": n} for term, n in counts.most_common()]
    out.write_text(json.dumps(ranked) + "\n")
    print(
        f"ranked {len(ranked)} terms over {rows:,} pairs in {time.monotonic() - started:.0f}s; "
        f"wrote {out}"
    )


if __name__ == "__main__":
    main()
