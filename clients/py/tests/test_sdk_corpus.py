"""The proof python-sdk.md asks for: a corpus declaration regenerated from calls.

`tessera check` prints its disclosure table on stdout and the schemas it read, with their paths, on
stderr. The declarations name the same files by different paths: one reads `data/notebook/`
directly, the other reads it through a relative path from a temporary directory. **Stdout is
compared whole, and stderr by its file names.** That is the whole normalisation: the disclosure table carries no
path, which is what makes it the thing to compare.

Beside it, the first commit through `tessera build --mint-external-ids`.
"""

import json
import os
import subprocess
import urllib.request
from pathlib import Path

import pytest

from tesseradb import _instance
from tesseradb._auth import authorise
from tesseradb._database import create

pytest.importorskip("pyarrow")


def binary() -> str:
    try:
        return _instance.find_binary()[0]
    except Exception as why:  # noqa: BLE001, the skip message is the whole point
        pytest.skip(f"no tessera binary: {why}")


def notebook_corpus() -> Path:
    """`data/notebook/`, which is gitignored and shared by every worktree of this checkout."""
    named = os.environ.get("TESSERA_NOTEBOOK_DATA")
    if named:
        return Path(named)
    here = Path(__file__).resolve()
    common = subprocess.run(
        ["git", "rev-parse", "--git-common-dir"],
        cwd=here.parent,
        capture_output=True,
        text=True,
    )
    roots = []
    if common.returncode == 0:
        roots.append(Path(common.stdout.strip()).resolve().parent)
    roots.append(here.parents[3])
    for root in roots:
        if (root / "data" / "notebook" / "schema.toml").exists():
            return root / "data" / "notebook"
    pytest.skip("data/notebook/ is not in this checkout; set TESSERA_NOTEBOOK_DATA")


def declare_notebook(db, corpus: Path) -> None:
    """python-sdk.md §10.2: `data/notebook/schema.toml` as calls, the files staged as paths."""
    db.stage("points", str(corpus / "points.parquet"), default=True)
    for name, file in [
        ("archive", "archive"),
        ("primary_category", "primary_category"),
        ("kmeans", "clusters-kmeans"),
        ("kmeans_members", "clusters-kmeans-members"),
        ("kmeans_topics", "topics-kmeans"),
        ("kmeans_topic_members", "topics-kmeans-members"),
        ("hdbscan", "clusters-hdbscan"),
        ("hdbscan_members", "clusters-hdbscan-members"),
        ("hdbscan_topics", "topics-hdbscan"),
        ("hdbscan_topic_members", "topics-hdbscan-members"),
        ("taxonomy", "taxonomy-arxiv"),
        ("taxonomy_members", "taxonomy-arxiv-members"),
    ]:
        db.stage(name, str(corpus / f"{file}.parquet"))

    db.declare_view("s0", source="points", access="categories", title="arXiv, 50,000 papers")
    db.declare_vocabulary(
        "archive", source="archive", closed=True, width="u8", title="arXiv archive"
    )
    db.declare_vocabulary(
        "primary_category",
        source="primary_category",
        closed=True,
        width="u16",
        title="arXiv subject class",
    )
    db.declare_attribute(
        "archive", type="category", vocabulary="archive", render=True, index=True, title="Archive"
    )
    db.declare_attribute(
        "primary_category",
        type="category",
        vocabulary="primary_category",
        render=True,
        index=True,
        title="Primary category",
    )
    db.declare_attribute("submitted_at", type="timestamp_us", render=True, title="Submitted")
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("abstract", type="text", index=True)
    db.declare_attribute("arxiv_id", type="keyword", index=True, title="arXiv ID")

    db.declare_layer(
        "clusters/kmeans",
        kind="flat",
        source="kmeans",
        members="kmeans_members",
        require_member_visibility={"count": 50},
        title="k-means clusters",
    )
    db.declare_labels(
        "topics/kmeans",
        of="clusters/kmeans",
        source="kmeans_topics",
        members="kmeans_topic_members",
        content_requires="all",
        title="k-means topics",
    )
    db.declare_layer(
        "clusters/hdbscan",
        kind="nested",
        source="hdbscan",
        members="hdbscan_members",
        require_member_visibility={"fraction": 0.05},
        title="HDBSCAN clusters",
    )
    db.declare_labels(
        "topics/hdbscan",
        of="clusters/hdbscan",
        source="hdbscan_topics",
        members="hdbscan_topic_members",
        content_requires="all",
        title="HDBSCAN topics",
    )
    db.declare_layer(
        "taxonomy/arxiv",
        kind="tiered",
        source="taxonomy",
        members="taxonomy_members",
        levels=[(0, "archive"), (1, "subject class")],
        require_member_visibility={"count": 1},
        computed=("centroid", "box"),
        title="arXiv classification",
    )


