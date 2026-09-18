"""A later commit: the plan, the pre-flight, and the pages that carry it (python-sdk.md §6).

The first commit builds. Every commit after it pages what was inserted since the last one through
the control plane, in the order §6.2 fixes: declarations, points per view, values on existing
entities, artifacts per layer, then a flush that waits for the publication it arms. A part supplied
twice is accepted and a part supplied differently is a `409` on that part (ingest §1.1), so the
order matters for existence and for nothing else.

`check()` runs the planner and the pre-flight and sends nothing; `commit()` runs the same plan. The
two therefore cannot disagree about what would be sent.

**The plan is built from what was inserted and what the database says it holds.** The SDK keeps no
record of what it sent: a table goes as it was inserted, and a row the database already holds is a
`409` on that page which the report carries. What the database has already been told is read from
`/v1/meta` rather than from a log: its views, its groups and its layers (§3, §6.4). A re-run of a
cell is a re-run.

**The target decides the route.** An insert into a view is a page of points, one into an attribute
or a layer by key fills cells on entities the database holds, and a layer's two tables are
publications. Nothing is inferred from the columns a table carries: the target was declared and
the call named its columns.

**Nothing is dropped or rewritten.** What the call named is what is sent, and a finding stops the
plan rather than trimming it (§6.3). The user corrects the data or the declaration and commits
again.

**The commit waits once, at the end.** Every acknowledgement names the publication its work
becomes visible in, and `?wait=visible` holds a route's answer until the counter has reached that
number (decision 0144). Every page of the plan goes unwaited and one `POST
/control/flush?wait=visible` closes the commit: the flush arms a cycle and then waits on the
number that cycle will carry, so it covers every page before it and the SDK reads no counter of
its own. `visible: true` ends the commit. `visible: false` — the server's
`serve.visible_wait_max_secs` reached — is a finding: the write happened and is durable, and what
it wrote reaches the served forms at the next cycle.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Sequence

import pyarrow as pa

from . import _control
from ._control import Answer, Control, addressed, arrow_body, batch_id
from . import _declaration as _D
from ._declaration import SHAPE_FIELDS, rows_of
from ._refusal import Refusal

@dataclass
class Finding:
    """One pre-flight finding (§6.3).

    A finding refuses the commit. The pre-flight reports and sends nothing while one stands, and
    it never drops a row or a column to make the rest sendable: what the user inserted is what a
    commit sends, or the commit does not happen.
    """

    what: str
    detail: str

    def __str__(self) -> str:
        return f"refused: {self.what}. {self.detail}"


@dataclass
class Page:
    """One request the plan will make, with its body already built.

    The body is built here rather than at send time because a batch id is a hash of the bytes
    (§6.4): a body re-serialised before the retry would be a new batch rather than a replay.
    """

    kind: str
    name: str
    line: str
    body: Any = None
    batch: str | None = None
    view: str | None = None
    rows: int = 0
    artifacts: int = 0
    members: int = 0
    level: int = 0


#: How many `{key, title?}` rows one `PATCH /control/vocabularies/{name}/values` carries. The
#: route publishes a body cap (`limits.declarations.max_body_bytes`, 2 MiB) and no row cap, so the
#: bytes are what bound a page and this is the SDK's own figure for how many rows to measure at a
#: time: long titles make a page over the cap, which is halved and measured again.
VALUES_PER_PAGE = 10_000

#: Room left under the body cap for a vocabulary declaration's own fields — its width, value set,
#: visibility, title and reserved codes — when the first page of values travels on it.
DECLARATION_HEADROOM = 4096


def _block(document: dict, kind: str, name: str) -> dict:
    """One declared block by kind and name, or an empty one where the document carries none."""
    for block in document.get(kind, []):
        if block.get("name") == name:
            return block
    for block in document.get("layer", []):
        labels = block.get("labels")
        if isinstance(labels, dict) and labels.get("name") == name:
            return labels
    return {}


def _scoped_to(block: dict) -> str | None:
    """The view group a block's `scope` names, or `None` where it is entity-scoped."""
    scope = block.get("scope")
    return scope.get("group") if isinstance(scope, dict) else None


def _owners_first(groups: list[dict]) -> list[dict]:
    """The groups in declaration order, each after the group its `members` names (views.md §3.3).

    A group taking another's views is a `404` at the route until that group exists, and a chain is
    refused at the declaration, so one pass placing an owner before its sharer is the whole of it.
    """
    by_name = {entry["name"]: entry for entry in groups}
    ordered: list[dict] = []
    for entry in groups:
        owner = by_name.get(entry["body"].get("members"))
        if owner is not None and owner not in ordered:
            ordered.append(owner)
        if entry not in ordered:
            ordered.append(entry)
    return ordered


# ---------------------------------------------------------------------------- the pre-flight


def rows_with_no_id(inserts: Sequence[Any], findings: list[Finding]) -> None:
    """Rows with no id where the insert names an id column (§6.3).

    A row whose id is null is a row no member table and no value can reach, and the SDK mints
    nothing in its place. The finding lists them; the remedy is the data's.
    """
    for insert in inserts:
        column = insert.columns.get("id")
        if column is None or not insert.rows:
            continue
        values = insert.table()[column]
        empty = values.null_count
        if empty:
            findings.append(
                Finding(
                    "rows with no id",
                    f"the insert into {insert.kind} '{insert.target}' names id='{column}', and "
                    f"{empty} of its {insert.rows} row(s) carry no value there. A row's id is its "
                    f"address at every door, and the SDK mints none. Fill the column, or insert "
                    f"the rows with no id= at all, which names them by their tessera_id",
                )
            )


def keys_into_supplied_content(
    document: dict, inserts: Sequence[Any], findings: list[Finding]
) -> None:
    """A key column inserted into a layer that declares supplied content (§6.3)."""
    supplied = {
        block["name"]
        for block in document.get("layer", [])
        if (block.get("content") or {}).get("supplied")
    }
    for insert in inserts:
        if insert.role != "key" or insert.target not in supplied:
            continue
        findings.append(
            Finding(
                "a key column inserted into a layer with supplied content",
                f"layer '{insert.target}' declares supplied content, and an artifact served "
                f"without content its layer declares cannot be told from one whose content was "
                f"withheld. Such a layer takes an artifacts table: "
                f"insert('{insert.target}', artifacts=…, key=…, contents=…) beside "
                f"insert('{insert.target}', members=…, id=…, key=…)",
            )
        )


