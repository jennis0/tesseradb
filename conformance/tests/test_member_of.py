"""**The `member_of` leaf: one artifact of one layer, resolved to `membership ∩ M_auth`, as a
filter clause** (`highlight-and-hierarchy.md` §3), black-box, over the real binary, against
`oracle.filters`' own definition.

The fixture is `test_shape_membership.py`'s — a lattice of points on tile lines beside a scatter,
two spatial layers whose polygons sit on tile corners, three principals — with
`test_region_leaf.py`'s gated strip beside them so a withheld artifact exists to be asked about.

What is asserted, and by which second reader:

- **A leaf's count is the artifact's masked count and the oracle's**, for every served artifact of
  both layers under three principals. The oracle is `MemberOfColumn` over the fixture's own
  geometry — the members each shape holds, intersected with the principal's grant — which shares
  no code with the engine's membership form.
- **It composes**: `all_of` with a numeric leaf, and `none_of` as the complement within the
  visible set, both against the oracle's own tree.
- **An unknown id, an artifact of another layer, a suppressed one and one withheld by the
  principal's criterion are one response, byte for byte** — the empty operand of §3, and never a
  refusal, which is what keeps the leaf from being an existence oracle over what the criterion
  withholds.
- **An unknown layer is `422`** — deployment schema, the other side of contracts §3.2's
  unknown-column/unknown-value split — and a malformed body is too.
- **`member_of` is a reserved column name** — a declaration naming it is refused at the build.
"""

from __future__ import annotations

import subprocess

import pytest

from oracle.filters import MemberOfColumn, NumericColumn, evaluate
from oracle.harness import CLI_BIN, REPO_ROOT, build_env, spawn_server, stop_server, write_deployment
from oracle.wire import FRAME_TRAILER, decode_viewport, decode_viewport_artifacts, split_frames

from test_region_leaf import GATED, gated_layer_toml
from test_shape_membership import (
    BOXES,
    ID_KEY_HEX,
    PRINCIPALS,
    SHAPES,
    VIEW_ID,
    WHOLE_MAP,
    build_bundle,
    config_toml,
    containing,
    fixture_points,
    visible,
)


def matched_sum(server, token: str, filters: dict) -> int:
    resp = server.viewport_response(token, VIEW_ID, 0, WHOLE_MAP, k=0, filters=filters)
    assert resp.status_code == 200, resp.text
    tiles, _points = decode_viewport(resp.content)
    return sum(t[2] for t in tiles)


def leaf(layer: str, tessera_id: int) -> dict:
    """The wire form: the identifier as a decimal string, which is how a client that cannot carry
    a `u64` intact sends it and what the server accepts beside a JSON number."""
    return {"member_of": {"layer": layer, "artifact": str(tessera_id)}}


def member_column(points, served, terms: list[str]) -> MemberOfColumn:
    """The oracle's column: each served artifact's members as the fixture's own geometry defines
    them, intersected with this principal's grant. An artifact absent from `served` — withheld,
    suppressed, of another layer — is absent here too, which is the empty operand."""
    seen = visible(points, terms)
    members = {
        (a.layer, str(a.tessera_id)): {p[0] for p in seen if a.key in containing(p[1], p[2]).get(a.layer, set())}
        for a in served
    }
    return MemberOfColumn(members=members, rowed={p[0] for p in points})


@pytest.fixture(scope="module")
def member_server(tmp_path_factory):
    points = fixture_points()
    work = tmp_path_factory.mktemp("member-of-fixture")
    build_bundle(work, points)
    (work / "gated.toml").write_text(config_toml() + gated_layer_toml())
    bundle = work / "bundle-gated"
    deployment = write_deployment(work / "tessera-gated.toml", bundle=bundle, schema=work / "gated.toml")
    subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(bundle), "--mint-external-ids"],
        cwd=REPO_ROOT,
        env=build_env(ID_KEY_HEX),
        check=True,
    )
    server, proc = spawn_server(
        bundle,
        tmp_path_factory.mktemp("member-of-server"),
        max_k=100_000,
        k_max_marks=100_000,
        theta_target_marks=10**12,
    )
    yield server, points
    stop_server(proc)


def test_the_leaf_is_the_artifacts_masked_count_and_the_oracles(member_server):
    server, points = member_server
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        served = [a for a in decode_viewport_artifacts(server.viewport(token, VIEW_ID, 0, WHOLE_MAP, k=100_000)) if a.layer in (SHAPES, BOXES)]
        assert served, f"principal {terms}: the shape layers serve artifacts"
        column = member_column(points, served, terms)
        candidate = {p[0] for p in visible(points, terms)}
        for artifact in served:
            clause = leaf(artifact.layer, artifact.tessera_id)
            got = matched_sum(server, token, clause)
            assert got == artifact.masked_count, f"{artifact.layer}/{artifact.key}: {got} != {artifact.masked_count}"
            expected = len(evaluate(clause, {"member_of": column}, candidate))
            assert got == expected, f"{artifact.layer}/{artifact.key}: served {got}, oracle {expected}"


