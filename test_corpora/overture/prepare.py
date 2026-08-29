"""Overture Maps places + divisions — 73,631,092 places, the ladder's second geographic rung.

DuckDB with the `spatial` extension over the staged GeoParquet. Two passes: the divisions are
staged once as a local polygon table, and the places are streamed part by part through a
point-in-polygon join against it. No GPU, no embedding — the coordinates are already WGS84 and
become positions by [`..common.projection`][] and quantisation.

**Everything here projects, and it should not.** See the standing deferral in `../README.md`.

Five decisions this script makes about the source, each because the survey of 2026-08-28 forced it
rather than because the campaign plan called for it. The plan is
`docs/evidence/memos/2026-08-27-ingest-campaign-plan.md` §9.2 and it is departed from at each.

- **The boundary layer is `nested`, not `tiered`.** The plan reads Overture's twelve division
  subtypes as twelve levels. They are not levels: a division's own hierarchy path runs 1 to 9
  entries deep and `locality` appears at every position from 1 to 8 — a locality contains a
  locality, which is an edge no ladder can hold. `nested` is a tree in the edges with every
  artifact at level 0 (`configuration.md`, the four hierarchy kinds), which is what the data is.
- **The lineage comes from the divisions' own hierarchy, not from stacking spatial hits.** One
  join finds the containing areas; the deepest one's `hierarchies[1]` is the whole ancestry, and
  it is consistent by construction. Stacking the hits by subtype would not be: `division_area`
  polygons are generalised for cartography, so a point can sit inside a locality and outside the
  county that locality belongs to, and the stacked chain would then assert a containment the
  publisher does not.
- **`is_land` only.** 1,983 divisions carry a maritime area beside their land one. Joining both
  places a point at sea inside a country, which is true of the territorial claim and not of the
  boundary layer this rung is for.
- **`basic_category` is a rollup, not the leaf.** 278 distinct values against 1,847 leaves in one
  part, and it equals `taxonomy.primary` on 38% of rows. It is an ancestor within the path, so it
  is a *third* category column beside the root and the leaf rather than a substitute for either.
- **`source_dataset` is the one `sources` entry with an empty property.** The rest are
  property-level provenance — `/properties/confidence` on every row, `/properties/operating_status`
  on 13% — and counting them makes `Overture` look like the dataset every place came from.

The plan's column table names `country`, `source_dataset` and `update_time` as though they were
top-level columns of `theme=places`. None of the three is: they live in `addresses[]`, and in
`sources[]` twice over. The expressions below are where that is repaired.
"""

from __future__ import annotations

import argparse
import json
import shutil
import time
from pathlib import Path

import duckdb

from ..common import projection
from ..common.paths import ladder, staged
from ..common.timing import Steps

RUNG = "overture"
VINTAGE = "2026-08-19.0"

#: The subtypes whose division id is lifted out of the lineage into an indexed column, so that the
#: same boundary can be asked for as an attribute membership as well as through the nested layer —
#: which of the two the surface prefers is what the campaign plan wants measured at this rung.
#: `locality` is deliberately not among them: 553,493 values is the nested layer's job.
COLUMN_SUBTYPES = ("country", "region", "county")

#: `theme=places/type=place`, sixteen parts. Streamed one at a time so the join's working set is a
#: part rather than the corpus, and so a failure names the part it failed on.
PLACES = "theme=places/type=place"
DIVISION_AREA = "theme=divisions/type=division_area"
DIVISION = "theme=divisions/type=division"

