"""**The identity rule at the service, against its model** (`oracle.naming`).

Every write names items by `tessera_id` and by the values of fields declared unique. This module
sends ingest batches, changes and member tables to a real `tessera serve` and compares each answer
with the model's: which rows were refused and why, the receipt's counts and `tessera_ids`, the
status a strict request is refused with, and what the deployment then serves, read back through
`/v1/items`.

The corpus is 120 items on one view, each with a distinct integer position. Two fields are unique:
`code`, a keyword, and `num`, a `u64`; some items hold neither, some one, some both. Every value a
batch sets that no item holds is new to the whole run, so a model and a server disagreeing about
one row cannot hide behind a value that happens to collide.

Each test serves its own copy of the bundle, so every test starts from the corpus as built.
"""

from __future__ import annotations

import io
import json
import random
import shutil
import subprocess
from itertools import count
from pathlib import Path

import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.parquet as pq
import pytest
import requests

from oracle import naming
from oracle.harness import (
    CLI_BIN,
    REPO_ROOT,
    ensure_cli_built,
    spawn_server,
    stop_server,
    write_deployment,
)
from oracle.naming import INTEGER, KEYWORD, Creates, Holdings, Names, Refused
from oracle.wire import split_items_frames

VIEW = "world"
N_ITEMS = 120
EXTENT = 1024
GROUPS = "groups"
TOPICS = "topics"

UNIQUE = {"code": KEYWORD, "num": INTEGER}

CONFIG = f"""
[sources]
points = "points.parquet"

[[view]]
name             = "{VIEW}"
extent           = {{ x = [0.0, {EXTENT}.0], y = [0.0, {EXTENT}.0] }}
source           = "points"
point_visibility = {{ default = "public" }}

[[attribute]]
name   = "code"
type   = "keyword"
unique = true
source = "points"

[[attribute]]
name   = "num"
type   = "u64"
unique = true
source = "points"

[[layer]]
name                      = "{GROUPS}"
views                     = ["{VIEW}"]
membership                = "enumerated"
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy                 = {{ kind = "flat" }}

[[layer]]
name                      = "{TOPICS}"
views                     = ["{VIEW}"]
membership                = "enumerated"
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy                 = {{ kind = "flat" }}

  [[layer.content.supplied]]
  name                      = "topic"
  type                      = "text"
  require_member_visibility = "all"
"""


def planted(i: int) -> dict:
    """Item `i` as the points file carries it. Positions are integers on a lattice no batch reuses,
    so every position names one item and reads back exactly."""
    row: dict = {"x": float(1 + i % 12 * 80), "y": float(1 + i // 12 * 80)}
    if i % 5 != 0:
        row["code"] = f"c{i:04d}"
    if i % 3 != 0:
        row["num"] = 1_000 + i
    return row


@pytest.fixture(scope="module")
def pristine_bundle(tmp_path_factory) -> Path:
    ensure_cli_built()
    work = tmp_path_factory.mktemp("identity-rule-fixture")
    rows = [planted(i) for i in range(N_ITEMS)]
    pq.write_table(
        pa.table(
            {
                "x": pa.array([r["x"] for r in rows], pa.float64()),
                "y": pa.array([r["y"] for r in rows], pa.float64()),
                "code": pa.array([r.get("code") for r in rows], pa.string()),
                "num": pa.array([r.get("num") for r in rows], pa.uint64()),
            }
        ),
        work / "points.parquet",
    )
    (work / "identity.toml").write_text(CONFIG)
    bundle = work / "bundle"
    deployment = write_deployment(work / "tessera.toml", bundle=bundle, schema=work / "identity.toml")
    subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(bundle)],
        cwd=REPO_ROOT,
        check=True,
    )
    return bundle


