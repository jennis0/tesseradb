"""GeoNames — 13,463,857 features, the ladder's first geographic rung.

One DuckDB pass over `allCountries.txt` produces the points file, eight vocabularies, two member
files and the admin layer's artifact names. No GPU, no embedding, no projection in the UMAP sense — the coordinates are already WGS84
and become positions by [`..common.projection`][] and quantisation, which is why this rung is the
cheapest real map in the corpus.

**Everything here projects, and it should not.** See the standing deferral in `../README.md`: when
Tessera gains a projection layer this script emits `lon`/`lat` unchanged and the declaration names
a projection. The WGS84 box that the frame was asked for is written to `frame.json` so the
migration substitutes those numbers rather than deriving them back out of a constant.

Four decisions this script makes about the source, each because the data forced it rather than
because the campaign plan called for it:

- **Admin codes are country-qualified**, `GB.ENG` and not `ENG`. `admin1` alone has 823 distinct
  codes standing for 4,823 real regions, so the bare code merges Scotland with an unrelated
  Brazilian state — and GeoNames' own `admin1CodesASCII.txt` and `admin2Codes.txt` are qualified,
  so this is the publisher's convention rather than an invention.
- **A feature code is keyed by its `class.code` pair**, which is again how `featureCodes_en.txt` is
  keyed, and which is what keeps the taxonomy a tree: a bare code would put one artifact under
  several parents.
- **An absent value is null, never a key.** The empty string is refused as a category key and a
  null key means *absent* at the reserved code 0 (`per-point-attributes.md` §5), so a feature whose
  code was never recorded belongs to its class and to no code — which is what the source says. This
  covers blank feature classes (5,003 rows), blank countries (6,997) and the ragged admin tail.
- **`dem`'s -9999 becomes null.** It is the source's unknown sentinel, and carrying it as a value
  would put a false floor on every range filter over the column.
"""

from __future__ import annotations

import argparse
import json
import shutil
import time
import zipfile
from pathlib import Path

import duckdb

from ..common import projection
from ..common.paths import ladder, staged

RUNG = "geonames"

#: The artifact key component standing for *no code was recorded at this level*. It has to be a
#: real key rather than an empty component: the empty string is refused as a key, and a level-0
#: placeholder would otherwise have no name at all. Asserted against the source at every run.
NOT_RECORDED = "-"
VINTAGE = "2026-08-27"

#: The nine feature classes, from the publisher's own readme — there is no file for them, so this
#: is the one authored vocabulary in the rung. A tenth, blank, class exists in the data on 5,003
#: rows and is read as absent rather than given a key.
FEATURE_CLASSES = {
    "A": "Country, state, region",
    "H": "Stream, lake, other water body",
    "L": "Park, area, other region",
    "P": "City, village, other populated place",
    "R": "Road, railroad",
    "S": "Spot, building, farm",
    "T": "Mountain, hill, rock",
    "U": "Undersea feature",
    "V": "Forest, heath, other vegetation",
}

#: `countryInfo.txt`: a commented header, 19 tab-separated fields, and a ragged trailing one.
COUNTRY_INFO_COLUMNS = (
    "iso iso3 iso_numeric fips country_name capital area population continent tld "
    "currency_code currency_name phone postal_format postal_regex languages geonameid "
    "neighbours equivalent_fips"
).split()

#: `admin1CodesASCII.txt` and `admin2Codes.txt`, which are already country-qualified.
ADMIN_CODE_COLUMNS = ["key", "title", "ascii_title", "geonameid"]

#: `featureCodes_en.txt`: `class.code`, name, description — the description is often absent.
FEATURE_CODE_COLUMNS = ["key", "title", "description"]

#: The 19 columns of `allCountries.txt`, in order, with the types to read them at.
COLUMNS = {
    "geonameid": "BIGINT",
    "name": "VARCHAR",
    "asciiname": "VARCHAR",
    "alternatenames": "VARCHAR",
    "latitude": "DOUBLE",
    "longitude": "DOUBLE",
    "feature_class": "VARCHAR",
    "feature_code": "VARCHAR",
    "country_code": "VARCHAR",
    "cc2": "VARCHAR",
    "admin1_code": "VARCHAR",
    "admin2_code": "VARCHAR",
    "admin3_code": "VARCHAR",
    "admin4_code": "VARCHAR",
    "population": "BIGINT",
    "elevation": "INTEGER",
    "dem": "INTEGER",
    "timezone": "VARCHAR",
    "modification_date": "DATE",
}


