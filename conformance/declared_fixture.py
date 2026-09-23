"""Two deployments of one corpus: one built with a declaration, one declared at a running service.

The corpus is small and synthetic, every value a pure function of an item's source id, so the two
deployments are fed exactly the same items. A deployment writes its files, runs `tessera build`
into a bundle root of its own and serves it through `suite.driver.SuiteHarness`, with the tick
clock pinned and the automatic folds off, so the test decides when a flush or a fold happens.

`observe` records one battery per principal and turns each answer into a form both deployments
can be compared in. The two assign entity ids differently (a build sorts every item at once; a
live ingest appends), so anything carrying an identifier is rekeyed here, and only here:

- a point by its `fx` column, which holds the source id;
- an item card by the `fx` it was asked for;
- an artifact by `(layer, key)`, and its parents by their keys;
- a rendered category's code on a point by its key. Codes themselves are compared on the
  category lists and the typeahead. Both deployments draw a code at random for every value they
  do not pin, which is what `data-model.md` describes, so a drawn code is not compared across
  them. Whatever the live side serves is held to one code per key across every principal and
  every stage.

Rows whose order is set by `tessera_id` (points within a tile, browse rows with equal counts)
are sorted by the rekeyed identity instead. The artifacts frame's rows are compared by key, and
the order they were served in is kept beside them as its own answer: a build serves a level's
artifacts in key order and a running service in the order they were published, which the test
pins as a known difference.
"""

from __future__ import annotations

import base64
import hashlib
import json
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

import pyarrow as pa
import pyarrow.parquet as pq
import requests

from oracle.harness import CLI_BIN, OPERATOR_CREDENTIAL, REPO_ROOT, build_env, write_deployment
from suite.battery import (
    ArtifactCard,
    Browse,
    Categories,
    DEEP_ZOOM,
    Item,
    Meta,
    Suggest,
    Viewport,
    record_one,
)
from suite.canonical import Streamed
from suite.driver import SuiteHarness, poll
from suite.verification import streams_table

ID_KEY_HEX = "d0d1d2d3d4d5d6d7d8d9dadbdcdddedf"

#: Items in the corpus, and how many of them the live deployment's build holds. The rest reach it
#: through `/control/ingest`, so both doors are exercised.
N_ITEMS = 1600
N_BUILT = 1280

WORLD = "world"
WORLD_EXTENT = (0.0, 1024.0, 0.0, 1024.0)
BBOX = (0.0, 0.0, 1024.0, 1024.0)

#: Above every principal's visible total, so every tile is saturated and points compare exactly.
K = 100_000

PRINCIPALS = {
    "everyone": ("pa", "pb", "pc"),
    "two": ("pb", "pc"),
    "few": ("pc",),
}


def _hash(*parts: object) -> int:
    return int(hashlib.sha256(":".join(map(str, parts)).encode()).hexdigest()[:16], 16)


def access_of(i: int) -> str:
    """One label per item, drawn from a hash so it does not line up with the modular choices the
    columns and views make: `pc` is about one item in twenty, so the `few` principal sees little."""
    r = _hash("access", i) % 20
    if r == 19:
        return "pc"
    return "pb" if r >= 10 else "pa"


def external_id(i: int) -> str:
    """The id `--mint-external-ids` gives an item built with entity id `i`, base64 as JSON carries
    it."""
    return base64.b64encode(i.to_bytes(8, "little")).decode()


def position(view_seed: int, i: int, extent=WORLD_EXTENT) -> tuple[float, float]:
    """A position inside the frame with a margin, as a multiple of 1/64 so every reader holds it
    exactly."""
    x_min, x_max, y_min, y_max = extent
    h = _hash(view_seed, i)
    span_x = int((x_max - x_min - 2) * 64)
    span_y = int((y_max - y_min - 2) * 64)
    x = x_min + 1 + (h % span_x) / 64
    y = y_min + 1 + ((h >> 24) % span_y) / 64
    return x, y


# ---------------------------------------------------------------------------------------------
# Columns: a name, a declaration, a value per item
# ---------------------------------------------------------------------------------------------


