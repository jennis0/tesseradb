"""**Two modes: a corpus built as one shard and as several answers every viewer the same**,
black-box, over the real binary.

Each corpus is built twice in this run, at the default shard size and at the size the many-shard
mode gives it (`oracle.harness.shards_mode`), and both builds are served. The same requests go to
both, as the same principals, and every field of every answer is compared. A shard number, a
layer-space number or a figure taken over one shard would make some field differ from the
one-shard answer, which is how this module looks for them.

**What is not compared, and why.** Every other field is compared exactly.

1. *Identifiers.* Each build draws its own identity key, and an item's shard and number differ
   between the modes, so its `mosaica_id` does. Each identifier, in a request and in an answer,
   is put as a stable key: an item by its unique attribute, read by the oracle from the bundle,
   and an artifact by its layer, level and key, as a principal holding every term reads them in
   bulk. An identifier naming neither stays as it is, and fails the comparison.
2. *Cursors and elapsed times.* A cursor carries identifiers and positions. Whether a page has one
   is compared.
3. *Order and choice decided by identifiers.* Each of these is compared as a multiset, in place:
   points within a tile; rows of a map-order read within one map cell, its tile at depth 16;
   rows of one artifacts frame sharing layer, rung and count; browse rows sharing the count they
   are ordered by, joined across pages; an artifact's parents; the artifacts a layer grouping
   ranks. A stored-order read walks shards in number order, so its rows are compared as one
   multiset. A point's `membership:<layer>` names the deepest served artifact holding it, the
   lowest `mosaica_id` among those at one rung, so it is put as that tie class, and a tag
   outside its class or not the lowest identifier in it fails the comparison.
4. *Samples drawn by rank:* a sampled histogram's counts, and the groups and items its head says
   it counted, whose cut is a bound on `mosaica_id`; and the points of a viewport whose cap binds
   in some tile. Colour slots are a function of `mosaica_id` too, so no request names a palette.
5. *Where θ is a sketch.* Over several shards the view has a segment per shard, so θ's `N_occ` is
   estimated, and `served`, the points and the trailer's counts may move by the sketch's error.
   The catalogue is served twice in each mode: θ-saturated, where everything is compared, and
   θ-live (`theta_target_marks = 16`), where the counts are compared and those three are not.
6. *Status figures of the process and of the layout:* the caches, the heap and the sessions,
   which the control plane keeps per shard and per process; `segments[].count`, since each shard
   has its base segment; `shards.size`; and the write executor's counters, its log and
   `publication`, since a bundle of several shards takes no write and its executor idles. The
   corpus figures, summed over shards, are compared.

**The byte scanner.** Every many-shard answer is swept as `test_byte_scan.py` sweeps the
catalogue: the identifier columns, the identifiers in JSON and the bytes of every cursor hold no
item's number in its shard and no artifact's entity in its layer space, and no body holds the
identity key. Counts, coordinates and text are not swept here; `test_byte_scan.py` sweeps them in
the one-shard mode.

A difference fails with each request, the path of each differing field and both values, and both
answers are written to a file the message names.
"""

from __future__ import annotations

import io
import json
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

import pyarrow.ipc as ipc
import pytest
import requests

from oracle import artifact_label_fixture as lfx
from oracle import catalogue as cat
from oracle import identity as identity_mod
from oracle import wire
from oracle.bundle import Bundle
from oracle.harness import (
    JOIN_FIELD,
    Server,
    shards_mode,
    spawn_server,
    stop_server,
)

from suite.scan import cursor_windows, le_windows, value_buffer
from test_browse import TREE, tree_layer_toml
from test_highlight import CLUSTERS, LABELS, dependent_layers_toml
from test_region_leaf import GATED, gated_layer_toml
from test_shape_membership import (
    BOXES,
    N_POINTS,
    SHAPES,
    build_with_layers,
)

pytestmark = pytest.mark.two_modes

MODES = (("one", False), ("many", True))
WHOLE = [0.0, 0.0, 65536.0, 65536.0]
#: Above every corpus's size, so a θ-saturated server serves every visible point of every tile.
K_ALL = 200_000
PER_TILE = 1_000
#: The columns and JSON fields that carry identifiers.
ID_NAMES = frozenset({"mosaica_id", "parent_ids", "parents", "parent", "target", "artifact"})
ELAPSED = frozenset({"stream_us", "arrow_serialise_ns"})
CURSORS = frozenset({"next", "cursor"})
#: Top-level `/control/status` keys left out (module doc, item 6).
STATUS_PROCESS = frozenset(
    {
        "heap",
        "fragment_cache",
        "masked_count_cache",
        "row_projection_cache",
        "region_cache",
        "occupancy",
        "suggest_sets",
        "derived_cache",
        "projection_builds_by_route",
        "sessions",
        "shards",
        "publication",
    }
)
#: The differing fields a failure shows per request, and the report holds.
SHOWN = 12
REPORTED = 1_000
#: The most of each answer the report holds.
REPORTED_BYTES = 1 << 20


# ---- requests ----------------------------------------------------------------------------------


@dataclass(frozen=True)
class Request:
    """One request, as both modes are sent it. A string `"@<stable key>"` anywhere in `body` is
    the identifier that key names in the mode it is sent to."""

    route: str
    principal: str
    body: dict | None = None
    #: `saturated` or `live` (module doc, item 5).
    server: str = "saturated"

    def describe(self) -> str:
        body = "" if self.body is None else " " + json.dumps(self.body, sort_keys=True)
        return f"{self.route} as {self.principal} on the {self.server} server{body}"


@dataclass
class Corpus:
    name: str
    #: Builds the corpus in the mode in force and returns its bundle root.
    build: Callable[[Path], Path]
    #: The unique attribute an item's stable key is read from.
    key_field: str
    view: str
    principals: dict[str, list[str]]
    #: The principal whose bulk reads name every artifact.
    everyone: str
    #: The servers each mode starts, by name, with their `spawn_server` options.
    servers: dict[str, dict]
    battery: Callable[["Side"], list[Request]]


