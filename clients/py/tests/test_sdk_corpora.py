"""Every corpus declaration under `test_corpora/`, regenerated through the typed verbs.

The proof python-sdk.md §12 asks for is `tessera check` over the regenerated file printing what the
committed file prints. The files these declarations name are not on this machine, so `check` cannot
read them and the proof here is the declaration itself: each corpus is built through the typed
verbs, written, parsed, and compared with the committed `corpus.toml` block by block and key by
key. `test_sdk_corpus.py` carries the two corpora whose files can be present, where the comparison
is the binary's own disclosure table.

Every source is staged as a placeholder parquet carrying the id column, a view's coordinate columns
and its access column. That is what the SDK reads to see where identity is and to decide what to
infer; no other column is staged, so nothing is inferred and every block compared is one a call
below wrote.

**The normalisations.** Four of them fill the committed document in with what §4.8 requires the
SDK to write, so the test fails if the SDK stops writing one; the rest are applied to both
documents and to nothing else.

Filled into the committed document alone:

1. `[defaults].allocation_view` where one view is declared and the file names none.
2. `visibility = "public"` on a view that declares none.
3. `source` on a view or an attribute that names none, which is `[defaults].source`. Not on a
   layer, and not on a group-scoped attribute: `[defaults].source` reaches neither, a scoped
   column with no source of its own being read from each of its group's views' points files.
4. A layer's `value_set` where the file names none, which is `closed`.

Applied to both:

5. `[sources]` is compared by its key set. The committed file names the corpus's own parquets and
   this test names placeholders in a temporary directory.
6. `[defaults].entity_id_field` where it is `entity_id`, the surface's own default.
7. A view's `fields` where each column carries its canonical name.
8. `hierarchy.prune_children` where it is false, the surface's default.
9. A layer's `default_space` where it is `view`, the surface's default.
10. A layer's `[layer.content]` where it carries no computed property and no supplied kind, which
    is the block being absent.

An `extent` is compared as written: `"auto"` and a box are different frames, and so are `"auto"`
and a fitted frame widened by a margin.

A corpus whose committed file carries a `<marker>` a run fills in names that marker's own
normalisation in its own test.
"""

from pathlib import Path

import pytest

from tesseradb._database import create

pytest.importorskip("pyarrow")

try:  # Python 3.11 carries a TOML reader; below it the dev extra supplies one.
    import tomllib as _toml
except ModuleNotFoundError:  # pragma: no cover, taken on 3.10 alone
    _toml = pytest.importorskip("tomli")

CORPORA = Path(__file__).resolve().parents[3] / "test_corpora"

WORLD = {"lon": [-180.0, 180.0], "lat": [-85.0511287798066, 85.0511287798066]}


# ---------------------------------------------------------------------------- the placeholders


def parquet(directory: Path, name: str, columns) -> str:
    """A one-row source file. The SDK reads a source's schema; the corpus's values are not here."""
    import pyarrow as pa
    import pyarrow.parquet as pq

    directory.mkdir(parents=True, exist_ok=True)
    arrays = {}
    for column in columns:
        if column in ("entity_id", "entity"):
            arrays[column] = pa.array([0], pa.int64())
        elif column in ("x", "y", "lon", "lat"):
            arrays[column] = pa.array([0.0], pa.float64())
        else:
            arrays[column] = pa.array(["a"], pa.string())
    path = directory / f"{name}.parquet"
    pq.write_table(pa.table(arrays), path)
    return str(path)


def database(tmp_path: Path, points: dict, tables) -> object:
    """A database with every source of one corpus staged: the points files, then the tables."""
    db = create(tmp_path / "db")
    files = tmp_path / "files"
    for index, (name, columns) in enumerate(points.items()):
        db.stage(name, parquet(files, name, columns), default=index == 0)
    for name in tables:
        db.stage(name, parquet(files, name, ("key",)))
    return db


# ---------------------------------------------------------------------------- the comparison


def committed(corpus: str) -> dict:
    return _toml.loads((CORPORA / corpus / "corpus.toml").read_text())


def generated(db) -> dict:
    return _toml.loads(db.declaration)