class Planner:
    """§6.2's plan and §6.3's pre-flight over what was inserted since the last commit."""

    def __init__(self, database, control: Control, meta: dict) -> None:
        self.db = database
        self.control = control
        self.meta = meta
        self.limits = control.limits()
        self.findings: list[Finding] = []
        self.pages: list[Page] = []
        #: What the database says it already carries: the views, the groups and the layers
        #: `/v1/meta` lists. A declaration it names is one this commit does not send again.
        self.held_views = {str(view.get("id")) for view in meta.get("views", [])}
        self.held_views |= {str(group.get("name")) for group in meta.get("groups", [])}
        self.held_layers = {str(layer.get("name")) for layer in meta.get("layers", [])}
        #: The layers this commit mints artifacts into at the values route, and the layers its
        #: publications attach to or publish into: where the two meet, the mint is flushed first.
        self._minted: set[str] = set()
        self._attached_to: set[str] = set()
        self._published: set[str] = set()
        #: The columns `/v1/meta` publishes, entity-scoped and group-scoped alike, and the
        #: vocabularies their categories name. `/v1/meta` carries no vocabulary list of its own: a
        #: value set is published through the column that reads it, so a vocabulary declared with
        #: no column yet is not held here and its declaration is sent again, which the route
        #: answers as an identical redeclaration (contracts §3.4).
        columns = list(meta.get("declared_scalars", [])) + list(meta.get("scoped_scalars", []))
        self.held_attributes = {str(column.get("name")) for column in columns}
        self.held_vocabularies = {
            str((column.get("category") or {}).get("vocabulary"))
            for column in columns
            if column.get("category")
        }

    @property
    def inserts(self) -> list:
        return list(self.db.pending)

    def _for(self, kind: str, role: str, target: str | None = None) -> list:
        return [
            one
            for one in self.inserts
            if one.kind == kind and one.role == role and (target is None or one.target == target)
        ]

    # ------------------------------------------------------------------ entry

    def plan(self) -> tuple[list[Page], list[Finding]]:
        document = self.db._document()
        rows_with_no_id(self.inserts, self.findings)
        keys_into_supplied_content(document, self.inserts, self.findings)
        self._declarations(document)
        rows = self._phase(self._rows, document)
        values = self._phase(self._values, document)
        artifacts = self._phase(self._artifacts, document)
        self.pages += rows
        if rows and values:
            # A value addresses a row the database holds, so the rows this commit sent are made
            # visible before the values that fill them (§6.2 step 2).
            self.pages.append(_flush("the rows", "makes them visible"))
        self.pages += values
        if artifacts and (self._minted & (self._attached_to | self._published)):
            # A publication attaching to or growing a key this commit's values step mints: a
            # minted artifact is resolvable only from its publication, so the mint goes first
            # (§6.2 step 4).
            named = ", ".join(sorted(self._minted & (self._attached_to | self._published)))
            self.pages.append(
                _flush("the values", f"makes the artifacts they mint into {named} resolvable")
            )
        self.pages += artifacts
        return self.pages, self.findings

    def _phase(self, step, document: dict) -> list[Page]:
        """One step's pages, taken aside so the flush between two of them can be placed."""
        held, self.pages = self.pages, []
        step(document)
        mine, self.pages = self.pages, held
        return mine

    # ------------------------------------------------------------------ 1. declarations

    def _declarations(self, document: dict) -> None:
        """The runtime `PUT`s for every block this database does not carry yet (§6.2 step 1).

        The bodies come from `tessera check --payloads` over the SDK's own declaration, so the
        mapping from a block to a request body is the binary's and not a second one in Python. The
        one body it does not carry is a group's roster record, which is a request rather than a
        declaration (views.md §3.2): the SDK builds it from the roster it was given.

        Which of them are new is read from `/v1/meta`: the database is what knows what it holds.
        The order is the one the routes need: a group before a view of it, a view before the layer
        drawn on it, a vocabulary before the attribute that names it, and every declaration before
        the values pages that fill the columns it declared.
        """
        payloads = self.db._payloads() if self._pending(document) else {}
        for entry in _owners_first(payloads.get("view_groups", [])):
            name = entry["name"]
            if name in self.held_views:
                continue
            self.pages.append(
                Page(
                    kind="view_group",
                    name=name,
                    line=f"declare view group '{name}'",
                    body=entry["body"],
                )
            )
        self._roster(document)
        for entry in payloads.get("views", []):
            name = entry["name"]
            if name in self.held_views:
                continue
            self.pages.append(
                Page(
                    kind="view",
                    name=name,
                    view=name,
                    line=f"declare view '{name}'",
                    body=entry["body"],
                )
            )
        for entry in payloads.get("vocabularies", []):
            self._vocabulary(entry)
        for name in self._held_vocabularies_with_values():
            self._value_pages(name)
        for body in payloads.get("attributes", []):
            name = body["name"]
            if name in self.held_attributes:
                continue
            self.pages.append(
                Page(
                    kind="attribute",
                    name=name,
                    line=f"declare attribute '{name}' ({body['type']})",
                    body=body,
                )
            )
        for payload in payloads.get("layers", []):
            name = payload["name"]
            if name in self.held_layers:
                continue
            self.pages.append(
                Page(
                    kind="layer",
                    name=name,
                    line=f"declare layer '{name}' ({payload['hierarchy']['kind']})",
                    body=payload,
                )
            )

    def _roster(self, document: dict) -> None:
        """A view of a group, created from the roster its group was given (views.md §3.2)."""
        for insert in self._for("view_group", "roster"):
            block = next(
                (one for one in document.get("view_group", []) if one["name"] == insert.target),
                {},
            )
            names = _D.metadata_names(block)
            table = insert.table()
            records = _records(table, insert)
            for record in records:
                view = f"{insert.target}:{record['key']}"
                if view in self.held_views:
                    continue
                self.pages.append(
                    Page(
                        kind="group_view",
                        name=view,
                        view=view,
                        line=f"create view '{view}' of group '{insert.target}'",
                        body=_D.roster_body(record, names),
                    )
                )

    def _held_vocabularies_with_values(self) -> list[str]:
        """A vocabulary the database already holds, with values inserted since the last commit."""
        return [
            insert.target
            for insert in self._for("vocabulary", "values")
            if insert.target in self.held_vocabularies
        ]

    def _vocabulary(self, entry: dict) -> None:
        """One vocabulary's declaration and the pages of its values (§6.2 step 1).

        The body is the emitter's. A closed set declared at a running service carries its inserted
        keys inline, because the route refuses a closed set declared with no values: the set is
        the authority on what may be ingested, and an empty one refuses every value. The pages
        that follow carry the same values with their titles; a key already bound binds nothing and
        the route answers such a page without a record (contracts §3.4).
        """
        name = entry["name"]
        if name in self.held_vocabularies:
            return
        body = dict(entry["body"])
        self.pages.append(
            Page(
                kind="vocabulary",
                name=name,
                line=f"declare vocabulary '{name}' ({body.get('value_set')})",
                body=body,
                rows=len(body.get("values", [])),
            )
        )
        self._value_pages(name)

    def _value_pages(self, name: str) -> None:
        """`PATCH /control/vocabularies/{name}/values` for what was inserted into it."""
        values: list[dict] = []
        for insert in self._for("vocabulary", "values", name):
            table = insert.table()
            keys = table[insert.columns["key"]].to_pylist()
            title = insert.columns.get("title")
            titles = table[title].to_pylist() if title else [None] * table.num_rows
            if insert.columns.get("code"):
                self.findings.append(
                    Finding(
                        "a value set carrying its own codes",
                        f"the insert into vocabulary '{name}' names code="
                        f"'{insert.columns['code']}'. A code is the server's to assign "
                        f"(per-point-attributes §3.1) and both vocabulary routes refuse a body "
                        f"that names one. Drop code=, and read the codes back from the values "
                        f"verb",
                    )
                )
                return
            for key, shown in zip(keys, titles):
                if key is None:
                    continue
                row = {"key": str(key)}
                if shown is not None:
                    row["title"] = str(shown)
                values.append(row)
        cap = int(self.limits.get("declarations", {}).get("max_body_bytes", 2 << 20))
        for page in _value_pages(values, cap - DECLARATION_HEADROOM):
            self.pages.append(
                Page(
                    kind="vocabulary_values",
                    name=name,
                    line=f"page {len(page)} value(s) into vocabulary '{name}'",
                    body={"values": page},
                    rows=len(page),
                )
            )

    def _pending(self, document: dict) -> bool:
        """Whether this declaration carries a block the database does not."""
        for group in document.get("view_group", []):
            if group["name"] not in self.held_views:
                return True
        for block in document.get("view", []):
            if block["name"] not in self.held_views:
                return True
        for block in document.get("layer", []):
            if block["name"] not in self.held_layers:
                return True
            labels = block.get("labels")
            if labels is not None and labels["name"] not in self.held_layers:
                return True
        for block in document.get("vocabulary", []):
            if block["name"] not in self.held_vocabularies:
                return True
        for block in document.get("attribute", []):
            if block["name"] not in self.held_attributes:
                return True
        return False

    # ------------------------------------------------------------------ 2. rows

    def _rows(self, document: dict) -> None:
        """An insert into a view or a group: a page of points, the allocation view first."""
        anchor = document.get("defaults", {}).get("allocation_view")
        inserts = self._for("view", "rows") + self._for("view_group", "rows")
        inserts.sort(key=lambda one: 0 if one.target == anchor else 1)
        for insert in inserts:
            block = _block(document, insert.kind, insert.target)
            table = insert.table()
            if insert.kind == "view":
                self._points_of(insert, block, table, insert.target)
                continue
            if insert.view_key is not None:
                # One view for the whole table: a group whose views each have their own file.
                self._points_of(insert, block, table, f"{insert.target}:{insert.view_key}")
                continue
            # A group's rows say which view each belongs to, so the table is split by that column
            # and each part is a page into the view it names. A key the group does not hold is a
            # 404 at the route, which is the refusal a mistyped key must be (views.md §3.2).
            column = table[insert.columns["view"]].to_pylist()
            for key in dict.fromkeys(value for value in column if value is not None):
                rows = [i for i, value in enumerate(column) if value == key]
                self._points_of(
                    insert,
                    block,
                    table.take(pa.array(rows, pa.int64())),
                    f"{insert.target}:{key}",
                )

    def _points_of(self, insert, block: dict, table: pa.Table, name: str) -> None:
        rows = list(range(table.num_rows))
        self._refuse_outside_the_frame(name, table, insert, rows)
        if not rows:
            return
        self._page_rows(
            kind="points",
            name=insert.source,
            view=name,
            table=_selected(table, rows, self._point_columns(insert, block, table)),
            limits=self.limits.get("ingest", {}),
            line=f"points into view '{name}'",
        )

    def _refuse_outside_the_frame(
        self, view: str, table: pa.Table, insert, rows: list[int]
    ) -> None:
        """List the rows outside the view's frame, which refuses the commit (§6.3).

        A frame is fixed at the first commit and the ingest route refuses a whole page carrying a
        row outside it. The rows stand as they were inserted: dropping them would commit a corpus
        the user did not insert, and the remedy is theirs: move the rows, or rebuild the database
        with an `extent=` that holds them.
        """
        frame = next(
            (v.get("quantisation") for v in self.meta.get("views", []) if v.get("id") == view),
            None,
        )
        if frame is None:
            return
        x_column = insert.columns.get("x") or insert.columns.get("lon")
        y_column = insert.columns.get("y") or insert.columns.get("lat")
        if x_column not in table.column_names or y_column not in table.column_names:
            return
        xs = table[x_column].to_pylist()
        ys = table[y_column].to_pylist()
        outside = [
            row
            for row in rows
            if xs[row] is not None
            and ys[row] is not None
            and not (
                frame["x_min"] <= xs[row] <= frame["x_max"]
                and frame["y_min"] <= ys[row] <= frame["y_max"]
            )
        ]
        if outside:
            self.findings.append(
                Finding(
                    "rows outside the frame",
                    f"{len(outside)} of {len(rows)} row(s) fall outside view '{view}''s frame "
                    f"[{frame['x_min']:g}, {frame['x_max']:g}] × "
                    f"[{frame['y_min']:g}, {frame['y_max']:g}], and the ingest route refuses a "
                    f"page carrying one. A frame is fixed at the first commit, so the remedy is "
                    f"the rows or a rebuild with an extent= that holds them",
                )
            )

    def _point_columns(self, insert, block: dict, table: pa.Table) -> list[tuple[str, Any]]:
        """The wire's columns for one points page: geometry, labels, the id, and the values.

        A column no target reads is not here: what travels is what the insert named, which is
        what the declaration says this view reads (§3).
        """
        x_column = insert.columns.get("x") or insert.columns.get("lon")
        y_column = insert.columns.get("y") or insert.columns.get("lat")
        columns: list[tuple[str, Any]] = [
            ("x", table[x_column].cast(pa.float64())),
            ("y", table[y_column].cast(pa.float64())),
        ]
        access = insert.columns.get("access")
        if access:
            columns.append(("access", _label_lists(table[access])))
        identity = insert.columns.get("id")
        if identity:
            columns.append(("external_id", _external_ids(table[identity])))
        for name, column in insert.named_attributes.items():
            columns.append((name, table[column]))
        return columns

    # ------------------------------------------------------------------ 3. values

    def _values(self, document: dict) -> None:
        """What `POST /control/values` carries: cells on rows the database holds (§6.2 step 3).

        An insert into an attribute fills that column's cells. An insert into a layer by key is a
        column named for the layer, which the route reads by `/control/ingest`'s own rule: a key
        an artifact holds joins the entity to it, and a key no artifact holds mints the artifact
        it names on a layer whose value set is `open` (contracts §3.4).

        A group-scoped family and a scoped layer are paged per view: the insert's `view=` column
        says which view each row's value belongs to, and the page carries that view in
        `x-tessera-view`, without which no family may be named.
        """
        for insert in self._for("attribute", "values"):
            block = _block(document, "attribute", insert.target)
            if block.get("render"):
                self.findings.append(
                    Finding(
                        "a rendered column on the values route",
                        f"attribute '{insert.target}' is declared `render`, and a rendered value "
                        f"is drawn from the hot column of the row that carries it. The values "
                        f"route fills entities and acquires no row, so it refuses one. Insert the "
                        f"rows that carry it into the view instead",
                    )
                )
                continue
            self._values_per_view(insert, _scoped_to(block), insert.columns["value"])
        for insert in self._for("layer", "key"):
            block = _block(document, "layer", insert.target)
            self._minted.add(insert.target)
            self._values_per_view(insert, _scoped_to(block), insert.columns["key"])

    def _values_per_view(self, insert, group: str | None, column: str) -> None:
        """One insert's pages, whole where it is entity-scoped and per view where it is not."""
        table = insert.table()
        if group is None:
            self._values_of(insert, table, list(range(table.num_rows)), None, column)
            return
        view_column = insert.columns.get("view")
        if view_column is None:
            self.findings.append(
                Finding(
                    "a scoped insert with no view column",
                    f"'{insert.target}' is scoped to group '{group}', and a value belongs to one "
                    f"view. The values route names the view in a header, so the insert names the "
                    f"column that says which: view=",
                )
            )
            return
        keys = table[view_column].to_pylist()
        for key in dict.fromkeys(value for value in keys if value is not None):
            rows = [i for i, value in enumerate(keys) if value == key]
            self._values_of(insert, table, rows, f"{group}:{key}", column)

    def _values_of(
        self, insert, table: pa.Table, rows: list[int], view: str | None, column: str
    ) -> None:
        """One page sequence of `POST /control/values`, over the rows this insert named."""
        if not rows:
            return
        columns = [
            ("external_id", _external_ids(table[insert.columns["id"]])),
            (insert.target, table[column]),
        ]
        where = "" if view is None else f" of view '{view}'"
        self._page_rows(
            kind="values",
            name=insert.source if view is None else f"{insert.source}-{view}",
            view=view,
            table=_selected(table, rows, columns),
            limits=self.limits.get("values", {}),
            line=f"values into '{insert.target}'{where}",
        )

    # ------------------------------------------------------------------ 2/3. paging

    def _page_rows(
        self,
        kind: str,
        name: str,
        view: str | None,
        table: pa.Table,
        limits: dict,
        line: str,
    ) -> None:
        """Slice a table into pages under both of the route's units and build each body.

        The row cap sizes a slice; a slice whose encoded body is over the byte cap is halved and
        each half encoded again, so the body that is measured is the body that is sent. A single
        row over the cap is sent as it is and the route's refusal is reported.
        """
        rows = int(limits.get("max_batch_rows", 10_000))
        cap = int(limits.get("max_batch_bytes", 16 << 20))
        for start in range(0, table.num_rows, rows):
            for first, body, count in _bodies(table.slice(start, rows), start, cap):
                self.pages.append(
                    Page(
                        kind=kind,
                        name=name,
                        view=view,
                        line=f"{line}: {count} row(s)",
                        body=body,
                        batch=batch_id(name, first, body),
                        rows=count,
                    )
                )

    # ------------------------------------------------------------------ 4. artifacts

    def _artifacts(self, document: dict) -> None:
        for block in _in_dependency_order(document.get("layer", [])):
            self._artifacts_of(block, "layer")
            labels = block.get("labels")
            if labels is not None:
                self._artifacts_of(labels, "labels", parent=block["name"])

    def _artifacts_of(self, block: dict, kind: str, parent: str | None = None) -> None:
        layer = block["name"]
        artifacts = self._for(kind, "artifacts", layer) + self._for(kind, "text", layer)
        members = self._for(kind, "members", layer)
        # An inline roster stays in the declaration, so it is offered once: with the layer it
        # belongs to, which this commit is declaring.
        inline = (block.get("artifacts") or []) if layer not in self.held_layers else []
        if not artifacts and not members and not inline:
            return
        if parent is not None and parent not in self.held_layers and not self._planned(parent):
            self.findings.append(
                Finding(
                    "a labels insert before its clustering",
                    f"'{layer}' labels '{parent}', which this commit neither holds nor inserts. "
                    f"Insert the clustering beside the labels: its artifacts and members, or its "
                    f"key column",
                )
            )
            return
        rows = _artifact_rows(artifacts, members, inline)
        if not rows:
            return
        self._published.add(layer)
        for row in rows:
            attached = row.get("attached")
            if attached:
                self._attached_to.add(attached["layer"])
        wkb = [row["key"] for row in rows if row.get("geometry_wkb")]
        if wkb:
            self.findings.append(
                Finding(
                    "a polygon written in WKB at the publication route",
                    f"layer '{layer}': {len(wkb)} artifact(s), the first '{wkb[0]}', carry a "
                    f"'geometry' column of WKB, which is what the build reads. The publication "
                    f"route takes a polygon as WKT text, so a layer published at a running "
                    f"service writes its geometry as text",
                )
            )
            return
        self._publish(block, rows)

    def _planned(self, layer: str) -> bool:
        """Whether this plan publishes a key into a layer (§6.3).

        A clustering this database declared at an earlier commit may hold any key, and the SDK does
        not enumerate the server's artifacts to find out: a label naming a key it does not hold is
        a `404` on the attachment, reported per artifact. What the pre-flight can decide is the
        case where the clustering is new in this commit, where a label can only attach to a key
        this commit inserts.
        """
        if layer in self._minted:
            # A key column this commit sends mints its artifacts at the values route, which is
            # the other door a clustering arrives by (contracts §3.4).
            return True
        return any(page.name == layer and page.kind == "publish" for page in self.pages)

    def _publish(self, block: dict, rows: list[dict]) -> None:
        """One layer's artifact rows, as publications (§6.2 step 4).

        Within a layer the levels go coarse first, a nested batch resolves parents that are its own
        siblings, and a key the level already holds falls under the route's fill rule: its members
        join, its absent fixed parts are filled and a differing one is a `409` the report carries
        (ingest §1.5). Content gated `all` travels on the publish record with the first page of its
        generating set, the route refusing a content fill on such a layer; further set pages are
        `PATCH` at the rank.
        """
        layer = block["name"]
        scoped = _scoped_to(block) is not None
        publish_limits = self.limits.get("publish", {})
        grow_limits = self.limits.get("grow", {})
        cap = int(publish_limits.get("max_body_bytes", 64 << 20))
        most = int(publish_limits.get("max_artifacts_per_request", 10_000))
        most_excluded = int(publish_limits.get("max_excluded_per_request", 1_000_000))

        carried: list[dict] = []
        for row in rows:
            excluding = row.get("excluding")
            if excluding is not None and len(excluding) > most_excluded:
                self.findings.append(
                    Finding(
                        "an exclusion list over the route's bound",
                        f"layer '{layer}', artifact '{row['key']}': the membership leaves out "
                        f"{len(excluding)} entities, over the {most_excluded} the exclusion "
                        f"spelling admits (limits.publish.max_excluded_per_request). What must "
                        f"fit one request is the list, the complement being taken against the "
                        f"view's entities on the executor. Name the members the artifact holds "
                        f"instead, which pages",
                    )
                )
                continue
            carried.append(row)

        for level in sorted({int(row.get("level") or 0) for row in carried}):
            at_level = [row for row in carried if int(row.get("level") or 0) == level]
            for batch in _batched(_parents_first(at_level), cap, most):
                blocks, artifacts, members = batch
                body = _publish_body(level, blocks)
                self._artifact_page(
                    "publish",
                    layer,
                    level,
                    f"publish {len(artifacts)} artifact(s) into '{layer}' level {level}",
                    body,
                    artifacts=len(artifacts),
                    members=members,
                )
                for row in artifacts:
                    if scoped and row.get("remainders"):
                        self.findings.append(
                            Finding(
                                "a scoped membership over one publication",
                                f"layer '{layer}', view '{row.get('view')}', artifact "
                                f"'{row['key']}': its membership does not fit one publication, and "
                                f"the growth route that pages the rest carries no view. Split the "
                                f"artifact into keys whose memberships fit",
                            )
                        )
                        continue
                    # The publication carries a first page of every set; what did not fit follows
                    # as growths.
                    for rank, remainder in row.get("remainders", []):
                        self._grow_pages(layer, level, row["key"], rank, remainder, grow_limits)

    def _artifact_page(
        self,
        kind: str,
        layer: str,
        level: int,
        line: str,
        body: bytes,
        artifacts: int = 0,
        members: int = 0,
    ) -> None:
        """One publication or growth. The artifact routes carry no batch-id header."""
        self.pages.append(
            Page(
                kind=kind,
                name=layer,
                level=level,
                line=line,
                body=body,
                artifacts=artifacts,
                members=members,
            )
        )

    def _grow_pages(
        self,
        layer: str,
        level: int,
        key: str,
        rank: int | None,
        members: Sequence[Any],
        grow_limits: dict,
    ) -> None:
        """The pages that join one set, in order."""
        if not members:
            return
        cap = int(grow_limits.get("max_body_bytes", 64 << 20))
        most = int(grow_limits.get("max_members_per_request", 5_000_000))
        per_page = max(1, min((cap - 512) // 16, most))
        for start in range(0, len(members), per_page):
            slice_ = list(members[start : start + per_page])
            body = patch_body(level, key, joining=slice_, rank=rank)
            what = "members" if rank is None else f"the generating set at rank {rank}"
            self._artifact_page(
                "grow",
                layer,
                level,
                f"join {len(slice_)} {what} of '{key}' in '{layer}'",
                body,
                members=len(slice_),
            )


# ---------------------------------------------------------------------------- helpers


def _external_ids(column) -> pa.Array:
    """One id column as the wire's `external_id`: the bytes each value holds (§3)."""
    return pa.array([_control.external_id(v) for v in column.to_pylist()], pa.binary())


def _selected(table: pa.Table, rows: list[int], columns: list[tuple[str, Any]]) -> pa.Table:
    """The named columns of the named rows, as the table a page is encoded from."""
    indices = pa.array(rows, pa.int64())
    arrays = []
    names = []
    for name, array in columns:
        chunked = array if isinstance(array, (pa.Array, pa.ChunkedArray)) else pa.array(array)
        taken = chunked.take(indices)
        arrays.append(taken.combine_chunks() if isinstance(taken, pa.ChunkedArray) else taken)
        names.append(name)
    return pa.table(arrays, names=names)


def _label_lists(column) -> pa.Array:
    """The access column as the wire's list of labels, one element per label (decision 0129).

    A list column travels as itself, a scalar column as one-element lists, and a null as the empty
    list, which the view's declaration decides: a `point_visibility.default` gives the row that
    label, and a view declaring none refuses the batch, in the terms the build refuses the same
    corpus (decision 0133).
    """
    values = column.to_pylist()
    if pa.types.is_list(column.type) or pa.types.is_large_list(column.type):
        return pa.array([[] if v is None else list(v) for v in values], pa.list_(pa.string()))
    return pa.array([[] if v is None else [str(v)] for v in values], pa.list_(pa.string()))


def _bodies(table: pa.Table, start: int, cap: int):
    """Yield `(first row, body, rows)` for a slice, every body under the byte cap.

    A body over the cap is not sent: the slice is halved and each half encoded again, so the pieces
    come out in row order and each carries the index of its first row. A single row over the cap
    cannot be split and is sent as it is, the route's refusal being what says so.
    """
    pending = [(start, table)]
    while pending:
        first, piece = pending.pop()
        body = arrow_body(piece)
        if len(body) > cap and piece.num_rows > 1:
            half = piece.num_rows // 2
            pending.append((first + half, piece.slice(half)))
            pending.append((first, piece.slice(0, half)))
            continue
        yield first, body, piece.num_rows


def _value_pages(values: list[dict], cap: int) -> list[list[dict]]:
    """A value set sliced into pages under both of the route's units (§6.2 step 1).

    The row figure sizes a slice and the byte cap decides it: a slice whose encoded body is over
    the cap is halved and each half encoded again, so the body that is measured is the body that
    is sent. A single value over the cap is sent as it is and the route's refusal is what says so.
    """
    pages: list[list[dict]] = []
    for start in range(0, len(values), VALUES_PER_PAGE):
        pending = [values[start : start + VALUES_PER_PAGE]]
        while pending:
            piece = pending.pop()
            if len(json.dumps({"values": piece}).encode()) > cap and len(piece) > 1:
                half = len(piece) // 2
                pending.append(piece[half:])
                pending.append(piece[:half])
                continue
            pages.append(piece)
    return pages


def _in_dependency_order(layers: Sequence[dict]) -> list[dict]:
    """Declaration order, with a layer after everything it depends on (§6.2 step 4)."""
    by_name = {block["name"]: block for block in layers}
    ordered: list[dict] = []
    placed: set[str] = set()

    def place(block: dict, walking: set[str]) -> None:
        name = block["name"]
        if name in placed or name in walking:
            return
        walking.add(name)
        for other in block.get("depends_on", []) or []:
            if other in by_name:
                place(by_name[other], walking)
        walking.discard(name)
        placed.add(name)
        ordered.append(block)

    for block in layers:
        place(block, set())
    return ordered


def _parents_first(rows: list[dict]) -> list[dict]:
    """A nested batch resolves parents that are its own siblings, so a parent goes first."""
    by_key = {row["key"]: row for row in rows}
    ordered: list[dict] = []
    placed: set[str] = set()

    def place(row: dict, walking: set[str]) -> None:
        key = row["key"]
        if key in placed or key in walking:
            return
        walking.add(key)
        for parent in row.get("parent") or []:
            if parent in by_key:
                place(by_key[parent], walking)
        walking.discard(key)
        placed.add(key)
        ordered.append(row)

    for row in rows:
        place(row, set())
    return ordered


def _artifact_block(row: dict, budget: int) -> tuple[bytes, list, int]:
    """One artifact's JSON, with as many members and set entries as the budget holds.

    Returns the block, the `(rank, members)` remainders that did not fit, and how many members the
    block carries. A remainder is sent as a `PATCH` after the publication: members join the
    membership, and a set page at the rank pages that content's generating set.
    """
    remainders: list[tuple[int | None, list]] = []
    record: dict[str, Any] = {"key": row["key"]}
    if row.get("view") is not None:
        # Part of the artifact's identity on a layer whose scope names a group, and no later
        # record fills it (contracts §3.4 r84).
        record["view"] = row["view"]
    if row.get("parent"):
        record["parent"] = list(row["parent"])
    if row.get("attached"):
        record["attached_to"] = row["attached"]
    for shape_field in SHAPE_FIELDS:
        if row.get(shape_field) is not None:
            record[shape_field] = row[shape_field]
    if row.get("space") is not None:
        record["space"] = row["space"]
    members = list(row.get("members", []))
    sets = [list(one) for one in row.get("sets", [])]
    contents = row.get("content") or []
    if contents:
        record["content"] = [
            {
                "values": list(values),
                "generated_from": [addressed(e) for e in (sets[rank] if rank < len(sets) else [])],
            }
            for rank, values in enumerate(contents)
        ]
    if row.get("excluding") is not None:
        # A membership spelled by exclusion travels whole: the executor complements the list
        # against the view's entities as of that step, so a second page would name a different
        # set (ingest §2.3). The route's count bound is checked before the plan is built.
        record["excluding"] = [addressed(e) for e in row["excluding"]]
    elif not members and record.get("attached_to"):
        # **A memberless label carries no `members` at all** (decision 0145): an attached artifact
        # with no members of its own is served over its target's membership, so the field is
        # omitted rather than sent empty. An empty list would say the same thing today, and saying
        # nothing is what the mapping form — a cluster key to a line of text — actually means.
        pass
    else:
        # A record carrying neither `members` nor `excluding` and attaching to nothing is a `422`,
        # and the route makes no exception for a shape: "an artifact whose membership holds nobody
        # is published with an empty `members` list" (contracts §3.4). So a spatial record carries
        # the empty list beside its shape, which the shape's own resolution then supersedes.
        record["members"] = [addressed(e) for e in members]
    body = json.dumps(record).encode()
    # A membership spelled by exclusion travels whole, so nothing of it is trimmed; every other
    # record pages its membership and then its generating sets, a memberless attached one included
    # — it has sets to page even with no `members` field of its own.
    while len(body) > budget and row.get("excluding") is None and (members or any(sets)):
        # Trim the membership first, then each generating set from the last rank down: the page
        # that follows carries the rest, and a set's own page is a `PATCH` at its rank.
        if len(members) > 1:
            keep = len(members) // 2
            remainders.append((None, members[keep:]))
            members = members[:keep]
            record["members"] = [addressed(e) for e in members]
        elif any(len(one) > 1 for one in sets):
            rank = max(i for i, one in enumerate(sets) if len(one) > 1)
            keep = len(sets[rank]) // 2
            remainders.append((rank, sets[rank][keep:]))
            sets[rank] = sets[rank][:keep]
            record["content"][rank]["generated_from"] = [addressed(e) for e in sets[rank]]
        else:
            break
        body = json.dumps(record).encode()
    return body, remainders, len(members)


def _batched(rows: list[dict], cap: int, most: int):
    """Pack one level's artifact records into batches under the route's two units."""
    budget = max(1024, cap - 256)
    batch: list[bytes] = []
    carried: list[dict] = []
    size = 0
    members = 0
    for row in rows:
        one, remainders, count = _artifact_block(row, budget // 2)
        row["remainders"] = remainders
        if batch and (size + len(one) + 1 > budget or len(batch) >= most):
            yield batch, carried, members
            batch, carried, size, members = [], [], 0, 0
        batch.append(one)
        carried.append(row)
        size += len(one) + 1
        members += count
    if batch:
        yield batch, carried, members


def patch_body(
    level: int,
    key: str,
    joining: Sequence[Any] = (),
    leaving: Sequence[Any] = (),
    rank: int | None = None,
) -> bytes:
    """One `PATCH` row: the set this page moves, and the members joining or leaving it.

    `rank` absent names the membership and present names the generating set of the content at that
    rank (ingest §1.1). Only a generating set may shrink, so `leaving` without a rank is a refusal
    the route makes and this function does not pre-empt.
    """
    row: dict[str, Any] = {"key": key}
    if rank is not None:
        row["rank"] = rank
    if joining:
        row["members"] = [addressed(i) for i in joining]
    if leaving:
        row["leaving"] = [addressed(i) for i in leaving]
    return json.dumps({"level": level, "addressing": "external", "artifacts": [row]}).encode()


def _publish_body(level: int, blocks: list[bytes]) -> bytes:
    return (
        b'{"level":'
        + str(level).encode()
        + b',"addressing":"external","artifacts":['
        + b",".join(blocks)
        + b"]}"
    )


def _flush(what: str, why: str) -> Page:
    """One `POST /control/flush?wait=visible` inside a commit, where a step needs the last one."""
    return Page(kind="flush", name=what, line=f"flush {what}, and wait for the publication that {why}")


def _blank(key: str, level: int, view: str | None = None) -> dict:
    return {"key": key, "level": level, "view": view, "members": [], "sets": [], "content": [],
            "parent": [], "attached": None, "excluding": None, "space": None}


def _artifact_rows(artifacts, members, inline=None) -> list[dict]:
    """One layer's inserted tables as artifact records: the key, its parts and its sets.

    A member table's grain is `(key, entity, rank)`: a null rank is the membership and rank *k* is
    content *k*'s generating set (annotation-write-cycle §6.1). An artifacts table's `contents` is
    one value list per rank, positional over the kinds the layer declares. A shape column, `space`
    and `excluding` are columns of the artifact row, and the publication record carries each as
    the row wrote it (contracts §3.4). Every column is the one the insert named, or the artifact
    table's own name for it where the insert renamed nothing (§3).
    """
    rows: dict[tuple[int, str, str | None], dict] = {}
    for insert in artifacts or []:
        for record in _records(insert.table(), insert):
            level = int(record.get("level") or 0)
            key = str(record["key"])
            # On a layer scoped to a group the key is unique per view, one key in two views being
            # two artifacts (contracts §3.4 r84), so the view is part of the row's identity here.
            view = record.get("view")
            row = rows.setdefault((level, key, view), _blank(key, level, view))
            _artifact_parts(row, record)
    for record in inline or []:
        record = dict(record)
        level = int(record.get("level") or 0)
        key = str(record["key"])
        row = rows.setdefault((level, key, None), _blank(key, level, None))
        _artifact_parts(row, record)
    for insert in members or []:
        table = insert.table()
        columns = insert.columns
        entity_column = columns["id"]
        key_column = columns["key"]
        level_column = columns.get("level")
        rank_column = columns.get("rank")
        view_column = columns.get("view")
        levels = table[level_column].to_pylist() if level_column else [0] * table.num_rows
        ranks = table[rank_column].to_pylist() if rank_column else [None] * table.num_rows
        views = table[view_column].to_pylist() if view_column else [None] * table.num_rows
        keys = table[key_column].to_pylist()
        entities = table[entity_column].to_pylist()
        for level, key, rank, entity, view in zip(levels, keys, ranks, entities, views):
            level = int(level or 0)
            record = rows.setdefault((level, str(key), view), _blank(str(key), level, view))
            if rank is None:
                record["members"].append(entity)
            else:
                rank = int(rank)
                while len(record["sets"]) <= rank:
                    record["sets"].append([])
                record["sets"][rank].append(entity)
    return list(rows.values())


#: How a shape written in a table's own columns reaches the publication record, whose fields are
#: the record's own (contracts §3.4): `bbox = [min_x, min_y, max_x, max_y]`, `circle = [cx, cy, r]`,
#: `ellipse = [cx, cy, a, b, angle]`. A polygon's `geometry` column is WKB at the build and the
#: record's `wkt` is text, so the two doors read one column two ways and the plan says so.
SHAPE_RECORD = {
    "bbox": ("min_x", "min_y", "max_x", "max_y"),
    "circle": ("cx", "cy", "r"),
    "ellipse": ("cx", "cy", "a", "b", "angle"),
}


def _shape_of(row: dict, record: dict) -> None:
    """The publication record's shape, from the columns the table wrote it in."""
    for field, columns in SHAPE_RECORD.items():
        if all(record.get(column) is not None for column in columns):
            row[field] = [float(record[column]) for column in columns]
    geometry = record.get("geometry")
    if isinstance(geometry, str):
        row["wkt"] = geometry
    elif geometry is not None:
        # WKB, which the build reads and the publication route does not take.
        row["geometry_wkb"] = True


def _artifact_parts(row: dict, record: dict) -> None:
    """The parts one artifact row carries, folded onto the record being built."""
    _shape_of(row, record)
    contents = record.get("contents")
    if contents:
        row["content"] = [list(values) for values in contents]
    parent = record.get("parent")
    if parent is not None:
        row["parent"] = [parent] if isinstance(parent, str) else list(parent)
    target = record.get("attached_layer")
    key_of = record.get("attached_key")
    if target and key_of:
        attached = {"layer": target, "key": key_of}
        at = record.get("attached_level")
        if at is not None:
            attached["level"] = int(at)
        row["attached"] = attached
    for shape_field in SHAPE_FIELDS:
        if record.get(shape_field) is not None:
            row[shape_field] = record[shape_field]
    if record.get("space") is not None:
        row["space"] = record["space"]
    if record.get("excluding") is not None:
        row["excluding"] = list(record["excluding"])
    if record.get("members") is not None:
        row["members"] = list(record["members"])


def _records(table: pa.Table, insert) -> list[dict]:
    """One table's rows under the artifact table's own names, as the insert named its columns.

    What is read is what the call named, and the shape columns `shape=` named, and nothing else:
    a canonical column the call passed over is refused at the verb (§3), so the pages this plan
    sends carry exactly what the build would have read from the same table.
    """
    named = {column: role for role, column in insert.columns.items()}
    named.update({column: role for role, column in insert.metadata_columns.items()})
    for column in insert.shape_columns:
        named.setdefault(column, column)
    out = []
    for row in rows_of(table):
        record = {name: row[column] for column, name in named.items() if column in row}
        out.append(record)
    return out


# ---------------------------------------------------------------------------- the run


def run(control: Control, pages: Sequence[Page], report) -> None:
    """Send the plan, in order, then flush once and wait for the publication that flush arms.

    Every page goes unwaited and the flush is the commit's whole wait, whatever the pages did: a
    commit that reported a refusal on one page should not also leave its accepted ones sitting for
    the executor's own period. Where nothing was accepted there is nothing to publish and no flush
    is sent.
    """
    accepted = 0
    for page in pages:
        answer = _send(control, page)
        _fold(report, page, answer)
        if answer.ok:
            accepted += 1
    if accepted:
        _waited(report, control.flush(wait=True))


def _send(control: Control, page: Page) -> Answer:
    if page.kind == "layer":
        return control.declare_layer(page.body)
    if page.kind == "view_group":
        return control.declare_view_group(page.name, page.body)
    if page.kind == "view":
        return control.declare_view(page.name, page.body)
    if page.kind == "attribute":
        return control.declare_attribute(page.body)
    if page.kind == "vocabulary":
        return control.declare_vocabulary(page.name, page.body)
    if page.kind == "vocabulary_values":
        return control.vocabulary_values(page.name, page.body)
    if page.kind == "group_view":
        group, _, key = page.name.partition(":")
        return control.create_view(group, key, page.body)
    if page.kind == "flush":
        return control.flush(wait=True)
    if page.kind == "points":
        return control.ingest(page.body, page.batch, page.view)
    if page.kind == "values":
        return control.values(page.body, page.batch, page.view)
    if page.kind == "publish":
        return control.publish(page.name, page.body)
    if page.kind == "grow":
        return control.grow(page.name, page.body)
    raise Refusal(f"commit: no route for a page of kind {page.kind!r}")


def _fold(report, page: Page, answer: Answer) -> None:
    # A page's acknowledgement names the cycle its own work publishes in (decision 0144), and the
    # report does not print it: the closing flush's number is the one every page is visible at.
    if not answer.ok:
        report.refusals.append(
            {
                "what": page.line,
                "layer_or_view": page.view or page.name,
                "status": answer.status,
                "detail": _detail(answer),
            }
        )
        return
    body = answer.body
    if body.get("replayed"):
        # The same bytes under the same batch id: the server answered the first attempt's receipt
        # and applied nothing (contracts §3.4). Acceptance is an effect and a replay has none.
        report.replayed.append(page.line)
        return
    if page.kind == "points":
        view = page.view or ""
        report.rows_accepted[view] = report.rows_accepted.get(view, 0) + int(
            body.get("accepted", 0)
        )
        report.artifacts_minted += int(body.get("minted", 0))
        report.tessera_ids += [str(i) for i in body.get("tessera_ids", [])]
        report.clipped += int(body.get("clipped", 0))
    elif page.kind == "values":
        report.values_filled += int(body.get("filled", 0))
        report.already_present += int(body.get("held", 0))
        # A values page carrying a layer's key column mints artifacts from it as the other two
        # doors do, and `joined` counts the members that joined artifacts the database held.
        report.artifacts_minted += int(body.get("minted", 0))
        report.memberships_joined += int(body.get("joined", 0))
    elif page.kind == "publish":
        report.artifacts_minted += int(body.get("created", 0))
        report.already_present += int(body.get("filled", 0))
        report.memberships_joined += int(body.get("joined", 0))
        report.without_content += int(body.get("without_content", 0))
        minted = report.artifact_ids.setdefault(page.name, {})
        for one in body.get("artifacts", []):
            minted[one["key"]] = str(one["tessera_id"])
    elif page.kind in ("attribute", "vocabulary"):
        # `existing: true` is the held-part arm of the fill rule: the name is there under this
        # identity and the request applied nothing but its values (contracts §3.4).
        report.already_present += 1 if body.get("existing") else 0
        report.values_bound += int(body.get("added", 0))
        report.titles_set += int(body.get("titles", 0))
    elif page.kind == "vocabulary_values":
        # Here `existing` is a count: the keys of this page the value set already bound.
        report.already_present += int(body.get("existing", 0))
        report.values_bound += int(body.get("added", 0))
        report.titles_set += int(body.get("titles", 0))
    elif page.kind == "grow":
        for one in body.get("artifacts", []):
            report.memberships_joined += int(one.get("joined", 0))
            report.already_present += int(one.get("filled", 0))


def _detail(answer: Answer) -> str:
    body = answer.body
    if isinstance(body, dict) and body.get("detail"):
        return str(body["detail"])
    return answer.detail.strip()[:1000]


def _waited(report, answer: Answer) -> None:
    """What the closing flush says about the commit's visibility (§6.2 step 5).

    `visible: true` is the wait: the publication the flush armed has completed, so the next cell
    reads what this commit wrote. `visible: false` is the server's bound reached, which is a
    finding rather than a refusal — the write is durable and publishes at the next cycle.
    """
    if not answer.ok:
        # The pages landed and are durable; what failed is the request that would have published
        # them, so this is a finding on visibility and not a refusal of the commit.
        report.flush_reached = False
        report.findings.append(
            Finding(
                "the closing flush was refused",
                f"POST /control/flush?wait=visible answered {answer.status}: "
                f"{_detail(answer)}. Every page this commit sent was acknowledged and is durable; "
                f"what it wrote reaches the served forms at the executor's next cycle",
            )
        )
        return
    report.flush_wait = answer.seconds
    if answer.body.get("publication") is not None:
        report.publication = int(answer.body["publication"])
    if answer.body.get("visible"):
        return
    report.flush_reached = False
    report.findings.append(
        Finding(
            "the publication did not arrive within the server's wait",
            f"publication {answer.body.get('publication')} had not completed when the flush's "
            f"wait ran out (serve.visible_wait_max_secs). Every page this commit sent was "
            f"acknowledged and is durable; what it wrote reaches the served forms at the "
            f"executor's next cycle",
        )
    )


def changes(control: Control, items: Sequence[dict], op: str, limits: dict) -> list[Answer]:
    """`POST /control/changes` for one op, paged under the route's two units (§6.5)."""
    rows = [{**item, "op": op} for item in items]
    per_page = int(limits.get("changes", {}).get("max_changes_per_request", 10_000))
    answers = []
    for start in range(0, len(rows), per_page):
        answers.append(control.changes(rows[start : start + per_page]))
    return answers


def leave(control: Control, layer: str, key: str, ids: Sequence[Any], rank: int,
          level: int = 0) -> Answer:
    """`PATCH` a generating set at a rank, the one set that may shrink (decision 0135, §6.5)."""
    return control.grow(layer, patch_body(level, key, leaving=ids, rank=rank))
