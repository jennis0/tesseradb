"""Read a Tessera bundle directly off disk (Reference Sheet R4, contracts §2.1-§2.3).

Independent of `tessera-store`: this module re-parses `CURRENT`/`MANIFEST.json`/
`SEGMENTS-<n>.json`, `permutation.bin`, `morton.u32`, `postings.arrow` and `columns.arrow` from
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

from . import identity as identity_mod
from . import morton as morton_mod

PERMUTATION_MAGIC = b"TSPM"
PERMUTATION_VERSION = 1
PERMUTATION_ABSENT = 0xFFFF_FFFF

# Finding 6 (task-5 review): an absent MANIFEST `identity` object is, per the memo, "a
# typed reader error, not a default... it does not acquire a minted key, a zero key or a
# legacy path" (docs/design-memos/2026-07-30-tessera-id-construction.md §2). The fallback
# below violates that rule on purpose, as a temporary scaffold: no bundle in this checkout
# carries an `identity` object yet, because tessera-build/tessera-store have not been
# repointed at the tessera_id column (only tessera-types/identity.rs has landed as of
# Task 5/12). REMOVE THIS FALLBACK the moment Task 6/7 land build-side `identity` emission
# -- at that point every bundle this oracle reads is post-r6 and an absent `identity`
# object must raise, full stop.
PRE_R6_IDENTITY_FALLBACK_REMOVE_AT = (
    "Task 6/7: tessera-build/tessera-store emitting MANIFEST `identity` and the "
    "`tessera_id` column"
)


def _sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@dataclass
class Segment:
    """One (partition, slice, seg_id)'s row-space geometry.

    Contracts r6 replaced the `entity_id` column in `columns.arrow` with `tessera_id`
    (`docs/design-memos/2026-07-30-tessera-id-construction.md`): `tessera_id` is now read
    directly off the row, and `entity_id` is *derived* -- either by inverting it through
    `identity.invert` (pure, no file I/O) when the bundle carries an `identity` key, or, for
    a pre-r6 bundle that still stores `entity_id` directly, read as before. Exactly one of
    `tessera_id`/`entity_id` is the bundle's stored column; the other is always derived, and
    both are kept on `Segment` so callers (`viewport.py`'s entity-space mask membership,
    `test_byte_scan.py`'s entity-ID sweep) do not need to know which.
    """

    entity_id: np.ndarray  # uint64, row order (stored pre-r6, DERIVED post-r6)
    x: np.ndarray  # float32, row order
    y: np.ndarray  # float32, row order
    morton: np.ndarray  # uint32, row order (raw sorted codes from morton.u32)
    row_count: int
    tessera_id: np.ndarray | None = None  # uint64, row order (stored post-r6; absent pre-r6)


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

        # `identity` (contracts r6, docs/design-memos/2026-07-30-tessera-id-construction.md
        # §2): the per-deployment key and the §13.3 shard prefix `tessera_id` is built
        # under. A bundle that *does* carry `identity` is read strictly, per the memo's
        # fail-closed rule: bad construction/rounds/key/shard_id/epoch all refuse, none
        # default. An absent object takes the PRE_R6_IDENTITY_FALLBACK_REMOVE_AT scaffold
        # path above instead of raising -- see its docstring for why that is still
        # tolerated and when it must go.
        identity_obj = self.manifest.get("identity")
        if identity_obj is not None:
            if identity_obj.get("construction") != "feistel-splitmix64-v1":
                raise ValueError(
                    f"unsupported identity construction {identity_obj.get('construction')!r}"
                )
            if identity_obj.get("rounds") != identity_mod.ROUNDS:
                raise ValueError(f"unsupported identity round count {identity_obj.get('rounds')!r}")
            self.identity_key: identity_mod.IdentityKey | None = identity_mod.IdentityKey.from_hex(
                identity_obj["key"]
            )
            self.identity_shard_id: int | None = identity_obj["shard_id"]
            if "epoch" not in identity_obj:
                raise ValueError("manifest `identity` object is missing `epoch`")
            self.identity_epoch: int | None = identity_obj["epoch"]
        else:
            self.identity_key = None
            self.identity_shard_id = None
            self.identity_epoch = None

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
            perm_path = seg_dir.parent.parent / "permutation.bin"
            self._segment_cache[slice_id] = _read_segment(seg_dir, perm_path)
        return self._segment_cache[slice_id]

    def verify_identity_cross_check(self, slice_id: str, sample: int = 200) -> None:
        """The only test that catches a key/column disagreement (Task 12 brief, Step 2): for
        a sample of rows, `identity.forward(shard, entity_of_row[r]) == tessera_id[r]`, where
        `entity_of_row` came from the permutation (key-independent) and `tessera_id` came
        from the stored column. Requires a post-r6 bundle."""
        if self.identity_key is None:
            raise ValueError("bundle has no `identity` object in MANIFEST (pre-r6 bundle)")
        seg = self.segment(slice_id)
        if seg.tessera_id is None:
            raise ValueError("segment has no stored tessera_id column (pre-r6 bundle)")
        rng = np.random.default_rng(20260730)
        n = min(sample, seg.row_count)
        rows = rng.choice(seg.row_count, size=n, replace=False) if seg.row_count else np.array([])
        for row in rows:
            entity_id = int(seg.entity_id[row])
            expected = int(seg.tessera_id[row])
            got = identity_mod.forward(self.identity_key, self.identity_shard_id, entity_id)
            if got != expected:
                raise ValueError(
                    f"identity cross-check failed at row {row}: entity {entity_id} forwards to "
                    f"{got:#x}, stored tessera_id is {expected:#x}"
                )

    def tessera_id_of(self, entity_id: int) -> int:
        """Pure function, no file read (memo §6): `forward(identity.key, identity.shard_id,
        entity_id)`. Requires a post-r6 bundle (`identity` present in MANIFEST)."""
        if self.identity_key is None:
            raise ValueError("bundle has no `identity` object in MANIFEST (pre-r6 bundle)")
        return identity_mod.forward(self.identity_key, self.identity_shard_id, entity_id)

    def derive_row_order(self, slice_id: str) -> np.ndarray:
        """Re-derive row order from `(morton_of(x, y, extent), forward(identity.key,
        identity.shard_id, entity_id))` ascending, with no further tiebreak (the
        priority-as-identity-prefix fold; `tessera_id` is already unique so nothing else is
        needed to break ties). Row order is therefore key-dependent, where it previously
        was not -- this reads `identity.key` and `identity.shard_id` from MANIFEST, which
        `Bundle.__init__` already parses.

        Delegates to `row_order_from_geometry`, the module-level, key-dependent
        re-derivation -- computed from `(x, y)` and the permutation-derived `entity_id`,
        **never from the stored `morton`/`tessera_id` columns** (finding 5): a build that
        emitted a wrong `tessera_id` column and sorted consistently by its own wrong
        values must fail this check, not pass it. `test_identity.py` calls the same
        function, so the shipped path is the tested path.

        Returns the row indices that would produce sorted order, i.e. `stored_order[result]`
        is the re-derived order; compare against `np.arange(row_count)` to check the stored
        columns are already in that order.
        """
        if self.identity_key is None:
            raise ValueError("bundle has no `identity` object in MANIFEST (pre-r6 bundle)")
        seg = self.segment(slice_id)
        return row_order_from_geometry(
            self.identity_key, self.identity_shard_id, seg.entity_id, seg.x, seg.y, self.extent
        )

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

    def _ext_locator_path(self) -> Path:
        # memo §7: "one positional u32 locator into the sorted external-ID extents
        # (entities/ext-locator.u32, singular -- no <k> suffix)".
        return self.prefix_dir / "entities" / "ext-locator.u32"

    def sidecar_round_trips(self, sample: int = 50) -> None:
        """For a sample of entities: `external_id_of(e)` resolves back to `e` through the
        sorted extents. Both directions of a mapping stored once sorted by key and once
        indexed by entity; they must agree or `/control/changes` addresses the wrong item
        (memo §7)."""
        entities = list(getattr(self, "_entity_to_external", {}) or {})
        if not entities:
            self.external_id_of(next(iter(self._known_entity_ids())))  # populate the cache
            entities = list(self._entity_to_external)
        rng = np.random.default_rng(20260730)
        n = min(sample, len(entities))
        if n == 0:
            return
        chosen = rng.choice(entities, size=n, replace=False)
        for entity_id in chosen:
            entity_id = int(entity_id)
            ext = self.external_id_of(entity_id)
            # Re-derive entity -> external via the locator sidecar, if present (post-r6
            # shape); otherwise the check degenerates to "the map is self-consistent",
            # which is still a real (if weaker) check on a pre-r6 bundle.
            locator_path = self._ext_locator_path()
            if locator_path.exists():
                locator = np.fromfile(locator_path, dtype="<u4")
                if entity_id >= len(locator):
                    raise ValueError(
                        f"entity {entity_id} has no ext-locator slot (locator has "
                        f"{len(locator)} entries)"
                    )
                slot = int(locator[entity_id])
                if slot == 0xFFFFFFFF:
                    raise ValueError(
                        f"entity {entity_id} has an external_id ({ext!r}) but its "
                        "ext-locator slot is the no-external-id sentinel"
                    )
            resolved = self._entity_to_external[entity_id]
            if resolved != ext:
                raise ValueError(
                    f"sidecar round-trip failed for entity {entity_id}: {resolved!r} != {ext!r}"
                )

    def _known_entity_ids(self):
        for rel in self.segments_manifest.get("external_id_extents", []):
            path = self.prefix_dir / rel
            with ipc.open_file(path) as reader:
                table = reader.read_all()
            for ent in table.column("entity_id").to_pylist():
                yield ent

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


def row_order_from_geometry(
    key: identity_mod.IdentityKey,
    shard_id: int,
    entity_ids: np.ndarray,
    x: np.ndarray,
    y: np.ndarray,
    extent: tuple[float, float, float, float],
) -> np.ndarray:
    """The single, module-level row-order re-derivation (finding 5, task-5 review): sort
    ascending by `(morton_of(x, y, extent), forward(key, shard_id, entity_id))`, computed
    from geometry and the permutation-derived entity id -- never by reading the stored
    `morton`/`tessera_id` columns, so a build that emits a wrong column but sorts
    consistently by its own wrong values does not pass this check.

    `Bundle.derive_row_order` and `test_identity.py`'s
    `test_row_order_is_morton_then_tessera_id_ascending` both call this function rather
    than each re-implementing the lexsort inline, so the shipped ordering path is the
    tested ordering path: swapping the two `np.lexsort` arguments here breaks the test
    directly, instead of the test silently re-deriving the same (possibly also swapped)
    order alongside it.

    Uses `identity.row_sort_key` (not a hand-inlined `forward` + tuple) so that function
    has a real call site too.
    """
    n = len(entity_ids)
    mortons = np.empty(n, dtype=np.uint64)
    tesseras = np.empty(n, dtype=np.uint64)
    for i in range(n):
        morton_code = morton_mod.morton_of(float(x[i]), float(y[i]), extent)
        morton_code, tessera_id = identity_mod.row_sort_key(
            key, shard_id, int(entity_ids[i]), morton_code
        )
        mortons[i] = morton_code
        tesseras[i] = tessera_id
    # np.lexsort sorts by the LAST key primary -- (morton, tessera_id) ascending means
    # tessera_id is the secondary (fastest-varying) key, morton primary.
    return np.lexsort((tesseras, mortons))


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


def _entity_of_rows(perm: Permutation, rows: np.ndarray) -> dict[int, int]:
    """row -> entity, by inverting `permutation.bin`'s entity_to_row for TOUCHED rows only.

    Contracts r6 removed the entity_id column from `columns.arrow`; the permutation is the
    only key-independent artefact relating the two spaces (§5.1, I4). Computed for the rows
    asked about rather than materialised whole: a full inversion at 10^9 needs ~17-20 GB
    transient (brief), so the scan over `perm.slots` is chunked in slices of 2**24 and only
    entries landing in `rows` are collected.
    """
    wanted = np.asarray(rows, dtype=np.uint32)
    remaining = set(int(r) for r in wanted)
    result: dict[int, int] = {}
    chunk = 1 << 24
    bound = perm.bound
    for start in range(0, bound, chunk):
        if not remaining:
            break
        end = min(start + chunk, bound)
        window = perm.slots[start:end]
        matches = np.isin(window, wanted)
        if not matches.any():
            continue
        for offset in np.nonzero(matches)[0]:
            row_val = int(window[offset])
            if row_val in remaining:
                result[row_val] = start + int(offset)
                remaining.discard(row_val)
    return result


def _read_segment(seg_dir: Path, perm_path: Path | None = None) -> Segment:
    columns_path = seg_dir / "columns.arrow"
    morton_path = seg_dir / "morton.u32"

    with ipc.open_file(columns_path) as reader:
        table = reader.read_all()
    x = table.column("x").to_numpy(zero_copy_only=False).astype(np.float32)
    y = table.column("y").to_numpy(zero_copy_only=False).astype(np.float32)

    morton_bytes = morton_path.read_bytes()
    morton = np.frombuffer(morton_bytes, dtype="<u4")
    row_count = len(x)
    if len(morton) != row_count:
        raise ValueError(
            f"{seg_dir}: columns.arrow has {row_count} rows but morton.u32 has {len(morton)}"
        )

    column_names = set(table.schema.names)
    if "tessera_id" in column_names:
        # Post-r6: columns.arrow stores tessera_id; entity_id is DERIVED via the
        # permutation (the key-independent direction, memo §7 -- "there is no
        # tessera_id -> entity sidecar").
        tessera_id = table.column("tessera_id").to_numpy(zero_copy_only=False).astype(np.uint64)
        if perm_path is None or not perm_path.exists():
            raise ValueError(
                f"{seg_dir}: columns.arrow has tessera_id but no permutation.bin was given to "
                "derive entity_id from"
            )
        perm = _read_permutation(perm_path)
        rows = np.arange(row_count, dtype=np.uint32)
        row_to_entity = _entity_of_rows(perm, rows)
        missing = row_count - len(row_to_entity)
        if missing:
            raise ValueError(
                f"{seg_dir}: permutation.bin accounts for {len(row_to_entity)} of {row_count} "
                f"rows; {missing} rows have no entity in the permutation"
            )
        entity_id = np.array(
            [row_to_entity[r] for r in range(row_count)], dtype=np.uint64
        )
    elif "entity_id" in column_names:
        # Pre-r6: columns.arrow stores entity_id directly.
        entity_id = table.column("entity_id").to_numpy(zero_copy_only=False).astype(np.uint64)
        tessera_id = None
    else:
        raise ValueError(f"{seg_dir}: columns.arrow has neither entity_id nor tessera_id")

    return Segment(
        entity_id=entity_id,
        x=x,
        y=y,
        morton=morton,
        row_count=row_count,
        tessera_id=tessera_id,
    )


def _read_postings_array(path: Path) -> pa.Array:
    with ipc.open_file(path) as reader:
        table = reader.read_all()
    return table.column("posting").combine_chunks()