class Deployment:
    """One server over a private copy of the bundle, and the model of what it holds."""

    def __init__(self, server):
        self.server = server
        self.token = server.authorise([])["token"]
        self.batches = count()
        self.holdings = Holdings(unique=dict(UNIQUE), items=self.served())
        assert sorted(map(_canonical, self.holdings.items.values())) == sorted(
            _canonical(planted(i)) for i in range(N_ITEMS)
        ), "the build serves the corpus it was given"

    # -- reads --------------------------------------------------------------------------------

    def _read(self, fields: list[str], filters: dict | None = None) -> pa.Table:
        body: dict = {"view": VIEW, "fields": fields, "order": "map"}
        if fields:
            body["system_fields"] = ["position"]
        if filters is not None:
            body["filters"] = filters
        tables = []
        while True:
            resp = self.server.items(self.token, **body)
            assert resp.status_code == 200, resp.text
            decoded = split_items_frames(resp.content)
            for records, _end in decoded.pages:
                with ipc.open_stream(io.BytesIO(records)) as reader:
                    tables.append(reader.read_all())
            if decoded.trailer["next"] is None:
                break
            body["cursor"] = decoded.trailer["next"]
        return pa.concat_tables(tables)

    def served(self) -> dict[int, dict]:
        """Every served item by `tessera_id`, with the values it holds. A position is rounded to
        the integer it was given at, after checking it is within the grid's half step of one."""
        table = self._read(["code", "num"]).to_pylist()
        out = {}
        for row in table:
            values = {}
            for column in ("code", "num"):
                if row[column] is not None:
                    values[column] = row[column]
            for column in ("x", "y"):
                got = row[f"tessera:{column}"]
                assert abs(got - round(got)) < 1e-3, row
                values[column] = float(round(got))
            out[int(row["tessera_id"])] = values
        return out

    def members(self, layer: str, artifact: str) -> set[int]:
        """The served items the artifact `artifact` of `layer` holds."""
        leaf = {"member_of": {"layer": layer, "artifact": artifact}}
        return {int(t) for t in self._read([], leaf).column("tessera_id").to_pylist()}

    def assert_serves_the_model(self) -> None:
        expected = {t: v for t, v in self.holdings.items.items() if t in self.holdings.visible()}
        assert self.served() == expected

    # -- writes -------------------------------------------------------------------------------

    def ingest(
        self,
        rows: list[dict] | bytes,
        *,
        strict: bool = False,
        batch_id: str | None = None,
        wait: bool = True,
    ) -> requests.Response:
        """One batch, JSON rows or bytes of an Arrow stream, answered once its rows are served
        unless `wait` is false."""
        batch_id = batch_id or f"identity-{next(self.batches)}"
        params = {"strict": str(strict).lower()}
        if wait:
            params["wait"] = "visible"
        if isinstance(rows, bytes):
            content_type, data = "application/vnd.apache.arrow.stream", rows
        else:
            content_type, data = "application/json", json.dumps(rows)
        return requests.post(
            f"{self.server.control_base}/control/ingest",
            params=params,
            headers={
                "Authorization": f"Bearer {self.server.operator_credential}",
                "x-tessera-batch-id": batch_id,
                "x-tessera-view": VIEW,
                "Content-Type": content_type,
            },
            data=data,
            timeout=60,
        )

    def changes(self, changes: list[dict], *, strict: bool = False):
        return requests.post(
            f"{self.server.control_base}/control/changes",
            params={"strict": str(strict).lower()},
            headers={"Authorization": f"Bearer {self.server.operator_credential}"},
            json=changes,
            timeout=60,
        )

    def artifacts(
        self, method: str, layer: str, body: dict | bytes, *, strict: bool = False
    ) -> requests.Response:
        """A publication (`PUT`) or growth (`PATCH`); `body` is JSON, or bytes of an Arrow
        stream."""
        headers = {"Authorization": f"Bearer {self.server.operator_credential}"}
        if isinstance(body, bytes):
            headers["Content-Type"] = "application/vnd.apache.arrow.stream"
            payload = {"data": body}
        else:
            payload = {"json": body}
        return requests.request(
            method,
            f"{self.server.control_base}/control/layers/{layer}/artifacts",
            params={"wait": "visible", "strict": str(strict).lower()},
            headers=headers,
            timeout=60,
            **payload,
        )


def _canonical(values: dict) -> tuple:
    return tuple(sorted(values.items()))


@pytest.fixture
def deployment(pristine_bundle, tmp_path):
    bundle = tmp_path / "bundle"
    shutil.copytree(pristine_bundle, bundle)
    state = tmp_path / "state"
    state.mkdir()
    server, proc = spawn_server(bundle, state)
    try:
        yield Deployment(server)
    finally:
        stop_server(proc)


# ---------------------------------------------------------------------------------------------
# A seeded batch generator
# ---------------------------------------------------------------------------------------------

#: Values no item of the corpus holds, drawn once per process so no two batches share one.
_FRESH = count(1)


def fresh_code() -> str:
    return f"n{next(_FRESH):05d}"


def fresh_num() -> int:
    return 50_000 + next(_FRESH)


_POSITIONS = count(0)


