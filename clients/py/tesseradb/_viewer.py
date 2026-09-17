"""Reading a Tessera database: the widget and the query verbs, over the viewer plane (§8).

A `Viewer` is a viewer-plane URL and a token source, and nothing else. It holds no bundle, no
schema and no id map: every answer here is a request the server authorises, so a local database
and a hosted deployment are read by the same three verbs and neither reads a file. The token is
the whole of the authority — what `meta()` names, what `viewport()` counts and what `item()`
returns are computed inside the principal's mask (**I2**), so two viewers over one database
legitimately disagree.

`connect(url, token)` is the hosted form: a token the deployment issued, as a string, a `Token`
or a callable returning either. It has no `viewer(terms)` — minting for another principal needs
the session credential, which a hosted analyst does not hold — and no write verb, the control
plane having one operator credential and no per-principal authority (§1).

`Database.viewer(terms)` is the local form, whose token source mints from the directory's session
credential through `authorise`. **The credential stays in the kernel**: the source is a closure
the `Map` calls, and what reaches the page is the minted token, as a custom message that is never
widget state (client-components §7).
"""

from __future__ import annotations

import io
import json
import struct
import urllib.error
import urllib.request
from typing import Any, Callable, Optional, Sequence, Union

from ._auth import Token
from ._refusal import Refusal

#: How near expiry a held token may come before the next read mints another, in seconds.
TOKEN_MARGIN = 60.0

#: The frame kinds of a `/v1/viewport` body (contracts §3.2). The decoder below refuses one it
#: does not know rather than skipping it: a future kind carrying data an old reader drops would
#: be a sample standing in for the set.
FRAME_TILES = 1
FRAME_SUB_CELLS = 2
FRAME_POINTS = 3
FRAME_TRAILER = 4
FRAME_ARTIFACTS = 5
_KINDS = {FRAME_TILES, FRAME_SUB_CELLS, FRAME_POINTS, FRAME_TRAILER, FRAME_ARTIFACTS}

TokenSource = Union[str, Token, Callable[[], Union[str, Token]]]