#: **The join runs finest-first, and each tier sees only what the one before it left unplaced.**
#:
#: Only the *deepest* containing area is wanted — its ancestry comes from the division hierarchy,
#: not from a second polygon — so a place already inside a locality never needs testing against its
#: county, its region or its country, and testing it there is where a naive join spends nearly all
#: of its time. The cost of a polygon is its bounding box (how many points reach a full
#: point-in-polygon test) times its vertex count (what that test costs), and measured over the
#: staged release on 2026-08-28 the three coarse subtypes are almost the whole bill:
#:
#: | tier | polygons | vertices | Σ bbox area |
#: |---|---|---|---|
#: | fine — locality, neighborhood, microhood, macrohood, localadmin | 1,029,095 | 252.1M | **16,652 deg²** |
#: | county | 38,908 | 81.5M | 37,835 deg² |
#: | coarse — region, dependency, country | 4,191 | 35.2M | **148,532 deg²** |
#:
#: The world is 64,800 deg². The coarse tier's boxes cover it 2.3 times over at up to 255,252
#: vertices apiece, against 0.26 of a covering for the fine tier that answers almost every place.
#: Splitting them into separate tables rather than filtering one is deliberate: the join builds its
#: index over the whole build side, so a `subtype IN (…)` predicate would pay for the giants anyway.
JOIN_TIERS = (
    ("fine", ("locality", "neighborhood", "microhood", "macrohood", "localadmin")),
    ("county", ("county",)),
    ("coarse", ("region", "dependency", "country")),
)


def places_parts(src: Path) -> list[Path]:
    parts = sorted((src / PLACES).glob("*.parquet"))
    if not parts:
        raise FileNotFoundError(f"no places parts under {src / PLACES}")
    return parts


def build_areas(con, src: Path, out: Path) -> tuple[int, int]:
    """The polygon side, staged locally once with its lineage already resolved.

    `division_area` carries the geometry and `division_id`; the ancestry lives on `type=division`,
    the point form, as `hierarchies` — which the staged README and the campaign plan both place on
    the polygons. Joining the two here means the spatial join downstream returns a whole path
    rather than a single id, and it happens once for 1.07M polygons instead of once per place.

    **Written to a local parquet before anything is timed.** The polygons are 5.2 GB on the share
    and every places part probes against all of them; leaving them there would put SMB inside the
    join's figure sixteen times over, which is the staging tier's first rule (`../README.md`).

    **No persistent R-tree.** `CREATE INDEX … USING RTREE` on this table costs over an hour and the
    join does not read it: DuckDB's `SPATIAL_JOIN` operator plans as a sequential scan of both
    sides and builds its own index on the build side. Measured 2026-08-28, after an hour of a trial
    run went into exactly that index.

    `hierarchies` is a list of lists — a division may in principle carry several paths, which is
    the polyhierarchy the configuration surface refuses. Measured 2026-08-28: **all 4,658,700
    divisions carry exactly one**, so `[1]` is the whole of it and the count is asserted below
    rather than assumed.
    """
    paths = con.execute(
        f"""SELECT count(*), count(*) FILTER (len(hierarchies) <> 1)
            FROM read_parquet('{src / DIVISION}/*.parquet')"""
    ).fetchone()
    if paths[1]:
        raise SystemExit(
            f"{paths[1]:,} of {paths[0]:,} divisions carry more than one hierarchy path. This "
            f"script reads `hierarchies[1]` as the ancestry, which would silently choose one of "
            f"them; a polyhierarchy needs a ruling, not a subscript."
        )
    staged_areas = out / "areas.parquet"
    if not staged_areas.exists():
        con.execute(
            f"""
            COPY (
                SELECT a.division_id,
                       a.subtype,
                       len(d.hierarchies[1])                                AS depth,
                       list_transform(d.hierarchies[1], h -> h.division_id) AS lineage,
                       list_transform(d.hierarchies[1], h -> h.subtype)     AS lineage_subtypes,
                       a.geometry                                          AS geom
                FROM read_parquet('{src / DIVISION_AREA}/*.parquet') a
                LEFT JOIN read_parquet('{src / DIVISION}/*.parquet') d ON d.id = a.division_id
                WHERE a.is_land
            ) TO '{staged_areas}' (FORMAT parquet, COMPRESSION zstd)
            """
        )
    # Straight from the parquet into one table per tier, and no master table: `areas` whole is
    # ~11 GB resident, and materialising it beside its own partition doubles that for nothing.
    for tier, subtypes in JOIN_TIERS:
        con.execute(
            f"""CREATE OR REPLACE TABLE areas_{tier} AS
                SELECT division_id, subtype, depth, lineage, lineage_subtypes, geom
                FROM read_parquet('{staged_areas}')
                WHERE subtype IN ({", ".join(f"'{s}'" for s in subtypes)})"""
        )
    # An area whose division is absent has no ancestry to contribute and would place its points in
    # no artifact at all. Reported rather than refused: it is the publisher's join, not ours, and a
    # handful of them costs those points their boundary and nothing else.
    return con.execute(
        f"""SELECT count(*), count(*) FILTER (lineage IS NULL)
            FROM read_parquet('{staged_areas}')"""
    ).fetchone()