@dataclass(frozen=True)
class Column:
    """One attribute: how a build declares it, how the control plane declares it, its values.

    `arrow` is the parquet type the build reads; `value(i)` is `None` where the item carries no
    value. A category's values are keys.
    """

    name: str
    type: str
    arrow: pa.DataType
    value: Callable[[int], object]
    index: bool = False
    render: bool = False
    vocabulary: str | None = None
    scope: str | None = None

    def toml(self) -> str:
        lines = [f'[[attribute]]\nname = "{self.name}"\ntype = "{self.type}"']
        if self.vocabulary:
            lines.append(f'vocabulary = "{self.vocabulary}"')
        if self.index:
            lines.append("index = true")
        if self.render:
            lines.append("render = true")
        if self.scope:
            lines.append(f'scope = {{ group = "{self.scope}" }}')
        return "\n".join(lines) + "\n"

    def payload(self) -> dict:
        body = {"name": self.name, "type": self.type, "index": self.index, "render": self.render}
        if self.vocabulary:
            body["vocabulary"] = self.vocabulary
        if self.scope:
            body["scope"] = {"group": self.scope}
        return body


@dataclass(frozen=True)
class Vocabulary:
    """A value set. Its values and titles are written to a small source file for the build and
    sent in the body of `PUT /control/vocabularies/{name}` at a running service."""

    name: str
    width: str
    value_set: str
    visibility: str
    values: tuple[tuple[str, str | None], ...] = ()

    def toml(self) -> str:
        text = (
            f'[[vocabulary]]\nname = "{self.name}"\nwidth = "{self.width}"\n'
            f'value_set = "{self.value_set}"\nvisibility = "{self.visibility}"\n'
        )
        if self.values:
            text += f'source = "{self.source_name}"\n'
        return text

    @property
    def source_name(self) -> str:
        return f"vocab_{self.name}"

    def table(self) -> pa.Table:
        return pa.table(
            {
                "key": pa.array([k for k, _ in self.values], type=pa.string()),
                "title": pa.array([t for _, t in self.values], type=pa.string()),
            }
        )

    def payload(self) -> dict:
        body = {"value_set": self.value_set, "visibility": self.visibility, "width": self.width}
        if self.values:
            body["values"] = [
                {"key": k, **({"title": t} if t is not None else {})} for k, t in self.values
            ]
        return body


def fx_column() -> Column:
    """The join column every deployment carries from its build: the source id, rendered, so a
    served point names its item without a second request."""
    return Column("fx", "u64", pa.uint64(), lambda i: i, render=True)


# ---------------------------------------------------------------------------------------------
# A deployment
# ---------------------------------------------------------------------------------------------


@dataclass
class Corpus:
    """What one deployment's build reads: the declaration's blocks, a points file per view, the
    vocabularies, and any other source file by name.

    `points` maps a source name to `(source ids, columns, position seed, frame)`. Every points
    file carries `entity_id`, `x`, `y` and `access`, and then the columns given for it.
    """

    blocks: list[str]
    points: dict[str, tuple[list[int], list[Column], int, tuple]] = field(default_factory=dict)
    vocabularies: list[Vocabulary] = field(default_factory=list)
    extra: dict[str, pa.Table] = field(default_factory=dict)

    def write(self, work: Path) -> Path:
        work.mkdir(parents=True, exist_ok=True)
        sources = []
        for name, (ids, columns, seed, extent) in self.points.items():
            sources.append(f'{name} = "{name}.parquet"')
            xs, ys = zip(*(position(seed, i, extent) for i in ids)) if ids else ((), ())
            data = {
                "entity_id": pa.array(ids, type=pa.uint64()),
                "x": pa.array(xs, type=pa.float64()),
                "y": pa.array(ys, type=pa.float64()),
                "access": pa.array([access_of(i) for i in ids], type=pa.string()),
            }
            for column in columns:
                data[column.name] = pa.array([column.value(i) for i in ids], type=column.arrow)
            pq.write_table(pa.table(data), work / f"{name}.parquet")
        for vocabulary in self.vocabularies:
            if vocabulary.values:
                sources.append(f'{vocabulary.source_name} = "{vocabulary.source_name}.parquet"')
                pq.write_table(vocabulary.table(), work / f"{vocabulary.source_name}.parquet")
        for name, table in self.extra.items():
            sources.append(f'{name} = "{name}.parquet"')
            pq.write_table(table, work / f"{name}.parquet")
        text = "[sources]\n" + "\n".join(sources) + "\n\n"
        text += f'[defaults]\nsource = "{WORLD}"\nentity_id_field = "entity_id"\n'
        text += f'allocation_view = "{WORLD}"\n\n'
        text += "\n".join(v.toml() for v in self.vocabularies) + "\n"
        text += "\n".join(self.blocks)
        schema = work / "declaration.toml"
        schema.write_text(text)
        return schema