# ---- one mode's servers, and its identifiers ---------------------------------------------------


@dataclass
class Scan:
    """What the byte scanner gathers from one mode's answers."""

    id_windows: set[int] = field(default_factory=set)
    cursor_windows: set[int] = field(default_factory=set)
    bodies_holding_the_key: list[str] = field(default_factory=list)


class Side:
    """One mode: its bundle, its servers and the stable key of each identifier it serves."""

    def __init__(self, root: Path, corpus: Corpus, tmp: Path):
        self.corpus = corpus
        self.bundle = Bundle(root)
        self.key_raw = bytes.fromhex(self.bundle.manifest["identity"]["key"])
        self.servers: dict[str, tuple[Server, object]] = {}
        for name, options in corpus.servers.items():
            copy = tmp / f"{name}-bundle"
            shutil.copytree(root, copy)
            (tmp / name).mkdir()
            self.servers[name] = spawn_server(copy, tmp / name, **options)
        self._tokens: dict[tuple[str, str], str] = {}
        self._members: dict[tuple[str, str, str, str], frozenset[str]] = {}
        self.stable: dict[int, str] = {}
        self.unknown: set[int] = set()
        self.scan = Scan()
        for value, item in self.bundle.unique_entities(corpus.key_field).items():
            self.stable[self.bundle.mosaica_id_of(item)] = f"item {corpus.key_field}={value}"
        self._name_artifacts()
        self.raw = {stable: raw for raw, stable in self.stable.items()}

    def server(self, name: str) -> Server:
        return self.servers[name][0]

    def token(self, server: str, principal: str) -> str:
        if (server, principal) not in self._tokens:
            terms = self.corpus.principals[principal]
            self._tokens[(server, principal)] = self.server(server).authorise(terms)["token"]
        return self._tokens[(server, principal)]

    def stop(self) -> None:
        for _server, proc in self.servers.values():
            stop_server(proc)

    def _name_artifacts(self) -> None:
        """Every artifact, by layer, level and key, from the bulk reads of the principal holding
        every term."""
        server = self.server("saturated")
        token = self.token("saturated", self.corpus.everyone)
        for layer in server.meta(token)["layers"]:
            body = {"view": self.corpus.view, "layer": layer["name"], "fields": ["key", "level"]}
            for page in read_pages(server, token, "/v1/artifacts", body):
                for row in decode_rows(page[0])[1]:
                    name = f"artifact {layer['name']}/{row['level']}/{row['key']}"
                    assert name not in self.stable.values(), f"two artifacts are named {name}"
                    self.stable[row["mosaica_id"]] = name

    def members(self, tags: "Tags", layer: str, artifact: str) -> frozenset[str]:
        """The items of `artifact` the principal of `tags` may see, by stable key."""
        key = (tags.request.server, tags.request.principal, layer, artifact)
        if key not in self._members:
            body = {
                "view": self.corpus.view,
                "fields": [],
                "filters": {"member_of": {"layer": layer, "artifact": str(self.raw[artifact])}},
            }
            items = set()
            for records, _end in read_pages(tags.server, tags.token, "/v1/items", body):
                items |= {self.stable[r["mosaica_id"]] for r in decode_rows(records)[1]}
            self._members[key] = frozenset(items)
        return self._members[key]

    def map_cell(self, row: dict) -> tuple[int, int]:
        """The map cell a row of a map-order read is in: its tile at depth 16."""
        x0, x1, y0, y1 = self.bundle.extent_of(self.corpus.view)
        return (
            int((row["mosaica:x"] - x0) / (x1 - x0) * 65536),
            int((row["mosaica:y"] - y0) / (y1 - y0) * 65536),
        )

    def numbers(self) -> set[int]:
        """Each item's number in its shard, and each artifact's entity in its layer space: what an
        identifier must never carry."""
        items = identity_mod.numbers_of(
            self.bundle.unique_entities(self.corpus.key_field).values()
        )
        artifacts = {
            identity_mod.invert_artifact(self.bundle.identity_key, raw)[1]
            for raw, name in self.stable.items()
            if name.startswith("artifact ")
        }
        return items | artifacts

    # ---- normalising an answer ------------------------------------------------------------------

    def resolve(self, value):
        """A request body with each `"@<stable key>"` replaced by this mode's identifier."""
        if isinstance(value, dict):
            return {k: self.resolve(v) for k, v in value.items()}
        if isinstance(value, list):
            return [self.resolve(v) for v in value]
        if isinstance(value, str) and value.startswith("@"):
            return str(self.raw[value[1:]])
        return value

    def rekey(self, value, name: str | None = None, *, cursors: bool = True):
        """`value` as plain JSON with every identifier put as its stable key (module doc, item 1),
        with `cursors` every cursor as whether there is one (item 2), and no elapsed time."""
        if isinstance(value, dict):
            out = {}
            for k, v in value.items():
                if k in ELAPSED:
                    continue
                if cursors and k in CURSORS:
                    if v is not None:
                        self.scan.cursor_windows |= cursor_windows(v)
                    out[k] = None if v is None else "a cursor"
                    continue
                out[k] = self.rekey(v, k, cursors=cursors)
            return out
        if isinstance(value, (list, tuple)):
            return [self.rekey(v, name, cursors=cursors) for v in value]
        if isinstance(value, bytes):
            return value.hex()
        if isinstance(value, bool) or value is None or isinstance(value, float):
            return value
        if isinstance(value, (int, str)):
            raw = value if isinstance(value, int) else int(value) if value.isdigit() else None
            if raw is not None and name in ID_NAMES:
                self.scan.id_windows.add(raw)
            if raw is not None and raw in self.stable:
                return self.stable[raw]
            if raw is not None and name in ID_NAMES:
                self.unknown.add(raw)
            return value
        return str(value)

    def rekey_row(self, row: dict) -> dict:
        """One Arrow row, [`rekey`]'d in its identifier columns alone: no other column is JSON
        or holds an identifier."""
        out = {}
        for name, value in row.items():
            if name in ID_NAMES or name == "key" or name.startswith("membership:"):
                value = self.rekey(value, name)
            elif isinstance(value, bytes):
                value = value.hex()
            out[name] = value
        return out

    def body_scanned(self, label: str, raw: bytes) -> bytes:
        if self.key_raw in raw:
            self.scan.bodies_holding_the_key.append(label)
        return raw


