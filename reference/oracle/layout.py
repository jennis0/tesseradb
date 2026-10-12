"""Where a shard's and a layer space's files are in a bundle prefix. A partition's own directory
holds its `SEGMENTS-<n>.json`, its shards and its layer spaces; the term dictionary is under
`dictionary/`."""

from __future__ import annotations

import json
from pathlib import Path

#: The separator between a group's name and a view's key in a view id: `quarter:2026-Q3`.
#: `view_dir` is the one place it is split on.
GROUP_SEPARATOR = ":"


def live_prefix_dir(root: Path) -> Path:
    """The prefix directory `CURRENT` names under the bundle root `root`."""
    return root / json.loads((root / "CURRENT").read_text())["prefix"]


def partition_dir(prefix_dir: Path, partition: str = "default") -> Path:
    """`partitions/<partition>/` under `prefix_dir`."""
    return prefix_dir / "partitions" / partition


def shard_dir(prefix_dir: Path, partition: str = "default", shard: int = 0) -> Path:
    """`partitions/<partition>/shards/<shard>/` under `prefix_dir`."""
    return partition_dir(prefix_dir, partition) / "shards" / str(shard)


def layer_space_dir(prefix_dir: Path, space: int, partition: str = "default") -> Path:
    """`partitions/<partition>/layers/<space>/` under `prefix_dir`: one annotation layer's records,
    supplied content and held shapes."""
    return partition_dir(prefix_dir, partition) / "layers" / str(space)


def view_dir(shard_dir: Path, view_id: str) -> Path:
    """`views/<view>/`, or `views/<group>/<key>/`, under `shard_dir`. A grouped view's directory
    is two components because `:` is not a path character everywhere."""
    path = shard_dir / "views"
    for component in view_id.split(GROUP_SEPARATOR, 1):
        path = path / component
    return path