def normalised(document: dict, filling: bool = False) -> dict:
    """One document ready to compare. `filling` adds what §4.8 requires the SDK to write."""
    out = dict(document)
    out["sources"] = sorted(document.get("sources", {}))
    defaults = dict(document.get("defaults", {}))
    if defaults.get("entity_id_field") == "entity_id":
        defaults.pop("entity_id_field")
    views = document.get("view", [])
    if filling and len(views) == 1:
        defaults.setdefault("allocation_view", views[0]["name"])
    out["defaults"] = defaults
    out["view"] = [_view(block, defaults, filling) for block in views]
    out["attribute"] = [
        block if block.get("scope") else _sourced(block, defaults, filling)
        for block in document.get("attribute", [])
    ]
    out["layer"] = [_layer(block, filling) for block in document.get("layer", [])]
    return {key: value for key, value in out.items() if value not in ({}, [])}


def _sourced(block: dict, defaults: dict, filling: bool) -> dict:
    block = dict(block)
    if filling and "source" not in block and defaults.get("source"):
        block["source"] = defaults["source"]
    return block


def _view(block: dict, defaults: dict, filling: bool) -> dict:
    block = _sourced(block, defaults, filling)
    fields = block.get("fields")
    if fields and all(name == column for name, column in fields.items()):
        block.pop("fields")
    if filling:
        block.setdefault("visibility", "public")
    return block


def _layer(block: dict, filling: bool) -> dict:
    block = dict(block)
    hierarchy = dict(block.get("hierarchy", {}))
    if hierarchy.get("prune_children") is False:
        hierarchy.pop("prune_children")
    block["hierarchy"] = hierarchy
    if filling:
        block.setdefault("value_set", "closed")
    if block.get("default_space") == "view":
        block.pop("default_space")
    content = block.get("content")
    if content is not None and not content.get("computed") and not content.get("supplied"):
        block.pop("content")
    return block


def same(written: dict, holds: dict) -> None:
    """Block by block, key by key, so a failure names the block and the key that differ."""
    left, right = normalised(written), normalised(holds, filling=True)
    assert sorted(left) == sorted(right), "the kinds of block declared"
    for kind in sorted(left):
        one, other = left[kind], right[kind]
        if not (isinstance(one, list) and one and isinstance(one[0], dict)):
            assert one == other, kind
            continue
        by_name = {block.get("name", index): block for index, block in enumerate(one)}
        theirs = {block.get("name", index): block for index, block in enumerate(other)}
        assert sorted(by_name) == sorted(theirs), f"the {kind} blocks declared"
        for name in sorted(by_name):
            assert sorted(by_name[name]) == sorted(theirs[name]), f"{kind} '{name}': its keys"
            for key in sorted(by_name[name]):
                assert by_name[name][key] == theirs[name][key], f"{kind} '{name}': {key}"


# ---------------------------------------------------------------------------- the corpora


def test_arxiv(tmp_path):
    db = database(
        tmp_path,
        {
            "points": ("entity_id", "x", "y", "categories"),
            "points_pca64": ("entity_id", "x", "y", "categories"),
        },
        ["archive", "primary_category", "kmeans", "kmeans_members", "hdbscan", "hdbscan_members"],
    )
    db.declare_view("knn", source="points", access="categories", extent="auto", title="Topic map")
    db.declare_view(
        "pca64",
        source="points_pca64",
        access="categories",
        extent="auto",
        title="Topic map (PCA-64)",
    )
    db.declare_vocabulary("archive", source="archive", closed=True, width="u8",
                          title="arXiv archive")
    db.declare_vocabulary("primary_category", source="primary_category", closed=True, width="u16",
                          title="arXiv subject class")
    db.declare_attribute("archive", type="category", vocabulary="archive", render=True,
                         index=True, title="Archive")
    db.declare_attribute("primary_category", type="category", vocabulary="primary_category",
                         render=True, index=True, title="Primary category")
    db.declare_attribute("submitted_at", type="timestamp_us", render=True, index=True,
                         title="Submitted")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("abstract", type="text", index=True)
    db.declare_attribute("authors", type="text", index=True, title="Authors")
    db.declare_attribute("arxiv_id", type="keyword", index=True, title="arXiv ID")
    for name, source, members, kind, requirement, title in [
        ("clusters/kmeans", "kmeans", "kmeans_members", "flat", {"count": 50}, "k-means clusters"),
        ("clusters/hdbscan", "hdbscan", "hdbscan_members", "nested", {"fraction": 0.05},
         "HDBSCAN clusters"),
    ]:
        db.declare_layer(name, kind=kind, source=source, members=members, views=["knn", "pca64"],
                         require_member_visibility=requirement,
                         supplied=[("topic", "text", "all")], title=title)
    same(generated(db), committed("arxiv"))