# ---- the wire ----------------------------------------------------------------------------------


def decode_rows(payload: bytes) -> tuple[list[str], list[dict]]:
    """One Arrow stream: its schema as `name: type` strings, and its rows."""
    table = ipc.open_stream(io.BytesIO(payload)).read_all()
    return [f"{f.name}: {f.type}" for f in table.schema], table.to_pylist()


def id_column_values(payload: bytes) -> set[int]:
    """Every value of every identifier column: `mosaica_id`'s 8-byte windows at its stride, and
    the values of the nullable and list columns, whose null slots hold zeros."""
    values: set[int] = set()
    with ipc.open_stream(io.BytesIO(payload)) as reader:
        for batch in reader:
            for name in batch.schema.names:
                column = batch.column(name)
                if name == "mosaica_id":
                    values |= le_windows(value_buffer(column, 8), 8, stride=8)
                elif (
                    name in ID_NAMES
                    or name.startswith("membership:")
                    or (name == "key" and str(column.type) == "uint64")
                ):
                    if str(column.type).startswith("list"):
                        column = column.flatten()
                    values |= {v for v in column.to_pylist() if v is not None}
    return values


def post(server: Server, path: str, token: str, body: dict) -> requests.Response:
    return requests.post(
        f"{server.viewer_base}{path}",
        headers={"Authorization": f"Bearer {token}"},
        json=body,
        timeout=120,
    )


def read_pages(server: Server, token: str, path: str, body: dict) -> list[tuple[bytes, dict]]:
    """Every page of a paged read, carried across responses by the cursor."""
    pages = []
    body = dict(body)
    while True:
        resp = post(server, path, token, body)
        assert resp.status_code == 200, resp.text
        decoded = wire.split_items_frames(resp.content)
        pages += decoded.pages
        if decoded.trailer["next"] is None:
            return pages
        body["cursor"] = decoded.trailer["next"]


def multiset(rows: list) -> list:
    """`rows` in one order whatever order they came in. Rows of one answer share their columns'
    order, so their `repr` is a total order."""
    return sorted(rows, key=repr)


def ties_in_place(rows: list[dict], tie: Callable[[dict], object]) -> list[dict]:
    """`rows` with each run of consecutive rows sharing `tie` sorted, the runs left in order
    (module doc, item 3)."""
    out: list[dict] = []
    run: list[dict] = []
    for row in rows:
        if run and tie(run[0]) != tie(row):
            out += multiset(run)
            run = []
        run.append(row)
    return out + multiset(run)


def sort_parents(row: dict) -> dict:
    for name in ("parent_ids", "parents"):
        if isinstance(row.get(name), list):
            row[name] = sorted(row[name])
    return row


# ---- one request, sent and normalised ----------------------------------------------------------


def send(side: Side, request: Request):
    """`request`'s answer in `side`'s mode, normalised for the comparison."""
    server = side.server(request.server)
    token = side.token(request.server, request.principal)
    body = side.resolve(request.body or {})
    sender = SENDERS[request.route]
    return sender(side, server, token, body, request)


def send_viewport(side, server, token, body, request):
    resp = post(server, "/v1/viewport", token, body)
    if resp.status_code != 200:
        return {"status": resp.status_code, "body": resp.json()}
    raw = side.body_scanned(request.describe(), resp.content)
    wire.decode_frames(raw)
    tiles_schema, tiles, underlay, points, points_schemas, trailer = None, [], None, [], [], None
    zoom = body["zoom"]
    for kind, payload in wire.split_frames(raw):
        if kind == wire.FRAME_TILES:
            tiles_schema, tiles = decode_rows(payload)
        elif kind == wire.FRAME_SUB_CELLS:
            underlay = list(decode_rows(payload))
        elif kind == wire.FRAME_POINTS:
            side.scan.id_windows |= id_column_values(payload)
            schema, rows = decode_rows(payload)
            if not points_schemas or points_schemas[-1] != schema:
                points_schemas.append(schema)
            points += rows
        elif kind == wire.FRAME_TRAILER:
            trailer = side.rekey(json.loads(payload))
    tiles = sorted(tiles, key=lambda t: t["tile"])
    out = {"tiles_schema": tiles_schema, "underlay": underlay, "tiles": tiles, "trailer": trailer}
    if request.server == "live":
        for tile in tiles:
            del tile["served"]
        del trailer["points"], trailer["flushes"]
        return out
    out["points_schemas"] = points_schemas
    if any(t["served"] < t["matched"] for t in tiles):
        out["points"] = "a rank sample"
        return out
    tags = Tags(side, server, token, body, request) if "layers" in body else None
    by_tile: dict[int, list] = {}
    for row in points:
        tile = row["code"] >> (64 - 2 * zoom) if zoom else 0
        row = side.rekey_row(row)
        if tags is not None:
            row = tags.classes(row)
        by_tile.setdefault(tile, []).append(row)
    out["points"] = [{"tile": tile, "rows": multiset(rows)} for tile, rows in by_tile.items()]
    return out


