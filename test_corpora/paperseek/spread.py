"""The share of the map an artifact's **middle 90%** of members occupies, per layer.

The measurement that withdrew two taxonomies at rungs 1 and 2 and kept both of rung 3's: the box
between the 5th and 95th percentile of an artifact's members in x and in y, as a fraction of the
whole map's box. **A layer earns its place by drawing something in the view it is declared over**,
and this is how that is decided rather than argued.

    ~/venvs/projection/bin/python -m test_corpora.paperseek.spread --out $TESSERA_LADDER/paperseek

It reads `points.parquet` and the layers' own member files — the build's inputs, not the bundle —
so it can be run before or after a build and needs no server.

**Neither layer is read whole where it does not have to be.** `clusters/kmeans` is 256 artifacts
over every point and is measured exactly; `topics/openalex` is thousands of artifacts across four
levels and 10⁸-scale membership, so a uniform sample of keys **per level** is taken and only their
member rows are kept as the file streams past. A sampled figure says so in what it prints.

⊘ **This is not the build's `everywhere` fraction.** A box covering 1% of the map can still be
wider than the tile-index node at the depth a level is served from, so an artifact may be compact
here and served as a list there. Rung 3 was the first corpus to separate the two and this rung
reports both; they measure different things.
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

from ..arxiv.writer import slug
from ..common.paths import ladder

#: Artifacts sampled per level on a layer too large to measure exactly.
SAMPLE_PER_LEVEL = 150

#: An artifact with fewer members than this has no middle 90% worth quoting.
MIN_MEMBERS = 10


def layer_spread(
    out: Path, layer: str, x: np.ndarray, y: np.ndarray, area: float, *, sample: int | None, seed: int
) -> dict:
    """One layer's table. `sample` caps the artifacts measured per level; `None` measures all."""
    artifacts = pq.read_table(out / f"{slug(layer)}.parquet", columns=["level", "key"])
    levels = np.asarray(artifacts.column("level"), dtype=np.int64)
    keys = np.asarray(artifacts.column("key"))

    rng = np.random.default_rng(seed)
    wanted: dict[str, int] = {}
    for level in sorted(set(levels.tolist())):
        at = np.flatnonzero(levels == level)
        if sample is not None and len(at) > sample:
            at = rng.choice(at, sample, replace=False)
        for i in at:
            wanted[str(keys[i])] = int(level)

    # One streaming pass over the member file, keeping only the sampled keys' rows. The whole file
    # is 10⁸–10⁹ rows and the entity column alone would be gigabytes held.
    held: dict[str, list[np.ndarray]] = {k: [] for k in wanted}
    reader = pq.ParquetFile(out / f"{slug(layer)}-members.parquet")
    rows = 0
    # The filter runs in Arrow against the dictionary-encoded key column, once per row group. The
    # obvious spelling — a numpy comparison per wanted key — is 150 passes over every row group,
    # which is 10¹⁰ comparisons on this rung's member file.
    value_set = pa.array(list(wanted), pa.string())
    for group in range(reader.metadata.num_row_groups):
        batch = reader.read_row_group(group, columns=["key", "rank", "entity"])
        rows += batch.num_rows
        # A ranked row is a generating set, not the artifact's own membership.
        sel = pc.and_(
            pc.is_null(batch.column("rank")),
            pc.is_in(batch.column("key").cast(pa.string()), value_set=value_set),
        )
        sub = batch.filter(sel)
        if sub.num_rows:
            key = np.asarray(sub.column("key").combine_chunks().cast(pa.string()))
            entity = np.asarray(sub.column("entity"), dtype=np.int64)
            uniq, inverse = np.unique(key, return_inverse=True)
            for i, k in enumerate(uniq):
                held[str(k)].append(entity[inverse == i])
            del key, entity
        del batch, sub, sel

    measured = []
    for key, level in wanted.items():
        if not held[key]:
            continue
        e = np.concatenate(held[key])
        if len(e) < MIN_MEMBERS:
            continue
        px = np.percentile(x[e], [5, 95])
        py = np.percentile(y[e], [5, 95])
        measured.append(
            {"key": key, "level": level, "members": int(len(e)),
             "share": float((px[1] - px[0]) * (py[1] - py[0]) / area)}
        )

    shares = np.array([r["share"] for r in measured]) if measured else np.zeros(0)
    return {
        "layer": layer,
        "member_rows_scanned": rows,
        "artifacts_declared": len(keys),
        "artifacts_measured": len(measured),
        "sampled": sample is not None and len(keys) > sample,
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


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", type=Path, default=None, help="default $TESSERA_LADDER/paperseek")
    ap.add_argument("--sample", type=int, default=SAMPLE_PER_LEVEL,
                    help="artifacts measured per level on a layer larger than this")
    ap.add_argument("--seed", type=int, default=0)
    args = ap.parse_args()

    out = args.out or ladder("paperseek")
    points = pq.read_table(out / "points.parquet", columns=["x", "y"])
    x = np.asarray(points.column("x"), dtype=np.float64)
    y = np.asarray(points.column("y"), dtype=np.float64)
    del points
    area = (x.max() - x.min()) * (y.max() - y.min())
    print(f"{len(x):,} points, map {x.max() - x.min():.2f} x {y.max() - y.min():.2f}", flush=True)

    report = []
    for layer in ("clusters/kmeans", "topics/openalex"):
        if not (out / f"{slug(layer)}-members.parquet").exists():
            print(f"⊘ {layer}: no member file — not declared by this run")
            continue
        # The clustering is 256 artifacts over the whole corpus and is measured exactly; the topic
        # tree is thousands across four levels and is sampled.
        sample = None if layer == "clusters/kmeans" else args.sample
        got = layer_spread(out, layer, x, y, area, sample=sample, seed=args.seed)
        report.append(got)
        print(
            f"{layer}: {got['artifacts_measured']} of {got['artifacts_declared']} artifact(s) "
            f"{'sampled' if got['sampled'] else 'measured exactly'} — median "
            f"{got['median_share']:.4%}, p90 {got['p90_share']:.4%}, max {got['max_share']:.4%}, "
            f"{got['under_5pc']:.0%} under 5%",
            flush=True,
        )

    (out / "spread.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"wrote {out / 'spread.json'}")


if __name__ == "__main__":
    sys.exit(main())
