"""Reading a Tessera database over HTTP: the reader, the selection and the answers they give.

A `Viewer` is a server address and a way to get a token. It holds no copy of the data: every
answer is a request the server checks against the token, so a local database and a hosted one are
read the same way, and two readers holding different access terms get different answers about
the same data.
"""

from __future__ import annotations

import base64
import io
import json
import struct
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Callable, Optional, Sequence

from ._auth import Token, TokenSource, minted
from ._refusal import Refusal

#: How near expiry a held token may come before the next read gets another, in seconds.
TOKEN_MARGIN = 60.0

#: The `/v1/viewport` fields the server reads as integers.
_VIEWPORT_INTEGERS = {"k", "artifact_budget", "underlay_offset"}

#: The frame kinds of a `/v1/viewport` body. An unknown kind is refused rather than skipped, since
#: skipping one could drop data and present what is left as the whole answer.
FRAME_TILES = 1
FRAME_SUB_CELLS = 2
FRAME_POINTS = 3
FRAME_TRAILER = 4
FRAME_ARTIFACTS = 5
_KINDS = {FRAME_TILES, FRAME_SUB_CELLS, FRAME_POINTS, FRAME_TRAILER, FRAME_ARTIFACTS}


def split_frames(body: bytes) -> list[tuple[int, bytes]]:
    """A `/v1/viewport` body as `(kind, payload)` pairs, or a refusal if it is not a whole one.

    Each frame is a one-byte kind, a four-byte little-endian length and the payload. The trailer
    comes last and marks the body complete, so a body cut short is refused rather than read as a
    smaller answer.
    """
    frames: list[tuple[int, bytes]] = []
    at = 0
    while at < len(body):
        if len(body) - at < 5:
            raise Refusal(f"viewport: truncated frame header at byte {at}")
        kind = body[at]
        if kind not in _KINDS:
            raise Refusal(f"viewport: unknown frame kind {kind} at byte {at}")
        (length,) = struct.unpack_from("<I", body, at + 1)
        end = at + 5 + length
        if end > len(body):
            raise Refusal(f"viewport: the frame at byte {at} claims a payload past the body's end")
        frames.append((kind, body[at + 5 : end]))
        at = end
    if not frames or frames[0][0] != FRAME_TILES:
        raise Refusal("viewport: the tiles frame must be first")
    if frames[-1][0] != FRAME_TRAILER:
        raise Refusal("viewport: no trailer, so the response is incomplete")
    return frames


def _tables(payloads: Sequence[bytes]):
    """The Arrow streams of one frame kind joined in arrival order, or `None` if there were none."""
    import pyarrow as pa
    import pyarrow.ipc as ipc

    tables = [ipc.open_stream(io.BytesIO(payload)).read_all() for payload in payloads]
    return pa.concat_tables(tables) if tables else None


def _no_points():
    """The points table of an answer that served none: the two columns every answer has."""
    import pyarrow as pa

    return pa.table({"tessera_id": pa.array([], pa.uint64()), "code": pa.array([], pa.uint64())})


def _tile_counts(frames: Sequence[tuple[int, bytes]]) -> dict:
    """The tiles frame's counts, summed over its tiles."""
    counted = _tables([p for kind, p in frames if kind == FRAME_TILES])
    return {
        name: sum(int(v) for v in counted.column(name).to_pylist())
        for name in ("visible", "matched", "highlighted", "served")
        if name in counted.column_names
    }


def _query(params: dict) -> str:
    return "?" + urllib.parse.urlencode(params) if params else ""


def _all_of(expressions: Sequence[dict]) -> Optional[dict]:
    """Several filter expressions as one, with any top-level `all_of` opened into its parts."""
    parts: list[dict] = []
    for expression in expressions:
        if set(expression) == {"all_of"}:
            parts.extend(expression["all_of"])
        else:
            parts.append(expression)
    if not parts:
        return None
    return parts[0] if len(parts) == 1 else {"all_of": parts}


