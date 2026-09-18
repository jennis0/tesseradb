"""Every corpus declaration under `test_corpora/`, regenerated through the typed verbs.

The proof python-sdk.md §12 asks for is `tessera check` over the regenerated file printing what the
committed file prints. The files these declarations name are not on this machine, so `check` cannot
read them and the proof here is the declaration itself: each corpus is declared through the typed
verbs, given its tables through `insert`, written, parsed, and compared with the committed
`corpus.toml` block by block and key by key. `test_sdk_corpus.py` carries the two corpora whose
files can be present, where the comparison is the binary's own disclosure table.

Every insert is a placeholder parquet carrying the columns its target reads: the id column, a
view's coordinates and access column and one column per declared attribute, or an artifact
table's `key` and a member table's `(key, entity)`. No value is in them; what is compared is the
declaration each insert wrote.

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

5. `[sources]` itself is not compared, and every `source` is compared by **which blocks read it
   together** rather than by its key. The committed file names the corpus's own parquets and the
   SDK names each source after the target its insert bound it to; and a corpus whose committed
   file carries a `<marker>` names files for objects a run fills in, which are not declared here.
6. `[defaults].entity_id_field` where it is `entity_id`, the surface's own default.
7. A view's `fields`, and a `[layer.members]`'s, where each column carries its canonical name.
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
    """A one-row table. The SDK reads a source's schema; the corpus's values are not here."""
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


def database(tmp_path: Path) -> object:
    db = create(tmp_path / "db")
    db.files = tmp_path / "files"
    return db


def points(db, view: str, columns) -> None:
    """One view's own insert: its geometry, its labels, its id, and every column it fills.

    The attribute columns are the declared attributes' own names, which a frame inserted into the
    allocation view fills by name (§3), so the placeholder carries one column each.
    """
    attributes = [
        block["name"]
        for block in db.blocks.blocks["attribute"]
        if not block.get("scope")
    ]
    named = {"id": "entity_id"}
    named["lon" if "lon" in columns else "x"] = "lon" if "lon" in columns else "x"
    named["lat" if "lat" in columns else "y"] = "lat" if "lat" in columns else "y"
    access = [one for one in columns if one not in ("entity_id", "x", "y", "lon", "lat")]
    if access:
        named["access"] = access[0]
    every = list(dict.fromkeys(list(columns) + attributes))
    db.insert(view, parquet(db.files, f"{view}_points".replace("/", "_"), every), **named)


def artifacts(db, layer: str, columns=("key",)) -> None:
    db.insert(
        layer,
        artifacts=parquet(db.files, f"{layer}_artifacts".replace("/", "_"), columns),
        key="key",
    )


def members(db, layer: str) -> None:
    db.insert(
        layer,
        members=parquet(db.files, f"{layer}_members".replace("/", "_"), ("key", "entity")),
        id="entity",
        key="key",
    )


def values(db, vocabulary: str) -> None:
    db.insert(
        vocabulary,
        parquet(db.files, f"{vocabulary}_values", ("key",)),
        key="key",
    )


# ---------------------------------------------------------------------------- the comparison


def committed(corpus: str) -> dict:
    return _toml.loads((CORPORA / corpus / "corpus.toml").read_text())


def generated(db) -> dict:
    return _toml.loads(db.declaration)


def _readers(document: dict) -> dict[str, tuple]:
    """Which blocks name each source key, so a key can be compared by what reads it."""
    readers: dict[str, list[str]] = {}
    for kind in ("view", "view_group", "vocabulary", "attribute", "layer"):
        for block in document.get(kind, []):
            for one, where in (
                (block, f"{kind} {block.get('name')}"),
                (block.get("members"), f"{kind} {block.get('name')} members"),
                (block.get("views"), f"{kind} {block.get('name')} roster"),
                (block.get("labels"), f"{kind} {block.get('name')} labels"),
                ((block.get("labels") or {}).get("members")
                 if isinstance(block.get("labels"), dict) else None,
                 f"{kind} {block.get('name')} labels members"),
            ):
                if isinstance(one, dict) and one.get("source"):
                    readers.setdefault(one["source"], []).append(where)
    return {key: tuple(sorted(names)) for key, names in readers.items()}


def _by_readers(block: dict, readers: dict) -> dict:
    block = dict(block)
    if block.get("source") in readers:
        block["source"] = readers[block["source"]]
    return block


def normalised(document: dict, filling: bool = False) -> dict:
    """One document ready to compare. `filling` adds what §4.8 requires the SDK to write."""
    out = dict(document)
    out.pop("sources", None)
    defaults = dict(document.get("defaults", {}))
    if defaults.get("entity_id_field") == "entity_id":
        defaults.pop("entity_id_field")
    views = document.get("view", [])
    if filling and len(views) == 1:
        defaults.setdefault("allocation_view", views[0]["name"])
    out["defaults"] = defaults
    readers = _readers(document)
    if filling and defaults.get("source"):
        # `[defaults].source` is resolved onto every block below before the keys are compared, so
        # a block that names none reads the default's file with everything else that does.
        for kind in ("view", "attribute"):
            for block in document.get(kind, []):
                if "source" not in block and not block.get("scope"):
                    readers.setdefault(defaults["source"], ())
                    readers[defaults["source"]] = tuple(
                        sorted(readers[defaults["source"]] + (f"{kind} {block['name']}",))
                    )
    out["view"] = [_by_readers(_view(block, defaults, filling), readers) for block in views]
    out["attribute"] = [
        block if block.get("scope")
        else _by_readers(_sourced(block, defaults, filling), readers)
        for block in document.get("attribute", [])
    ]
    out["vocabulary"] = [
        _by_readers(block, readers) for block in document.get("vocabulary", [])
    ]
    out["layer"] = [_layer(block, filling, readers) for block in document.get("layer", [])]
    # `[defaults].source` is resolved onto every block above and compared there. The SDK writes it
    # nowhere: `default=True` fills the source onto each block that named none, which is what §4.8
    # asks for, so a default in the document would only bind a column declared at a running
    # service to the file the first commit built from.
    defaults.pop("source", None)
    return {key: value for key, value in out.items() if value not in ({}, [])}


def _sourced(block: dict, defaults: dict, filling: bool) -> dict:
    block = dict(block)
    if filling and "source" not in block and defaults.get("source"):
        block["source"] = defaults["source"]
    return block


def _view(block: dict, defaults: dict, filling: bool) -> dict:
    block = _fields(_sourced(block, defaults, filling))
    if filling:
        block.setdefault("visibility", "public")
    return block


def _fields(block: dict) -> dict:
    """A `fields` map where every column carries its canonical name says nothing (§4.8)."""
    block = dict(block)
    fields = block.get("fields")
    if fields and all(name == column for name, column in fields.items()):
        block.pop("fields")
    return block


def _layer(block: dict, filling: bool, readers: dict | None = None) -> dict:
    block = _by_readers(block, readers or {})
    if isinstance(block.get("members"), dict):
        block["members"] = _by_readers(_fields(block["members"]), readers or {})
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


def bindings(db) -> None:
    """Every source the SDK named is the file of the target and role its insert bound (§4.8).

    The comparison below reads a `source` by which blocks name it, so a binding swapped between
    two targets would compare equal. The key the SDK writes is the target's own name and the
    role's, and the file under it is the one that insert wrote, which is what this asserts.
    """
    from pathlib import Path as _Path

    for insert in db.inserts:
        stem = insert.target.replace("/", "_")
        expected = stem if insert.role in ("rows", "values", "text") else f"{stem}_{insert.role}"
        assert insert.source == expected, f"{insert.target} ({insert.role}): {insert.source}"
        assert _Path(insert.path).exists(), insert.path
    keys = set(_toml.loads(db.declaration).get("sources", {}))
    assert keys == {insert.source for insert in db.inserts}


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
    db = database(tmp_path)
    db.declare_view("knn", extent="auto", title="Topic map")
    db.declare_view("pca64", extent="auto", title="Topic map (PCA-64)")
    db.declare_vocabulary("archive", closed=True, width="u8", title="arXiv archive")
    db.declare_vocabulary("primary_category", closed=True, width="u16",
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
    for name, kind, requirement, title in [
        ("clusters/kmeans", "flat", {"count": 50}, "k-means clusters"),
        ("clusters/hdbscan", "nested", {"fraction": 0.05}, "HDBSCAN clusters"),
    ]:
        db.declare_layer(name, kind=kind, views=["knn", "pca64"],
                         require_member_visibility=requirement,
                         supplied=[("topic", "text", "all")], title=title)

    points(db, "knn", ("entity_id", "x", "y", "categories"))
    points(db, "pca64", ("entity_id", "x", "y", "categories"))
    values(db, "archive")
    values(db, "primary_category")
    for name in ("clusters/kmeans", "clusters/hdbscan"):
        artifacts(db, name)
        members(db, name)
    bindings(db)
    same(generated(db), committed("arxiv"))


def test_gbif(tmp_path):
    """`# <vocabulary>` is the `kingdom` value set, whose size the run sees: not declared here."""
    db = database(tmp_path)
    db.declare_view("geo", projection="web_mercator", extent=WORLD,
                    default_label="UNRECORDED", title="Where it was recorded")
    db.declare_attribute("kingdom", type="category", vocabulary="kingdom", render=True,
                         title="Kingdom")
    db.declare_attribute("specieskey", type="keyword", index=True, title="GBIF species key")
    db.declare_attribute("year", type="u16", index=True, title="Year")
    db.declare_attribute("scientificname", type="keyword", title="Scientific name")
    db.declare_layer(
        "taxonomy/tree",
        kind="tiered",
        views=["geo"],
        value_set="open",
        prune_children=True,
        require_member_visibility={"count": 1},
        levels=[(0, "Family", [0, 5]), (1, "Genus", [4, 10]), (2, "Species", [9, 16])],
        computed=("centroid", "box"),
        title="Taxonomy",
    )
    points(db, "geo", ("entity_id", "lon", "lat", "countrycode"))
    members(db, "taxonomy/tree")
    bindings(db)
    same(generated(db), committed("gbif"))


def test_geonames(tmp_path):
    db = database(tmp_path)
    db.declare_view("world", projection="web_mercator", extent=WORLD, title="GeoNames")
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
        db.declare_vocabulary(name, closed=True, width=width, visibility=visibility, title=title)
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
    points(db, "world", ("entity_id", "lon", "lat", "country"))
    for name in ("feature_class", "feature_code", "country", "admin1", "admin2", "admin3",
                 "admin4", "timezone"):
        values(db, name)
    members(db, "features/taxonomy")
    artifacts(db, "admin/hierarchy")
    members(db, "admin/hierarchy")
    bindings(db)
    same(generated(db), committed("geonames"))


def test_medcpt(tmp_path):
    """`# <abstract-attribute>` and `# <mesh-layer>` are the run's: neither is declared here."""
    db = database(tmp_path)
    db.declare_view("knn", extent="auto", title="Literature map")
    db.declare_vocabulary("branch", closed=True, width="u8", title="MeSH branch")
    db.declare_attribute("published", type="timestamp_us", render=True, index=True,
                         title="Published")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("mesh_major", type="text", index=True, title="MeSH major topics")
    db.declare_attribute("pmid", type="keyword", index=True, title="PMID")
    db.declare_layer("clusters/kmeans", kind="flat", views=["knn"],
                     require_member_visibility={"count": 50},
                     supplied=[("topic", "text", "all")], title="k-means clusters")
    points(db, "knn", ("entity_id", "x", "y", "branches"))
    values(db, "branch")
    artifacts(db, "clusters/kmeans")
    members(db, "clusters/kmeans")
    bindings(db)
    same(generated(db), committed("medcpt"))


