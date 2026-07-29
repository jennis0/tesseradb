"""Read a Tessera bundle directly off disk (Reference Sheet R4, contracts §2.1-§2.3).

Independent of `tessera-store`: this module re-parses `CURRENT`/`MANIFEST.json`/
`SEGMENTS-<n>.json`, `permutation.bin`, `morton.u64`, `postings.arrow` and `columns.arrow` from
their byte-level definitions, verifying every file digest the manifests name along the way. Tag-1
postings records are read via `pyroaring.BitMap.deserialize`, which reads the portable Roaring
format directly — this doubles as the cross-implementation portable-format check the brief calls
for.
"""

from __future__ import annotations

import hashlib
import json
import struct
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc
from pyroaring import BitMap

PERMUTATION_MAGIC = b"TSPM"
PERMUTATION_VERSION = 1
PERMUTATION_ABSENT = 0xFFFF_FFFF


def _sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@dataclass
class Segment:
    """One (partition, slice, seg_id)'s row-space geometry."""

    entity_id: np.ndarray  # uint64, row order
    x: np.ndarray  # float32, row order
    y: np.ndarray  # float32, row order
    morton: np.ndarray  # uint64, row order (raw sorted codes from morton.u64)
    row_count: int


@dataclass
class Permutation:
    """`permutation.bin`: entity id -> row id (or absent), for one slice's single segment."""

    bound: int
    slots: np.ndarray  # uint32, length == bound; PERMUTATION_ABSENT where entity has no row

    def row_of(self, entity_id: int) -> int | None:
        if entity_id >= self.bound:
            return None
        row = int(self.slots[entity_id])
        return None if row == PERMUTATION_ABSENT else row


class Bundle:
    """A verified, opened bundle: `CURRENT` -> `MANIFEST.json` -> `SEGMENTS-0.json` -> segments.

    Phase 1 has exactly one partition (`default`) and, per slice, exactly one segment — matching
    the build's own scope (tessera-build's module doc).
    """

    def __init__(self, root: Path | str):
        self.root = Path(root)
        current = json.loads((self.root / "CURRENT").read_text())
        self.prefix = current["prefix"]
        self.prefix_dir = self.root / self.prefix

        manifest_path = self.prefix_dir / "MANIFEST.json"
        manifest_bytes = manifest_path.read_bytes()
        digest = _sha256_hex(manifest_bytes)
        if digest != current["manifest_digest"]:
            raise ValueError(
                f"CURRENT names manifest digest {current['manifest_digest']} but "
                f"MANIFEST.json hashes to {digest}"
            )
        self.manifest = json.loads(manifest_bytes)
        self._verify_files(self.manifest["files"])

        self.quantisation = self.manifest["quantisation"]
        self.extent = (
            self.quantisation["x_min"],
            self.quantisation["x_max"],
            self.quantisation["y_min"],
            self.quantisation["y_max"],
        )

        # Phase 1: exactly one partition, "default".
        partition_dir = self.prefix_dir / "partitions" / "default"
        segments_path = partition_dir / "SEGMENTS-0.json"
        self.segments_manifest = json.loads(segments_path.read_bytes())
        self._verify_files(self.segments_manifest.get("files", {}))

        self._partition_dir = partition_dir
        self._segment_cache: dict[str, Segment] = {}
        self._permutation_cache: dict[str, Permutation] = {}

        # Dictionary: term_id (ordinal) -> descriptor bytes.
        self.dictionary: list[bytes] = []
        for extent_entry in self.segments_manifest.get("dict_extents", []):
            dict_path = self.prefix_dir / extent_entry["path"]
            self.dictionary.extend(_read_dictionary(dict_path))
        self.descriptor_to_term_id = {d: i for i, d in enumerate(self.dictionary)}

        self._pairs_path_cache: Path | None = None

    def _verify_files(self, files: dict) -> None:
        for rel, info in files.items():
            path = self.prefix_dir / rel
            data = path.read_bytes()
            if len(data) != info["size"]:
                raise ValueError(f"{rel}: size {len(data)} != manifest's {info['size']}")
            got = _sha256_hex(data)
            if got != info["sha256"]:
                raise ValueError(f"{rel}: sha256 {got} != manifest's {info['sha256']}")

    def term_id_of(self, descriptor: bytes) -> int | None:
        return self.descriptor_to_term_id.get(descriptor)

    def segment_dir(self, slice_id: str) -> Path:
        for seg in self.segments_manifest["segments"]:
            if seg["slice"] == slice_id:
                return (
                    self._partition_dir
                    / "slices"
                    / slice_id
                    / "segments"
                    / seg["seg_id"]
                )
        raise KeyError(f"no segment for slice '{slice_id}'")

    def segment(self, slice_id: str) -> Segment:
        if slice_id not in self._segment_cache:
            seg_dir = self.segment_dir(slice_id)
            self._segment_cache[slice_id] = _read_segment(seg_dir)
        return self._segment_cache[slice_id]

    def permutation(self, slice_id: str) -> Permutation:
        if slice_id not in self._permutation_cache:
            path = self._partition_dir / "slices" / slice_id / "permutation.bin"
            self._permutation_cache[slice_id] = _read_permutation(path)
        return self._permutation_cache[slice_id]

    def pairs_path(self, slice_id: str = "default") -> Path:
        # Phase 1 has one partition; pairs.parquet lives at partitions/default/terms/pairs.parquet
        # regardless of slice (postings/pairs are entity-space, not slice-scoped).
        return self._partition_dir / "terms" / "pairs.parquet"

    def external_id_of(self, entity_id: int) -> bytes:
        """Invert `entities/external-ids-0.arrow` (sorted by external_id bytes) to find the
        external id for a given entity id — needed to address `/control/changes` at a specific
        entity (the tessera-build convention: 8 bytes little-endian of the source corpus id)."""
        if not hasattr(self, "_entity_to_external"):
            mapping: dict[int, bytes] = {}
            for rel in self.segments_manifest.get("external_id_extents", []):
                path = self.prefix_dir / rel
                with ipc.open_file(path) as reader:
                    table = reader.read_all()
                ext_col = table.column("external_id").to_pylist()
                ent_col = table.column("entity_id").to_pylist()
                for ext, ent in zip(ext_col, ent_col):
                    mapping[ent] = ext
            self._entity_to_external = mapping
        return self._entity_to_external[entity_id]

    def postings(self, term_id: int) -> np.ndarray:
        """Term `term_id`'s sorted entity-id array, decoded from `terms/postings.arrow`."""
        if not hasattr(self, "_postings_array"):
            self._postings_array = _read_postings_array(self._partition_dir / "terms" / "postings.arrow")
        record = self._postings_array[term_id].as_py()
        tag = record[0]
        payload = record[1:]
        if tag == 0:
            return np.frombuffer(payload, dtype="<u4")
        if tag == 1:
            bm = BitMap.deserialize(payload)
            return np.fromiter(bm, dtype=np.uint32, count=len(bm))
        raise ValueError(f"postings.arrow: term {term_id} has unknown tag {tag}")


