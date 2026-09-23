"""A Tessera database in a directory: create it, fill it, commit it.

The directory is everything `tessera build` and `tessera serve` read, so a notebook prototype
becomes a deployment by copying it: `tessera serve --deployment <dir>/tessera.toml` serves the
same database from wherever it was copied to.

Three verbs carry the model, and each does one thing. `declare_*` says what exists and takes
no data. `insert(target, table, **columns)` hands a table to a declared thing and names the
columns it needs. `commit()` sends what has been inserted since the last commit and forgets it:
the first time through the build, after that through the control plane. `check()` is `commit()`
with nothing sent.

Beside the declaration the SDK writes, it keeps its own copy of the blocks as JSON under
`.tessera/`, written at every declaration and every insert, so `open()` reads them back without
parsing TOML and a database saved before its first commit reopens where it was left.

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

from . import _columns
from . import _commit as C
from . import _declaration as D
from . import _inserts, _instance
from ._auth import authorise, revoke
from ._control import Control, addressed
from ._inserts import Insert, is_integer_type
from ._refusal import Refusal
from ._reports import (
    ChangeReport,
    CommitReport,
    Declared,
    PagedReport,
    Report,
    render_columns_of,
)
from ._toml import Inline, dumps
from ._viewer import Viewer

#: The declaration check reads the declaration in this process where the extension module is
#: installed, and through `tessera check` where it is not. The two read one declaration with one
#: parser; what the extension adds is a refusal that names the block it is about. The wheel
#: carries the object inside `tesseradb_native`, so a bare `import _tessera` finds it only once
#: that package has been imported: `find_extension` is what knows the three places it can be.
_tessera = _instance.find_extension()

#: A temporary database goes here when the platform has a RAM-backed filesystem.
RAM_BACKED = Path("/dev/shm")

#: The role each kind of target's plain `insert(target, table, …)` is.
PLAIN_ROLE = {
    "view": "rows",
    "view_group": "rows",
    "attribute": "values",
    "layer": "key",
    "labels": "text",
    "vocabulary": "values",
}


def _accepted(answer, what: str) -> dict:
    """The body of a control-plane answer, or a refusal naming the status and what it said."""
    if not answer.ok:
        raise Refusal(f"{what}: refused ({answer.status}): {answer.detail[:1000]}")
    return answer.body


class Database:
    """One database directory, and the declaration the SDK is building for it."""

    def __init__(self, path: Path, temporary: bool = False) -> None:
        self.path = Path(path)
        self.temporary = temporary
        if temporary:
            _instance.temporary.add(self.path)
        #: The tables inserted before the first commit: what the build reads.
        self.inserts: list[Insert] = []
        #: The tables inserted since the last commit, which the next one sends and forgets.
        self.pending: list[Insert] = []
        self.blocks = D.Declaration()
        #: Every access label this database has inserted, plus each view's default. Computed
        #: at each commit from the inserted access columns and kept in the SDK's JSON declaration
        #: copy, which is the one thing `map()` needs and no route answers.
        self.terms: list[str] = []
        self.built = (self.path / "bundle" / "CURRENT").exists()
        self._child: subprocess.Popen | None = None
        self.listening: _instance.Listening | None = None
        self._loaded_text: str | None = None

    # ------------------------------------------------------------------ declarations

    def declare(self, kind: str, block: dict) -> dict:
        """One block of the declaration, spelled with the declaration's own keys.

        Every block is expressible this way; the typed verbs below build the dict and call here,
        and this is the way to write a key no typed verb has a parameter for.
        """
        if kind == "attribute":
            self._refuse_a_render_column(block.get("name"), block.get("render"))
            self._mark_a_filled_column(block)
        return self._declared(self.blocks.add(kind, block))

    def declare_view(self, name: str, **kwargs) -> dict:
        """One `[[view]]` block, at any commit.

        A view declared after the first commit is sent as `PUT /control/views/{name}` at the next
        commit, with the body `tessera check --payloads` emits over this declaration. A frame is
        fixed for the life of a view and the route has no rows to fit one against, so such a view
        declares `extent=`; an `auto` frame reaches the route as written and is refused there.
        """
        return self._declared(self.blocks.add("view", D.view_block(name, **kwargs)))

    def declare_view_group(self, name: str, **kwargs) -> dict:
        """One `[[view_group]]` block, at any commit.

        A group declared after the first commit is sent as `PUT /control/view_groups/{name}`, and
        each row of its roster as `PUT /control/views/{group}/{key}`, the group first, since a
        create resolves its group.
        """
        return self._declared(self.blocks.add("view_group", D.view_group_block(name, **kwargs)))

    def declare_vocabulary(self, name: str, **kwargs) -> dict:
        """One `[[vocabulary]]` block, at any commit.

        A vocabulary declared after the first commit is sent as
        `PUT /control/vocabularies/{name}` at the next commit, with the body this declaration
        serialises to. A closed set may be declared empty, so its values follow it as
        `PATCH /control/vocabularies/{name}/values`.
        """
        return self._declared(self.blocks.add("vocabulary", D.vocabulary_block(name, **kwargs)))

    def declare_attribute(self, name: str, type: str, **kwargs) -> dict:
        """One `[[attribute]]` block.

        An attribute declared after the first commit is sent as `PUT /control/attributes` at the
        next commit; the column reads absent on every entity that predates it, and an insert on
        the attribute fills it through `POST /control/values`. `render=True` is the one such
        attribute the route refuses, and the refusal is here.
        """
        block = D.attribute_block(name, type, **kwargs)
        self._refuse_a_render_column(name, block.get("render"))
        self._refuse_an_undeclared_group("attribute", name, block)
        self._mark_a_filled_column(block)
        return self._declared(self.blocks.add("attribute", block))

    def declare_columns(
        self,
        frame: Any,
        skip: Sequence[str] = (),
        render: Sequence[str] = (),
        index: Sequence[str] = (),
        keyword: Sequence[str] = (),
        category: Sequence[str] = (),
    ) -> Declared:
        """Declare every column of a frame from its dtype, as details only.

        Every column not in `skip` and not already declared is declared from its dtype, stored in
        the record blob and shown at drill-down. `render` and `index` apply their flags to the
        columns named; `keyword` and `category` choose those families for string columns, which
        are `text` otherwise. The id and coordinate columns are columns like any other, so `skip`
        names them.

        Nothing is inferred: the helper reads the frame's schema and never its values, and it
        never chooses `render`, which is fixed at the first commit.
        """
        schema = _inserts.schema_of(frame)
        attributes, vocabularies, rows = _columns.columns_of(
            schema,
            set(skip),
            set(render),
            set(index),
            set(keyword),
            set(category),
            self.blocks.attribute_names(),
        )
        held = self.blocks.vocabulary_names()
        for block in vocabularies:
            if block["name"] not in held:
                self.declare("vocabulary", block)
        for block in attributes:
            self.declare("attribute", block)
        report = Declared(
            columns=rows, vocabularies=[block["name"] for block in vocabularies]
        )
        print(report)
        return report

    def declare_layer(self, name: str, kind: str, **kwargs) -> dict:
        """One `[[layer]]` block, at any commit.

        A layer declared after the first commit is sent as `PUT /control/layers` at the next
        commit, with the body `tessera check --payloads` emits over this declaration.
        """
        block = D.layer_block(name, kind, **kwargs)
        self._refuse_an_undeclared_group("layer", name, block)
        return self._declared(self.blocks.add("layer", block))

    def declare_labels(self, name: str, of: str, **kwargs) -> dict:
        """A label set over a clustering: the `[layer.labels]` block on the layer `of`.

        It expands to a flat layer of supplied content, so it is declarable at any commit on
        `declare_layer`'s terms. Its text comes from `insert(name, {key: text})` or
        `insert(name, table, key=, text=)`.
        """
        parent = self.blocks.layer(of)
        if "labels" in parent:
            raise Refusal(
                f"layer {of!r} already carries a label set. A second one is a `[[layer]]` of its "
                f"own; write it through declare_layer"
            )
        parent["labels"] = D.labels_block(name, **kwargs)
        self._declared(parent["labels"])
        return parent["labels"]

    def _declared(self, block: dict) -> dict:
        self._save_state()
        return block

    def _mark_a_filled_column(self, block: dict) -> None:
        """An attribute declared at a running service is filled, not read.

        The mark is kept beside the block and never written: what it decides is that the written
        declaration names no source for this column, since the file the first commit built from has
        never carried it. `tessera check` takes such a block as a note and emits its payload, which
        is what the next commit declares.
        """
        if self.built and not block.get("scope"):
            block[D.FILLED] = True

    def _refuse_a_render_column(self, name: Any, render: Any) -> None:
        """A render column belongs to the first commit.

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
            f"declares a column against entities that already exist, so it refuses one. Declare "
            f"it with index=True, which is filterable and drawn at drill-down, or rebuild the "
            f"database with create(path, replace=True)"
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

    # ------------------------------------------------------------------ inserting

    def insert(
        self,
        target: str,
        table: Any = None,
        roster: Any = None,
        artifacts: Any = None,
        members: Any = None,
        columns: dict | None = None,
        **named: str,
    ) -> Insert:
        """Hand a table to a declared thing, naming every column it reads.

        `table` is a pandas or polars frame, a pyarrow table, or a path to a parquet file, which
        is read in place. Every column the target needs is named on the call; a column the call
        does not name is ignored, and the two lists are printed. A layer takes two tables under
        their own keywords, `artifacts=` and `members=`, since both carry `key` and `level`.

        A table in Tessera's own shape is no exception: a canonical column the call did not name
        is refused naming the column and the two remedies, so nothing is read silently at one
        door and ignored at the other. A group's rows name the column saying which view each
        belongs to with `view=`, or the one view the whole table is for with `view_key=`.

        Before the first commit the table is bound to its target for the build. After it, the same
        call is sent at the next `commit()` by the route its target owns. Several inserts on one
        target accumulate.
        """
        kind, block = self._target(target, named, roster, artifacts, members)
        role, data = self._role(target, kind, table, roster, artifacts, members, named)
        if _inserts.is_path(data) and not Path(data).expanduser().exists():
            raise Refusal(f"insert into {target!r}: {Path(data).expanduser()} does not exist")
        if (kind, role) not in _inserts.CONTRACTS:
            raise Refusal(
                f"insert into {kind} {target!r}: a {kind} takes no {role} table"
            )
        self._refuse_an_insert_the_target_cannot_take(target, kind, role, block, named)
        if kind == "layer" and role == "artifacts" and named.get("access"):
            D.carry_labels(target, block, named["access"])
        projected = kind in ("view", "view_group") and block.get("projection", "none") != "none"
        metadata = D.metadata_names(block) if role == "roster" else ()
        if kind == "labels" and role == "text":
            data, named = self._label_text(target, block, data, named)
        matched = self._attribute_columns(target, kind, role, data, columns, named)
        insert = _inserts.build(
            target=target,
            kind=kind,
            role=role,
            data=data,
            named=named,
            directory=self.path,
            source=self._source_key(target, kind, role, named.get("view_key")),
            at_build=not self.built,
            projected=projected,
            metadata=metadata,
            named_attributes=matched,
        )
        self._refuse_a_second_view_without_its_labels(kind, role, target, insert)
        insert = self._accumulated(insert)
        (self.pending if self.built else self.inserts).append(insert)
        self._save_state()
        print(insert)
        return insert

    def _target(
        self, target: str, named: dict, roster=None, artifacts=None, members=None
    ) -> tuple[str, dict]:
        """The declared thing this name is, or a refusal naming the verb that declares one.

        One name may be held by two kinds: a category column and the vocabulary it reads are
        each declared under the value set's own name, which the declaration allows, and the
        columns the call names are what say which is meant: an attribute reads `id=` and
        `value=`, a value set reads `key=`, `title=` and `code=`.
        """
        found: list[tuple[str, dict]] = []
        for kind in D.KINDS:
            for block in self.blocks.blocks[kind]:
                if block.get("name") == target:
                    found.append((kind, block))
        for block in self.blocks.blocks["layer"]:
            labels = block.get("labels")
            if isinstance(labels, dict) and labels.get("name") == target:
                found.append(("labels", labels))
        if not found:
            raise Refusal(
                f"insert into {target!r}: nothing of that name is declared. A table is handed to "
                f"a declared thing, so declare_view, declare_view_group, declare_attribute, "
                f"declare_layer, declare_labels or declare_vocabulary comes first"
            )
        if len(found) == 1:
            return found[0]
        table_word = {"roster": roster, "artifacts": artifacts, "members": members}
        fits = [
            (kind, block)
            for kind, block in found
            if _fits(kind, named, next((w for w, v in table_word.items() if v is not None), None))
        ]
        if len(fits) == 1:
            return fits[0]
        kinds = ", ".join(sorted(kind for kind, _ in found))
        raise Refusal(
            f"insert into {target!r}: {kinds} are declared under that name, and the columns this "
            f"call names fit {'both' if not fits else 'neither'}. An attribute reads id= and "
            f"value=; a vocabulary reads key=, title= and code=; a layer takes artifacts= or "
            f"members="
        )

    def _role(
        self, target: str, kind: str, table: Any, roster: Any, artifacts: Any, members: Any,
        named: dict,
    ) -> tuple[str, Any]:
        """Which of the target's tables this call is, and the data it carries."""
        given = [word for word, value in
                 (("roster", roster), ("artifacts", artifacts), ("members", members))
                 if value is not None]
        if artifacts is not None and isinstance(members, str):
            # On an artifacts insert `members=` names the column the memberships are in, which is
            # the artifact table's own key.
            named["members"] = members
            given.remove("members")
            members = None
        if table is not None and given:
            raise Refusal(
                f"insert into {target!r}: a table and {given[0]}= are two tables, and each insert "
                f"hands over one. Make them two calls"
            )
        if len(given) > 1:
            raise Refusal(
                f"insert into {target!r}: {' and '.join(given)} are two tables, each with its own "
                f"column names. Make them two calls"
            )
        if given:
            return given[0], {"roster": roster, "artifacts": artifacts, "members": members}[
                given[0]
            ]
        if table is None:
            raise Refusal(f"insert into {target!r}: no table was given")
        return PLAIN_ROLE[kind], table

    def _refuse_an_insert_the_target_cannot_take(
        self, target: str, kind: str, role: str, block: dict, named: dict
    ) -> None:
        """What this target cannot be given: the scope's own column, and the rule not built yet."""
        scope = block.get("scope")
        group = scope.get("group") if isinstance(scope, dict) else None
        if group is not None and role in ("artifacts", "members", "key") and "view" not in named:
            raise Refusal(
                f"insert into {kind} {target!r}: this layer is scoped to group {group!r} and keys "
                f"its artifacts per view, one key in two views being two artifacts, so every row "
                f"carries the view it belongs to. Name the column that says which with view="
            )

    def _refuse_a_second_view_without_its_labels(
        self, kind: str, role: str, target: str, insert: Insert
    ) -> None:
        """A second view over the same entities carries its own access column.

        The build refuses an entity whose labels disagree between views and the ingest route
        refuses a join row whose labels differ from the held ones, so a frame that lacks the
        column is refused here. The SDK copies nothing.
        """
        if kind not in ("view", "view_group") or role != "rows" or insert.columns.get("access"):
            return
        for other in self.inserts + self.pending:
            if other.role == "rows" and other.target != target and other.columns.get("access"):
                raise Refusal(
                    f"insert into {kind} {target!r}: view {other.target!r} reads each point's "
                    f"labels from column '{other.columns['access']}', and every view over one "
                    f"entity carries that entity's labels: a row whose labels disagree between "
                    f"views is refused at the build and on the ingest route. Name this frame's "
                    f"own label column with access="
                )

    def _attribute_columns(
        self, target: str, kind: str, role: str, data: Any, columns: dict | None, named: dict
    ) -> dict:
        """Which attributes a frame inserted into the allocation view fills, by name.

        The one place a name match is what the user meant, as SQL's `INSERT BY NAME` is: the
        attribute was declared, and a column of its name in the frame inserted into the
        allocation view fills it. On any other view's insert attribute-named columns are ignored,
        so a frame inserted for its coordinates alone carries nothing it was not meant to.
        """
        if columns and not (kind == "view" and role == "rows"):
            raise Refusal(
                f"insert into {kind} {target!r}: columns= names an attribute's value column on "
                f"the allocation view's own insert, which this is not"
            )
        if kind != "view" or role != "rows":
            return {}
        if target != self.blocks.allocation_view():
            if columns:
                raise Refusal(
                    f"insert into view {target!r}: an attribute is filled from the allocation "
                    f"view's frame, which is {self.blocks.allocation_view()!r}. Insert the values "
                    f"into the attribute itself: insert(<attribute>, table, id=…, value=…)"
                )
            return {}
        schema = _inserts.schema_of(data)
        matched = {}
        for block in self.blocks.blocks["attribute"]:
            if block.get("scope") or block.get(D.FILLED):
                continue
            name = block["name"]
            column = (columns or {}).get(name, name)
            # The id column is this frame's identity rather than one of its values; a column
            # that is also the geometry or the labels is still a declared attribute's column
            # where the user declared one of that name.
            if column in schema and column != named.get("id"):
                matched[name] = column
        for name, column in (columns or {}).items():
            if column not in schema:
                raise Refusal(
                    f"insert into view {target!r}: columns={{{name!r}: {column!r}}} names no "
                    f"column of this table. Its columns are {', '.join(schema)}"
                )
            if name not in self.blocks.attribute_names():
                raise Refusal(
                    f"insert into view {target!r}: columns= names attribute {name!r}, which this "
                    f"declaration does not carry. declare_attribute({name!r}, …) first"
                )
        return matched

    def _source_key(self, target: str, kind: str, role: str, view_key: str | None = None) -> str:
        """The `[sources]` key one insert writes under: the target's name, and its role.

        A group whose views each have their own file inserts one table per view, so the view's
        own key names its file: the roster the SDK writes is one record per source.
        """
        stem = target.replace("/", "_").replace(":", "_")
        if view_key is not None:
            stem = f"{stem}_{view_key}".replace("-", "_").replace("/", "_")
        key = stem if role in ("rows", "values", "text") else f"{stem}_{role}"
        # Never a key another insert holds: a second part of one target is written beside the
        # first and the two are read and written as one below.
        taken = {insert.source for insert in self.inserts + self.pending}
        # One name may be held by two kinds, a category column and its value set, and each has
        # its own file: the second is named for its kind rather than numbered.
        candidate, at = key, 1
        if candidate in taken:
            candidate = f"{key}_{kind}"
        while candidate in taken:
            at += 1
            candidate = f"{key}_{at}"
        return candidate

    def _accumulated(self, insert: Insert) -> Insert:
        """Several inserts on one target before a commit accumulate.

        A block names one file, so where a target's role is inserted twice before the first commit
        the tables are read and written as one under `sources/`. The columns each call named must
        be the same, the declaration naming them once, and so must the types: a second part whose
        schema differs from the first is refused naming the two, rather than promoted to a type
        neither part was written in. The first part is copied where it was a path read in place,
        there being one file for the block to name.
        """
        if self.built:
            return insert
        held = next(
            (
                one
                for one in self.inserts
                if one.target == insert.target
                and one.kind == insert.kind
                and one.role == insert.role
                # A group whose views each have their own file inserts one table per view, and
                # the parts of one view are the parts that name it.
                and one.view_key == insert.view_key
            ),
            None,
        )
        if held is None:
            return insert
        if held.columns != insert.columns or held.named_attributes != insert.named_attributes:
            raise Refusal(
                f"insert into {insert.kind} {insert.target!r}: this table names its columns "
                f"differently from the one already inserted ({held.columns} against "
                f"{insert.columns}). A block names its columns once, so a corpus in parts names "
                f"them the same way in every part"
            )
        differs = [
            (name, held.schema[name], insert.schema[name])
            for name in held.schema
            if name in insert.schema and str(held.schema[name]) != str(insert.schema[name])
        ]
        if differs or set(held.schema) != set(insert.schema):
            name, one, other = differs[0] if differs else (
                sorted(set(held.schema) ^ set(insert.schema))[0], "present", "absent"
            )
            raise Refusal(
                f"insert into {insert.kind} {insert.target!r}: this part's schema differs from "
                f"the one already inserted, at column {name!r}: {one} against {other}. A block "
                f"reads one file, and the parts are written as one, so a promoted column would "
                f"be a type neither part was written in. Write the parts in one schema"
            )
        table = pa.concat_tables([held.table(), insert.table()])
        path = self.path / "sources" / f"{held.source}.parquet"
        path.parent.mkdir(parents=True, exist_ok=True)
        pq.write_table(table, path)
        if not insert.in_place and insert.path != path:
            insert.path.unlink(missing_ok=True)
        self.inserts.remove(held)
        insert.path = path
        insert.declared_path = f"sources/{held.source}.parquet"
        insert.source = held.source
        insert.in_place = False
        insert.rows = table.num_rows
        insert.schema = dict(zip(table.schema.names, table.schema.types))
        return insert

    def _label_text(
        self, target: str, block: dict, data: Any, named: dict
    ) -> tuple[Any, dict]:
        """A label set's text as the artifacts table its layer reads.

        Two paths, and which one it is decides where the attachment comes from. **Where the SDK
        writes the table**, for a mapping from cluster key to text or a table naming `text=`, a
        plain string column no route reads as a ranked content, it writes the attachment too:
        `attached_layer` is the layer the label set was declared `of`, and `attached_key` is the
        label's own key, which is the cluster's, unless the call named a column for either. Every
        other column the call named travels as it stands, `level=` included. **Where the table is
        read as it stands**, under `contents=`, the attachment is in the table and named on the call,
        and a table carrying none is refused: a label set expands to a layer that depends on its
        clustering, and every artifact it publishes attaches to one.
        """
        of = next(
            one["name"]
            for one in self.blocks.blocks["layer"]
            if isinstance(one.get("labels"), dict) and one["labels"]["name"] == target
        )
        if isinstance(data, dict):
            if named:
                raise Refusal(
                    f"insert into labels {target!r}: a mapping from key to text carries no "
                    f"columns, so {', '.join(f'{one}=' for one in sorted(named))} names nothing. "
                    f"Insert a table to name columns on it"
                )
            keys = [str(key) for key in data]
            texts = [[[value]] if isinstance(value, str) else [list(value)]
                     for value in data.values()]
            levels = [0] * len(keys)
            attached_keys = list(keys)
            attached_layers = [of] * len(keys)
        elif "text" in named:
            table = _inserts.as_table(data) if not _inserts.is_path(data) else pq.read_table(data)
            missing = [
                name for name, column in named.items() if column not in table.column_names
            ]
            if missing:
                raise Refusal(
                    f"insert into labels {target!r}: "
                    + ", ".join(f"{name}={named[name]!r}" for name in missing)
                    + f" names no column of this table. Its columns are "
                    f"{', '.join(table.column_names)}"
                )
            keys = [str(key) for key in table[named["key"]].to_pylist()]
            texts = [[[value]] for value in table[named["text"]].to_pylist()]
            levels = (
                [int(one or 0) for one in table[named["level"]].to_pylist()]
                if "level" in named
                else [0] * len(keys)
            )
            attached_keys = (
                [str(one) for one in table[named["attached_key"]].to_pylist()]
                if "attached_key" in named
                else list(keys)
            )
            attached_layers = (
                [str(one) for one in table[named["attached_layer"]].to_pylist()]
                if "attached_layer" in named
                else [of] * len(keys)
            )
            carried = {"key", "text", "level", "attached_key", "attached_layer"}
            beyond = sorted(set(named) - carried)
            if beyond:
                raise Refusal(
                    f"insert into labels {target!r}: "
                    + ", ".join(f"{one}=" for one in beyond)
                    + " names a column the table the SDK writes from text= does not carry. That "
                    "table is the key, the text as one ranked content, the level and the "
                    "attachment. Write the table in the publication's own shape and insert it "
                    "with contents=, which is read as it stands"
                )
        else:
            if "attached_key" not in named:
                raise Refusal(
                    f"insert into labels {target!r}: a label set expands to a layer that depends "
                    f"on its clustering, so every artifact it publishes attaches to one, and a "
                    f"table read as it stands carries the attachment. Name it with "
                    f"attached_key= (and attached_layer= where the table says which layer), or "
                    f"insert the text as a mapping or with text=, which attaches each label to "
                    f"the cluster its key names"
                )
            return data, named
        written = pa.table(
            {
                "level": pa.array(levels, type=pa.uint32()),
                "key": pa.array(keys, type=pa.string()),
                "contents": pa.array(texts, type=pa.list_(pa.list_(pa.string()))),
                "attached_layer": pa.array(attached_layers, type=pa.string()),
                "attached_key": pa.array(attached_keys, type=pa.string()),
            }
        )
        return written, {
            "key": "key",
            "contents": "contents",
            "level": "level",
            "attached_layer": "attached_layer",
            "attached_key": "attached_key",
        }

    # ------------------------------------------------------------------ the document

    @property
    def declaration(self) -> str:
        """The declaration as TOML: what the SDK writes and `tessera check` reads."""
        if self._loaded_text is not None:
            return self._loaded_text
        return dumps(self._document())

    def _document(self) -> dict:
        sources = {
            insert.source: insert.declared_path
            for insert in self.inserts
            if insert.declared_path is not None
        }
        document = self.blocks.document(sources)
        self._anchor_a_group(document)
        D.bind(document, self.inserts)
        D.bind_value_sets(document, self.inserts + self.pending)
        return document

    def _anchor_a_group(self, document: dict) -> None:
        """A declaration carrying groups alone anchors on the first view of the first roster.

        Entity ids are ordered by the item's Morton code in the anchor view and are permanent, so
        with several views the anchor is a declaration rather than a default. A
        view of a group is addressed `<group>:<key>`, and the key is the roster's first row.
        """
        defaults = document.setdefault("defaults", {})
        if defaults.get("allocation_view") or not document.get("view_group"):
            return
        for group in document["view_group"]:
            for record in group.get("view") or []:
                defaults["allocation_view"] = f"{group['name']}:{record['key']}"
                return
            for insert in self.inserts + self.pending:
                if insert.target != group["name"] or insert.role != "roster":
                    continue
                keys = insert.table()[insert.columns["key"]].to_pylist()
                if keys:
                    defaults["allocation_view"] = f"{group['name']}:{keys[0]}"
                    return
        if not defaults:
            document.pop("defaults")

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
        """The SDK's own copy of the declaration, so `open()` reads the blocks back."""
        state = {
            "inserts": [_stored(insert) for insert in self.inserts],
            "pending": [_stored(insert) for insert in self.pending],
            "terms": list(self.terms),
            "blocks": _tagged(self.blocks.blocks),
        }
        path = self.path / ".tessera" / "declaration.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(state, indent=1), encoding="utf-8")

    # ------------------------------------------------------------------ check and commit

    def check(self) -> Report | PagedReport:
        """What the next commit would do, with nothing sent.

        Before the first commit that is the declaration check over this directory: what the
        declaration reads from each file, schemas only and no rows. After it, the plan and the
        pre-flight.
        """
        if self.built:
            return self._paged(sent=False)
        document = self.write()
        findings = self._preflight(document)
        ok, page = self._checked()
        return Report(
            what="check",
            ok=ok and not findings,
            frames=self._frames(document),
            render_columns=render_columns_of(document.get("attribute", [])),
            notes=self._notes(),
            findings=findings,
            output=page,
        )

    def _checked(self) -> tuple[bool, str]:
        """The declaration check over the file this database has written: whether it passed, and
        the page it printed.

        Through the extension module where it is installed, so a refusal arrives as the findings
        the check made rather than as a page of text an exit code came with. The page is the
        binary's own either way: one renderer sits under both paths.
        """
        deployment = str(self.path / "tessera.toml")
        if _tessera is None:
            result = self._run(["check", "--deployment", deployment])
            return result.returncode == 0, result.stdout + result.stderr
        try:
            report = _tessera.check(deployment)
        except _tessera.DeclarationError as refused:
            return False, f"check FAILED: {refused}"
        return report.ok, report.page

    def commit(self) -> CommitReport | PagedReport:
        """Make what was inserted part of the database: the build the first time, pages after.

        The first commit runs the declaration check, then `tessera build`, then `tessera
        serve`. Three
        things happen there and at no later commit, and the report says each: the frame is fixed,
        the column types and render flags are fixed, and the allocation is signature-sorted over
        the whole inserted corpus. Every commit after it pages what was inserted since the
        last one through the control plane in order, flushes once and waits for the
        publication that flush arms. Then the inserts are forgotten.

        A commit that did not happen raises `Refusal` carrying its report as `report`: a finding
        that stopped it before anything was sent, a failed build, or every page refused. A commit
        some of whose pages landed returns its report.
        """
        if self.built:
            return self._paged(sent=True)
        document = self.write()
        self._refuse_an_empty_build(document)
        findings = self._preflight(document)
        if findings:
            raise Refusal(
                "commit: the pre-flight found what follows, and nothing was sent or built\n"
                + "\n".join(f"  {finding}" for finding in findings)
            )
        ok, page = self._checked()
        if not ok:
            raise Refusal("commit: the declaration did not check\n" + page)
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
            frames=self._frames(document),
            render_columns=render_columns_of(document.get("attribute", [])),
            notes=self._notes(),
            output=page + "\n" + build.stdout + build.stderr,
            identity=self._identity_in_words(),
        )
        if build.returncode != 0:
            raise Refusal("commit: the build failed\n" + report.output, report)
        self.built = True
        self._record_terms(document)
        self.serve()
        if self.listening is not None:
            report.viewer = self.listening.viewer
            report.session = self.listening.session
            report.control = self.listening.control
        return report

    def _preflight(self, document: dict) -> list[C.Finding]:
        """The findings the build's own inserts can raise, before a byte is read."""
        findings: list[C.Finding] = []
        C.rows_with_no_id(self.inserts, findings)
        C.keys_into_supplied_content(document, self.inserts, findings)
        return findings

    def _refuse_an_empty_build(self, document: dict) -> None:
        """A first commit with no rows inserted needs an explicit extent on every view."""
        inserted = {
            insert.target for insert in self.inserts if insert.role == "rows" and insert.rows
        }
        for block in document.get("view", []) + document.get("view_group", []):
            if block["name"] in inserted:
                continue
            extent = block.get("extent")
            fitted = extent == "auto" or (isinstance(extent, dict) and extent.get("auto"))
            if fitted:
                raise Refusal(
                    f"commit: view {block['name']!r} has no inserted rows to fit a frame around. "
                    f"An empty database needs extent= on every view"
                )

    # ------------------------------------------------------------------ the paged commit

    def _paged(self, sent: bool) -> PagedReport:
        """The plan over what was inserted since the last commit, run where `sent`."""
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
        if not sent:
            return report
        if report.findings:
            raise Refusal(str(report), report)
        accepted = C.run(control, pages, report)
        self._record_terms(self._document())
        self.pending.clear()
        self._save_state()
        if pages and not accepted:
            raise Refusal(str(report), report)
        return report

    @property
    def control(self) -> Control:
        """The operator plane of this database's own server."""
        listening = self.serve()
        credential = (self.path / ".tessera" / "operator.cred").read_text(encoding="utf-8")
        return Control(f"http://{listening.control}", credential.strip())

    def _payloads(self) -> dict:
        """The control-plane payloads this declaration serialises to: one body per block kind.

        The emitter writes one object with a key per block kind: `layers` and `attributes` as
        bare bodies, and `views`, `view_groups` and `vocabularies` as `{name, body}`, each
        addressed by a path segment.

        The declaration minus its acquisition keys *is* the payload, so this is the binary
        serialising what it parsed rather than a second emitter in Python.
        """
        self.write()
        deployment = str(self.path / "tessera.toml")
        refused = "commit: the declaration did not check, so no declaration payload was emitted\n"
        if _tessera is None:
            result = self._run(["check", "--deployment", deployment, "--payloads"])
            if result.returncode != 0:
                raise Refusal(refused + result.stdout + result.stderr)
            return json.loads(result.stdout)
        try:
            return json.loads(_tessera.payloads(deployment))
        except _tessera.DeclarationError as why:
            raise Refusal(f"{refused}check FAILED: {why}") from None

    def token(self, terms: Sequence[str] | None = None):
        """A viewer token for this database, minted from its own session credential.

        With no terms it mints for every access label the SDK has inserted plus each view's
        default label, which is Python asserting the local principal's authority: admissible on a
        single-operator database and nowhere else.
        """
        self.serve()
        chosen = list(terms) if terms is not None else list(self.terms)
        return authorise(self.session_url, self.session_credential, chosen)

    def viewer(self, terms: Sequence[str] | None = None) -> Viewer:
        """A `Viewer` on this database as the principal whose visibility is `terms`.

        The map of any principal is one call: `db.viewer(["public"]).map()` is what a viewer
        holding that one term sees, computed inside their mask and not filtered down from the
        operator's. With no terms it is the union the SDK recorded, which is this database's own
        principal.

        An empty term list is refused. A principal holding no term sees nothing, which is the
        blank map this refusal exists to prevent, and `viewer()` with no argument is how the
        database's own principal is asked for. Which terms a session may hold is the session
        plane's to decide, so a term this database has not inserted is minted and reads what it
        reaches, which is nothing.

        The credential stays here: what the viewer holds is a source that calls `token()`, and
        what the source hands out is the minted token.
        """
        self._refuse_before_the_first_commit("viewer")
        if terms is not None and not list(terms):
            raise Refusal(
                "viewer: a principal holding no term sees nothing, and a map of nothing is "
                "what this refuses. viewer() with no terms is this database's own principal"
            )
        chosen = list(terms) if terms is not None else list(self.terms)
        self.serve()
        return Viewer(self.viewer_url, lambda: self.token(chosen), terms=chosen)

    def map(
        self,
        view: str | None = None,
        layers: Sequence[str] | None = None,
        colour_by: str | None = None,
        filters: dict | None = None,
        height: int = 480,
        **kwargs,
    ):
        """The explorer in this cell, over this database as its own principal.

        `db.viewer(terms).map(...)` is the same widget as any other principal. The token is minted
        here and handed to the page as a custom message; the session credential never leaves the
        kernel and no traitlet carries either.
        """
        self._refuse_before_the_first_commit("map")
        return self.viewer().map(
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
        return self.viewer().meta()

    def item(self, tessera_id, idset: int | None = None) -> dict:
        """The drill-down record for one item, as this database's own principal.

        `external_id` comes back as the inserted id column's own type: an integer column's eight
        little-endian bytes as an integer, a string column's as text, anything else as the bytes
        themselves. The wire says bytes and the SDK knows which column those bytes came from, so
        the id a cell prints here is the id the user inserted and can look up in their own frame.
        """
        self._refuse_before_the_first_commit("item")
        record = self.viewer().item(tessera_id, idset)
        if record.get("external_id") is not None:
            record["external_id"] = self._inserted_id(record["external_id"])
        return record

    def _inserted_id(self, raw: bytes):
        """External-id bytes read as the type the id column carried (`_control.external_id`)."""
        insert = self._identity_insert()
        dtype = None if insert is None else insert.id_type
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
        **rest,
    ):
        """What is served for a box, as this database's own principal.

        `Viewer.viewport` is the verb and this is it under the union of every term the SDK
        inserted; `rest` is the rest of its keywords — `tiles`, `highlight`, `layers`, `levels`,
        `computed`, `artifact_budget`, `artifact_rows`, `point_rows`, `underlay_offset` and
        `pin` — passed through untouched.
        """
        self._refuse_before_the_first_commit("viewport")
        return self.viewer().viewport(bbox, view, filters, k, zoom, **rest)

    def _id_arguments(self) -> list[str]:
        """`--mint-external-ids`, where the identity column is an integer.

        A supplied key is an external id and the build writes it without a flag. An integer id
        column is a source-corpus number rather than a namespace the caller owns, so writing the
        sidecar from it is opt-in, and the SDK asks for it, because every route the later commits
        use addresses a row by the bytes of the column the user inserted. A database whose points
        name no identity takes neither the flag nor the sidecar: its rows are `tessera_id` rows.
        """
        insert = self._identity_insert()
        if insert is None or insert.id_column is None:
            return []
        return ["--mint-external-ids"] if is_integer_type(insert.id_type) else []

    def _identity_insert(self) -> Insert | None:
        """The rows insert this declaration reads identity from: the allocation view's."""
        anchor = self.blocks.allocation_view()
        rows = [one for one in self.inserts + self.pending if one.role == "rows"]
        for insert in rows:
            if insert.target == anchor:
                return insert
        return rows[0] if rows else None

    def _identity_in_words(self) -> str:
        """How this database names a row, for the commit report."""
        insert = self._identity_insert()
        if insert is None or insert.id_column is None:
            return (
                "the points name no id column, so every row is named by its tessera_id and the "
                "bundle writes no external id"
            )
        kind = "an integer" if is_integer_type(insert.id_type) else "bytes"
        return (
            f"rows are named by '{insert.id_column}' on the insert into "
            f"{insert.target!r}, read as {kind}"
        )

    def _record_terms(self, document: dict) -> None:
        """Every access label this commit inserted: what `viewer()` with no terms mints for.

        A session's terms are fixed when it is authorised, so each read verb mints its own token
        rather than holding one: a token minted before a commit reaches neither the labels nor the
        views that commit added.
        """
        for term in self._inserted_terms(document):
            if term not in self.terms:
                self.terms.append(term)
        self._save_state()

    def _inserted_terms(self, document: dict) -> list[str]:
        """Every access label inserted into a view or onto an artifact, plus each view's default
        label."""
        terms: list[str] = []
        for block in document.get("view", []) + document.get("view_group", []):
            default = dict(block.get("point_visibility") or {}).get("default")
            if default:
                terms.append(default)
        for insert in self.inserts + self.pending:
            column = insert.columns.get("access") if insert.role in ("rows", "artifacts") else None
            if column is None:
                continue
            for value in insert.table()[column].to_pylist():
                if value is None:
                    continue
                for label in value if isinstance(value, list) else [value]:
                    terms.append(str(label))
        return terms

    # ------------------------------------------------------------------ verbs that are not inserts

    def remove(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Delete rows by the ids their id column holds, or by their `tessera_id`.

        A deletion leaves the overlay at the compaction that removes its rows and at no other
        point. A removed id inserted again goes as a point row: an edit is a delete and a
        re-ingest. `compact()` asks for that compaction.
        """
        return self._changes(ids, "delete")

    def suppress(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Hide rows by their ids. A suppression is lifted by `unsuppress` alone."""
        return self._changes(ids, "suppress")

    def unsuppress(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Lift a suppression."""
        return self._changes(ids, "unsuppress")

    def addresses(self, ids: Iterable[Hashable]) -> list[dict]:
        """How `/control/changes` names the rows these ids name.

        A database whose points named an id column is addressed by the bytes that column holds;
        one that named none has no external id anywhere and is addressed by the `tessera_id`
        the ingest route and a pick hand back, which carries the idset it was minted under.
        """
        insert = self._identity_insert()
        if insert is not None and insert.id_column is not None:
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
        view: str | None = None,
    ) -> ChangeReport:
        """Shrink a content's generating set, the one set that may.

        A page that empties a set withdraws the content: the record is removed and the caller
        supplies it again rather than refilling the set. `view` names the view the artifact belongs
        to on a layer scoped to a group, where one key in two views is two artifacts.
        """
        self._refuse_before_the_first_commit("leave")
        wanted = list(ids)
        report = ChangeReport(op=f"leave {layer}/{key} rank {rank}", requested=len(wanted))
        answer = C.leave(self.control, layer, key, wanted, rank, level, view)
        if not answer.ok:
            report.refusals.append({"status": answer.status, "detail": answer.detail[:1000]})
        return report

    def status(self) -> dict:
        """`GET /control/status`: what the operator plane says about this served database.

        The counterpart of `meta()`, which is what a principal is served. This is the operator's:
        the watermarks, the queue depths and the pagination units every write route publishes.
        """
        self._refuse_before_the_first_commit("status")
        return _accepted(self.control.status_answer(), "status")

    def compact(self) -> dict:
        """`POST /control/compact`: ask for the fold that removes deleted rows.

        A deletion leaves the overlay here and nowhere else, so this is what ends one. The fold
        runs behind the answer: it is accepted, not finished, when this returns.
        """
        self._refuse_before_the_first_commit("compact")
        return _accepted(self.control.compact(), "compact")

    def drop_layer(self, name: str, wait: bool = False) -> dict:
        """`DELETE /control/layers/{name}`: the inverse of `declare_layer`.

        The name is tombstoned, not freed: a later declaration under it is refused, so no stale
        reference to the layer that was reaches the layer that is. `wait` holds until the
        publication the answer names has happened.
        """
        self._refuse_before_the_first_commit("drop_layer")
        return _accepted(self.control.drop_layer(name, wait), f"drop_layer {name}")

    def drop_view(
        self, group: str, key: str, delete_dangling: bool = False, wait: bool = False
    ) -> dict:
        """`DELETE /control/views/{group}/{key}`: the inverse of `create_view`.

        Dropping a view deletes no entity. `delete_dangling` submits the entities that hold a row
        in no other view as ordinary deletions, which retire at the next fold; the answer's
        `deleted` says how many, and a deletion is not undone.
        """
        self._refuse_before_the_first_commit("drop_view")
        return _accepted(
            self.control.drop_view(group, key, delete_dangling, wait), f"drop_view {group}/{key}"
        )

    def revoke(self, token) -> None:
        """End a session this database minted, by the `token_id` its `Token` carries.

        The capability never transits a second time: what is sent is the handle. A `token_id` as
        an integer is taken too, and one naming no live session is accepted in silence.
        """
        self.serve()
        revoke(self.session_url, self.session_credential, token)

    def _refuse_before_the_first_commit(self, verb: str) -> None:
        if not self.built:
            raise Refusal(
                f"{verb}: this database has not been committed, so there is nothing serving it "
                f"and no rows to address. commit() builds it first"
            )

    def serve(self) -> _instance.Listening:
        """Start `tessera serve` over this directory and read the addresses it bound."""
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
        """The credential this database mints its tokens with, which stays in the kernel."""
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
        """What each insert read and ignored, and what is declared and still empty."""
        notes = []
        for insert in self.inserts + self.pending:
            notes.append(
                f"{insert.kind} '{insert.target}' ({insert.role}): read "
                f"{', '.join(insert.read) or 'nothing'}; ignored "
                f"{', '.join(insert.ignored) or 'nothing'}"
            )
        filled = {insert.target for insert in self.inserts + self.pending}
        anchor = self.blocks.allocation_view()
        rows = next(
            (
                one
                for one in self.inserts + self.pending
                if one.role == "rows" and one.target == anchor
            ),
            None,
        )
        for block in self.blocks.blocks["attribute"]:
            name = block["name"]
            if name in filled or any(name in one.named_attributes for one in self.inserts):
                continue
            notes.append(f"attribute '{name}' is declared and empty: " + self._why_empty(name, rows, block))
        return notes

    def _why_empty(self, name: str, rows: Insert | None, block: dict) -> str:
        """Why no column filled this attribute, which is what the reader needs to act on."""
        if block.get(D.FILLED):
            return (
                "it was declared after the first commit, so no frame the build read carries it; "
                f"insert({name!r}, table, id=…, value=…) fills it at the next commit"
            )
        if block.get("scope"):
            return (
                "a group-scoped column belongs to one view, so it is filled by an insert of its "
                f"own: insert({name!r}, table, id=…, value=…, view=…)"
            )
        if rows is None:
            return "no frame has been inserted into the allocation view"
        if rows.columns.get("id") == name:
            return (
                f"the frame inserted into '{rows.target}' carries a column of that name and it is "
                f"the id column, which is that frame's identity rather than one of its values"
            )
        if name in rows.schema:
            return (
                f"the frame inserted into '{rows.target}' carries a column of that name, and the "
                f"attribute was declared after that insert: the match is made where the frame is "
                f"handed over. Declare it first, or insert the values with "
                f"insert({name!r}, table, id=…, value=…)"
            )
        return (
            f"the frame inserted into '{rows.target}' carries no column of that name, and no "
            f"insert names one for it"
        )

    # ------------------------------------------------------------------ the directory

    def save(self, path: str | os.PathLike) -> Path:
        """Copy this database out, so a temporary one survives `close()`."""
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
        if self.temporary:
            shutil.rmtree(self.path, ignore_errors=True)
            _instance.temporary.discard(self.path)

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *exception) -> None:
        self.close()


def _fits(kind: str, named: dict, table_word: str | None) -> bool:
    """Whether the columns this call names are the ones that kind of target reads."""
    roles = {
        role
        for (one, _), contract in _inserts.CONTRACTS.items()
        if one == kind
        for role in contract.required + contract.optional + contract.either
    }
    if table_word is not None:
        return (kind, table_word) in _inserts.CONTRACTS
    return bool(named) and set(named) <= roles


def _extent_in_words(extent: Any) -> str:
    if isinstance(extent, dict) and extent.get("auto"):
        margin = extent.get("margin", 0.01)
        return f"fitted to the inserted rows, with {margin:g} of the data span as headroom each side"
    if extent == "auto":
        return "fitted to the inserted rows, squared, with the build's own margin"
    return str(extent)


# ---------------------------------------------------------------------- create and open


def create(path: str | os.PathLike | None = None, replace: bool = False) -> Database:
    """A new database, in `path` or in a temporary directory.

    With no path the directory is on a RAM-backed filesystem where the platform has one
    (`/dev/shm` on Linux and WSL2) and on disk otherwise, and the call says which: the build reads
    and the server maps that directory, so a small corpus on the RAM-backed path touches no disk.
    `close()` removes a temporary directory, and so does interpreter exit.
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
    """A saved database: the directory, its tables and its declaration.

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


def _stored(insert: Insert) -> dict:
    """One insert as the SDK's own JSON copy holds it."""
    return {
        "target": insert.target,
        "kind": insert.kind,
        "role": insert.role,
        "columns": dict(insert.columns),
        "named_attributes": dict(insert.named_attributes),
        "metadata_columns": dict(insert.metadata_columns),
        "source": insert.source,
        "path": str(insert.path),
        "declared_path": insert.declared_path,
        "in_place": insert.in_place,
        "rows": insert.rows,
        "schema": {c: str(t) for c, t in insert.schema.items()},
        "read": list(insert.read),
        "ignored": list(insert.ignored),
        "shape": insert.shape,
        "shape_columns": list(insert.shape_columns),
        "view_key": insert.view_key,
    }


def _restored(stored: dict) -> Insert:
    return Insert(
        target=stored["target"],
        kind=stored["kind"],
        role=stored["role"],
        columns=dict(stored["columns"]),
        named_attributes=dict(stored["named_attributes"]),
        metadata_columns=dict(stored["metadata_columns"]),
        source=stored["source"],
        path=Path(stored["path"]),
        declared_path=stored["declared_path"],
        in_place=stored["in_place"],
        rows=stored["rows"],
        schema={c: _arrow_type(t) for c, t in stored["schema"].items()},
        read=list(stored["read"]),
        ignored=list(stored["ignored"]),
        shape=stored["shape"],
        shape_columns=list(stored["shape_columns"]),
        view_key=stored["view_key"],
    )


def _load(database: Database, state: dict) -> None:
    database.inserts = [_restored(one) for one in state.get("inserts", [])]
    database.pending = [_restored(one) for one in state.get("pending", [])]
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