def plain_view_toml(name: str = WORLD, extent=WORLD_EXTENT) -> str:
    """A public plain view reading its points from the source of its own name."""
    x0, x1, y0, y1 = extent
    return (
        f'[[view]]\nname = "{name}"\nextent = {{ x = [{x0}, {x1}], y = [{y0}, {y1}] }}\n'
        f'source = "{name}"\nvisibility = "public"\n'
        'point_visibility = { field = "access", default = "public" }\n'
    )


class Refused(RuntimeError):
    """A control-plane request answered with a status the caller did not expect."""

    def __init__(self, method: str, path: str, status: int, text: str):
        super().__init__(f"{method} {path} answered {status}: {text}")
        self.status = status


class NeverVisible(RuntimeError):
    """A flush waited on with `wait=visible` did not publish."""


class Deployment:
    """One bundle root, built once and served by its own harness.

    Every deployment started and not yet stopped is in `RUNNING`, so a caller whose setup fails
    part way can stop what it started."""

    RUNNING: list["Deployment"] = []

    def __init__(self, work: Path, corpus: Corpus):
        self.work = work
        self.bundle = work / "bundle"
        schema = corpus.write(work / "corpus")
        deployment = write_deployment(work / "tessera-build.toml", bundle=self.bundle, schema=schema)
        subprocess.run(
            [
                str(CLI_BIN), "build",
                "--deployment", str(deployment),
                "--out", str(self.bundle),
                "--mint-external-ids",
            ],
            cwd=REPO_ROOT,
            env=build_env(ID_KEY_HEX),
            check=True,
            capture_output=True,
        )
        run_dir = work / "run"
        run_dir.mkdir(exist_ok=True)
        self.harness = SuiteHarness(
            bundle_root=self.bundle,
            run_dir=run_dir,
            grants=PRINCIPALS["everyone"],
            view_id=WORLD,
            bbox=BBOX,
            k=K,
            automatic_folds=False,
        )
        self.harness.spawn()
        Deployment.RUNNING.append(self)
        self._ready()

    @classmethod
    def stop_all(cls) -> None:
        while cls.RUNNING:
            cls.RUNNING[-1].stop()

    @property
    def server(self):
        return self.harness.server

    def _ready(self) -> None:
        poll(
            lambda: all(p["readiness"] for p in self.harness.status()["partitions"]),
            "the bundle never became ready",
            timeout=30.0,
        )

    def stop(self) -> None:
        self.harness.stop()
        if self in Deployment.RUNNING:
            Deployment.RUNNING.remove(self)

    def restart(self) -> None:
        self.harness.stop()
        self.harness.spawn()
        self._ready()

    def fold(self) -> None:
        """Ask for a fold, wait for it to land, then for the publication after it."""
        self.server.compact()
        self.publish()

    def publish(self) -> None:
        """Pull a publication cycle and wait until it is visible: every write sent before it is
        then what a new session sees."""
        resp = self.control("POST", "/control/flush?wait=visible", expect=(202,))
        if resp.json().get("visible") is not True:
            raise NeverVisible(f"the flush did not become visible: {resp.text}")

    def control(self, method: str, path: str, *, expect=(200, 201), view=None, **kwargs):
        headers = {"Authorization": f"Bearer {OPERATOR_CREDENTIAL}"}
        if view is not None:
            headers["x-tessera-view"] = view
        headers.update(kwargs.pop("headers", {}))
        resp = requests.request(
            method, f"{self.server.control_base}{path}", headers=headers, timeout=60, **kwargs
        )
        if resp.status_code not in expect:
            raise Refused(method, path, resp.status_code, resp.text)
        return resp

    def rows(self, route: str, rows: list[dict], batch: str, view: str | None = None) -> dict:
        """`/control/ingest` or `/control/values`, as JSON, in pages the server accepts."""
        out: dict = {}
        for n, start in enumerate(range(0, len(rows), 500)):
            resp = self.control(
                "POST",
                route,
                expect=(200,),
                view=view,
                headers={"x-tessera-batch-id": f"{batch}-{n}", "content-type": "application/json"},
                data=json.dumps(rows[start : start + 500]),
            )
            for key, value in resp.json().items():
                if isinstance(value, int) and not isinstance(value, bool):
                    out[key] = out.get(key, 0) + value
        return out


