"""Where a shard's files are in a bundle prefix. A partition's own directory holds only its
`SEGMENTS-<n>.json`; the term dictionary is under `dictionary/`."""

from __future__ import annotations

from pathlib import Path

#: The separator between a group's name and a view's key in a view id: `quarter:2026-Q3`.
#: `view_dir` is the one place it is split on.
GROUP_SEPARATOR = ":"


def partition_dir(prefix_dir: Path, partition: str = "default") -> Path:
    """`partitions/<partition>/` under `prefix_dir`."""
    return prefix_dir / "partitions" / partition


def shard_dir(prefix_dir: Path, partition: str = "default", shard: int = 0) -> Path:
    """`partitions/<partition>/shards/<shard>/` under `prefix_dir`."""
    return partition_dir(prefix_dir, partition) / "shards" / str(shard)


def view_dir(shard_dir: Path, view_id: str) -> Path:
    """`views/<view>/`, or `views/<group>/<key>/`, under `shard_dir`. A grouped view's directory
    is two components because `:` is not a path character everywhere."""
    path = shard_dir / "views"
    for component in view_id.split(GROUP_SEPARATOR, 1):
        path = path / component
    return path
