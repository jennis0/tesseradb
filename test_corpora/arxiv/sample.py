"""A uniform random sample of a notebook corpus directory, as a directory of the same shape.

Every file of the source is written again restricted to the sampled papers: the points and every
members table are filtered to them, and the artifact tables, the label tables and the two
vocabularies are copied whole, so one notebook reads either directory with the same code.

One exception: a label with a content whose generating papers were all left out of the sample is
dropped with its member rows. That content would have an empty generating set, which the build
refuses.

```bash
python -m test_corpora.arxiv.sample --source data/notebook-2m4-live --out data/notebook-sample \
    --papers 50000 --seed 0
```
"""

from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq


def sampled_ids(points: Path, papers: int, seed: int) -> pa.Array:
    ids = pq.read_table(points, columns=["entity_id"]).column("entity_id").combine_chunks()
    chosen = np.random.default_rng(seed).choice(len(ids), size=papers, replace=False)
    return ids.take(pa.array(np.sort(chosen)))


def filter_file(source: Path, out: Path, column: str, keep: pa.Array) -> None:
    """Write `source` to `out` keeping the rows whose `column` is in `keep`, one batch at a time."""
    reader = pq.ParquetFile(source)
    with pq.ParquetWriter(out, reader.schema_arrow) as writer:
        for batch in reader.iter_batches(batch_size=200_000):
            writer.write_batch(batch.filter(pc.is_in(batch.column(column), value_set=keep)))


def drop_labels_with_no_generating_set(artifacts: Path, members: Path, source: Path) -> None:
    """Drop the rows of `artifacts`, and their members, whose generating set for some content rank
    is in the unfiltered `source` members file and not in the filtered `members` file."""
    def ranked(path):
        table = pq.read_table(path, columns=["key", "rank"]).to_pandas().dropna()
        return set(zip(table["key"], table["rank"]))

    emptied = {key for key, _ in ranked(source) - ranked(members)}
    table = pq.read_table(artifacts)
    kept = table.filter(pc.invert(pc.is_in(table["key"], pa.array(sorted(emptied), pa.string()))))
    pq.write_table(kept, artifacts)
    held = pq.read_table(members)
    pq.write_table(held.filter(pc.is_in(held["key"], kept["key"])), members)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--source", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--papers", type=int, default=50_000)
    ap.add_argument("--seed", type=int, default=0)
    args = ap.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    keep = sampled_ids(args.source / "points.parquet", args.papers, args.seed)

    for path in sorted(args.source.glob("*.parquet")):
        target = args.out / path.name
        if path.name == "points.parquet":
            filter_file(path, target, "entity_id", keep)
        elif path.name.endswith("-members.parquet"):
            filter_file(path, target, "entity", keep)
        else:
            shutil.copyfile(path, target)
    for members in sorted(args.out.glob("*-members.parquet")):
        artifacts = members.with_name(members.name.removesuffix("-members.parquet") + ".parquet")
        drop_labels_with_no_generating_set(artifacts, members, args.source / members.name)
    shutil.copyfile(args.source / "schema.toml", args.out / "schema.toml")

    files = {}
    for path in sorted(args.out.glob("*.parquet")):
        files[path.name] = pq.ParquetFile(path).metadata.num_rows
        print(f"{path.name}: {files[path.name]:,} rows")

    manifest = {
        "sampled_from": str(args.source.resolve()),
        "papers": args.papers,
        "seed": args.seed,
        "rows": files,
    }
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
