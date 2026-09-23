"""**An artifact's own access label**, black-box, against `oracle.artifact_label_fixture`.

The oracle computes from the planting rules which artifacts each principal is served, with what
masked count, which served parent and which served target. Every assertion here compares the
running binary with that answer, over a bundle built from artifact sources and over a service the
same artifacts were published into, one of them labelled by a fill after its publication. Each is
checked live (the published service by the server that took the writes, straight after the fill),
after a restart, after a fold, and after a restart following the fold, which is when every record
is read back from a packed extent.

What is compared, per principal: the artifacts frame (keys, masked counts, parent links, targets),
the points frame's membership columns, the identifier route (status and body, a withheld artifact
answering exactly as an identifier nothing was issued under), `member_of` counts, and browse's
roots, children and search, walked page by page through `next`.
"""

from __future__ import annotations

import io

import pyarrow.ipc as ipc
import pytest
import requests

from oracle import artifact_label_fixture as fx
from oracle.harness import spawn_server, stop_server
from oracle.wire import FRAME_POINTS, decode_frames, decode_viewport_artifacts, split_frames

WHOLE = [0.0, 0.0, fx.EXTENT_MAX, fx.EXTENT_MAX]
WIDEST = ["3", "1", "2", fx.UNCARRIED]
#: An identifier this deployment never issued: the answer a withheld artifact must match.
NEVER_ISSUED = "1"


def viewport(server, token):
    return server.viewport(token, fx.VIEW_ID, 0, WHOLE, k=1000, artifact_budget=1000)


def served(server, terms) -> tuple[fx.Served, dict]:
    """The engine's answer in the oracle's shape, and the identifiers it served, by key."""
    token = server.authorise(terms)["token"]
    body = viewport(server, token)
    rows = decode_viewport_artifacts(body)
    key_of = {row.tessera_id: (row.layer, row.key) for row in rows}
    out = {}
    for row in rows:
        parents = [key_of[p][1] for p in row.parent_ids]
        assert len(parents) <= 1, row
        target = key_of[row.target][1] if row.target is not None else None
        out[(row.layer, row.key)] = (row.masked_count, parents[0] if parents else None, target)
    membership_names_only_served(body, {row.tessera_id for row in rows})
    return out, {(row.layer, row.key): row.tessera_id for row in rows}


def membership_names_only_served(body: bytes, served_ids: set[int]) -> None:
    """Every value of a points frame's membership column names an artifact of the same response."""
    seen = 0
    for kind, payload in split_frames(body):
        if kind != FRAME_POINTS:
            continue
        table = ipc.open_stream(io.BytesIO(payload)).read_all()
        for name in table.column_names:
            if name.startswith("membership:"):
                for value in table.column(name).to_pylist():
                    if value is not None:
                        seen += 1
                        assert value in served_ids, (name, value)
    assert seen, "the response carried membership values to check"


def drill(server, token, tessera_id):
    response = requests.post(
        f"{server.viewer_base}/v1/artifacts/{tessera_id}",
        headers={"Authorization": f"Bearer {token}"},
        json={"view": fx.VIEW_ID},
        timeout=30,
    )
    return response.status_code, response.text


def browse_all(server, token, **form) -> list[dict]:
    """Every row of one browse form, walked two at a time through `next`."""
    rows, cursor = [], None
    while True:
        response = server.browse(token, view=fx.VIEW_ID, layer=fx.TEAMS, limit=2, cursor=cursor,
                                 **form)
        assert response.status_code == 200, response.text
        page = response.json()
        rows += page["artifacts"]
        cursor = page.get("next")
        if cursor is None:
            return rows


def browsed(rows) -> dict[str, int]:
    out = {row["key"]: row["masked_count"] for row in rows}
    assert len(out) == len(rows), "no row is served twice across pages"
    return out