def point_rows(ids, columns: list[Column], seed: int = 0, extent=WORLD_EXTENT) -> list[dict]:
    """Ingest rows for items `ids`: the fixed columns and one per column given, null where the
    item has no value, since a column a batch carries is on every row of it."""
    out = []
    for i in ids:
        x, y = position(seed, i, extent)
        row = {"external_id": external_id(i), "x": x, "y": y, "access": [access_of(i)]}
        for column in columns:
            row[column.name] = column.value(i)
        out.append(row)
    return out


def value_rows(ids, columns: list[Column]) -> list[dict]:
    """Values rows for items `ids`: the address, then one cell per column, null where the item
    has no value."""
    out = []
    for i in ids:
        row = {"external_id": external_id(i)}
        for column in columns:
            row[column.name] = column.value(i)
        out.append(row)
    return out


# ---------------------------------------------------------------------------------------------
# What is asked, and how an answer is made comparable
# ---------------------------------------------------------------------------------------------


@dataclass(frozen=True)
class Plan:
    """The battery for one comparison, stated without any identifier a deployment assigns."""

    views: tuple[str, ...] = (WORLD,)
    #: `(label, expression, views)`: each is asked as a filter on the views named, or on every view
    #: where `views` is None. The first is also asked as a highlight.
    filters: tuple[tuple[str, dict, tuple[str, ...] | None], ...] = ()
    #: Category columns to enumerate, pinned with `@key` where group-scoped.
    categories: tuple[str, ...] = ()
    #: `(column, q, counts)` for the typeahead.
    suggest: tuple[tuple[str, str, bool], ...] = ()
    #: Items to open, by source id.
    items: tuple[int, ...] = ()
    #: Layers to ask for on the viewport and to browse.
    layers: tuple[str, ...] = ()
    #: `(view, extent)` for every view whose frame is not the world's, so its whole-map box is
    #: stated in its own coordinates.
    frames: tuple[tuple[str, tuple[float, float, float, float]], ...] = ()

    def bbox(self, view: str) -> tuple[float, float, float, float]:
        x0, x1, y0, y1 = dict(self.frames).get(view, WORLD_EXTENT)
        return (x0, y0, x1, y1)


def _canon(value) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


@dataclass
class Observation:
    """What one deployment answered: `principal -> label -> answer`, ready to compare, and beside
    it `principal -> column -> {key: code}` as served, which is not compared across deployments."""

    answers: dict[str, dict[str, object]]
    codes: dict[str, dict[str, dict[str, int]]]


def observe(server, plan: Plan) -> Observation:
    """Every principal's answers to `plan`, rekeyed so two deployments compare."""
    answers, codes = {}, {}
    # Shared across principals, the widest first, so a narrower principal is also asked for the
    # items and artifacts it may not see, and answers those as withheld.
    fx_of: dict[int, int] = {}
    artifact_of: dict[int, tuple[str, str]] = {}
    for name, terms in PRINCIPALS.items():
        answers[name], codes[name] = _observe_one(server, terms, plan, fx_of, artifact_of)
    return Observation(answers, codes)


