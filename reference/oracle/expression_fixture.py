"""A corpus whose items carry access expressions: conjunctions, several labels on one item, a
quoted term, disjunctions with conjunctions among their operands, a conjunction that another of the
item's labels absorbs, and items with no label of their own.

The oracle's answers here come from the labels each item was given, read by `oracle.access`'s own
parser and evaluated by direct recursion. Nothing is read back from the bundle, so a build that
indexed a label under the wrong keys disagrees with the oracle rather than with itself.

Each item's labels are chosen by its source id modulo the length of `LABELS`, so every shape occurs
many times and in every part of the map. `PRINCIPALS` holds each shape's edge: half of a
conjunction, the whole of it, one operand of a disjunction inside it, a term only a second label
names, the quoted term with and without its partner, and a held term that appears only inside a
conjunction the principal does not satisfy, on an item it sees through another label.
"""

from __future__ import annotations

import random
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from . import access
from .harness import (
    JOIN_COLUMN,
    cli_build,
    ensure_cli_built,
    join_attribute_toml,
    write_deployment,
)

N_ITEMS = 240
VIEW_ID = "s0"
EXTENT_MAX = 65536.0
SEED = 20261002

#: The labels an item carries, by source id modulo the length of this list. `[]` takes the view's
#: default, `public`.
LABELS: list[list[str]] = [
    ["eu&(ir:legal|ir:new)"],
    ["eu&(ir:legal|ir:new)", "ir:secret"],
    ["ir:new|ir:other"],
    ["x&y", '"team b"&eu'],
    [],
    ["(a&b&c)|(d&e)", "ir:new"],
    ["(t&c)|(s&(b|a))"],
    ["ir:other|(x&y)", "ir:other&eu"],
]

PRINCIPALS: list[list[str]] = [
    [],
    ["eu"],
    ["eu", "ir:new"],
    ["eu", "ir:legal", "ir:new", "ir:secret"],
    ["ir:secret"],
    ["x"],
    ["x", "y"],
    ["team b", "eu"],
    ["team b"],
    ["a", "b", "c", "d", "e"],
    ["d", "e"],
    ["s", "a", "b", "t", "c"],
    ["s"],
    ["ir:secret", "eu"],
    ["ir:new", "d"],
]


def labels_of(source_id: int) -> list[str]:
    """The labels the item with this source id was given."""
    return LABELS[source_id % len(LABELS)]


def visible_to(terms: list[str]) -> set[int]:
    """The source ids a principal presenting `terms` sees."""
    held = _held(terms)
    return {
        source_id
        for source_id in range(N_ITEMS)
        if access.admits(labels_of(source_id) or [access.PUBLIC], held)
    }


def card_of(source_id: int, terms: list[str]) -> list[str]:
    """What an item card serves for this item to a principal presenting `terms`."""
    return access.card_labels(labels_of(source_id) or [access.PUBLIC], _held(terms))


def _held(terms: list[str]):
    held = {h for t in terms if (h := access.held_term(t)) is not None}
    return held.__contains__


def _write_points(path: Path) -> None:
    rng = random.Random(SEED)
    pq.write_table(
        pa.table(
            {
                JOIN_COLUMN: pa.array(range(N_ITEMS), type=pa.uint64()),
                "x": pa.array([rng.uniform(0.0, EXTENT_MAX) for _ in range(N_ITEMS)], pa.float32()),
                "y": pa.array([rng.uniform(0.0, EXTENT_MAX) for _ in range(N_ITEMS)], pa.float32()),
                "access": pa.array(
                    [labels_of(s) for s in range(N_ITEMS)], type=pa.list_(pa.string())
                ),
            }
        ),
        path,
    )


CONFIG_TOML = f"""
[sources]
points = "points.parquet"

[[view]]
name             = "{VIEW_ID}"
extent           = {{ x = [0.0, {EXTENT_MAX}], y = [0.0, {EXTENT_MAX}] }}
source           = "points"
point_visibility = {{ field = "access", default = "public" }}

""" + join_attribute_toml("points")


def build_bundle(work_dir: Path) -> Path:
    """Write the corpus and its declaration under `work_dir`, build the bundle and return its
    root."""
    ensure_cli_built()
    work_dir.mkdir(parents=True, exist_ok=True)
    _write_points(work_dir / "points.parquet")
    config = work_dir / "expressions.toml"
    config.write_text(CONFIG_TOML)
    bundle = work_dir / "bundle"
    deployment = write_deployment(work_dir / "tessera.toml", bundle=bundle, schema=config)
    cli_build(deployment, bundle)
    return bundle
