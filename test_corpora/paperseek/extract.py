"""The one pass over OpenAlex `works`, and the PaperSeek id set it is filtered against.

**What this produces.** `staging/openalex-extract.parquet`: one row per PaperSeek work that
OpenAlex still carries, with the six columns `openalex.py` serves — `id`, `publication_year`,
`type`, `is_oa`, `licence`, `topic_id`. Everything downstream reads that file and never the share
again.

**The id set is integers, not strings.** Both sides spell the id as the full URL
`https://openalex.org/W1526114719` (`probes/2026-09-02-rung-4-share-reads/` §3), and the part after
the `W` is a number. So membership is exact on a `uint64` — no hashing, no collisions to argue
about — and 102 million of them are an 817 MB array that `np.searchsorted` probes in one call per
part. A Python set of 10^8 strings is the thing this exists not to be: it would be ~10 GB and a
per-row `in`.

**The projection is nested, and that is the whole cost model.** The probe measured a projected scan
of the six *top-level* fields at ~10 MB/s aggregate and modelled 54 minutes over the 2,428 parts,
because `primary_topic`, `open_access` and `best_oa_location` are struct-typed and reading the
struct reads every leaf under it — `best_oa_location.source` alone carries two lists and eleven
scalars nobody here wants. Parquet stores leaves, and pyarrow projects them by dotted path, so this
reads `primary_topic.id`, `open_access.is_oa` and `best_oa_location.license` instead of their
parents. Measured on one part (`updated_date=2025-11-06/part_0782`, 291,242 rows) that is
**3.65 MB against 16.38 MB compressed, 4.5x fewer bytes**; the scan's own measured wall and rate are
in `README-openalex.md`. This is a cheaper read of the same columns, not a different projection.

**Resumable per part**, because the scan is tens of minutes and the share drops: each part's matched
rows go to their own shard under `staging/openalex-parts/`, and a JSONL ledger records the parts
that finished. A rerun skips them. `--combine` then concatenates the shards, sorts by id, and writes
the single extract — sorted, because `OpenAlex.resolve` probes it with `searchsorted` and a sorted
file is what makes that legal.

    python -m test_corpora.paperseek.extract --ids       # the id set, from the 53 chunks
    python -m test_corpora.paperseek.extract --scan      # the one scan of `works`, resumable
    python -m test_corpora.paperseek.extract --combine   # shards -> openalex-extract.parquet
    python -m test_corpora.paperseek.extract --licence-sample 3   # the multi-licence side check

**The licence is `best_oa_location.license`.** The dataset README names a top-level `license`
column; there is none, on this vintage or any other partition sampled. Corrected in the README on
the share on 2026-09-02 and in the probe before that.
"""

from __future__ import annotations

import argparse
import glob
import json
import re
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from ..common.paths import ladder, staged

VINTAGE = "2026-08-27"

#: The 53 PaperSeek chunks: `id`, `title`, `abstract`, `embedding`. Only `id` is read here.
PAPERSEEK = "paperseek-openalex"

#: OpenAlex `works`, Hive-partitioned by `updated_date` across 2,428 parts.
OPENALEX = "openalex"

#: `https://openalex.org/W…` — the prefix both sides carry, and the length of it.
PREFIX = "https://openalex.org/W"

#: The leaves this scan reads. Dotted paths, not the parent structs: see the module docstring.
PROJECT = [
    "id",
    "publication_year",
    "type",
    "primary_topic.id",
    "open_access.is_oa",
    "best_oa_location.license",
]

EXTRACT_SCHEMA = pa.schema(
    [
        # The `W…` number, not the URL. The id is a number spelled as a URL on the share; storing
        # it as one saves 3.4 GB against the string form and is what `resolve` probes.
        pa.field("id", pa.uint64()),
        pa.field("publication_year", pa.int32()),
        pa.field("type", pa.string()),
        pa.field("is_oa", pa.bool_()),
        pa.field("licence", pa.string()),
        pa.field("topic_id", pa.string()),
    ]
)


def rung() -> Path:
    return ladder("paperseek")


def staging() -> Path:
    path = rung() / "staging"
    path.mkdir(parents=True, exist_ok=True)
    return path