def _category_columns(meta: dict) -> set[str]:
    """Category columns. No vocabulary here pins a code, so both deployments draw every code at
    random."""
    families = meta.get("declared_scalars", []) + meta.get("scoped_scalars", [])
    return {d["name"] for d in families if d.get("category")}


def _observe_one(
    server, terms, plan: Plan, fx_of: dict[int, int], artifact_of: dict[int, tuple[str, str]]
) -> tuple[dict[str, object], dict]:
    token = server.authorise(list(terms))["token"]
    out: dict[str, object] = {}
    served_codes: dict[str, dict[str, int]] = {}
    meta = record_one(server, token, Meta()).payload
    out["meta"] = normalise_meta(meta)
    keys_of = _code_maps(server, token, meta)
    drawn = _category_columns(meta)

    def without_drawn_codes(column: str, values: list[dict]) -> None:
        if column.split("@")[0] in drawn:
            for value in values:
                value["code"] = "drawn at random"

    for column in plan.categories:
        try:
            pages = record_one(server, token, Categories(column)).payload["pages"]
        except requests.HTTPError as refused:
            # A column or pinned view the deployment does not hold is a real answer here.
            out[f"categories {column}"] = {"status": refused.response.status_code}
            continue
        served_codes[column] = {v["key"]: v["code"] for page in pages for v in page["values"]}
        for page in pages:
            without_drawn_codes(column, page["values"])
        out[f"categories {column}"] = pages
    for column, q, counts in plan.suggest:
        answer = record_one(server, token, Suggest(column, q, counts=counts)).payload
        if answer["status"] == 200:
            without_drawn_codes(column, answer["body"]["values"])
        out[f"suggest {column} {q!r} counts={counts}"] = answer

    layers = _canon(list(plan.layers)) if plan.layers else None
    for view in plan.views:
        bbox = plan.bbox(view)
        asks = [
            (f"{view} z0", Viewport(view, 0, bbox=bbox, k=K, underlay_offset=2, layers=layers)),
            (f"{view} z3", Viewport(view, 3, bbox=bbox, k=K, underlay_offset=2, layers=layers)),
            (
                f"{view} z{DEEP_ZOOM}",
                Viewport(view, DEEP_ZOOM, bbox=bbox, k=K, underlay_offset=1, layers=layers),
            ),
        ]
        filters = [(label, expr) for label, expr, on in plan.filters if on is None or view in on]
        for label, expr in filters:
            for zoom in (2, 4):
                asks.append(
                    (
                        f"{view} z{zoom} filter {label}",
                        Viewport(view, zoom, bbox=bbox, k=K, filters=_canon(expr),
                                 underlay_offset=1, layers=layers),
                    )
                )
        if filters:
            label, expr = filters[0]
            asks.append(
                (
                    f"{view} z1 highlight {label}",
                    Viewport(view, 1, bbox=bbox, k=K, highlight=_canon(expr), underlay_offset=1,
                             layers=layers),
                )
            )
        for label, query in asks:
            try:
                canon = record_one(server, token, query)
            except requests.HTTPError as refused:
                out[f"viewport {label}"] = {"status": refused.response.status_code}
                continue
            out[f"viewport {label}"] = normalise_viewport(canon, keys_of, fx_of, artifact_of)

    for i in plan.items:
        tessera = next((t for t, fx in fx_of.items() if fx == i), None)
        if tessera is None:
            out[f"item {i}"] = "not served"
            continue
        out[f"item {i}"] = record_one(server, token, Item(tessera)).payload

    for layer in plan.layers:
        for view in plan.views:
            roots = record_one(server, token, Browse(view, layer)).payload
            out[f"browse {layer} {view} roots"] = normalise_browse(roots, artifact_of)
            search = record_one(server, token, Browse(view, layer, q="a")).payload
            out[f"browse {layer} {view} q=a"] = normalise_browse(search, artifact_of)
            for label, expr, on in plan.filters:
                if on is None or view in on:
                    browse = Browse(view, layer, filters=_canon(expr))
                    filtered = record_one(server, token, browse).payload
                    out[f"browse {layer} {view} filter {label}"] = normalise_browse(
                        filtered, artifact_of
                    )
            held = sorted((k, t) for t, (lay, k) in artifact_of.items() if lay == layer)
            for key, tessera in held:
                children = record_one(server, token, Browse(view, layer, parent=tessera)).payload
                out[f"browse {layer} {view} children of {key}"] = normalise_browse(
                    children, artifact_of
                )
                card = record_one(server, token, ArtifactCard(tessera, view)).payload
                out[f"artifact {layer} {key} on {view}"] = card
    return out, served_codes


