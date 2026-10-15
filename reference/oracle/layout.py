"""Where a shard's and a layer space's files are in a bundle prefix, and the reader of a
publication. A partition's own directory holds its `SEGMENTS-<n>.json`, which names each shard's
and each layer space's own `SEGMENTS-<m>.json` by number and digest; the term dictionary is under
`dictionary/`."""

from __future__ import annotations

import hashlib
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


def segments_name(n: int) -> str:
    """The `SEGMENTS-<n>.json` publication `n` writes in a partition's, a shard's or a layer
    space's directory."""
    return f"SEGMENTS-{n}.json"


def publication_numbers(prefix_dir: Path, partition: str = "default") -> list[int]:
    """The `n` of every partition `SEGMENTS-<n>.json` under `prefix_dir`, ascending. A name that is
    not a number's canonical spelling is left out."""
    found = []
    for path in partition_dir(prefix_dir, partition).glob("SEGMENTS-*.json"):
        number = path.name.removeprefix("SEGMENTS-").removesuffix(".json")
        if number.isdigit() and str(int(number)) == number:
            found.append(int(number))
    return sorted(found)


def _read_named(path: Path, named: dict) -> dict:
    """The shard or space file at `path`, checked against the size and digest `named` gives it."""
    data = path.read_bytes()
    if len(data) != named["size"] or hashlib.sha256(data).hexdigest() != named["sha256"]:
        raise ValueError(f"{path} does not match the size and digest its partition file names")
    return json.loads(data)


def read_publication(prefix_dir: Path, n: int, partition: str = "default") -> dict:
    """Publication `n` of `partition` under `prefix_dir`, assembled: its partition
    `SEGMENTS-<n>.json` with each shard's and each layer space's own `SEGMENTS-<m>.json` it names
    read, checked against the size and digest it names, and merged in, so `shards.records` holds
    the shard records, `layer_spaces` the space records and `files` every file any of them names.
    """
    published = json.loads((partition_dir(prefix_dir, partition) / segments_name(n)).read_bytes())
    files = dict(published.get("files", {}))
    shards = published["shards"]
    records = []
    for named in shards["files"]:
        record = _read_named(
            shard_dir(prefix_dir, partition, named["id"]) / segments_name(named["n"]), named
        )
        if record.get("id") != named["id"]:
            raise ValueError(f"shard {named['id']}'s file holds shard {record.get('id')}'s record")
        files.update(record.pop("files"))
        records.append(record)
    spaces = []
    for named in published["layer_spaces"]:
        record = _read_named(
            layer_space_dir(prefix_dir, named["space"], partition) / segments_name(named["n"]),
            named,
        )
        if record.get("space") != named["space"]:
            raise ValueError(
                f"layer space {named['space']}'s file holds space {record.get('space')}'s record"
            )
        files.update(record.pop("files"))
        spaces.append(record)
    _check_stored_fields(shards["next_id"], records, spaces)
    return {
        **published,
        "files": files,
        "shards": {"size": shards["size"], "next_id": shards["next_id"], "records": records},
        "layer_spaces": spaces,
    }


def newest_publication(prefix_dir: Path, partition: str = "default") -> tuple[int, dict]:
    """The highest-numbered publication of `partition` under `prefix_dir` and its `n`, read as
    [`read_publication`] reads one."""
    numbers = publication_numbers(prefix_dir, partition)
    if not numbers:
        raise FileNotFoundError(f"{partition_dir(prefix_dir, partition)} holds no publication")
    return numbers[-1], read_publication(prefix_dir, numbers[-1], partition)


def _check_stored_fields(next_id: int, records: list[dict], spaces: list[dict]) -> None:
    """Refuses a publication whose shard fields a writer could not have written: a state that is
    neither open nor sealed, an identity that is not a SHA-256 digest, a shard numbered at or past
    `next_id`, or a deletion owed by a shard the publication does not name."""
    ids = {record["id"] for record in records}
    for record in records:
        if record["state"] not in ("open", "sealed"):
            raise ValueError(f"shard {record['id']} is in state {record['state']!r}")
        identity = record["identity"]
        if len(identity) != 64 or any(c not in "0123456789abcdef" for c in identity):
            raise ValueError(f"shard {record['id']}'s identity {identity!r} is not a digest")
        if record["id"] >= next_id:
            raise ValueError(f"shard {record['id']} is numbered at or past next_id {next_id}")
    for space in spaces:
        for owed in space["owed"]:
            if owed["shard"] not in ids:
                raise ValueError(
                    f"layer space {space['space']} names shard {owed['shard']}, which the "
                    "publication does not"
                )
