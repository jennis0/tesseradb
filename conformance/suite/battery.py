"""The read battery — correctness-suite §3's query set, as one shared artefact.

A battery is a list of queries and a recorded response per query (§12.2):

    Query    = Meta | Categories | Suggest | Viewport | Aggregate | Item | Browse | ArtifactCard
    Recorded = dict[Query, Canonical]

The membership is §3's table, and it is small because the query surface is deliberately small —
the same property that makes the leak register enumerable. Every served surface is here: the
schema (`/v1/meta`), a category's values (`/v1/categories/{column}`) and its typeahead
(`/suggest`), the viewport's streamed surfaces (tiles, points and underlay, with a filter or a
highlight where asked — the underlay must be *requested*, since at `underlay_offset = 0` it emits
nothing and silently drops out of every comparison) and, where `layers` names some, the
artifacts the same request is served from `/v1/artifacts/viewport`, the counts by group over a drawn region
(`/v1/aggregate`), the drill-down
(`/v1/items/{id}`), which earns its place twice over: it is the only surface that reads all three
homes, so a blob-resident field dropped by a producer is visible nowhere else, and a layer's
browse page (`/v1/artifacts/browse`) and one artifact's card (`/v1/artifacts/{id}`).

The region breakdown the correctness suite named is served by `/v1/aggregate`: one query with a
region as its set, the whole visible set as its reference, and a size, a value breakdown, a
density surface and a density per value as its groupings.

**The battery always carries one viewport deep enough to hold saturated tiles** ([`DEEP_ZOOM`]),
because the stage-invariance comparison needs a surface where membership is comparable and
saturation cannot be arranged — it is a per-tile fact of each response, not a configuration
(`suite.entitlement`'s module doc; correctness-suite §10). Selection truncates per tile once a
tile's visible count passes the mark budget, and a truncated tile supports only count
comparison: at ten million items the whole-extent tile is a million-mark window over ten million
visible rows, and rows displaced out of that window read as vanished. Depth divides the load —
a zoom-*z* full-extent viewport spreads the corpus over 4^*z* tiles — so the deep viewport's
tiles stay saturated roughly a thousandfold past the corpus that caps the zoom-0 one, and the
diff keeps a membership surface while the shallow viewports fall back to counts. When even the
deep tiles truncate, the diff reports the recording as uncheckable for point sets rather than
quietly narrowing to counts; the remedy is a deeper battery, and this constant is where it goes.

Queries are frozen and hashable — they are `Recorded`'s keys, and a recording is compared against
another recording *of the same query* by lookup, never by position. `filters` is therefore carried
as canonical JSON text rather than a dict; [`build_battery`] does the encoding and the recorder
decodes it back at the wire.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Iterable, Sequence, Union

import requests

from .canonical import Canonical, Json, canonicalise_viewport, table_rows


@dataclass(frozen=True)
class Meta:
    """`GET /v1/meta` — the schema and operand list. Absence from the battery would hide a
    producer that dropped a declared column from the manifest (§3)."""


@dataclass(frozen=True)
class Categories:
    """`GET /v1/categories/{column}`, the bare enumerating form — a category's values. Absence
    would hide a vocabulary extension lost at a flush or not carried by a fold (§3)."""

    column: str


@dataclass(frozen=True)
class Viewport:
    """`POST /v1/viewport` — tiles, points and the density underlay, three surfaces in one
    response (contracts §3.2). Exactly one of `bbox`/`tiles` (the contract's two request forms).

    `underlay_offset` is not in §12.2's parameter sketch but is load-bearing here: the underlay
    surface exists only when requested, and a battery whose viewports never ask for it compares
    two of three surfaces while reading as if it compared all of them — the exact silent narrowing
    the canary suite measured before its comparator split surfaces apart.
    """

    view_id: str
    zoom: int
    bbox: tuple[float, float, float, float] | None = None
    tiles: tuple[int, ...] | None = None
    k: int | None = None
    #: Canonical JSON (`json.dumps(..., sort_keys=True)`) or None — text so the query is hashable.
    filters: str | None = None
    underlay_offset: int = 0
    #: Canonical JSON for the `highlight` expression, or None.
    highlight: str | None = None
    #: Canonical JSON for `layers` (`"all"` or a list of names), or None for none: the layers that
    #: tag the points, and whose artifacts the same request is read for.
    layers: str | None = None

    def __post_init__(self):
        if (self.bbox is None) == (self.tiles is None):
            raise ValueError("exactly one of bbox/tiles — the contract's own rule (§3.2)")


@dataclass(frozen=True)
class Aggregate:
    """`POST /v1/aggregate`: counts by group over a set, compared with a reference. Every field
    but `view_id` is canonical JSON text, as on [`Viewport`]; `reference` `"{}"` is the whole
    visible set."""

    view_id: str
    groupings: str
    filters: str | None = None
    reference: str | None = None


@dataclass(frozen=True)
class Item:
    """`POST /v1/items/{tessera_id}` — the whole record, assembled from all three homes. Nothing
    else reads the record blob on the viewer plane, so this is the only surface where a
    blob-resident field dropped by a producer is visible at all (§3)."""

    tessera_id: int


@dataclass(frozen=True)
class Suggest:
    """`GET /v1/categories/{column}/suggest`: the typeahead over a category's values, with the
    per-principal counts when `counts` is set."""

    column: str
    q: str
    counts: bool = False


@dataclass(frozen=True)
class Browse:
    """`POST /v1/artifacts/browse`: one page of a layer's hierarchy. The roots form when `parent`
    and `q` are both None, the children form with a `parent` tessera id, the search form with `q`."""

    view_id: str
    layer: str
    parent: int | None = None
    q: str | None = None
    #: Canonical JSON, as on [`Viewport`].
    filters: str | None = None


@dataclass(frozen=True)
class ArtifactCard:
    """`POST /v1/artifacts/{tessera_id}`: one artifact as this principal sees it."""

    tessera_id: int
    view_id: str


Query = Union[Meta, Categories, Suggest, Viewport, Aggregate, Item, Browse, ArtifactCard]


Battery = tuple[Query, ...]

Recorded = dict[Query, Canonical]

#: The depth of the battery's membership surface (module doc): 1,024 full-extent tiles, each
#: holding 1/1024th of the corpus, so per-tile saturation survives three orders of magnitude
#: past the whole-extent viewport's ceiling. Five is also the deepest full-extent viewport whose
#: underlay fits the engine's default cell budget — 1,024 tiles × 4^offset cells must stay under
#: `max_underlay_cells` (8,192), which is why the deep viewport requests ``underlay_offset = 1``
#: (4,096 cells) rather than the battery's usual 2 (16,384 would be refused at the door).
DEEP_ZOOM = 5


def build_battery(
    meta: dict,
    *,
    view_id: str,
    item_ids: Sequence[int],
    bbox: tuple[float, float, float, float],
    zooms: Sequence[int] = (0, 3),
    k: int | None = None,
    underlay_offset: int = 2,
    filters: dict | None = None,
) -> Battery:
    """The battery for one deployment: every §3 surface, parameterised by what the bundle offers.

    `meta` is the server's own `/v1/meta` answer — the category columns are read from its
    `declared_scalars`, so the battery covers whatever vocabulary surfaces the deployment
    declares rather than a hard-coded list that rots. `item_ids` must come from served responses
    (they are per-build `tessera_id`s and nothing may persist them across builds — the identity
    key is minted per fixture); at least one is required, because a battery without the
    drill-down has no reader of the record blob at all. `filters`, when given, adds one filtered
    viewport beside the unfiltered ones rather than replacing them — a battery whose every
    viewport is filtered never exercises the unfiltered `matched = visible` surface. A
    [`DEEP_ZOOM`] viewport rides every battery whose own `zooms` stop short of it — the
    membership surface the module doc argues.
    """
    if not item_ids:
        raise ValueError(
            "a battery needs at least one item id: /v1/items is the only surface that reads "
            "all three homes (§3), and a battery without it cannot see a dropped blob field"
        )
    if underlay_offset < 1:
        raise ValueError(
            "the underlay must be requested (underlay_offset >= 1): at 0 no kind-2 frame is "
            "emitted and the underlay surface silently drops out of every comparison"
        )

    category_columns = sorted(
        d["name"] for d in meta.get("declared_scalars", []) if d.get("category") is not None
    )

    entries: list[Query] = [Meta()]
    entries += [Categories(column) for column in category_columns]
    entries += [
        Viewport(view_id, zoom, bbox=bbox, k=k, underlay_offset=underlay_offset)
        for zoom in zooms
    ]
    if max(zooms) < DEEP_ZOOM:
        # The membership surface (module doc): deep enough that its tiles stay saturated —
        # observed per response, never arranged — after the shallow viewports truncate.
        entries.append(
            Viewport(view_id, DEEP_ZOOM, bbox=bbox, k=k, underlay_offset=1)
        )
    if filters is not None:
        entries.append(
            Viewport(
                view_id,
                zooms[0],
                bbox=bbox,
                k=k,
                filters=json.dumps(filters, sort_keys=True, separators=(",", ":")),
                underlay_offset=underlay_offset,
            )
        )
    entries.append(_aggregate(meta, view_id, bbox))
    entries += [Item(tessera_id) for tessera_id in item_ids]
    return tuple(entries)


def _aggregate(meta: dict, view_id: str, bbox: tuple[float, float, float, float]) -> Aggregate:
    """The counts over the left half of `bbox` against everything visible: the size, the first
    countable category's values by count with and without cells, and a density surface."""
    countable = sorted(
        d["name"]
        for d in meta.get("declared_scalars", [])
        if d.get("category") is not None
        and (d.get("index") or d.get("render") or d["category"].get("visibility") == "derived")
    )
    groupings: list[dict] = [{}, {"cells": {"depth": 4}}]
    if countable:
        groupings += [
            {"by": {"field": countable[0], "top": 5}},
            {"by": {"field": countable[0], "top": 3}, "cells": {"depth": 2}},
        ]
    x0, y0, x1, y1 = bbox
    region = {"region": {"bbox": [x0, y0, (x0 + x1) / 2, y1]}}
    return Aggregate(
        view_id,
        groupings=_canonical_json(groupings),
        filters=_canonical_json(region),
        reference="{}",
    )


def _canonical_json(value) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def record(server, token: str, battery: Iterable[Query]) -> Recorded:
    """Issue the battery against a live server and canonicalise every response.

    `server` is `oracle.harness.Server` (or anything with its `viewer_base`/`meta`/`item`
    surface).
    """
    return {entry: record_one(server, token, entry) for entry in battery}


def record_one(server, token: str, query: Query) -> Canonical:
    """One query, issued and canonicalised."""
    if isinstance(query, Meta):
        return Json(server.meta(token))
    if isinstance(query, Categories):
        return Json({"pages": _category_pages(server, token, query.column)})
    if isinstance(query, Suggest):
        params = {"q": query.q}
        if query.counts:
            params["counts"] = "true"
        resp = requests.get(
            f"{server.viewer_base}/v1/categories/{query.column}/suggest",
            headers={"Authorization": f"Bearer {token}"},
            params=params,
            timeout=10,
        )
        return _status_and_body(resp, (200, 404))
    if isinstance(query, Browse):
        body: dict = {"view": query.view_id, "layer": query.layer}
        if query.parent is not None:
            body["parent"] = str(query.parent)
        if query.q is not None:
            body["q"] = query.q
        if query.filters is not None:
            body["filters"] = json.loads(query.filters)
        resp = requests.post(
            f"{server.viewer_base}/v1/artifacts/browse",
            headers={"Authorization": f"Bearer {token}"},
            json=body,
            timeout=30,
        )
        # A layer this principal cannot reach is refused as an unknown one: a real answer here.
        return _status_and_body(resp, (200, 422))
    if isinstance(query, ArtifactCard):
        resp = requests.post(
            f"{server.viewer_base}/v1/artifacts/{query.tessera_id}",
            headers={"Authorization": f"Bearer {token}"},
            json={"view": query.view_id},
            timeout=10,
        )
        return _status_and_body(resp, (200, 404))
    if isinstance(query, Viewport):
        artifacts = None if query.layers is None else _artifacts_body(server, token, query)
        return canonicalise_viewport(_viewport_body(server, token, query), artifacts)
    if isinstance(query, Item):
        # 404 is a *real* answer on this surface — contracts §3.2 returns it identically for "no
        # such ID" and "not visible to this principal", and a deny stage legitimately moves a
        # battery item from 200 to 404. Anything else is a harness failure.
        resp = server.item(token, query.tessera_id)
        if resp.status_code not in (200, 404):
            resp.raise_for_status()
        return Json({"status": resp.status_code, "body": resp.json()})
    if isinstance(query, Aggregate):
        return canonicalise_aggregate(server, token, query)
    raise TypeError(f"not a battery query: {query!r}")


def _status_and_body(resp: requests.Response, answers: tuple[int, ...]) -> Json:
    """A JSON surface whose refusals in `answers` are real answers; any other status is a harness
    failure."""
    if resp.status_code not in answers:
        resp.raise_for_status()
        raise RuntimeError(f"unexpected {resp.status_code}: {resp.text}")
    return Json({"status": resp.status_code, "body": resp.json()})


def _category_pages(server, token: str, column: str) -> list[dict]:
    """The full enumeration, as the page sequence the wire served.

    Paged to exhaustion rather than recorded as a single default page: `next` is the last key
    returned (contracts §3.2), so the walk terminates, and a one-page recording would silently
    stop covering the vocabulary the moment it grows past the deployment's page ceiling — the
    same silent narrowing as an unrequested underlay. The pages are kept as served (boundaries
    are a function of the value set and the ceiling, both deterministic), not re-joined into a
    shape the wire never carried.
    """
    pages: list[dict] = []
    after: str | None = None
    while True:
        params = {} if after is None else {"after": after}
        resp = requests.get(
            f"{server.viewer_base}/v1/categories/{column}",
            headers={"Authorization": f"Bearer {token}"},
            params=params,
            timeout=10,
        )
        resp.raise_for_status()
        page = resp.json()
        pages.append(page)
        after = page["next"]
        if after is None:
            return pages


def _viewport_body(server, token: str, query: Viewport) -> bytes:
    """POST the viewport request and return the raw framed body.

    Issued directly rather than through `oracle.harness.Server.viewport`, because that helper
    speaks only the `bbox` request form and this battery must also carry the contract's `tiles`
    form (§3.2: two request forms, and a divergence between them is exactly what a battery
    exists to catch).
    """
    body: dict = {"view": query.view_id, "zoom": query.zoom}
    if query.bbox is not None:
        body["bbox"] = list(query.bbox)
    if query.tiles is not None:
        body["tiles"] = list(query.tiles)
    if query.k is not None:
        body["k"] = query.k
    if query.filters is not None:
        body["filters"] = json.loads(query.filters)
    if query.underlay_offset:
        body["underlay_offset"] = query.underlay_offset
    if query.highlight is not None:
        body["highlight"] = json.loads(query.highlight)
    if query.layers is not None:
        body["layers"] = json.loads(query.layers)
    resp = requests.post(
        f"{server.viewer_base}/v1/viewport",
        headers={"Authorization": f"Bearer {token}"},
        json=body,
        timeout=30,
    )
    resp.raise_for_status()
    return resp.content


def _artifacts_body(server, token: str, query: Viewport) -> bytes:
    """The `/v1/artifacts/viewport` body of the same request: its tiles, filter, highlight and
    layers, every artifact of each tile."""
    body: dict = {"view": query.view_id, "zoom": query.zoom, "layers": json.loads(query.layers)}
    if query.bbox is not None:
        body["bbox"] = list(query.bbox)
    if query.tiles is not None:
        body["tiles"] = list(query.tiles)
    if query.filters is not None:
        body["filters"] = json.loads(query.filters)
    if query.highlight is not None:
        body["highlight"] = json.loads(query.highlight)
    body["per_tile"] = server.meta(token)["selection"]["max_artifacts_per_tile"]
    resp = requests.post(
        f"{server.viewer_base}/v1/artifacts/viewport",
        headers={"Authorization": f"Bearer {token}"},
        json=body,
        timeout=30,
    )
    resp.raise_for_status()
    return resp.content


def canonicalise_aggregate(server, token: str, query: Aggregate) -> Json:
    """Every table of the whole read, carried across responses by the trailer's cursor: each
    table's head without `resumed`, and its rows joined across pages as plain values.

    Cursors and times are left out, since they differ between two issues of one request; page
    boundaries are left out because a table's rows are the same however they are paged."""
    body: dict = {"view": query.view_id, "groupings": json.loads(query.groupings)}
    if query.filters is not None:
        body["filters"] = json.loads(query.filters)
    if query.reference is not None:
        body["reference"] = json.loads(query.reference)
    heads: dict[int, dict] = {}
    rows: dict[int, list[dict]] = {}
    while True:
        resp = requests.post(
            f"{server.viewer_base}/v1/aggregate",
            headers={"Authorization": f"Bearer {token}"},
            json=body,
            timeout=60,
        )
        resp.raise_for_status()
        tables, trailer = table_rows(resp.content)
        for head, table in tables:
            grouping = head["grouping"]
            heads.setdefault(grouping, {k: v for k, v in head.items() if k != "resumed"})
            rows.setdefault(grouping, []).extend(table)
        if trailer["next"] is None:
            break
        body["cursor"] = trailer["next"]
    return Json({"tables": [{"head": heads[g], "rows": rows.get(g, [])} for g in sorted(heads)]})


__all__ = [
    "Aggregate",
    "ArtifactCard",
    "Battery",
    "Browse",
    "Categories",
    "DEEP_ZOOM",
    "Item",
    "Meta",
    "Query",
    "Recorded",
    "Suggest",
    "Viewport",
    "build_battery",
    "record",
    "record_one",
]