class Tags:
    """A point's `membership:<layer>` names the deepest served artifact of the layer holding it,
    the lowest `mosaica_id` among those at one rung (module doc, item 3). Each tag is put as that
    tie class: the artifacts of the same request's artifacts answer holding the point at the
    deepest rung among those holding it. A tag outside its class, or not the lowest identifier in
    it, is put as what was chosen and over what."""

    def __init__(self, side: Side, server: Server, token: str, body: dict, request: Request):
        self.side, self.server, self.token, self.request = side, server, token, request
        artifacts = {k: v for k, v in body.items() if k not in ("k", "underlay_offset")}
        resp = post(server, "/v1/artifacts/viewport", token, {**artifacts, "per_tile": PER_TILE})
        assert resp.status_code == 200, resp.text
        frames, _trailer = wire.decode_artifact_frames(resp.content)
        self.rung = {
            (row.layer, side.stable.get(row.mosaica_id, row.mosaica_id)): row.rung
            for _tile, rows in frames
            for row in rows
        }

    def classes(self, row: dict) -> dict:
        for name in [n for n in row if n.startswith("membership:")]:
            if row[name] is None:
                continue
            layer, tag = name.removeprefix("membership:"), row[name]
            holding = {
                a: rung
                for (name_of, a), rung in self.rung.items()
                if name_of == layer and row["mosaica_id"] in self.side.members(self, layer, a)
            }
            deepest = max(holding.values(), default=None)
            tied = sorted(a for a, rung in holding.items() if rung == deepest)
            lowest = min(tied, key=lambda a: self.side.raw[a], default=None)
            row[name] = tied if tag == lowest else {"chose": tag, "over": tied}
        return row


def send_artifacts_viewport(side, server, token, body, request):
    resp = post(server, "/v1/artifacts/viewport", token, body)
    if resp.status_code != 200:
        return {"status": resp.status_code, "body": resp.json()}
    raw = side.body_scanned(request.describe(), resp.content)
    wire.decode_artifact_frames(raw)

    def tie(r):
        return r["layer"], r["rung"], r["masked_count"]

    frames = []
    payloads = wire.split_frames(raw)
    for kind, payload in payloads[:-1]:
        side.scan.id_windows |= id_column_values(payload)
        schema, rows = decode_rows(payload)
        rows = [sort_parents(side.rekey_row(row)) for row in rows]
        frames.append({"schema": schema, "rows": ties_in_place(rows, tie)})
    return {"frames": frames, "trailer": side.rekey(json.loads(payloads[-1][1]))}


def send_paged(path: str):
    """A paged read's every head, page and trailer, and its rows joined across pages, put in
    order by `body["order"]` (module doc, item 3)."""

    def sender(side, server, token, body, request):
        heads, pages, trailers, rows, schema = [], [], [], [], None
        body = dict(body)
        while True:
            resp = post(server, path, token, body)
            if resp.status_code != 200:
                return {"status": resp.status_code, "body": resp.json(), "after": len(trailers)}
            decoded = wire.split_items_frames(side.body_scanned(request.describe(), resp.content))
            heads.append(decoded.head)
            for records, end in decoded.pages:
                side.scan.id_windows |= id_column_values(records)
                schema, page = decode_rows(records)
                rows += [sort_parents(side.rekey_row(row)) for row in page]
                pages.append({"rows": len(page), "end": side.rekey(end)})
            trailers.append(decoded.trailer)
            if decoded.trailer["next"] is None:
                break
            body["cursor"] = decoded.trailer["next"]
            body.pop("count", None)
        order = body.get("order")
        if order == "map":
            rows = ties_in_place(rows, side.map_cell)
        elif order == "stored":
            rows = multiset(rows)
        heads, trailers = side.rekey(heads), side.rekey(trailers)
        return {
            "heads": heads,
            "pages": pages,
            "trailers": trailers,
            "schema": schema,
            "rows": rows,
        }

    return sender


def send_aggregate(side, server, token, body, request):
    heads: dict[int, dict] = {}
    tables: dict[int, list] = {}
    schemas: dict[int, list] = {}
    trailers = []
    body = dict(body)
    while True:
        resp = post(server, "/v1/aggregate", token, body)
        if resp.status_code != 200:
            return {"status": resp.status_code, "body": resp.json()}
        decoded = wire.split_aggregate_frames(side.body_scanned(request.describe(), resp.content))
        for head, pages in decoded.tables:
            grouping = head["grouping"]
            heads.setdefault(grouping, {k: v for k, v in head.items() if k != "resumed"})
            for records, end in pages:
                side.scan.id_windows |= id_column_values(records)
                schemas[grouping], rows = decode_rows(records)
                tables.setdefault(grouping, []).extend(side.rekey_row(row) for row in rows)
        trailers.append(decoded.trailer)
        if decoded.trailer["next"] is None:
            break
        body["cursor"] = decoded.trailer["next"]
    out = []
    for grouping, head in sorted(heads.items()):
        by = body["groupings"][grouping].get("by", {})
        rows = tables.get(grouping, [])
        if "layer" in by and "top" in by:
            listed = [r for r in rows if r["group"] == "listed"]
            blocks: dict[str, list] = {}
            for row in listed:
                blocks.setdefault(row["key"], []).append(row)
            rows = multiset(list(blocks.values())) + [r for r in rows if r["group"] != "listed"]
        if head.get("sample") is not None and head["sample"].get("sampled"):
            head = {k: v for k, v in head.items() if k != "groups"} | {"sample": {"sampled": True}}
            rows = [
                {k: v for k, v in row.items() if k not in ("count", "reference_count", "lift")}
                for row in rows
            ]
        out.append({"head": side.rekey(head), "schema": schemas.get(grouping), "rows": rows})
    return {"tables": out, "trailers": side.rekey(trailers)}


def send_card(path: str):
    def sender(side, server, token, body, request):
        body = dict(body)
        resp = post(server, path.format(body.pop("id")), token, body)
        side.body_scanned(request.describe(), resp.content)
        return {"status": resp.status_code, "body": side.rekey(resp.json())}

    return sender