def normalise_meta(meta: dict) -> dict:
    """`/v1/meta` with the one field that names a deployment's identifier space removed."""
    meta = json.loads(json.dumps(meta))
    meta.pop("idset", None)
    return meta


def _code_maps(server, token: str, meta: dict) -> dict[str, dict[int, str]]:
    """`column -> {code: key}` for every rendered category, from this deployment's own list."""
    out: dict[str, dict[int, str]] = {}
    for d in meta.get("declared_scalars", []):
        if d.get("category") is None or not d.get("render"):
            continue
        pages = record_one(server, token, Categories(d["name"])).payload["pages"]
        out[d["name"]] = {v["code"]: v["key"] for page in pages for v in page["values"]}
    return out


def normalise_viewport(
    canon: Streamed,
    keys_of: dict[str, dict[int, str]],
    fx_of: dict[int, int],
    artifact_of: dict[int, tuple[str, str]],
) -> dict:
    """The five surfaces of one viewport answer, with points named by `fx` and artifacts by key.

    Records every `tessera_id -> fx` and `tessera_id -> (layer, key)` it sees into the two maps,
    which the item and browse requests then read.
    """
    tiles = streams_table(canon.tiles)
    underlay = streams_table(canon.underlay)
    out: dict[str, object] = {
        "tiles": sorted(tuple(r.values()) for r in tiles.to_pylist()) if tiles else [],
        "underlay": sorted(tuple(r.values()) for r in underlay.to_pylist()) if underlay else None,
        "trailer": json.loads(canon.trailer),
    }

    artifacts = streams_table(canon.artifacts)
    rows = artifacts.to_pylist() if artifacts is not None else []
    local = {row["tessera_id"]: (row["layer"], row["key"]) for row in rows}
    artifact_of.update(local)

    def key(tessera):
        if tessera is None:
            return None
        return local[tessera][1] if tessera in local else f"unserved artifact {tessera}"

    # Kept apart from the rows, which are compared by key, so an order difference is reported as
    # one and cannot hide a difference in what is served.
    out["artifact order"] = [[row["layer"], row["key"]] for row in rows]
    for row in rows:
        row.pop("tessera_id")
        row["parent_ids"] = sorted(key(p) for p in row.get("parent_ids") or [])
        if "target" in row:
            row["target"] = key(row["target"])
    out["artifacts"] = sorted(rows, key=lambda r: (r["layer"], r["key"] or "", r["rung"]))

    points = streams_table(canon.points)
    served = []
    for row in points.to_pylist() if points is not None else []:
        fx_of[row.pop("tessera_id")] = row["fx"]
        for name, value in row.items():
            if name in keys_of:
                row[name] = keys_of[name].get(value, value) if value else None
            elif name.startswith("membership:"):
                row[name] = (
                    sorted(key(v) for v in value) if isinstance(value, list) else key(value)
                )
        served.append(row)
    out["points"] = sorted(served, key=lambda r: (r.get("code"), r["fx"]))
    return out


class Unordered(Exception):
    """A served page is not in the order the contract gives it."""