def read_schema_lines(stderr: str) -> list[str]:
    """`tessera check`'s "read schema" lines, with the path column cut to its file name.

    The two declarations name one set of files by two paths, one relative to `data/notebook/` and
    one relative to a temporary directory, so the file name is what can be compared. What is being
    compared is which object reads which file, which the name carries.
    """
    lines = []
    for line in stderr.splitlines():
        if "read schema" not in line and "no source" not in line:
            continue
        head, _, path = line.rpartition(" ")
        # The object column is padded to a width the longer object names overflow, so the spacing
        # is collapsed before the two runs are compared.
        lines.append(" ".join(head.split()) + "  " + Path(path.strip()).name)
    return lines


def check_committed(tessera: str, declaration: Path, directory: Path) -> tuple[str, str]:
    """`tessera check` over a declaration this repository holds, and its disclosure table."""
    (directory / "cache").mkdir(parents=True, exist_ok=True)
    (directory / "tessera.toml").write_text(
        "[bundle]\n"
        'path = "bundle"\ncache = "cache"\nwal = "wal.log"\n\n'
        f'[build]\nschema = "{declaration}"\n\n'
        '[plugin]\nmodule = "builtin:passthrough"\n\n'
        "[disclosure]\ntoken_max_lifetime = 3600\n"
    )
    done = subprocess.run(
        [tessera, "check", "--deployment", str(directory / "tessera.toml")],
        capture_output=True,
        text=True,
    )
    assert done.returncode == 0, done.stderr
    return done.stdout, done.stderr


def test_the_notebook_declaration_regenerated_discloses_what_the_committed_one_discloses(tmp_path):
    tessera = binary()
    corpus = notebook_corpus()
    db = create(tmp_path / "db")
    declare_notebook(db, corpus)
    report = db.check()
    assert report.ok, report.output
    committed, committed_stderr = check_committed(
        tessera, corpus / "schema.toml", tmp_path / "committed"
    )
    generated = report.output[: report.output.index("  read schema")]
    assert generated.strip() == committed.strip()
    # And the same files read by the same objects: the paths differ, the file names do not.
    read = read_schema_lines(report.output)
    assert len(read) == 12
    assert read == read_schema_lines(committed_stderr)


def test_the_regenerated_declaration_states_what_the_committed_one_leaves_to_a_default(tmp_path):
    corpus = notebook_corpus()
    binary()
    db = create(tmp_path / "db")
    declare_notebook(db, corpus)
    text = db.declaration
    # §4.8: the allocation view, every value set and every disclosure control, written out.
    assert 'allocation_view = "s0"' in text
    # Two vocabularies and three layers. A `[layer.labels]` block takes no value set: its keys
    # are configuration.md's labels table, and a key that table does not name is refused at parse.
    assert text.count("value_set") == 2 + 3
    # Three layers, and each label set twice: the layer grain and the content grain.
    assert text.count("require_member_visibility") == 3 + 2 * 2
    assert text.count("artifact_visibility") == 3 + 2
    assert text.count('visibility = "public"') == 2 + 1 + 3
    # The points file's ids are integers, so it is read in place and its user id is the source id:
    # no keyword attribute is written for it beyond the one the declaration names.
    assert text.count('type = "keyword"') == 1
    # Each layer's value set, written whether the user chose it or the SDK did: every one of these
    # names its artifacts in a table, so every one is closed.
    layers = text.split("[[layer]]")[1:]
    assert len(layers) == 3
    for layer in layers:
        assert 'value_set = "closed"' in layer
    assert "/dev/shm" not in text and not any(
        line.startswith('points = "/') for line in text.splitlines()
    )


