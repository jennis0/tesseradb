"""The own-label fixture: artifacts that carry access labels of their own, and the oracle's answer.

An artifact whose own label a viewer does not hold does not exist for them, whatever they can see
of its members. This module plants a small corpus and four layers, and computes from the planting
rules alone which artifacts each principal is served, with what masked count, which served parent
and which served target. It never reads a response to decide an answer.

**What the corpus separates.** Points 0..49 carry term `1`, points 50..99 term `2`, and points
100..199 term `3`, which every principal holds so that no artifact's absence is an empty map. The
principals `1` and `1, 2` differ by one term.

**What the layers exercise.**

* `teams` is a tree whose artifacts carry no label, one, two, or a label (`9`) that no point
  carries. A child's parent can be withheld while the child is served, and the reverse. `t-empty`
  is admitted by its label to principal `1` and withheld from them by its membership requirement.
  `t-late` is labelled `2`: from its source at a build, and by a fill after its publication at a
  running service.
* `sealed` names a default label, `9`, so its unlabelled artifact is served only to a holder of `9`.
* `names` depends on `teams`: each artifact is attached to a team, carries no members of its own,
  and is served exactly where its team is.
* `gated` is behind its own layer label, `2`, and its artifacts carry labels too: a principal needs
  both.

The same artifacts reach a deployment two ways, by a build reading artifact sources and by
publication at a running service, and both must answer as this module says.
"""

from __future__ import annotations

import base64
import json
import random
import subprocess
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from .harness import CLI_BIN, REPO_ROOT, build_env, ensure_cli_built, write_deployment

N_ITEMS = 200
VIEW_ID = "s0"
EXTENT_MAX = 65536.0
SEED = 20260923
ID_KEY_HEX = "000102030405060708090a0b0c0d0e0f"

TEAMS = "teams"
SEALED = "sealed"
NAMES = "names"
GATED = "gated"
#: The label no point carries, and `sealed`'s default.
UNCARRIED = "9"
#: `gated`'s own layer label.
GATE = "2"

#: `(key, members, labels, parent)` per artifact of `teams`, parents first.
TEAM_ROWS: list[tuple[str, list[int], list[str] | None, str | None]] = [
    ("t-open", list(range(0, 60)) + list(range(150, 160)), None, None),
    ("t-one", list(range(0, 20)), ["1"], "t-open"),
    ("t-two", list(range(50, 70)), ["2"], None),
    ("t-either", list(range(40, 60)), ["1", "2"], "t-two"),
    ("t-uncarried", list(range(100, 120)), [UNCARRIED], "t-open"),
    ("t-under-uncarried", list(range(0, 10)), None, "t-uncarried"),
    ("t-empty", list(range(50, 60)), ["1"], None),
    ("t-late", list(range(60, 80)) + list(range(120, 130)), ["2"], None),
]
#: The artifact a running service publishes unlabelled and then fills.
FILLED_LATER = "t-late"

#: `(key, members, labels)` per artifact of `sealed`.
SEALED_ROWS: list[tuple[str, list[int], list[str] | None]] = [
    ("s-default", list(range(100, 110)), None),
    ("s-one", list(range(0, 10)), ["1"]),
]

#: `(key, team, text)` per artifact of `names`.
NAME_ROWS: list[tuple[str, str, str]] = [
    ("n-open", "t-open", "Open"),
    ("n-two", "t-two", "Two"),
    ("n-uncarried", "t-uncarried", "Uncarried"),
]

#: `(key, members, labels)` per artifact of `gated`.
GATED_ROWS: list[tuple[str, list[int], list[str] | None]] = [
    ("g-one", list(range(0, 10)), ["1"]),
    ("g-bare", list(range(100, 110)), None),
]

#: The principals, by the terms they hold. `1` and `1, 2` are one term apart.
PRINCIPALS: list[list[str]] = [
    ["3", "1"], ["3", "2"], ["3", "1", "2"], ["3", UNCARRIED], ["3", "1", "2", UNCARRIED], ["3"],
]


def terms_of(source_id: int) -> list[int]:
    if source_id < 50:
        return [1]
    if source_id < 100:
        return [2]
    return [3]


def visible_to(terms: list[str]) -> set[int]:
    """The points a principal sees: those carrying a term they hold."""
    held = set(terms)
    return {e for e in range(N_ITEMS) if held & {str(t) for t in terms_of(e)}}


def _admits(labels: list[str] | None, default: str | None, terms: list[str]) -> bool:
    """Whether a label list admits a principal: any label they hold, `public` being held by all.
    An artifact with no label takes the layer's default, `None` being `inherited`."""
    held = set(terms) | {"public"}
    if not labels:
        return default is None or default in held
    return bool(held & set(labels))


Served = dict[tuple[str, str], tuple[int, str | None, str | None]]


