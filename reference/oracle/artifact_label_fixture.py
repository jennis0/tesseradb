"""The own-label fixture: artifacts that carry access labels of their own, and the oracle's answer.

An artifact whose own label a viewer does not hold does not exist for them, whatever they can see
of its members. This module plants a small corpus and two layers, and computes from the planting
rules alone which artifacts each principal is served, with what masked count and which served
parent. It never reads a response to decide an answer.

**What the corpus separates.** Points 0..49 carry term `1`, points 50..99 term `2`, and points
100..199 term `3`, which every principal holds so that no artifact's absence is an empty map. The
principals `1` and `1, 2` differ by one term.

**What the layers exercise.** `teams` is a tree whose artifacts carry labels as a list: none, one,
two, and a label (`9`) that no point carries. A child's parent can be withheld while the child is
served, and the reverse. `t-empty` is admitted by its label for principal `1` and holds no member
they see, so it is withheld by its membership requirement rather than its label: a control that the
label is not the only thing the service tests. `sealed` names a default label, `9`, so its
unlabelled artifact is served only to a principal holding `9`.

The same artifacts reach a deployment two ways, by a build reading an artifact source file and by
publication at a running service, and both must answer as this module says.
"""

from __future__ import annotations

import base64
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
#: The label no point carries, and `sealed`'s default.
UNCARRIED = "9"

#: `(key, members, labels, parent)` per artifact of `teams`, parents first.
TEAM_ROWS: list[tuple[str, list[int], list[str] | None, str | None]] = [
    ("t-open", list(range(0, 60)) + list(range(150, 160)), None, None),
    ("t-one", list(range(0, 20)), ["1"], "t-open"),
    ("t-two", list(range(50, 70)), ["2"], None),
    ("t-either", list(range(40, 60)), ["1", "2"], "t-two"),
    ("t-uncarried", list(range(100, 120)), [UNCARRIED], "t-open"),
    ("t-under-uncarried", list(range(0, 10)), None, "t-uncarried"),
    ("t-empty", list(range(50, 60)), ["1"], None),
]

#: `(key, members, labels)` per artifact of `sealed`.
SEALED_ROWS: list[tuple[str, list[int], list[str] | None]] = [
    ("s-default", list(range(100, 110)), None),
    ("s-one", list(range(0, 10)), ["1"]),
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


def served(terms: list[str]) -> dict[tuple[str, str], tuple[int, str | None]]:
    """`(layer, key) -> (masked count, served parent key)` for one principal.

    Both layers require one visible member (`count = 1`). A parent is named only where it is itself
    served, which is the rule a withheld artifact is treated by: as though it never existed.
    """
    visible = visible_to(terms)
    out: dict[tuple[str, str], tuple[int, str | None]] = {}
    rows = [(TEAMS, key, members, labels, parent, None) for key, members, labels, parent in TEAM_ROWS]
    rows += [(SEALED, key, members, labels, None, UNCARRIED) for key, members, labels in SEALED_ROWS]
    passing = {}
    for layer, key, members, labels, parent, default in rows:
        count = len(visible & set(members))
        if _admits(labels, default, terms) and count >= 1:
            passing[(layer, key)] = (count, parent)
    for (layer, key), (count, parent) in passing.items():
        named = parent if parent is not None and (layer, parent) in passing else None
        out[(layer, key)] = (count, named)
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


VIEW_TOML = f"""
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"
{{teams_source}}
[[view]]
name             = "{VIEW_ID}"
extent           = {{{{ x = [0.0, {EXTENT_MAX}], y = [0.0, {EXTENT_MAX}] }}}}
source           = "points"
point_visibility = {{{{ source = "pairs", default = "public" }}}}
"""

LAYERS_TOML = f"""
[[layer]]
name                      = "{TEAMS}"
title                     = "teams"
views                     = ["{VIEW_ID}"]
source                    = "teams"
membership                = "enumerated"
visibility                = "public"
artifact_visibility       = {{ field = "team", default = "inherited" }}
require_member_visibility = {{ count = 1 }}
hierarchy                 = {{ kind = "nested", prune_children = false }}

[[layer]]
name                      = "{SEALED}"
title                     = "sealed"
views                     = ["{VIEW_ID}"]
membership                = "enumerated"
visibility                = "public"
artifact_visibility       = {{ field = "team", default = "{UNCARRIED}" }}
require_member_visibility = {{ count = 1 }}
hierarchy                 = {{ kind = "flat", prune_children = false }}
artifacts = [
{chr(10).join(
    '  { key = "' + key + '", members = [' + ', '.join(str(m) for m in members) + ']'
    + ('' if not labels else ', access = [' + ', '.join('"' + l + '"' for l in labels) + ']')
    + ' },'
    for key, members, labels in SEALED_ROWS
)}
]
"""


def build_bundle(work_dir: Path, *, with_layers: bool) -> Path:
    """Write the corpus and build it, with the two layers declared and read from their sources, or
    with none, for a service the same artifacts are published into."""
    ensure_cli_built()
    work_dir.mkdir(parents=True, exist_ok=True)
    _write_points(work_dir / "points.parquet")
    _write_pairs(work_dir / "pairs.parquet")
    text = VIEW_TOML.replace(
        "{teams_source}", 'teams  = "teams.parquet"\n' if with_layers else ""
    ).replace("{{", "{").replace("}}", "}")
    if with_layers:
        _write_teams(work_dir / "teams.parquet")
        text += LAYERS_TOML
    config = work_dir / "labels.toml"
    config.write_text(text)
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


def _declaration(name: str, kind: str, default) -> dict:
    return {
        "name": name,
        "title": name,
        "views": [VIEW_ID],
        "membership": "enumerated",
        "value_set": "closed",
        "visibility": None,
        "artifact_visibility": {"field": "team", "default": default},
        "require_member_visibility": {"count": 1},
        "hierarchy": {"kind": kind, "prune_children": False},
        "content": {"computed": [], "supplied": []},
        "depends_on": [],
        "levels": [],
    }


def publish(server) -> None:
    """Declare the two layers at a running service and publish the same artifacts into them."""
    for declaration in (
        _declaration(TEAMS, "nested", "inherited"),
        _declaration(SEALED, "flat", {"label": UNCARRIED}),
    ):
        response = server.register_layer(declaration)
        assert response.status_code == 201, response.text

    def record(key, members, labels, parent=None):
        out = {"key": key, "members": [external_id(m) for m in members]}
        if labels:
            out["access"] = labels
        if parent:
            out["parent"] = [parent]
        return out

    response = server.publish_artifacts(
        TEAMS, addressing="external", artifacts=[record(*row) for row in TEAM_ROWS]
    )
    assert response.status_code == 201, response.text
    response = server.publish_artifacts(
        SEALED, addressing="external", artifacts=[record(*row) for row in SEALED_ROWS]
    )
    assert response.status_code == 201, response.text
    published(server)


def published(server) -> None:
    """Wait for the publication that serves what was written: the flush route's own wait."""
    import requests

    response = requests.post(
        f"{server.control_base}/control/flush?wait=visible",
        headers={"Authorization": f"Bearer {server.operator_credential}"},
        timeout=120,
    )
    assert response.status_code == 202 and response.json().get("visible") is True, response.text