@pytest.mark.skipif(
    not (Path(os.environ.get("TESSERA_LADDER", "/nonexistent")) / "arxiv").is_dir(),
    reason="$TESSERA_LADDER/arxiv is not on this machine: the ladder's arXiv rung is not staged",
)
def test_the_arxiv_declaration_regenerated_discloses_what_the_committed_one_discloses(tmp_path):
    tessera = binary()
    corpus = Path(os.environ["TESSERA_LADDER"]) / "arxiv"
    declaration = corpus / "corpus.toml"
    if "clusters/toponymy" in declaration.read_text():
        pytest.skip("this rung's copy carries the spliced Toponymy layer, which §10.2 does not")
    db = create(tmp_path / "db")
    db.stage("points", str(corpus / "points.parquet"), default=True)
    db.stage("points_pca64", str(corpus / "points-pca64.parquet"))
    for name, file in [
        ("archive", "archive"),
        ("primary_category", "primary_category"),
        ("kmeans", "clusters-kmeans"),
        ("kmeans_members", "clusters-kmeans-members"),
        ("hdbscan", "clusters-hdbscan"),
        ("hdbscan_members", "clusters-hdbscan-members"),
    ]:
        db.stage(name, str(corpus / f"{file}.parquet"))
    db.declare_view(
        "knn", source="points", access="categories", extent="auto", title="Topic map"
    )
    db.declare_view(
        "pca64",
        source="points_pca64",
        access="categories",
        extent="auto",
        title="Topic map (PCA-64)",
    )
    db.declare_vocabulary(
        "archive", source="archive", closed=True, width="u8", title="arXiv archive"
    )
    db.declare_vocabulary(
        "primary_category",
        source="primary_category",
        closed=True,
        width="u16",
        title="arXiv subject class",
    )
    db.declare_attribute(
        "archive", type="category", vocabulary="archive", render=True, index=True, title="Archive"
    )
    db.declare_attribute(
        "primary_category",
        type="category",
        vocabulary="primary_category",
        render=True,
        index=True,
        title="Primary category",
    )
    db.declare_attribute(
        "submitted_at", type="timestamp_us", render=True, index=True, title="Submitted"
    )
    db.declare_attribute("title", type="text", index=True)
    db.declare_attribute("abstract", type="text", index=True)
    db.declare_attribute("authors", type="text", index=True, title="Authors")
    db.declare_attribute("arxiv_id", type="keyword", index=True, title="arXiv ID")
    for name, source, members, kind, requirement in [
        ("clusters/kmeans", "kmeans", "kmeans_members", "flat", {"count": 50}),
        ("clusters/hdbscan", "hdbscan", "hdbscan_members", "nested", {"fraction": 0.05}),
    ]:
        db.declare_layer(
            name,
            kind=kind,
            source=source,
            members=members,
            views=["knn", "pca64"],
            require_member_visibility=requirement,
            supplied=[("topic", "text", "all")],
            title="k-means clusters" if kind == "flat" else "HDBSCAN clusters",
        )
    report = db.check()
    assert report.ok, report.output
    committed, committed_stderr = check_committed(tessera, declaration, tmp_path / "committed")
    generated = report.output[: report.output.index("  read schema")]
    assert generated.strip() == committed.strip()
    assert read_schema_lines(report.output) == read_schema_lines(committed_stderr)