def served(terms: list[str]) -> Served:
    """`(layer, key) -> (masked count, served parent key, served target key)` for one principal.

    Every layer requires one visible member (`count = 1`). A parent is named only where it is
    itself served. An attached artifact counts its target's members and is served only where its
    target is.
    """
    visible = visible_to(terms)
    passing: dict[tuple[str, str], tuple[int, str | None]] = {}
    rows = [(TEAMS, key, members, labels, parent, None) for key, members, labels, parent in TEAM_ROWS]
    rows += [(SEALED, key, members, labels, None, UNCARRIED) for key, members, labels in SEALED_ROWS]
    if GATE in terms:
        rows += [(GATED, key, members, labels, None, None) for key, members, labels in GATED_ROWS]
    for layer, key, members, labels, parent, default in rows:
        count = len(visible & set(members))
        if _admits(labels, default, terms) and count >= 1:
            passing[(layer, key)] = (count, parent)
    out: Served = {}
    for (layer, key), (count, parent) in passing.items():
        named = parent if parent is not None and (layer, parent) in passing else None
        out[(layer, key)] = (count, named, None)
    for key, team, _ in NAME_ROWS:
        if (TEAMS, team) in passing:
            out[(NAMES, key)] = (passing[(TEAMS, team)][0], None, team)
    return out


def external_id(source_id: int) -> str:
    """A point's external id as the publication route takes it: the build's own convention, the
    source id as eight bytes little-endian, base64."""
    return base64.b64encode(source_id.to_bytes(8, "little")).decode()


def _write_points(path: Path) -> None:
    rng = random.Random(SEED)
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N_ITEMS), type=pa.uint64()),
                "x": pa.array([rng.uniform(0.0, EXTENT_MAX) for _ in range(N_ITEMS)], pa.float32()),
                "y": pa.array([rng.uniform(0.0, EXTENT_MAX) for _ in range(N_ITEMS)], pa.float32()),
            }
        ),
        path,
    )


def _write_pairs(path: Path) -> None:
    rows = [(e, t) for e in range(N_ITEMS) for t in terms_of(e)]
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array([r[0] for r in rows], type=pa.uint64()),
                "term_id": pa.array([r[1] for r in rows], type=pa.uint32()),
            }
        ),
        path,
    )


def _write_teams(path: Path) -> None:
    pq.write_table(
        pa.table(
            {
                "key": pa.array([r[0] for r in TEAM_ROWS], pa.string()),
                "members": pa.array([r[1] for r in TEAM_ROWS], pa.list_(pa.uint64())),
                "team": pa.array([r[2] for r in TEAM_ROWS], pa.list_(pa.string())),
                "parent": pa.array([r[3] for r in TEAM_ROWS], pa.string()),
            }
        ),
        path,
    )


def _toml(value) -> str:
    return json.dumps(value)


def _inline(rows) -> str:
    lines = []
    for row in rows:
        lines.append("  { " + ", ".join(f"{k} = {_toml(v)}" for k, v in row.items()) + " },")
    return "artifacts = [\n" + "\n".join(lines) + "\n]\n"


def _layer_toml(name: str, *, visibility: str, field: str | None, default: str, kind: str,
                extra: str = "") -> str:
    visibility_line = f"visibility = {_toml(visibility)}\n"
    artifact = f'field = "{field}", ' if field else ""
    return (
        f"\n[[layer]]\nname = {_toml(name)}\ntitle = {_toml(name)}\nviews = [{_toml(VIEW_ID)}]\n"
        f'membership = "enumerated"\n{visibility_line}'
        f"artifact_visibility = {{ {artifact}default = {_toml(default)} }}\n"
        "require_member_visibility = { count = 1 }\n"
        f'hierarchy = {{ kind = "{kind}", prune_children = false }}\n{extra}'
    )


def _config(with_layers: bool) -> str:
    text = '[sources]\npoints = "points.parquet"\npairs  = "pairs.parquet"\n'
    if with_layers:
        text += 'teams  = "teams.parquet"\n'
    text += (
        f"\n[[view]]\nname = {_toml(VIEW_ID)}\n"
        f"extent = {{ x = [0.0, {EXTENT_MAX}], y = [0.0, {EXTENT_MAX}] }}\n"
        'source = "points"\npoint_visibility = { source = "pairs", default = "public" }\n'
    )
    if not with_layers:
        return text
    text += _layer_toml(TEAMS, visibility="public", field="team", default="inherited",
                        kind="nested", extra='source = "teams"\n')
    text += _layer_toml(
        SEALED, visibility="public", field="team", default=UNCARRIED, kind="flat",
        extra=_inline(
            {"key": key, "members": members, **({"access": labels} if labels else {})}
            for key, members, labels in SEALED_ROWS
        ),
    )
    text += _layer_toml(
        GATED, visibility=GATE, field="team", default="inherited", kind="flat",
        extra=_inline(
            {"key": key, "members": members, **({"access": labels} if labels else {})}
            for key, members, labels in GATED_ROWS
        ),
    )
    text += _layer_toml(
        NAMES, visibility="public", field=None, default="inherited", kind="flat",
        extra=f'depends_on = ["{TEAMS}"]\n' + _inline(
            {"key": key, "attached_layer": TEAMS, "attached_key": team, "contents": [[text]]}
            for key, team, text in NAME_ROWS
        ) + '\n  [[layer.content.supplied]]\n  name = "name"\n  type = "text"\n'
        '  require_member_visibility = "inherited"\n',
    )
    return text


