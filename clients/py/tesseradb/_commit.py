"""A later commit: the plan, the pre-flight, and the pages that carry it (python-sdk.md §6).

The first commit builds. Every commit after it pages the staged deltas through the control plane,
in the order §6.2 fixes: declarations, points per view, values on existing entities, artifacts per
layer, then a flush that waits for the publication it arms. A part supplied twice
is accepted and a part supplied differently is a `409` on that part (ingest §1.1), so the order
matters for existence and for nothing else.

`check()` runs the planner and the pre-flight and sends nothing; `commit()` runs the same plan. The
two therefore cannot disagree about what would be sent.

**The plan is built from what was staged and what the database says it holds.** The SDK keeps no
record of what it sent: a delta goes as it was staged, and a row the database already holds is a
`409` on that page which the report carries. What the database has already been told is read from
`/v1/meta` rather than from a log: its views, its groups and its layers (§3, §6.4). A re-run of a
cell is a re-run.

**The declaration decides which route a delta takes.** A delta on a view's points source carrying
both of that view's coordinate columns is a page of points; one carrying neither fills values on
entities the database already holds. One carrying exactly one of them is refused naming both: a
row with half a position is not a point, and sending it as values would drop the coordinate it
did carry.

**Nothing is dropped or rewritten.** Every column of a delta is sent or the commit is refused
naming what could not be, and a finding stops the plan rather than trimming it (§6.3). The user
corrects the data or the declaration and commits again.

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

import datetime
import json
from dataclasses import dataclass
from typing import Any, Sequence

import pyarrow as pa
import pyarrow.parquet as pq

from . import _control
from ._control import Answer, Control, addressed, arrow_body, batch_id
from ._declaration import SHAPE_FIELDS, rows_of, view_entries
from ._refusal import Refusal

@dataclass
class Finding:
    """One pre-flight finding (§6.3).

    A finding refuses the commit. The pre-flight reports and sends nothing while one stands, and
    it never drops a row or a column to make the rest sendable: what the user staged is what a
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


def _scoped_to(block: dict) -> str | None:
    """The view group a block's `scope` names, or `None` where it is entity-scoped."""
    scope = block.get("scope")
    return scope.get("group") if isinstance(scope, dict) else None


def _groups_of(view: dict) -> set[str]:
    """The groups whose scoped families a batch into this view may name (decision 0116).

    The address of a scoped value is `(attribute → its group, key)` and never the view, so a view
    of a group that shares another's keys writes the owner's cells through its own door.
    """
    if view["group"] is None:
        return set()
    return {view["group"], view["block"].get("members")} - {None}


def _from_columns(document: dict) -> dict[str, str]:
    """Each layer whose membership is a column of a points file, and the column (§4.6).

    The declaration is what says so: such a layer's `[layer.members]` reads the points source and
    names the column as `key`. The column travels on a points page under the layer's own name, and
    the window close mints the artifact it names (contracts §3.4).
    """
    points = {entry["source"] for entry in view_entries(document, None)}
    columns: dict[str, str] = {}
    for block in document.get("layer", []):
        members = block.get("members")
        if not isinstance(members, dict) or members.get("source") not in points:
            continue
        key = dict(members.get("fields", {})).get("key")
        if key:
            columns[block["name"]] = key
    return columns


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


def _roster_body(group: dict, record: dict) -> dict:
    """One `PUT /control/views/{group}/{key}` body: the roster record as a request.

    Every metadata name the group declared is here and typed against it (contracts §3.4 r55); a
    `timestamp_us` travels as microseconds since the epoch, JSON carrying no date type.
    """
    body: dict[str, Any] = {}
    if record.get("visibility") is not None:
        body["visibility"] = record["visibility"]
    metadata = {
        name: _metadata_value(value)
        for name, value in record.items()
        if name not in ("key", "source", "visibility")
    }
    body["metadata"] = metadata
    return body


def _metadata_value(value: Any) -> Any:
    if isinstance(value, datetime.datetime):
        moment = value if value.tzinfo else value.replace(tzinfo=datetime.timezone.utc)
        return int(moment.timestamp() * 1_000_000)
    return value


# ---------------------------------------------------------------------------- the pre-flight