class Sample:
    """The points a map draws at one zoom, with the annotations drawn beside them.

    It reads as its points table: `num_rows`, `column()`, `schema` and `to_pandas()` all reach
    it, and `points` names it. The table holds `tessera_id`, `code` (the point's position, on a
    grid of 2^32 steps per axis across the view) and every column declared with `render=True`.

    `artifacts` is the annotations served with the points, as a pyarrow table, and `sub_cells`
    the finer counts that `underlay_offset` asks for. Each is `None` when the answer carried
    none.

    The points are a sample thinned for drawing. `k` caps how many each map tile carries, so
    the table is smaller than the set it was drawn from. The table's schema metadata says by how
    much: `tessera.counts` holds `visible` (the items this reader may see in the tiles the
    request touched), `matched` (those that also match the filter), `highlighted` (those that
    also match the highlight) and `served` (the rows in this table). `tessera.request` is the
    request that was sent, and `tessera.trailer` the server's closing summary.
    """

    def __init__(self, points, artifacts=None, sub_cells=None):
        self.points = points
        self.artifacts = artifacts
        self.sub_cells = sub_cells

    def __getattr__(self, name: str):
        # Guards against recursion while `__init__` has not yet set these.
        if name in ("points", "artifacts", "sub_cells"):
            raise AttributeError(name)
        return getattr(self.points, name)

    def __getitem__(self, key):
        return self.points[key]

    def __len__(self) -> int:
        return self.points.num_rows

    def __repr__(self) -> str:
        served = [f"{self.points.num_rows} points"]
        if self.artifacts is not None:
            served.append(f"{self.artifacts.num_rows} artifacts")
        if self.sub_cells is not None:
            served.append(f"{self.sub_cells.num_rows} sub-cells")
        return f"Sample({', '.join(served)})"