def _read_dictionary(path: Path) -> list[bytes]:
    """Records: u32 LE length ‖ descriptor bytes; term_id = ordinal (Reference Sheet R4)."""
    data = path.read_bytes()
    out = []
    offset = 0
    while offset < len(data):
        (length,) = struct.unpack_from("<I", data, offset)
        offset += 4
        out.append(data[offset : offset + length])
        offset += length
    return out


def _read_permutation(path: Path) -> Permutation:
    data = path.read_bytes()
    if data[0:4] != PERMUTATION_MAGIC:
        raise ValueError(f"{path}: bad permutation magic {data[0:4]!r}")
    (version,) = struct.unpack_from("<H", data, 4)
    if version != PERMUTATION_VERSION:
        raise ValueError(f"{path}: unsupported permutation version {version}")
    (bound,) = struct.unpack_from("<Q", data, 8)
    slots = np.frombuffer(data, dtype="<u4", count=bound, offset=16)
    return Permutation(bound=bound, slots=slots)


def _read_segment(seg_dir: Path) -> Segment:
    columns_path = seg_dir / "columns.arrow"
    morton_path = seg_dir / "morton.u64"

    with ipc.open_file(columns_path) as reader:
        table = reader.read_all()
    entity_id = table.column("entity_id").to_numpy(zero_copy_only=False).astype(np.uint64)
    x = table.column("x").to_numpy(zero_copy_only=False).astype(np.float32)
    y = table.column("y").to_numpy(zero_copy_only=False).astype(np.float32)

    morton_bytes = morton_path.read_bytes()
    morton = np.frombuffer(morton_bytes, dtype="<u8")

    row_count = len(entity_id)
    if len(morton) != row_count:
        raise ValueError(
            f"{seg_dir}: columns.arrow has {row_count} rows but morton.u64 has {len(morton)}"
        )
    return Segment(entity_id=entity_id, x=x, y=y, morton=morton, row_count=row_count)


def _read_postings_array(path: Path) -> pa.Array:
    with ipc.open_file(path) as reader:
        table = reader.read_all()
    return table.column("posting").combine_chunks()
