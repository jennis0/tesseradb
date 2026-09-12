"""The taxonomy membership rows a `--limit N` build assigned entities for.

`--limit N` is a prefix of `points.parquet` alone. A membership row naming an entity past the
limit names an entity the build did not assign, and the layer publication refuses the batch
(`tessera build --help`), so a limited build needs a limited membership file, given to it with
`--file taxonomy=<path>`.

`entity` ascends across `members-taxonomy.parquet`, checked here against the row-group
statistics, so the rows wanted are a row-group prefix with the straddling group filtered.

  limit_members.py <members.parquet> <limit> <out.parquet>
"""

import sys
from pathlib import Path

import pyarrow.compute as pc
import pyarrow.parquet as pq


def main():
    src, limit, out = Path(sys.argv[1]), int(sys.argv[2]), Path(sys.argv[3])
    reader = pq.ParquetFile(src)
    meta = reader.metadata
    prev = -1
    for g in range(meta.num_row_groups):
        stats = meta.row_group(g).column(0).statistics
        if stats is None or stats.min < prev:
            sys.exit("entity does not ascend across row groups; the prefix argument fails")
        prev = stats.max

    writer = pq.ParquetWriter(out, reader.schema_arrow, compression="zstd")
    rows = 0
    for g in range(meta.num_row_groups):
        stats = meta.row_group(g).column(0).statistics
        if stats.min >= limit:
            break
        table = reader.read_row_group(g)
        if stats.max >= limit:
            table = table.filter(pc.less(table.column("entity"), limit))
        rows += table.num_rows
        writer.write_table(table)
    writer.close()
    print(f"{out.name}: {rows:,} membership rows under entity {limit:,}")


if __name__ == "__main__":
    main()