def build_bundle(work_dir: Path, *, with_layers: bool) -> Path:
    """Write the corpus and build it, with the layers declared and read from their sources, or
    with none, for a service the same artifacts are published into."""
    ensure_cli_built()
    work_dir.mkdir(parents=True, exist_ok=True)
    _write_points(work_dir / "points.parquet")
    _write_pairs(work_dir / "pairs.parquet")
    if with_layers:
        _write_teams(work_dir / "teams.parquet")
    config = work_dir / "labels.toml"
    config.write_text(_config(with_layers))
    bundle = work_dir / "bundle"
    deployment = write_deployment(work_dir / "tessera.toml", bundle=bundle, schema=config)
    subprocess.run(
        [str(CLI_BIN), "build", "--deployment", str(deployment), "--out", str(bundle),
         "--mint-external-ids"],
        cwd=REPO_ROOT,
        env=build_env(ID_KEY_HEX),
        check=True,
    )
    return bundle


def _declaration(name: str, kind: str, default, *, field: str | None = "team",
                 visibility: str | None = None, depends_on: list[str] | None = None,
                 supplied: list[dict] | None = None) -> dict:
    return {
        "name": name,
        "title": name,
        "views": [VIEW_ID],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": visibility,
        "artifact_visibility": {"field": field, "default": default},
        "require_member_visibility": {"count": 1},
        "hierarchy": {"kind": kind, "prune_children": False},
        "content": {"computed": [], "supplied": supplied or []},
        "depends_on": depends_on or [],
        "levels": [],
    }


def publish(server) -> None:
    """Declare the layers at a running service and publish the same artifacts into them, with
    `t-late` published unlabelled and given its label by a fill afterwards."""
    for declaration in (
        _declaration(TEAMS, "nested", "inherited"),
        _declaration(SEALED, "flat", {"label": UNCARRIED}),
        _declaration(GATED, "flat", "inherited", visibility=GATE),
        _declaration(
            NAMES, "flat", "inherited", field=None, depends_on=[TEAMS],
            supplied=[{"name": "name", "type": "text", "require_member_visibility": "inherited"}],
        ),
    ):
        response = server.register_layer(declaration)
        assert response.status_code == 201, response.text

    def record(key, members, labels, parent=None):
        out = {"key": key, "members": [external_id(m) for m in members]}
        if labels and key != FILLED_LATER:
            out["access"] = labels
        if parent:
            out["parent"] = [parent]
        return out

    for layer, rows in ((TEAMS, TEAM_ROWS), (SEALED, SEALED_ROWS), (GATED, GATED_ROWS)):
        response = server.publish_artifacts(
            layer, addressing="external", artifacts=[record(*row) for row in rows]
        )
        assert response.status_code == 201, response.text
    response = server.publish_artifacts(
        NAMES,
        addressing="external",
        artifacts=[
            {"key": key, "attached_to": {"layer": TEAMS, "key": team}, "content": [{"values": [text]}]}
            for key, team, text in NAME_ROWS
        ],
    )
    assert response.status_code == 201, response.text
    labels = next(row[2] for row in TEAM_ROWS if row[0] == FILLED_LATER)
    fill(server, TEAMS, FILLED_LATER, labels)
    published(server)


def fill(server, layer: str, key: str, labels: list[str]):
    """`PATCH` one artifact's label, as a fill."""
    import requests

    response = requests.patch(
        f"{server.control_base}/control/layers/{layer}/artifacts",
        headers={"Authorization": f"Bearer {server.operator_credential}"},
        json={"addressing": "external", "artifacts": [{"key": key, "access": labels}]},
        timeout=60,
    )
    assert response.status_code == 200, response.text
    return response


def published(server) -> None:
    """Wait for the publication that serves what was written: the flush route's own wait."""
    import requests

    response = requests.post(
        f"{server.control_base}/control/flush?wait=visible",
        headers={"Authorization": f"Bearer {server.operator_credential}"},
        timeout=120,
    )
    assert response.status_code == 202 and response.json().get("visible") is True, response.text
