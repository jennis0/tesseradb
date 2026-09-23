"""Two corpus declarations, regenerated through the typed verbs and checked.

`test_corpora/` holds the declarations the demo corpora were built from. Two of them reach a
declaration shape nothing else here does: `multiview` carries both roster forms, a group-scoped
column, a spatial layer with an authored roster and an artifact-visibility column, and
`treeoflife` a tiered taxonomy beside a predicate layer over a category column. Each is declared
through the typed verbs, given a placeholder table for every file it names, and handed to the
declaration check: what is asserted is that the check accepts what the verbs wrote.

The placeholders carry the columns their target reads, typed as the declaration types them, and
no values: the corpora's own files are not on this machine and no row is read by a check.
"""

from pathlib import Path

import pytest

from tesseradb._database import create

pytest.importorskip("pyarrow")

WORLD = {"lon": [-180.0, 180.0], "lat": [-85.0511287798066, 85.0511287798066]}

#: What a declared attribute's column is written as, where the placeholder carries one.
ARROW = {
    "i64": "int64", "u16": "uint16", "u8": "uint8", "f32": "float", "f64": "double",
    "timestamp_us": "timestamp[us]",
}


def parquet(db, name: str, columns) -> str:
    """A one-row table under the columns named, each typed as the declaration types it."""
    import pyarrow as pa
    import pyarrow.parquet as pq

    declared = {
        block["name"]: block["type"] for block in db.blocks.blocks["attribute"]
    }
    arrays = {}
    for column in columns:
        if column in ("entity_id", "entity"):
            arrays[column] = pa.array([0], pa.int64())
        elif column in ("x", "y", "lon", "lat"):
            arrays[column] = pa.array([0.0], pa.float64())
        elif column in declared and declared[column] in ARROW:
            arrays[column] = pa.array([0], pa.type_for_alias(ARROW[declared[column]]))
        else:
            arrays[column] = pa.array(["a"], pa.string())
    db.files.mkdir(parents=True, exist_ok=True)
    path = db.files / f"{name}.parquet"
    pq.write_table(pa.table(arrays), path)
    return str(path)


def database(tmp_path: Path):
    db = create(tmp_path / "db")
    db.files = tmp_path / "files"
    return db


def points(db, view: str, columns, file: str | None = None) -> None:
    """One view's own insert: its geometry, its labels, its id, and every column it fills."""
    attributes = [
        block["name"] for block in db.blocks.blocks["attribute"] if not block.get("scope")
    ]
    named = {"id": "entity_id"}
    named["lon" if "lon" in columns else "x"] = "lon" if "lon" in columns else "x"
    named["lat" if "lat" in columns else "y"] = "lat" if "lat" in columns else "y"
    access = [one for one in columns if one not in ("entity_id", "x", "y", "lon", "lat")]
    if access:
        named["access"] = access[0]
    every = list(dict.fromkeys(list(columns) + attributes))
    name = (file or f"{view}_points").replace("/", "_")
    db.insert(view, parquet(db, name, every), **named)


def artifacts(
    db, layer: str, columns=("key",), view: str | None = None, access: str | None = None
) -> None:
    named = {"key": "key"}
    if view is not None:
        named["view"] = view
    if access is not None:
        named["access"] = access
    db.insert(
        layer,
        artifacts=parquet(db, f"{layer}_artifacts".replace("/", "_"), columns),
        **named,
    )


def members(db, layer: str) -> None:
    db.insert(
        layer,
        members=parquet(db, f"{layer}_members".replace("/", "_"), ("key", "entity")),
        id="entity",
        key="key",
    )


def values(db, vocabulary: str) -> None:
    db.insert(vocabulary, parquet(db, f"{vocabulary}_values", ("key",)), key="key")


def accepts(db) -> None:
    """The declaration the verbs wrote, through the check the build and the SDK both run."""
    report = db.check()
    assert report.ok, report