def normalise_browse(answer: dict, artifact_of: dict[int, tuple[str, str]]) -> dict:
    """A browse page with identifiers replaced by keys.

    The page is ordered by count, descending, and then by `tessera_id`, which the two deployments
    number differently. So each side's counts are checked never to rise down the page, and rows
    are re-sorted by key only within a run of equal counts.
    """
    if answer["status"] != 200:
        return answer
    body = json.loads(json.dumps(answer["body"]))

    def key(tessera: str) -> str:
        found = artifact_of.get(int(tessera))
        return found[1] if found else f"unmapped:{tessera}"

    for group in ("artifacts", "parents"):
        rows = body.get(group, [])
        counts = [row.get("matched_count", row["masked_count"]) for row in rows]
        if any(later > earlier for earlier, later in zip(counts, counts[1:])):
            raise Unordered(f"browse {group} counts rise down the page: {counts}")
        for row in rows:
            artifact_of.setdefault(int(row["tessera_id"]), (None, row.get("key")))
            row["tessera_id"] = key(row["tessera_id"])
            row["parent_ids"] = sorted(key(p) for p in row["parent_ids"])
        runs: list[list[dict]] = []
        for row, count in zip(rows, counts):
            if runs and count == runs[-1][0][1]:
                runs[-1].append((row, count))
            else:
                runs.append([(row, count)])
        body[group] = [
            row for run in runs for row, _ in sorted(run, key=lambda rc: rc[0]["tessera_id"])
        ]
    if "next" in body:
        body["next"] = "present"
    return {"status": 200, "body": body}


@dataclass
class Difference:
    """One answer the two deployments disagree on, and every place inside it where they part."""

    principal: str
    label: str
    places: list[tuple[str, object, object]]

    def __str__(self) -> str:
        shown = "\n".join(
            f"    at {path}: built {_short(x)}, live {_short(y)}" for path, x, y in self.places[:4]
        )
        more = f"\n    and {len(self.places) - 4} more" if len(self.places) > 4 else ""
        return f"[{self.principal}] {self.label}:\n{shown}{more}"


def differences(built: dict, live: dict) -> list[Difference]:
    """Every answer that differs, per principal and label."""
    out = []
    for principal in PRINCIPALS:
        a, b = built[principal], live[principal]
        for label in sorted(set(a) | set(b)):
            if a.get(label) != b.get(label):
                out.append(Difference(principal, label, _parting(a.get(label), b.get(label))))
    return out


def split(found: list[Difference], patterns: tuple[tuple[str, str], ...]):
    """`(the rest, the matching)`: the places where some `(label, path)` pair of patterns fully
    matches the answer's label and the path inside it, taken out of `found` into differences of
    their own."""

    def hit(d: Difference, place) -> bool:
        return any(
            re.fullmatch(label, d.label) and re.fullmatch(path, place[0])
            for label, path in patterns
        )

    rest, matching = [], []
    for d in found:
        mine = [p for p in d.places if hit(d, p)]
        others = [p for p in d.places if p not in mine]
        if mine:
            matching.append(Difference(d.principal, d.label, mine))
        if others:
            rest.append(Difference(d.principal, d.label, others))
    return rest, matching


def _parting(a, b, path: str = "") -> list[tuple[str, object, object]]:
    """Where two JSON-shaped values differ: dicts by key, lists by position and length."""
    if isinstance(a, dict) and isinstance(b, dict):
        found = []
        for key in sorted(set(a) | set(b), key=str):
            if a.get(key, _MISSING) != b.get(key, _MISSING):
                found += _parting(a.get(key, _MISSING), b.get(key, _MISSING), f"{path}.{key}")
        return found
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        found = []
        for n, (x, y) in enumerate(zip(a, b)):
            if x != y:
                found += _parting(x, y, f"{path}[{n}]")
        if len(a) != len(b):
            found.append((f"{path} length", len(a), len(b)))
        return found
    return [(path or "(whole)", a, b)]


_MISSING = "(absent)"


def _short(value, limit: int = 300) -> str:
    text = json.dumps(value, sort_keys=True, default=str)
    return text if len(text) <= limit else text[:limit] + f"... ({len(text)} chars)"


__all__ = [
    "BBOX",
    "Column",
    "Corpus",
    "Deployment",
    "K",
    "N_BUILT",
    "N_ITEMS",
    "NeverVisible",
    "PRINCIPALS",
    "Plan",
    "Refused",
    "Vocabulary",
    "WORLD",
    "WORLD_EXTENT",
    "access_of",
    "Observation",
    "Difference",
    "differences",
    "split",
    "external_id",
    "fx_column",
    "observe",
    "point_rows",
    "plain_view_toml",
    "position",
    "value_rows",
]