def fresh_position() -> dict:
    """A position off the corpus's lattice, and never given twice."""
    n = next(_POSITIONS)
    return {"x": float(41 + n % 24 * 40), "y": float(41 + n // 24 * 40 % 960)}


def unissued_tessera_id(rng: random.Random, holdings: Holdings) -> str:
    while True:
        candidate = rng.getrandbits(63)
        if candidate not in holdings.items:
            return str(candidate)


def identifiers_of(rng: random.Random, item: int, values: dict) -> dict:
    """A non-empty choice of the identifiers that name `item`."""
    choices = [("tessera_id", str(item))]
    choices += [(column, values[column]) for column in UNIQUE if column in values]
    picked = [c for c in choices if rng.random() < 0.5] or [rng.choice(choices)]
    row = dict(picked)
    if "num" in row and rng.random() < 0.3:
        row["num"] = str(row["num"])
    return row


def generate_batch(rng: random.Random, holdings: Holdings, n: int) -> list[dict]:
    """A batch of `n` rows mixing every case the rule decides."""
    held = sorted(holdings.items)
    set_here: list[tuple[str, object]] = []
    named_here: list[int] = []
    rows: list[dict] = []
    while len(rows) < n:
        kind = rng.randrange(12)
        if kind in (0, 1):  # a new item, with new values or none
            row = fresh_position()
            if rng.random() < 0.6:
                row["code"] = fresh_code()
                set_here.append(("code", row["code"]))
            elif rng.random() < 0.5:
                row["code"] = None
            if rng.random() < 0.6:
                row["num"] = fresh_num()
                set_here.append(("num", row["num"]))
        elif kind == 2 and set_here:  # a value an earlier row of this batch set
            column, value = rng.choice(set_here)
            row = {column: value, **fresh_position()}
        elif kind in (3, 4, 5):  # names one held item
            item = rng.choice(held)
            row = identifiers_of(rng, item, holdings.items[item])
            if rng.random() < 0.5:
                row.update(
                    {"x": holdings.items[item]["x"], "y": holdings.items[item]["y"]}
                    if rng.random() < 0.5
                    else fresh_position()
                )
            if set_here and rng.random() < 0.3:  # a value an earlier row of this batch set
                column, value = rng.choice(set_here)
                row.setdefault(column, value)
            elif "code" not in row and rng.random() < 0.3:
                row["code"] = fresh_code()
                set_here.append(("code", row["code"]))
            named_here.append(item)
        elif kind == 6:  # two held items
            a, b = rng.sample(held, 2)
            row = identifiers_of(rng, a, holdings.items[a])
            other = identifiers_of(rng, b, holdings.items[b])
            clash = [c for c in other if c not in row]
            if not clash:
                continue
            row[clash[0]] = other[clash[0]]
        elif kind == 7:  # a tessera_id naming nothing
            row = {"tessera_id": unissued_tessera_id(rng, holdings)}
            if rng.random() < 0.5:
                row.update(fresh_position())
        elif kind == 8 and named_here:  # an item an earlier row of this batch names
            item = rng.choice(named_here)
            row = identifiers_of(rng, item, holdings.items[item])
        elif kind == 9:  # names nothing, and has no position to create with
            row = {"code": fresh_code()} if rng.random() < 0.5 else {"num": fresh_num()}
        elif kind == 10:  # nulls and a position
            row = {"tessera_id": None, "code": None, "num": None, **fresh_position()}
        elif kind == 11 and set_here:  # a held item with a value an earlier row set, then again
            item = rng.choice(held)
            column, value = rng.choice(set_here)
            rows.append({"tessera_id": str(item), column: value})
            row = identifiers_of(rng, item, holdings.items[item])
            named_here.append(item)
        else:
            continue
        rows.append(row)
    return rows


# ---------------------------------------------------------------------------------------------
# Ingest
# ---------------------------------------------------------------------------------------------


def as_arrow(rows: list[dict]) -> tuple[bytes, list[dict]]:
    """`rows` as one Arrow stream, and the rows as the service reads that stream. Every row carries
    every column, so a cell a JSON row leaves out is a null there, which clears what an item holds;
    a null position is no position."""
    types = {
        "tessera_id": pa.string(),
        "code": pa.string(),
        "num": pa.uint64(),
        "x": pa.float64(),
        "y": pa.float64(),
    }
    columns = sorted({c for row in rows for c in row})
    cells = [
        {c: (int(row[c]) if c == "num" and row.get(c) is not None else row.get(c)) for c in columns}
        for row in rows
    ]
    table = pa.table({c: pa.array([cell[c] for cell in cells], types[c]) for c in columns})
    sink = io.BytesIO()
    with ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    read = [
        {c: v for c, v in cell.items() if c not in naming.POSITION or v is not None}
        for cell in cells
    ]
    return sink.getvalue(), read


def check_ingest(dep: Deployment, rows: list[dict], *, arrow: bytes | None = None) -> list:
    """Send `rows`, compare the receipt with the model, apply the model and compare what is served.
    `arrow` is sent in place of `rows` as JSON, an Arrow stream the service reads as `rows`. Answers
    the model's verdicts."""
    verdicts = naming.resolve_ingest(dep.holdings, rows)
    resp = dep.ingest(rows if arrow is None else arrow)
    assert resp.status_code == 200, resp.text
    receipt = resp.json()
    assert receipt["refused"] == naming.refused(verdicts), rows
    ids = receipt["tessera_ids"]
    assert len(ids) == len(rows)
    created = {}
    for i, (verdict, tid) in enumerate(zip(verdicts, ids)):
        if isinstance(verdict, Refused):
            assert tid is None, (i, rows[i], receipt)
        elif isinstance(verdict, Names):
            assert tid == str(verdict.item), (i, rows[i], receipt)
        else:
            assert tid is not None and int(tid) not in dep.holdings.items, (i, rows[i], receipt)
            created[i] = int(tid)
    assert len(set(created.values())) == len(created), "each created row is its own item"
    counts = naming.apply_ingest(dep.holdings, rows, verdicts, created)
    assert {k: receipt[k] for k in counts} == counts, receipt
    assert receipt["rows"] == len(rows)
    dep.assert_serves_the_model()
    return verdicts


@pytest.mark.parametrize("seed", [1, 2, 3])
def test_generated_ingest_batches_are_resolved_row_by_row_as_the_model_resolves_them(
    deployment, seed
):
    """The middle batch of each seed is sent as Arrow, the others as JSON."""
    rng = random.Random(seed)
    reached: set = set()
    cascaded = 0
    for batch in range(3):
        rows = generate_batch(rng, deployment.holdings, 40)
        arrow = None
        if batch == 1:
            arrow, rows = as_arrow(rows)
        held = deployment.holdings.holders()
        named = [naming.named_by(deployment.holdings, held, row) for row in rows]
        verdicts = check_ingest(deployment, rows, arrow=arrow)
        # A row refused for a value an earlier row set, whose item a later row is kept naming.
        cascaded += sum(
            v == Refused(naming.ONE_VALUE_TWICE)
            and len(named[i] or ()) == 1
            and Names(*named[i]) in verdicts[i + 1 :]
            for i, v in enumerate(verdicts)
        )
        reached |= {v.reason if isinstance(v, Refused) else type(v) for v in verdicts}
    assert reached == {Creates, Names, *naming.REASONS}, "the batches reach every verdict"
    assert cascaded, "a row refused for a value leaves its item to a later row"


def test_the_worked_example_of_two_unique_fields(deployment):
    """Items (a=x, b=p) and (a=k, b=y): (a=x, b=y) is refused, (a=x, b=q) edits the first item's
    `num`, and (a=z, b=q) then names nothing held and creates."""
    dep = deployment
    check_ingest(
        dep,
        [
            {"code": "ex-x", "num": 7001, **fresh_position()},
            {"code": "ex-k", "num": 7002, **fresh_position()},
        ],
    )
    (x,) = (t for t, v in dep.holdings.items.items() if v.get("code") == "ex-x")

    verdicts = check_ingest(dep, [{"code": "ex-x", "num": 7002}, {"code": "ex-x", "num": 7003}])
    assert verdicts == [Refused(naming.NAMES_TWO_ITEMS), Names(x)]
    assert dep.holdings.items[x]["num"] == 7003
    verdicts = check_ingest(dep, [{"code": "ex-z", "num": 7001, **fresh_position()}])
    assert verdicts == [Creates()], "7001 was freed by the edit, so the row names nothing"


def test_a_null_clears_a_unique_value_and_a_deleted_item_frees_its_values(deployment):
    dep = deployment
    held = [t for t, v in dep.holdings.items.items() if "code" in v and "num" in v][:2]
    cleared, deleted = held
    code_cleared = dep.holdings.items[cleared]["code"]
    code_deleted = dep.holdings.items[deleted]["code"]

    check_ingest(dep, [{"tessera_id": str(cleared), "code": None}])
    assert "code" not in dep.holdings.items[cleared]
    check_changes(dep, [{"op": "delete", "match": {"tessera_id": str(deleted)}}])

    verdicts = check_ingest(
        dep,
        [
            {"code": code_cleared, **fresh_position()},
            {"code": code_deleted, **fresh_position()},
            {"tessera_id": str(deleted), **fresh_position()},
        ],
    )
    assert verdicts == [Creates(), Creates(), Refused(naming.UNKNOWN_TESSERA_ID)]


def test_a_strict_batch_is_refused_whole_at_its_first_refused_row(deployment):
    dep = deployment
    rng = random.Random(11)
    rows = generate_batch(rng, dep.holdings, 30)
    verdicts = naming.resolve_ingest(dep.holdings, rows)
    assert naming.first_refusal(verdicts) is not None, "the batch plants a refusal"
    before = dep.served()
    resp = dep.ingest(rows, strict=True)
    assert resp.status_code == naming.INGEST_STRICT_STATUS, resp.text
    assert dep.served() == before, "a refused strict batch writes nothing"

    clean = [row for row, v in zip(rows, verdicts) if not isinstance(v, Refused)]
    clean_verdicts = naming.resolve_ingest(dep.holdings, clean)
    assert naming.first_refusal(clean_verdicts) is None
    resp = dep.ingest(clean, strict=True)
    assert resp.status_code == 200, resp.text
    receipt = resp.json()
    assert receipt["refused"] == []
    created = {i: int(t) for i, (t, v) in enumerate(zip(receipt["tessera_ids"], clean_verdicts))
               if isinstance(v, Creates)}
    counts = naming.apply_ingest(dep.holdings, clean, clean_verdicts, created)
    assert {k: receipt[k] for k in counts} == counts
    dep.assert_serves_the_model()


def test_a_replayed_batch_answers_its_first_acceptance(deployment):
    """A retry answers the first acceptance's `tessera_ids` and `refused` whatever `strict` says.
    The retries are sent before any flush, since a flush forgets the batch ids below the write-ahead
    log it keeps."""
    dep = deployment
    rows = generate_batch(random.Random(21), dep.holdings, 25)
    first = dep.ingest(rows, batch_id="identity-replay", wait=False)
    assert first.status_code == 200, first.text
    assert first.json()["refused"], "the batch plants refusals"
    for strict in (False, True):
        again = dep.ingest(rows, strict=strict, batch_id="identity-replay", wait=False)
        assert again.status_code == 200, again.text
        assert again.json().get("replayed") is True, (first.json(), again.json())
        assert again.json()["tessera_ids"] == first.json()["tessera_ids"]
        assert again.json()["refused"] == first.json()["refused"]
        assert again.json()["created"] == 0


# ---------------------------------------------------------------------------------------------
# Changes
# ---------------------------------------------------------------------------------------------


def check_changes(dep: Deployment, changes: list[dict]) -> list:
    matches = [c["match"] for c in changes]
    verdicts = naming.resolve_addresses(dep.holdings, matches)
    resp = dep.changes(changes)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["refused"] == naming.refused(verdicts), changes
    assert set(body["ignored_columns"]) == naming.ignored_columns(dep.holdings, matches), body
    assert body["accepted"] == naming.apply_changes(dep.holdings, changes, verdicts)
    dep.assert_serves_the_model()
    return verdicts


def test_changes_name_items_by_every_identifier_and_refuse_row_by_row(deployment):
    dep = deployment
    items = {t: v for t, v in dep.holdings.items.items() if "code" in v and "num" in v}
    a, b, c, d, e, f = sorted(items)[:6]
    v = dep.holdings.items
    rng = random.Random(5)
    verdicts = check_changes(
        dep,
        [
            {"op": "suppress", "match": {"tessera_id": str(a)}},
            {"op": "suppress", "match": {"code": v[b]["code"]}},
            {"op": "suppress", "match": {"num": v[c]["num"]}},
            {"op": "suppress", "match": {"num": str(v[d]["num"]), "tessera_id": str(d)}},
            {"op": "suppress", "match": {"code": v[e]["code"], "num": v[f]["num"]}},
            {"op": "suppress", "match": {"code": fresh_code()}},
            {"op": "suppress", "match": {}},
            {"op": "suppress", "match": {"tessera_id": unissued_tessera_id(rng, dep.holdings)}},
            {"op": "suppress", "match": {"code": None, "num": v[a]["num"]}},
        ],
    )
    assert [type(x) for x in verdicts[:4]] == [Names] * 4
    assert [x.reason for x in verdicts[4:8]] == [
        naming.NAMES_TWO_ITEMS,
        naming.NAMES_NO_ITEM,
        naming.NAMES_NO_ITEM,
        naming.UNKNOWN_TESSERA_ID,
    ]

    # A suppressed item is still named; a deleted one names nothing.
    verdicts = check_changes(
        dep,
        [
            {"op": "unsuppress", "match": {"code": v[b]["code"]}},
            {"op": "delete", "match": {"tessera_id": str(c)}},
        ],
    )
    assert verdicts == [Names(b), Names(c)]
    verdicts = check_changes(
        dep,
        [
            {"op": "suppress", "match": {"tessera_id": str(c)}},
            {"op": "suppress", "match": {"code": items[c]["code"]}},
            {"op": "unsuppress", "match": {"tessera_id": str(a)}},
        ],
    )
    assert [x.reason if isinstance(x, Refused) else x for x in verdicts] == [
        naming.UNKNOWN_TESSERA_ID,
        naming.NAMES_NO_ITEM,
        Names(a),
    ]


def test_a_strict_change_request_is_refused_whole_with_the_status_of_its_first_refusal(
    deployment,
):
    dep = deployment
    items = sorted(t for t, v in dep.holdings.items.items() if "code" in v and "num" in v)
    v = dep.holdings.items
    ok = {"op": "suppress", "match": {"tessera_id": str(items[0])}}
    nothing = {"op": "suppress", "match": {"code": fresh_code()}}
    unknown = {"op": "suppress", "match": {"tessera_id": unissued_tessera_id(random.Random(9), dep.holdings)}}
    two = {"op": "suppress", "match": {"code": v[items[1]]["code"], "num": v[items[2]]["num"]}}
    before = dep.served()
    for changes in ([ok, nothing, two], [ok, unknown, two], [ok, two, nothing]):
        verdicts = naming.resolve_addresses(dep.holdings, [c["match"] for c in changes])
        _, reason = naming.first_refusal(verdicts)
        resp = dep.changes(changes, strict=True)
        assert resp.status_code == naming.ADDRESSING_STRICT_STATUS[reason], resp.text
        assert dep.served() == before, "a refused strict request applies nothing"
    resp = dep.changes([ok], strict=True)
    assert resp.status_code == 200, resp.text
    assert resp.json()["refused"] == [] and resp.json()["accepted"] == 1


def test_a_match_key_naming_nothing_is_ignored_and_a_match_left_empty_names_no_item(deployment):
    dep = deployment
    items = sorted(t for t, v in dep.holdings.items.items() if "code" in v)
    v = dep.holdings.items
    verdicts = check_changes(
        dep,
        [
            {"op": "suppress", "match": {"tessera_id": str(items[0]), "colour": "red", "weight": 0.5}},
            {"op": "suppress", "match": {"colour": "blue"}},
            {"op": "suppress", "match": {}},
            {"op": "suppress", "match": {"code": v[items[1]]["code"], "size": 3, "tags": [True, {}]}},
        ],
    )
    assert verdicts == [
        Names(items[0]),
        Refused(naming.NAMES_NO_ITEM),
        Refused(naming.NAMES_NO_ITEM),
        Names(items[1]),
    ]


@pytest.mark.parametrize(
    "matches", [[{}], [{"colour": "red"}], [{}, {"colour": "red", "size": 3}]]
)
def test_a_change_request_naming_items_by_no_column_is_malformed(deployment, matches):
    assert naming.malformed_addressing(deployment.holdings, matches)
    before = deployment.served()
    resp = deployment.changes([{"op": "delete", "match": m} for m in matches])
    assert resp.status_code == 422, resp.text
    assert deployment.served() == before


# ---------------------------------------------------------------------------------------------
# Memberships
# ---------------------------------------------------------------------------------------------


def member_table(rows: list[dict]) -> dict:
    """Rows naming members, as the columnar table the wire takes. A column is present where any row
    carries it, and null in the rows that do not."""
    columns = sorted({c for row in rows for c in row})
    return {c: [row.get(c) for row in rows] for c in columns}


def generate_members(rng: random.Random, holdings: Holdings, n: int) -> list[dict]:
    """Member rows naming one item in most rows, and nothing, two items or an unknown
    `tessera_id` in the rest. Several rows may name one item."""
    held = sorted(holdings.items)
    rows = []
    while len(rows) < n:
        kind = rng.randrange(10)
        if kind < 6:
            item = rng.choice(held)
            rows.append(identifiers_of(rng, item, holdings.items[item]))
        elif kind == 6:
            rows.append({"code": fresh_code()} if rng.random() < 0.5 else {"code": None})
        elif kind == 7:
            a, b = rng.sample([t for t in held if "code" in holdings.items[t]], 2)
            rows.append({"tessera_id": str(a), "code": holdings.items[b]["code"]})
        elif kind == 8:
            rows.append({"tessera_id": unissued_tessera_id(rng, holdings)})
    return rows


def expected_refused(holdings: Holdings, lists: list[tuple[int, str, list[dict]]]) -> list[dict]:
    out = []
    for artifact, name, rows in lists:
        for entry in naming.refused(naming.resolve_addresses(holdings, rows)):
            out.append({"artifact": artifact, "list": name, **entry})
    return out


def named(holdings: Holdings, rows: list[dict]) -> set[int]:
    return {
        v.item for v in naming.resolve_addresses(holdings, rows) if isinstance(v, Names)
    }


def test_a_publication_and_its_growth_name_members_by_the_rule(deployment):
    dep = deployment
    rng = random.Random(31)
    # Some members are suppressed: they are named, and simply not served.
    suppressed = sorted(dep.holdings.items)[::17]
    check_changes(dep, [{"op": "suppress", "match": {"tessera_id": str(t)}} for t in suppressed])

    members = [generate_members(rng, dep.holdings, 30) for _ in range(2)]
    excluding = generate_members(rng, dep.holdings, 20)
    body = {
        "artifacts": [
            {"key": "g0", "members": member_table(members[0])},
            {"key": "g1", "members": member_table(members[1])},
            {"key": "g2", "excluding": member_table(excluding)},
            {"key": "g3", "members": {}},
        ]
    }
    lists = [(0, "members", members[0]), (1, "members", members[1]), (2, "excluding", excluding)]
    resp = dep.artifacts("PUT", GROUPS, body)
    assert resp.status_code == 201, resp.text
    published = resp.json()
    assert published["refused"] == expected_refused(dep.holdings, lists)
    assert published["ignored_columns"] == []
    ids = {row["key"]: row["tessera_id"] for row in published["artifacts"]}

    visible = dep.holdings.visible()
    held = set(dep.holdings.items)
    membership = {
        "g0": named(dep.holdings, members[0]),
        "g1": named(dep.holdings, members[1]),
        "g2": held - named(dep.holdings, excluding),
        "g3": set(),
    }
    for key, expected in membership.items():
        assert dep.members(GROUPS, ids[key]) == expected & visible, key

    # Growth: new rows for g0 and g3, some naming items g0 already holds.
    joining = {"g0": generate_members(rng, dep.holdings, 25), "g3": generate_members(rng, dep.holdings, 10)}
    grow = {"artifacts": [{"key": key, "members": member_table(rows)} for key, rows in joining.items()]}
    resp = dep.artifacts("PATCH", GROUPS, grow)
    assert resp.status_code == 200, resp.text
    grown = resp.json()
    assert grown["refused"] == expected_refused(
        dep.holdings, [(i, "members", rows) for i, rows in enumerate(joining.values())]
    )
    assert grown["ignored_columns"] == []
    for row, (key, rows) in zip(grown["artifacts"], joining.items()):
        new = named(dep.holdings, rows) - membership[key]
        assert row["joined"] == len(new), (key, row)
        membership[key] |= new
        assert dep.members(GROUPS, ids[key]) == membership[key] & visible, key


def test_a_member_table_may_name_members_by_any_mix_of_columns(deployment):
    dep = deployment
    items = sorted(t for t, v in dep.holdings.items.items() if "code" in v and "num" in v)[:4]
    v = dep.holdings.items
    table = {
        "tessera_id": [str(items[0]), None, None, str(items[3])],
        "code": [None, v[items[1]]["code"], None, v[items[3]]["code"]],
        "num": [None, None, str(v[items[2]]["num"]), None],
    }
    resp = dep.artifacts("PUT", GROUPS, {"artifacts": [{"key": "mixed", "members": table}]})
    assert resp.status_code == 201, resp.text
    assert resp.json()["refused"] == []
    tid = resp.json()["artifacts"][0]["tessera_id"]
    assert dep.members(GROUPS, tid) == set(items)


def test_a_member_table_column_naming_nothing_is_ignored(deployment):
    """A column that is neither `tessera_id` nor a unique field is ignored, on a publication and a
    growth; a table left with no identifying column names nothing in each row, and a request in
    which no table has one is malformed."""
    dep = deployment
    items = sorted(t for t, v in dep.holdings.items.items() if "code" in v)
    v = dep.holdings.items
    tagged = [{"tessera_id": str(items[0]), "colour": "red"}, {"code": v[items[1]]["code"], "colour": None}]
    untagged = [{"colour": "blue"}, {"colour": "green"}]
    body = {
        "artifacts": [
            {"key": "tagged", "members": member_table(tagged)},
            {"key": "untagged", "members": member_table(untagged)},
        ]
    }
    resp = dep.artifacts("PUT", GROUPS, body)
    assert resp.status_code == 201, resp.text
    published = resp.json()
    assert set(published["ignored_columns"]) == naming.ignored_columns(dep.holdings, tagged + untagged)
    assert published["refused"] == expected_refused(
        dep.holdings, [(0, "members", tagged), (1, "members", untagged)]
    )
    ids = {row["key"]: row["tessera_id"] for row in published["artifacts"]}
    assert dep.members(GROUPS, ids["tagged"]) == {items[0], items[1]}
    assert dep.members(GROUPS, ids["untagged"]) == set()

    joining = [{"tessera_id": str(items[2]), "weight": 2}]
    resp = dep.artifacts("PATCH", GROUPS, {"artifacts": [{"key": "untagged", "members": member_table(joining)}]})
    assert resp.status_code == 200, resp.text
    assert set(resp.json()["ignored_columns"]) == naming.ignored_columns(dep.holdings, joining)
    assert resp.json()["refused"] == []
    assert dep.members(GROUPS, ids["untagged"]) == {items[2]}

    for method, table in (("PUT", {"colour": ["red"]}), ("PATCH", {"colour": ["red", "blue"]})):
        assert naming.malformed_addressing(dep.holdings, naming.table_rows(table))
        resp = dep.artifacts(method, GROUPS, {"artifacts": [{"key": "untagged", "members": table}]})
        assert resp.status_code == 422, resp.text
    assert dep.members(GROUPS, ids["untagged"]) == {items[2]}


@pytest.mark.parametrize("method", ["PUT", "PATCH"])
def test_a_strict_member_request_is_refused_whole_with_the_status_of_its_first_refusal(
    deployment, method
):
    dep = deployment
    items = sorted(t for t, v in dep.holdings.items.items() if "code" in v)
    v = dep.holdings.items
    if method == "PATCH":
        resp = dep.artifacts("PUT", GROUPS, {"artifacts": [{"key": "held", "members": {"tessera_id": [str(items[0])]}}]})
        assert resp.status_code == 201, resp.text
        held_id = resp.json()["artifacts"][0]["tessera_id"]
    ok = {"tessera_id": str(items[1])}
    nothing = {"code": fresh_code()}
    two = {"tessera_id": str(items[2]), "code": v[items[3]]["code"]}
    for rows in ([ok, nothing, two], [ok, two, nothing]):
        key = "held" if method == "PATCH" else "strict"
        verdicts = naming.resolve_addresses(dep.holdings, rows)
        _, reason = naming.first_refusal(verdicts)
        body = {"artifacts": [{"key": key, "members": member_table(rows)}]}
        resp = dep.artifacts(method, GROUPS, body, strict=True)
        assert resp.status_code == naming.ADDRESSING_STRICT_STATUS[reason], resp.text
    if method == "PATCH":
        assert dep.members(GROUPS, held_id) == {items[0]}, "a refused growth applies nothing"
    else:
        resp = dep.artifacts("PUT", GROUPS, {"artifacts": [{"key": "strict", "members": {}}]})
        assert resp.status_code == 201, resp.text
        assert resp.json()["created"] == 1, "a refused publication created nothing"


def test_a_generating_set_naming_nothing_or_two_items_refuses_the_publication(deployment):
    dep = deployment
    items = sorted(t for t, v in dep.holdings.items.items() if "code" in v)
    v = dep.holdings.items
    good = [{"tessera_id": str(items[0])}, {"code": v[items[1]]["code"]}]
    for extra, status in (
        ({"code": fresh_code()}, 404),
        ({"tessera_id": str(items[2]), "code": v[items[3]]["code"]}, 409),
    ):
        rows = good + [extra]
        body = {
            "artifacts": [
                {
                    "key": "t0",
                    "members": member_table(good),
                    "content": [{"values": ["a topic"], "generated_from": member_table(rows)}],
                }
            ]
        }
        for strict in (False, True):
            resp = dep.artifacts("PUT", TOPICS, body, strict=strict)
            assert resp.status_code == status, resp.text

    body = {
        "artifacts": [
            {
                "key": "t0",
                "members": member_table(good),
                "content": [{"values": ["a topic"], "generated_from": member_table(good)}],
            }
        ]
    }
    resp = dep.artifacts("PUT", TOPICS, body)
    assert resp.status_code == 201, resp.text
    assert resp.json()["created"] == 1, "the refused publications created nothing"
    assert resp.json()["refused"] == []

    # Leaving a generating set names members by the same rule, row by row.
    leaving = [{"code": v[items[1]]["code"]}, {"code": fresh_code()}, {"tessera_id": str(items[0]), "code": v[items[2]]["code"]}]
    grow = {"artifacts": [{"key": "t0", "rank": 0, "leaving": member_table(leaving)}]}
    resp = dep.artifacts("PATCH", TOPICS, grow)
    assert resp.status_code == 200, resp.text
    assert resp.json()["refused"] == expected_refused(dep.holdings, [(0, "leaving", leaving)])
    assert resp.json()["artifacts"][0]["left"] == 1


def test_members_joining_a_generating_set_that_name_nothing_or_two_refuse_the_growth(deployment):
    """Strict or not; members leaving it are refused row by row."""
    dep = deployment
    items = sorted(t for t, v in dep.holdings.items.items() if "code" in v)
    v = dep.holdings.items
    body = {
        "artifacts": [
            {
                "key": "t0",
                "members": {"tessera_id": [str(t) for t in items[:4]]},
                "content": [{"values": ["a topic"], "generated_from": {"tessera_id": [str(items[0])]}}],
            }
        ]
    }
    resp = dep.artifacts("PUT", TOPICS, body)
    assert resp.status_code == 201, resp.text
    tid = resp.json()["artifacts"][0]["tessera_id"]
    before = dep.members(TOPICS, tid)
    joining = {"tessera_id": str(items[1])}
    unknown = {"tessera_id": unissued_tessera_id(random.Random(3), dep.holdings)}
    for bad in ({"code": fresh_code()}, unknown, {"tessera_id": str(items[2]), "code": v[items[3]]["code"]}):
        rows = [joining, bad]
        _, reason = naming.first_refusal(naming.resolve_addresses(dep.holdings, rows))
        grow = {"artifacts": [{"key": "t0", "rank": 0, "members": member_table(rows)}]}
        for strict in (False, True):
            resp = dep.artifacts("PATCH", TOPICS, grow, strict=strict)
            assert resp.status_code == naming.ADDRESSING_STRICT_STATUS[reason], resp.text
            assert dep.members(TOPICS, tid) == before

    grow = {"artifacts": [{"key": "t0", "rank": 0, "members": member_table([joining])}]}
    resp = dep.artifacts("PATCH", TOPICS, grow)
    assert resp.status_code == 200, resp.text
    assert resp.json()["refused"] == []


def test_the_arrow_growth_names_members_by_the_struct_fields(deployment):
    dep = deployment
    items = sorted(t for t, v in dep.holdings.items.items() if "num" in v)
    v = dep.holdings.items
    resp = dep.artifacts("PUT", GROUPS, {"artifacts": [{"key": "arrow", "members": {}}]})
    assert resp.status_code == 201, resp.text
    tid = resp.json()["artifacts"][0]["tessera_id"]

    rows = [
        {"tessera_id": str(items[0]), "num": None},
        {"tessera_id": None, "num": v[items[1]]["num"]},
        {"tessera_id": str(items[2]), "num": v[items[2]]["num"]},
        {"tessera_id": None, "num": fresh_num()},
        {"tessera_id": str(items[3]), "num": v[items[4]]["num"]},
    ]
    member = pa.struct([("tessera_id", pa.string()), ("num", pa.uint64())])
    schema = pa.schema([("key", pa.string()), ("members", pa.list_(member))], metadata={"level": "0"})
    batch = pa.record_batch([pa.array(["arrow"]), pa.array([rows], pa.list_(member))], schema=schema)
    sink = io.BytesIO()
    with ipc.new_stream(sink, schema) as writer:
        writer.write_batch(batch)
    resp = dep.artifacts("PATCH", GROUPS, sink.getvalue())
    assert resp.status_code == 200, resp.text
    assert resp.json()["refused"] == expected_refused(dep.holdings, [(0, "members", rows)])
    assert dep.members(GROUPS, tid) == named(dep.holdings, rows)


# ---------------------------------------------------------------------------------------------
# An ingest that can only edit
# ---------------------------------------------------------------------------------------------


def test_an_ingest_that_can_only_edit_and_names_items_by_no_column_is_malformed(deployment):
    """A batch in which no row carries a position can only edit items, so it needs `tessera_id` or
    a unique column to name them by. One carrying either, or a row that can create, is resolved row
    by row."""
    dep = deployment
    before = dep.served()
    for rows in ([{}, {}], [{"x": None, "y": None}]):
        assert naming.malformed_ingest(dep.holdings, rows)
        assert dep.ingest(rows).status_code == 422
        assert dep.served() == before
    resp = dep.ingest(as_arrow([{"x": None, "y": None}])[0])
    assert resp.status_code == 422, resp.text
    assert dep.served() == before

    for rows in ([{}, {"code": None}], [{"tessera_id": None}], [{}, fresh_position()]):
        assert not naming.malformed_ingest(dep.holdings, rows)
        check_ingest(dep, rows)
