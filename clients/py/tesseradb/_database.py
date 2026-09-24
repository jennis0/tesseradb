"""A Tessera database in a directory: declare it, fill it, commit it and read it.

The directory holds everything `tessera build` and `tessera serve` read, so copying it moves the
database: `tessera serve --deployment <dir>/tessera.toml` serves it from wherever it lands.

Beside the declaration file, the package keeps its own copy of the declared blocks and pending
inserts as JSON under `.tessera/`, written at every declaration and insert, so `open()` restores a
database saved before its first commit. It keeps nothing about what the database contains: what
the database holds is always asked of its server.
"""

from __future__ import annotations

import datetime
import json
import os
import re
import shutil
import subprocess
import tempfile
import time
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
from ._viewer import Selection, Viewer

#: The compiled extension that checks a declaration in this process, or `None`, in which case
#: the check runs `tessera check` instead. Both use the same parser.
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
    """A Tessera database in a directory, which you declare, fill, commit and read.

    Make one with `create()` or reopen one with `open()`. Writing takes three steps. The
    `declare_*` methods say what exists: views, columns, vocabularies, annotation layers. They
    take no data. `insert` hands a table to something declared and names the columns it
    needs. `commit` makes what was inserted part of the database: the first commit builds it
    and starts a server over it, and each later one sends only what was inserted since.

    Reading goes through that server, as a reader holding every access term the database's
    rows carry. `view(name)` starts a count, a sample or a map; `viewer(terms)` reads as
    someone holding only those terms.

        db = tesseradb.create()
        db.declare_view("papers")
        db.insert("papers", frame, id="paper_id", x="x", y="y", access="labels")
        db.commit()
        db.view("papers").count()
    """

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
        #: Every access label this database's rows carry, plus each view's default label: the
        #: terms `viewer()` holds when given none. The server has no route that lists them.
        self.terms: list[str] = []
        self.built = (self.path / "bundle" / "CURRENT").exists()
        self._child: subprocess.Popen | None = None
        self.listening: _instance.Listening | None = None
        self._loaded_text: str | None = None
        #: Category keys looked up by `sample()`, per reader: by server address and terms.
        self._keys: dict = {}

    @property
    def binary(self) -> str:
        """The `tessera` program this database runs.

        `TESSERA_BIN` when set, else the first `tessera` on `PATH`, else the one the
        `tesseradb-native` wheel installed, else a checkout's own build.
        """
        return _instance.find_binary()[0]

    def __repr__(self) -> str:
        kind = "a temporary database" if self.temporary else "a database"
        state = "committed" if self.built else "not yet committed"
        return f"{kind} at {self.path}, {state}"

    # ------------------------------------------------------------------ declarations

    def declare(self, kind: str, block: dict) -> dict:
        """Declare one block written with the declaration file's own keys, and return it.

        - `kind`: `"view"`, `"view_group"`, `"vocabulary"`, `"attribute"` or `"layer"`.
        - `block`: the block as a dictionary, with the keys `schema.toml` uses.

        The `declare_*` methods build these dictionaries for you. Use this for a key none of them
        takes.

            db.declare("attribute", {"name": "score", "type": "f64", "index": True})
        """
        if kind == "attribute":
            self._refuse_a_render_column(block.get("name"), block.get("render"))
            self._mark_a_filled_column(block)
        return self._declared(self.blocks.add(kind, block))

    def declare_view(self, name: str, **kwargs) -> dict:
        """Declare a view, and return its block.

        A view is one layout of the items: a map with its own x and y coordinates. One set of items
        can have several views, such as two projections of the same embedding. Its rows come from
        `insert(name, table, id=, x=, y=, access=)`.

        - `name`: the view's name.
        - `extent`: the coordinate range the view covers, as `{"x": [min, max], "y": [min, max]}`,
          or `{"lon": [...], "lat": [...]}` under a projection. By default it is fitted to the rows
          of the first commit with room around them. It cannot change later, so a view declared
          after the first commit must give one.
        - `projection`: `"none"` for plain x and y, or a map projection such as `"web_mercator"`
          for longitude and latitude.
        - `default_label`: the access label of a row whose access column is empty. It is
          `"public"` unless you say otherwise; `None` gives such rows no label.
        - `visibility`: who may see the view at all: `"public"`, or the access label, or list of
          labels, that decides it.
        - `anchor`: `True` makes this the view whose layout orders the items on disk. The first
          view declared is the anchor otherwise.
        - `title`: a display name.

            db.declare_view("papers", extent={"x": [0, 100], "y": [0, 100]})
        """
        return self._declared(self.blocks.add("view", D.view_block(name, **kwargs)))

    def declare_view_group(self, name: str, **kwargs) -> dict:
        """Declare a view group, and return its block.

        A view group is a set of views that share every setting and differ by a key, such as one
        view per year. Each view is named `"<group>:<key>"`. The views and their metadata come from
        `insert(name, roster=table, key=, ...)`, and their rows from `insert(name, table, ...)` with
        `view=` naming the column that says which view each row is in.

        - `name`: the group's name.
        - `metadata`: the metadata each view carries, as `{name: type}`, filled from the roster.
        - `members`: the name of another group whose views this group shares. Such a group has no
          roster or metadata of its own.
        - `extent`, `projection`, `default_label`, `visibility`, `title`: as for `declare_view`,
          applied to every view in the group.

            db.declare_view_group("years", metadata={"year": "i32"})
        """
        return self._declared(self.blocks.add("view_group", D.view_group_block(name, **kwargs)))

    def declare_vocabulary(self, name: str, **kwargs) -> dict:
        """Declare a vocabulary, the values a category column may take, and return its block.

        Each value has a key, a small integer code and an optional title. Its values come from
        `values=` or from `insert(name, table, key=, title=, code=)`.

        - `name`: the vocabulary's name.
        - `closed`: `True` if only the values given are allowed. An open vocabulary adds each new
          value it meets.
        - `width`: the integer size of a code, `"u8"`, `"u16"` (the default) or `"u32"`, which
          limits how many values there can be.
        - `values`: the values, as a list of keys or as `{key: code}` to fix each code.
        - `reserved`: codes never to hand out.
        - `visibility`: `"public"` to list every value to every reader, or `"derived"` to list a
          value only to a reader who may see at least one item carrying it.
        - `title`: a display name.

            db.declare_vocabulary("venue", closed=True, values=["neurips", "icml", "iclr"])
        """
        return self._declared(self.blocks.add("vocabulary", D.vocabulary_block(name, **kwargs)))

    def declare_attribute(self, name: str, type: str, **kwargs) -> dict:
        """Declare a column the items carry, and return its block.

        Its values come from a column of the same name in the table inserted into the anchor view,
        or from `insert(name, table, id=, value=)`.

        - `name`: the column's name.
        - `type`: `"bool"`, an integer type from `"u8"` to `"i64"`, `"f32"`, `"f64"`,
          `"timestamp_us"`, `"keyword"` (a string matched exactly), `"text"` (a string searched by
          word) or `"category"` (a value from a vocabulary).
        - `render`: `True` sends the value with every point drawn, so a map can colour by it. It
          is fixed at the first commit, and a column declared after it cannot be rendered.
        - `index`: `True` makes the column filterable.
        - `vocabulary`: for a category, the vocabulary its values come from.
        - `analyser`: for text, how the text is split into words. `"unicode"` is the one there is.
        - `scope`: `{"group": name}` gives the column a separate value in each view of that
          view group. The default is one value per item.
        - `title`: a display name.

            db.declare_attribute("year", type="i32", render=True, index=True)
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
        """Declare every column of a data frame from its data type, and return what was declared.

        Each column is declared as a detail: stored, and shown when an item is opened, but neither
        drawn nor filterable unless named below. The report returned lists what was declared.

        - `frame`: a pandas or polars data frame or a pyarrow table. Only its column types are read.
        - `skip`: columns to leave out, such as the id and the coordinates.
        - `render`: columns to send with every point drawn.
        - `index`: columns to make filterable.
        - `keyword`, `category`: string columns to declare as keywords or as categories. Other
          string columns are declared as text.

        A categorical column (a pandas `Categorical` or an Arrow dictionary column of strings) is
        declared as a category, unless `keyword` names it. A category declared here reads a new
        open vocabulary of the same name, which adds each value it meets, with codes of width
        `u16`, so it holds at most 65,535 values. The width cannot be changed after the first
        commit; for another width, call `declare_vocabulary` and `declare_attribute` instead.
        A column already declared is left as it is.

            db.declare_columns(frame, skip=["paper_id", "x", "y"], category=["venue"])
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
        return Declared(columns=rows, vocabularies=[block["name"] for block in vocabularies])

    def declare_layer(self, name: str, kind: str, **kwargs) -> dict:
        """Declare an annotation layer, and return its block.

        A layer is a set of annotations over the items, such as one clustering, a set of regions or
        a taxonomy. Each annotation, or artifact, has a key and a set of member items, and a reader
        is shown one only when they may see enough of its members. Its annotations and members
        come from `insert(name, table, id=, key=)`, or from `insert(name, artifacts=...)` and
        `insert(name, members=...)`.

        - `name`: the layer's name.
        - `kind`: how the annotations relate. `"flat"` has no parents; `"nested"` is a tree;
          `"dag"` allows several parents; `"stacked"` holds independent levels; `"tiered"` holds
          levels where each coarser annotation contains finer ones.
        - `views`: the views it is drawn on. The default is every view.
        - `membership`: how members are decided. `"enumerated"` (the default) reads them from a
          table; `"spatial"` makes each annotation a shape whose members are the items inside it;
          `{"attribute": column}` makes one annotation per value of a category column.
        - `shape`: for a spatial layer, the kind of shape: `"bbox"`, `"circle"`, `"ellipse"` or
          `"polygon"`.
        - `default_space`: for a spatial layer, the coordinates shapes are given in: `"view"` or
          `"wgs84"` (longitude and latitude).
        - `levels`: the levels of a layered hierarchy, as `(level, title)` or
          `(level, title, (min_zoom, max_zoom))`.
        - `require_member_visibility`: how much of an annotation's membership a reader must see
          for it to be shown: `"all"`, `"any"`, `"none"`, `{"fraction": 0.1}` or `{"count": 50}`.
        - `visibility`: who may see the layer at all: `"public"` or an access label.
        - `artifact_visibility`: the access label of an annotation that carries none of its own,
          or `{"field": column, "default": label}`. The field names the column each annotation's
          own labels are read from; every artifacts insert names that column with `access=`, and
          an insert naming another column or none is refused. Without a field, the first
          artifacts insert naming `access=` sets it.
        - `computed`: which properties the server computes per reader: `"centroid"`, `"box"` and
          `"hull"`.
        - `supplied`: content you provide per annotation, such as text, as
          `(name, type, gate)` entries.
        - `depends_on`: layers this one attaches to, such as the clustering a label set names.
        - `prune_children`: `True` shows a parent in place of its children when both qualify.
        - `withdraw_on_member_deletion`: `True` removes an annotation when a member is deleted.
        - `value_set`: `"closed"` if no annotation keys may appear beyond those inserted, `"open"`
          otherwise. By default it follows the inserts.
        - `scope`: `{"group": name}` keeps a separate set of annotations per view of that group.
        - `layout`: how the server stores memberships for serving: `"rows"`, `"column"` or
          `"list"`. It changes speed, never answers.
        - `artifacts`: annotations written in the declaration itself, as a list of dictionaries or
          a table.
        - `title`: a display name.

            db.declare_layer("clusters", kind="flat", require_member_visibility={"count": 20})
        """
        block = D.layer_block(name, kind, **kwargs)
        self._refuse_an_undeclared_group("layer", name, block)
        return self._declared(self.blocks.add("layer", block))

    def declare_labels(self, name: str, of: str, **kwargs) -> dict:
        """Declare text labels for the annotations of another layer, and return their block.

        Each label names one annotation of the layer `of`, such as a topic line for a cluster, and
        is drawn where that annotation is drawn. Its text comes from `insert(name, {key: text})` or
        `insert(name, table, key=, text=)`.

        - `name`: the label set's name.
        - `of`: the layer it labels.
        - `content_requires`: `"inherited"` (the default) shows a label wherever its annotation is
          shown. `"all"` shows it only to a reader who may see every item the text was written
          from; those items come from `insert(name, members=table, id=, key=)`.
        - `require_member_visibility`, `title`: as for `declare_layer`.
        - `artifact_visibility`: the access label of every label in the set. A label carries none
          of its own, so no `field` is taken.

            db.declare_labels("topics", of="clusters")
            db.insert("topics", {"c0": "graph neural networks", "c1": "diffusion models"})
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
        """Mark an attribute declared after the first commit, which only inserts fill.

        The mark is never written. It keeps the written declaration from naming a source for the
        column, since the files the first commit read do not carry it.
        """
        if self.built and not block.get("scope"):
            block[D.FILLED] = True

    def _refuse_a_render_column(self, name: Any, render: Any) -> None:
        """Refuse `render=True` after the first commit.

        The server cannot add a rendered column to rows that already exist, so the refusal comes
        here, where the user can still change the declaration.
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
        """Hand a table to something declared, naming the columns it reads, and return a record.

        - `target`: the name of a declared view, view group, attribute, layer, label set or
          vocabulary.
        - `table`: a pandas or polars data frame, a pyarrow table, or the path of a Parquet file,
          which is read where it lies.
        - `roster`, `artifacts`, `members`: tables of a view group's views, a layer's annotations,
          and a layer's memberships. Each is its own insert, since their columns share names.
        - `columns`: on the anchor view's insert, `{attribute: column}` for an attribute filled
          from a column with another name.
        - the other keywords: which column of the table holds each thing the target needs, such as
          `id=`, `x=`, `y=` and `access=` for a view.

        A categorical column (a pandas `Categorical` or an Arrow dictionary column) is read as the
        values it holds, wherever a column of those values is read. A column the call does not
        name is ignored, and the record returned says what was read and what was ignored. On the
        anchor view, a column named like a declared attribute fills that attribute. Several inserts into one target add up. Nothing is sent until `commit()`.

            db.insert("papers", frame, id="paper_id", x="x", y="y", access="labels")
            db.insert("clusters", frame, id="paper_id", key="cluster")
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
        self._refuse_a_label_column_the_layer_does_not_read(target, kind, role, block, named)
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
        return insert

    def _target(
        self, target: str, named: dict, roster=None, artifacts=None, members=None
    ) -> tuple[str, dict]:
        """The declared block this name refers to, or a refusal naming how to declare one.

        A category column and its vocabulary may share a name. The columns the call names then say
        which is meant: an attribute reads `id=` and `value=`, a vocabulary `key=`, `title=` and
        `code=`.
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
        """Refuse rows for a scoped layer that do not name their view."""
        scope = block.get("scope")
        group = scope.get("group") if isinstance(scope, dict) else None
        if group is not None and role in ("artifacts", "members", "key") and "view" not in named:
            raise Refusal(
                f"insert into {kind} {target!r}: this layer is scoped to group {group!r} and keys "
                f"its artifacts per view, one key in two views being two artifacts, so every row "
                f"carries the view it belongs to. Name the column that says which with view="
            )

    def _refuse_a_label_column_the_layer_does_not_read(
        self, target: str, kind: str, role: str, block: dict, named: dict
    ) -> None:
        """Refuse an artifacts insert whose `access=` is not the column the layer reads its labels
        from: the column the layer declares, a commit sent or an uncommitted insert named.

        An insert naming no column is refused where the layer reads one, before and after the
        first commit, since its artifacts would reach the layer with no labels.
        """
        if kind != "layer" or role != "artifacts":
            return
        column = named.get("access") or None
        held = dict(block.get("artifact_visibility") or {}).get("field") or next(
            (
                one.columns["access"]
                for one in (self.pending if self.built else self.inserts)
                if one.target == target and one.kind == "layer" and one.role == "artifacts"
                and one.columns.get("access")
            ),
            None,
        )
        if column is None and held is not None:
            raise Refusal(
                f"insert into layer {target!r}: this layer reads each artifact's own access labels "
                f"from column {held!r}, and this insert names no access column. Name it with "
                f"access={held!r}, giving that column nulls for artifacts with no label of their "
                f"own"
            )
        if column is not None and held is not None:
            D.carry_labels(target, {"artifact_visibility": {"field": held}}, column)

    def _refuse_a_second_view_without_its_labels(
        self, kind: str, role: str, target: str, insert: Insert
    ) -> None:
        """Refuse rows for a second view that name no access column.

        An item carries the same labels in every view, and the build and the server both refuse rows
        whose labels disagree, so the missing column is refused here.
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
        """The attributes a table inserted into the anchor view fills, as `{attribute: column}`.

        A column fills the declared attribute of the same name, or the one `columns=` maps it to.
        Attribute-named columns in any other view's table are ignored.
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
            # The id column never fills an attribute. A coordinate or label column can.
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
        """The `[sources]` key an insert is written under, unique among this database's inserts."""
        stem = target.replace("/", "_").replace(":", "_")
        if view_key is not None:
            stem = f"{stem}_{view_key}".replace("-", "_").replace("/", "_")
        key = stem if role in ("rows", "values", "text") else f"{stem}_{role}"
        taken = {insert.source for insert in self.inserts + self.pending}
        # A category column and its vocabulary may share a name; the second is named for its kind.
        candidate, at = key, 1
        if candidate in taken:
            candidate = f"{key}_{kind}"
        while candidate in taken:
            at += 1
            candidate = f"{key}_{at}"
        return candidate

    def _accumulated(self, insert: Insert) -> Insert:
        """Join a second insert into the same target before the first commit onto the first.

        A block reads one file, so the parts are written together under `sources/`. Parts that name
        different columns, or whose column types differ, are refused.
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
        """A label set's text as the annotations table its layer reads, and the columns it names.

        Given a mapping from key to text, or a table with `text=`, the table is written here and
        each label is attached to the annotation with its own key in the layer it labels. Given a
        table with `contents=`, the table is read as it is and must name its attachment.
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
            table = _inserts.decoded(
                _inserts.as_table(data) if not _inserts.is_path(data) else pq.read_table(data)
            )
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
        """The declaration as the TOML text the database's `schema.toml` holds."""
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
        """Anchor on the first view of the first group's roster when no plain view is declared."""
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
        """Write `schema.toml` and `tessera.toml` into the directory, and return the declaration.

        `check()` and `commit()` do this themselves. Call it to look at the files first.
        """
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
        """What the next `commit()` would do, with nothing sent, as a report.

        Before the first commit it reads the declaration against the inserted files and reports
        each problem it finds, reading the files' column types only. After it, the report lists the
        requests the commit would send and any problem found before sending.

            db.check()
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
            rows=self._inserted_rows(),
            findings=findings,
            log=page,
        )

    def _checked(self) -> tuple[bool, str]:
        """Check the written declaration, and return whether it passed and the page it printed.

        The check runs in this process where the extension is installed, and as `tessera check`
        otherwise. The page is the same either way.
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
        """Make what was inserted part of the database, and return a report of what happened.

        The first commit checks the declaration, builds the database from the inserted tables and
        starts a server over it. Three things are fixed then and cannot change: each view's extent,
        the column types, and which columns are rendered.

        Each later commit sends what was inserted since the last one to the running server and
        waits until it can be read. Then the inserts are forgotten.

        A commit that did nothing raises `Refusal`, with the report as its `report`: a problem found
        before anything was sent, a build that failed, or every request refused. A commit in which
        some requests succeeded returns its report, with the refusals listed.

            db.commit()
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
        started = time.monotonic()
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
            rows=self._inserted_rows(),
            log=page + "\n" + build.stdout + build.stderr,
            identity=self._identity_in_words(),
        )
        if build.returncode != 0:
            raise Refusal("commit: the build failed\n" + report.log, report)
        self.built = True
        self._record_label_columns(document.get("layer", []))
        self._record_terms(document)
        self.serve()
        report.seconds = time.monotonic() - started
        if self.listening is not None:
            report.viewer = self.listening.viewer
            report.session = self.listening.session
            report.control = self.listening.control
        for view in self.meta().get("views", []):
            name = view.get("group") or view["id"]
            report.views[name] = report.views.get(name, 0) + 1
        # From the declaration: a layer gated on a label no row carries is in no reader's meta.
        for block in document.get("layer", []):
            report.layers.append(block["name"])
            if "labels" in block:
                report.layers.append(block["labels"]["name"])
        built = _BUILT.search(build.stdout + build.stderr)
        if built is not None:
            report.items = int(built.group("items"))
            report.minted = int(built.group("minted"))
            report.unclustered = int(built.group("unclustered"))
        return report

    def _inserted_rows(self) -> dict:
        """The rows inserted before the first commit, by view or view group."""
        rows: dict = {}
        for insert in self.inserts:
            if insert.role == "rows":
                rows[insert.target] = rows.get(insert.target, 0) + insert.rows
        return rows

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
            # The closing flush is where the commit waits, so the plan lists it.
            plan=[page.line for page in pages]
            + (["flush, and wait for the publication it arms"] if pages else []),
            findings=findings,
        )
        if not sent:
            return report
        if report.findings:
            raise Refusal(str(report), report)
        accepted = C.run(control, pages, report)
        self._record_label_columns(page.body for page in accepted if page.kind == "layer")
        self._record_terms(self._document())
        self.pending.clear()
        self._save_state()
        if pages and not accepted:
            raise Refusal(str(report), report)
        return report

    @property
    def control(self) -> Control:
        """A client for this database's operator endpoint, starting the server if needed."""
        listening = self.serve()
        credential = (self.path / ".tessera" / "operator.cred").read_text(encoding="utf-8")
        return Control(f"http://{listening.control}", credential.strip())

    def _payloads(self) -> dict:
        """The request bodies the declaration becomes at a running server, one entry per block kind.

        `tessera check --payloads` writes them, so the binary serialises what it parsed.
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
        """A token for reading this database, made with its own session credential.

        - `terms`: the access terms the token grants. By default it grants every access label the
          database's rows carry, and each view's default label.

        Pass the token to `connect` or `Map` to read as that reader.

            token = db.token(["cs.LG"])
        """
        self.serve()
        chosen = list(terms) if terms is not None else list(self.terms)
        return authorise(self.session_url, self.session_credential, chosen)

    def viewer(self, terms: Sequence[str] | None = None) -> Viewer:
        """A reader of this database holding only the access terms given.

        - `terms`: the access terms. The reader sees an item when it holds one of the item's
          labels. By default it holds every label the database's rows carry, which sees everything.

        Every count, map and record the reader is given covers only what those terms let it see.
        An empty list is refused, since such a reader sees nothing. A term no row carries is
        accepted and reaches nothing.

            db.viewer(["cs.LG"]).view("papers").count()
            db.viewer(["cs.LG"]).map()
        """
        self._refuse_before_the_first_commit("viewer")
        if terms is not None and not list(terms):
            raise Refusal(
                "viewer: a reader holding no terms sees nothing. Name at least one term, or "
                "call viewer() with no terms to read everything"
            )
        chosen = list(terms) if terms is not None else list(self.terms)
        self.serve()
        reader = Viewer(self.viewer_url, lambda: self.token(chosen), terms=chosen)
        # A new token is made for each call, so a commit's new views are seen, while the category
        # keys already looked up are kept for every reader holding the same terms.
        reader._keys = self._keys.setdefault((self.viewer_url, frozenset(chosen)), {})
        return reader

    def map(
        self,
        view: str | None = None,
        layers: Sequence[str] | None = None,
        colour_by: str | None = None,
        filters: dict | None = None,
        height: int = 480,
        **kwargs,
    ):
        """The interactive map of this database, as a notebook widget, showing everything.

        - `view`: the view to open on. `None` opens the first one.
        - `layers`: the annotation layers to draw. `None` lets the map choose and `[]` draws none.
        - `colour_by`: the column to colour points by, or `"cluster:<layer>"`.
        - `filters`: a filter expression to apply, as `Selection.filter` takes one.
        - `height`: the widget's height in pixels.

        Other keywords go to `Map` unchanged. `db.view(name).map()` opens on a selection instead,
        and `db.viewer(terms).map()` shows what a reader holding those terms sees.

            db.map(colour_by="cluster:clusters")
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
        """The database's structure as its server describes it, as a dictionary.

        It lists the views with their coordinate ranges, the annotation layers, the columns and
        how each can be filtered, and the server's limits.
        """
        self._refuse_before_the_first_commit("meta")
        return self.viewer().meta()

    def item(self, tessera_id, idset: int | None = None) -> dict:
        """One item's full record.

        - `tessera_id`: the item's id, as a sample's `tessera_id` column or a map pick gives it.
        - `idset`: as for `Viewer.item`.

        The record has `fields`, `labels` and `views` as `Viewer.item` describes, and
        `external_id`, the id the item was inserted with, as the type its id column had: an integer
        column gives an integer and a string column a string.

            db.item(db.view("papers").sample(k=1).column("tessera_id")[0].as_py())
        """
        self._refuse_before_the_first_commit("item")
        record = self.viewer().item(tessera_id, idset)
        if record.get("external_id") is not None:
            record["external_id"] = self._inserted_id(record["external_id"])
        return record

    def _inserted_id(self, raw: bytes):
        """External-id bytes read back as the type the id column had."""
        insert = self._identity_insert()
        dtype = None if insert is None else insert.id_type
        if is_integer_type(dtype):
            # Eight little-endian bytes, signed where the column was.
            return int.from_bytes(raw, "little", signed=str(dtype).startswith("int"))
        if dtype is not None and (pa.types.is_string(dtype) or pa.types.is_large_string(dtype)):
            return raw.decode()
        return raw

    def view(self, name: str) -> Selection:
        """The whole of one view, as this database's own reader sees it: every item.

        `name` is a view's name. A view in a view group is named `"<group>:<key>"`. An unknown
        name is refused, and the refusal lists the views there are.

            db.view("papers").count()
            db.view("papers").filter({"venue": {"eq": "neurips"}}).map()
        """
        self._refuse_before_the_first_commit("view")
        self.viewer()._require_view(name)
        return Selection(self.viewer, name)

    def categories(
        self,
        column: str,
        prefix: str | None = None,
        view: str | None = None,
        codes: Sequence[int] | None = None,
    ):
        """The values of a category column, as a pyarrow table.

        This is `Viewer.categories` as this database's own reader, which sees every value.
        `prefix` lists only the values starting with it, with a count of items for each; `view`
        names the view to read a column declared for a view group in; `codes` lists only the
        values of those codes.

            db.categories("venue")
            db.categories("venue", prefix="neur")
            db.categories("venue", codes=[1, 3])
        """
        self._refuse_before_the_first_commit("categories")
        return self.viewer().categories(column, prefix, view, codes)

    def _id_arguments(self) -> list[str]:
        """The build's `--mint-external-ids` flag, where the id column is an integer.

        A string or binary id column is written as the external id without a flag. An integer one
        needs the flag, and every later commit addresses rows by it.
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

    def _record_label_columns(self, layers: Iterable[dict]) -> None:
        """Write onto the SDK's own layer blocks the label column each layer was committed with,
        so a later insert naming another column is refused."""
        for layer in layers:
            field = dict(layer.get("artifact_visibility") or {}).get("field")
            if field:
                D.carry_labels(layer["name"], self.blocks.layer(layer["name"]), field)
        self._save_state()

    def _record_terms(self, document: dict) -> None:
        """Remember every access label inserted so far: the terms `viewer()` holds by default."""
        for term in self._inserted_terms(document):
            if term not in self.terms:
                self.terms.append(term)
        self._save_state()

    def _inserted_terms(self, document: dict) -> list[str]:
        """Every access label inserted into a view or onto an artifact, plus each view's default
        label and each layer's named default."""
        terms: list[str] = []
        for block in document.get("view", []) + document.get("view_group", []):
            default = dict(block.get("point_visibility") or {}).get("default")
            if default:
                terms.append(default)
        for block in document.get("layer", []):
            default = dict(block.get("artifact_visibility") or {}).get("default")
            if default and default != "inherited":
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
        """Delete items by the ids their id column holds, and return a report.

        - `ids`: the ids, or the `tessera_id`s where the rows were inserted without an id column.

        The items stop being served at once. Their rows are removed from disk at the next
        compaction; `compact()` asks for one. To change an item, remove it and insert it again.

            db.remove(["paper-17", "paper-23"])
        """
        return self._changes(ids, "delete")

    def suppress(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Hide items by their ids until `unsuppress` lifts it, and return a report.

        - `ids`: as for `remove`.

        A hidden item is left out of every answer from the moment the call returns.
        """
        return self._changes(ids, "suppress")

    def unsuppress(self, ids: Iterable[Hashable]) -> ChangeReport:
        """Show items hidden by `suppress` again, and return a report.

        - `ids`: as for `remove`.
        """
        return self._changes(ids, "unsuppress")

    def addresses(self, ids: Iterable[Hashable]) -> list[dict]:
        """The ids given, in the form the server's change requests take them.

        Where the rows were inserted with an id column, each id is sent as the bytes that column
        held. Otherwise each is a `tessera_id`, sent with the id numbering it belongs to.
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
        """Take items out of the set a label's text was written from, and return a report.

        A label declared with `content_requires="all"` is shown only to a reader who may see every
        item its text was written from. This removes items from that set.

        - `layer`: the label set.
        - `key`: the label's key.
        - `ids`: the items to take out.
        - `rank`: which of the label's texts, where it has several.
        - `level`: the level the label is at.
        - `view`: the view the label belongs to, on a layer scoped to a group.

        Taking out every item withdraws the text; insert it again to replace it.
        """
        self._refuse_before_the_first_commit("leave")
        wanted = list(ids)
        report = ChangeReport(op=f"leave {layer}/{key} rank {rank}", requested=len(wanted))
        answer = C.leave(self.control, layer, key, wanted, rank, level, view)
        if not answer.ok:
            report.refusals.append({"status": answer.status, "detail": answer.detail[:1000]})
        return report

    def status(self) -> dict:
        """The server's own report on this database, as a dictionary.

        It holds what the operator sees: how far writes have got, queue depths, and the page sizes
        each write request accepts.
        """
        self._refuse_before_the_first_commit("status")
        return _accepted(self.control.status_answer(), "status")

    def compact(self) -> dict:
        """Ask the server to remove the rows of deleted items from disk now.

        It returns once the server has accepted the request, before the work is done.
        """
        self._refuse_before_the_first_commit("compact")
        return _accepted(self.control.compact(), "compact")

    def drop_layer(self, name: str, wait: bool = False) -> dict:
        """Remove an annotation layer.

        - `name`: the layer's name. It cannot be used for a new layer afterwards.
        - `wait`: `True` returns only once readers no longer see the layer.
        """
        self._refuse_before_the_first_commit("drop_layer")
        return _accepted(self.control.drop_layer(name, wait), f"drop_layer {name}")

    def drop_view(
        self, group: str, key: str, delete_dangling: bool = False, wait: bool = False
    ) -> dict:
        """Remove one view of a view group, and return the server's answer.

        - `group`, `key`: the view is `"<group>:<key>"`.
        - `delete_dangling`: `True` also deletes the items that were in no other view. The
          answer's `deleted` says how many. A deletion cannot be undone.
        - `wait`: `True` returns only once readers no longer see the view.

        Without `delete_dangling`, no item is deleted.
        """
        self._refuse_before_the_first_commit("drop_view")
        return _accepted(
            self.control.drop_view(group, key, delete_dangling, wait), f"drop_view {group}/{key}"
        )

    def revoke(self, token) -> None:
        """End a token this database made, so it can no longer read.

        - `token`: the `Token`, or its `token_id`.

        Only the token's id is sent. An id that names no live token is accepted without comment.
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
        """Start the server if it is not running, and return the addresses it listens on.

        Reading and later commits do this themselves.
        """
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
        """The secret this database makes its tokens with. It stays on this machine."""
        return (self.path / ".tessera" / "session.cred").read_text(encoding="utf-8").strip()

    @property
    def viewer_url(self) -> str | None:
        """The address readers read from, or `None` if the server is not running."""
        return None if self.listening is None else f"http://{self.listening.viewer}"

    @property
    def session_url(self) -> str | None:
        """The address tokens are made at, or `None` if the server is not running."""
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
        """Copy the database directory to the empty directory `path`, and return that path.

        A temporary database is deleted when it is closed, so this is how to keep one. The copy
        can be reopened with `open(path)` or served with
        `tessera serve --deployment <path>/tessera.toml`.
        """
        target = Path(path).expanduser()
        if target.exists() and any(target.iterdir()):
            raise Refusal(f"save: {target} is not empty")
        shutil.copytree(self.path, target, dirs_exist_ok=True)
        return target

    def close(self) -> None:
        """Stop the server, and delete the directory if the database is a temporary one.

        Tokens the database made stay valid until they expire, within the hour; `revoke` ends one
        sooner. A directory you named is never deleted.
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


#: The build's closing line: `built <bundle> (v…): N items, …, M artifact(s) minted, K
#: unclustered member row(s)`.
_BUILT = re.compile(
    r"^built .*: (?P<items>\d+) items, .* (?P<minted>\d+) artifact\(s\) minted, "
    r"(?P<unclustered>\d+) unclustered member row\(s\)$",
    re.MULTILINE,
)


# ---------------------------------------------------------------------- create and open


def create(path: str | os.PathLike | None = None, replace: bool = False) -> Database:
    """Make a new, empty database, in `path` or in a temporary directory.

    - `path`: the directory. It must be empty or not yet exist. Without it the database goes in
      a temporary directory, in memory where the system offers one (`/dev/shm` on Linux), and is
      deleted when closed or when Python exits.
    - `replace`: `True` deletes a Tessera database already at `path` first. A directory that holds
      anything else is refused.

    `db.path` is where the database is, and `db.binary` the `tessera` program it runs.

        db = tesseradb.create()
        db = tesseradb.create("~/maps/papers", replace=True)
    """
    if path is None:
        parent = RAM_BACKED if RAM_BACKED.is_dir() else None
        directory = Path(tempfile.mkdtemp(prefix="tesseradb-", dir=parent))
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
    return database


def open(path: str | os.PathLike) -> Database:  # noqa: A001
    """Reopen a database saved in `path`.

    A committed database reopens ready to read, and its next commit adds to it. One saved before
    its first commit reopens with its declarations and inserts as they were left.

        db = tesseradb.open("~/maps/papers")
        db.view("papers").count()
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


#: How an inline TOML table is marked in the JSON copy, to tell it from a block of its own.
INLINE = "__inline__"

#: How a TOML date-time, which a view group's metadata may hold, is marked in the JSON copy.
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