def check(server) -> None:
    widest, ids = served(server, WIDEST)
    assert widest == fx.served(WIDEST)
    never = drill(server, server.authorise(WIDEST)["token"], NEVER_ISSUED)
    assert never[0] == 404
    for terms in fx.PRINCIPALS:
        expected = fx.served(terms)
        got, _ = served(server, terms)
        assert got == expected, terms
        token = server.authorise(terms)["token"]
        never = drill(server, token, NEVER_ISSUED)
        for key, tessera_id in ids.items():
            answer = drill(server, token, tessera_id)
            if key in expected:
                assert answer[0] == 200, (terms, key)
            else:
                assert answer == never, (terms, key)
            if key[0] == fx.NAMES:
                continue
            response = server.viewport_request(
                token, fx.VIEW_ID, 0, WHOLE, k=0,
                filters={"member_of": {"layer": key[0], "artifact": str(tessera_id)}},
            )
            if key[0] == fx.GATED and fx.GATE not in terms:
                # A layer this principal does not reach is refused by name, as one never declared.
                assert response.status_code == 422, (terms, key)
                continue
            assert response.status_code == 200, response.text
            tiles = decode_frames(response.content)[0]
            matched = sum(tile[2] for tile in tiles)
            assert matched == (expected[key][0] if key in expected else 0), (terms, key)
        teams = {key: value for (layer, key), value in expected.items() if layer == fx.TEAMS}
        assert browsed(browse_all(server, token)) == {
            key: count for key, (count, parent, _) in teams.items() if parent is None
        }, terms
        never_children = browse_all(server, token, parent=NEVER_ISSUED)
        for key, tessera_id in ids.items():
            if key[0] != fx.TEAMS:
                continue
            children = browse_all(server, token, parent=str(tessera_id))
            if key[1] in teams:
                assert browsed(children) == {
                    child: count for child, (count, parent, _) in teams.items() if parent == key[1]
                }, (terms, key)
            else:
                assert children == never_children, (terms, key)
            assert set(browsed(browse_all(server, token, q=key[1]))) <= set(teams), (terms, key)
            assert (key[1] in browsed(browse_all(server, token, q=key[1]))) == (key[1] in teams)


def test_the_fixture_separates_two_principals_by_one_label():
    """The oracle's own claim, before anything rests on it. Principals `1` and `1, 2` are served
    different artifacts because of labels. Every artifact but the unlabelled `t-open`, which holds
    points every principal sees, is served to some principal and withheld from another. `t-empty`
    is admitted by its label to `1` and withheld from them by its membership requirement."""
    narrow, wide = fx.served(["3", "1"]), fx.served(["3", "1", "2"])
    assert ("teams", "t-two") in wide and ("teams", "t-two") not in narrow
    assert ("names", "n-two") in wide and ("names", "n-two") not in narrow
    assert ("gated", "g-one") in wide and ("gated", "g-one") not in fx.served(["3", "2"])
    every = set(fx.served(WIDEST))
    for key in every:
        if key not in {("teams", "t-open"), ("names", "n-open")}:
            assert any(key not in fx.served(p) for p in fx.PRINCIPALS), key
    assert ("teams", "t-empty") not in narrow and ("teams", "t-empty") in wide


@pytest.fixture(scope="module")
def built(tmp_path_factory):
    bundle = fx.build_bundle(tmp_path_factory.mktemp("labels-built"), with_layers=True)
    state = tmp_path_factory.mktemp("labels-built-serve")
    yield bundle, state


@pytest.fixture(scope="module")
def published(tmp_path_factory):
    bundle = fx.build_bundle(tmp_path_factory.mktemp("labels-published"), with_layers=False)
    state = tmp_path_factory.mktemp("labels-published-serve")
    yield bundle, state


def live_restart_fold_restart(bundle, state, prepare=None) -> None:
    """Check one deployment live, after a restart, after a fold, and after a restart following
    it. `prepare` writes into the first server before its live check, so that check reads the
    state the writes left in memory."""
    started = []
    try:
        for stage in ("live", "restart", "fold", "fold-restart"):
            if stage in ("live", "restart", "fold-restart"):
                if started:
                    stop_server(started[-1])
                server, proc = spawn_server(bundle, state)
                started.append(proc)
                if stage == "live" and prepare is not None:
                    prepare(server)
            if stage == "fold":
                server.compact()
            check(server)
    finally:
        for proc in started:
            if proc.poll() is None:
                stop_server(proc)


def test_a_built_bundle_serves_each_principal_what_the_oracle_says(built):
    live_restart_fold_restart(*built)


def test_a_published_service_serves_each_principal_what_the_oracle_says(published):
    live_restart_fold_restart(*published, prepare=fx.publish)