def test_treeoflife(tmp_path):
    """A tiered taxonomy over two views, beside a predicate layer on a category column."""
    db = database(tmp_path)
    db.declare_view("bioclip", extent="auto", default_label="unpublished",
                    title="Specimen map")
    db.declare_view("geo", projection="web_mercator", extent=WORLD,
                    default_label="unpublished", title="Where it was recorded")
    # The corpus's own file leaves these to the run, which declares one value set per rank.
    for name in ("kingdom", "phylum", "class", "order", "family", "genus", "species",
                 "publisher", "source_dataset", "basis", "img_type"):
        db.declare_vocabulary(name, width="u32")
    for name, vocabulary, title in [
        ("kingdom", "kingdom", "Kingdom"), ("phylum", "phylum", "Phylum"),
        ("class", "class", "Class"), ("order", "order", "Order"),
        ("family", "family", "Family"), ("genus", "genus", "Genus"),
        ("species", "species", "Species"),
    ]:
        db.declare_attribute(name, type="category", vocabulary=vocabulary, render=True,
                             title=title)
    db.declare_attribute("publisher", type="category", vocabulary="publisher", render=True,
                         index=True, title="Publisher")
    db.declare_attribute("source_dataset", type="category", vocabulary="source_dataset",
                         render=True, index=True, title="Source dataset")
    db.declare_attribute("basisOfRecord", type="category", vocabulary="basis", render=True,
                         title="Basis of record")
    db.declare_attribute("img_type", type="category", vocabulary="img_type", render=True,
                         title="Image type")
    db.declare_attribute("scientific_name", type="keyword", index=True, title="Scientific name")
    db.declare_attribute("common_name", type="text", index=True, title="Common name")
    db.declare_attribute("uuid", type="keyword", index=True, title="TreeOfLife uuid")
    db.declare_layer(
        "taxonomy/tree",
        kind="tiered",
        views=["bioclip", "geo"],
        value_set="open",
        prune_children=True,
        require_member_visibility={"count": 1},
        levels=[(0, "Kingdom", [0, 3]), (1, "Phylum", [2, 5]), (2, "Class", [4, 7]),
                (3, "Order", [6, 9]), (4, "Family", [8, 11]), (5, "Genus", [10, 13]),
                (6, "Species", [12, 16])],
        computed=("centroid", "box"),
        title="Taxonomy",
    )
    db.declare_layer(
        "publishers/source",
        kind="flat",
        views=["bioclip", "geo"],
        membership={"attribute": "publisher"},
        require_member_visibility={"count": 1},
        title="Publishing institution",
    )
    points(db, "bioclip", ("entity_id", "x", "y", "publisher"))
    points(db, "geo", ("entity_id", "lon", "lat", "publisher"))
    members(db, "taxonomy/tree")
    accepts(db)