def load_lookup(con, table: str, path: Path, columns: list[str]) -> int:
    """Read one of the small tab-separated lookup files into a table.

    Done here rather than with `read_csv` because DuckDB's dialect sniffer runs ahead of an explicit
    `columns` and `countryInfo.txt`'s commented header defeats it whatever is declared. These files
    are at most 2.4 MB, so the argument is not worth having.
    """
    rows = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        fields += [""] * (len(columns) - len(fields))
        rows.append(tuple(f.strip() or None for f in fields[: len(columns)]))
    con.execute(
        f"CREATE OR REPLACE TABLE {table} ({', '.join(f'{c} VARCHAR' for c in columns)})"
    )
    if rows:
        con.executemany(
            f"INSERT INTO {table} VALUES ({', '.join('?' * len(columns))})", rows
        )
    return len(rows)


def unpack(src: Path, out: Path, member: str) -> Path:
    """Extract one member of a staged zip to the derived directory, once."""
    target = out / member
    if target.exists():
        print(f"  {member}: already unpacked ({target.stat().st_size / 1e9:.2f} GB)")
        return target
    started = time.monotonic()
    with zipfile.ZipFile(src) as zf:
        zf.extract(member, out)
    size = target.stat().st_size
    print(f"  {member}: {size / 1e9:.2f} GB in {time.monotonic() - started:.0f}s")
    return target


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--frame", choices=("unit", "metres"), default="unit")
    ap.add_argument("--vintage", default=VINTAGE)
    ap.add_argument("--limit", type=int, default=None, help="keep the first N by entity id")
    args = ap.parse_args()

    src, out = staged(RUNG, args.vintage), ladder(RUNG)
    ext = projection.extent(args.frame)
    print(f"source  {src}\noutput  {out}\nframe   {args.frame} — extent [{ext.min}, {ext.max}]\n")

    print("unpacking")
    all_countries = unpack(src / "allCountries.zip", out, "allCountries.txt")

    con = duckdb.connect(str(out / "prepare.duckdb"))
    con.execute("PRAGMA enable_progress_bar=false")

    print("\nlookups")
    for table, filename, columns in (
        ("country_info", "countryInfo.txt", COUNTRY_INFO_COLUMNS),
        ("admin1_titles", "admin1CodesASCII.txt", ADMIN_CODE_COLUMNS),
        ("admin2_titles", "admin2Codes.txt", ADMIN_CODE_COLUMNS),
        ("feature_code_titles", "featureCodes_en.txt", FEATURE_CODE_COLUMNS),
    ):
        n = load_lookup(con, table, src / filename, columns)
        print(f"  {table:<20} {n:>7,} rows")

    columns = ", ".join(f"'{name}': '{ty}'" for name, ty in COLUMNS.items())
    print("\nreading allCountries.txt")
    started = time.monotonic()
    con.execute(
        f"""
        CREATE OR REPLACE TABLE raw AS
        SELECT * FROM read_csv(
            '{all_countries}',
            delim = '\t', header = false, quote = '', escape = '',
            nullstr = '', columns = {{{columns}}}
        )
        """
    )
    rows = con.execute("SELECT count(*) FROM raw").fetchone()[0]
    print(f"  {rows:,} rows in {time.monotonic() - started:.0f}s")


    x_expr, y_expr = projection.sql("longitude", "latitude", args.frame)

    print("\nprojecting and assigning entity ids")
    started = time.monotonic()
    con.execute(
        f"""
        CREATE OR REPLACE TABLE points AS
        WITH marked AS (
            SELECT *,
                   coalesce(country_code, '{NOT_RECORDED}') AS m0,
                   coalesce(admin1_code,  '{NOT_RECORDED}') AS m1,
                   coalesce(admin2_code,  '{NOT_RECORDED}') AS m2,
                   coalesce(admin3_code,  '{NOT_RECORDED}') AS m3,
                   coalesce(admin4_code,  '{NOT_RECORDED}') AS m4
            FROM raw
        )
        SELECT
            row_number() OVER (ORDER BY modification_date, geonameid) - 1 AS entity_id,
            geonameid,
            {x_expr} AS x,
            {y_expr} AS y,
            feature_class,
            CASE WHEN feature_class IS NOT NULL AND feature_code IS NOT NULL
                 THEN feature_class || '.' || feature_code END AS feature_code,
            CASE WHEN country_code IS NOT NULL OR admin1_code IS NOT NULL
                      OR admin2_code IS NOT NULL OR admin3_code IS NOT NULL
                      OR admin4_code IS NOT NULL THEN m0 END AS country_artifact,
            country_code AS country,
            -- **A hole in the chain becomes an artifact, not a null.** GeoNames carries 115,042
            -- rows with an admin4 and no admin3, and dropping them would discard what the file
            -- says. A null entry would keep them but cost the containment edge across it:
            -- `parent_edges` is `windows(2)` and deliberately does not read past a gap, because
            -- for a clustering a null means *noise at that resolution* and skipping would state a
            -- containment no row makes. Here it means *no code was recorded*, which is a different
            -- claim — so the level gets an explicit `NOT_RECORDED` artifact naming exactly that
            -- set. It is not invented: the key `US.-.X` already names it, and this materialises the
            -- intermediate so the tree is total and every edge exists.
            --
            -- A level is filled only where something **below** it is present. A chain that simply
            -- ends is not a hole, and its remaining levels stay null.
            CASE WHEN admin1_code IS NOT NULL OR admin2_code IS NOT NULL
                      OR admin3_code IS NOT NULL OR admin4_code IS NOT NULL
                 THEN concat_ws('.', m0, m1) END AS admin1,
            CASE WHEN admin2_code IS NOT NULL OR admin3_code IS NOT NULL
                      OR admin4_code IS NOT NULL
                 THEN concat_ws('.', m0, m1, m2) END AS admin2,
            CASE WHEN admin3_code IS NOT NULL OR admin4_code IS NOT NULL
                 THEN concat_ws('.', m0, m1, m2, m3) END AS admin3,
            CASE WHEN admin4_code IS NOT NULL
                 THEN concat_ws('.', m0, m1, m2, m3, m4) END AS admin4,
            timezone,
            population,
            elevation::SMALLINT AS elevation,
            nullif(dem, -9999)::SMALLINT AS dem,
            modification_date::TIMESTAMP AS modification_date,
            name,
            latitude,
            admin1_code, admin2_code, admin3_code, admin4_code
        FROM marked
        {f"QUALIFY entity_id < {args.limit}" if args.limit else ""}
        """
    )
    collisions = con.execute(
        f"""
        SELECT count(*) FROM raw WHERE '{NOT_RECORDED}' IN
            (country_code, admin1_code, admin2_code, admin3_code, admin4_code)
        """
    ).fetchone()[0]
    if collisions:
        raise SystemExit(
            f"{collisions:,} source rows carry {NOT_RECORDED!r} as an administrative code, which "
            f"is the marker this script uses for a level the source did not record. A placeholder "
            f"would merge with a real region; choose another marker."
        )
    kept = con.execute("SELECT count(*) FROM points").fetchone()[0]
    # Counted over the rows kept rather than over the file, so that --limit reports its own corpus.
    # The build's clamp report structurally cannot see these: a clipped point lands at exactly the
    # frame maximum, and a point at the maximum is not clamped (`contracts` §2.5), which is why
    # `projections.md` §4a asks for a separate count.
    clipped = con.execute(
        f"""
        SELECT count(*) FILTER (latitude > {projection.MAX_LATITUDE!r}),
               count(*) FILTER (latitude < {-projection.MAX_LATITUDE!r})
        FROM points
        """
    ).fetchone()
    print(f"  {kept:,} points in {time.monotonic() - started:.0f}s")

    print("\nwriting")
    con.execute(
        f"""COPY (
            SELECT entity_id, x, y, feature_class, feature_code, country,
                   admin1, admin2, admin3, admin4, timezone,
                   population, elevation, dem, modification_date, name
            FROM points ORDER BY entity_id
        ) TO '{out / "points.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
    )

    # --- vocabularies -------------------------------------------------------------------------
    # Codes are dense from 1: code 0 is reserved for an absent value (`per-point-attributes.md`
    # §5), so a vocabulary that started at 0 would collide with absence on its first key.
    vocabularies = {
        "feature-class": f"""
            SELECT key, row_number() OVER (ORDER BY key) AS code, title FROM (
                SELECT unnest([{", ".join(f"'{k}'" for k in FEATURE_CLASSES)}]) AS key,
                       unnest([{", ".join(chr(39) + t.replace(chr(39), chr(39) * 2) + chr(39)
                                          for t in FEATURE_CLASSES.values())}]) AS title
            )""",
        "feature-code": f"""
            SELECT key, row_number() OVER (ORDER BY key) AS code, title FROM (
                SELECT key, title
                FROM feature_code_titles
                WHERE key <> 'null'
            )""",
        "country": f"""
            SELECT key, row_number() OVER (ORDER BY key) AS code, title FROM (
                SELECT DISTINCT p.country AS key,
                       coalesce(any_value(c.country_name), p.country) AS title
                FROM points p
                LEFT JOIN country_info c ON c.iso = p.country
                WHERE p.country IS NOT NULL
                GROUP BY p.country
            )""",
        "admin1": f"""
            SELECT key, row_number() OVER (ORDER BY key) AS code, title FROM (
                SELECT DISTINCT p.admin1 AS key,
                       coalesce(any_value(a.title), p.admin1) AS title
                FROM points p
                LEFT JOIN admin1_titles a ON a.key = p.admin1
                WHERE p.admin1 IS NOT NULL
                GROUP BY p.admin1
            )""",
        "admin2": f"""
            SELECT key, row_number() OVER (ORDER BY key) AS code, title FROM (
                SELECT DISTINCT p.admin2 AS key,
                       coalesce(any_value(a.title), p.admin2) AS title
                FROM points p
                LEFT JOIN admin2_titles a ON a.key = p.admin2
                WHERE p.admin2 IS NOT NULL
                GROUP BY p.admin2
            )""",
        "admin3": "SELECT admin3 AS key, row_number() OVER (ORDER BY admin3) AS code, "
        "admin3 AS title FROM (SELECT DISTINCT admin3 FROM points WHERE admin3 IS NOT NULL)",
        "admin4": "SELECT admin4 AS key, row_number() OVER (ORDER BY admin4) AS code, "
        "admin4 AS title FROM (SELECT DISTINCT admin4 FROM points WHERE admin4 IS NOT NULL)",
        "timezone": "SELECT timezone AS key, row_number() OVER (ORDER BY timezone) AS code, "
        "timezone AS title FROM (SELECT DISTINCT timezone FROM points WHERE timezone IS NOT NULL)",
    }
    sizes = {}
    for name, query in vocabularies.items():
        path = out / f"vocab-{name}.parquet"
        con.execute(f"COPY ({query}) TO '{path}' (FORMAT parquet)")
        sizes[name] = con.execute(f"SELECT count(*) FROM '{path}'").fetchone()[0]

    # --- the admin layer's names ------------------------------------------------------------
    # One row per artifact, carrying its name as the layer's supplied `name` content: countries
    # from `countryInfo.txt`, admin1 and admin2 from GeoNames' own code tables, and the code itself
    # everywhere else — admin3 and admin4, which GeoNames publishes no names for, and the
    # placeholder `-` levels. **Every artifact, not only the named ones**: a layer declaring a
    # supplied kind is one whose every artifact carries it, and the build refuses an artifact that
    # does not ("carries no supplied content, and this layer declares 1 kind(s)"). Enrichment
    # beside an open value set: the artifacts are still exactly what the member file names.
    con.execute(
        f"""COPY (
            SELECT level::UINTEGER AS level, key, [[coalesce(title, key)]] AS contents FROM (
                SELECT 0 AS level, k.key, v.title FROM (SELECT DISTINCT country_artifact AS key FROM points) k
                    LEFT JOIN '{out / "vocab-country.parquet"}' v USING (key)
                UNION ALL SELECT 1, k.key, v.title FROM (SELECT DISTINCT admin1 AS key FROM points) k
                    LEFT JOIN '{out / "vocab-admin1.parquet"}' v USING (key)
                UNION ALL SELECT 2, k.key, v.title FROM (SELECT DISTINCT admin2 AS key FROM points) k
                    LEFT JOIN '{out / "vocab-admin2.parquet"}' v USING (key)
                UNION ALL SELECT 3, key, NULL FROM (SELECT DISTINCT admin3 AS key FROM points)
                UNION ALL SELECT 4, key, NULL FROM (SELECT DISTINCT admin4 AS key FROM points)
            ) WHERE key IS NOT NULL
            ORDER BY level, key
        ) TO '{out / "artifacts-admin.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
    )
    named = con.execute(
        f"""SELECT level, count(*), count(*) FILTER (contents[1][1] <> key)
            FROM '{out / "artifacts-admin.parquet"}' GROUP BY level ORDER BY level"""
    ).fetchall()
    print("  admin artifacts, and how many carry a published name rather than their code:")
    for level, total, titled in named:
        print(f"    {('country', 'admin1', 'admin2', 'admin3', 'admin4')[level]:<8} {total:>8,}  named {titled:>8,}")

    # --- member files -------------------------------------------------------------------------
    # One row per point, `key` a list whose positions are the declared levels — the shape
    # `[layer.members]` reads for a tiered layer. A null entry means *in no artifact at that
    # level*, which is what the ragged admin tail and an unrecorded feature code both are.
    con.execute(
        f"""COPY (
            SELECT entity_id::UBIGINT AS entity, [feature_class, feature_code] AS key
            FROM points ORDER BY entity_id
        ) TO '{out / "members-feature.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
    )
    con.execute(
        f"""COPY (
            SELECT entity_id::UBIGINT AS entity,
                   [country_artifact, admin1, admin2, admin3, admin4] AS key
            FROM points ORDER BY entity_id
        ) TO '{out / "members-admin.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
    )

    # --- the frame report ---------------------------------------------------------------------
    bounds = con.execute(
        "SELECT min(x), max(x), min(y), max(y) FROM points"
    ).fetchone()
    frame = {
        "frame": args.frame,
        "extent": {"min": ext.min, "max": ext.max},
        "projection": "web_mercator",
        "y_direction": "south",
        "asked_for_wgs84": projection.WORLD_BOX_WGS84,
        "clipped": {"north": clipped[0], "south": clipped[1], "total": sum(clipped)},
        "data_bounds": {"x": [bounds[0], bounds[1]], "y": [bounds[2], bounds[3]]},
        "points": kept,
        "source": str(src),
        "vintage": args.vintage,
    }
    # --- report -------------------------------------------------------------------------------
    print(f"\npoints        {kept:,}")
    print(f"extent        [{ext.min}, {ext.max}] — {args.frame}, y south")
    print(f"data bounds   x [{bounds[0]:.6f}, {bounds[1]:.6f}]  y [{bounds[2]:.6f}, {bounds[3]:.6f}]")
    print(
        f"clipped       {sum(clipped):,} beyond ±{projection.MAX_LATITUDE:.4f}° "
        f"({clipped[0]:,} north, {clipped[1]:,} south) — at the frame edge, not clamped"
    )
    print("\nvocabularies")
    for name, size in sizes.items():
        print(f"  {name:<14} {size:>9,}")

    # Every row is placed as deep as its own codes go, so nothing is dropped — but a key with an
    # empty component is a key whose containment edge above it does not exist, and that is a
    # property of the corpus worth stating rather than discovering. Reported, never refused.
    gaps = con.execute(
        """
        SELECT
            count(*) FILTER (admin1_code IS NOT NULL AND country IS NULL),
            count(*) FILTER (admin2_code IS NOT NULL AND admin1_code IS NULL),
            count(*) FILTER (admin3_code IS NOT NULL
                             AND (admin1_code IS NULL OR admin2_code IS NULL)),
            count(*) FILTER (admin4_code IS NOT NULL
                             AND (admin1_code IS NULL OR admin2_code IS NULL
                                  OR admin3_code IS NULL)),
            count(admin1_code), count(admin2_code), count(admin3_code), count(admin4_code)
        FROM points
        """
    ).fetchone()
    print(f"\nadmin chain — levels filled with a {NOT_RECORDED!r} artifact to keep the tree total")
    for i, level in enumerate(("admin1", "admin2", "admin3", "admin4")):
        filled, carried = gaps[i], gaps[i + 4]
        pct = filled * 100.0 / carried if carried else 0.0
        print(f"  {level:<8} {filled:>9,} of {carried:>10,} ({pct:5.1f}%) sit below a filled level")
    placeholders = con.execute(
        f"""
        SELECT
            count(DISTINCT country_artifact) FILTER (country IS NULL),
            count(DISTINCT admin1) FILTER (admin1_code IS NULL),
            count(DISTINCT admin2) FILTER (admin2_code IS NULL),
            count(DISTINCT admin3) FILTER (admin3_code IS NULL),
            count(DISTINCT admin4) FILTER (admin4_code IS NULL)
        FROM points
        """
    ).fetchone()
    print(f"  placeholder artifacts created: {dict(zip(('country','admin1','admin2','admin3','admin4'), placeholders))}")

    frame["admin_chain_filled"] = {
        level: {"below_a_filled_level": gaps[i], "carrying": gaps[i + 4]}
        for i, level in enumerate(("admin1", "admin2", "admin3", "admin4"))
    }
    (out / "frame.json").write_text(json.dumps(frame, indent=2) + "\n")

    # **Every numeric's measured range, printed whether or not anything is wrong.** A width is
    # declared from these, and a declaration written from a maximum alone is how `population` came
    # to be `u64` against a column holding -12. The build refuses such a value rather than
    # truncating it, which is right and is also a slow way to find out.
    print("\nnumeric ranges — the widths in the declaration are written from these")
    for column in ("population", "elevation", "dem", "entity_id"):
        lo, hi, neg = con.execute(
            f"SELECT min({column}), max({column}), count(*) FILTER ({column} < 0) FROM points"
        ).fetchone()
        print(f"  {column:<14} [{lo}, {hi}]" + (f"  — {neg:,} negative" if neg else ""))

    print("\nnull counts")
    nulls = con.execute(
        """
        SELECT
            count(*) - count(feature_class), count(*) - count(feature_code),
            count(*) - count(country), count(*) - count(admin1), count(*) - count(admin2),
            count(*) - count(admin3), count(*) - count(admin4), count(*) - count(timezone),
            count(*) - count(elevation), count(*) - count(dem), count(*) - count(name)
        FROM points
        """
    ).fetchone()
    for name, n in zip(
        "feature_class feature_code country admin1 admin2 admin3 admin4 timezone "
        "elevation dem name".split(),
        nulls,
    ):
        print(f"  {name:<14} {n:>12,}  ({n * 100.0 / kept:5.1f}%)")

    for path in sorted(out.glob("*.parquet")):
        print(f"  {path.name:<28} {path.stat().st_size / 1e6:>9.1f} MB")

    # **The demo's candidate terms, ranked by coverage.** `run_demo.sh --ranks` composes the
    # sparse/medium/heavy coverage principals from this: at a 253-term dictionary any single
    # country is a sliver of 13.5M points, so switching principal between two of them demonstrates
    # nothing. The shape is the one `scripts/rank_terms.py` emits for the arXiv fixtures.
    ranks = con.execute(
        """
        SELECT country AS term, count(*) AS pairs
        FROM points WHERE country IS NOT NULL
        GROUP BY country ORDER BY pairs DESC
        """
    ).fetchall()
    (out / "country-ranks.json").write_text(
        json.dumps([{"term": t, "pairs": n} for t, n in ranks], indent=None) + "\n"
    )
    (out / "country-terms.txt").write_text(",".join(t for t, _ in ranks) + "\n")
    print(f"\nwrote country-ranks.json and country-terms.txt ({len(ranks)} terms)")
    print(f"  top five: {', '.join(f'{t} {n:,}' for t, n in ranks[:5])}")

    # The declaration is copied rather than referenced: `[sources]` is relative to the declaring
    # document (`configuration.md` §3), so the copy beside the parquets is what resolves. The git
    # one stays the reviewed original.
    here = Path(__file__).parent
    shutil.copy(here / "corpus.toml", out / "corpus.toml")

    # The deployment file is generated rather than committed — every value in it is a path or a
    # port on this machine, which is the split `configuration.md` §3 draws.
    (out / "tessera.toml").write_text(
        f"""# Generated by `test_corpora/geonames/prepare.py`. Machine-specific by construction: paths
# and ports, and the *name* of the variable carrying the identity key, never the key.

[bundle]
path  = "bundle"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "corpus.toml"

[plugin]
module = "builtin:passthrough"

[identity]
env = "TESSERA_IDENTITY_KEY"

[disclosure]
min_visible_members = 10
token_max_lifetime  = 3600

[serve]
viewer  = "127.0.0.1:8081"
session = "127.0.0.1:8082"
control = "127.0.0.1:8083"
max_k   = 200
session_credential_env  = "TESSERA_GEONAMES_SESSION_CRED"
operator_credential_env = "TESSERA_GEONAMES_OPERATOR_CRED"
"""
    )

    # **Created once and never overwritten.** The identity key is what every `tessera_id` this
    # corpus has ever served is derived from, so regenerating it on a rerun would invalidate every
    # identifier a client holds and reorder every row (contracts §2.2). A rebuild that preserves
    # this corpus's identity is the ordinary path, and that means the key outlives the script.
    env = out / ".env"
    if not env.exists():
        import secrets

        env.write_text(f"TESSERA_IDENTITY_KEY={secrets.token_hex(16)}\n")
        env.chmod(0o600)
        print(f"minted a new identity key in {env} — this corpus's lineage starts here")

    print(f"\nwrote {out / 'corpus.toml'} and {out / 'tessera.toml'}")
    print(f"\nnext:\n  cd {out}\n  tessera check\n  tessera build")

    con.close()


if __name__ == "__main__":
    main()