def send_browse(side, server, token, body, request):
    """Every page, carried by the cursor, and the rows joined across pages, since a page can end
    inside a run of tied counts (module doc, item 3)."""
    pages, rows, parents = [], [], None
    body = dict(body)
    while True:
        resp = post(server, "/v1/artifacts/browse", token, body)
        side.body_scanned(request.describe(), resp.content)
        if resp.status_code != 200:
            return {"status": resp.status_code, "body": resp.json(), "after": len(pages)}
        answer = resp.json()
        cursor = answer.pop("next", None)
        rows += [sort_parents(row) for row in side.rekey(answer.pop("artifacts"))]
        parents = side.rekey(answer.pop("parents"))
        pages.append({"rows": len(rows), "rest": side.rekey(answer), "next": cursor is not None})
        if cursor is None:
            break
        body["cursor"] = cursor

    # Under a filter the rows are ordered by `matched_count` alone, and otherwise by
    # `masked_count`.
    def tie(r):
        return r["matched_count"] if "matched_count" in r else r["masked_count"]

    return {"pages": pages, "artifacts": ties_in_place(rows, tie), "parents": parents}


def send_get(path: str, paged: bool = False):
    def sender(side, server, token, body, request):
        params = dict(body)
        url = f"{server.viewer_base}{path.format(params.pop('column', ''))}"
        answers = []
        while True:
            resp = requests.get(
                url, headers={"Authorization": f"Bearer {token}"}, params=params, timeout=60
            )
            side.body_scanned(request.describe(), resp.content)
            # A category listing's `next` is the last key it holds, not a cursor.
            answer = side.rekey(resp.json(), cursors=not paged)
            answers.append({"status": resp.status_code, "body": answer})
            if not paged or resp.status_code != 200 or resp.json()["next"] is None:
                return answers
            params["after"] = resp.json()["next"]

    return sender


def send_suggest(side, server, token, body, request):
    body = dict(body)
    resp = post(server, f"/v1/categories/{body.pop('column')}/suggest", token, body)
    side.body_scanned(request.describe(), resp.content)
    return {"status": resp.status_code, "body": side.rekey(resp.json())}


def send_status(side, server, token, body, request):
    status = {k: v for k, v in server.status().items() if k not in STATUS_PROCESS}
    for segments in status["segments"]:
        del segments["count"]
    flush = status["write_executor"]["flush"]
    status["write_executor"] = {
        "posture": status["write_executor"]["posture"],
        "buffered_items": flush["buffered_items"],
        "flushable_items": flush["flushable_items"],
    }
    return side.rekey(status)


SENDERS = {
    "POST /v1/viewport": send_viewport,
    "POST /v1/artifacts/viewport": send_artifacts_viewport,
    "POST /v1/items": send_paged("/v1/items"),
    "POST /v1/artifacts": send_paged("/v1/artifacts"),
    "POST /v1/aggregate": send_aggregate,
    "POST /v1/items/{id}": send_card("/v1/items/{}"),
    "POST /v1/artifacts/{id}": send_card("/v1/artifacts/{}"),
    "POST /v1/artifacts/browse": send_browse,
    "GET /v1/categories/{column}": send_get("/v1/categories/{}", paged=True),
    "GET /v1/categories/{column}/suggest": send_get("/v1/categories/{}/suggest"),
    "POST /v1/categories/{column}/suggest": send_suggest,
    "GET /v1/meta": send_get("/v1/meta"),
    "GET /control/status": send_status,
}


# ---- comparing ---------------------------------------------------------------------------------


def differences(one, many, path: str = "$", out: list | None = None, limit: int = SHOWN) -> list:
    """`(path, one-shard value, many-shard value)` for each differing field, at most `limit`."""
    out = [] if out is None else out
    if len(out) >= limit:
        return out
    if isinstance(one, dict) and isinstance(many, dict):
        for k in sorted(set(one) | set(many), key=str):
            if k not in one or k not in many:
                out.append((f"{path}.{k}", one.get(k, "(absent)"), many.get(k, "(absent)")))
            else:
                differences(one[k], many[k], f"{path}.{k}", out, limit)
    elif isinstance(one, list) and isinstance(many, list):
        if len(one) != len(many):
            out.append((f"{path} length", len(one), len(many)))
        for i, (a, b) in enumerate(zip(one, many)):
            differences(a, b, f"{path}[{i}]", out, limit)
    elif one != many or type(one) is not type(many):
        out.append((path, one, many))
    return out[:limit]


@dataclass
class Comparison:
    corpus: str
    shards: dict[str, int]
    compared: int
    #: `(request, one-shard answer, many-shard answer)` for each request answered differently.
    differing: list[tuple[Request, object, object]]
    #: Identifiers in an identifier field that name nothing this module knows, by mode.
    unknown: dict[str, set[int]]
    scan: Scan
    numbers: set[int]
    identity_key: identity_mod.IdentityKey
    report: Path


def compare(corpus: Corpus, sides: dict[str, Side], report: Path) -> Comparison:
    named = {
        mode: {k for k in side.raw if k.startswith("artifact ")} for mode, side in sides.items()
    }
    assert named["one"] == named["many"], (
        f"artifacts served to {corpus.everyone} in one mode alone: "
        f"{sorted(named['one'] ^ named['many'])}"
    )
    battery = corpus.battery(sides["one"])
    battery += [
        Request("GET /control/status", corpus.everyone, server=name) for name in corpus.servers
    ]
    differing = []
    for request in battery:
        one, many = (send(sides[mode], request) for mode, _ in MODES)
        if one != many:
            differing.append((request, one, many))
    with report.open("w") as out:
        for request, one, many in differing:
            out.write(f"== {request.describe()}\n")
            for path, a, b in differences(one, many, limit=REPORTED):
                out.write(f"   {path}: one shard {a!r}, many shards {b!r}\n")
            for mode, answer in (("one shard", one), ("many shards", many)):
                text = json.dumps(answer, sort_keys=True, indent=1)
                cut = "" if len(text) <= REPORTED_BYTES else f"\n(cut at {REPORTED_BYTES} bytes)"
                out.write(f"-- {mode}\n{text[:REPORTED_BYTES]}{cut}\n")
    return Comparison(
        corpus=corpus.name,
        shards={mode: len(side.bundle.shards) for mode, side in sides.items()},
        compared=len(battery),
        differing=differing,
        unknown={mode: side.unknown for mode, side in sides.items()},
        scan=sides["many"].scan,
        numbers=sides["many"].numbers(),
        identity_key=sides["many"].bundle.identity_key,
        report=report,
    )


