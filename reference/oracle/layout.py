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


def named_files(
    prefix_dir: Path, published: dict, partition: str = "default"
) -> list[tuple[Path, dict]]:
    """Each shard's and each layer space's own `SEGMENTS-<m>.json` that the partition file
    `published` names, with the reference naming it: the shards' first, then the spaces'."""
    return [
        (shard_dir(prefix_dir, partition, named["id"]) / segments_name(named["n"]), named)
        for named in published["shards"]["files"]
    ] + [
        (layer_space_dir(prefix_dir, named["space"], partition) / segments_name(named["n"]), named)
        for named in published["layer_spaces"]
    ]


def read_publication(prefix_dir: Path, n: int, partition: str = "default") -> dict:
    """Publication `n` of `partition` under `prefix_dir`, with each shard and space file it names
    checked against its size and digest and merged in: `shards.records` holds the shard records,
    `layer_spaces` the space records, and `files` every file any of them names."""
    published = json.loads((partition_dir(prefix_dir, partition) / segments_name(n)).read_bytes())
    files = dict(published.get("files", {}))
    shards = published["shards"]
    records = []
    spaces = []
    for path, named in named_files(prefix_dir, published, partition):
        record = _read_named(path, named)
        kind, key, held = ("shard", "id", records) if "id" in named else ("space", "space", spaces)
        if record.get(key) != named[key]:
            raise ValueError(f"{kind} {named[key]}'s file holds {kind} {record.get(key)}'s record")
        files.update(record.pop("files"))
        held.append(record)
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


def newest_side_manifest(root: Path | str, partition: str = "default") -> dict:
    """The highest-numbered publication of the prefix `CURRENT` names under the bundle root `root`,
    read as [`read_publication`] reads one: what a running server last published, where
    [`Bundle`] reads the build's publication 0."""
    return newest_publication(live_prefix_dir(Path(root)), partition)[1]


def holds_only_the_build_publication(prefix_dir: Path) -> bool:
    """Whether no partition under `prefix_dir` holds a publication other than the build's
    `SEGMENTS-0.json`. A folded bundle holds one publication too, at a number past 0."""
    return all(
        publication_numbers(prefix_dir, partition.name) in ([], [0])
        for partition in (prefix_dir / "partitions").glob("*")
        if partition.is_dir()
    )


def _check_stored_fields(next_id: int, records: list[dict], spaces: list[dict]) -> None:
    """Refuses shard and space fields no writer writes."""
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
