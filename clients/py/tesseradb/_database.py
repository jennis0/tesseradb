"""A Tessera database in a directory: create it, fill it, commit it (python-sdk.md §2, §6).

The directory is everything `tessera build` and `tessera serve` read, so a notebook prototype
becomes a deployment by copying it: `tessera serve --deployment <dir>/tessera.toml` serves the
same database from wherever it was copied to.

Three verbs carry the model. `stage` binds a named source to a frame or a file, `declare_*` adds a
block to the declaration and names the sources it reads, and `commit` makes the staged data part
of the database. `check` is `commit` with nothing sent.

Beside the declaration the SDK writes, it keeps its own copy of the blocks as JSON under
`.tessera/`, written at every staging and every declaration, so `open()` reads them back without
parsing TOML and a database saved before its first commit reopens where it was left.

Not built yet: the commit that pages a delta through the control plane. The first commit builds,
and a verb that would start a later one says so and names what it would take.
"""

from __future__ import annotations

import json
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
from ._refusal import Refusal
from ._reports import CommitReport, Inference, Report, render_columns_of
from ._sources import (
    StagedSource,
    copy_column,
    is_integer_type,
    mint_entity_ids,
    stage_frame,
    stage_path,
)
from ._toml import Inline, dumps

#: A temporary database goes here when the platform has a RAM-backed filesystem (§2).
RAM_BACKED = Path("/dev/shm")

