"""A later commit: the plan, the pre-flight, and the pages that carry it (python-sdk.md §6).

The first commit builds. Every commit after it pages the staged deltas through the control plane,
in the order §6.2 fixes: declarations, points per view, values on existing entities, artifacts per
layer, then a flush and a wait for the publication that follows the last acknowledgement. A part
supplied twice is accepted and a part supplied differently is a `409` on that part (ingest §1.1),
so the order matters for existence and for nothing else.

`check()` runs the planner and the pre-flight and sends nothing; `commit()` runs the same plan.
The two therefore cannot disagree about what would be sent.

A points delta's held rows send nothing. python-sdk.md §6.3 lists a row whose id the map already
holds as acknowledged and does not send it, which is what makes a re-staged frame read as already
present rather than as a page of refusals, and the values route at step 3 therefore carries a delta
on a source that feeds attributes and no view. The one part of a held row that travels is a
from-column layer's key, which §6.2 step 3 sends as a publication in step 4.
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass, field
from typing import Any, Iterable, Sequence

import pyarrow as pa
import pyarrow.parquet as pq

from . import _control
from ._control import (
    Answer,
    Control,
    addressed,
    arrow_body,
    batch_id,
    content_digest,
    members_digest,
    parts_digest,
)
from ._refusal import Refusal

#: How long `commit()` waits for the publication after its last acknowledgement, in seconds. The
#: flush request pulls the tick's deadline forward, so the wait is one executor loop on an idle
#: server; the figure is a ceiling on a busy one, not an expectation.
FLUSH_TIMEOUT = 60.0

#: How often the flush wait reads `/control/status`, in seconds.
FLUSH_INTERVAL = 0.2


@dataclass
class Finding:
    """One pre-flight finding (§6.3). `refuses` is true where the server would refuse too."""

    what: str
    detail: str
    refuses: bool = False

    def __str__(self) -> str:
        return f"{'refused' if self.refuses else 'reported'}: {self.what}. {self.detail}"


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
    #: The source ids a points page carries, moved to `acknowledged` at its acknowledgement (§3).
    entities: tuple = ()
    #: The sets this page is the last of, as `(layer, key, rank, digest)`. Recorded at the page's
    #: acknowledgement, so a later commit knows the set was sent whole and plans no page for it.
    completes: list = field(default_factory=list)


# ---------------------------------------------------------------------------- the pre-flight


class Planner:
    """§6.2's plan and §6.3's pre-flight over one database's staged deltas."""

    def __init__(self, database, control: Control, meta: dict) -> None:
        self.db = database
        self.control = control
        self.meta = meta
        self.log = database.commit_log
        self.limits = control.limits()
        self.findings: list[Finding] = []
        self.pages: list[Page] = []
        #: The keys this plan will publish per layer, so a later step reads them as held.
        self._planned_keys: dict[str, set[str]] = {}
        #: How many artifact pages this plan has built per layer, which is a page's index in its
        #: batch id. The artifact routes carry no batch-id header, so the id is the SDK's own
        #: bookkeeping: the commit log holds it and a re-run skips the page rather than re-sending
        #: a publication whose every part the level already holds.
        self._artifact_pages: dict[str, int] = {}

    # ------------------------------------------------------------------ entry

    def plan(self) -> tuple[list[Page], list[Finding]]:
        document = self.db._document()
        self._refuse_unknown_columns(document)
        self._declarations(document)
        self._points(document)
        self._values(document)
        self._artifacts(document)
        if self.pages:
            self.pages.append(Page(kind="flush", name="", line="flush, then wait for the tick"))
        return self.pages, self.findings

    # ------------------------------------------------------------------ 1. declarations

    def _declarations(self, document: dict) -> None:
        """The runtime `PUT`s for every layer declared since the last commit (§6.2 step 1).

        The bodies come from `tessera check --payloads` over the SDK's own declaration, so the
        mapping from a block to a request body is the binary's and not a second one in Python.

        Not built yet: the emitter covers layers only. An attribute, a vocabulary, a view or a view
        group declared after the first commit is refused at the verb, naming `declare(kind, block)`
        and a rebuild as what to do instead (§11.2 C).
        """
        declared: list[str] = []
        for block in document.get("layer", []):
            declared.append(block["name"])
            if block.get("labels"):
                declared.append(block["labels"]["name"])
        pending = [name for name in declared if not self.log.declared(name)]
        if not pending:
            return
        payloads = self.db._payloads()
        by_name = {payload["name"]: payload for payload in payloads}
        for payload in payloads:
            name = payload["name"]
            if self.log.declared(name):
                continue
            self.pages.append(
                Page(
                    kind="layer",
                    name=name,
                    line=f"declare layer '{name}' ({payload['hierarchy']['kind']})",
                    body=payload,
                )
            )
        missing = [name for name in pending if name not in by_name]
        if missing:
            raise Refusal(
                "commit: `tessera check --payloads` emitted no body for "
                + ", ".join(sorted(missing))
            )

    # ------------------------------------------------------------------ 2. points

    def _points(self, document: dict) -> None:
        for view in self._views_in_order(document):
            source = view.get("source") or self.db.default_source
            delta = self.db.deltas.get(source)
            if delta is None:
                continue
            self._points_of(document, view, source, delta)

    def _views_in_order(self, document: dict) -> list[dict]:
        """The allocation view first, then the rest in declaration order (§6.2 step 2)."""
        views = list(document.get("view", []))
        anchor = document.get("defaults", {}).get("allocation_view")
        views.sort(key=lambda block: 0 if block.get("name") == anchor else 1)
        return views

    def _points_of(self, document: dict, view: dict, source: str, delta) -> None:
        name = view["name"]
        table = pq.read_table(delta.path)
        entities = table["entity_id"].to_pylist()
        acknowledged = self.db.id_map.acknowledged()
        held = [i for i, entity in enumerate(entities) if entity in acknowledged]
        fresh = [i for i, entity in enumerate(entities) if entity not in acknowledged]
        if held:
            self.findings.append(
                Finding(
                    "rows already present",
                    f"{len(held)} of {len(entities)} row(s) staged on '{source}' carry an id this "
                    f"database already holds, so they are not sent as points of view '{name}'",
                )
            )
        fresh = self._inside_the_frame(name, table, view, fresh)
        if not fresh:
            return
        columns = self._point_columns(document, view, source, table)
        self._page_rows(
            kind="points",
            name=source,
            view=name,
            table=_selected(table, fresh, columns),
            limits=self.limits.get("ingest", {}),
            line=f"points into view '{name}' from '{source}'",
            entities=[entities[row] for row in fresh],
        )

    def _inside_the_frame(
        self, view: str, table: pa.Table, block: dict, rows: list[int]
    ) -> list[int]:
        """Drop the rows outside the view's frame and list them (§6.3).

        A frame is fixed at the first commit and the ingest route refuses a whole page carrying a
        row outside it, so a row that would take the page down is dropped here and reported with
        the frame it missed.
        """
        frame = next(
            (v.get("quantisation") for v in self.meta.get("views", []) if v.get("id") == view),
            None,
        )
        if frame is None:
            return rows
        fields = dict(block.get("fields", {}))
        x_column = fields.get("x") or fields.get("lon") or "x"
        y_column = fields.get("y") or fields.get("lat") or "y"
        if x_column not in table.column_names or y_column not in table.column_names:
            return rows
        xs = table[x_column].to_pylist()
        ys = table[y_column].to_pylist()
        inside, outside = [], []
        for row in rows:
            x, y = xs[row], ys[row]
            if x is None or y is None:
                inside.append(row)
                continue
            if frame["x_min"] <= x <= frame["x_max"] and frame["y_min"] <= y <= frame["y_max"]:
                inside.append(row)
            else:
                outside.append(row)
        if outside:
            self.findings.append(
                Finding(
                    "rows outside the frame",
                    f"{len(outside)} row(s) fall outside view '{view}''s frame "
                    f"[{frame['x_min']:g}, {frame['x_max']:g}] × "
                    f"[{frame['y_min']:g}, {frame['y_max']:g}], and are dropped: the server would "
                    f"refuse the whole page. A frame is fixed at the first commit",
                )
            )
        return inside

    def _point_columns(
        self, document: dict, view: dict, source: str, table: pa.Table
    ) -> list[tuple[str, pa.Array]]:
        """The wire's columns for one points page: geometry, labels, the id, values, layer keys."""
        fields = dict(view.get("fields", {}))
        x_column = fields.get("x") or fields.get("lon") or "x"
        y_column = fields.get("y") or fields.get("lat") or "y"
        columns: list[tuple[str, Any]] = [
            ("x", table[x_column].cast(pa.float64())),
            ("y", table[y_column].cast(pa.float64())),
        ]
        access = dict(view.get("point_visibility", {})).get("field")
        if access and access in table.column_names:
            columns.append(("access", _label_lists(table[access])))
        columns.append(
            (
                "external_id",
                pa.array(
                    [_control.external_id(e) for e in table["entity_id"].to_pylist()], pa.binary()
                ),
            )
        )
        for block in document.get("attribute", []):
            if (block.get("source") or self.db.default_source) != source:
                continue
            column = block.get("field") or block["name"]
            if column in table.column_names:
                columns.append((block["name"], table[column]))
        for layer, column in self.db.from_columns.items():
            if column in table.column_names and self._layer_draws(document, layer, view["name"]):
                columns.append((layer, table[column]))
        return columns

    def _layer_draws(self, document: dict, layer: str, view: str) -> bool:
        for block in document.get("layer", []):
            if block.get("name") == layer:
                views = block.get("views")
                return not isinstance(views, list) or view in views
        return False

    # ------------------------------------------------------------------ 3. values

    def _values(self, document: dict) -> None:
        """A delta on a source that feeds attributes and no view (§6.2 step 3)."""
        view_sources = {
            block.get("source") or self.db.default_source for block in document.get("view", [])
        }
        for source, delta in self.db.deltas.items():
            if source in view_sources:
                continue
            attributes = [
                block
                for block in document.get("attribute", [])
                if (block.get("source") or self.db.default_source) == source
            ]
            if not attributes:
                continue
            table = pq.read_table(delta.path)
            rendered = [block["name"] for block in attributes if block.get("render")]
            if rendered:
                self.findings.append(
                    Finding(
                        "a rendered column on the values route",
                        f"'{source}' carries {', '.join(rendered)}, declared `render`, and a "
                        f"rendered value is drawn from the hot column of the row that carries it. "
                        f"The values route fills entities and acquires no row, so it refuses one; "
                        f"the column is left as the row that created it carries it",
                    )
                )
            columns: list[tuple[str, Any]] = [
                (
                    "external_id",
                    pa.array(
                        [_control.external_id(e) for e in table["entity_id"].to_pylist()],
                        pa.binary(),
                    ),
                )
            ]
            for block in attributes:
                if block.get("render"):
                    continue
                column = block.get("field") or block["name"]
                if column in table.column_names:
                    columns.append((block["name"], table[column]))
            if len(columns) == 1:
                continue
            self._page_rows(
                kind="values",
                name=source,
                view=None,
                table=_selected(table, list(range(table.num_rows)), columns),
                limits=self.limits.get("values", {}),
                line=f"values on existing entities from '{source}'",
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
        entities: Sequence[int] = (),
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
                        entities=tuple(entities[first : first + count]),
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
        column = self.db.from_columns.get(layer)
        if column is not None:
            self._from_column(document, block, column)
            return
        artifacts = self.db.deltas.get(block.get("source"))
        members_source = (block.get("members") or {}).get("source")
        members = self.db.deltas.get(members_source)
        inline = block.get("artifacts")
        if artifacts is None and members is None and not inline:
            return
        if parent is not None and not self._clustering_is_reachable(parent):
            self.findings.append(
                Finding(
                    "a labels delta before its clustering",
                    f"'{layer}' labels '{parent}', which this commit neither holds nor stages. "
                    f"Stage the clustering's artifacts beside the labels",
                    refuses=True,
                )
            )
            return
        rows = _artifact_rows(artifacts, members, block, inline)
        if not rows:
            return
        self._publish(block, rows)

    def _clustering_is_reachable(self, parent: str) -> bool:
        """Whether a label set's clustering can hold the keys the labels attach to (§6.3).

        A clustering this database declared at an earlier commit may hold any key, and the SDK does
        not enumerate the server's artifacts to find out: a label naming a key it does not hold is
        a `404` on the attachment, reported per artifact. What the pre-flight can decide is the
        case where the clustering is new in this commit, where a label can only attach to a key
        this commit stages.
        """
        if self.log.declared(parent) or self.log.published(parent):
            return True
        return bool(self._planned_keys.get(parent))

    def _from_column(self, document: dict, block: dict, column: str) -> None:
        """A from-column layer's keys on entities this database already holds (§6.2 step 3).

        The values route fills a column and mints nothing, so a key no artifact holds is refused
        there. The delta is grouped by key instead and sent as publications, each artifact carrying
        its members, which is what the column would have done at the build. A key on a row this
        commit creates travels with the row and mints its artifact at the window close.

        The column is read from the first of the layer's views whose points source has a delta. A
        layer drawn on several views reads one of them, the column naming one key per entity and an
        entity holding one row per view.
        """
        views = block.get("views")
        source = None
        for view in document.get("view", []):
            if isinstance(views, list) and view["name"] not in views:
                continue
            candidate = view.get("source") or self.db.default_source
            if candidate in self.db.deltas:
                source = candidate
                break
        if source is None:
            return
        table = pq.read_table(self.db.deltas[source].path)
        if column not in table.column_names:
            return
        acknowledged = self.db.id_map.acknowledged()
        keys = table[column].to_pylist()
        entities = table["entity_id"].to_pylist()
        grouped: dict[str, list[int]] = {}
        for key, entity in zip(keys, entities):
            if key is None or entity not in acknowledged:
                continue
            grouped.setdefault(str(key), []).append(entity)
        if not grouped:
            return
        rows = [
            {"key": key, "level": 0, "members": members, "content": [], "parent": [], "attached": None}
            for key, members in grouped.items()
        ]
        self._publish(block, rows)

    def _publish(self, block: dict, rows: list[dict]) -> None:
        """One layer's artifact rows, as publications and growths (§6.2 step 4).

        Within a layer the levels go coarse first, a nested batch resolves parents that are its own
        siblings, and a key the level already holds falls under the fill rule: its members join by
        `PATCH` and its `inherited` content is filled the same way. Content gated `all` travels on
        the publish record with the first page of its generating set, the route refusing a content
        fill on such a layer; further set pages are `PATCH` at the rank.
        """
        layer = block["name"]
        gates = _content_gates(block)
        held = self.log.published(layer)
        publish_limits = self.limits.get("publish", {})
        grow_limits = self.limits.get("grow", {})
        cap = int(publish_limits.get("max_body_bytes", 64 << 20))
        most = int(publish_limits.get("max_artifacts_per_request", 10_000))
        planned: set[str] = self._planned_keys.setdefault(layer, set())

        most_excluded = int(publish_limits.get("max_excluded_per_request", 1_000_000))
        spatial = block.get("membership") == "spatial"

        new_rows: list[dict] = []
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
                        refuses=True,
                    )
                )
                continue
            state = held.get(row["key"])
            if state is None:
                new_rows.append(row)
                continue
            if row["content"] and "all" in gates and not state.get("content"):
                self.findings.append(
                    Finding(
                        "content gated `all` on a held artifact that has none",
                        f"layer '{layer}', artifact '{row['key']}': a content whose gate is `all` "
                        f"is served only to a viewer who can see every member of its generating "
                        f"set, so it travels on the publication with the set's first page. Not "
                        f"built yet: filling one onto an artifact published without it has no "
                        f"route. Publish the artifact under a new key",
                        refuses=True,
                    )
                )
                continue
            if (
                row["content"]
                and "all" in gates
                and content_digest(row["content"]) != state.get("content")
            ):
                # The artifact holds a different content, and a content is a fixed part: supplied
                # once and replaced never (ingest §1.5). The route refuses a content fill on a
                # layer whose content requires every member visible, so this one is not sent, and
                # the caller is told rather than left to read an unchanged label as an applied one.
                # A content re-supplied unchanged is a cell re-run and says nothing.
                self.findings.append(
                    Finding(
                        "content gated `all` differs from the one the artifact holds",
                        f"layer '{layer}', artifact '{row['key']}': a content is supplied once and "
                        f"replaced never, so this one is not sent. Publish the artifact under a "
                        f"new key",
                    )
                )
            if spatial:
                # A spatial artifact's membership is its shape, resolved per request against each
                # generation's own segments, so there is no member set to page (§6.2 step 4).
                continue
            self._grow(block, row, gates, grow_limits, state)

        for level in sorted({int(row.get("level") or 0) for row in new_rows}):
            at_level = [row for row in new_rows if int(row.get("level") or 0) == level]
            for batch in _batched(_parents_first(at_level), cap, most):
                blocks, artifacts, members = batch
                body = _publish_body(level, blocks)
                keys = [row["key"] for row in artifacts]
                planned.update(keys)
                self._artifact_page(
                    "publish",
                    layer,
                    level,
                    f"publish {len(keys)} artifact(s) into '{layer}' level {level}",
                    body,
                    artifacts=len(keys),
                    members=members,
                )
                page = self.pages[-1]
                for row in artifacts:
                    # The publication carries a first page of every set; what did not fit follows
                    # as growths. The page that carries the last of a set is the one that records
                    # it, so a set half sent is a set this database does not claim to hold.
                    last: dict[Any, Page] = {}
                    for rank, remainder in row.get("remainders", []):
                        grown = self._grow_pages(
                            layer, level, row["key"], rank, remainder, grow_limits
                        )
                        if grown:
                            last[rank] = grown[-1]
                    for rank, members in [(None, row.get("members", []))] + list(
                        enumerate(row.get("sets", []))
                    ):
                        if not members:
                            continue
                        carrier = last.get(rank, page)
                        carrier.completes.append(
                            (layer, row["key"], rank, members_digest(members))
                        )

    def _grow(
        self, block: dict, row: dict, gates: set[str], grow_limits: dict, state: dict
    ) -> None:
        """A key the level holds: its members join, and an `inherited` content is filled.

        A set this database has already sent whole is not sent again. A page of it would be a
        lawful no-op, the join answering `joined: 0`, but §3 says a re-staged frame sends nothing.
        """
        layer = block["name"]
        level = int(row.get("level") or 0)
        fills: dict[str, Any] = {"key": row["key"]}
        if row["content"] and "all" not in gates:
            fills["content"] = [
                {"rank": rank, "values": list(values)}
                for rank, values in enumerate(row["content"])
                if values is not None
            ]
        # A fixed part this database published with the artifact is already what the artifact
        # holds, so no record carries it a second time.
        if parts_digest(row.get("parent"), row.get("attached")) != state.get("parts"):
            if row.get("parent"):
                fills["parent"] = list(row["parent"])
            if row.get("attached"):
                fills["attached_to"] = row["attached"]
        if len(fills) > 1:
            body = json.dumps(
                {"level": level, "addressing": "external", "artifacts": [fills]}
            ).encode()
            self._artifact_page(
                "grow",
                layer,
                level,
                f"fill '{row['key']}' in '{layer}' level {level}",
                body,
                artifacts=1,
            )
        for rank, members in [(None, row.get("members", []))] + list(
            enumerate(row.get("sets", []))
        ):
            self._set_pages(layer, level, row["key"], rank, members, grow_limits)

    def _set_pages(
        self,
        layer: str,
        level: int,
        key: str,
        rank: int | None,
        members: Sequence[int],
        grow_limits: dict,
    ) -> None:
        """One set's growth pages, unless this database has already sent that set whole."""
        if not members:
            return
        digest = members_digest(members)
        if self.log.holds_set(layer, key, rank, digest):
            return
        pages = self._grow_pages(layer, level, key, rank, members, grow_limits)
        if pages:
            pages[-1].completes.append((layer, key, rank, digest))

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
        """One publication or growth, with the batch id the commit log records it under."""
        index = self._artifact_pages.get(layer, 0)
        self._artifact_pages[layer] = index + 1
        self.pages.append(
            Page(
                kind=kind,
                name=layer,
                level=level,
                line=line,
                body=body,
                batch=batch_id(layer, index, body),
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
        members: Sequence[int],
        grow_limits: dict,
    ) -> list[Page]:
        """The pages that join one set, in order. The caller marks the last of them."""
        if not members:
            return []
        cap = int(grow_limits.get("max_body_bytes", 64 << 20))
        most = int(grow_limits.get("max_members_per_request", 5_000_000))
        per_page = max(1, min((cap - 512) // 16, most))
        appended: list[Page] = []
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
            appended.append(self.pages[-1])
        return appended

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
        for layer, column in self.db.from_columns.items():
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
            # The user's own id column is in the file the SDK wrote beside the `entity_id` it
            # minted from it, so a delta on that source carries it too.
            into = claimed.setdefault(name, {"entity_id", "entity"})
            if staged.user_id_column:
                into.add(staged.user_id_column)
        points: set[str] = set()
        for view in document.get("view", []):
            source = view.get("source") or self.db.default_source
            points.add(source)
            into = claimed.setdefault(source, {"entity_id", "entity"})
            into |= set(dict(view.get("fields", {})).values())
            access = dict(view.get("point_visibility", {})).get("field")
            if access:
                into.add(access)
        for block in document.get("attribute", []):
            source = block.get("source") or self.db.default_source
            claimed.setdefault(source, {"entity_id", "entity"}).add(
                block.get("field") or block["name"]
            )
        for block in document.get("layer", []):
            # A label set is a block of its own inside its clustering's, and it names two sources.
            for one in (block, block.get("labels")):
                if not isinstance(one, dict):
                    continue
                members = one.get("members") if isinstance(one.get("members"), dict) else {}
                for table in (one.get("source"), members.get("source")):
                    if table:
                        claimed.setdefault(table, set()).update(self.ARTIFACT_COLUMNS)
                if members.get("source"):
                    claimed[members["source"]].update(dict(members.get("fields", {})).values())
        for column in self.db.from_columns.values():
            for source in points:
                claimed.setdefault(source, set()).add(column)
        return claimed


# ---------------------------------------------------------------------------- helpers


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


def _content_gates(block: dict) -> set[str]:
    """The content-grain gates a layer's supplied kinds declare: `all`, `inherited`, or neither."""
    content = block.get("content") or {}
    gates = {
        kind.get("require_member_visibility")
        for kind in content.get("supplied", [])
        if isinstance(kind, dict)
    }
    named = content.get("require_member_visibility")
    if named:
        gates.add(named)
    return {gate for gate in gates if gate}


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
    remainders: list[tuple[int | None, list[int]]] = []
    record: dict[str, Any] = {"key": row["key"]}
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
    joining: Sequence[int] = (),
    leaving: Sequence[int] = (),
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


#: An artifact row's shape, in its layer's kind's field and no other (configuration.md §1).
SHAPE_FIELDS = ("bbox", "circle", "ellipse", "wkt")


def _blank(key: str, level: int) -> dict:
    return {"key": key, "level": level, "members": [], "sets": [], "content": [], "parent": [],
            "attached": None, "excluding": None, "space": None}


def _artifact_rows(artifacts, members, block: dict, inline=None) -> list[dict]:
    """One layer's staged tables as artifact records: the key, its parts and its sets.

    A member table's grain is `(key, entity, rank)`: a null rank is the membership and rank *k* is
    content *k*'s generating set (annotation-write-cycle §6.1). An artifacts table's `contents` is
    one value list per rank, positional over the kinds the layer declares. A shape column, `space`
    and `excluding` ride the artifact row and reach the publication as they are written
    (contracts §3.4).
    """
    rows: dict[tuple[int, str], dict] = {}
    for record in _declared_artifacts(artifacts, inline):
        level = int(record.get("level") or 0)
        key = str(record["key"])
        row = rows.setdefault((level, key), _blank(key, level))
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
        entity_column = "entity" if "entity" in fields else "entity_id"
        levels = table["level"].to_pylist() if "level" in fields else [0] * table.num_rows
        ranks = table["rank"].to_pylist() if "rank" in fields else [None] * table.num_rows
        keys = table["key"].to_pylist()
        entities = table[entity_column].to_pylist()
        for level, key, rank, entity in zip(levels, keys, ranks, entities):
            level = int(level or 0)
            record = rows.setdefault((level, str(key)), _blank(str(key), level))
            if rank is None:
                record["members"].append(entity)
            else:
                rank = int(rank)
                while len(record["sets"]) <= rank:
                    record["sets"].append([])
                record["sets"][rank].append(entity)
    return list(rows.values())


def _declared_artifacts(artifacts, inline) -> list[dict]:
    """The layer's own artifact rows: a staged table's, then any the declaration carries inline."""
    records: list[dict] = []
    if artifacts is not None:
        table = pq.read_table(artifacts.path)
        fields = table.column_names
        columns = {name: table[name].to_pylist() for name in fields}
        records += [
            {name: values[i] for name, values in columns.items() if values[i] is not None}
            for i in range(table.num_rows)
        ]
    records += [dict(row) for row in (inline or [])]
    return records


# ---------------------------------------------------------------------------- the run


def run(database, control: Control, pages: Sequence[Page], report) -> None:
    """Send the plan, in order, and fold every answer into the report.

    The log and the id map are written after every page rather than at the end. A page is durable
    at its acknowledgement, and a commit interrupted after one would otherwise send it again at the
    next: within the WAL retention window that is a replay, and past it a page of duplicates.
    """
    log = database.commit_log
    #: The publication as it stood before the page in flight was sent. The flush wait compares
    #: against this and not against a reading taken afterwards: a period tick landing between the
    #: last page and the request would otherwise have already moved every counter, and the wait
    #: would run to its ceiling.
    published = control.publication()
    accepted = 0
    rows = 0
    try:
        for page in pages:
            if page.kind == "flush":
                # A commit that wrote nothing has nothing to make visible, and a tick over an empty
                # buffer publishes nothing and moves no counter, so the wait would run to its
                # ceiling and report a flush that never happened.
                if accepted:
                    report.flush_wait = _flush(control, published, report, rows > 0)
                continue
            if page.batch is not None and log.holds(page.batch):
                report.already_present += 1
                report.skipped.append(page.batch)
                continue
            published = control.publication()
            answer = _send(control, page)
            _fold(database, report, page, answer)
            log.save()
            database.id_map.save()
            if answer.ok:
                accepted += 1
                if page.kind in ("points", "values"):
                    rows += 1
    finally:
        log.save()
        database.id_map.save()


def _send(control: Control, page: Page) -> Answer:
    if page.kind == "layer":
        return control.declare_layer(page.body)
    if page.kind == "points":
        return control.ingest(page.body, page.batch, page.view)
    if page.kind == "values":
        return control.values(page.body, page.batch)
    if page.kind == "publish":
        return control.publish(page.name, page.body)
    if page.kind == "grow":
        return control.grow(page.name, page.body)
    raise Refusal(f"commit: no route for a page of kind {page.kind!r}")


def _fold(database, report, page: Page, answer: Answer) -> None:
    log = database.commit_log
    if not answer.ok:
        if page.kind == "points" and answer.status == 409:
            # A duplicate external id is the database saying it holds the row already. The page is
            # not acknowledged, since none of it was applied, but the ids it named are held, and
            # marking them so is what stops the next commit offering them again.
            database.id_map.acknowledge_ids(page.entities)
        report.refusals.append(
            {
                "what": page.line,
                "layer_or_view": page.view or page.name,
                "status": answer.status,
                "detail": _detail(answer),
            }
        )
        return
    if page.batch is not None:
        log.acknowledge(page.batch, page.kind, answer)
    for layer, key, rank, digest in page.completes:
        log.record_set(layer, key, rank, digest)
    body = answer.body
    if page.kind == "layer":
        log.declare([page.name])
    elif page.kind == "points":
        # The commit that carried the row is what acknowledges its id (§3), so the next commit's
        # pre-flight reads it as already present and a page refused here is sent again.
        database.id_map.acknowledge_ids(page.entities)
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
        sent = json.loads(page.body.decode())["artifacts"]
        log.publish(
            page.name,
            [
                (
                    one["key"],
                    content_digest([c["values"] for c in one.get("content", [])]),
                    parts_digest(one.get("parent"), one.get("attached_to")),
                )
                for one in sent
            ],
        )
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


def _flush(
    control: Control, published: tuple[dict[str, int], int, int], report, rows: bool
) -> float:
    """`POST /control/flush`, then the wait for the publication after the last acknowledgement.

    The flush pulls the tick's deadline forward and the 202 means accepted rather than done, so the
    wait is on `/control/status`: a partition whose published `segments_version` has moved past the
    one read at the last acknowledgement has published the rows this commit sent. The wait is
    reported whether or not it was reached, a commit being durable at its acknowledgements and
    visible at the tick.
    """
    versions, flushes, ticks = published
    control.flush()
    started = time.monotonic()
    deadline = started + FLUSH_TIMEOUT
    while time.monotonic() < deadline:
        now, flushed, ticked = control.publication()
        if any(now.get(name, 0) > version for name, version in versions.items()):
            return time.monotonic() - started
        if flushed > flushes:
            return time.monotonic() - started
        # A commit that sent no row wrote nothing the buffer holds, so its effects reach the served
        # forms at the executor's next loop and move neither counter above.
        if not rows and ticked > ticks:
            return time.monotonic() - started
        time.sleep(FLUSH_INTERVAL)
    report.flush_reached = False
    return time.monotonic() - started


def changes(control: Control, source_ids: Iterable[int], op: str, limits: dict) -> list[Answer]:
    """`POST /control/changes` for one op, paged under the route's two units (§6.5)."""
    items = [{"external_id": addressed(i), "op": op} for i in source_ids]
    per_page = int(limits.get("changes", {}).get("max_changes_per_request", 10_000))
    answers = []
    for start in range(0, len(items), per_page):
        answers.append(control.changes(items[start : start + per_page]))
    return answers


def leave(control: Control, layer: str, key: str, source_ids: Sequence[int], rank: int,
          level: int = 0) -> Answer:
    """`PATCH` a generating set at a rank, the one set that may shrink (decision 0135, §6.5)."""
    return control.grow(layer, patch_body(level, key, leaving=source_ids, rank=rank))