def test_overture(tmp_path):
    db = database(tmp_path)
    db.declare_view("world", projection="web_mercator", extent=WORLD, title="Overture places")
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
        db.declare_vocabulary(name, closed=True, width=width, visibility=visibility, title=title)
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
    points(db, "world", ("entity_id", "lon", "lat", "country"))
    for name in ("category_root", "category", "basic_category", "country", "source_dataset",
                 "operating_status", "division_country", "division_region", "division_county"):
        values(db, name)
    artifacts(db, "boundaries/divisions")
    bindings(db)
    same(generated(db), committed("overture"))


def test_paperseek(tmp_path):
    """Three markers the run fills: the access vocabulary, the topics layer, and `point_visibility`
    on the view. The SDK cannot leave the third out, a view naming a label for every point, so it
    is declared here and dropped before the comparison.
    """
    db = database(tmp_path)
    db.declare_view("knn", extent="auto", default_label="unlicensed", title="Scholarly map")
    db.declare_vocabulary("type", width="u8", title="Work type")
    db.declare_attribute("publication_year", type="i32", render=True, index=True,
                         title="Published")
    db.declare_attribute("type", type="category", vocabulary="type", render=True,
                         title="Work type")
    db.declare_attribute("is_oa", type="bool", render=True, title="Open access")
    db.declare_attribute("openalex_id", type="keyword", index=True, title="OpenAlex id")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("abstract", type="text", index=True)
    db.declare_layer("clusters/kmeans", kind="flat", views=["knn"],
                     require_member_visibility={"count": 50},
                     supplied=[("topic", "text", "all")], title="k-means clusters")
    points(db, "knn", ("entity_id", "x", "y", "licence"))
    values(db, "type")
    artifacts(db, "clusters/kmeans")
    members(db, "clusters/kmeans")
    bindings(db)
    written = generated(db)
    written["view"][0].pop("point_visibility")
    same(written, committed("paperseek"))


