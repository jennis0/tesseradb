"""A Tessera database in a directory: create it, fill it, commit it (python-sdk.md §2, §6).

The directory is everything `tessera build` and `tessera serve` read, so a notebook prototype
becomes a deployment by copying it: `tessera serve --deployment <dir>/tessera.toml` on another
machine serves the same database.

Three verbs carry the model. `stage` binds a named source to a frame or a file, `declare_*` adds a
block to the declaration and names the sources it reads, and `commit` makes the staged data part
of the database. `check` is `commit` with nothing sent.

Stage S1 of §12: the first commit, which builds. Later commits page through the control plane and
are stage S3; the verbs that would start one refuse naming it.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any, Hashable, Iterable, Sequence

import pyarrow as pa
import pyarrow.parquet as pq

from . import _declaration as D
from . import _infer, _instance
from ._idmap import IdMap
from ._reports import CommitReport, Inference, Report, render_columns_of
from ._sources import Refusal, StagedSource, is_pandas_frame, stage_frame, stage_path
from ._toml import dumps

#: A temporary database goes here when the platform has a RAM-backed filesystem (§2).
RAM_BACKED = Path("/dev/shm")

LATER_COMMITS = (
    "later commits page through the control plane and are stage S3 of python-sdk.md §12"
)


class Database:
    """One database directory, and the declaration the SDK is building for it."""

    def __init__(self, path: Path, temporary: bool = False) -> None:
        self.path = Path(path)
        self.temporary = temporary
        self.sources: dict[str, StagedSource] = {}
        self.blocks = D.Declaration()
        self.id_map = IdMap(self.path / ".tessera" / "idmap.json")
        self.built = (self.path / "bundle" / "CURRENT").exists()
        self._child: subprocess.Popen | None = None
        self.listening: _instance.Listening | None = None
        self._loaded_text: str | None = None
        self._inference = Inference()

    # ------------------------------------------------------------------ sources

    def stage(
        self,
        name: str,
        data: Any,
        id: str | None = None,
        default: bool = False,
    ) -> StagedSource:
        """Bind `name` in `[sources]` to a frame or a file (§3).

        A frame is written to `sources/<name>.parquet` with the `entity_id` column the build
        reads; a path is recorded and read in place where its ids already are what the build
        reads. Staging a name twice before the first commit replaces the earlier data.
        """
        if isinstance(data, (str, os.PathLike)) and not is_pandas_frame(data):
            staged = stage_path(name, data, self.path, self.id_map, id=id, default=default)
        else:
            staged = stage_frame(
                name,
                data,
                self.path,
                self.id_map,
                id=id,
                default=default,
                first_commit=not self.built,
            )
        if default:
            for other in self.sources.values():
                if other.default:
                    other.default = False
                    staged.notes.append(f"the default source, replacing '{other.name}'")
        self.sources[name] = staged
        self.id_map.save()
        return staged

    @property
    def default_source(self) -> str | None:
        for staged in self.sources.values():
            if staged.default:
                return staged.name
        return None

    # ------------------------------------------------------------------ declarations

    def declare(self, kind: str, block: dict) -> dict:
        """One block of the declaration, spelled with configuration.md's own keys (§4.1).

        Every block is expressible this way; the typed verbs below build the dict and call here,
        and this is the escape hatch for a key whose verb has not landed.
        """
        self._refuse_after_the_first_commit("declare")
        return self.blocks.add(kind, block)

    def declare_view(self, name: str, source: str | None = None, **kwargs) -> dict:
        self._refuse_after_the_first_commit("declare_view")
        return self.blocks.add("view", D.view_block(name, source, **kwargs))

    def declare_view_group(self, *args, **kwargs):
        raise Refusal(
            "declare_view_group is stage S5 of python-sdk.md §12. Write the block through "
            "declare('view_group', …) until it lands"
        )

    def declare_vocabulary(self, name: str, **kwargs) -> dict:
        self._refuse_after_the_first_commit("declare_vocabulary")
        return self.blocks.add("vocabulary", D.vocabulary_block(name, **kwargs))

    def declare_attribute(self, name: str, type: str, **kwargs) -> dict:
        self._refuse_after_the_first_commit("declare_attribute")
        return self.blocks.add("attribute", D.attribute_block(name, type, **kwargs))

    def declare_layer(self, name: str, kind: str, **kwargs) -> dict:
        self._refuse_after_the_first_commit("declare_layer")
        views = kwargs.get("views")
        block = D.layer_block(name, kind, self._points_source(views), **kwargs)
        return self.blocks.add("layer", block)

    def declare_labels(
        self,
        name: str,
        of: str,
        source: Any,
        members: str | None = None,
        **kwargs,
    ) -> dict:
        """A label set over a clustering: the `[layer.labels]` block on the layer `of` (§4.7)."""
        self._refuse_after_the_first_commit("declare_labels")
        parent = self.blocks.layer(of)
        if isinstance(source, dict):
            source = self._stage_label_text(name, source)
        block = D.labels_block(name, source, members=members, **kwargs)
        if "labels" in parent:
            raise Refusal(
                f"layer {of!r} already carries a label set. A second one is a `[[layer]]` of its "
                f"own; write it through declare_layer"
            )
        parent["labels"] = block
        return block

    def _stage_label_text(self, name: str, mapping: dict) -> str:
        """A mapping from cluster key to text, as the `(key, contents)` table the block reads."""
        source_name = name.replace("/", "_")
        table = pa.table(
            {
                "level": pa.array([0] * len(mapping), type=pa.uint32()),
                "key": pa.array([str(k) for k in mapping], type=pa.string()),
                "contents": pa.array(
                    [[[v]] if isinstance(v, str) else [list(v)] for v in mapping.values()],
                    type=pa.list_(pa.list_(pa.string())),
                ),
            }
        )
        path = self.path / "sources" / f"{source_name}.parquet"
        path.parent.mkdir(parents=True, exist_ok=True)
        pq.write_table(table, path)
        self.sources[source_name] = StagedSource(
            name=source_name,
            path=path,
            declared_path=f"sources/{source_name}.parquet",
            rows=table.num_rows,
            columns=dict(zip(table.schema.names, table.schema.types)),
            notes=["written from a mapping of cluster key to text"],
        )
        return source_name

    def _points_source(self, views: Iterable[str] | None) -> str | None:
        """The points source a `from_column` layer reads: its first view's, or the default."""
        names = list(views) if views is not None else self.blocks.view_names()
        for name in names:
            for block in self.blocks.blocks["view"]:
                if block["name"] == name and block.get("source"):
                    return block["source"]
        return self.default_source

    def _refuse_after_the_first_commit(self, verb: str) -> None:
        if self.built:
            raise Refusal(f"{verb}: this database is built, and {LATER_COMMITS}")

    # ------------------------------------------------------------------ the document

    @property
    def declaration(self) -> str:
        """The declaration as TOML: what the SDK writes and `tessera check` reads."""
        if self._loaded_text is not None:
            return self._loaded_text
        return dumps(self._document())

    def _document(self) -> dict:
        inferred_attributes, inferred_vocabularies = self._infer_columns()
        document = self.blocks.document(
            {name: staged.declared_path for name, staged in self.sources.items()},
            self.default_source,
            inferred_attributes,
            inferred_vocabularies,
        )
        # Every source named on every block (§4.8): a view or an attribute that named none reads
        # `[defaults].source`, and writing it out is what lets a reader of `schema.toml` see the
        # whole declaration.
        for kind in ("view", "attribute"):
            for block in document.get(kind, []):
                if "source" not in block:
                    if self.default_source is None:
                        raise Refusal(
                            f"{kind} {block.get('name')!r} names no source and no source is "
                            f"staged with default=True"
                        )
                    block["source"] = self.default_source
        return document

    def _infer_columns(self) -> tuple[list[dict], list[dict]]:
        """§4.5's inference over the default source's unclaimed columns."""
        self._inference = Inference()
        default = self.default_source
        if default is None:
            return [], []
        staged = self.sources[default]
        claimed = {"entity_id", "entity"}
        for block in self.blocks.blocks["view"]:
            if block.get("source") in (default, None):
                claimed |= set(dict(block.get("fields", {})).values())
                visibility = dict(block.get("point_visibility", {}))
                if "field" in visibility:
                    claimed.add(visibility["field"])
        for block in self.blocks.blocks["layer"]:
            members = block.get("members")
            if isinstance(members, dict) and members.get("source") == default:
                claimed |= set(dict(members.get("fields", {})).values())
        declared = self.blocks.attribute_names()
        attributes: list[dict] = []
        vocabularies: list[dict] = []
        if staged.user_id_column and staged.user_id_column not in declared:
            # The user's own id, kept as an indexed keyword attribute under its own name, so a
            # record served at drill-down carries it and a pick joins back to the user's frame.
            attributes.append(
                {
                    "name": staged.user_id_column,
                    "type": "keyword",
                    "source": default,
                    "index": True,
                }
            )
            claimed.add(staged.user_id_column)
        unclaimed = [c for c in staged.columns if c not in claimed and c not in declared]
        if unclaimed:
            table = pq.read_table(staged.path, columns=unclaimed)
            inferred, inferred_vocabularies, rows = _infer.infer(table, set())
            attributes += inferred
            vocabularies += inferred_vocabularies
            self._inference = Inference(
                source=default,
                columns=rows,
                vocabularies=[v["name"] for v in inferred_vocabularies],
            )
        return attributes, vocabularies

    def write(self) -> dict:
        """Write `schema.toml` and `tessera.toml`, and return the document written."""
        document = self._document()
        self.path.mkdir(parents=True, exist_ok=True)
        (self.path / "schema.toml").write_text(dumps(document), encoding="utf-8")
        _instance.write_deployment(self.path, _instance.notebook_origins())
        _instance.secrets_for(self.path)
        self._loaded_text = None
        return document

    # ------------------------------------------------------------------ check and commit

    def check(self) -> Report:
        """`tessera check` over this directory: what the declaration reads, and what it discloses.

        Schemas only, no rows. After the first commit this plans the commit and runs the
        pre-flight, which is stage S3.
        """
        if self.built:
            raise Refusal(f"check: this database is built, and {LATER_COMMITS}")
        document = self.write()
        result = self._run(["check", "--deployment", str(self.path / "tessera.toml")])
        return Report(
            what="check",
            ok=result.returncode == 0,
            inference=self._inference,
            frames=self._frames(document),
            render_columns=render_columns_of(document.get("attribute", [])),
            notes=self._notes(),
            output=result.stdout + result.stderr,
        )

    def commit(self) -> CommitReport:
        """The first commit: `tessera check`, `tessera build --mint-external-ids`, `tessera serve`.

        Three things happen here and at no later commit, and the report says each: the frame is
        fixed, the column types and render flags are fixed, and the allocation is signature-sorted
        over the whole staged corpus (§6.1).
        """
        if self.built:
            raise Refusal(f"commit: this database is built, and {LATER_COMMITS}")
        document = self.write()
        self._refuse_an_empty_build(document)
        check = self._run(["check", "--deployment", str(self.path / "tessera.toml")])
        if check.returncode != 0:
            raise Refusal("commit: the declaration did not check\n" + check.stdout + check.stderr)
        build = self._run(
            [
                "build",
                "--deployment",
                str(self.path / "tessera.toml"),
                "--mint-external-ids",
                "--identity-file",
                str(self.path / ".tessera" / "identity.toml"),
            ]
        )
        report = CommitReport(
            what="commit",
            ok=build.returncode == 0,
            inference=self._inference,
            frames=self._frames(document),
            render_columns=render_columns_of(document.get("attribute", [])),
            notes=self._notes(),
            output=check.stdout + build.stdout + build.stderr,
            entities=len(self.id_map),
        )
        if build.returncode != 0:
            raise Refusal("commit: the build failed\n" + report.output)
        self.built = True
        self.id_map.acknowledge_all()
        self.id_map.save()
        self.serve()
        if self.listening is not None:
            report.viewer = self.listening.viewer
            report.session = self.listening.session
            report.control = self.listening.control
        return report

    def _refuse_an_empty_build(self, document: dict) -> None:
        """A first commit with no points staged needs an explicit extent on every view (§6.1)."""
        for block in document.get("view", []):
            staged = self.sources.get(block.get("source"))
            if staged is not None and staged.rows:
                continue
            if isinstance(block.get("extent"), dict):
                raise Refusal(
                    f"commit: view {block['name']!r} has no staged rows to fit a frame around. "
                    f"An empty database needs extent= on every view"
                )

    def serve(self) -> _instance.Listening:
        """Start `tessera serve` over this directory and read the addresses it bound (§7)."""
        if self._child is not None:
            return self.listening
        identity = (self.path / ".tessera" / "identity.key").read_text(encoding="utf-8").strip()
        self._child, self.listening = _instance.start(
            _instance.find_binary(), self.path / "tessera.toml", identity
        )
        return self.listening

    def _run(self, arguments: Sequence[str]) -> subprocess.CompletedProcess:
        return subprocess.run(
            [_instance.find_binary(), *arguments],
            capture_output=True,
            text=True,
            cwd=self.path,
        )

    def _frames(self, document: dict) -> list[tuple[str, str]]:
        return [
            (block["name"], _extent_in_words(block.get("extent")))
            for block in document.get("view", [])
        ]

    def _notes(self) -> list[str]:
        notes = [f"{len(self.id_map)} id(s) in the map"] if len(self.id_map) else []
        for staged in self.sources.values():
            for note in staged.notes:
                notes.append(f"source '{staged.name}': {note}")
        return notes

    # ------------------------------------------------------------------ the directory

    def save(self, path: str | os.PathLike) -> Path:
        """Copy this database out, so a temporary one survives `close()` (§2)."""
        target = Path(path).expanduser()
        if target.exists() and any(target.iterdir()):
            raise Refusal(f"save: {target} is not empty")
        shutil.copytree(self.path, target, dirs_exist_ok=True)
        return target

    def close(self) -> None:
        """Stop the child and, for a temporary database, remove the directory."""
        if self._child is not None:
            _instance.stop(self._child)
            self._child = None
            self.listening = None
        if self.temporary and self.path.exists():
            shutil.rmtree(self.path, ignore_errors=True)

    def remove(self, ids: Iterable[Hashable]) -> int:
        """Mark ids removed in the map. The `/control/changes` call is stage S3."""
        raise Refusal(f"remove: {LATER_COMMITS}")

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *exception) -> None:
        self.close()