def test_gbif(tmp_path):
    """`# <vocabulary>` is the `kingdom` value set, whose size the run sees: not declared here."""
    db = database(tmp_path, {"points": ("entity_id", "lon", "lat", "countrycode")},
                  ["kingdom", "taxonomy"])
    db.declare_view("geo", source="points", projection="web_mercator", extent=WORLD,
                    access="countrycode", default_label="UNRECORDED",
                    title="Where it was recorded")
    db.declare_attribute("kingdom", type="category", vocabulary="kingdom", render=True,
                         title="Kingdom")
    db.declare_attribute("specieskey", type="keyword", index=True, title="GBIF species key")
    db.declare_attribute("year", type="u16", index=True, title="Year")
    db.declare_attribute("scientificname", type="keyword", title="Scientific name")
    db.declare_layer(
        "taxonomy/tree",
        kind="tiered",
        views=["geo"],
        members="taxonomy",
        value_set="open",
        prune_children=True,
        require_member_visibility={"count": 1},
        levels=[(0, "Family", [0, 5]), (1, "Genus", [4, 10]), (2, "Species", [9, 16])],
        computed=("centroid", "box"),
        title="Taxonomy",
    )
    same(generated(db), committed("gbif"))


def test_geonames(tmp_path):
    db = database(
        tmp_path,
        {"points": ("entity_id", "lon", "lat", "country")},
        ["feature_class", "feature_code", "country", "admin1", "admin2", "admin3", "admin4",
         "timezone", "members_feature", "members_admin", "artifacts_admin"],
    )
    db.declare_view("world", projection="web_mercator", extent=WORLD, access="country",
                    title="GeoNames")
    for name, width, visibility, title in [
        ("feature_class", "u8", "public", "Feature class"),
        ("feature_code", "u16", "public", "Feature code"),
        ("country", "u16", "derived", "Country"),
        ("admin1", "u16", "derived", "First-level administrative division"),
        ("admin2", "u32", "derived", "Second-level administrative division"),
        ("admin3", "u32", "derived", "Third-level administrative division"),
        ("admin4", "u32", "derived", "Fourth-level administrative division"),
        ("timezone", "u16", "derived", "Time zone"),
    ]:
        db.declare_vocabulary(name, source=name, closed=True, width=width, visibility=visibility,
                              title=title)
    for name, render in [
        ("feature_class", True), ("feature_code", True), ("country", True), ("admin1", False),
        ("admin2", False), ("admin3", False), ("admin4", False), ("timezone", False),
    ]:
        db.declare_attribute(name, type="category", vocabulary=name, index=True,
                             render=True if render else None)
    db.declare_attribute("population", type="i64", render=True)
    db.declare_attribute("elevation", type="i16", render=True)
    db.declare_attribute("dem", type="i16", render=True)
    db.declare_attribute("modification_date", type="timestamp_us", render=True)
    db.declare_attribute("name", type="text", index=True)
    db.declare_layer(
        "features/taxonomy",
        kind="tiered",
        views=["world"],
        members="members_feature",
        value_set="open",
        prune_children=True,
        require_member_visibility={"count": 1},
        levels=[(0, "Class", [0, 6]), (1, "Code", [5, 16])],
        computed=(),
        title="Feature taxonomy",
    )
    db.declare_layer(
        "admin/hierarchy",
        kind="tiered",
        source="artifacts_admin",
        members="members_admin",
        views=["world"],
        value_set="open",
        prune_children=True,
        require_member_visibility={"count": 1},
        levels=[(0, "Country", [0, 4]), (1, "Admin 1", [3, 7]), (2, "Admin 2", [6, 10]),
                (3, "Admin 3", [9, 13]), (4, "Admin 4", [12, 16])],
        computed=("centroid", "box"),
        supplied=[("name", "text", "inherited")],
        title="Administrative hierarchy",
    )
    same(generated(db), committed("geonames"))


def test_medcpt(tmp_path):
    """`# <abstract-attribute>` and `# <mesh-layer>` are the run's: neither is declared here."""
    db = database(tmp_path, {"points": ("entity_id", "x", "y", "branches")},
                  ["branch", "kmeans", "kmeans_members", "mesh", "mesh_members"])
    db.declare_view("knn", source="points", extent="auto", access="branches",
                    title="Literature map")
    db.declare_vocabulary("branch", source="branch", closed=True, width="u8", title="MeSH branch")
    db.declare_attribute("published", type="timestamp_us", render=True, index=True,
                         title="Published")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("mesh_major", type="text", index=True, title="MeSH major topics")
    db.declare_attribute("pmid", type="keyword", index=True, title="PMID")
    db.declare_layer("clusters/kmeans", kind="flat", source="kmeans", members="kmeans_members",
                     views=["knn"], require_member_visibility={"count": 50},
                     supplied=[("topic", "text", "all")], title="k-means clusters")
    same(generated(db), committed("medcpt"))


