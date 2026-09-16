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

After the first commit `stage` binds a delta and `commit` pages it through the control plane
(`_commit`). The verbs that are not stages, `remove`, `suppress`, `unsuppress` and `leave`, address
rows by the external ids the build minted.
"""

from __future__ import annotations

import datetime
import json
import os
import shutil
import subprocess
import tempfile
import urllib.request
from pathlib import Path
from typing import Any, Hashable, Iterable, Sequence

import pyarrow as pa
import pyarrow.parquet as pq

from . import _commit as C
from . import _declaration as D
from . import _infer, _instance
from ._auth import authorise
from ._control import Control, CommitLog
from ._idmap import IdMap
from ._refusal import Refusal
from ._reports import ChangeReport, CommitReport, Inference, PagedReport, Report, render_columns_of
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

#: How near expiry a held token may come before the next call mints another, in seconds.
TOKEN_MARGIN = 60.0

#: Where a delta's parquet goes. A delta is rows to add to what a source already holds, so it is
#: not the source's own file: `tessera check` reads `sources/` and would otherwise read a delta as
#: the whole corpus.
DELTA_FOLDER = ".tessera/deltas"

#: What a declaration verb that has no runtime route says. The emitter behind `PUT /control/layers`
#: covers layers, so a layer and a label set are declarable at any commit and the other four blocks
#: are not (§6.2 step 1, §11.2 C).
NO_RUNTIME_DECLARATION = (
    "not built yet, {verb} after the first commit. `tessera check --payloads` emits the runtime "
    "body for a layer and for nothing else, so an attribute, a vocabulary, a view or a view group "
    "declared now has no route to the running service. Declare it before the first commit, or "
    "rebuild the database with create(path, replace=True)"
)


class Database:
    """One database directory, and the declaration the SDK is building for it."""

    def __init__(self, path: Path, temporary: bool = False) -> None:
        self.path = Path(path)
        self.temporary = temporary
        self.sources: dict[str, StagedSource] = {}
        #: Rows staged since the last commit, to add to what the source already holds (§3).
        self.deltas: dict[str, StagedSource] = {}
        #: The column each `from_column` layer mints its artifacts from, which a delta carries.
        self.from_columns: dict[str, str] = {}
        self.blocks = D.Declaration()
        self.id_map = IdMap(self.path / ".tessera" / "idmap.json")
        self.commit_log = CommitLog(self.path / ".tessera" / "commit-log.json")
        self.built = (self.path / "bundle" / "CURRENT").exists()
        self._child: subprocess.Popen | None = None
        self.listening: _instance.Listening | None = None
        self._loaded_text: str | None = None
        self._inference = Inference()
        self._token = None

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

        After the first commit the same call binds a **delta**: rows to add to what the source
        already holds. The declaration says what the source feeds, so a delta on the points source
        feeds the view, the attributes and every layer minted from one of its columns, and a delta
        on a members source feeds one layer's memberships. A name the declaration does not know is
        refused.
        """
        if self.built:
            return self._stage_delta(name, data, id=id, default=default)
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

    def _stage_delta(
        self, name: str, data: Any, id: str | None = None, default: bool = False
    ) -> StagedSource:
        """Rows to add to what a source already holds (§3).

        The delta's parquet goes under `.tessera/deltas/` rather than over the source's own file:
        `tessera check` reads `[sources]` at every commit, and a delta written there would be read
        as the whole corpus. The ids go through the same map, so a user id seen before keeps its
        source id and one that is new is assigned the next.
        """
        if default:
            raise Refusal(
                f"stage: '{name}' cannot become the default source after the first commit. "
                f"`[defaults].source` is what a block that names no source reads, and every block "
                f"of a built declaration has already been read"
            )
        if name not in self.sources and not self._declaration_reads(name):
            raise Refusal(
                f"stage: no block of this declaration reads a source named {name!r}. A delta names "
                f"a source the declaration knows; declare the block that reads it first"
            )
        if self._names_entities(name) and not _frame_names_entities(data, id):
            raise Refusal(
                f"stage: '{name}' is read as naming entities and nothing in this delta names one. "
                f"The block that reads it was read at the first commit, so there is no later pass "
                f"in which the ids could be minted. Name the id column with id=, or carry an "
                f"'entity' column"
            )
        if isinstance(data, (str, os.PathLike)):
            staged = stage_path(
                name, data, self.path, self.id_map, id=id, folder=DELTA_FOLDER
            )
        else:
            staged = stage_frame(
                name,
                data,
                self.path,
                self.id_map,
                id=id,
                first_commit=False,
                folder=DELTA_FOLDER,
            )
        self.deltas[name] = staged
        self.id_map.save()
        self._save_state()
        return staged

    def _names_entities(self, name: str) -> bool:
        """Whether a block reads this source as naming entities: points, members or attributes.

        A vocabulary's values and a layer's artifacts name no entity, so a delta on one carries no
        id and is not held to one.
        """
        if name == self.default_source:
            return True
        for block in self.blocks.blocks["view"] + self.blocks.blocks["attribute"]:
            if block.get("source") == name:
                return True
        for block in self.blocks.blocks["layer"]:
            for part in (block, block.get("labels")):
                if isinstance(part, dict) and isinstance(part.get("members"), dict):
                    if part["members"].get("source") == name:
                        return True
        return False

    def _declaration_reads(self, name: str) -> bool:
        """Whether any block names this source, the label sets' own blocks included."""
        for kind in D.KINDS:
            for block in self.blocks.blocks[kind]:
                if _names_source(block, name):
                    return True
        return False

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
        if kind != "layer":
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
        """One `[[layer]]` block, at any commit (§4.6).

        A layer declared after the first commit is sent as `PUT /control/layers` at the next
        commit, with the body `tessera check --payloads` emits over this declaration. Its
        `from_column` becomes a members table under `sources/`: `tessera check` reads the
        declaration against the files each time, and a points file read in place does not carry a
        column staged since it was written.
        """
        views = kwargs.get("views")
        from_column = kwargs.get("from_column")
        if self.built and from_column is not None:
            kwargs = dict(kwargs)
            kwargs.pop("from_column")
            kwargs["members"] = self._runtime_members_source(name)
            block = D.layer_block(name, kind, self._points_source(views), **kwargs)
            block["value_set"] = "open"
        else:
            block = D.layer_block(name, kind, self._points_source(views), **kwargs)
        if from_column is not None:
            self.from_columns[name] = from_column
        return self._declared(self.blocks.add("layer", block))

    def _runtime_members_source(self, layer: str) -> str:
        """The `[sources]` name a from-column layer declared after the first commit reads."""
        name = layer.replace("/", "_") + "_members"
        if name in self.sources:
            raise Refusal(
                f"layer {layer!r} would write its membership to source {name!r}, which is already "
                f"staged. Stage the (level, key, entity) table yourself and name it in members="
            )
        return name

    def declare_labels(
        self,
        name: str,
        of: str,
        source: Any,
        members: str | None = None,
        **kwargs,
    ) -> dict:
        """A label set over a clustering: the `[layer.labels]` block on the layer `of` (§4.7).

        It expands to a flat layer of supplied content, so it is declarable at any commit on
        `declare_layer`'s terms.
        """
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
        staged = StagedSource(
            name=source_name,
            path=path,
            declared_path=f"sources/{source_name}.parquet",
            rows=table.num_rows,
            columns=dict(zip(table.schema.names, table.schema.types)),
            notes=["written from a mapping of cluster key to text"],
        )
        self.sources[source_name] = staged
        if self.built:
            # The file is in `[sources]` so `tessera check` reads it, and in the deltas so the
            # commit publishes what it holds: a built database has already read every source it
            # names, and a table written now is rows it has not seen.
            self.deltas[source_name] = staged
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
            raise Refusal(NO_RUNTIME_DECLARATION.format(verb=verb))

    # ------------------------------------------------------------------ the document

    @property
    def declaration(self) -> str:
        """The declaration as TOML: what the SDK writes and `tessera check` reads."""
        if self._loaded_text is not None:
            return self._loaded_text
        return dumps(self._document())

    def _document(self) -> dict:
        self._write_runtime_members()
        self._resolve_sources()
        inferred_attributes, inferred_vocabularies = self._infer_columns()
        # A block declared after the first commit may name a source this database has never read.
        # Its `[sources]` entry is the delta's own file: `tessera check` resolves a name rather than
        # a path, so a name the table does not carry is refused rather than read as a relative path.
        paths = {name: staged.declared_path for name, staged in self.sources.items()}
        for name, staged in self.deltas.items():
            paths.setdefault(name, staged.declared_path)
        document = self.blocks.document(
            paths,
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

    def _write_runtime_members(self) -> None:
        """The members table a from-column layer declared after the first commit reads (§4.6).

        `tessera check` reads the declaration against the files at every commit, and a points file
        read in place does not carry a column staged since it was written, so the column is written
        out as `(level, key, entity)` under `sources/`. The rows are the delta's own: an entity
        this commit creates carries its key on the points page instead.
        """
        for layer, column in self.from_columns.items():
            block = next((b for b in self.blocks.blocks["layer"] if b["name"] == layer), None)
            if block is None:
                continue
            members = block.get("members")
            if not isinstance(members, dict):
                continue
            name = members.get("source")
            if name is None or name in self.sources:
                continue
            keys: list[str] = []
            entities: list[int] = []
            for delta in self.deltas.values():
                if column not in delta.columns:
                    continue
                table = pq.read_table(delta.path)
                if column not in table.column_names or "entity_id" not in table.column_names:
                    continue
                for key, entity in zip(
                    table[column].to_pylist(), table["entity_id"].to_pylist()
                ):
                    if key is not None:
                        keys.append(str(key))
                        entities.append(int(entity))
            written = pa.table(
                {
                    "level": pa.array([0] * len(keys), pa.uint32()),
                    "key": pa.array(keys, pa.string()),
                    "entity": pa.array(entities, pa.uint64()),
                }
            )
            path = self.path / "sources" / f"{name}.parquet"
            path.parent.mkdir(parents=True, exist_ok=True)
            pq.write_table(written, path)
            self.sources[name] = StagedSource(
                name=name,
                path=path,
                declared_path=f"sources/{name}.parquet",
                rows=written.num_rows,
                columns=dict(zip(written.schema.names, written.schema.types)),
                notes=[f"column '{column}' written as layer '{layer}''s members table"],
            )

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
            "sources": {name: _stored(staged) for name, staged in self.sources.items()},
            "deltas": {name: _stored(staged) for name, staged in self.deltas.items()},
            "from_columns": dict(self.from_columns),
            "blocks": _tagged(self.blocks.blocks),
        }
        path = self.path / ".tessera" / "declaration.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(state, indent=1), encoding="utf-8")

    # ------------------------------------------------------------------ check and commit

    def check(self) -> Report | PagedReport:
        """What the next commit would do, with nothing sent (§5).

        Before the first commit that is `tessera check` over this directory: what the declaration
        reads from each file and the disclosure decisions it makes, schemas only and no rows. After
        it, the plan (§6.2) and the pre-flight (§6.3).
        """
        if self.built:
            return self._paged(sent=False)
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

    def commit(self) -> CommitReport | PagedReport:
        """Make the staged data part of the database: the build the first time, pages after (§6).

        The first commit runs `tessera check`, `tessera build --mint-external-ids` and then
        `tessera serve`. Three things happen there and at no later commit, and the report says
        each: the frame is fixed, the column types and render flags are fixed, and the allocation
        is signature-sorted over the whole staged corpus (§6.1). Every commit after it pages the
        deltas through the control plane in §6.2's order and waits for the publication that
        follows its last acknowledgement.
        """
        if self.built:
            return self._paged(sent=True)
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
            # Zero under identity mode: the points file's own ids are the source
            # ids, so the map assigned none and records them to carry their state.
            entities=0 if self.id_map.identity else len(self.id_map),
        )
        if build.returncode != 0:
            raise Refusal("commit: the build failed\n" + report.output)
        self.built = True
        if self.id_map.identity:
            for staged, _ in self._view_sources():
                column = "entity_id" if "entity_id" in staged.columns else "entity"
                if column in staged.columns:
                    self.id_map.record_identity(
                        pq.read_table(staged.path, columns=[column])[column].to_pylist()
                    )
        self.id_map.acknowledge_all()
        self.id_map.save()
        # Every layer the build compiled exists at the running service, so the next commit declares
        # only what was added after this one. The names are the emitter's, a label set expanding to
        # a layer of its own.
        self.commit_log.declare(payload["name"] for payload in self._payloads())
        # The inline roster the build compiled: recorded as published so the next commit does not
        # offer the same keys to the control plane (§6.4).
        for layer, keys in C.inline_publications(document):
            self.commit_log.publish(layer, keys)
        self.commit_log.add_terms(self._staged_terms(document))
        self.commit_log.save()
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

    # ------------------------------------------------------------------ the paged commit

    def _paged(self, sent: bool) -> PagedReport:
        """§6.2's plan over the staged deltas, run where `sent` and printed where not."""
        self.serve()
        control = self.control
        planner = C.Planner(self, control, self.meta())
        pages, findings = planner.plan()
        report = PagedReport(
            sent=sent,
            plan=[page.line for page in pages],
            findings=findings,
        )
        if not sent or not report.ok:
            return report
        C.run(self, control, pages, report)
        self.id_map.save()
        self.commit_log.add_terms(self._staged_terms(self._document()))
        self.commit_log.save()
        self.deltas.clear()
        self._save_state()
        return report

    @property
    def control(self) -> Control:
        """The operator plane of this database's own server."""
        listening = self.serve()
        credential = (self.path / ".tessera" / "operator.cred").read_text(encoding="utf-8")
        return Control(f"http://{listening.control}", credential.strip())

    def _payloads(self) -> list[dict]:
        """`tessera check --payloads` over this declaration: one `PUT /control/layers` body a layer.

        The declaration minus its acquisition keys *is* the payload (configuration.md §2), so this
        is the binary serialising what it parsed rather than a second emitter in Python.
        """
        self.write()
        result = self._run(
            ["check", "--deployment", str(self.path / "tessera.toml"), "--payloads"]
        )
        if result.returncode != 0:
            raise Refusal(
                "commit: the declaration did not check, so no layer payload was emitted\n"
                + result.stdout
                + result.stderr
            )
        return json.loads(result.stdout)

    def token(self, terms: Sequence[str] | None = None):
        """A viewer token for this database, minted from its own session credential (§8).

        With no terms it mints for every access label the SDK has staged plus each view's default
        label, which is Python asserting the local principal's authority: admissible on a
        single-operator database and nowhere else.
        """
        self.serve()
        chosen = list(terms) if terms is not None else list(self.commit_log.terms)
        return authorise(self.session_url, self.session_credential, chosen)

    def _local_token(self):
        """One token for this process, minted again when the one it holds is near expiry.

        Minting is a round trip to the session plane and a plugin call, and the pre-flight reads
        `/v1/meta` on every `check()`. The token is the local principal's, so there is one to hold.
        """
        held = self._token
        if held is not None and (held.seconds_left is None or held.seconds_left > TOKEN_MARGIN):
            return held
        self._token = self.token()
        return self._token

    def meta(self) -> dict:
        """`/v1/meta` as this database's own principal reads it: the frames and the schema."""
        token = self._local_token()
        request = urllib.request.Request(
            self.viewer_url + "/v1/meta", headers={"authorization": f"Bearer {token.token}"}
        )
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.loads(response.read())

    def _staged_terms(self, document: dict) -> list[str]:
        """Every access label this commit staged, plus each view's default label (§8)."""
        terms: list[str] = []
        for block in document.get("view", []):
            visibility = dict(block.get("point_visibility", {}))
            default = visibility.get("default")
            if default:
                terms.append(default)
            field = visibility.get("field")
            source = block.get("source") or self.default_source
            for staged in (self.sources.get(source), self.deltas.get(source)):
                if staged is None or not field or field not in staged.columns:
                    continue
                column = pq.read_table(staged.path, columns=[field])[field]
                for value in column.to_pylist():
                    if value is None:
                        continue
                    for label in value if isinstance(value, list) else [value]:
                        terms.append(str(label))
        return terms

    # ------------------------------------------------------------------ verbs that are not stages

    def remove(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Delete rows by the user's own ids (§6.5).

        A deletion leaves the overlay at the compaction that removes its rows and at no other point
        (write-path §5.4). The ids are marked removed in the map, so one staged again goes as a
        point row, which decision 0047 allows: an edit is a delete and a re-ingest.
        """
        wanted = list(ids)
        report = self._changes(wanted, "delete")
        if report.ok:
            self.id_map.remove(wanted)
            self.id_map.save()
        return report

    def suppress(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Hide rows by the user's own ids. A suppression is lifted by `unsuppress` alone (§6.5)."""
        return self._changes(ids, "suppress")

    def unsuppress(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Lift a suppression (§6.5)."""
        return self._changes(ids, "unsuppress")

    def _mapped(self, ids: Iterable[Hashable]) -> tuple[list[int], list[Hashable]]:
        """The source id of each user id, and the ids this map has never seen.

        An id the map does not hold addresses nothing: every route here names a row by the external
        id minted from its source id, so an unmapped id is reported rather than sent.
        """
        source_ids: list[int] = []
        unknown: list[Hashable] = []
        for user_id in ids:
            source_id = self.id_map.source_id_of(user_id)
            if source_id is None:
                unknown.append(user_id)
            else:
                source_ids.append(source_id)
        return source_ids, unknown

    def _changes(self, ids: Iterable[Hashable], op: str) -> ChangeReport:
        self._refuse_before_the_first_commit(op)
        source_ids, unknown = self._mapped(ids)
        report = ChangeReport(op=op, requested=len(source_ids), unknown=unknown)
        control = self.control
        for answer in C.changes(control, source_ids, op, control.limits()):
            if not answer.ok:
                report.refusals.append({"status": answer.status, "detail": answer.detail[:1000]})
        return report

    def leave(
        self,
        layer: str,
        key: str,
        ids: Iterable[Hashable],
        rank: int = 0,
        level: int = 0,
    ) -> ChangeReport:
        """Shrink a content's generating set, the one set that may (decision 0135, §6.5).

        A page that empties a set withdraws the content: the record is removed and the caller
        supplies it again rather than refilling the set.
        """
        self._refuse_before_the_first_commit("leave")
        source_ids, unknown = self._mapped(ids)
        report = ChangeReport(op=f"leave {layer}/{key} rank {rank}", requested=len(source_ids),
                              unknown=unknown)
        answer = C.leave(self.control, layer, key, source_ids, rank, level)
        if not answer.ok:
            report.refusals.append({"status": answer.status, "detail": answer.detail[:1000]})
        return report

    def _refuse_before_the_first_commit(self, verb: str) -> None:
        if not self.built:
            raise Refusal(
                f"{verb}: this database has not been committed, so there are no rows to address. "
                f"commit() builds it first"
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

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *exception) -> None:
        self.close()


def _frame_names_entities(data: Any, id: str | None) -> bool:
    """Whether a staged frame or file carries something that names an entity."""
    if id is not None:
        return True
    if isinstance(data, (str, os.PathLike)):
        schema = pq.ParquetFile(Path(data).expanduser()).schema_arrow
        return any(column in schema.names for column in ("entity", "entity_id"))
    from ._sources import ENTITY_COLUMNS, is_pandas_frame

    names = list(data.columns) if is_pandas_frame(data) else list(pa.table(data).column_names)
    if any(column in names for column in ENTITY_COLUMNS):
        return True
    return is_pandas_frame(data) and data.index.name is not None


def _names_source(block: Any, name: str) -> bool:
    """Whether a declaration block, or anything nested in it, names this source."""
    if isinstance(block, dict):
        if block.get("source") == name:
            return True
        return any(_names_source(value, name) for value in block.values())
    if isinstance(block, list):
        return any(_names_source(value, name) for value in block)
    return False


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


def _stored(staged: StagedSource) -> dict:
    """One staged source as the SDK's own JSON copy holds it."""
    return {
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
        "user_ids": list(staged.user_ids),
    }


def _restored(name: str, source: dict) -> StagedSource:
    return StagedSource(
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
        user_ids=list(source.get("user_ids", [])),
    )


def _load(database: Database, state: dict) -> None:
    for name, source in state.get("sources", {}).items():
        database.sources[name] = _restored(name, source)
    for name, source in state.get("deltas", {}).items():
        database.deltas[name] = _restored(name, source)
    database.from_columns = dict(state.get("from_columns", {}))
    for kind, blocks in state.get("blocks", {}).items():
        database.blocks.blocks[kind] = [_untagged(block) for block in blocks]


#: How an inline table is marked in the SDK's JSON copy. A plain dict is a block of its own, and
#: reading one back as the other would move `extent` out of its view's table.
INLINE = "__inline__"

#: How a TOML offset date-time is marked in the same copy: a view group's metadata carries them,
#: and JSON has no spelling for one.
MOMENT = "__moment__"


def _tagged(value: Any) -> Any:
    if isinstance(value, Inline):
        return {INLINE: {k: _tagged(v) for k, v in value.items()}}
    if isinstance(value, dict):
        return {k: _tagged(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_tagged(v) for v in value]
    if isinstance(value, datetime.datetime):
        return {MOMENT: value.isoformat()}
    return value


def _untagged(value: Any) -> Any:
    if isinstance(value, dict):
        if set(value) == {INLINE}:
            return Inline({k: _untagged(v) for k, v in value[INLINE].items()})
        if set(value) == {MOMENT}:
            return datetime.datetime.fromisoformat(value[MOMENT])
        return {k: _untagged(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_untagged(v) for v in value]
    return value
