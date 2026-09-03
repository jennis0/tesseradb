"""The share of a view an artifact's **middle 90%** of members occupies, per layer and per view.

The measurement that withdrew two taxonomies at rungs 1 and 2 and kept rungs 3 and 4's: the box
between the 5th and 95th percentile of an artifact's members in x and in y, as a fraction of the
whole map's box. **A layer earns its place by drawing something in the view it is declared over**,
and this is how that is decided rather than argued.

    ~/venvs/projection/bin/python -m test_corpora.treeoflife.spread

Two things are this rung's own and rung 4's copy did neither.

- **Both views.** `taxonomy/tree` and `publishers/source` are declared over `bioclip` and `geo`, so
  each is measured twice: a clade compact in the embedding may be spread over three continents, and
  the interesting figure is whether the same artifact draws in both geometries. `geo` holds only
  the 75.9% of rows the GBIF join placed, so an artifact's box there is over its *placed* members
  and its member count differs.
- **A list-keyed member file, with no artifact roster.** `taxonomy/tree` is `value_set = "open"`, so
  the artifacts are exactly what the member file names and there is no `taxonomy-tree.parquet` to
  read them from. The keys are drawn from the member file itself in one pass, per level — the list's
  positions *are* the levels — and a uniform sample of them is measured.

`publishers/source` is a predicate layer with no member file at all: its membership is the indexed
`publisher` column, so it is measured straight off `points.parquet`.

It reads the build's inputs rather than a bundle, so it can be run before or after a build and
needs no server.

⊘ **This is not the build's `everywhere` fraction.** A box covering 1% of a view can still be wider
than the tile-index node at the depth a level is served from, so an artifact may be compact here
and served as a list there. Rung 3 was the first corpus to separate the two and rung 4 reported
both; they measure different things and this rung reports both again.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

from ..common.paths import ladder
from . import sources

#: Artifacts sampled per level on a layer too large to measure exactly.
SAMPLE_PER_LEVEL = 150

#: An artifact with fewer members than this has no middle 90% worth quoting.
MIN_MEMBERS = 10

#: Rows the key survey reads before it stops. The member file is 2.33x10^8 rows and a level's keys
#: recur constantly, so a prefix names nearly every artifact; the sample is over what it names and
#: the report says how much of the file it saw.
SURVEY_ROWS = 20_000_000


def positions(out: Path) -> dict[str, tuple[np.ndarray, np.ndarray, float, np.ndarray]]:
    """`view -> (x, y, box area, entity ids)`, read off the two points files.

    `bioclip` holds every row in entity order, so its entity ids are implicit; `geo` holds a subset
    and carries its own, which is what makes an artifact's box on that view a box over its *placed*
    members alone.
    """
    got = {}
    points = pq.read_table(out / "points.parquet", columns=["x", "y"])
    x = np.asarray(points.column("x"), dtype=np.float64)
    y = np.asarray(points.column("y"), dtype=np.float64)
    del points
    got["bioclip"] = (x, y, (x.max() - x.min()) * (y.max() - y.min()),
                      np.arange(len(x), dtype=np.int64))

    geo_path = out / "points-geo.parquet"
    if geo_path.exists():
        geo = pq.read_table(geo_path, columns=["entity_id", "lon", "lat"])
        gx = np.asarray(geo.column("lon"), dtype=np.float64)
        gy = np.asarray(geo.column("lat"), dtype=np.float64)
        ge = np.asarray(geo.column("entity_id"), dtype=np.int64)
        del geo
        got["geo"] = (gx, gy, (gx.max() - gx.min()) * (gy.max() - gy.min()), ge)
    return got


def survey_keys(path: Path, levels: int, rng, sample: int, rows_cap: int) -> tuple[dict, int, int]:
    """`{key: level}` for a sample of the list-keyed member file's artifacts, and what it saw.

    One pass over the file's leading `rows_cap` rows, taking each level's distinct keys off the
    list column's position `j`. A taxonomy's coarse levels are named in the first row group and its
    species in the last, so the sample is a sample of what the prefix named and the count of keys
    seen is reported beside it.
    """
    reader = pq.ParquetFile(path)
    seen: list[set] = [set() for _ in range(levels)]
    rows = 0
    for group in range(reader.metadata.num_row_groups):
        batch = reader.read_row_group(group, columns=["key"])
        listed = batch.column("key").combine_chunks()
        # The child array and the list's own offsets, rather than `flatten()`: position j of a row
        # is `offsets[row] + j` into the child, and a null entry must keep its slot.
        flat = listed.values
        offsets = np.asarray(listed.offsets, dtype=np.int64)
        width = offsets[1] - offsets[0]
        assert width == levels, f"a member row carries {width} entries against {levels} levels"
        for j in range(levels):
            at = offsets[:-1] + j
            seen[j].update(v for v in flat.take(pa.array(at)).to_pylist() if v is not None)
        rows += batch.num_rows
        del batch, listed, flat
        if rows >= rows_cap:
            break

    wanted: dict[str, int] = {}
    for j, keys in enumerate(seen):
        got = sorted(keys)
        if len(got) > sample:
            got = [got[i] for i in rng.choice(len(got), sample, replace=False)]
        for key in got:
            wanted[key] = j
    return wanted, rows, sum(len(s) for s in seen)


def taxonomy_spread(out: Path, views: dict, sample: int, seed: int, rows_cap: int) -> dict:
    """`taxonomy/tree`'s table, per view. One pass over the member file per run, both views scored
    off the same held member sets."""
    path = out / "members-taxonomy.parquet"
    rng = np.random.default_rng(seed)
    levels = len(sources.RANKS)
    wanted, surveyed, keys_seen = survey_keys(path, levels, rng, sample, rows_cap)
    print(f"taxonomy/tree: {len(wanted)} keys sampled from the {keys_seen:,} named in the first "
          f"{surveyed:,} member rows", flush=True)

    held: dict[str, list[np.ndarray]] = {k: [] for k in wanted}
    value_set = pa.array(list(wanted), pa.string())
    reader = pq.ParquetFile(path)
    rows = 0
    for group in range(reader.metadata.num_row_groups):
        batch = reader.read_row_group(group, columns=["entity", "key"])
        rows += batch.num_rows
        listed = batch.column("key").combine_chunks()
        entity = np.asarray(batch.column("entity"), dtype=np.int64)
        offsets = np.asarray(listed.offsets, dtype=np.int64)
        flat = listed.values
        for j in range(levels):
            at = offsets[:-1] + j
            column = flat.take(pa.array(at))
            hit = np.asarray(pc.fill_null(pc.is_in(column, value_set=value_set), False))
            if not hit.any():
                continue
            key = np.asarray(column.take(pa.array(np.flatnonzero(hit))))
            rows_here = entity[hit]
            uniq, inverse = np.unique(key, return_inverse=True)
            for i, k in enumerate(uniq):
                held[str(k)].append(rows_here[inverse == i])
            del key, rows_here
        del batch, listed, flat, entity
    return {"member_rows_scanned": rows,
            "views": {name: score(wanted, held, *views[name]) for name in views}}


def score(wanted: dict, held: dict, x, y, area, entities) -> dict:
    """One view's table over the member sets already gathered.

    `entities` maps a position in `x`/`y` to an entity id; on a view holding a subset of the corpus
    the members it does not hold are dropped, which is what makes the figure a box over the members
    that view actually draws.
    """
    of_entity = None
    if len(entities) and (len(entities) != len(x) or entities[-1] != len(entities) - 1):
        of_entity = np.full(int(entities.max()) + 1, -1, dtype=np.int64)
        of_entity[entities] = np.arange(len(entities))

    measured = []
    for key, level in wanted.items():
        if not held[key]:
            continue
        e = np.concatenate(held[key])
        if of_entity is not None:
            e = e[e < len(of_entity)]
            e = of_entity[e]
            e = e[e >= 0]
        if len(e) < MIN_MEMBERS:
            continue
        px = np.percentile(x[e], [5, 95])
        py = np.percentile(y[e], [5, 95])
        measured.append({"key": key, "level": level, "members": int(len(e)),
                         "share": float((px[1] - px[0]) * (py[1] - py[0]) / area)})

    shares = np.array([r["share"] for r in measured]) if measured else np.zeros(0)
    return {
        "artifacts_measured": len(measured),
        "min_members": MIN_MEMBERS,
        "median_share": float(np.median(shares)) if len(shares) else None,
        "p90_share": float(np.percentile(shares, 90)) if len(shares) else None,
        "max_share": float(shares.max()) if len(shares) else None,
        "under_5pc": float((shares < 0.05).mean()) if len(shares) else None,
        "members": {
            "min": int(min(r["members"] for r in measured)) if measured else None,
            "median": int(np.median([r["members"] for r in measured])) if measured else None,
            "max": int(max(r["members"] for r in measured)) if measured else None,
        },
        "per_level": {
            str(level): {
                "artifacts": sum(1 for r in measured if r["level"] == level),
                "median_share": float(
                    np.median([r["share"] for r in measured if r["level"] == level])
                ),
            }
            for level in sorted({r["level"] for r in measured})
        },
    }


def predicate_spread(out: Path, views: dict, column: str) -> dict:
    """A predicate layer's table: its artifacts are the distinct values of an indexed column, so
    its membership is that column and there is no member file to read."""
    values = pq.read_table(out / "points.parquet", columns=[column]).column(column).combine_chunks()
    codes = pc.dictionary_encode(values)
    keys = np.asarray(codes.dictionary.cast(pa.string()))
    index = np.asarray(pc.fill_null(codes.indices, -1), dtype=np.int64)
    del values, codes
    order = np.argsort(index, kind="stable")
    counts = np.bincount(index[index >= 0], minlength=len(keys))
    bounds = np.concatenate([[int((index < 0).sum())], np.cumsum(counts) + int((index < 0).sum())])
    held = {str(keys[i]): [order[bounds[i] : bounds[i + 1]]] for i in range(len(keys))}
    wanted = {str(k): 0 for k in keys}
    return {"member_rows_scanned": int(len(index)),
            "views": {name: score(wanted, held, *views[name]) for name in views}}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", type=Path, default=None,
                    help=f"default $TESSERA_LADDER/{sources.RUNG}")
    ap.add_argument("--sample", type=int, default=SAMPLE_PER_LEVEL,
                    help="artifacts measured per level")
    ap.add_argument("--survey-rows", type=int, default=SURVEY_ROWS,
                    help="member rows the key survey reads before it stops")
    ap.add_argument("--seed", type=int, default=0)
    args = ap.parse_args()

    out = args.out or ladder(sources.RUNG)
    views = positions(out)
    for name, (x, y, area, entities) in views.items():
        print(f"{name}: {len(x):,} points, box {x.max() - x.min():.4f} x {y.max() - y.min():.4f}",
              flush=True)

    report = {}
    if (out / "members-taxonomy.parquet").exists():
        report["taxonomy/tree"] = taxonomy_spread(out, views, args.sample, args.seed,
                                                  args.survey_rows)
    report["publishers/source"] = predicate_spread(out, views, "publisher")

    for layer, got in report.items():
        for view, table in got["views"].items():
            print(f"{layer} on {view}: {table['artifacts_measured']} artifact(s) — median "
                  f"{table['median_share']:.4%}, p90 {table['p90_share']:.4%}, max "
                  f"{table['max_share']:.4%}, {table['under_5pc']:.0%} under 5%", flush=True)
            if len(table["per_level"]) > 1:
                print("    by level: " + " · ".join(
                    f"{lv} {v['median_share']:.3%} ({v['artifacts']})"
                    for lv, v in table["per_level"].items()))

    (out / "spread.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"wrote {out / 'spread.json'}")


if __name__ == "__main__":
    sys.exit(main())