class Planner:
    """§6.2's plan and §6.3's pre-flight over one database's staged deltas."""

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
        #: The sources this plan sends as points, so the values step leaves their columns alone.
        self._as_points: set[str] = set()

    # ------------------------------------------------------------------ entry

    def plan(self) -> tuple[list[Page], list[Finding]]:
        document = self.db._document()
        self._refuse_unknown_columns(document)
        self._declarations(document)
        self._points(document)
        self._values(document)
        self._artifacts(document)
        return self.pages, self.findings

    # ------------------------------------------------------------------ 1. declarations

    def _declarations(self, document: dict) -> None:
        """The runtime `PUT`s for every block this database does not carry yet (§6.2 step 1).

        The bodies come from `tessera check --payloads` over the SDK's own declaration, so the
        mapping from a block to a request body is the binary's and not a second one in Python. The
        one body it does not carry is a group's roster record, which is a request rather than a
        declaration (views.md §3.2): the SDK holds the record and sends it here.

        Which of them are new is read from `/v1/meta`: the database is what knows what it holds.
        The order is the one the routes need: a group before a view of it, and a view before the
        layer drawn on it. Not built yet: an attribute and a vocabulary declared after the first
        commit, refused at the verb (§6.2 step 1).
        """
        if not self._pending(document):
            return
        payloads = self.db._payloads()
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
        for group in document.get("view_group", []):
            for record in group.get("view") or []:
                view = f"{group['name']}:{record['key']}"
                if view in self.held_views:
                    continue
                self.pages.append(
                    Page(
                        kind="group_view",
                        name=view,
                        view=view,
                        line=f"create view '{view}' of group '{group['name']}'",
                        body=_roster_body(group, record),
                    )
                )
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

    def _pending(self, document: dict) -> bool:
        """Whether this declaration carries a block the database does not."""
        for group in document.get("view_group", []):
            if group["name"] not in self.held_views:
                return True
            for record in group.get("view") or []:
                if f"{group['name']}:{record['key']}" not in self.held_views:
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
        return False

    # ------------------------------------------------------------------ 2. points

    def _points(self, document: dict) -> None:
        for view in self._views_in_order(document):
            source = view["source"]
            delta = self.db.deltas.get(source)
            if delta is None:
                continue
            carried = [c for c in (view["x"], view["y"]) if c in delta.columns]
            if len(carried) == 1:
                self.findings.append(
                    Finding(
                        "a delta carrying one of a view's two coordinate columns",
                        f"'{source}' stages '{carried[0]}' and not "
                        f"'{view['x'] if carried[0] == view['y'] else view['y']}', and view "
                        f"'{view['id'] or view['group']}' reads its positions from both. A row "
                        f"with half a position is not a point, and a position is not a value the "
                        f"values route can fill. Stage both columns, or neither",
                    )
                )
                continue
            if not carried:
                # A row with no coordinates is not a point. The delta fills values on entities the
                # database already holds, and step 3 carries it.
                continue
            self._as_points.add(source)
            table = pq.read_table(delta.path)
            if view["discriminator"] is None:
                self._points_of(document, view, source, table, view["id"])
                continue
            # A group whose views are its source's own distinct values: the rows say which view
            # each belongs to, so the delta is split by that column and each part is a page into
            # the view it names. A key the group does not hold is a 404 at the route, which is the
            # refusal a mistyped key must be (views.md §3.2).
            column = table[view["discriminator"]].to_pylist()
            for key in dict.fromkeys(value for value in column if value is not None):
                rows = [i for i, value in enumerate(column) if value == key]
                self._points_of(
                    document,
                    view,
                    source,
                    table.take(pa.array(rows, pa.int64())),
                    f"{view['group']}:{key}",
                )

    def _views_in_order(self, document: dict) -> list[dict]:
        """The allocation view first, then the rest in declaration order (§6.2 step 2)."""
        views = view_entries(document, self.db.default_source)
        anchor = document.get("defaults", {}).get("allocation_view")
        views.sort(key=lambda entry: 0 if entry["id"] == anchor else 1)
        return views

    def _points_of(
        self, document: dict, view: dict, source: str, table: pa.Table, name: str
    ) -> None:
        rows = list(range(table.num_rows))
        self._refuse_outside_the_frame(name, table, view, rows)
        if not rows:
            return
        columns = self._point_columns(document, view, source, table)
        self._page_rows(
            kind="points",
            name=source,
            view=name,
            table=_selected(table, rows, columns),
            limits=self.limits.get("ingest", {}),
            line=f"points into view '{name}' from '{source}'",
        )

    def _refuse_outside_the_frame(
        self, view: str, table: pa.Table, entry: dict, rows: list[int]
    ) -> None:
        """List the rows outside the view's frame, which refuses the commit (§6.3).

        A frame is fixed at the first commit and the ingest route refuses a whole page carrying a
        row outside it. The rows stand as they were staged: dropping them would commit a corpus
        the user did not stage, and the remedy is theirs — move the rows, or rebuild the database
        with an `extent=` that holds them.
        """
        frame = next(
            (v.get("quantisation") for v in self.meta.get("views", []) if v.get("id") == view),
            None,
        )
        if frame is None:
            return
        x_column, y_column = entry["x"], entry["y"]
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

    def _point_columns(
        self, document: dict, view: dict, source: str, table: pa.Table
    ) -> list[tuple[str, pa.Array]]:
        """The wire's columns for one points page: geometry, labels, the id, values, layer keys.

        A group-scoped family reading its group's views' own points files travels here under its
        plain name: the view is known from `x-tessera-view`, so the column is not qualified
        (views.md §5).
        """
        columns: list[tuple[str, Any]] = [
            ("x", table[view["x"]].cast(pa.float64())),
            ("y", table[view["y"]].cast(pa.float64())),
        ]
        access = view["point_visibility"].get("field")
        if access and access in table.column_names:
            columns.append(("access", _label_lists(table[access])))
        identity = self.db._id_column_of(source)
        if identity and identity in table.column_names:
            columns.append(("external_id", _external_ids(table[identity])))
        for block in document.get("attribute", []):
            group = _scoped_to(block)
            if group is None:
                if (block.get("source") or self.db.default_source) != source:
                    continue
            elif block.get("source") or group not in _groups_of(view):
                # A family with a source of its own is read through that source's own view column
                # and goes to the values route; one scoped to another group has no value here.
                continue
            column = block.get("field") or block["name"]
            if column in table.column_names:
                columns.append((block["name"], table[column]))
        for layer, column in _from_columns(document).items():
            if column in table.column_names and self._layer_draws(document, layer, view):
                columns.append((layer, table[column]))
        return columns

    def _layer_draws(self, document: dict, layer: str, view: dict) -> bool:
        """Whether a layer is drawn on one view: by its own id, or by its group's name."""
        named = {view["id"], view["group"]} - {None}
        for block in document.get("layer", []):
            if block.get("name") == layer:
                views = block.get("views")
                return not isinstance(views, list) or bool(named & set(views))
        return False

    # ------------------------------------------------------------------ 3. values

    def _values(self, document: dict) -> None:
        """A delta that fills columns on entities the database already holds (§6.2 step 3).

        Every staged delta an attribute reads comes here, except one this plan is already sending
        as points: a points page carries its own attribute columns, so a values page beside it
        would write the same cells twice and, for a row this commit creates, fill an entity the
        bundle does not hold yet.

        A group-scoped family reading a source of its own is paged per view: its `fields.view`
        column says which view each row's value belongs to, and the page carries that view in
        `x-tessera-view`, without which no family may be named (contracts §3.4).
        """
        for source, delta in self.db.deltas.items():
            if source in self._as_points:
                continue
            attributes = [
                block
                for block in document.get("attribute", [])
                if (block.get("source") or self.db.default_source) == source
            ]
            if not attributes:
                continue
            table = pq.read_table(delta.path)
            rendered = [
                block["name"]
                for block in attributes
                if block.get("render") and (block.get("field") or block["name"]) in delta.columns
            ]
            if rendered:
                self.findings.append(
                    Finding(
                        "a rendered column on the values route",
                        f"'{source}' stages {', '.join(rendered)}, declared `render`, and a "
                        f"rendered value is drawn from the hot column of the row that carries it. "
                        f"The values route fills entities and acquires no row, so it refuses one, "
                        f"and the rest of this delta is not sent without it. Drop the column from "
                        f"the frame, or stage the rows that carry it as points",
                    )
                )
                continue
            unread = self._unread_by_values(document, source, delta, attributes)
            if unread:
                self.findings.append(
                    Finding(
                        "a column the values route has nowhere to put",
                        f"'{source}' stages {', '.join(repr(c) for c in unread)}, which no "
                        f"attribute of this declaration reads. The values route fills declared "
                        f"columns on entities that exist, so a column it does not know would be "
                        f"dropped from the page. Drop it from the frame, or declare what reads it",
                    )
                )
                continue
            fillable = [block for block in attributes if not block.get("render")]
            entity = [block for block in fillable if _scoped_to(block) is None]
            self._values_of(source, table, entity, list(range(table.num_rows)), None)
            for group in dict.fromkeys(
                _scoped_to(block) for block in fillable if _scoped_to(block) is not None
            ):
                family = [block for block in fillable if _scoped_to(block) == group]
                # Each column says for itself where its view is: two families on one source may
                # name different discriminators, `fields.view` being the column's own key.
                by_column: dict[str, list[dict]] = {}
                for one in family:
                    column = dict(one.get("fields", {})).get("view") or "view"
                    if column not in table.column_names:
                        self.findings.append(
                            Finding(
                                "a scoped family with no view column",
                                f"'{source}' carries {one['name']!r}, scoped to group '{group}', "
                                f"and a value belongs to one view. The values route names the view "
                                f"in a header, so the delta carries column '{column}', which the "
                                f"declaration's fields.view names and this delta does not",
                            )
                        )
                        continue
                    by_column.setdefault(column, []).append(one)
                for column, columns in by_column.items():
                    keys = table[column].to_pylist()
                    for key in dict.fromkeys(value for value in keys if value is not None):
                        rows = [i for i, value in enumerate(keys) if value == key]
                        self._values_of(source, table, columns, rows, f"{group}:{key}")

    def _unread_by_values(
        self, document: dict, source: str, delta, attributes: list[dict]
    ) -> list[str]:
        """The delta's columns the values step would not send (§6.3).

        A points source's delta that took this route carries no coordinates, so anything beyond
        the id column and the attributes' own columns is a column the route has no place for: a
        layer's key column is the case that matters, since minting an artifact from a column is
        the build's and the ingest route's, and this one fills cells. A source that is a layer's
        own table as well as an attribute source keeps the artifact table's columns.
        """
        read = {self.db._id_column_of(source)}
        for block in attributes:
            read.add(block.get("field") or block["name"])
            if _scoped_to(block) is not None:
                read.add(dict(block.get("fields", {})).get("view") or "view")
        points = {entry["source"] for entry in view_entries(document, self.db.default_source)}
        if source not in points:
            for block in document.get("layer", []):
                for one in (block, block.get("labels")):
                    if not isinstance(one, dict):
                        continue
                    members = one.get("members") if isinstance(one.get("members"), dict) else {}
                    if source in (one.get("source"), members.get("source")):
                        read |= self.ARTIFACT_COLUMNS
                        read |= set(dict(one.get("fields", {})).values())
                        read |= set(dict(members.get("fields", {})).values())
        return [column for column in delta.columns if column not in read]

    def _values_of(
        self, source: str, table: pa.Table, attributes: list[dict], rows: list[int], view: str | None
    ) -> None:
        """One page sequence of `POST /control/values`, over the rows and columns named."""
        if not attributes or not rows:
            return
        identity = self.db._id_column_of(source)
        if identity is None or identity not in table.column_names:
            self.findings.append(
                Finding(
                    "a values delta that names no entity",
                    f"'{source}' fills columns on entities this database holds, and a value is "
                    f"addressed by the row it belongs to. Name the id column with id=",
                )
            )
            return
        columns: list[tuple[str, Any]] = [("external_id", _external_ids(table[identity]))]
        for block in attributes:
            column = block.get("field") or block["name"]
            if column in table.column_names:
                columns.append((block["name"], table[column]))
        if len(columns) == 1:
            return
        where = "" if view is None else f" of view '{view}'"
        self._page_rows(
            kind="values",
            name=source if view is None else f"{source}-{view}",
            view=view,
            table=_selected(table, rows, columns),
            limits=self.limits.get("values", {}),
            line=f"values on existing entities{where} from '{source}'",
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
            self._artifacts_of(document, block)
            labels = block.get("labels")
            if labels is not None:
                self._artifacts_of(document, labels, parent=block["name"])

    def _artifacts_of(self, document: dict, block: dict, parent: str | None = None) -> None:
        layer = block["name"]
        view_field = _view_field(block)
        artifacts = self.db.deltas.get(block.get("source"))
        members_source = (block.get("members") or {}).get("source")
        members = self.db.deltas.get(members_source)
        # An inline roster stays in the declaration, so it is offered once: with the layer it
        # belongs to, which this commit is declaring. The build compiled the roster of a layer
        # declared before the first commit, and a key already published takes no second record.
        inline = (block.get("artifacts") or []) if layer not in self.held_layers else []
        if artifacts is None and members is None and not inline:
            return
        if parent is not None and parent not in self.held_layers and not self._planned(parent):
            self.findings.append(
                Finding(
                    "a labels delta before its clustering",
                    f"'{layer}' labels '{parent}', which this commit neither holds nor stages. "
                    f"Stage the clustering's artifacts beside the labels",
                )
            )
            return
        rows = _artifact_rows(artifacts, members, block, inline, view_field)
        if not rows:
            return
        self._publish(block, rows)

    def _planned(self, layer: str) -> bool:
        """Whether this plan publishes a key into a layer (§6.3).

        A clustering this database declared at an earlier commit may hold any key, and the SDK does
        not enumerate the server's artifacts to find out: a label naming a key it does not hold is
        a `404` on the attachment, reported per artifact. What the pre-flight can decide is the
        case where the clustering is new in this commit, where a label can only attach to a key
        this commit stages.
        """
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

    # ------------------------------------------------------------------ the column check

    #: What an artifacts table's columns may be called, whatever a layer declares of them.
    ARTIFACT_COLUMNS = frozenset(
        {"level", "key", "parent", "contents", "attached_layer", "attached_level", "attached_key",
         "space", "excluding", "rank", "entity", "entity_id", "bbox", "circle", "ellipse", "wkt"}
    )

    def _refuse_unknown_columns(self, document: dict) -> None:
        """A column no block declares, and a key column for a layer with supplied content (§6.3)."""
        supplied = {
            block["name"]
            for block in document.get("layer", [])
            if (block.get("content") or {}).get("supplied")
        }
        for layer, column in _from_columns(document).items():
            if layer not in supplied:
                continue
            for source, delta in self.db.deltas.items():
                if column in delta.columns:
                    raise Refusal(
                        f"commit: '{source}' stages column '{column}' as the membership of layer "
                        f"'{layer}', which declares supplied content. An artifact served without "
                        f"content its layer declares cannot be told from one whose content was "
                        f"withheld, so such a layer takes an artifacts table: declare it with "
                        f"source= and members=, and publish through those"
                    )
        claimed = self._claimed_columns(document)
        for source, delta in self.db.deltas.items():
            allowed = claimed.get(source)
            if allowed is None:
                continue
            unknown = [c for c in delta.columns if c not in allowed]
            if unknown:
                raise Refusal(
                    f"commit: '{source}' stages column(s) "
                    + ", ".join(repr(c) for c in unknown)
                    + " that no block of this declaration reads. A column's type is fixed at the "
                    "first commit, so a column declared now would have no home in the rows already "
                    "built. Drop it from the frame, or rebuild the database"
                )

    def _claimed_columns(self, document: dict) -> dict[str, set[str]]:
        """Which columns each staged source may carry, by the blocks that read it."""
        claimed: dict[str, set[str]] = {}
        for name, staged in list(self.db.sources.items()) + list(self.db.deltas.items()):
            # A source's id column is claimed wherever it is read: it is this source's identity
            # rather than one of its values (§3).
            into = claimed.setdefault(name, set())
            if staged.id_column:
                into.add(staged.id_column)
        points: set[str] = set()
        by_group: dict[str, set[str]] = {}
        for view in view_entries(document, self.db.default_source):
            source = view["source"]
            points.add(source)
            into = claimed.setdefault(source, set())
            into |= {view["x"], view["y"]}
            if view["discriminator"]:
                into.add(view["discriminator"])
            access = view["point_visibility"].get("field")
            if access:
                into.add(access)
            for group in _groups_of(view):
                by_group.setdefault(group, set()).add(source)
        for block in document.get("attribute", []):
            group = _scoped_to(block)
            column = block.get("field") or block["name"]
            if group is not None and not block.get("source"):
                # A family read from each view's own points file: the column is that file's.
                for source in by_group.get(group, ()):
                    claimed.setdefault(source, set()).add(column)
                continue
            source = block.get("source") or self.db.default_source
            into = claimed.setdefault(source, set())
            into.add(column)
            if group is not None:
                into.add(dict(block.get("fields", {})).get("view") or "view")
        for block in document.get("layer", []):
            # A label set is a block of its own inside its clustering's, and it names two sources.
            for one in (block, block.get("labels")):
                if not isinstance(one, dict):
                    continue
                members = one.get("members") if isinstance(one.get("members"), dict) else {}
                for table in (one.get("source"), members.get("source")):
                    if table:
                        claimed.setdefault(table, set()).update(self.ARTIFACT_COLUMNS)
                        # A scoped layer's rows carry the view, wherever its `fields` names it.
                        claimed[table].update(dict(one.get("fields", {})).values())
                if members.get("source"):
                    claimed[members["source"]].update(dict(members.get("fields", {})).values())
        for column in _from_columns(document).values():
            for source in points:
                claimed.setdefault(source, set()).add(column)
        return claimed


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
    else:
        # A record carrying neither `members` nor `excluding` is a `422`, and the route makes no
        # exception for a shape: "an artifact whose membership holds nobody is published with an
        # empty `members` list" (contracts §3.4). So a spatial record carries the empty list
        # beside its shape, which the shape's own resolution then supersedes.
        record["members"] = [addressed(e) for e in members]
    body = json.dumps(record).encode()
    while len(body) > budget and "members" in record and (members or any(sets)):
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


def _blank(key: str, level: int, view: str | None = None) -> dict:
    return {"key": key, "level": level, "view": view, "members": [], "sets": [], "content": [],
            "parent": [], "attached": None, "excluding": None, "space": None}


def _artifact_rows(artifacts, members, block: dict, inline=None, view_field=None) -> list[dict]:
    """One layer's staged tables as artifact records: the key, its parts and its sets.

    A member table's grain is `(key, entity, rank)`: a null rank is the membership and rank *k* is
    content *k*'s generating set (annotation-write-cycle §6.1). An artifacts table's `contents` is
    one value list per rank, positional over the kinds the layer declares. A shape column, `space`
    and `excluding` are columns of the artifact row, and the publication record carries each as
    the row wrote it (contracts §3.4).
    """
    rows: dict[tuple[int, str, str | None], dict] = {}
    for record in _declared_artifacts(artifacts, inline):
        level = int(record.get("level") or 0)
        key = str(record["key"])
        # On a layer scoped to a group the key is unique per view, one key in two views being two
        # artifacts (contracts §3.4 r84), so the view is part of the row's identity here.
        view = None if view_field is None else record.get(view_field)
        row = rows.setdefault((level, key, view), _blank(key, level, view))
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
    if members is not None:
        table = pq.read_table(members.path)
        fields = table.column_names
        entity_column = _member_entity(block, members, fields)
        levels = table["level"].to_pylist() if "level" in fields else [0] * table.num_rows
        ranks = table["rank"].to_pylist() if "rank" in fields else [None] * table.num_rows
        keys = table["key"].to_pylist()
        entities = table[entity_column].to_pylist()
        views = (
            table[view_field].to_pylist()
            if view_field is not None and view_field in fields
            else [None] * table.num_rows
        )
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


def _member_entity(block: dict, members, fields: Sequence[str]) -> str:
    """The column a member row names its entity in: what `[layer.members].fields` says (§4.6)."""
    named = dict((block.get("members") or {}).get("fields", {})).get("entity")
    for column in (named, members.id_column, "entity", "entity_id"):
        if column and column in fields:
            return column
    raise Refusal(
        f"commit: the members table '{members.name}' names no entity column. "
        f"[layer.members].fields = {{ entity = <column> }} says which column that is"
    )


def _view_field(block: dict) -> str | None:
    """The column a scoped layer's rows carry their view in, and `None` on an entity-scoped one."""
    return dict(block.get("fields", {})).get("view") if _scoped_to(block) else None


def _declared_artifacts(artifacts, inline) -> list[dict]:
    """The layer's own artifact rows: a staged table's, then any the declaration carries inline."""
    records = rows_of(pq.read_table(artifacts.path)) if artifacts is not None else []
    return records + [dict(row) for row in (inline or [])]


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
    if page.kind == "group_view":
        group, _, key = page.name.partition(":")
        return control.create_view(group, key, page.body)
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
    if answer.ok:
        # Every write acknowledgement names the cycle its work is published in (decision 0144).
        # The report prints the last of them: the numbers do not decrease, so it is the one every
        # page of this commit is visible at.
        held = answer.body.get("publication")
        if held is not None:
            report.publication = int(held)
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
        report.memberships_joined += int(body.get("joined", 0))
    elif page.kind == "publish":
        report.artifacts_minted += int(body.get("created", 0))
        report.already_present += int(body.get("filled", 0))
        report.memberships_joined += int(body.get("joined", 0))
        report.without_content += int(body.get("without_content", 0))
        minted = report.artifact_ids.setdefault(page.name, {})
        for one in body.get("artifacts", []):
            minted[one["key"]] = str(one["tessera_id"])
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