class Selection:
    """Part of one view, as one reader sees it: a view, the filters applied to it, and a box.

    A view is one layout of the items: one map, with its own coordinates. A reader is who is
    asking. It sees only the items its access terms let it see, and every number here is
    computed over those items alone.

    Get one with `db.view(name)` or `viewer.view(name)`. `filter` and `within` return a new,
    narrower selection and leave this one as it was, so one selection can be the start of
    several.

        papers = db.view("papers")
        recent = papers.filter({"year": {"range": {"gte": 2020}}})
        corner = recent.within((0, 0, 10, 10))
        corner.count()
    """

    def __init__(
        self,
        reader: Callable[[], "Viewer"],
        view: str,
        filters: tuple = (),
        boxes: tuple = (),
    ) -> None:
        self._reader = reader
        self._view = view
        self._filters = filters
        self._boxes = boxes

    @property
    def view(self) -> str:
        """The name of the view this selection is part of."""
        return self._view

    @property
    def filters(self) -> Optional[dict]:
        """The filters applied so far, as one expression, or `None` if there are none."""
        return _all_of(self._filters)

    @property
    def box(self) -> Optional[tuple]:
        """The box this selection is limited to, as `(min_x, min_y, max_x, max_y)`, or `None`.

        After two calls to `within` it is the overlap of the two boxes. If they do not overlap,
        it is `None` and the selection holds nothing.
        """
        if not self._boxes:
            return None
        overlap = (
            max(box[0] for box in self._boxes),
            max(box[1] for box in self._boxes),
            min(box[2] for box in self._boxes),
            min(box[3] for box in self._boxes),
        )
        if overlap[0] > overlap[2] or overlap[1] > overlap[3]:
            return None
        return overlap

    def filter(self, expression: dict) -> "Selection":
        """A narrower selection: the items here that also match `expression`.

        `expression` is a filter written as a dictionary: a column name mapped to a test, or
        `all_of`, `any_of` or `none_of` over a list of expressions. Filters added one after
        another must all match.

            db.view("papers").filter({"venue": {"in": ["neurips", "icml"]}})
            db.view("papers").filter({"year": {"range": {"gte": 2020, "lt": 2024}}})
            db.view("papers").filter({"title": {"match": "graph neural network"}})
        """
        if not isinstance(expression, dict) or not expression:
            raise Refusal(
                f"filter takes one expression as a dictionary, such as "
                f"{{'year': {{'range': {{'gte': 2020}}}}}}; got {expression!r}"
            )
        return Selection(self._reader, self._view, self._filters + (expression,), self._boxes)

    def within(self, box: Sequence[float]) -> "Selection":
        """A narrower selection: the items here whose position is inside `box`.

        `box` is `(min_x, min_y, max_x, max_y)` in the view's own coordinates, the ones the rows
        were inserted with. An item on an edge is inside.

            db.view("papers").within((0.0, 0.0, 100.0, 50.0)).count()
        """
        edges = tuple(float(v) for v in box)
        if len(edges) != 4:
            raise Refusal(f"within takes (min_x, min_y, max_x, max_y); got {tuple(box)!r}")
        if edges[0] > edges[2] or edges[1] > edges[3]:
            raise Refusal(
                f"within: {edges} has a minimum above its maximum. Write "
                f"(min_x, min_y, max_x, max_y)"
            )
        return Selection(self._reader, self._view, self._filters, self._boxes + (edges,))

    def count(self) -> int:
        """How many items this reader may see in this selection.

        It counts every item in the view that the reader may see, that matches every filter,
        and that lies inside the box. With no filter and no box it is everything the
        reader may see in the view.

        A box whose outline is longer than the server's `max_region_cells` setting allows is
        counted over the grid cells covering it, so the number can include items just outside
        the box.

            db.view("papers").count()
            db.viewer(["cs.LG"]).view("papers").filter({"year": {"eq": 2023}}).count()
        """
        request = {"view": self._view, "zoom": 0, "tiles": [0], "k": 0}
        expression = self._expression()
        if expression is not None:
            request["filters"] = expression
        body = self._reader()._request("POST", "/v1/viewport", request)
        return _tile_counts(split_frames(body)).get("matched", 0)

    def sample(
        self,
        zoom: int = 0,
        k: Optional[int] = None,
        *,
        tiles: Optional[Sequence[int]] = None,
        highlight: Optional[dict] = None,
        layers: Any = None,
        levels: Any = None,
        computed: Optional[Sequence[str]] = None,
        artifact_budget: Optional[int] = None,
        artifact_rows: Optional[str] = None,
        point_rows: Optional[str] = None,
        underlay_offset: Optional[int] = None,
        pin: Any = None,
    ) -> Sample:
        """The points a map of this selection draws at `zoom`, as a table.

        This is a sample for drawing. Dense areas are thinned so that each map tile carries at
        most `k` points, and the table holds fewer rows than the selection has items. Use
        `count()` for how many items there are.

        - `zoom`: the map's zoom level, from 0 (the whole view as one tile) to 16. Each level
          splits every tile into four.
        - `k`: the most points drawn per tile. Leave it out for the server's setting.
        - `tiles`: sample these tiles only, each given by its number at this zoom in Z order.
          Without it the sample covers the box, or the whole view if there is no box.
        - `highlight`: a second filter. The points drawn are the same, and each carries a
          `highlighted` column saying whether it matches.
        - `layers`: which annotation layers to include, as a list of names, or `"all"`. A layer
          is a set of annotations over the items, such as clusters or regions. Without it the
          sample has none. `levels` chooses which levels of a layered hierarchy, `computed`
          which of `centroid`, `box` and `shape` to compute, `artifact_budget` the most
          annotations to return, and `artifact_rows="identity"` a short set of their columns.
        - `underlay_offset`: also count the items in tiles this many levels finer than `zoom`,
          returned as `sub_cells`.
        - `point_rows`: `"highlight"` returns each point as `tessera_id` and `highlighted`
          only, which is enough to update a highlight on points already held.
        - `pin`: the `x-tessera-pin` value from an earlier answer. The answer then says whether
          the data has changed since.

        Every option is sent only when given, so the server's own setting applies otherwise.

            sample = db.view("papers").sample(zoom=3, k=256)
            sample.to_pandas()  # needs pandas installed
        """
        reader = self._reader()
        request: dict = {"view": self._view, "zoom": int(zoom)}
        if tiles is not None:
            request["tiles"] = [int(tile) for tile in tiles]
        else:
            request["bbox"] = list(self.box or reader._extent(self._view))
        for name, value in (
            ("k", k),
            ("filters", self._expression()),
            ("highlight", highlight),
            ("layers", layers),
            ("levels", levels),
            ("computed", None if computed is None else list(computed)),
            ("artifact_budget", artifact_budget),
            ("artifact_rows", artifact_rows),
            ("point_rows", point_rows),
            ("underlay_offset", underlay_offset),
            ("pin", pin),
        ):
            if value is not None:
                request[name] = int(value) if name in _VIEWPORT_INTEGERS else value
        body = reader._request("POST", "/v1/viewport", request)
        frames = split_frames(body)
        artifacts = _tables([p for kind, p in frames if kind == FRAME_ARTIFACTS])
        sub_cells = _tables([p for kind, p in frames if kind == FRAME_SUB_CELLS])
        # A points frame with no rows is still the server's schema, so test for None, not falsity.
        points = _tables([p for kind, p in frames if kind == FRAME_POINTS])
        if points is None:
            points = _no_points()
        trailer = json.loads(next(p for kind, p in frames if kind == FRAME_TRAILER).decode())
        if points.num_rows != trailer.get("points", points.num_rows):
            raise Refusal(
                f"viewport: the trailer says {trailer['points']} points and the body carries "
                f"{points.num_rows}"
            )
        return Sample(
            points.replace_schema_metadata(
                {
                    "tessera.counts": json.dumps(_tile_counts(frames)),
                    "tessera.trailer": json.dumps(trailer),
                    "tessera.request": json.dumps(request),
                }
            ),
            artifacts,
            sub_cells,
        )

    def map(
        self,
        colour_by: Optional[str] = None,
        layers: Optional[Sequence[str]] = None,
        height: int = 480,
    ):
        """The interactive map of this selection, as a notebook widget.

        It opens on this view with this selection's filters applied, framed on its box if it
        has one. Items outside the box are still drawn when they are in frame.

        - `colour_by`: the column to colour points by, or `"cluster:<layer>"` to colour them by
          the clusters of that layer.
        - `layers`: the annotation layers to draw. `None` lets the map choose and `[]` draws
          none.
        - `height`: the widget's height in pixels.

            db.view("papers").filter({"year": {"range": {"gte": 2020}}}).map(colour_by="venue")
        """
        return self._reader().map(
            view=self._view,
            layers=layers,
            colour_by=colour_by,
            filters=self.filters,
            height=height,
            bbox=self.box,
        )

    def _expression(self) -> Optional[dict]:
        """The filters and every box, as the one expression the server tests items against."""
        boxes = [{"region": {"bbox": list(box)}} for box in self._boxes]
        return _all_of(list(self._filters) + boxes)

    def __repr__(self) -> str:
        parts = [f"view={self._view!r}"]
        if self._filters:
            parts.append(f"filters={self.filters!r}")
        if self._boxes:
            parts.append(f"box={self.box!r}")
        return f"Selection({', '.join(parts)})"