def test_treeoflife(tmp_path):
    """`# <vocabularies>` and `# <kmeans-layer>` are the run's: neither is declared here."""
    db = database(tmp_path)
    db.declare_view("bioclip", extent="auto", default_label="unpublished",
                    title="Specimen map")
    db.declare_view("geo", projection="web_mercator", extent=WORLD,
                    default_label="unpublished", title="Where it was recorded")
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
    bindings(db)
    same(generated(db), committed("treeoflife"))


@pytest.mark.xfail(
    strict=True,
    reason="the insert surface has no spelling for a roster of inline views (one "
    "`[[view_group.view]]` per view, each naming its own points file), which this corpus's "
    "`quarter` group uses. A group's views and their metadata come from one roster insert beside "
    "one points table with a discriminator (python-sdk.md §4.3), which is the corpus's other "
    "group. Reported against §4.3",
)
def test_multiview(tmp_path):
    """Every block of this corpus through the typed verbs, the view groups included."""
    db = database(tmp_path)
    db.declare_view_group("quarter", title="By quarter",
                          extent={"x": [-40.0, 40.0], "y": [-40.0, 40.0]},
                          metadata={"label": "text", "starts": "timestamp_us",
                                    "ends": "timestamp_us"})
    raise AssertionError("form A is not expressible through insert")