# --------------------------------------------------------------------------------- the id set


def chunks() -> list[Path]:
    """The 53 PaperSeek chunks in **ascending numeric** order, which is entity order.

    The numbering has gaps (`chunk_3` and `chunk_45` are absent) and runs past nine, so a
    lexical sort would put `chunk_10` second and shift every entity id after it. The interface
    fixes this order; it is asserted here rather than assumed.
    """
    base = staged(PAPERSEEK, VINTAGE)
    found = []
    for path in base.glob("chunk_*.parquet"):
        m = re.fullmatch(r"chunk_(\d+)", path.stem)
        if m:
            found.append((int(m.group(1)), path))
    return [path for _, path in sorted(found)]


def keys(ids: pa.Array) -> np.ndarray:
    """`https://openalex.org/W123` → `123`, as `uint64`, vectorised.

    An id not of that shape is given key 0, which no work carries, so it matches nothing and is
    counted by the caller rather than refusing the scan — a malformed id on either side is a row
    lost from a demo corpus, not a disclosure (CLAUDE.md, *what the strictness is for*).
    """
    if isinstance(ids, pa.ChunkedArray):
        ids = ids.combine_chunks()
    ok = pc.fill_null(pc.starts_with(ids, pattern=PREFIX), False)
    tail = pc.utf8_slice_codeunits(pc.fill_null(ids, PREFIX + "0"), len(PREFIX))
    # Digits-only, checked before the cast so a stray suffix is counted rather than raised on.
    digits = pc.fill_null(pc.utf8_is_digit(tail), False)
    good = pc.and_(ok, digits)
    out = np.zeros(len(ids), np.uint64)
    picked = np.asarray(pc.cast(tail.filter(good), pa.uint64()))
    out[np.asarray(good)] = picked
    return out