def build_divisions_lookup(con, src: Path) -> None:
    """Every division's name and subtype, for naming the artifacts a lineage names."""
    con.execute(
        f"""
        CREATE OR REPLACE TABLE divisions AS
        SELECT id AS division_id, subtype, country, names.primary AS name
        FROM read_parquet('{src / DIVISION}/*.parquet')
        """
    )


PLACE_COLUMNS = """
    id                                                          AS gers_id,
    ST_X(geometry)                                              AS lon,
    ST_Y(geometry)                                              AS lat,
    geometry                                                    AS geom,
    names.primary                                               AS name,
    taxonomy.hierarchy                                          AS category_path,
    taxonomy.hierarchy[1]                                       AS category_root,
    taxonomy.primary                                            AS category,
    basic_category                                              AS basic_category,
    confidence::FLOAT                                           AS confidence,
    operating_status                                            AS operating_status,
    addresses[1].country                                        AS country,
    -- The feature-level source entry, which is the one whose `property` is empty. Exactly one per
    -- place, asserted at the end of the run over the whole corpus.
    list_filter(sources, s -> s.property IS NULL OR s.property = '')[1].dataset  AS source_dataset,
    try_cast(
        list_filter(sources, s -> s.property IS NULL OR s.property = '')[1].update_time
        AS TIMESTAMP)                                           AS update_time
"""


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--frame", choices=("unit", "metres"), default="unit")
    ap.add_argument("--vintage", default=VINTAGE)
    ap.add_argument("--parts", type=int, default=None, help="read only the first N places parts")
    ap.add_argument(
        "--keep-scratch",
        action="store_true",
        help="leave prepare.duckdb in place; it holds the polygons and is ~11 GB",
    )
    # **The join is half an hour and everything after it is five minutes**, so a failure past the
    # join — running out of disk while writing the outputs is the one that happened — should not
    # cost the join again. `placed` is the join's whole result; this skips straight to it.
    ap.add_argument(
        "--resume",
        action="store_true",
        help="continue from an existing prepare.duckdb whose join has already completed",
    )
    args = ap.parse_args()

    src, out = staged(RUNG, args.vintage), ladder(RUNG)
    ext = projection.extent(args.frame)
    steps = Steps()
    parts = places_parts(src)[: args.parts]
    print(f"source  {src}\noutput  {out}\nframe   {args.frame} — extent [{ext.min}, {ext.max}]")
    print(f"places  {len(parts)} of {len(places_parts(src))} parts\n")

    scratch = out / "prepare.duckdb"
    if not args.resume:
        for stale in (scratch, Path(f"{scratch}.wal")):
            stale.unlink(missing_ok=True)
    elif not scratch.exists():
        raise SystemExit(f"--resume needs an existing {scratch}, and there is none")
    con = duckdb.connect(str(scratch))
    con.execute("PRAGMA enable_progress_bar=false")
    con.execute(f"PRAGMA temp_directory='{out / 'tmp'}'")
    con.execute("INSTALL spatial; LOAD spatial")

    x_expr, y_expr = projection.sql("lon", "lat", args.frame)

    if args.resume:
        # The polygons are not reloaded: nothing past the join reads a geometry, and `divisions` —
        # which the vocabularies and the artifact names do read — is in the scratch already.
        placed_rows = con.execute("SELECT count(*) FROM placed").fetchone()[0]
        areas, orphans = con.execute(
            f"""SELECT count(*), count(*) FILTER (lineage IS NULL)
                FROM read_parquet('{out / "areas.parquet"}')"""
        ).fetchone()
        # Which tier placed a point is recoverable rather than remembered: the last entry of a
        # lineage *is* the division that placed it, so its subtype names the tier.
        tier_of = {sub: tier for tier, subtypes in JOIN_TIERS for sub in subtypes}
        tier_counts = {tier: 0 for tier, _ in JOIN_TIERS}
        for subtype, n in con.execute(
            "SELECT lineage_subtypes[-1], count(*) FROM placed WHERE lineage IS NOT NULL GROUP BY 1"
        ).fetchall():
            tier_counts[tier_of[subtype]] += n
        print(f"resuming from {placed_rows:,} joined places in {scratch.name}\n")
    else:
        with steps.step("divisions"):
            areas, orphans = build_areas(con, src, out)
            build_divisions_lookup(con, src)
            print(
                f"  {areas:,} land areas indexed"
                + (f", {orphans:,} with no division" if orphans else "")
            )

    if not args.resume:
        con.execute(
            """
            CREATE OR REPLACE TABLE placed (
                gers_id VARCHAR, x DOUBLE, y DOUBLE, lat DOUBLE,
                name VARCHAR, category_path VARCHAR[], category_root VARCHAR,
                category VARCHAR, basic_category VARCHAR, confidence FLOAT,
                operating_status VARCHAR, country VARCHAR, source_dataset VARCHAR,
                update_time TIMESTAMP, lineage VARCHAR[], lineage_subtypes VARCHAR[],
                containing UINTEGER
            )
            """
        )

        # --- the join, one part at a time, finest tier first ---------------------------------------
        # `arg_max(.., depth)` takes the most specific containing area *within a tier*, and its lineage
        # is the whole ancestry. A place placed by a tier is dropped from what the next one sees, so
        # the coarse polygons are only ever tested against the residue. A place inside no polygon at
        # all keeps a null lineage — in no artifact, counted rather than refused, which is what a
        # member list of nulls means.
        #
        # ⊘ Tier order is by subtype, and the hierarchy's depth is not strictly a function of subtype:
        # one division in 4.66M is a `locality` sitting at path depth 1, above counties that sit at 3.
        # A place inside both would take the locality, which is one artifact's worth of a shallower
        # answer than the deepest-first rule would give. Counted at the end of the run, not corrected.
        tier_counts = {tier: 0 for tier, _ in JOIN_TIERS}
        with steps.step("join"):
            for i, part in enumerate(parts):
                started = time.monotonic()
                con.execute(
                    f"CREATE OR REPLACE TEMP TABLE part AS SELECT {PLACE_COLUMNS} FROM read_parquet('{part}')"
                )
                con.execute("CREATE OR REPLACE TEMP TABLE hits (gers_id VARCHAR, lineage VARCHAR[], "
                            "lineage_subtypes VARCHAR[], containing UINTEGER)")
                con.execute("CREATE OR REPLACE TEMP TABLE todo AS SELECT gers_id, geom FROM part")
                timings = []
                for tier, _ in JOIN_TIERS:
                    at = time.monotonic()
                    con.execute(
                        f"""
                        INSERT INTO hits
                        SELECT t.gers_id,
                               arg_max(a.lineage, a.depth)           AS lineage,
                               arg_max(a.lineage_subtypes, a.depth)  AS lineage_subtypes,
                               count(*)::UINTEGER                    AS containing
                        FROM todo t JOIN areas_{tier} a ON ST_Within(t.geom, a.geom)
                        GROUP BY t.gers_id
                        """
                    )
                    # `hits` accumulates across tiers, so this tier placed the difference.
                    cumulative = con.execute("SELECT count(*) FROM hits").fetchone()[0]
                    con.execute("DELETE FROM todo WHERE gers_id IN (SELECT gers_id FROM hits)")
                    left = con.execute("SELECT count(*) FROM todo").fetchone()[0]
                    tier_counts[tier] += cumulative - (timings[-1][1] if timings else 0)
                    timings.append((tier, cumulative, left, time.monotonic() - at))
                con.execute(
                    f"""
                    INSERT INTO placed
                    SELECT p.gers_id, {x_expr}, {y_expr}, p.lat,
                           p.name, p.category_path, p.category_root, p.category, p.basic_category,
                           p.confidence, p.operating_status, p.country, p.source_dataset,
                           p.update_time,
                           h.lineage, h.lineage_subtypes, coalesce(h.containing, 0)
                    FROM part p LEFT JOIN hits h USING (gers_id)
                    """
                )
                n = con.execute("SELECT count(*) FROM part").fetchone()[0]
                trail = "  ".join(f"{tier} {dt:.0f}s→{left:,} left" for tier, _, left, dt in timings)
                print(
                    f"  part {i:02d}  {n:>9,} places  {time.monotonic() - started:>6.0f}s   {trail}",
                    flush=True,
                )
            for table in ("part", "hits", "todo"):
                con.execute(f"DROP TABLE IF EXISTS {table}")
            # The polygons are done with, and they are the biggest thing in the scratch database —
            # ~11 GB across the three tiers. Nothing below this line reads a geometry.
            for tier, _ in JOIN_TIERS:
                con.execute(f"DROP TABLE IF EXISTS areas_{tier}")

    # --- entity ids, in ingest order ----------------------------------------------------------
    # `update_time` rather than the GERS id, so that replaying the corpus in id order *is* the
    # time-ordered ingest the campaign plan asks each rung for. Ties break on the id so the order
    # is total and the run is reproducible.
    with steps.step("entity ids"):
        con.execute(
            f"""
            CREATE OR REPLACE TABLE points AS
            SELECT row_number() OVER (ORDER BY update_time, gers_id) - 1 AS entity_id, *,
                   {", ".join(
                       f"list_extract(lineage, list_position(lineage_subtypes, '{s}'))"
                       f" AS division_{s}" for s in COLUMN_SUBTYPES)}
            FROM placed
            """
        )
        kept = con.execute("SELECT count(*) FROM points").fetchone()[0]
        # `points` is `placed` plus the entity id and the three lifted columns, so holding both is
        # two copies of the corpus in the scratch database for no reader.
        con.execute("DROP TABLE placed")

    clipped = con.execute(
        f"""SELECT count(*) FILTER (lat > {projection.MAX_LATITUDE!r}),
                   count(*) FILTER (lat < {-projection.MAX_LATITUDE!r}) FROM points"""
    ).fetchone()

    with steps.step("points.parquet"):
        con.execute(
            f"""COPY (
                SELECT entity_id, x, y, category_root, category, basic_category, country,
                       source_dataset, operating_status, confidence, update_time, name,
                       {", ".join(f"division_{s}" for s in COLUMN_SUBTYPES)}
                FROM points ORDER BY entity_id
            ) TO '{out / "points.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
        )

    # --- vocabularies -------------------------------------------------------------------------
    # Codes are dense from 1: code 0 is reserved for an absent value (`per-point-attributes.md`
    # §5), so a vocabulary starting at 0 would collide with absence on its first key. Every one of
    # these is minted from the corpus rather than from a published file — Overture publishes the
    # taxonomy but not as a file in the staged release — which is why none of them is `public`.
    plain = {
        "category-root": "category_root",
        "category": "category",
        "basic-category": "basic_category",
        "country": "country",
        "source-dataset": "source_dataset",
        "operating-status": "operating_status",
    }
    vocabularies = {
        name: f"""
            SELECT key, row_number() OVER (ORDER BY key) AS code, key AS title
            FROM (SELECT DISTINCT {column} AS key FROM points WHERE {column} IS NOT NULL)"""
        for name, column in plain.items()
    }
    # A division column's key is a GERS uuid and its title is the division's own name, which is the
    # only thing a viewer can read. Without it the vocabulary listing is 40,000 uuids.
    for subtype in COLUMN_SUBTYPES:
        vocabularies[f"division-{subtype}"] = f"""
            SELECT key, row_number() OVER (ORDER BY key) AS code, title FROM (
                SELECT DISTINCT p.division_{subtype} AS key,
                       coalesce(any_value(d.name), p.division_{subtype}) AS title
                FROM points p LEFT JOIN divisions d ON d.division_id = p.division_{subtype}
                WHERE p.division_{subtype} IS NOT NULL
                GROUP BY p.division_{subtype}
            )"""

    sizes = {}
    with steps.step("vocabularies"):
        for name, query in vocabularies.items():
            path = out / f"vocab-{name}.parquet"
            con.execute(f"COPY ({query}) TO '{path}' (FORMAT parquet)")
            sizes[name] = con.execute(f"SELECT count(*) FROM '{path}'").fetchone()[0]

    # --- member files -------------------------------------------------------------------------
    # `members-taxonomy`: a fixed six-element list, one entry per declared level, nulls past the
    # leaf. The path is a clean tree — measured 2026-08-28, every category appears at exactly one
    # depth and has exactly one parent — which is what makes `tiered` legal here and not below.
    #
    # `members-divisions`: a variable-length lineage, entry k the parent of entry k+1. `nested`
    # refuses `[[layer.levels]]` and a fixed-size list, so the two member files are deliberately
    # different shapes over the same points.
    with steps.step("member files"):
        con.execute(
            f"""COPY (
                SELECT entity_id::UBIGINT AS entity,
                       [category_path[1], category_path[2], category_path[3],
                        category_path[4], category_path[5], category_path[6]] AS key
                FROM points ORDER BY entity_id
            ) TO '{out / "members-taxonomy.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
        )
        con.execute(
            f"""COPY (
                SELECT entity_id::UBIGINT AS entity, lineage AS key
                FROM points ORDER BY entity_id
            ) TO '{out / "members-divisions.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
        )

    # --- the divisions layer: names, declared edges and geometry ------------------------------
    # One row per artifact a place's lineage names, carrying the division's own name — the layer
    # declares a supplied `name` and the build refuses an artifact that carries none — its
    # **parent** as the division's own hierarchy declares it, and its **polygon** as WKB in a
    # `geometry` column, GeoParquet's name and the one `[layer.shape]` reads by default.
    #
    # **The membership is the polygon now, and the lineage is the tree** (`polygon-membership.md`
    # §6.1, §6.2): the layer is declared `membership = "spatial"`, so the service resolves which
    # places each division holds from the geometry, and `members-divisions.parquet` is no longer
    # what the layer reads. The offline join above stays: it lifts `division_country`,
    # `division_region` and `division_county` into indexed columns, and its lineage is what names
    # the roster. The edges are **declared**, from each division's own path, never derived from
    # containment — the polygons are generalised for cartography and do not nest reliably, and
    # the build reports the children whose bounds escape their parent's rather than checking them.
    #
    # The geometry is projected into the frame the points are in — Web Mercator, normalised, y
    # south, exactly `projection.project` — so the shape is written in the view's own space
    # (`space = "view"`), which is the only space the build honours. Latitude is clipped first, as
    # the points' is: a vertex beyond ±MAX_LATITUDE would project outside the plane.
    with steps.step("artifacts"):
        roster = "(SELECT DISTINCT unnest(lineage) AS key FROM points WHERE lineage IS NOT NULL)"
        w = projection.WORLD_HALF_M
        clip = (
            f"ST_Intersection(a.geom, ST_MakeEnvelope(-180.0, -{projection.MAX_LATITUDE!r}, "
            f"180.0, {projection.MAX_LATITUDE!r}))"
        )
        mercator = f"ST_Transform({clip}, 'EPSG:4326', 'EPSG:3857', true)"
        projected = (
            f"ST_Affine({mercator}, {1.0 / (2.0 * w)!r}, 0.0, 0.0, {-1.0 / (2.0 * w)!r}, 0.5, 0.5)"
            if args.frame == "unit"
            else f"ST_Affine({mercator}, 1.0, 0.0, 0.0, -1.0, 0.0, 0.0)"
        )
        con.execute(
            f"""COPY (
                SELECT k.key,
                       [[coalesce(d.name, k.key)]] AS contents,
                       CASE WHEN len(l.lineage) >= 2 AND l.lineage[-2] IN (SELECT key FROM {roster})
                            THEN l.lineage[-2] END                       AS parent,
                       ({projected})::WKB_BLOB                            AS geometry
                FROM {roster} k
                LEFT JOIN divisions d ON d.division_id = k.key
                LEFT JOIN (
                    SELECT division_id, any_value(lineage) AS lineage
                    FROM read_parquet('{out / "areas.parquet"}') GROUP BY division_id
                ) l ON l.division_id = k.key
                LEFT JOIN (
                    SELECT division_id, ST_Union_Agg(geom) AS geom
                    FROM read_parquet('{out / "areas.parquet"}') GROUP BY division_id
                ) a ON a.division_id = k.key
                ORDER BY k.key
            ) TO '{out / "artifacts-divisions.parquet"}' (FORMAT parquet, COMPRESSION zstd)"""
        )
        artifacts, named = con.execute(
            f"""SELECT count(*), count(*) FILTER (contents[1][1] <> key)
                FROM '{out / "artifacts-divisions.parquet"}'"""
        ).fetchone()
        with_geometry, with_parent = con.execute(
            f"""SELECT count(*) FILTER (geometry IS NOT NULL), count(*) FILTER (parent IS NOT NULL)
                FROM '{out / "artifacts-divisions.parquet"}'"""
        ).fetchone()
        print(f"  artifacts {artifacts:,}: {with_geometry:,} with a polygon, {with_parent:,} with a parent")

    # --- the frame report ---------------------------------------------------------------------
    bounds = con.execute("SELECT min(x), max(x), min(y), max(y) FROM points").fetchone()
    unplaced = con.execute("SELECT count(*) FROM points WHERE lineage IS NULL").fetchone()[0]
    depth_hist = con.execute(
        "SELECT len(lineage) d, count(*) c FROM points WHERE lineage IS NOT NULL GROUP BY 1 ORDER BY 1"
    ).fetchall()
    containing_hist = con.execute(
        "SELECT containing, count(*) c FROM points GROUP BY 1 ORDER BY 1"
    ).fetchall()
    frame = {
        "frame": args.frame,
        "extent": {"min": ext.min, "max": ext.max},
        "projection": "web_mercator",
        "y_direction": "south",
        "asked_for_wgs84": projection.WORLD_BOX_WGS84,
        "clipped": {"north": clipped[0], "south": clipped[1], "total": sum(clipped)},
        "data_bounds": {"x": [bounds[0], bounds[1]], "y": [bounds[2], bounds[3]]},
        "points": kept,
        "parts_read": len(parts),
        "division_areas": areas,
        "division_artifacts": artifacts,
        "places_in_no_division": unplaced,
        "lineage_depth": {str(d): c for d, c in depth_hist},
        "containing_areas": {str(n): c for n, c in containing_hist},
        "placed_by_tier": tier_counts,
        "source": str(src),
        "vintage": args.vintage,
        "steps": dict(steps),
    }

    # --- report -------------------------------------------------------------------------------
    print(f"\npoints        {kept:,}")
    print(f"extent        [{ext.min}, {ext.max}] — {args.frame}, y south")
    print(f"data bounds   x [{bounds[0]:.6f}, {bounds[1]:.6f}]  y [{bounds[2]:.6f}, {bounds[3]:.6f}]")
    print(
        f"clipped       {sum(clipped):,} beyond ±{projection.MAX_LATITUDE:.4f}° "
        f"({clipped[0]:,} north, {clipped[1]:,} south) — at the frame edge, not clamped"
    )
    print(f"\ndivision artifacts  {artifacts:,}, of which {named:,} carry a published name")
    print(f"places in no division {unplaced:,} ({unplaced * 100.0 / kept:.2f}%)")
    print("lineage depth")
    for d, c in depth_hist:
        print(f"  {d}  {c:>12,}  ({c * 100.0 / kept:5.1f}%)")
    print("containing areas per place, within the tier that placed it")
    for n, c in containing_hist:
        print(f"  {n}  {c:>12,}")
    print("placed by tier")
    for tier, c in tier_counts.items():
        print(f"  {tier:<8} {c:>12,}  ({c * 100.0 / kept:5.1f}%)")

    print("\nvocabularies")
    for name, size in sizes.items():
        print(f"  {name:<20} {size:>9,}")

    # **Every numeric's measured range, printed whether or not anything is wrong.** A width is
    # declared from these; a declaration written from a maximum alone is how the GeoNames rung came
    # to declare `population` as `u64` against a column holding -12.
    print("\nnumeric ranges — the widths in the declaration are written from these")
    for column in ("confidence", "entity_id"):
        lo, hi = con.execute(f"SELECT min({column}), max({column}) FROM points").fetchone()
        print(f"  {column:<16} [{lo}, {hi}]")
    lo, hi = con.execute("SELECT min(update_time), max(update_time) FROM points").fetchone()
    print(f"  {'update_time':<16} [{lo}, {hi}]")

    print("\nnull counts")
    columns = (
        "category_root category basic_category country source_dataset operating_status "
        "confidence update_time name"
    ).split() + [f"division_{s}" for s in COLUMN_SUBTYPES]
    nulls = con.execute(
        f"SELECT {', '.join(f'count(*) - count({c})' for c in columns)} FROM points"
    ).fetchone()
    for name, n in zip(columns, nulls):
        print(f"  {name:<20} {n:>12,}  ({n * 100.0 / kept:5.1f}%)")

    # The feature-level source entry is asserted here rather than trusted: the whole `source_dataset`
    # column is that one entry, and a place carrying two of them would silently take whichever came
    # first. Measured on one part at the survey; checked over the corpus at every run.
    if nulls[columns.index("source_dataset")]:
        print(
            f"  ⊘ {nulls[columns.index('source_dataset')]:,} places carry no source entry with an "
            f"empty property — `source_dataset` is null there, and the declaration's default applies"
        )

    (out / "frame.json").write_text(json.dumps(frame, indent=2) + "\n")

    for path in sorted(out.glob("*.parquet")):
        print(f"  {path.name:<32} {path.stat().st_size / 1e6:>9.1f} MB")

    # **The demo's candidate terms, ranked by coverage** — `run_demo.sh --ranks` composes the
    # sparse/medium/heavy principals from this. Same shape as the GeoNames rung's.
    ranks = con.execute(
        """SELECT country AS term, count(*) AS pairs FROM points
           WHERE country IS NOT NULL GROUP BY 1 ORDER BY pairs DESC"""
    ).fetchall()
    (out / "country-ranks.json").write_text(
        json.dumps([{"term": t, "pairs": n} for t, n in ranks], indent=None) + "\n"
    )
    (out / "country-terms.txt").write_text(",".join(t for t, _ in ranks) + "\n")
    print(f"\nwrote country-ranks.json and country-terms.txt ({len(ranks)} terms)")
    print(f"  top five: {', '.join(f'{t} {n:,}' for t, n in ranks[:5])}")

    # The declaration is copied rather than referenced: `[sources]` is relative to the declaring
    # document (`configuration.md` §3), so the copy beside the parquets is what resolves.
    here = Path(__file__).parent
    shutil.copy(here / "corpus.toml", out / "corpus.toml")

    (out / "tessera.toml").write_text(
        f"""# Generated by `test_corpora/overture/prepare.py`. Machine-specific by construction: paths
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
session_credential_env  = "TESSERA_OVERTURE_SESSION_CRED"
operator_credential_env = "TESSERA_OVERTURE_OPERATOR_CRED"
# Development only — the origins the demo viewer is served from, so a browser on this machine can
# talk to this process (client-interaction §7). The list is enumerated: a wildcard is refused.
dev_cors_origins        = ["http://localhost:5173", "http://127.0.0.1:5173"]
"""
    )

    con.close()
    if not args.keep_scratch:
        scratch.unlink(missing_ok=True)
        Path(f"{scratch}.wal").unlink(missing_ok=True)
    print(f"\ntotal {steps.total()}s — " + "  ".join(f"{k} {v}s" for k, v in steps.items()))


if __name__ == "__main__":
    main()
