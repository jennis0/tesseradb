"""A prefix of the GBIF ladder corpus: the first K row groups of both source files.

points.parquet and members-taxonomy.parquet are row-aligned and their entity_id ascends from
zero, so K row groups of each is a corpus in its own right — every member names a point the
slice holds. A prefix and not a spread: what is being measured is bytes an item, and the
publisher's export order changes which values a slice holds rather than how many bytes they are.

A whole-corpus `--limit` will not do instead. It is a prefix of the *points* file alone, and a
member row past the limit names an entity the build did not assign, which the layer publication
refuses (`tessera build --help`).

Re-encoded rather than byte-copied, pyarrow having no row-group copy.

  slice.py <row groups> <output directory> [source corpus]
"""
import shutil, sys
from pathlib import Path
import pyarrow.parquet as pq

groups = int(sys.argv[1])
out = Path(sys.argv[2])
src = Path(sys.argv[3] if len(sys.argv) > 3 else "data/ladder/gbif")
out.mkdir(parents=True, exist_ok=True)

for name in ["points.parquet", "members-taxonomy.parquet"]:
    reader = pq.ParquetFile(src / name)
    writer = pq.ParquetWriter(out / name, reader.schema_arrow, compression="zstd")
    rows = 0
    for g in range(groups):
        table = reader.read_row_group(g)
        rows += table.num_rows
        writer.write_table(table)
    writer.close()
    print(f"{name}: {rows:,} rows in {groups} row groups")

for name in ["corpus.toml", "tessera.toml", ".env", "vocab-kingdom.parquet",
             "country-terms.txt", "country-ranks.json"]:
    shutil.copy(src / name, out / name)
print(f"wrote {out}")