# ---- the corpora -------------------------------------------------------------------------------


def viewports(
    principal: str, view: str, layers, filters: dict, highlight: dict, *, depths: bool = True
) -> list[Request]:
    """The viewport and the artifacts of the same request filtered, highlighted and both, and
    with `depths` at several depths, over part of the map, and with a cap that binds."""
    shapes = [
        {"zoom": 2, "bbox": WHOLE, "filters": filters},
        {"zoom": 2, "bbox": WHOLE, "highlight": highlight},
        {"zoom": 3, "bbox": WHOLE, "filters": filters, "highlight": highlight},
    ]
    if depths:
        shapes += [
            {"zoom": 0, "bbox": WHOLE, "underlay_offset": 2},
            {"zoom": 3, "bbox": WHOLE, "underlay_offset": 2},
            {"zoom": 5, "bbox": WHOLE, "underlay_offset": 1},
            {"zoom": 4, "bbox": [3000.0, 1000.0, 41000.0, 30000.0]},
        ]
    out = []
    for shape in shapes:
        body = {"view": view, "k": K_ALL, **shape}
        if layers is not None:
            body["layers"] = layers
            artifacts = {k: v for k, v in body.items() if k not in ("k", "underlay_offset")}
            out.append(
                Request(
                    "POST /v1/artifacts/viewport", principal, {**artifacts, "per_tile": PER_TILE}
                )
            )
        out.append(Request("POST /v1/viewport", principal, body))
    if depths:
        capped = {"view": view, "zoom": 3, "bbox": WHOLE, "k": 3}
        out.append(Request("POST /v1/viewport", principal, capped))
    return out


def live_viewports(principal: str, view: str, **extra) -> list[Request]:
    return [
        Request("POST /v1/viewport", principal, body, server="live")
        for body in (
            {"view": view, "zoom": 0, "bbox": WHOLE, "underlay_offset": 2},
            {"view": view, "zoom": 3, "bbox": WHOLE, "k": 40},
            {"view": view, "zoom": 5, "bbox": WHOLE, "underlay_offset": 1},
            {"view": view, "zoom": 2, "bbox": WHOLE, **extra},
        )
    ]


def items_reads(principal: str, view: str, fields: list[str], filters: dict) -> list[Request]:
    base = {"view": view, "fields": fields, "page_rows": 4_999, "pages": 2}
    return [
        Request(
            "POST /v1/items",
            principal,
            {**base, "order": "map", "system_fields": ["position", "labels"], "count": True},
        ),
        Request(
            "POST /v1/items",
            principal,
            {
                **base,
                "order": "map",
                "system_fields": ["position"],
                "filters": filters,
                "keep_unmatched": True,
                "count": True,
            },
        ),
        Request(
            "POST /v1/items",
            principal,
            {
                **base,
                "order": "stored",
                "system_fields": ["position", "labels"],
                "filters": filters,
            },
        ),
    ]


CATALOGUE_PRINCIPALS = {
    "everyone": list(next(c for c in cat.catalogue() if c.name == "full_100pct").grants),
    "a tenth": list(next(c for c in cat.catalogue() if c.name == "crossover_above").grants),
    "fifteen items": list(next(c for c in cat.catalogue() if c.name == "sparse_0_01pct").grants),
    "nothing": [],
    "high_tail": [cat.BLOCKS["high_tail"].descriptor],
}
CATALOGUE_FIELDS = [
    "serial",
    "fx_key",
    "department",
    "archive",
    "title",
    "shelf",
    "submitter",
    "abstract",
    "note",
    "pages",
]


def catalogue_build(work: Path) -> Path:
    return cat.build_catalogue_bundle()[0]


