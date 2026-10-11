"""Where a shard's files are in a bundle prefix.

Every per-entity and per-view structure of a partition's point shard is under
`partitions/<partition>/shards/<shard>/`. A partition's own directory holds only its
`SEGMENTS-<n>.json`, and the term dictionary is under `dictionary/` for the whole bundle.
Transcribed from the design rather than imported from the Rust, like the rest of the oracle.
"""

from __future__ import annotations

from pathlib import Path


def shard_dir(prefix_dir: Path, partition: str = "default", shard: int = 0) -> Path:
    """`partitions/<partition>/shards/<shard>/` under `prefix_dir`."""
    return prefix_dir / "partitions" / partition / "shards" / str(shard)