class Viewer:
    """A reader of one Tessera database: an address and a token that says what it may see.

    A reader holds a set of access terms, the labels its token grants. Each item carries labels
    too, and the reader sees an item when they share one. Every count, map and record a reader
    is given is computed over the items it may see, so two readers can get different answers
    from the same database.

    Get one with `connect(url, token)` for a database someone else runs, or with
    `db.viewer(terms)` for one of your own.

    - `url`: the address of the database's reading endpoint.
    - `token`: a token as a string, a `Token`, or a function that returns either. A function is
      called again when the token it gave is close to expiry.
    - `terms`: the access terms the token was made for, if known. It is kept for display.
    """

    def __init__(self, url: str, token: TokenSource, *, terms: Optional[Sequence[str]] = None):
        if not url:
            raise Refusal("a viewer needs the address of the database's reading endpoint")
        if token is None:
            raise Refusal(
                "a viewer needs a token: the token your deployment issued you, a Token, or a "
                "function returning one"
            )
        self.url = url.rstrip("/")
        self._source: TokenSource = token
        self._token: Optional[Token] = None
        #: The access terms this reader's token was made for, or `None` when the token came from
        #: elsewhere and does not say.
        self.terms = None if terms is None else list(terms)

    def token(self) -> Token:
        """The token this reader sends, replaced with a fresh one when it is close to expiry.

        A `Token` from `authorise` renews itself. A function given as the token is called again.
        A plain string is sent as it is until the server refuses it.
        """
        held = self._token
        if held is not None and (held.seconds_left is None or held.seconds_left > TOKEN_MARGIN):
            return held
        if held is not None and held.renew is not None:
            self._token = held.renew()
            return self._token
        self._token = minted(self._source)
        return self._token

    def view(self, name: str) -> Selection:
        """The whole of one view, as this reader sees it, to count, sample or map.

        `name` is a view's name as `meta()` lists it. A view in a view group is named
        `"<group>:<key>"`. A name this reader cannot see is refused, and the refusal lists the
        names it can.

            v.view("papers").count()
        """
        self._require_view(name)
        return Selection(lambda: self, name)

    def map(
        self,
        view: Optional[str] = None,
        layers: Optional[Sequence[str]] = None,
        colour_by: Optional[str] = None,
        filters: Optional[dict] = None,
        height: int = 480,
        **kwargs: Any,
    ):
        """The interactive map, as a notebook widget, showing what this reader may see.

        - `view`: the view to open on. `None` opens the first one.
        - `layers`: the annotation layers to draw. `None` lets the map choose and `[]` draws
          none.
        - `colour_by`: the column to colour points by, or `"cluster:<layer>"`.
        - `filters`: a filter expression to apply, as `Selection.filter` takes one.
        - `height`: the widget's height in pixels.

        Other keywords go to `Map` unchanged, such as `bbox` to frame the camera on a box. The
        page in the browser fetches its own data from the database with this reader's token. The
        token is sent to the page as a message and is never saved with the notebook.
        """
        from .widget import Map

        return Map(
            self.url,
            token=self.token,
            view=view,
            layers=layers,
            colour_by=colour_by,
            filters=filters,
            height=height,
            **kwargs,
        )

    def meta(self) -> dict:
        """What this reader may see of the database's structure, as a dictionary.

        It lists the views with their coordinate ranges, the annotation layers, the columns and
        how each can be filtered, and the server's limits. Anything the reader may not see is
        left out.
        """
        return json.loads(self._request("GET", "/v1/meta", None))

    def item(self, tessera_id: Any, idset: Optional[int] = None) -> dict:
        """One item's full record, if this reader may see it.

        - `tessera_id`: the item's id, as a sample's `tessera_id` column or a map pick gives it.
        - `idset`: the id numbering the id came from, `meta()["idset"]`. Ids are renumbered when
          the database is rebuilt with a new id key. Given this, an id from an older numbering is
          refused; without it, such an id may name a different item.

        The record has `fields` (the item's values by column name, missing where it has none),
        `labels` (the item's access labels that this reader also holds), `views` (the views
        this reader can find it in) and, where the item was inserted with one, `external_id`, as
        bytes. An item this reader may not see is refused exactly as one that does not exist.

            v.item(sample.column("tessera_id")[0].as_py())
        """
        body = {} if idset is None else {"idset": int(idset)}
        record = json.loads(self._request("POST", f"/v1/items/{tessera_id}", body))
        if record.get("external_id") is not None:
            record["external_id"] = base64.b64decode(record["external_id"])
        return record

    def categories(
        self,
        column: str,
        prefix: Optional[str] = None,
        view: Optional[str] = None,
        codes: Optional[Sequence[int]] = None,
    ):
        """The values of a category column that this reader may see, as a pyarrow table.

        A category column stores a small integer code for each value. This lists what each code
        stands for, one row per value, with the columns `key` (the value as inserted), `code`
        and `title` (the display name, or `None` where none was given).

        - `column`: the category column's name.
        - `prefix`: list only the values whose key or title, or a word in either, starts with
          this text, ignoring case. The rows then also have `count`, the number of
          items this reader may see that carry the value. The server returns at most its
          `max_suggestions` setting of such values; the table's schema metadata
          `tessera.more` is `"true"` when more matched than were returned.
        - `view`: the view to read the column in. A column declared for a view group holds
          different values in each of the group's views, so it needs this.
        - `codes`: list only these codes, such as the codes in a sample's category column. A
          code with no value this reader may see is left out. It cannot be combined with
          `prefix`.

        Without a prefix the rows are in key order; with one, in the order of the matched text.
        `.to_pandas()` on the table gives a DataFrame, where pandas is installed.

            v.categories("venue")
            v.categories("venue", prefix="neur")
            v.categories("venue", codes=sample.column("venue").unique().to_pylist())
        """
        import pyarrow as pa

        def table(values: list, counted: bool = False):
            columns = {
                "key": pa.array([one["key"] for one in values], pa.string()),
                "code": pa.array([one["code"] for one in values], pa.uint32()),
                "title": pa.array([one.get("title") for one in values], pa.string()),
            }
            if counted:
                columns["count"] = pa.array([one.get("count") for one in values], pa.uint64())
            return pa.table(columns)

        if codes is not None and prefix is not None:
            raise Refusal(
                "categories: codes= lists the values of codes you hold and prefix= searches the "
                "values by text. Give one of them"
            )
        path = f"/v1/categories/{urllib.parse.quote(column, safe='')}"
        query: dict = {} if view is None else {"view": view}
        if codes is not None:
            wanted = [str(int(code)) for code in codes]
            # An empty `codes=` on the wire lists the whole vocabulary, so ask nothing.
            values = []
            if wanted:
                query["codes"] = ",".join(wanted)
                values = json.loads(self._request("GET", path + _query(query), None))["values"]
            return table(values)
        if prefix is not None:
            query.update(q=prefix, counts="true")
            page = json.loads(self._request("GET", path + "/suggest" + _query(query), None))
            more = "true" if page.get("more") else "false"
            return table(page["values"], counted=True).replace_schema_metadata(
                {"tessera.more": more}
            )
        values: list = []
        while True:
            page = json.loads(self._request("GET", path + _query(query), None))
            values += page["values"]
            if page.get("next") is None:
                break
            query["after"] = page["next"]
        return table(values)

    def browse_artifacts(
        self,
        view: str,
        layer: str,
        level: Optional[int] = None,
        parent: Any = None,
        q: Optional[str] = None,
        filters: Optional[dict] = None,
        limit: Optional[int] = None,
        cursor: Optional[str] = None,
    ) -> dict:
        """One page of a layer's annotations, as this reader sees them.

        An annotation, or artifact, is one member of a layer: a cluster, a region, a category in
        a hierarchy. This lists them without regard to what is on screen.

        - `view`, `layer`: the view and the layer to list.
        - `level`: list only this level of a layered hierarchy.
        - `parent`: list the children of this annotation, by its `tessera_id`. Without it, and
          without `q`, the list is the top-level annotations.
        - `q`: list the annotations whose key or first text starts with this, ignoring case.
          It cannot be combined with `parent`.
        - `filters`: a filter expression. Each row then also has `matched_count`.
        - `limit`: the most rows on the page.
        - `cursor`: the `next` value of the previous page, to get the page after it.

        Each row has `masked_count`, the number of the annotation's items this reader may see.

            page = v.browse_artifacts("papers", "clusters")
            more = v.browse_artifacts("papers", "clusters", cursor=page["next"])
        """
        request: dict = {"view": view, "layer": layer}
        if level is not None:
            request["level"] = int(level)
        if parent is not None:
            request["parent"] = str(parent)
        if q is not None:
            request["q"] = q
        if filters is not None:
            request["filters"] = filters
        if limit is not None:
            request["limit"] = int(limit)
        if cursor is not None:
            request["cursor"] = cursor
        return json.loads(self._request("POST", "/v1/artifacts/browse", request))

    def artifact(
        self,
        tessera_id: Any,
        view: str,
        idset: Optional[int] = None,
        zoom: Optional[int] = None,
    ) -> dict:
        """One annotation's record, if this reader may see it.

        - `tessera_id`: the annotation's id, as `browse_artifacts` or a sample's `artifacts`
          table gives it.
        - `view`: the view to read it in.
        - `idset`: as for `item`.
        - `zoom`: the zoom level to simplify its outline for. Without it the full outline comes
          back.

        The record has `masked_count`, the number of its items this reader may see, and, where
        its layer computes them, `centroid`, `box` and `shape` in the view's grid coordinates,
        computed over those items alone. A property the layer does not compute is missing.

            v.artifact(page["artifacts"][0]["tessera_id"], "papers")
        """
        request: dict = {"view": view}
        if idset is not None:
            request["idset"] = int(idset)
        if zoom is not None:
            request["zoom"] = int(zoom)
        return json.loads(self._request("POST", f"/v1/artifacts/{tessera_id}", request))

    def _views(self) -> list[dict]:
        return list(self.meta().get("views") or [])

    def _require_view(self, name: str) -> None:
        names = [block["id"] for block in self._views()]
        if name not in names:
            raise Refusal(
                f"there is no view named {name!r} that this reader can see. The views are: "
                f"{', '.join(names) or 'none'}"
            )

    def _extent(self, view: str) -> list[float]:
        """A view's whole coordinate range, `[min_x, min_y, max_x, max_y]`."""
        for block in self._views():
            if block["id"] == view:
                q = block["quantisation"]
                return [q["x_min"], q["y_min"], q["x_max"], q["y_max"]]
        raise Refusal(f"there is no view named {view!r} that this reader can see")

    def _request(self, method: str, path: str, body: Optional[dict]) -> bytes:
        """One request: the answer's body, or a refusal with what the server said."""
        request = urllib.request.Request(
            self.url + path,
            data=None if body is None else json.dumps(body).encode(),
            method=method,
            headers={
                "authorization": f"Bearer {self.token().token}",
                **({} if body is None else {"content-type": "application/json"}),
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return response.read()
        except urllib.error.HTTPError as refused:
            detail = refused.read().decode(errors="replace")[:1000]
            raise Refusal(f"{method} {path} refused ({refused.code}): {detail}") from None

    def __repr__(self) -> str:
        terms = "" if self.terms is None else f", terms={self.terms!r}"
        return f"Viewer(url={self.url!r}{terms})"


def connect(url: str, token: TokenSource) -> Viewer:
    """A reader of a Tessera database someone else runs.

    - `url`: the address of its reading endpoint.
    - `token`: the token its operator issued you, as a string, a `Token`, or a function that
      returns either. A function is called again when its token is close to expiry.

    The reader can read and map. It cannot write, and it cannot read as anyone else, since both
    need credentials only the operator holds.

        v = tesseradb.connect("https://maps.example/viewer", token=my_token)
        v.view("papers").count()
    """
    return Viewer(url, token)