def split_frames(body: bytes) -> list[tuple[int, bytes]]:
    """A framed viewport body as `(kind, payload)`, refusing anything that is not one.

    The framing is `u8 kind`, `u32` little-endian length, payload, repeated; every payload but the
    trailer's is a complete Arrow IPC stream (contracts §3.2). Truncation, an unknown kind, a
    misplaced tiles frame and a missing trailer all raise, because **a truncated body must never
    decode as a plausible shorter response**: the trailer's presence is the completeness signal,
    and a reader that accepted the prefix would present a sample as the set.
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
    """The Arrow IPC streams of one frame kind, concatenated in arrival order.

    Every frame carries the same schema by construction, and the pieces concatenate to the whole
    surface: a points frame is a chunk of the points stream, not a stream of its own. No frame at
    all is a table with no columns rather than an invented schema — the response named none.
    """
    import pyarrow as pa
    import pyarrow.ipc as ipc

    tables = [ipc.open_stream(io.BytesIO(payload)).read_all() for payload in payloads]
    return pa.concat_tables(tables) if tables else pa.table({})


class Viewer:
    """One principal's reading of one Tessera database (§8).

    `map()` is the widget of client-components §7, pointed here with this viewer's token source.
    `meta()`, `item()` and `viewport()` are the first cut of the query verbs (issue #47): they
    answer through the viewer plane with the token, never by reading a bundle.
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
        source = self._source
        got = source() if callable(source) and not isinstance(source, Token) else source
        if isinstance(got, str):
            got = Token(got)
        if not isinstance(got, Token) or not got.token:
            raise Refusal(
                f"a token must be a string, a Token or a callable returning one; got {got!r}"
            )
        self._token = got
        return got

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

        Every list here is already inside the mask — a view, a group, a layer or a vocabulary this
        principal cannot reach is absent, not marked.
        """
        return json.loads(self._request("GET", "/v1/meta", None))

    def item(self, tessera_id: Any, idset: Optional[int] = None) -> dict:
        """`POST /v1/items/{tessera_id}`: the drill-down record for one item.

        `fields` is the record by declared column name, absent where the item has no value;
        `external_id` is base64 and present only where the caller supplied one; `labels` is the
        item's own labels intersected with this session's satisfied set, never the full set
        (decision 0114); `views` is the views this principal may reach it in. An item this
        principal may not see is not found, on the refusal one that does not exist gets.

        `idset` is the partitioning the id was minted under (the `idset` of `meta()`). A
        `tessera_id` is durable only within one: omitting it accepts that an id from a past idset
        may now name a different item (contracts §2.6).
        """
        body = {} if idset is None else {"idset": int(idset)}
        return json.loads(self._request("POST", f"/v1/items/{tessera_id}", body))

    def viewport(
        self,
        bbox: Optional[Sequence[float]] = None,
        view: Optional[str] = None,
        filters: Optional[dict] = None,
        k: Optional[int] = None,
        zoom: int = 0,
        underlay_offset: int = 0,
    ):
        """`POST /v1/viewport`: the points this principal is served, as a pyarrow table.

        `bbox` is `[x0, y0, x1, y1]` in the view's own extent, and `None` is the whole of it as
        `meta()` declares it; `view` is a view id, and `None` the first this principal is served.
        `zoom` is the tile depth, 0–16, and `k` the per-tile mark budget, whose absence takes the
        deployment's. `filters` is the wire's filter expression.

        The table's columns are `tessera_id`, `code` and the columns the schema declares as
        rendered — a point, not a record: `item()` is the record. **A served set is not the whole
        set**: `k` bounds how many marks a tile carries, and the counts are what says how many
        there were. So the table's schema metadata carries them, all of them per-request facts
        about this principal's mask:

        - `tessera.counts`: the tiles frame summed — `visible` (inside the mask and the tiles),
          `matched` (and the filter), `served` (and the budget).
        - `tessera.trailer`: the response's trailer, whose presence is what marks it complete.
        - `tessera.request`: the body this sent, so a table in a later cell says what it is.
        """
        request: dict = {"view": view or self._first_view(), "zoom": int(zoom)}
        request["bbox"] = [float(v) for v in (bbox if bbox is not None else self._extent(request["view"]))]
        if k is not None:
            request["k"] = int(k)
        if filters is not None:
            request["filters"] = filters
        if underlay_offset:
            request["underlay_offset"] = int(underlay_offset)
        frames = split_frames(self._request("POST", "/v1/viewport", request))

        tiles = _tables([p for kind, p in frames if kind == FRAME_TILES])
        points = _tables([p for kind, p in frames if kind == FRAME_POINTS])
        trailer = json.loads(next(p for kind, p in frames if kind == FRAME_TRAILER).decode())
        counts = {
            name: sum(int(v) for v in tiles.column(name).to_pylist())
            for name in ("visible", "matched", "served")
            if name in tiles.column_names
        }
        if points.num_rows != trailer.get("points", points.num_rows):
            raise Refusal(
                f"viewport: the trailer says {trailer['points']} points and the body carries "
                f"{points.num_rows}"
            )
        return points.replace_schema_metadata(
            {
                "tessera.counts": json.dumps(counts),
                "tessera.trailer": json.dumps(trailer),
                "tessera.request": json.dumps(request),
            }
        )

    # ---- the plane --------------------------------------------------------------------------

    def _first_view(self) -> str:
        views = self.meta().get("views") or []
        if not views:
            raise Refusal(
                "viewport: this principal is served no view, so there is nothing to ask about"
            )
        return views[0]["id"]

    def _extent(self, view: str) -> list[float]:
        for block in self.meta().get("views") or []:
            if block["id"] == view:
                q = block["quantisation"]
                return [q["x_min"], q["y_min"], q["x_max"], q["y_max"]]
        raise Refusal(f"viewport: this principal is served no view named {view!r}")

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

    `token` is the viewer token that deployment issued you — a string, a `Token`, or a callable
    returning either, which is called again when the one it gave expires. There is no
    `viewer(terms)` here and no write verb: minting another principal's token needs the session
    credential, and writing needs the control plane's operator credential, neither of which a
    deployment hands an analyst.
    """
    return Viewer(url, token)