def test_the_first_commit_builds_a_bundle_and_mints_every_external_id(tmp_path):
    binary()
    corpus = notebook_corpus()
    db = create(tmp_path / "db")
    declare_notebook(db, corpus)
    try:
        report = db.commit()
        assert report.ok, report.output
    finally:
        db.close()
    bundle = db.path / "bundle"
    assert (bundle / "CURRENT").exists()
    entities = bundle / "v00000" / "partitions" / "default" / "entities"
    # `--mint-external-ids`: every built row is addressable afterwards, on the ingest and values
    # routes and in `remove()`.
    assert (entities / "ext-locator.u32").exists()
    assert list(entities.glob("external-ids-*.arrow"))
    # The regeneration proved through the build: the bundle's own disclosure report, which carries
    # no path, is what the committed declaration's build writes.
    built = build_committed(binary(), corpus / "schema.toml", tmp_path / "committed")
    assert json.loads((bundle / "reports" / "disclosure.json").read_text()) == json.loads(
        built.read_text()
    )


def build_committed(tessera: str, declaration: Path, directory: Path) -> Path:
    """Build a declaration this repository holds, and return its disclosure report."""
    check_committed(tessera, declaration, directory)
    done = subprocess.run(
        [
            tessera,
            "build",
            "--deployment",
            str(directory / "tessera.toml"),
            "--mint-external-ids",
            "--mint-id-key",
        ],
        capture_output=True,
        text=True,
    )
    assert done.returncode == 0, done.stderr
    return directory / "bundle" / "reports" / "disclosure.json"


def test_the_committed_database_is_served_and_close_stops_the_child(tmp_path):
    """The first commit through to a served answer: the announce line, a token, `/v1/meta`."""
    binary()
    corpus = notebook_corpus()
    db = create(tmp_path / "db")
    declare_notebook(db, corpus)
    report = db.commit()
    try:
        assert report.ok, report.output
        # The addresses are the child's own, read from the line it printed: the SDK declared port
        # 0 on each plane, so nothing here was guessed.
        for address in (report.viewer, report.session, report.control):
            assert address and address.startswith("127.0.0.1:")
            assert not address.endswith(":0")
        token = authorise(db.session_url, db.session_credential, ["public"])
        assert token.token and token.seconds_left > 0
        meta = json.loads(
            urllib.request.urlopen(
                urllib.request.Request(
                    db.viewer_url + "/v1/meta",
                    headers={"authorization": f"Bearer {token.token}"},
                ),
                timeout=30,
            ).read()
        )
        assert {view["id"] for view in meta["views"]} == {"s0"}
        columns = {column["name"]: column for column in meta["declared_scalars"]}
        assert set(columns) == {
            "archive",
            "primary_category",
            "submitted_at",
            "title",
            "abstract",
            "arxiv_id",
        }
        # The render flags the first commit froze, and the vocabulary a category names.
        assert [c for c in columns.values() if c["render"]] and columns["title"]["render"] is False
        assert columns["archive"]["category"]["vocabulary"] == "archive"
        layers = {layer["name"] for layer in meta["layers"]}
        assert layers == {
            "clusters/kmeans",
            "topics/kmeans",
            "clusters/hdbscan",
            "topics/hdbscan",
            "taxonomy/arxiv",
        }
        shapes = {
            layer["name"]: (layer["hierarchy"]["kind"], [level["level"] for level in layer["levels"]])
            for layer in meta["layers"]
        }
        assert shapes["clusters/kmeans"] == ("flat", [])
        assert shapes["clusters/hdbscan"] == ("nested", [])
        assert shapes["taxonomy/arxiv"] == ("tiered", [0, 1])
        # A label set expands to a flat layer of its own, depending on the clustering it names.
        assert shapes["topics/kmeans"] == ("flat", [])
        child = db._child.pid
    finally:
        db.close()
    assert db.listening is None
    with pytest.raises(OSError):
        os.kill(child, 0)
