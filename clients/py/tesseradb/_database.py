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
rows by the bytes their id column holds, or by their `tessera_id` where the declaration names no id
column (§3).

**The SDK holds nothing about what the database contains.** There is no id map and no commit log: a
row is named by its id column or by its `tessera_id`, and what the database already holds is asked
of the database: `/v1/meta` for the views and layers it carries, and the routes' own answers for
everything else. A re-run of a cell is a re-run, and the server's refusal is what the report
carries.
"""

from __future__ import annotations

import datetime
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any, Hashable, Iterable, Sequence

import pyarrow as pa
import pyarrow.parquet as pq

from . import _commit as C
from . import _declaration as D
from . import _infer, _instance
from ._auth import authorise
from ._control import Control, addressed
from ._refusal import Refusal
from ._reports import ChangeReport, CommitReport, Inference, PagedReport, Report, render_columns_of
from ._sources import StagedSource, is_integer_type, stage_frame, stage_path
from ._viewer import Viewer
from ._toml import Inline, dumps

#: A temporary database goes here when the platform has a RAM-backed filesystem (§2).
RAM_BACKED = Path("/dev/shm")

#: Where a delta's parquet goes. A delta is rows to add to what a source already holds, so it is
#: not the source's own file: `tessera check` reads `sources/` and would otherwise read a delta as
#: the whole corpus.
DELTA_FOLDER = ".tessera/deltas"

class Database:
    """One database directory, and the declaration the SDK is building for it."""

    def __init__(self, path: Path, temporary: bool = False) -> None:
        self.path = Path(path)
        self.temporary = temporary
        self.sources: dict[str, StagedSource] = {}
        #: Rows staged since the last commit, to add to what the source already holds (§3).
        self.deltas: dict[str, StagedSource] = {}
        self.blocks = D.Declaration()
        #: Every access label this database has staged, plus each view's default (§8). Computed at
        #: each commit from the staged access columns and kept in the SDK's JSON declaration copy,
        #: which is the one thing `map()` needs and no route answers.
        self.terms: list[str] = []
        self.built = (self.path / "bundle" / "CURRENT").exists()
        self._child: subprocess.Popen | None = None
        self.listening: _instance.Listening | None = None
        self._loaded_text: str | None = None
        self._inference = Inference()
        #: The all-terms viewer this database reads itself through, made on first use (§8).
        self._viewer: Viewer | None = None

    # ------------------------------------------------------------------ sources

    def stage(
        self,
        name: str,
        data: Any,
        id: str | None = None,
        default: bool = False,
    ) -> StagedSource:
        """Bind `name` in `[sources]` to a frame or a file (§3).

        A frame is written to `sources/<name>.parquet` as it was staged; a path is recorded and
        read where it lies. `id` names the column that names the rows, which the declaration then
        points at; without it the SDK reads `id`, `entity_id` or `entity`, and a source carrying
        none of them names its rows by their `tessera_id`. Staging a name twice before the first
        commit replaces the earlier data.

        After the first commit the same call binds a **delta**: rows to add to what the source
        already holds. The declaration says what the source feeds, so a delta on the points source
        feeds the view, the attributes and every layer minted from one of its columns, and a delta
        on a members source feeds one layer's memberships. A name the declaration does not know is
        refused.
        """
        if self.built:
            return self._stage_delta(name, data, id=id, default=default)
        if isinstance(data, (str, os.PathLike)):
            staged = stage_path(name, data, self.path, id=id, default=default)
        else:
            staged = stage_frame(name, data, self.path, id=id, default=default)
        if default:
            for other in self.sources.values():
                if other.default:
                    other.default = False
                    staged.notes.append(f"the default source, replacing '{other.name}'")
        self.sources[name] = staged
        self._save_state()
        return staged

    def _stage_delta(
        self, name: str, data: Any, id: str | None = None, default: bool = False
    ) -> StagedSource:
        """Rows to add to what a source already holds (§3).

        The delta's parquet goes under `.tessera/deltas/` rather than over the source's own file:
        `tessera check` reads `[sources]` at every commit, and a delta written there would be read
        as the whole corpus. The rows are sent as they were staged: a row the database already
        holds is refused by the server, whole page, and the report says so (§3).
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
        if isinstance(data, (str, os.PathLike)):
            staged = stage_path(name, data, self.path, id=id, folder=DELTA_FOLDER)
        else:
            staged = stage_frame(name, data, self.path, id=id, folder=DELTA_FOLDER)
        held = self.sources.get(name)
        if held is not None and held.id_column and staged.id_column != held.id_column:
            raise Refusal(
                f"stage: '{name}' names its rows by '{held.id_column}', which this delta does not "
                f"carry. The declaration reads identity from that column, and a row this delta "
                f"does not name is a row no member table and no value can reach. Name the id "
                f"column with id="
            )
        if held is not None and held.id_column is None and staged.id_column is not None:
            raise Refusal(
                f"stage: '{name}' names its rows by nothing, so every row of this database is "
                f"addressed by the tessera_id the server hands back, and the bundle carries no "
                f"external id for '{staged.id_column}' to match. Drop the column from the frame, "
                f"or rebuild the database with an id column"
            )
        self.deltas[name] = staged
        self._save_state()
        return staged

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
        and this is the way to write a key no typed verb has a parameter for.
        """
        if kind == "attribute":
            self._refuse_a_render_column(block.get("name"), block.get("render"))
            self._mark_a_filled_column(block)
        return self._declared(self.blocks.add(kind, block))

    def declare_view(self, name: str, source: str | None = None, **kwargs) -> dict:
        """One `[[view]]` block, at any commit (§4.2).

        A view declared after the first commit is sent as `PUT /control/views/{name}` at the next
        commit, with the body `tessera check --payloads` emits over this declaration. A frame is
        fixed for the life of a view and the route has no rows to fit one against, so such a view
        declares `extent=`; an `auto` frame reaches the route as written and is refused there.
        """
        return self._declared(self.blocks.add("view", D.view_block(name, source, **kwargs)))

    def declare_view_group(self, name: str, **kwargs) -> dict:
        """One `[[view_group]]` block, at any commit (§4.3).

        A group declared after the first commit is sent as `PUT /control/view_groups/{name}`, and
        each view of its roster as `PUT /control/views/{group}/{key}`, the group first, since a
        create resolves its group. `add_view` adds a key to a group that already exists.
        """
        block = D.view_group_block(name, **kwargs)
        for record in block.get("view") or []:
            self._refuse_an_unstaged_roster_source(name, record)
        return self._declared(self.blocks.add("view_group", block))

    def add_view(
        self,
        group: str,
        key: str,
        source: str | None = None,
        visibility: Any = None,
        **metadata,
    ) -> dict:
        """One more view of a declared group: the roster record (§4.3, views.md §3.2).

        Before the first commit it is a `[[view_group.view]]` block the build compiles; after it,
        `PUT /control/views/{group}/{key}` at the next commit. The record is immutable, so every
        metadata name the group declared is carried here and a wrong one is a drop and a recreate.
        """
        block = self.blocks.group(group)
        if block.get("members"):
            raise Refusal(
                f"view group {group!r}: its views are {block['members']!r}'s, and a key belongs to "
                f"the group that owns it. Add the view to {block['members']!r}; creating a key "
                f"there creates it here"
            )
        if block.get("source"):
            raise Refusal(
                f"view group {group!r}: its views are the distinct values of its own "
                f"'{dict(block.get('fields', {})).get('view')}' column, so a view added by hand "
                f"would be a second roster. Stage the rows that name the key"
            )
        record = D.roster_record(
            group, key, source, visibility, metadata, dict(block.get("metadata") or {})
        )
        if any(held["key"] == key for held in block.get("view") or []):
            raise Refusal(
                f"view group {group!r}: a view keyed {key!r} is already declared. A roster record "
                f"is immutable: drop the key and recreate it under the record you want"
            )
        self._refuse_an_unstaged_roster_source(group, record)
        block.setdefault("view", []).append(record)
        self._save_state()
        return record

    def _refuse_an_unstaged_roster_source(self, group: str, record: dict) -> None:
        """A view of a group is its own points file, and `[defaults].source` does not reach one."""
        if record.get("source") is None:
            raise Refusal(
                f"view group {group!r}, view {record['key']!r}: under a roster of inline views the "
                f"file is the view, and `[defaults].source` does not reach a group. Give source="
            )

    def declare_vocabulary(self, name: str, **kwargs) -> dict:
        """One `[[vocabulary]]` block, at any commit (§4.4).

        A vocabulary declared after the first commit is sent as
        `PUT /control/vocabularies/{name}` at the next commit, with the body `tessera check
        --payloads` emits over this declaration. A closed set's values follow as
        `PATCH /control/vocabularies/{name}/values`, from the `values=` list or from the `source=`
        table's `(key, title?)` rows; an open set needs nothing more, its codes being minted from
        the values that arrive.
        """
        return self._declared(self.blocks.add("vocabulary", D.vocabulary_block(name, **kwargs)))

    def declare_attribute(self, name: str, type: str, **kwargs) -> dict:
        """One `[[attribute]]` block (§4.5).

        `scope={"group": name}` makes it a family of columns, one per view of that group, and
        `fields={"view": column}` says where a source of its own carries the view each value
        belongs to.

        An attribute declared after the first commit is sent as `PUT /control/attributes` at the
        next commit, with the body `tessera check --payloads` emits over this declaration; the
        column reads absent on every entity that predates it, and a delta on the attribute's source
        fills it through `POST /control/values` as any values delta does. `render=True` is the one
        such attribute the route refuses, and the refusal is here.
        """
        block = D.attribute_block(name, type, **kwargs)
        self._refuse_a_render_column(name, block.get("render"))
        self._refuse_an_undeclared_group("attribute", name, block)
        self._mark_a_filled_column(block)
        return self._declared(self.blocks.add("attribute", block))

    def _mark_a_filled_column(self, block: dict) -> None:
        """An attribute declared at a running service is filled, not read (§6.2 step 1).

        The mark is kept beside the block and never written: what it decides is that the written
        declaration names no source for this column, since the file the first commit built from has
        never carried it. `tessera check` takes such a block as a note and emits its payload, which
        is what the next commit declares.
        """
        if self.built and "source" not in block and not block.get("scope"):
            block[D.FILLED] = True

    def _refuse_a_render_column(self, name: Any, render: Any) -> None:
        """A render column belongs to the first commit (decision 0136's amendment, §4.5).

        `PUT /control/attributes` refuses `render: true` whatever the type: a rendered value is
        served from the hot column of the row that carries it, and the route declares a column
        against entities rather than rows. The rows this database holds have no slot for one, so
        the refusal is at the verb, where the declaration is still the user's to change.
        """
        if not (self.built and render):
            return
        raise Refusal(
            f"attribute {name!r}: render=True is fixed at the first commit. A rendered value is "
            f"served from the hot column of the row that carries it, and PUT /control/attributes "
            f"declares a column against entities that already exist, so it refuses one (decision "
            f"0136's amendment). Declare it with index=True, which is filterable and drawn at "
            f"drill-down, or rebuild the database with create(path, replace=True)"
        )

    def _refuse_an_undeclared_group(self, kind: str, name: str, block: dict) -> None:
        """A scope names the group that owns the views its values or artifacts are keyed by."""
        scope = block.get("scope")
        group = scope.get("group") if isinstance(scope, dict) else None
        if group is None or group in self.blocks.group_names():
            return
        raise Refusal(
            f"{kind} {name!r}: scope names view group {group!r}, which this declaration does not "
            f"carry. declare_view_group({group!r}, …) before the block scoped to it"
        )

    def declare_layer(self, name: str, kind: str, **kwargs) -> dict:
        """One `[[layer]]` block, at any commit (§4.6).

        A layer declared after the first commit is sent as `PUT /control/layers` at the next
        commit, with the body `tessera check --payloads` emits over this declaration. `from_column`
        is refused there: the column is read at the build and by the ingest route, so a layer over
        rows the database already holds is published through an artifacts table (§6.2 step 3).
        """
        views = kwargs.get("views")
        from_column = kwargs.get("from_column")
        if from_column is not None and isinstance(kwargs.get("scope"), dict):
            # One key per point says nothing about which view an artifact belongs to, and the
            # block's own refusal names the tables that do. Asked before the one below, which
            # would otherwise answer a scoped layer with the wrong remedy.
            D.layer_block(name, kind, None, from_column=from_column, scope=kwargs["scope"],
                          fields=kwargs.get("fields"))
        if self.built and from_column is not None:
            raise Refusal(
                f"layer {name!r}: from_column= mints its artifacts from the rows that carry the "
                f"column, at the build and on the ingest route, and the rows this database holds "
                f"were read at the first commit. Publish the clustering through its tables: "
                f"declare_layer({name!r}, kind=..., source=<artifacts>, members=<members>)"
            )
        source = self._points_source(views)
        if from_column is not None:
            kwargs = dict(kwargs)
            kwargs["entity_field"] = self._id_column_of(source)
        block = D.layer_block(name, kind, source, **kwargs)
        self._refuse_an_undeclared_group("layer", name, block)
        return self._declared(self.blocks.add("layer", block))

    def _id_column_of(self, source: str | None) -> str | None:
        """The column a staged source names its rows by, or `None` where it names none (§3)."""
        staged = self.sources.get(source) or self.deltas.get(source)
        return None if staged is None else staged.id_column

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
            source = self._stage_label_text(name, of, source)
        parent["labels"] = D.labels_block(name, source, members=members, **kwargs)
        self._save_state()
        return parent["labels"]

    def _declared(self, block: dict) -> dict:
        self._save_state()
        return block

    def _stage_label_text(self, name: str, of: str, mapping: dict) -> str:
        """A mapping from cluster key to text, as the artifacts table the block reads (§4.7).

        Each row carries the attachment as well as the text: a label set expands to a layer that
        depends on its clustering, and every artifact such a layer publishes attaches to one, so a
        row naming no `attached_layer` and `attached_key` is refused at the build. The key the
        mapping gives is the cluster's, which is what the label attaches to and what names the
        label's own artifact in its own layer.
        """
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
                "attached_layer": pa.array([of] * len(mapping), type=pa.string()),
                "attached_key": pa.array([str(k) for k in mapping], type=pa.string()),
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

    # ------------------------------------------------------------------ the document

    @property
    def declaration(self) -> str:
        """The declaration as TOML: what the SDK writes and `tessera check` reads."""
        if self._loaded_text is not None:
            return self._loaded_text
        return dumps(self._document())

    def _document(self) -> dict:
        inferred_attributes, inferred_vocabularies = self._infer_columns()
        # A block declared after the first commit may name a source this database has never read.
        # Its `[sources]` entry is the delta's own file: `tessera check` resolves a name rather than
        # a path, so a name the table does not carry is refused rather than read as a relative path.
        paths = {name: staged.declared_path for name, staged in self.sources.items()}
        for name, staged in self.deltas.items():
            paths.setdefault(name, staged.declared_path)
        document = self.blocks.document(paths, inferred_attributes, inferred_vocabularies)
        # Every source named on every block (§4.8). `default=True` is the SDK's own convenience and
        # is written here rather than under `[defaults]`: the file an object reads is on the
        # object, which is what lets a reader of `schema.toml` see the whole declaration. A
        # group-scoped attribute is the exception, and the one the rule would break: its values are
        # read from each of its group's views' own points files (configuration.md §1).
        for kind in ("view", "attribute"):
            for block in document.get(kind, []):
                if block.pop(D.FILLED, False):
                    # An attribute declared at a running service names no source: its column is
                    # filled through `POST /control/values` and read from no file (§6.2 step 1).
                    continue
                if "source" in block or block.get("scope"):
                    continue
                if self.default_source is None:
                    raise Refusal(
                        f"{kind} {block.get('name')!r} names no source and no source is "
                        f"staged with default=True"
                    )
                block["source"] = self.default_source
        # Where identity is, block by block: the SDK rewrites no file, so a column staged under
        # the user's own name is named here rather than copied into a canonical one (§3).
        D.name_identity(document, self._id_column_of)
        self._refuse_a_view_without_its_labels(document)
        return document

    def _refuse_a_view_without_its_labels(self, document: dict) -> None:
        """A view whose points file does not carry the access column it names (§4.2).

        Every view over the same entities carries the same labels: the build refuses an entity
        whose labels disagree between views, and the ingest route refuses a join row whose labels
        differ from the held ones. The SDK copies no column between views, so a frame that lacks
        the one its view names is refused here, naming it.
        """
        for entry in D.view_entries(document, self.default_source):
            field = entry["point_visibility"].get("field")
            staged = self.sources.get(entry["source"])
            if not field or staged is None or field in staged.columns:
                continue
            raise Refusal(
                f"view {entry['id'] or entry['group']!r}: its labels are in column '{field}', "
                f"which source '{entry['source']}' does not carry. Every view over one entity "
                f"carries that entity's labels, and a row whose labels disagree between views is "
                f"refused at the build and on the ingest route. Add '{field}' to the frame"
            )

    def view_entries(self) -> list[dict]:
        """Every view this declaration carries, plain and grouped, as the commit works from it."""
        return D.view_entries(
            {"view": self.blocks.blocks["view"], "view_group": self.blocks.blocks["view_group"]},
            self.default_source,
        )

    def _infer_columns(self) -> tuple[list[dict], list[dict]]:
        """§4.5's inference over the default source's unclaimed columns."""
        self._inference = Inference()
        default = self.default_source
        if default is None:
            return [], []
        staged = self.sources[default]
        # The id column is this source's identity rather than one of its columns: the declaration
        # points at it and the build takes its bytes as the external id (§3).
        claimed = {staged.id_column} if staged.id_column else set()
        for entry in self.view_entries():
            if entry["source"] != default:
                continue
            claimed |= {entry["x"], entry["y"]}
            if entry["discriminator"]:
                claimed.add(entry["discriminator"])
            visibility = entry["point_visibility"]
            if "field" in visibility:
                claimed.add(visibility["field"])
        for block in self.blocks.blocks["attribute"]:
            # A group-scoped column with no source is read from each view's own points file, so it
            # is claimed on the default source wherever that file is one of them.
            if block.get("scope") and "source" not in block:
                claimed.add(block.get("field") or block["name"])
        for block in self.blocks.blocks["layer"]:
            members = block.get("members")
            if isinstance(members, dict) and members.get("source") == default:
                claimed |= set(dict(members.get("fields", {})).values())
        declared = self.blocks.attribute_names()
        attributes: list[dict] = []
        vocabularies: list[dict] = []
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
        _instance.write_deployment(self.path)
        _instance.secrets_for(self.path)
        self._loaded_text = None
        return document

    def _save_state(self) -> None:
        """The SDK's own copy of the declaration, so `open()` reads the blocks back (§2)."""
        state = {
            "sources": {name: _stored(staged) for name, staged in self.sources.items()},
            "deltas": {name: _stored(staged) for name, staged in self.deltas.items()},
            "terms": list(self.terms),
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

        The first commit runs `tessera check`, then `tessera build`, then `tessera serve`. Three
        things happen there and at no later commit, and the report says each: the frame is fixed,
        the column types and render flags are fixed, and the allocation is signature-sorted over
        the whole staged corpus (§6.1). Every commit after it pages the deltas through the control
        plane in §6.2's order, flushes once and waits for the publication that flush arms.
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
                *self._id_arguments(),
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
            identity=self._identity_in_words(),
        )
        if build.returncode != 0:
            raise Refusal("commit: the build failed\n" + report.output)
        self.built = True
        self._record_terms(document)
        self.serve()
        if self.listening is not None:
            report.viewer = self.listening.viewer
            report.session = self.listening.session
            report.control = self.listening.control
        return report

    def _refuse_an_empty_build(self, document: dict) -> None:
        """A first commit with no points staged needs an explicit extent on every view (§6.1)."""
        for entry in D.view_entries(document, self.default_source):
            staged = self.sources.get(entry["source"])
            if staged is not None and staged.rows:
                continue
            extent = entry["extent"]
            fitted = extent == "auto" or (isinstance(extent, dict) and extent.get("auto"))
            if fitted:
                named = (
                    f"view {entry['id']!r}"
                    if entry["id"]
                    else f"view group {entry['group']!r}"
                )
                raise Refusal(
                    f"commit: {named} has no staged rows to fit a frame around. "
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
            # The closing flush is a request of the plan and is printed as one: it is where the
            # commit blocks, and a plan that did not name it would understate what `commit()` does.
            plan=[page.line for page in pages]
            + (["flush, and wait for the publication it arms"] if pages else []),
            findings=findings,
        )
        if not sent or not report.ok:
            return report
        C.run(control, pages, report)
        self._record_terms(self._document())
        self.deltas.clear()
        self._save_state()
        return report

    @property
    def control(self) -> Control:
        """The operator plane of this database's own server."""
        listening = self.serve()
        credential = (self.path / ".tessera" / "operator.cred").read_text(encoding="utf-8")
        return Control(f"http://{listening.control}", credential.strip())

    def _payloads(self) -> dict:
        """`tessera check --payloads` over this declaration: one body per runtime declaration.

        The emitter writes one object with a key per block kind: `layers` and `attributes` as
        bare bodies, and `views`, `view_groups` and `vocabularies` as `{name, body}`, each
        addressed by a path segment. The paged commit sends every kind: the view groups and their
        roster views, the plain views, the vocabularies with the pages of their values, the
        attributes and the layers.

        The declaration minus its acquisition keys *is* the payload (configuration.md §2), so this
        is the binary serialising what it parsed rather than a second emitter in Python.
        """
        self.write()
        result = self._run(
            ["check", "--deployment", str(self.path / "tessera.toml"), "--payloads"]
        )
        if result.returncode != 0:
            raise Refusal(
                "commit: the declaration did not check, so no declaration payload was emitted\n"
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
        chosen = list(terms) if terms is not None else list(self.terms)
        return authorise(self.session_url, self.session_credential, chosen)

    def viewer(self, terms: Sequence[str] | None = None) -> Viewer:
        """A `Viewer` on this database as the principal whose visibility is `terms` (§8).

        The map of any principal is one call: `db.viewer(["public"]).map()` is what a viewer
        holding that one term sees, computed inside their mask and not filtered down from the
        operator's. With no terms it is the union the SDK recorded, which is this database's own
        principal.

        A term the union does not hold is refused and named. The SDK knows every label it staged,
        so a typo would otherwise mint a principal who sees nothing and draw an empty map with no
        error anywhere.

        An empty term list is refused too. A principal holding no term sees nothing, which is the
        blank map this refusal exists to prevent, and `viewer()` with no argument is how the
        database's own principal is asked for.

        The credential stays here: what the viewer holds is a source that calls `token()`, and
        what the source hands out is the minted token.
        """
        self._refuse_before_the_first_commit("viewer")
        if terms is not None:
            if not list(terms):
                raise Refusal(
                    "viewer: a principal holding no term sees nothing, and a map of nothing is "
                    "what this refuses. viewer() with no terms is this database's own principal"
                )
            unknown = [term for term in terms if term not in self.terms]
            if unknown:
                raise Refusal(
                    f"viewer: this database has staged no access label named "
                    f"{', '.join(repr(term) for term in unknown)}. It has staged "
                    f"{', '.join(repr(term) for term in self.terms) or 'none'}"
                )
            chosen = list(terms)
        else:
            chosen = list(self.terms)
        self.serve()
        return Viewer(self.viewer_url, lambda: self.token(chosen), terms=chosen)

    def _all_terms(self) -> Viewer:
        """The viewer this database reads itself through: every term the SDK has staged.

        Held for the life of the database so one token serves many reads, and dropped whenever a
        commit records a term it did not have — a held token grants what it was minted with, and
        a stale one would read the new rows as a principal who cannot see them.
        """
        if self._viewer is None:
            self._viewer = self.viewer()
        return self._viewer

    def map(
        self,
        view: str | None = None,
        layers: Sequence[str] | None = None,
        colour_by: str | None = None,
        filters: dict | None = None,
        height: int = 480,
        **kwargs,
    ):
        """The explorer in this cell, over this database as its own principal (§8).

        `db.viewer(terms).map(...)` is the same widget as any other principal. The token is minted
        here and handed to the page as a custom message; the session credential never leaves the
        kernel and no traitlet carries either.
        """
        self._refuse_before_the_first_commit("map")
        return self._all_terms().map(
            view=view,
            layers=layers,
            colour_by=colour_by,
            filters=filters,
            height=height,
            **kwargs,
        )

    def meta(self) -> dict:
        """`/v1/meta` as this database's own principal reads it: the frames and the schema."""
        self._refuse_before_the_first_commit("meta")
        return self._all_terms().meta()

    def item(self, tessera_id, idset: int | None = None) -> dict:
        """The drill-down record for one item, as this database's own principal (§8).

        `external_id` comes back as the staged id column's own type: an integer column's eight
        little-endian bytes as an integer, a string column's as text, anything else as the bytes
        themselves. The wire says bytes and the SDK knows which column those bytes came from, so
        the id a cell prints here is the id the user staged and can look up in their own frame.
        """
        self._refuse_before_the_first_commit("item")
        record = self._all_terms().item(tessera_id, idset)
        if record.get("external_id") is not None:
            record["external_id"] = self._staged_id(record["external_id"])
        return record

    def _staged_id(self, raw: bytes):
        """External-id bytes read as the type the id column staged (`_control.external_id`)."""
        staged = self.sources.get(self._identity_source())
        dtype = None if staged is None else staged.id_type
        if is_integer_type(dtype):
            # `_control.external_id` writes eight little-endian bytes, signed where the value was.
            return int.from_bytes(raw, "little", signed=str(dtype).startswith("int"))
        if dtype is not None and (pa.types.is_string(dtype) or pa.types.is_large_string(dtype)):
            return raw.decode()
        return raw

    def viewport(
        self,
        bbox: Sequence[float] | None = None,
        view: str | None = None,
        filters: dict | None = None,
        k: int | None = None,
        zoom: int = 0,
    ):
        """The points served for a box, as a pyarrow table (§8). `Viewer.viewport` is the verb."""
        self._refuse_before_the_first_commit("viewport")
        return self._all_terms().viewport(bbox, view, filters, k, zoom)

    def _id_arguments(self) -> list[str]:
        """`--mint-external-ids`, where the identity column is an integer (configuration.md §8).

        A supplied key is an external id and the build writes it without a flag. An integer id
        column is a source-corpus number rather than a namespace the caller owns, so writing the
        sidecar from it is opt-in, and the SDK asks for it, because every route the later commits
        use addresses a row by the bytes of the column the user staged. A database whose points
        name no identity takes neither the flag nor the sidecar: its rows are `tessera_id` rows.
        """
        source = self._identity_source()
        column = self._id_column_of(source)
        staged = self.sources.get(source)
        if column is None or staged is None:
            return []
        return ["--mint-external-ids"] if is_integer_type(staged.columns.get(column)) else []

    def _identity_source(self) -> str | None:
        """The points source this declaration reads identity from: its first view's (§3)."""
        for entry in self.view_entries():
            if entry["source"] is not None:
                return entry["source"]
        return self.default_source

    def _identity_in_words(self) -> str:
        """How this database names a row, for the commit report."""
        source = self._identity_source()
        column = self._id_column_of(source)
        if column is None:
            return (
                "the points name no id column, so every row is named by its tessera_id and the "
                "bundle writes no external id"
            )
        staged = self.sources.get(source)
        kind = "an integer" if is_integer_type(staged.columns.get(column)) else "bytes"
        return f"rows are named by '{column}' on '{source}', read as {kind} (configuration.md §8)"

    def _record_terms(self, document: dict) -> None:
        """Every access label this commit staged, kept for `map()` (§8)."""
        for term in self._staged_terms(document):
            if term not in self.terms:
                self.terms.append(term)
                # A held token grants the terms it was minted with, so a new label needs a new one.
                self._viewer = None
        self._save_state()

    def _staged_terms(self, document: dict) -> list[str]:
        """Every access label this commit staged, plus each view's default label (§8)."""
        terms: list[str] = []
        for entry in D.view_entries(document, self.default_source):
            visibility = entry["point_visibility"]
            default = visibility.get("default")
            if default:
                terms.append(default)
            field = visibility.get("field")
            source = entry["source"]
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
        """Delete rows by the ids their id column holds, or by their `tessera_id` (§6.5).

        A deletion leaves the overlay at the compaction that removes its rows and at no other point
        (write-path §5.4). A removed id staged again goes as a point row, which decision 0047
        allows: an edit is a delete and a re-ingest.
        """
        return self._changes(ids, "delete")

    def suppress(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Hide rows by their ids. A suppression is lifted by `unsuppress` alone (§6.5)."""
        return self._changes(ids, "suppress")

    def unsuppress(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Lift a suppression (§6.5)."""
        return self._changes(ids, "unsuppress")

    def addresses(self, ids: Iterable[Hashable]) -> list[dict]:
        """How `/control/changes` names the rows these ids name (§3, contracts §3.4).

        A database whose points declare an id column is addressed by the bytes that column holds;
        one that declares none has no external id anywhere and is addressed by the `tessera_id`
        the ingest route and a pick hand back, which carries the idset it was minted under.
        """
        if self._id_column_of(self._identity_source()) is not None:
            return [{"external_id": addressed(one)} for one in ids]
        idset = int(self.meta()["idset"])
        return [{"tessera_id": str(one), "idset": idset} for one in ids]

    def _changes(self, ids: Iterable[Hashable], op: str) -> ChangeReport:
        self._refuse_before_the_first_commit(op)
        addresses = self.addresses(ids)
        report = ChangeReport(op=op, requested=len(addresses))
        control = self.control
        for answer in C.changes(control, addresses, op, control.limits()):
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
        wanted = list(ids)
        report = ChangeReport(op=f"leave {layer}/{key} rank {rank}", requested=len(wanted))
        answer = C.leave(self.control, layer, key, wanted, rank, level)
        if not answer.ok:
            report.refusals.append({"status": answer.status, "detail": answer.detail[:1000]})
        return report

    def _refuse_before_the_first_commit(self, verb: str) -> None:
        if not self.built:
            raise Refusal(
                f"{verb}: this database has not been committed, so there is nothing serving it "
                f"and no rows to address. commit() builds it first"
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
        """Stop the child and, for a temporary database, remove the directory.

        Nothing is invalidated server-side: a token this database minted is good until its
        lifetime runs out (`[disclosure] token_max_lifetime`, one hour), and there is no route
        that withdraws one. What `close()` stops is the process that would answer it.
        """
        if self._child is not None:
            _instance.stop(self._child)
            self._child = None
            self.listening = None
            self._viewer = None
        if self.temporary and self.path.exists():
            shutil.rmtree(self.path, ignore_errors=True)

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *exception) -> None:
        self.close()


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
    """A saved database: the directory, its sources and its declaration (§2).

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
        "id_column": staged.id_column,
        "rows": staged.rows,
        "columns": {c: str(t) for c, t in staged.columns.items()},
        "notes": staged.notes,
    }


def _restored(name: str, source: dict) -> StagedSource:
    return StagedSource(
        name=name,
        path=Path(source["path"]),
        declared_path=source["declared_path"],
        default=source["default"],
        in_place=source["in_place"],
        id_column=source["id_column"],
        rows=source["rows"],
        columns={c: _arrow_type(t) for c, t in source["columns"].items()},
        notes=list(source["notes"]),
    )


def _load(database: Database, state: dict) -> None:
    for name, source in state.get("sources", {}).items():
        database.sources[name] = _restored(name, source)
    for name, source in state.get("deltas", {}).items():
        database.deltas[name] = _restored(name, source)
    database.terms = list(state.get("terms", []))
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