def test_overture(tmp_path):
    db = database(
        tmp_path,
        {"points": ("entity_id", "lon", "lat", "country")},
        ["category_root", "category", "basic_category", "country", "source_dataset",
         "operating_status", "division_country", "division_region", "division_county",
         "artifacts_divisions"],
    )
    db.declare_view("world", projection="web_mercator", extent=WORLD, access="country",
                    title="Overture places")
    for name, width, visibility, title in [
        ("category_root", "u8", "public", "Category root"),
        ("category", "u16", "public", "Category"),
        ("basic_category", "u16", "public", "Basic category"),
        ("country", "u16", "derived", "Country"),
        ("source_dataset", "u8", "public", "Source dataset"),
        ("operating_status", "u8", "public", "Operating status"),
        ("division_country", "u16", "derived", "Country division"),
        ("division_region", "u16", "derived", "Region"),
        ("division_county", "u32", "derived", "County"),
    ]:
        db.declare_vocabulary(name, source=name, closed=True, width=width, visibility=visibility,
                              title=title)
    for name in ("category_root", "category", "basic_category", "country", "source_dataset",
                 "operating_status"):
        db.declare_attribute(name, type="category", vocabulary=name, render=True, index=True)
    db.declare_attribute("confidence", type="f32", render=True)
    db.declare_attribute("update_time", type="timestamp_us", render=True)
    db.declare_attribute("name", type="text", index=True)
    for name in ("division_country", "division_region", "division_county"):
        db.declare_attribute(name, type="category", vocabulary=name, index=True)
    db.declare_layer(
        "boundaries/divisions",
        kind="nested",
        source="artifacts_divisions",
        views=["world"],
        membership="spatial",
        shape={"kind": "polygon"},
        default_space="wgs84",
        prune_children=True,
        require_member_visibility={"count": 1},
        computed=("centroid", "box"),
        supplied=[("name", "text", "inherited")],
        title="Administrative divisions",
    )
    db.declare_layer(
        "programmes/source",
        kind="flat",
        views=["world"],
        membership={"attribute": "source_dataset"},
        require_member_visibility={"count": 1},
        title="Contributing dataset",
    )
    same(generated(db), committed("overture"))


def test_paperseek(tmp_path):
    """Three markers the run fills: the access vocabulary, the topics layer, and `point_visibility`
    on the view. The SDK cannot leave the third out, a view naming a label for every point, so it
    is declared here and dropped before the comparison.
    """
    db = database(tmp_path, {"points": ("entity_id", "x", "y", "licence")},
                  ["licence", "type", "kmeans", "kmeans_members", "topics", "topics_members"])
    db.declare_view("knn", source="points", extent="auto", access="licence",
                    default_label="unlicensed", title="Scholarly map")
    db.declare_vocabulary("type", source="type", width="u8", title="Work type")
    db.declare_attribute("publication_year", type="i32", render=True, index=True,
                         title="Published")
    db.declare_attribute("type", type="category", vocabulary="type", render=True,
                         title="Work type")
    db.declare_attribute("is_oa", type="bool", render=True, title="Open access")
    db.declare_attribute("openalex_id", type="keyword", index=True, title="OpenAlex id")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("abstract", type="text", index=True)
    db.declare_layer("clusters/kmeans", kind="flat", source="kmeans", members="kmeans_members",
                     views=["knn"], require_member_visibility={"count": 50},
                     supplied=[("topic", "text", "all")], title="k-means clusters")
    written = generated(db)
    written["view"][0].pop("point_visibility")
    same(written, committed("paperseek"))


def test_treeoflife(tmp_path):
    """`# <vocabularies>` and `# <kmeans-layer>` are the run's: neither is declared here."""
    db = database(
        tmp_path,
        {
            "points": ("entity_id", "x", "y", "publisher"),
            "points_geo": ("entity_id", "lon", "lat", "publisher"),
        },
        ["publisher", "source_dataset", "basis", "img_type", "kingdom", "phylum", "class",
         "order", "family", "genus", "species", "taxonomy", "kmeans", "kmeans_members"],
    )
    db.declare_view("bioclip", source="points", extent="auto", access="publisher",
                    default_label="unpublished", title="Specimen map")
    db.declare_view("geo", source="points_geo", projection="web_mercator", extent=WORLD,
                    access="publisher", default_label="unpublished",
                    title="Where it was recorded")
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
        members="taxonomy",
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
    same(generated(db), committed("treeoflife"))