def catalogue_battery(side: Side) -> list[Request]:
    view = cat.VIEW_ID
    filters = {
        "all_of": [{"department": {"in": ["alpha", "beta"]}}, {"abstract": {"match": "harbour"}}]
    }
    highlight = {"submitter": {"prefix": "hub"}}
    # Every block's first, middle and last item, which between them lie in both shards.
    serials = sorted(
        cat.PLANTED_ID_BASE + source
        for block in cat.BLOCKS.values()
        for source in {block.start, (block.start + block.stop) // 2, block.stop - 1}
    )
    left = {"region": {"bbox": [0.0, 0.0, 32768.0, 65536.0]}}
    groupings = [
        {},
        {"cells": {"depth": 4}},
        {"by": {"field": "department", "top": 5}},
        {"by": {"field": "department", "top": 3}, "cells": {"depth": 2}},
        {"by": {"field": "archive", "values": ["red", "blue", "void", "nonesuch"]}},
        {"by": {"field": "shelf", "top": 2}},
        {"by": {"field": "fx_key", "bins": 10}},
        {"by": {"field": "serial", "bins": 12, "range": [1_000_000, 1_150_000]}},
        {"by": {"field": "serial", "bins": 6, "sample": 50}},
        {"by": {"field": "fx_key", "summary": True}},
        {
            "by": {"field": "department", "top": 6},
            "cells": {"depth": 3, "area": [0.0, 0.0, 20000.0, 40000.0]},
        },
    ]
    out = []
    for principal in CATALOGUE_PRINCIPALS:
        out += viewports(principal, view, None, filters, highlight)
        out += live_viewports(principal, view, filters=filters, highlight=highlight)
        out += items_reads(principal, view, CATALOGUE_FIELDS, {"department": {"eq": "beta"}})
        out.append(
            Request(
                "POST /v1/items",
                principal,
                {
                    "view": view,
                    "fields": ["serial", "title"],
                    "order": "map",
                    "system_fields": ["position"],
                    "count": True,
                    "filters": {"serial": {"in": serials}},
                },
            )
        )
        out += [
            Request("POST /v1/items/{id}", principal, {"id": f"@item serial={serial}"})
            for serial in serials
        ]
        out += [
            Request(
                "POST /v1/aggregate",
                principal,
                {"view": view, "groupings": groupings, "filters": left, "reference": {}},
            ),
            Request(
                "POST /v1/aggregate",
                principal,
                {
                    "view": view,
                    "groupings": groupings[:6],
                    "filters": {"department": {"eq": "alpha"}},
                    "reference": {"archive": {"eq": "red"}},
                    "page_rows": 7,
                    "pages": 2,
                },
            ),
        ]
        out += [
            Request("GET /v1/categories/{column}", principal, {"column": column})
            for column in ("department", "archive", "shelf")
        ]
        out += [
            Request(
                "GET /v1/categories/{column}/suggest",
                principal,
                {"column": c, "q": q, "counts": "true"},
            )
            for c, q in (
                ("department", "a"),
                ("archive", "g"),
                ("department", "zz"),
            )
        ]
        out += [
            Request("GET /v1/categories/{column}/suggest", principal, {"column": column, "q": q})
            for column, q in (("department", "o"), ("shelf", "s"))
        ]
        out += [
            Request(
                "GET /v1/categories/{column}/suggest",
                principal,
                {"column": "archive", "q": "b", "counts": "true", "view": view},
            ),
            Request(
                "POST /v1/categories/{column}/suggest",
                principal,
                {"column": "department", "q": "", "counts": True, "view": view, "filters": filters},
            ),
            Request(
                "POST /v1/categories/{column}/suggest",
                principal,
                {"column": "archive", "q": "", "counts": True, "view": view, "filters": left},
            ),
            Request("GET /v1/meta", principal),
        ]
    return out


SHAPE_PRINCIPALS = {"1": ["1"], "2": ["2"], "1 and 2": ["1", "2"]}
SHAPE_VIEW = "s0"
SHAPE_LAYERS = [SHAPES, BOXES, GATED, TREE, CLUSTERS, LABELS]


def shapes_build(work: Path) -> Path:
    return build_with_layers(
        work, "layers", gated_layer_toml() + tree_layer_toml() + dependent_layers_toml()
    )


def layer_reads(
    side: Side, principal: str, view: str, layers: list[str], attached: set[str], filters: dict
) -> list[Request]:
    """Bulk artifacts and the cards of every artifact of `layers`, and browse over each of them
    but the `attached`, which have no lineage of their own."""
    out = []
    every = [
        "key",
        "level",
        "parents",
        "target",
        "masked_count",
        "content",
        "centroid",
        "box",
        "shape",
    ]
    for layer in layers:
        base = {"view": view, "layer": layer}
        names = sorted(s for s in side.raw if s.startswith(f"artifact {layer}/"))
        out += [
            Request(
                "POST /v1/artifacts",
                principal,
                {**base, "fields": every, "count": True, "page_rows": 2, "pages": 2},
            ),
            Request(
                "POST /v1/artifacts",
                principal,
                {
                    **base,
                    "fields": ["key", "masked_count"],
                    "filters": filters,
                    "keep_unmatched": True,
                    "count": True,
                },
            ),
            Request("POST /v1/artifacts", principal, {**base, "fields": ["key"], "q": "e"}),
            Request(
                "POST /v1/artifacts",
                principal,
                {**base, "fields": ["key", "masked_count"], "ids": [f"@{n}" for n in names[::2]]},
            ),
        ]
        out += [
            Request("POST /v1/artifacts/{id}", principal, {"id": f"@{name}", "view": view})
            for name in names
        ]
        out += [
            Request(
                "POST /v1/artifacts", principal, {**base, "fields": ["key"], "parent": f"@{name}"}
            )
            for name in names
        ]
        if layer in attached:
            continue
        out += [
            Request("POST /v1/artifacts/browse", principal, base),
            Request("POST /v1/artifacts/browse", principal, {**base, "q": "e"}),
            Request("POST /v1/artifacts/browse", principal, {**base, "filters": filters}),
            Request("POST /v1/artifacts/browse", principal, {**base, "limit": 1}),
        ]
        out += [
            Request("POST /v1/artifacts/browse", principal, {**base, "parent": f"@{name}"})
            for name in names
        ]
    return out


def aggregates(
    side: Side,
    principal: str,
    view: str,
    layers: dict[str, dict],
    fields: list[dict],
    filters: dict,
) -> list[Request]:
    """Each layer's artifacts, ranked and named, with cells, one request a layer; and `fields`.
    Each over `filters`, against the whole visible set."""
    groupings = []
    for layer, extra in layers.items():
        names = sorted(s for s in side.raw if s.startswith(f"artifact {layer}/"))
        groupings.append(
            [
                {"by": {"layer": layer, "top": 50, **extra}},
                {"by": {"layer": layer, "top": 50, **extra}, "cells": {"depth": 2}},
                {"by": {"layer": layer, "artifacts": [f"@{n}" for n in names[::-1]]}},
            ]
        )
    groupings.append(fields)
    return [
        Request(
            "POST /v1/aggregate",
            principal,
            {"view": view, "groupings": each, "filters": filters, "reference": {}},
        )
        for each in groupings
    ]


def shapes_battery(side: Side) -> list[Request]:
    view = SHAPE_VIEW
    filters = {"fx_key": {"range": {"lt": 2000}}}
    highlight = {"fx_key": {"range": {"gte": 3000}}}
    square = f"@artifact {SHAPES}/0/square"
    layers = {
        SHAPES: {},
        BOXES: {},
        GATED: {},
        TREE: {"cut": {"zoom": 2, "bbox": WHOLE}},
        CLUSTERS: {},
    }
    fields = [{"by": {"field": "fx_key", "bins": 8}}, {"by": {"field": "fx_key", "summary": True}}]
    sources = range(0, N_POINTS, 211)
    out = []
    for principal in SHAPE_PRINCIPALS:
        out += viewports(principal, view, "all", filters, highlight)
        out += viewports(
            principal,
            view,
            [SHAPES, TREE],
            {"member_of": {"layer": SHAPES, "artifact": square}},
            {"region": {"bbox": [0.0, 0.0, 30000.0, 30000.0]}},
            depths=False,
        )
        out += items_reads(
            principal, view, ["fx_key"], {"member_of": {"layer": SHAPES, "artifact": square}}
        )
        out += [
            Request("POST /v1/items/{id}", principal, {"id": f"@item {JOIN_FIELD}={source}"})
            for source in sources
        ]
        out += layer_reads(side, principal, view, SHAPE_LAYERS, {LABELS}, filters)
        out += aggregates(side, principal, view, layers, fields, filters)
        out.append(Request("GET /v1/meta", principal))
    return out


LABEL_PRINCIPALS = {", ".join(terms): terms for terms in lfx.PRINCIPALS}


def labels_build(work: Path) -> Path:
    return lfx.build_bundle(work, with_layers=True)


def labels_battery(side: Side) -> list[Request]:
    view = lfx.VIEW_ID
    filters = {JOIN_FIELD: {"in": list(range(40, 130))}}
    highlight = {JOIN_FIELD: {"in": list(range(0, 60))}}
    tree = {"cut": {"zoom": 1, "bbox": WHOLE}}
    layers = {lfx.TEAMS: tree, lfx.SEALED: {}, lfx.NAMES: {}, lfx.TITLES: {}, lfx.GATED: {}}
    fields = [{}, {"cells": {"depth": 3}}]
    out = []
    for principal in LABEL_PRINCIPALS:
        out += viewports(principal, view, "all", filters, highlight)
        out += items_reads(principal, view, [JOIN_FIELD], filters)
        out += [
            Request("POST /v1/items/{id}", principal, {"id": f"@item {JOIN_FIELD}={source}"})
            for source in range(0, lfx.N_ITEMS, 7)
        ]
        out += layer_reads(side, principal, view, list(layers), {lfx.NAMES, lfx.TITLES}, filters)
        out += aggregates(side, principal, view, layers, fields, filters)
        out.append(Request("GET /v1/meta", principal))
    return out


CORPORA = [
    Corpus(
        name="catalogue",
        build=catalogue_build,
        key_field="serial",
        view=cat.VIEW_ID,
        principals=CATALOGUE_PRINCIPALS,
        everyone="everyone",
        servers={"saturated": {}, "live": {"theta_target_marks": 16}},
        battery=catalogue_battery,
    ),
    Corpus(
        name="shapes",
        build=shapes_build,
        key_field=JOIN_FIELD,
        view=SHAPE_VIEW,
        principals=SHAPE_PRINCIPALS,
        everyone="1 and 2",
        servers={"saturated": {}},
        battery=shapes_battery,
    ),
    Corpus(
        name="labels",
        build=labels_build,
        key_field=JOIN_FIELD,
        view=lfx.VIEW_ID,
        principals=LABEL_PRINCIPALS,
        everyone=", ".join(lfx.PRINCIPALS[4]),
        servers={"saturated": {}},
        battery=labels_battery,
    ),
]


@pytest.fixture(scope="module", params=CORPORA, ids=[c.name for c in CORPORA])
def comparison(request, tmp_path_factory) -> Comparison:
    """Both modes of one corpus, built, served and sent the battery; the servers are stopped
    before the tests read the result."""
    corpus: Corpus = request.param
    sides: dict[str, Side] = {}
    try:
        for mode, many in MODES:
            work = tmp_path_factory.mktemp(f"two-modes-{corpus.name}-{mode}")
            with shards_mode(many):
                root = corpus.build(work)
            sides[mode] = Side(root, corpus, work)
        report = tmp_path_factory.mktemp("two-modes-reports") / f"{corpus.name}.txt"
        result = compare(corpus, sides, report)
    finally:
        for side in sides.values():
            side.stop()
    return result


def test_the_many_shard_build_spans_several_shards(comparison: Comparison):
    assert comparison.shards["one"] == 1
    assert comparison.shards["many"] >= 2, comparison.shards


def test_every_answer_is_the_one_shard_answer(comparison: Comparison):
    lines = []
    for request, one, many in comparison.differing[:6]:
        lines.append(f"{request.describe()}")
        lines += [
            f"    {path}: one shard {a!r}, many shards {b!r}"
            for path, a, b in differences(one, many)
        ]
    assert not comparison.differing, (
        f"{len(comparison.differing)} of {comparison.compared} requests over the "
        f"{comparison.corpus} corpus are answered differently over "
        f"{comparison.shards['many']} shards; both answers of each are in {comparison.report}\n"
        + "\n".join(lines)
    )


def test_every_identifier_names_an_item_or_an_artifact(comparison: Comparison):
    assert not any(comparison.unknown.values()), {
        mode: sorted(ids)[:10] for mode, ids in comparison.unknown.items()
    }


def test_no_entity_id_crosses_the_wire_in_the_many_shard_mode(comparison: Comparison):
    scan, numbers = comparison.scan, comparison.numbers
    assert scan.id_windows - numbers, "the sweep saw no identifier, so its silence proves nothing"
    leaked = scan.id_windows & numbers
    assert not leaked, f"entity id(s) in an identifier column or field: {sorted(leaked)[:20]}"
    leaked = scan.cursor_windows & numbers
    assert not leaked, f"entity id(s) in the bytes of a cursor: {sorted(leaked)[:20]}"
    for half in (comparison.identity_key.k0, comparison.identity_key.k1):
        assert half not in scan.id_windows | scan.cursor_windows, "an identity key half on the wire"
    assert not scan.bodies_holding_the_key, scan.bodies_holding_the_key[:5]
