"""Reading a Tessera database: the widget and the query verbs, over the viewer plane (§8).

A `Viewer` is a viewer-plane URL and a token source, and nothing else. It holds no bundle, no
schema and no id map: every answer here is a request the server authorises, so a local database
and a hosted deployment are read by the same three verbs and neither reads a file. The token is
the whole of the authority. What `meta()` names, what `viewport()` counts and what `item()`
returns are computed inside the principal's mask (I2), so two viewers over one database
legitimately disagree.

`connect(url, token)` is the hosted form: a token the deployment issued, as a string, a `Token`
or a callable returning either. It has no `viewer(terms)`, minting for another principal needing
the session credential a hosted analyst does not hold, and no write verb, the control plane
having one operator credential and no per-principal authority (§1).

`Database.viewer(terms)` is the local form, whose token source mints from the directory's session
credential through `authorise`. The credential stays in the kernel: the source is a closure the
`Map` calls, and what reaches the page is the minted token, as a custom message that is never
widget state (client-components §7).
"""

from __future__ import annotations

import base64
import io
import json
import struct
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Optional, Sequence

from ._auth import Token, TokenSource, minted
from ._refusal import Refusal

#: How near expiry a held token may come before the next read mints another, in seconds.
TOKEN_MARGIN = 60.0

#: The fields of a `/v1/viewport` body that the wire carries as integers.
_VIEWPORT_INTEGERS = {"k", "artifact_budget", "underlay_offset"}

#: The frame kinds of a `/v1/viewport` body (contracts §3.2). The decoder below refuses one it
#: does not know rather than skipping it: a future kind carrying data an old reader drops would
#: be a sample standing in for the set.
FRAME_TILES = 1
FRAME_SUB_CELLS = 2
FRAME_POINTS = 3
FRAME_TRAILER = 4
FRAME_ARTIFACTS = 5
_KINDS = {FRAME_TILES, FRAME_SUB_CELLS, FRAME_POINTS, FRAME_TRAILER, FRAME_ARTIFACTS}

def split_frames(body: bytes) -> list[tuple[int, bytes]]:
    """A framed viewport body as `(kind, payload)`, refusing anything that is not one.

    The framing is `u8 kind`, `u32` little-endian length, payload, repeated; every payload but the
    trailer's is a complete Arrow IPC stream (contracts §3.2). Truncation, an unknown kind, a
    misplaced tiles frame and a missing trailer all raise. A truncated body must never decode as a
    plausible shorter response: the trailer's presence is the completeness signal, and a reader
    that accepted the prefix would present a sample as the set.
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


def _first(views: Sequence[dict]) -> str:
    """The first view this principal is served, which is what `view=None` means."""
    if not views:
        raise Refusal(
            "viewport: this principal is served no view, so there is nothing to ask about"
        )
    return views[0]["id"]


def _extent(views: Sequence[dict], view: str) -> list[float]:
    """A view's whole declared extent, which is what `bbox=None` means."""
    for block in views:
        if block["id"] == view:
            q = block["quantisation"]
            return [q["x_min"], q["y_min"], q["x_max"], q["y_max"]]
    raise Refusal(f"viewport: this principal is served no view named {view!r}")


def _tables(payloads: Sequence[bytes]):
    """The Arrow IPC streams of one frame kind, concatenated in arrival order.

    Every frame carries the same schema by construction, and the pieces concatenate to the whole
    surface: a points frame is a chunk of the points stream, not a stream of its own. `None` where
    the response carried no frame of that kind at all.
    """
    import pyarrow as pa
    import pyarrow.ipc as ipc

    tables = [ipc.open_stream(io.BytesIO(payload)).read_all() for payload in payloads]
    return pa.concat_tables(tables) if tables else None


def _no_points():
    """The points table of a response that served none: the two fixed columns, no rows.

    The rendered columns are not invented for it. A caller reading a column name off an empty
    table would be reading this decoder's guess rather than the schema, and `meta()` is where the
    schema is.
    """
    import pyarrow as pa

    return pa.table({"tessera_id": pa.array([], pa.uint64()), "code": pa.array([], pa.uint64())})


def _query(params: dict) -> str:
    """A query string, or nothing at all where no parameter was given."""
    return "?" + urllib.parse.urlencode(params) if params else ""