def _extent_in_words(extent: Any) -> str:
    if isinstance(extent, dict) and extent.get("auto"):
        margin = extent.get("margin", 0.01)
        return f"fitted to the staged rows, with {margin:g} of the data span as headroom each side"
    if extent == "auto":
        return "fitted to the staged rows, squared, with the build's own margin"
    return str(extent)


# ---------------------------------------------------------------------- create and open


def create(path: str | os.PathLike | None = None, replace: bool = False) -> Database:
    """A new database, in `path` or in a temporary directory `close()` removes (§2).

    With no path the directory is on a RAM-backed filesystem where the platform has one
    (`/dev/shm` on Linux and WSL2) and on disk otherwise, and the call says which: the build reads
    and the server maps that directory, so a small corpus on the RAM-backed path touches no disk.
    """
    if path is None:
        parent = RAM_BACKED if RAM_BACKED.is_dir() else None
        directory = Path(tempfile.mkdtemp(prefix="tesseradb-", dir=parent))
        where = "a RAM-backed filesystem" if parent is not None else "disk"
        print(f"tesseradb: a temporary database at {directory}, on {where}")
        database = Database(directory, temporary=True)
    else:
        directory = Path(path).expanduser()
        if directory.exists() and any(directory.iterdir()):
            if not replace:
                raise Refusal(
                    f"create: {directory} is not empty. open() reads a saved database, and "
                    f"replace=True removes what is there first"
                )
            shutil.rmtree(directory)
        directory.mkdir(parents=True, exist_ok=True)
        database = Database(directory)
    (database.path / "sources").mkdir(parents=True, exist_ok=True)
    (database.path / ".tessera").mkdir(parents=True, exist_ok=True)
    return database


def open(path: str | os.PathLike) -> Database:  # noqa: A001 — the design's verb is `td.open`
    """A saved database: the directory, its bundle and its id map (§2).

    Its next `commit()` ingests rather than builds, the bundle being present, which is stage S3 of
    §12. `db.declaration` is the declaration on disk.

    A directory with no bundle is refused naming `create()`: the SDK does not read a declaration
    back into blocks, so an unbuilt directory cannot be added to through the verbs.
    """
    directory = Path(path).expanduser()
    if not (directory / "tessera.toml").exists():
        raise Refusal(f"open: {directory} holds no tessera.toml. create() makes a new database")
    if not (directory / "bundle" / "CURRENT").exists():
        raise Refusal(
            f"open: {directory} holds no built bundle. create(path, replace=True) starts again "
            f"from the frames and files"
        )
    database = Database(directory)
    schema = directory / "schema.toml"
    if schema.exists():
        database._loaded_text = schema.read_text(encoding="utf-8")
    return database