def test_multiview(tmp_path):
    """Every block of this corpus through the typed verbs, the view groups included.

    The two groups are the two rosters `declare_view_group` carries: `quarter` is form A, one
    inline view per quarter naming its own file, and `quarter_alt` shares its keys and takes one
    points file with a discriminator.
    """
    import datetime as dt

    db = database(
        tmp_path,
        {
            "world": ("entity_id", "lon", "lat", "access"),
            "quarter_2026_q1": ("entity_id", "x", "y", "access"),
            "quarter_2026_q2": ("entity_id", "x", "y", "access"),
            "quarter_2026_q3": ("entity_id", "x", "y", "access"),
            "quarter_2026_q4": ("entity_id", "x", "y", "access"),
            "quarter_alt_pts": ("entity_id", "lon", "lat", "access"),
        },
        ["attrs_constant", "attrs_scoped", "vocab_kind", "collections", "clusters_q"],
    )
    db.declare_view("world", projection="web_mercator", extent=WORLD, access="access",
                    title="Whole corpus")
    db.declare_view("world_flat", source="world", projection="equirectangular", extent=WORLD,
                    access="access", title="Whole corpus, equirectangular")

    def quarter(key: str, source: str, label: str, starts, ends) -> dict:
        return {"key": key, "source": source, "label": label,
                "starts": dt.datetime(*starts, tzinfo=dt.timezone.utc),
                "ends": dt.datetime(*ends, tzinfo=dt.timezone.utc)}

    db.declare_view_group(
        "quarter",
        title="By quarter",
        extent={"x": [-40.0, 40.0], "y": [-40.0, 40.0]},
        access="access",
        metadata={"label": "text", "starts": "timestamp_us", "ends": "timestamp_us"},
        views=[
            quarter("2026-Q1", "quarter_2026_q1", "Q1 2026", (2026, 1, 1), (2026, 4, 1)),
            quarter("2026-Q2", "quarter_2026_q2", "Q2 2026", (2026, 4, 1), (2026, 7, 1)),
            quarter("2026-Q3", "quarter_2026_q3", "Q3 2026", (2026, 7, 1), (2026, 10, 1)),
            quarter("2026-Q4", "quarter_2026_q4", "Q4 2026", (2026, 10, 1), (2027, 1, 1)),
        ],
    )
    db.declare_view_group(
        "quarter_alt",
        title="By quarter, geographic",
        members="quarter",
        projection="web_mercator",
        extent=WORLD,
        source="quarter_alt_pts",
        view_field="quarter",
        access="access",
    )

    db.declare_vocabulary("kind", source="vocab_kind", closed=True, width="u8",
                          title="Feature kind")
    db.declare_attribute("importance", type="i64", index=True, source="attrs_constant")
    db.declare_attribute("kind", type="category", vocabulary="kind", index=True, render=True,
                         source="attrs_constant")
    db.declare_attribute("sentiment", type="f32", scope={"group": "quarter"}, index=True,
                         render=True)
    db.declare_vocabulary("mood", closed=True, width="u8", visibility="derived",
                          values=["calm", "tense", "wild", "still"], title="Mood")
    db.declare_attribute("mood", type="category", vocabulary="mood",
                         scope={"group": "quarter"}, index=True)
    db.declare_attribute("note", type="text", scope={"group": "quarter"}, index=True)
    db.declare_attribute("coverage", type="f32", scope={"group": "quarter"}, index=True,
                         source="attrs_scoped", fields={"view": "quarter"})

    db.declare_layer(
        "collections",
        kind="flat",
        source="collections",
        views=["world", "quarter"],
        artifact_visibility={"field": "access", "default": "inherited"},
        require_member_visibility="all",
        computed=(),
        supplied=[("tag", "text", "inherited")],
        title="Curated collections",
    )
    db.declare_layer(
        "quarter_clusters",
        kind="flat",
        source="clusters_q",
        views=["quarter"],
        scope={"group": "quarter"},
        fields={"view": "quarter"},
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
    same(generated(db), committed("multiview"))
