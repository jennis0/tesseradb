"""The places example dataset in `examples/data/geonames/`: about 150,000 GeoNames features of every
kind.

Every populated place (feature class P) of 15,000 people or more is kept, which is about GeoNames'
own `cities15000`, so every large city is there. The rest of the 150,000 is a seeded uniform sample
of all other features, so the mix of feature classes among them is the gazetteer's own: mostly
buildings, water, mountains and small places. Administrative regions also carry a population, and
are left to the sample so that they do not crowd the map with points.

The names of countries, first-level divisions and feature codes come from the vocabularies
`prepare.py` wrote beside `allCountries.txt`, which it read from GeoNames' own code tables, so this
script reads nothing but the ladder's directory.

The declaration follows the ladder rung's: lon and lat as GeoNames publishes them, a
`web_mercator` view over Web Mercator's whole domain, each feature visible to a principal holding
its country, and an administrative layer of two tiers, country over first-level division, each
named. A feature class is a short word rather than GeoNames' letter, so a legend reads without a
key; the vocabulary's title is GeoNames' own description. A population of 0 is GeoNames saying it
has none, and is null here.

Each feature is named by its GeoNames id, and the member file names it the same way.

```bash
python -m test_corpora.geonames.example --out examples/data/geonames \\
    --min-population 15000 --features 150000 --seed 0
```
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import duckdb
import numpy as np
import pyarrow as pa

from ..common.paths import LADDER_ROOT
from .prepare import COLUMNS, FEATURE_CLASSES, NOT_RECORDED, VINTAGE

#: The word each feature class is shown as. GeoNames' description is the vocabulary's title.
CLASS_WORDS = {
    "A": "region",
    "H": "water",
    "L": "area",
    "P": "settlement",
    "R": "road",
    "S": "building",
    "T": "mountain",
    "U": "undersea",
    "V": "forest",
}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--ladder", type=Path, default=LADDER_ROOT / "geonames",
                    help="the directory `prepare.py` wrote, holding allCountries.txt")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--min-population", type=int, default=15_000)
    ap.add_argument("--features", type=int, default=150_000)
    ap.add_argument("--seed", type=int, default=0)
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    src, out = args.ladder, args.out

    con = duckdb.connect()
    con.execute("PRAGMA enable_progress_bar=false")
    columns = ", ".join(f"'{name}': '{ty}'" for name, ty in COLUMNS.items())
    con.execute(
        f"""CREATE TABLE raw AS SELECT * FROM read_csv('{src / "allCountries.txt"}',
                delim = '\t', header = false, quote = '', escape = '', nullstr = '',
                columns = {{{columns}}})"""
    )
    is_populous = f"(feature_class = 'P' AND population >= {args.min_population})"
    populous = con.execute(f"SELECT count(*) FROM raw WHERE {is_populous}").fetchone()[0]
    others = con.execute(
        f"SELECT geonameid FROM raw WHERE NOT coalesce({is_populous}, false) ORDER BY geonameid"
    ).fetchnumpy()["geonameid"]
    drawn = np.random.default_rng(args.seed).choice(others, args.features - populous, replace=False)
    con.register("drawn", pa.table({"geonameid": pa.array(np.sort(drawn), pa.int64())}))

    words = pa.table({"letter": list(CLASS_WORDS), "word": list(CLASS_WORDS.values()),
                      "description": [FEATURE_CLASSES[c] for c in CLASS_WORDS]})
    con.register("words", words)
    con.execute(
        f"""
        CREATE TABLE places AS
        SELECT r.geonameid, r.longitude AS lon, r.latitude AS lat, r.name,
               w.word AS feature_class,
               r.feature_class || '.' || r.feature_code AS feature_code,
               r.country_code AS country,
               -- Country-qualified, as GeoNames' own admin1 table is keyed: a bare code merges
               -- regions of different countries.
               CASE WHEN r.admin1_code IS NOT NULL
                    THEN coalesce(r.country_code, '{NOT_RECORDED}') || '.' || r.admin1_code
               END AS admin1,
               nullif(r.population, 0) AS population
        FROM raw r LEFT JOIN words w ON w.letter = r.feature_class
        WHERE coalesce({is_populous}, false)
           OR r.geonameid IN (SELECT geonameid FROM drawn)
        """
    )
    kept = con.execute("SELECT count(*) FROM places").fetchone()[0]
    print(f"{kept:,} features: {populous:,} populated places of {args.min_population:,} or more, "
          f"{kept - populous:,} drawn from the other {len(others):,} with seed {args.seed}")

    con.execute(
        f"""COPY (
            SELECT geonameid, lon, lat, name, feature_class, feature_code, country, admin1,
                   population
            FROM places ORDER BY geonameid
        ) TO '{out / "points.parquet"}' (FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 19)"""
    )

    # The vocabularies, cut to the values the features carry and numbered again from 1, code 0
    # being the absent value. A name the code tables lack is the key itself, as in the ladder.
    con.execute(
        f"""COPY (
            SELECT word AS key, row_number() OVER (ORDER BY word)::UINTEGER AS code,
                   description AS title
            FROM words WHERE word IN (SELECT feature_class FROM places)
        ) TO '{out / "vocab-feature-class.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
    )
    for name, column in [("feature-code", "feature_code"), ("country", "country"),
                         ("admin1", "admin1")]:
        con.execute(
            f"""COPY (
                SELECT key, row_number() OVER (ORDER BY key)::UINTEGER AS code,
                       coalesce(v.title, key) AS title
                FROM (SELECT DISTINCT {column} AS key FROM places WHERE {column} IS NOT NULL)
                LEFT JOIN '{src / f"vocab-{name}.parquet"}' v USING (key)
                ORDER BY key
            ) TO '{out / f"vocab-{name}.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
        )

    # The administrative layer: one member row per feature, its country and its first-level
    # division as the two tiers, and one artifact per region carrying its name. A feature with a
    # division and no country sits under a `-` country, which is a real key naming that set.
    con.execute(
        f"""COPY (
            SELECT geonameid AS entity,
                   [CASE WHEN country IS NOT NULL OR admin1 IS NOT NULL
                         THEN coalesce(country, '{NOT_RECORDED}') END, admin1] AS key
            FROM places ORDER BY geonameid
        ) TO '{out / "members-admin.parquet"}' (FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 19)"""
    )
    con.execute(
        f"""COPY (
            SELECT level::UINTEGER AS level, key, [[title]] AS contents FROM (
                SELECT 0 AS level, key, title FROM '{out / "vocab-country.parquet"}'
                UNION ALL SELECT 0, '{NOT_RECORDED}', 'No country recorded'
                    WHERE EXISTS (SELECT 1 FROM places WHERE country IS NULL AND admin1 IS NOT NULL)
                UNION ALL SELECT 1, key, title FROM '{out / "vocab-admin1.parquet"}'
            ) ORDER BY level, key
        ) TO '{out / "artifacts-admin.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
    )

    classes = con.execute(
        "SELECT feature_class, count(*) FROM places GROUP BY 1 ORDER BY 2 DESC"
    ).fetchall()
    sizes = {p.name: p.stat().st_size for p in sorted(out.glob("*.parquet"))}
    rows = {name: con.execute(f"SELECT count(*) FROM '{out / name}'").fetchone()[0] for name in sizes}
    manifest = {
        "cut_from": f"geonames/{VINTAGE}/allCountries.txt",
        "min_population": args.min_population,
        "populous": populous,
        "drawn": kept - populous,
        "seed": args.seed,
        "features": kept,
        "feature_classes": dict(classes),
        "rows": rows,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for name, size in sizes.items():
        print(f"  {name:<28} {rows[name]:>9,} rows  {size / 1e6:6.2f} MB")
    print(f"{sum(sizes.values()) / 1e6:.2f} MB of Parquet in {out}")
    print("feature classes: " + ", ".join(f"{c} {n:,}" for c, n in classes))


if __name__ == "__main__":
    main()