class Viewport:
    """One `/v1/viewport` response: the points table, and the other frames beside it.

    Every attribute, index and length reaches the points table, so this is the points table
    wherever one is expected — `num_rows`, `column()`, `schema.metadata` and `to_pandas()` all
    read it, and `points` names it outright.

    `artifacts` is the annotation artifacts the response served, as a pyarrow table, and
    `sub_cells` the exact counts a non-zero `underlay_offset` asked for. Each is `None` where the
    response carried no frame of that kind, which the wire makes an absence rather than an empty
    table: a layer that served nothing sends no frame at all.
    """

    def __init__(self, points, artifacts=None, sub_cells=None):
        self.points = points
        self.artifacts = artifacts
        self.sub_cells = sub_cells

    def __getattr__(self, name: str):
        # Named here and not yet set: the points table is what `__init__` assigns first, and
        # delegating before it exists would recur until the stack ran out.
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
        return f"Viewport({', '.join(served)})"


class Viewer:
    """One principal's reading of one Tessera database (§8).

    `map()` is the widget of client-components §7, pointed here with this viewer's token source.
    The verbs beside it — `meta()`, `viewport()`, `item()`, `categories()`,
    `suggest_category_values()`, `browse_artifacts()` and `artifact()` — are the viewer plane's
    own, one method each, answered with the token and never by reading a bundle.
    """

    def __init__(self, url: str, token: TokenSource, *, terms: Optional[Sequence[str]] = None):
        if not url:
            raise Refusal("a viewer needs the viewer plane's URL")
        if token is None:
            raise Refusal(
                "a viewer needs a token: the viewer token your deployment issued you, a Token, or "
                "a callable returning one"
            )
        self.url = url.rstrip("/")
        #: The token source, held in this process and never in widget state.
        self._source: TokenSource = token
        self._token: Optional[Token] = None
        #: The terms this viewer was minted for, where it was minted here; `None` where the token
        #: came from a deployment, which does not tell a holder what it grants.
        self.terms = None if terms is None else list(terms)

    # ---- the token --------------------------------------------------------------------------

    def token(self) -> Token:
        """The token these reads present, minted again when the one held is near expiry.

        A `Token` from `authorise` renews itself with the credential that minted it, which stays
        wherever it was; a callable is called again; a bare string is what it is, and a server
        that refuses it says so on the read.
        """
        held = self._token
        if held is not None and (held.seconds_left is None or held.seconds_left > TOKEN_MARGIN):
            return held
        if held is not None and held.renew is not None:
            self._token = held.renew()
            return self._token
        self._token = minted(self._source)
        return self._token

    # ---- the widget -------------------------------------------------------------------------

    def map(
        self,
        view: Optional[str] = None,
        layers: Optional[Sequence[str]] = None,
        colour_by: Optional[str] = None,
        filters: Optional[dict] = None,
        height: int = 480,
        **kwargs: Any,
    ):
        """The explorer in this cell, against this viewer plane as this principal (§8).

        The widget is client-components §7's, unchanged. What crosses the kernel boundary is
        control and selection, never data: the page fetches from the viewer plane itself with the
        token this viewer's source mints, and asks for another before expiry. The token is a
        custom message and no traitlet carries it, so nothing that saves widget state saves it.
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

    # ---- the query verbs --------------------------------------------------------------------

    def meta(self) -> dict:
        """`GET /v1/meta` as this principal reads it: the views, the layers and the schema.

        Every list here is already inside the mask. A view, a group, a layer or a vocabulary this
        principal cannot reach is absent from it, with nothing in its place.
        """
        return json.loads(self._request("GET", "/v1/meta", None))

    def item(self, tessera_id: Any, idset: Optional[int] = None) -> dict:
        """`POST /v1/items/{tessera_id}`: the drill-down record for one item.

        `fields` is the record by declared column name, absent where the item has no value.
        `labels` is the item's own labels intersected with this session's satisfied set, never the
        full set (decision 0114), and `views` is the views this principal may reach it in. An item
        this principal may not see is not found, on the refusal one that does not exist gets.

        `external_id` is present only where the caller supplied one, and it is bytes here. The
        wire carries base64, an external id being bytes rather than text, and this decodes it. A
        `Database` goes one step further and reads those bytes as the type its id column carried.

        `idset` is the partitioning the id was minted under (the `idset` of `meta()`). A
        `tessera_id` is durable only within one: omitting it accepts that an id from a past idset
        may now name a different item (contracts §2.6).
        """
        body = {} if idset is None else {"idset": int(idset)}
        record = json.loads(self._request("POST", f"/v1/items/{tessera_id}", body))
        if record.get("external_id") is not None:
            record["external_id"] = base64.b64decode(record["external_id"])
        return record

    def viewport(
        self,
        bbox: Optional[Sequence[float]] = None,
        view: Optional[str] = None,
        filters: Optional[dict] = None,
        k: Optional[int] = None,
        zoom: int = 0,
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
    ) -> "Viewport":
        """`POST /v1/viewport`: the points, artifacts and counts this principal is served.

        `bbox` is `[x0, y0, x1, y1]` in the view's own extent, and `None` is the whole of it as
        `meta()` declares it; `tiles` is a list of depth-`zoom` Morton prefixes in its place, and
        the two are exclusive. `view` is a view id, and `None` the first this principal is served.
        `zoom` is the tile depth, 0-16, and `k` the per-tile mark budget, whose absence takes the
        deployment's and whose `0` is the counts-only request. `filters` is the wire's filter
        expression and `highlight` a second expression in the same grammar, which lights the
        served set without moving it.

        `layers` names the annotation layers to answer for — `"all"`, or a list of names — and
        `levels`, `computed`, `artifact_budget` and `artifact_rows` say which of their rungs,
        which derived properties, how many artifacts and which columns come back.
        `underlay_offset` asks for exact counts at `zoom + underlay_offset` as the sub-cells
        frame, `point_rows` projects the points to the highlight bit, and `pin` is the generation
        stamp a previous response's `x-tessera-pin` header carried. Each is sent only where it was
        given: what the deployment does with a field nobody named is the deployment's.

        The result is a `Viewport`. It reads as the points table it always did — `tessera_id`,
        `code` and the columns the schema declares as rendered — and carries `artifacts` and
        `sub_cells` beside it. They are points; `item()` is where a record is, and `artifact()`
        where one annotation's own record is. A served set is bounded by `k`, so the counts
        travel with the table in its schema metadata, each of them a per-request fact about this
        principal's mask:

        - `tessera.counts`: the tiles frame summed. `visible` is inside the mask and the tiles the
          box touches at this zoom, `matched` is that and the filter, `highlighted` that and the
          highlight, `served` is that and the budget.
        - `tessera.trailer`: the response's trailer, whose presence is what marks it complete.
        - `tessera.request`: the body this sent, so a table in a later cell says what it is.
        """
        if bbox is not None and tiles is not None:
            raise Refusal(
                "viewport: a request names a bbox or a list of tiles, never both. Drop one"
            )
        # One `/v1/meta`, whichever of the two a caller left out: both are answered from the
        # same document, and two reads could answer from two.
        views = self._views() if view is None or (bbox is None and tiles is None) else []
        request: dict = {"view": view or _first(views), "zoom": int(zoom)}
        if tiles is not None:
            request["tiles"] = [int(tile) for tile in tiles]
        else:
            request["bbox"] = [
                float(v) for v in (bbox if bbox is not None else _extent(views, request["view"]))
            ]
        for name, value in (
            ("k", k),
            ("filters", filters),
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
        frames = split_frames(self._request("POST", "/v1/viewport", request))

        counted = _tables([p for kind, p in frames if kind == FRAME_TILES])
        artifacts = _tables([p for kind, p in frames if kind == FRAME_ARTIFACTS])
        sub_cells = _tables([p for kind, p in frames if kind == FRAME_SUB_CELLS])
        # `is None` and not a truth test: a zero-row table is falsey, and a frame that arrived
        # carrying a schema and no rows is the server's schema, not this decoder's stand-in.
        points = _tables([p for kind, p in frames if kind == FRAME_POINTS])
        if points is None:
            points = _no_points()
        trailer = json.loads(next(p for kind, p in frames if kind == FRAME_TRAILER).decode())
        counts = {
            name: sum(int(v) for v in counted.column(name).to_pylist())
            for name in ("visible", "matched", "highlighted", "served")
            if name in counted.column_names
        }
        if points.num_rows != trailer.get("points", points.num_rows):
            raise Refusal(
                f"viewport: the trailer says {trailer['points']} points and the body carries "
                f"{points.num_rows}"
            )
        return Viewport(
            points.replace_schema_metadata(
                {
                    "tessera.counts": json.dumps(counts),
                    "tessera.trailer": json.dumps(trailer),
                    "tessera.request": json.dumps(request),
                }
            ),
            artifacts,
            sub_cells,
        )

    def categories(
        self,
        column: str,
        codes: Optional[Sequence[int]] = None,
        after: Optional[str] = None,
        limit: Optional[int] = None,
        view: Optional[str] = None,
    ) -> dict:
        """`GET /v1/categories/{column}`: what a category column's codes stand for.

        Two forms. `codes` resolves codes this caller already holds; without it the route
        enumerates one page of the vocabulary ascending by value key, resumed by handing the
        answer's `next` back as `after` and bounded by `limit`. A code no visible value explains
        is omitted from `values` rather than refused, so a shorter list is an answer and not a
        failure.

        `view` is the request's own view, which is what answers a group-scoped category: the
        route resolves the view before the column, whatever the column's scope. A scoped column
        can also be pinned in `column` itself, as `<column>@<key>`.
        """
        if codes is not None and not list(codes):
            # `codes=` empty is the enumeration form on the wire, which would fetch the whole
            # vocabulary — the opposite of what an empty list asked for.
            return {"values": [], "next": None}
        query: dict = {}
        if codes is not None:
            query["codes"] = ",".join(str(int(code)) for code in codes)
        if after is not None:
            query["after"] = after
        if limit is not None:
            query["limit"] = int(limit)
        if view is not None:
            query["view"] = view
        path = f"/v1/categories/{urllib.parse.quote(column, safe='')}"
        return json.loads(self._request("GET", path + _query(query), None))

    def suggest_category_values(
        self,
        column: str,
        q: str,
        limit: Optional[int] = None,
        counts: Optional[bool] = None,
        view: Optional[str] = None,
    ) -> dict:
        """`GET /v1/categories/{column}/suggest`: the typeahead over a category vocabulary.

        The values whose folded key, folded title or a word start of either has `q` as a prefix,
        ordered by the matched text and never by frequency, recency or count. `q` is echoed on the
        answer exactly as sent, so a caller can tell which request a page belongs to. `counts`
        serves the number of items carrying each value that this principal may see, and never
        orders the page.

        There is no cursor: a typeahead pages by the user typing another character, and `more`
        says the walk stopped early. One suggest is in flight per session, so a second sent while
        one is running is refused `429`, and the answer to that is to send it again.
        """
        query: dict = {"q": q}
        if limit is not None:
            query["limit"] = int(limit)
        if counts:
            query["counts"] = "true"
        if view is not None:
            query["view"] = view
        path = f"/v1/categories/{urllib.parse.quote(column, safe='')}/suggest"
        return json.loads(self._request("GET", path + _query(query), None))

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
        """`POST /v1/artifacts/browse`: one page of a layer's hierarchy, by lineage.

        Independent of the viewport: no box, no tiles and no zoom, so a layer spread across the
        map is read here whatever the viewport's budget cut reaches. Three forms — neither
        `parent` nor `q` is the roots, `parent` the artifacts naming it, `q` a case-insensitive
        search over a key or a first supplied text content — and both together is refused.

        Each row carries `masked_count`, which is this principal's count of the artifact's members
        and never its size, and under `filters` a `matched_count` beside it. `view` is required:
        a masked count is an intersection in row space, and row space is per view. Page with the
        answer's `next` as `cursor`.
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
        """`POST /v1/artifacts/{tessera_id}`: one annotation artifact's own record.

        A separate route from `item()`, so the shape of an answer never says which of the two an
        identifier names. `masked_count` is this principal's count of the artifact's members and
        never its size, and the geometry — `centroid`, `box`, `shape` — is recomputed per
        principal in the same grid units the viewport's `code` carries, so one principal's shape
        must not be held against the identifier for another. A geometry field that is absent says
        the layer declares no such property, never that it was withheld.

        `view` is required, as it is on `browse_artifacts` and unlike on `item()`. `zoom` is the
        depth the caller draws at, under which a predicate or an authored shape is generalised;
        absent serves the whole presimplified shape.
        """
        request: dict = {"view": view}
        if idset is not None:
            request["idset"] = int(idset)
        if zoom is not None:
            request["zoom"] = int(zoom)
        return json.loads(self._request("POST", f"/v1/artifacts/{tessera_id}", request))

    # ---- the plane --------------------------------------------------------------------------

    def _views(self) -> list[dict]:
        return list(self.meta().get("views") or [])

    def _request(self, method: str, path: str, body: Optional[dict]) -> bytes:
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
    """Read a deployment somebody else runs (§8, §10.6).

    `token` is the viewer token that deployment issued you: a string, a `Token`, or a callable
    returning either, which is called again when the one it gave expires. There is no
    `viewer(terms)` here and no write verb. Minting another principal's token needs the session
    credential and writing needs the control plane's operator credential, neither of which a
    deployment hands an analyst.
    """
    return Viewer(url, token)