def build_ids() -> dict:
    """The 53 chunks' ids, in entity order, plus the sorted-unique array the scan probes."""
    t0 = time.time()
    parts, counts = [], []
    for path in chunks():
        column = pq.ParquetFile(path).read(columns=["id"])["id"]
        k = keys(column)
        bad = int((k == 0).sum())
        assert bad == 0, f"{path.name}: {bad} ids are not {PREFIX}<digits>"
        parts.append(k)
        counts.append(len(k))
        print(f"{path.name}: {len(k):,} ids ({time.time() - t0:.0f}s)", flush=True)

    ids = np.concatenate(parts)
    np.save(staging() / "paperseek-ids.npy", ids)
    sorted_ids = np.unique(ids)
    np.save(staging() / "paperseek-ids-sorted.npy", sorted_ids)

    summary = {
        "chunks": len(counts),
        "rows": int(len(ids)),
        "distinct": int(len(sorted_ids)),
        "chunk_rows": counts,
        "chunk_files": [p.name for p in chunks()],
        "seconds": round(time.time() - t0, 1),
    }
    (staging() / "paperseek-ids.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({k: v for k, v in summary.items() if k not in ("chunk_rows", "chunk_files")}))
    return summary


# ------------------------------------------------------------------------------------ the scan


def parts() -> list[str]:
    base = staged(OPENALEX, VINTAGE) / "parquet" / "works"
    return sorted(glob.glob(f"{base}/*/part_*.parquet"))


def projected_bytes(pf: pq.ParquetFile) -> int:
    """The compressed bytes of the projected leaves, from the footer — what the scan moves."""
    total = 0
    for i in range(pf.metadata.num_row_groups):
        rg = pf.metadata.row_group(i)
        for c in range(rg.num_columns):
            col = rg.column(c)
            if col.path_in_schema.replace(".list.element", "") in PROJECT:
                total += col.total_compressed_size
    return total


def _flat(table: pa.Table, field: str, child: str) -> pa.Array:
    """One leaf out of a projected struct. pyarrow returns `primary_topic.id` as a struct with
    one child rather than as a flat column, so the child is taken back out here."""
    return pc.struct_field(table[field].combine_chunks(), [child])


def scan(limit: int | None = None) -> dict:
    """One pass over `works`, keeping the rows whose id is in the PaperSeek set.

    Resumable: a part whose shard is recorded in the ledger is skipped. Shards are per part and
    written only when the part matched something — most parts match tens of thousands of rows,
    and an empty file per empty part is 2,428 inodes for nothing.
    """
    wanted = np.load(staging() / "paperseek-ids-sorted.npy")
    shards = staging() / "openalex-parts"
    shards.mkdir(exist_ok=True)
    ledger_path = shards / "ledger.jsonl"

    done = set()
    if ledger_path.exists():
        for line in ledger_path.read_text().splitlines():
            if line.strip():
                done.add(json.loads(line)["part"])

    todo = parts()
    if limit is not None:
        todo = todo[:limit]
    base = str(staged(OPENALEX, VINTAGE) / "parquet" / "works") + "/"

    t0 = time.time()
    scanned = matched = read_bytes = rows_seen = 0
    with open(ledger_path, "a") as ledger:
        for i, path in enumerate(todo):
            name = path[len(base) :]
            if name in done:
                continue
            part_t0 = time.time()
            pf = pq.ParquetFile(path)
            table = pf.read(columns=PROJECT, use_threads=True)
            nbytes = projected_bytes(pf)

            key = keys(table["id"])
            pos = np.searchsorted(wanted, key)
            np.clip(pos, 0, len(wanted) - 1, out=pos)
            hit = wanted[pos] == key
            n = int(hit.sum())

            if n:
                mask = pa.array(hit)
                licence = pc.utf8_lower(
                    pc.utf8_trim_whitespace(_flat(table, "best_oa_location", "license"))
                )
                shard = pa.table(
                    {
                        "id": pa.array(key[hit], pa.uint64()),
                        "publication_year": pc.cast(
                            table["publication_year"].combine_chunks().filter(mask), pa.int32()
                        ),
                        "type": table["type"].combine_chunks().filter(mask),
                        "is_oa": _flat(table, "open_access", "is_oa").filter(mask),
                        "licence": licence.filter(mask),
                        "topic_id": pc.utf8_slice_codeunits(
                            _flat(table, "primary_topic", "id"), len("https://openalex.org/")
                        ).filter(mask),
                    },
                    schema=EXTRACT_SCHEMA,
                )
                pq.write_table(
                    shard,
                    shards / (name.replace("/", "__") + ".shard"),
                    compression="zstd",
                    use_dictionary=["type", "licence", "topic_id"],
                )

            record = {
                "part": name,
                "rows": table.num_rows,
                "matched": n,
                "bytes": nbytes,
                "seconds": round(time.time() - part_t0, 3),
            }
            ledger.write(json.dumps(record) + "\n")
            ledger.flush()
            scanned += 1
            matched += n
            read_bytes += nbytes
            rows_seen += table.num_rows
            if scanned % 25 == 0 or i == len(todo) - 1:
                wall = time.time() - t0
                print(
                    f"{scanned}/{len(todo) - len(done)} parts, {matched:,} matched, "
                    f"{read_bytes / 1e6:.0f} MB in {wall:.0f}s "
                    f"({read_bytes / 1e6 / max(wall, 1e-9):.1f} MB/s)",
                    flush=True,
                )

    wall = time.time() - t0
    return {
        "parts_scanned": scanned,
        "parts_skipped": len(done),
        "rows_seen": rows_seen,
        "matched": matched,
        "bytes": read_bytes,
        "seconds": round(wall, 1),
        "MBps": round(read_bytes / 1e6 / max(wall, 1e-9), 2),
    }


# --------------------------------------------------------------------------------- the combine


def combine() -> dict:
    """Shards → one extract, **sorted by id**, dictionary-encoded on the three low-cardinality
    columns. The sort is what `OpenAlex.resolve` depends on; the file's own order is otherwise
    the accident of which `updated_date` partition a work last moved in."""
    t0 = time.time()
    shards = sorted((staging() / "openalex-parts").glob("*.shard"))
    # Held once, sorted once. `concat_tables` is zero-copy over the shards' own buffers and
    # `take` materialises the sorted copy, so the peak is two copies of the extract and not four
    # — which is why there is no `combine_chunks` before the sort.
    table = pa.concat_tables(pq.read_table(s, schema=EXTRACT_SCHEMA) for s in shards)
    order = pa.array(np.argsort(np.asarray(table["id"].combine_chunks()), kind="stable"))
    table = table.take(order)
    del order

    out = staging() / "openalex-extract.parquet"
    pq.write_table(
        table,
        out,
        compression="zstd",
        use_dictionary=["type", "licence", "topic_id"],
        row_group_size=2_000_000,
    )

    ids = np.asarray(table["id"])
    licence = table["licence"].combine_chunks()
    counts = pc.value_counts(licence)
    dist = sorted(
        ((v["values"].as_py(), v["counts"].as_py()) for v in counts), key=lambda kv: -kv[1]
    )
    summary = {
        "shards": len(shards),
        "rows": table.num_rows,
        "distinct_ids": int(len(np.unique(ids))),
        "bytes_on_disk": out.stat().st_size,
        "licence_distribution": dist,
        "licensed_rows": int(table.num_rows - licence.null_count),
        "with_topic": int(table.num_rows - table["topic_id"].null_count),
        "with_year": int(table.num_rows - table["publication_year"].null_count),
        "is_oa_true": int(pc.sum(pc.fill_null(table["is_oa"], False)).as_py() or 0),
        "seconds": round(time.time() - t0, 1),
    }
    (staging() / "openalex-extract.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return summary


# ------------------------------------------------------- the side check: one work, one licence?

def licence_sample(n_parts: int) -> dict:
    """How often a work's `locations[]` carry more than one distinct licence.

    The extract takes `best_oa_location.license`, which is one string per work by construction, so
    the multi-licence question cannot arise *in the extract* — but it can in the data, and a
    compartment built on a single-valued label wants to know how much it is throwing away. This
    reads the (much more expensive) `locations.license` list on a few parts and reports the share
    of works whose locations disagree. A sample, and said to be one.
    """
    todo = parts()
    step = max(len(todo) // max(n_parts, 1), 1)
    chosen = todo[:: step][:n_parts]
    works = multi = with_any = 0
    pairs: dict[str, int] = {}
    for path in chosen:
        # `locations.license` is silently dropped by pyarrow — a leaf under a list needs its
        # full parquet path, and asking for the short form returns the *other* column with no
        # error at all. Measured here rather than assumed: the short spelling read a table with
        # one column in it.
        table = pq.ParquetFile(path).read(
            columns=["locations.list.element.license", "best_oa_location.license"],
            use_threads=True,
        )
        best = pc.struct_field(table["best_oa_location"].combine_chunks(), ["license"])
        for row, chosen_licence in zip(
            table["locations"].combine_chunks().to_pylist(), best.to_pylist()
        ):
            works += 1
            seen = {entry["license"] for entry in (row or []) if entry["license"]}
            if seen:
                with_any += 1
            if len(seen) > 1:
                multi += 1
                key = ",".join(sorted(seen)) + f" -> {chosen_licence}"
                pairs[key] = pairs.get(key, 0) + 1
    return {
        "parts": [str(Path(p).relative_to(Path(p).parents[2])) for p in chosen],
        "works": works,
        "works_with_a_location_licence": with_any,
        "works_with_several": multi,
        "share_of_licensed": round(multi / max(with_any, 1), 4),
        "top_combinations": sorted(pairs.items(), key=lambda kv: -kv[1])[:10],
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ids", action="store_true", help="build the PaperSeek id set")
    ap.add_argument("--scan", action="store_true", help="the one scan of `works`, resumable")
    ap.add_argument("--combine", action="store_true", help="shards -> the extract")
    ap.add_argument("--limit", type=int, help="scan only the first N parts (a smoke)")
    ap.add_argument("--licence-sample", type=int, metavar="PARTS", help="the side check")
    args = ap.parse_args()

    if args.licence_sample:
        print(json.dumps(licence_sample(args.licence_sample), indent=2))
        return 0
    every = not (args.ids or args.scan or args.combine)
    if args.ids or every:
        build_ids()
    if args.scan or every:
        print(json.dumps(scan(args.limit), indent=2))
    if args.combine or every:
        combine()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