def test_multiview(tmp_path):
    """Every block of this corpus through the typed verbs, the view groups included.

    `quarter` gives each view its own file, so each is inserted as its own table naming the one
    view it is for with `view_key=`, and the roster insert carries the metadata each record
    holds. `quarter_alt` shares its keys and takes one file with a discriminator column, named
    with `view=`.
    """
    import datetime as dt

    import pyarrow as pa
    import pyarrow.parquet as pq

    db = database(tmp_path)
    db.declare_view("world", projection="web_mercator", extent=WORLD, title="Whole corpus")
    db.declare_view("world_flat", projection="equirectangular", extent=WORLD,
                    title="Whole corpus, equirectangular")
    db.declare_view_group(
        "quarter",
        title="By quarter",
        extent={"x": [-40.0, 40.0], "y": [-40.0, 40.0]},
        metadata={"label": "text", "starts": "timestamp_us", "ends": "timestamp_us"},
    )
    db.declare_view_group(
        "quarter_alt",
        title="By quarter, geographic",
        members="quarter",
        projection="web_mercator",
        extent=WORLD,
    )

    db.declare_vocabulary("kind", closed=True, width="u8", title="Feature kind")
    db.declare_attribute("importance", type="i64", index=True)
    db.declare_attribute("kind", type="category", vocabulary="kind", index=True, render=True)
    db.declare_attribute("sentiment", type="f32", scope={"group": "quarter"}, index=True,
                         render=True)
    db.declare_vocabulary("mood", closed=True, width="u8", visibility="derived",
                          values=["calm", "tense", "wild", "still"], title="Mood")
    db.declare_attribute("mood", type="category", vocabulary="mood",
                         scope={"group": "quarter"}, index=True)
    db.declare_attribute("note", type="text", scope={"group": "quarter"}, index=True)
    db.declare_attribute("coverage", type="f32", scope={"group": "quarter"}, index=True)

    db.declare_layer(
        "collections",
        kind="flat",
        views=["world", "quarter"],
        require_member_visibility="all",
        computed=(),
        supplied=[("tag", "text", "inherited")],
        title="Curated collections",
    )
    db.declare_layer(
        "quarter_clusters",
        kind="flat",
        views=["quarter", "quarter_alt"],
        scope={"group": "quarter"},
        require_member_visibility="any",
        computed=(),
        supplied=[("tag", "text", "inherited")],
        title="Clusters by quarter",
    )
    db.declare_layer(
        "regions",
        kind="flat",
        views=["world", "world_flat"],
        membership="spatial",
        shape={"kind": "polygon"},
        require_member_visibility="none",
        computed=(),
        artifacts=[
            {"key": "europe_nw", "wkt": "POLYGON ((-11 49, 3 49, 3 61, -11 61, -11 49))",
             "space": "wgs84"},
            {"key": "iberia", "wkt": "POLYGON ((-10 36, 3 36, 3 44, -10 44, -10 36))",
             "space": "wgs84"},
            {"key": "japan", "wkt": "POLYGON ((129 31, 146 31, 146 46, 129 46, 129 31))",
             "space": "wgs84"},
        ],
        title="Regions",
    )

    # The two plain views, then each quarter's own file, then the shared group's one file.
    # One file for the two plain views, as the corpus has it: a second projection over the same
    # points is a second view reading the same rows.
    points(db, "world", ("entity_id", "lon", "lat", "access"), file="world")
    points(db, "world_flat", ("entity_id", "lon", "lat", "access"), file="world")
    quarters = ["2026-Q1", "2026-Q2", "2026-Q3", "2026-Q4"]
    starts = [dt.datetime(2026, q, 1, tzinfo=dt.timezone.utc) for q in (1, 4, 7, 10)]
    ends = starts[1:] + [dt.datetime(2027, 1, 1, tzinfo=dt.timezone.utc)]
    roster = tmp_path / "files" / "quarter_roster.parquet"
    roster.parent.mkdir(parents=True, exist_ok=True)
    pq.write_table(
        pa.table(
            {
                "key": pa.array(quarters, pa.string()),
                "label": pa.array([f"Q{i + 1} 2026" for i in range(4)], pa.string()),
                "starts": pa.array(starts, pa.timestamp("us", tz="UTC")),
                "ends": pa.array(ends, pa.timestamp("us", tz="UTC")),
            }
        ),
        roster,
    )
    db.insert("quarter", roster=str(roster), key="key", label="label", starts="starts",
              ends="ends")
    # Each quarter's own file carries the group-scoped columns read from it beside its points.
    scoped = ("sentiment", "mood", "note")
    for at, key in enumerate(quarters):
        db.insert(
            "quarter",
            parquet(db, f"quarter_2026_q{at + 1}", ("entity_id", "x", "y", "access", *scoped)),
            id="entity_id",
            x="x",
            y="y",
            access="access",
            view_key=key,
        )
    db.insert(
        "quarter_alt",
        parquet(db, "quarter_alt_pts", ("entity_id", "lon", "lat", "access", "quarter", *scoped)),
        id="entity_id",
        lon="lon",
        lat="lat",
        access="access",
        view="quarter",
    )
    # `kind` reads its keys from a file; `mood` carries its four inline, so it takes no insert.
    values(db, "kind")
    constants = parquet(db, "attrs_constant", ("entity_id", "importance", "kind"))
    for name in ("importance", "kind"):
        db.insert(name, constants, id="entity_id", value=name)
    # `coverage` is the one scoped column with a source of its own; the other three are read
    # from each view's own points file, so they are declared and take no insert.
    db.insert(
        "coverage",
        parquet(db, "attrs_scoped", ("entity_id", "quarter", "coverage")),
        id="entity_id",
        value="coverage",
        view="quarter",
    )
    artifacts(db, "collections", ("key", "access"), access="access")
    artifacts(db, "quarter_clusters", ("key", "quarter"), view="quarter")
    accepts(db)