def test_it_composes_with_a_numeric_leaf_and_negates_within_the_visible_set(member_server):
    server, points = member_server
    numeric = NumericColumn({p[0]: p[0] for p in points})
    for terms in PRINCIPALS:
        token = server.authorise(terms)["token"]
        served = [a for a in decode_viewport_artifacts(server.viewport(token, VIEW_ID, 0, WHOLE_MAP, k=100_000)) if a.layer in (SHAPES, BOXES)]
        column = member_column(points, served, terms)
        candidate = {p[0] for p in visible(points, terms)}
        artifact = max(served, key=lambda a: a.masked_count)
        clause = leaf(artifact.layer, artifact.tessera_id)
        columns = {"member_of": column, "fx_key": numeric}

        composed = {"all_of": [clause, {"fx_key": {"range": {"lt": 1500}}}]}
        got = matched_sum(server, token, composed)
        assert got == len(evaluate(composed, columns, candidate)), f"principal {terms}: {composed}"

        inside = matched_sum(server, token, clause)
        outside = matched_sum(server, token, {"none_of": [clause]})
        assert inside + outside == len(candidate), f"principal {terms}: {inside} + {outside} != {len(candidate)}"
        assert outside == len(evaluate({"none_of": [clause]}, columns, candidate))


def test_an_unknown_a_foreign_a_suppressed_and_a_withheld_artifact_are_one_response(member_server):
    server, points = member_server
    broad = server.authorise(["1", "2"])["token"]
    narrow = server.authorise(["1"])["token"]
    served = [a for a in decode_viewport_artifacts(server.viewport(broad, VIEW_ID, 0, WHOLE_MAP, k=100_000)) if a.layer == GATED]
    assert len(served) == 1 and served[0].masked_count > 0, served
    strip = served[0]
    assert not [a for a in decode_viewport_artifacts(server.viewport(narrow, VIEW_ID, 0, WHOLE_MAP, k=100_000)) if a.layer == GATED]

    def bytes_of(token: str, clause: dict) -> tuple[list, dict]:
        """The body's frames but the trailer, whose `stream_us` is a clock; and every header but
        the two timing ones (C4's channel, quantified elsewhere and deliberately not asserted)."""
        resp = server.viewport_response(token, VIEW_ID, 0, WHOLE_MAP, k=100_000, filters=clause)
        assert resp.status_code == 200, resp.text
        headers = {k: v for k, v in resp.headers.items() if k.startswith("x-tessera-") and k not in ("x-tessera-server-us", "x-tessera-admission-us")}
        frames = [(kind, payload) for kind, payload in split_frames(resp.content) if kind != FRAME_TRAILER]
        return frames, headers

    unknown = leaf(GATED, 8589934593)
    withheld = leaf(GATED, strip.tessera_id)
    assert bytes_of(narrow, withheld) == bytes_of(narrow, unknown)
    assert matched_sum(server, narrow, unknown) == 0

    # The **right identifier under the wrong layer**: a value that does not resolve within the
    # layer named, answered exactly as one that resolves to nothing at all.
    foreign = leaf(SHAPES, strip.tessera_id)
    assert bytes_of(broad, foreign) == bytes_of(broad, leaf(SHAPES, 8589934593))
    assert matched_sum(server, broad, foreign) == 0

    # Suppressed: served to the broad principal a moment ago, and an empty operand from the ack.
    before = matched_sum(server, broad, withheld)
    assert before == strip.masked_count
    idset = server.meta(broad)["idset"]
    resp = server.changes([{"tessera_id": str(strip.tessera_id), "idset": idset, "op": "suppress"}])
    assert resp.status_code in (200, 202), resp.text
    assert bytes_of(broad, withheld) == bytes_of(broad, unknown)
    resp = server.changes([{"tessera_id": str(strip.tessera_id), "idset": idset, "op": "unsuppress"}])
    assert resp.status_code in (200, 202), resp.text
    assert matched_sum(server, broad, withheld) == before


def test_an_unknown_layer_is_refused_where_an_unknown_artifact_is_not(member_server):
    server, _ = member_server
    token = server.authorise(["1", "2"])["token"]
    refused = server.viewport_request(token, VIEW_ID, 0, WHOLE_MAP, k=0, filters=leaf("regions/nowhere", 8589934593))
    assert refused.status_code == 422, refused.text
    assert "regions/nowhere" in refused.text
    # The same identifier under a layer that exists: answered, and empty.
    assert matched_sum(server, token, leaf(SHAPES, 8589934593)) == 0
    # Shape refusals: a missing half, an unknown key, a non-string layer.
    for body in ({"layer": SHAPES}, {"artifact": "1"}, {"layer": SHAPES, "artifact": "1", "level": 0}, {"layer": 3, "artifact": "1"}):
        resp = server.viewport_request(token, VIEW_ID, 0, WHOLE_MAP, k=0, filters={"member_of": body})
        assert resp.status_code == 422, f"{body}: {resp.text}"


def test_member_of_is_a_reserved_column_name(tmp_path):
    points = fixture_points()
    work = tmp_path / "reserved"
    work.mkdir()
    build_bundle(work, points)
    config = work / "shapes.toml"
    config.write_text(config_toml().replace('name   = "fx_key"', 'name   = "member_of"'))
    deployment = write_deployment(work / "tessera-reserved.toml", bundle=work / "bundle-reserved", schema=config)
    result = subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(work / "bundle-reserved"), "--mint-external-ids"],
        cwd=REPO_ROOT,
        env=build_env(ID_KEY_HEX),
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0
    assert "member_of" in (result.stderr + result.stdout) and "reserved" in (result.stderr + result.stdout).lower()
