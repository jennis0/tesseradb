"""**An artifact's own access label**, black-box, against `oracle.artifact_label_fixture`.

The oracle computes from the planting rules which artifacts each principal is served, with what
masked count and which served parent. Every assertion here compares the running binary with that
answer, over a bundle built from an artifact source and over a service the same artifacts were
published into, and again after a restart and a fold.

The routes compared: the viewport's artifacts frame (keys, masked counts, parent links), the
identifier route, browse's roots, and a `member_of` filter. For a principal lacking an artifact's
label each answers exactly as though the artifact had never been published.
"""

from __future__ import annotations

import pytest

from oracle import artifact_label_fixture as fx
from oracle.harness import spawn_server, stop_server
from oracle.wire import decode_frames, decode_viewport_artifacts

WHOLE = [0.0, 0.0, fx.EXTENT_MAX, fx.EXTENT_MAX]
WIDEST = ["3", "1", "2", fx.UNCARRIED]


def frame(server, terms):
    token = server.authorise(terms)["token"]
    body = server.viewport(token, fx.VIEW_ID, 0, WHOLE, k=1000, artifact_budget=1000)
    return token, decode_viewport_artifacts(body)


def served(server, terms) -> dict[tuple[str, str], tuple[int, str | None]]:
    """The engine's answer in the oracle's shape."""
    _, rows = frame(server, terms)
    key_of = {row.tessera_id: row.key for row in rows}
    out = {}
    for row in rows:
        parents = [key_of[p] for p in row.parent_ids]
        assert len(parents) <= 1, row
        out[(row.layer, row.key)] = (row.masked_count, parents[0] if parents else None)
    return out


def identifiers(server) -> dict[tuple[str, str], int]:
    """Every artifact's `tessera_id`, read as the principal who is served all of them."""
    _, rows = frame(server, WIDEST)
    return {(row.layer, row.key): row.tessera_id for row in rows}


@pytest.fixture(scope="module")
def built(tmp_path_factory):
    bundle = fx.build_bundle(tmp_path_factory.mktemp("labels-built"), with_layers=True)
    server, proc = spawn_server(bundle, tmp_path_factory.mktemp("labels-built-serve"))
    yield server
    stop_server(proc)


@pytest.fixture(scope="module")
def published(tmp_path_factory):
    work = tmp_path_factory.mktemp("labels-published")
    bundle = fx.build_bundle(work, with_layers=False)
    state = tmp_path_factory.mktemp("labels-published-serve")
    server, proc = spawn_server(bundle, state)
    fx.publish(server)
    yield server, bundle, state, proc


def test_the_fixture_separates_two_principals_by_one_label():
    """The oracle's own claim, before anything rests on it. Principals `1` and `1, 2` are served
    different artifacts because of labels. Every artifact but the unlabelled `t-open`, which holds
    public points, is served to some principal and withheld from another. `t-empty` is admitted by
    its label to `1` and withheld from them by its membership requirement, which is what a label
    test alone would not do."""
    narrow, wide = fx.served(["3", "1"]), fx.served(["3", "1", "2"])
    assert ("teams", "t-two") in wide and ("teams", "t-two") not in narrow
    every = {(fx.TEAMS, r[0]) for r in fx.TEAM_ROWS} | {(fx.SEALED, r[0]) for r in fx.SEALED_ROWS}
    for key in every:
        assert any(key in fx.served(p) for p in fx.PRINCIPALS), key
        if key != ("teams", "t-open"):
            assert any(key not in fx.served(p) for p in fx.PRINCIPALS), key
    assert ("teams", "t-empty") not in narrow and ("teams", "t-empty") in wide


def check(server) -> None:
    ids = identifiers(server)
    assert set(ids) == set(fx.served(WIDEST)), "the widest principal is served what the oracle says"
    for terms in fx.PRINCIPALS:
        expected = fx.served(terms)
        assert served(server, terms) == expected, terms
        token = server.authorise(terms)["token"]
        for key, tessera_id in ids.items():
            layer = key[0]
            # The identifier route: a withheld artifact is the 404 a never-issued identifier is.
            response = _drill(server, token, tessera_id)
            assert (response.status_code == 200) == (key in expected), (terms, key)
            # `member_of`: the matched count is the masked count where served, and nothing where
            # withheld.
            body = server.viewport(
                token,
                fx.VIEW_ID,
                0,
                WHOLE,
                k=0,
                filters={"member_of": {"layer": layer, "artifact": str(tessera_id)}},
            )
            tiles = decode_frames(body)[0]
            matched = sum(tile[2] for tile in tiles)
            assert matched == (expected[key][0] if key in expected else 0), (terms, key)
        # Browse's roots: served artifacts whose parent is not served.
        page = server.browse(token, view=fx.VIEW_ID, layer=fx.TEAMS, limit=100)
        assert page.status_code == 200, page.text
        roots = {row["key"] for row in page.json()["artifacts"]}
        assert roots == {
            key for (layer, key), (_, parent) in expected.items() if layer == fx.TEAMS and not parent
        }, terms


def _drill(server, token, tessera_id):
    import requests

    return requests.post(
        f"{server.viewer_base}/v1/artifacts/{tessera_id}",
        headers={"Authorization": f"Bearer {token}"},
        json={"view": fx.VIEW_ID},
        timeout=30,
    )


def test_a_built_bundle_serves_each_principal_what_the_oracle_says(built):
    check(built)


def test_a_published_service_serves_each_principal_what_the_oracle_says_across_a_restart_and_a_fold(
    published,
):
    server, bundle, state, proc = published
    check(server)
    stop_server(proc)
    server, proc = spawn_server(bundle, state)
    try:
        check(server)
        server.compact()
        check(server)
    finally:
        stop_server(proc)