LATER_COMMIT = (
    "not built yet, a commit into a built database: a delta pages through the control plane, and "
    "only the first commit builds"
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
        reads; a path is recorded and read where it lies where its ids already are what the build
        reads. Staging a name twice before the first commit replaces the earlier data.
        """
        if isinstance(data, (str, os.PathLike)):
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
        self._save_state()
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
        and this is the way to write a key whose verb is not built yet.
        """
        self._refuse_a_later_commit("declare")
        return self._declared(self.blocks.add(kind, block))

    def declare_view(self, name: str, source: str | None = None, **kwargs) -> dict:
        self._refuse_a_later_commit("declare_view")
        return self._declared(self.blocks.add("view", D.view_block(name, source, **kwargs)))

    def declare_view_group(self, *args, **kwargs):
        raise Refusal(
            "not built yet, declare_view_group. declare('view_group', block) writes the block as "
            "given"
        )

    def declare_vocabulary(self, name: str, **kwargs) -> dict:
        self._refuse_a_later_commit("declare_vocabulary")
        return self._declared(self.blocks.add("vocabulary", D.vocabulary_block(name, **kwargs)))

    def declare_attribute(self, name: str, type: str, **kwargs) -> dict:
        self._refuse_a_later_commit("declare_attribute")
        return self._declared(
            self.blocks.add("attribute", D.attribute_block(name, type, **kwargs))
        )

    def declare_layer(self, name: str, kind: str, **kwargs) -> dict:
        self._refuse_a_later_commit("declare_layer")
        views = kwargs.get("views")
        block = D.layer_block(name, kind, self._points_source(views), **kwargs)
        return self._declared(self.blocks.add("layer", block))

    def declare_labels(
        self,
        name: str,
        of: str,
        source: Any,
        members: str | None = None,
        **kwargs,
    ) -> dict:
        """A label set over a clustering: the `[layer.labels]` block on the layer `of` (§4.7)."""
        self._refuse_a_later_commit("declare_labels")
        parent = self.blocks.layer(of)
        if "labels" in parent:
            raise Refusal(
                f"layer {of!r} already carries a label set. A second one is a `[[layer]]` of its "
                f"own; write it through declare_layer"
            )
        if isinstance(source, dict):
            source = self._stage_label_text(name, source)
        parent["labels"] = D.labels_block(name, source, members=members, **kwargs)
        self._save_state()
        return parent["labels"]

    def _declared(self, block: dict) -> dict:
        self._save_state()
        return block

    def _stage_label_text(self, name: str, mapping: dict) -> str:
        """A mapping from cluster key to text, as the `(key, contents)` table the block reads."""
        source_name = name.replace("/", "_")
        if source_name in self.sources:
            raise Refusal(
                f"labels {name!r} would write its text to source {source_name!r}, which is "
                f"already staged. Stage the (key, contents) table yourself and name it in source="
            )
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

    def _refuse_a_later_commit(self, verb: str) -> None:
        if self.built:
            raise Refusal(f"{verb}: {LATER_COMMIT}")

    # ------------------------------------------------------------------ the document

    @property
    def declaration(self) -> str:
        """The declaration as TOML: what the SDK writes and `tessera check` reads."""
        if self._loaded_text is not None:
            return self._loaded_text
        return dumps(self._document())

    def _document(self) -> dict:
        self._resolve_sources()
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

    def _resolve_sources(self) -> None:
        """Settle what each staged source is, now that the declaration says what reads it.

        Three things are decided here rather than at staging, because staging does not know which
        source is a view's points and which is a layer's members: whether the map is the identity
        over a points file read in place, which staged frames need the `entity_id` column a block
        that reads them requires, and which second view needs the first view's access column.
        """
        for staged, _ in self._view_sources():
            if staged.in_place and not self.id_map.identity:
                entity = next(
                    (c for c in ("entity_id", "entity") if c in staged.columns),
                    None,
                )
                if entity is not None and is_integer_type(staged.columns[entity]):
                    self.id_map.use_identity(staged.name)
        for staged, _ in self._view_sources():
            if staged.pending_ids:
                mint_entity_ids(staged, self.id_map, "a view's points")
        for block in self.blocks.blocks["layer"]:
            members = block.get("members")
            staged = self.sources.get(members.get("source")) if isinstance(members, dict) else None
            if staged is not None and staged.pending_ids:
                mint_entity_ids(staged, self.id_map, "a layer's members")
        self._copy_access_columns()
        self.id_map.save()

    def _view_sources(self) -> list[tuple[StagedSource, dict]]:
        pairs = []
        for block in self.blocks.blocks["view"]:
            staged = self.sources.get(block.get("source") or self.default_source)
            if staged is not None:
                pairs.append((staged, block))
        return pairs

    def _copy_access_columns(self) -> None:
        """A second view over the same entities carries the first view's labels (§4.2).

        The build refuses an entity whose labels disagree between views, so a frame that lacks the
        column has it joined in by id from the view that holds it.
        """
        holder: StagedSource | None = None
        for staged, block in self._view_sources():
            access = dict(block.get("point_visibility", {})).get("field")
            if access is None:
                continue
            if access in staged.columns:
                holder = holder or staged
                continue
            if holder is None:
                # No view holds the column, so there is nothing to copy from. `tessera check`
                # refuses it naming the object, the field and the columns the file does carry.
                continue
            copy_column(holder, staged, access)

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

    def _save_state(self) -> None:
        """The SDK's own copy of the declaration, so `open()` reads the blocks back (§2)."""
        state = {
            "sources": {
                name: {
                    "path": str(staged.path),
                    "declared_path": staged.declared_path,
                    "default": staged.default,
                    "in_place": staged.in_place,
                    "user_id_column": staged.user_id_column,
                    "default_index": staged.default_index,
                    "index_available": staged.index_available,
                    "pending_ids": staged.pending_ids,
                    "rows": staged.rows,
                    "columns": {c: str(t) for c, t in staged.columns.items()},
                    "notes": staged.notes,
                }
                for name, staged in self.sources.items()
            },
            "blocks": _tagged(self.blocks.blocks),
        }
        path = self.path / ".tessera" / "declaration.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(state, indent=1), encoding="utf-8")

    # ------------------------------------------------------------------ check and commit

    def check(self) -> Report:
        """`tessera check` over this directory: what the declaration reads, and what it discloses.

        Schemas only, no rows. Not built yet: the plan and the pre-flight a later commit checks,
        which need the control plane; this is the first commit's check.
        """
        self._refuse_a_later_commit("check")
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
        self._refuse_a_later_commit("commit")
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
            extent = block.get("extent")
            fitted = extent == "auto" or (isinstance(extent, dict) and extent.get("auto"))
            if fitted:
                raise Refusal(
                    f"commit: view {block['name']!r} has no staged rows to fit a frame around. "
                    f"An empty database needs extent= on every view"
                )

    def serve(self) -> _instance.Listening:
        """Start `tessera serve` over this directory and read the addresses it bound (§7)."""
        if self._child is not None:
            return self.listening
        identity = (self.path / ".tessera" / "identity.key").read_text(encoding="utf-8").strip()
        binary, _ = _instance.find_binary()
        self._child, self.listening = _instance.start(
            binary, self.path / "tessera.toml", identity
        )
        return self.listening

    @property
    def session_credential(self) -> str:
        """The credential this database mints its tokens with, which stays in the kernel (§8)."""
        return (self.path / ".tessera" / "session.cred").read_text(encoding="utf-8").strip()

    @property
    def viewer_url(self) -> str | None:
        return None if self.listening is None else f"http://{self.listening.viewer}"

    @property
    def session_url(self) -> str | None:
        return None if self.listening is None else f"http://{self.listening.session}"

    def _run(self, arguments: Sequence[str]) -> subprocess.CompletedProcess:
        binary, _ = _instance.find_binary()
        return subprocess.run(
            [binary, *arguments],
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
        notes = []
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
        """Not built yet: `remove(ids)` sends `/control/changes`, which a later commit pages.

        The ids the map holds are marked removed at that point, and a removed id staged again goes
        as a point row (decision 0047).
        """
        raise Refusal(f"remove: {LATER_COMMIT}")

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
                    f"replace=True removes a Tessera database that is there first"
                )
            if not (directory / "tessera.toml").exists():
                raise Refusal(
                    f"create: {directory} is not empty and holds no tessera.toml, so it is not a "
                    f"Tessera database. replace=True removes a database, never a directory of "
                    f"somebody else's files"
                )
            shutil.rmtree(directory)
        directory.mkdir(parents=True, exist_ok=True)
        database = Database(directory)
    (database.path / "sources").mkdir(parents=True, exist_ok=True)
    (database.path / ".tessera").mkdir(parents=True, exist_ok=True)
    print(f"tesseradb: {_binary_in_words()}")
    return database


def _binary_in_words() -> str:
    try:
        binary, where = _instance.find_binary()
    except Refusal as why:
        return str(why)
    return f"the binary is {binary} (from {where})"


def open(path: str | os.PathLike) -> Database:  # noqa: A001, the design's verb is `td.open`
    """A saved database: the directory, its sources, its declaration and its id map (§2).

    A database that has committed reopens built, and its next commit ingests; one saved before its
    first commit reopens where it was left, the SDK's own copy of the blocks being what it reads
    rather than the TOML it wrote.
    """
    directory = Path(path).expanduser()
    if not (directory / "tessera.toml").exists():
        raise Refusal(f"open: {directory} holds no tessera.toml. create() makes a new database")
    database = Database(directory)
    state = directory / ".tessera" / "declaration.json"
    if state.exists():
        _load(database, json.loads(state.read_text(encoding="utf-8")))
    elif (directory / "schema.toml").exists():
        database._loaded_text = (directory / "schema.toml").read_text(encoding="utf-8")
    return database


def _arrow_type(alias: str):
    """The Arrow type a column carried, or its name where pyarrow spells no alias for it."""
    try:
        return pa.type_for_alias(alias)
    except ValueError:
        return alias


def _load(database: Database, state: dict) -> None:
    for name, source in state.get("sources", {}).items():
        database.sources[name] = StagedSource(
            name=name,
            path=Path(source["path"]),
            declared_path=source["declared_path"],
            default=source["default"],
            in_place=source["in_place"],
            user_id_column=source["user_id_column"],
            default_index=source["default_index"],
            index_available=source["index_available"],
            pending_ids=source["pending_ids"],
            rows=source["rows"],
            columns={c: _arrow_type(t) for c, t in source["columns"].items()},
            notes=list(source["notes"]),
        )
    for kind, blocks in state.get("blocks", {}).items():
        database.blocks.blocks[kind] = [_untagged(block) for block in blocks]


#: How an inline table is marked in the SDK's JSON copy. A plain dict is a block of its own, and
#: reading one back as the other would move `extent` out of its view's table.
INLINE = "__inline__"


def _tagged(value: Any) -> Any:
    if isinstance(value, Inline):
        return {INLINE: {k: _tagged(v) for k, v in value.items()}}
    if isinstance(value, dict):
        return {k: _tagged(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_tagged(v) for v in value]
    return value


def _untagged(value: Any) -> Any:
    if isinstance(value, dict):
        if set(value) == {INLINE}:
            return Inline({k: _untagged(v) for k, v in value[INLINE].items()})
        return {k: _untagged(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_untagged(v) for v in value]
    return value
