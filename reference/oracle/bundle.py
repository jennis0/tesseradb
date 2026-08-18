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
# legacy path" (docs/evidence/memos/2026-07-30-tessera-id-construction.md §2). The fallback
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
    """One (partition, view, seg_id)'s row-space geometry.

    Contracts r6 replaced the `entity_id` column in `columns.arrow` with `tessera_id`
    (`docs/evidence/memos/2026-07-30-tessera-id-construction.md`): `tessera_id` is now read
    directly off the row, and `entity_id` is *derived* -- either by inverting it through
    `identity.invert` (pure, no file I/O) when the bundle carries an `identity` key, or, for
    a pre-r6 bundle that still stores `entity_id` directly, read as before. Exactly one of
    `tessera_id`/`entity_id` is the bundle's stored column; the other is always derived, and
    both are kept on `Segment` so callers (`viewport.py`'s entity-space mask membership,
    `test_byte_scan.py`'s entity-ID sweep) do not need to know which.
    """

    entity_id: np.ndarray  # uint64, row order (stored pre-r6, DERIVED post-r6)
    residual: np.ndarray  # uint32, row order (the position's low half, from columns.arrow)
    morton: np.ndarray  # uint32, row order (raw sorted codes from morton.u32)
    row_count: int
    tessera_id: np.ndarray | None = None  # uint64, row order (stored post-r6; absent pre-r6)

    def stored_code(self, row: int) -> int:
        """Row `row`'s position as the bundle stores it: the two words concatenated.

        **Stored, therefore not independent.** This is what the engine would have to agree with
        itself about, so it is the right thing for a *wire* comparison (does the served point
        carry the position the segment holds?) and the wrong thing for a *geometry* comparison
        (is that position where the source said the item is?). The second question is what
        `Bundle.row_position_codes` answers, and it does not read this.
        """
        return (int(self.morton[row]) << 32) | int(self.residual[row])


@dataclass
class SourceGeometry:
    """The points file the build consumed, read back as 32-bit fixed point per axis.

    **The oracle's third input, and the reason it exists.** `columns.arrow` no longer holds
    coordinates: a position is the cell code in `morton.u32` plus a residual, and both were
    written by the build. Recomputing "the" Morton code from those would be
    `code == interleave(deinterleave(code))` — a tautology that a build emitting a wrong column
    and then sorting consistently by its own wrong values passes. So the geometry the differential
    checks against comes from **upstream of the build**, which is strictly stronger than the
    `(x, y)` columns it replaces.

    `conformance.md` §1 states the oracle's inputs and treats the count as load-bearing;
    `reference/tests/test_oracle_layering.py` keeps the definitional modules from reaching a
    fixture builder. Neither is circumvented here: this class *reads a path it is given*. It never
    finds one, and `Bundle` never opens one on its own — a driver calls
    `Bundle.attach_source_geometry`, exactly as the fixture already hands in the identity key.

    Nothing yet binds a points file to a bundle — no digest, no manifest entry — so a runner can
    hand in the wrong file, and `catalogue.py` records that drift having already bitten this suite
    once. Under this design it surfaces as a whole-suite geometry failure that reads like an
    engine bug. A source digest in `MANIFEST.json` would close it; that is not built (see the
    hot-row-geometry design §5.2), and until it is, the binding is the harness's discipline.
    """

    qx: dict[int, int]
    qy: dict[int, int]

    def position(self, source_id: int) -> tuple[int, int]:
        if source_id not in self.qx:
            raise KeyError(
                f"source geometry has no row for source id {source_id}: the points file handed "
                "to the oracle is not the one this bundle was built from"
            )
        return self.qx[source_id], self.qy[source_id]


def _row_groups_worth_reading(reader, limit: int | None) -> list[int]:
    """Row groups that may hold a row with `entity_id < limit`, by their own statistics.

    Mirrors the importer's own row-group filter (`tessera_build::input`), and for the same
    reason: the fixture corpus is 10⁹ rows and the fixture bundle a 250,000-row prefix, so a
    reader that visits every group to find a prefix is not slow, it is unusable. Returns **all**
    groups when there is no limit or no usable statistic — the filter may never drop a group it
    cannot prove is excluded.
    """
    metadata = reader.metadata
    if limit is None:
        return list(range(metadata.num_row_groups))
    column = reader.schema_arrow.names.index("entity_id")
    keep = []
    for i in range(metadata.num_row_groups):
        stats = metadata.row_group(i).column(column).statistics
        if stats is None or not stats.has_min_max or stats.min < limit:
            keep.append(i)
    return keep


def read_source_geometry(
    path: Path | str,
    extent: tuple[float, float, float, float],
    limit: int | None = None,
) -> SourceGeometry:
    """Read a points Parquet into [`SourceGeometry`], mirroring the importer's three schemas.

    Independently derived from `contracts` §2.5 and the importer's documented branches, not from
    the Rust — which is the whole point of an oracle. The branches, checked in this order:

    1. `entity_id` + `x` + `y` — coordinates quantised against `extent` by `fixed32`, the one
       place quantisation happens.
    2. `entity_id` + `morton` + `residual` — already in this form; the two words are reassembled
       rather than converted. Full 32 bits per axis.
    3. `entity_id` + `morton` — 16 bits per axis, widened with a zero residual, because that is
       genuinely all the file says about the point.

    Both Morton branches require the identity extent `[0, 65536)`, where `cell(v) = v`; under any
    other extent the cell indices would be re-quantised as though they were coordinates in that
    extent's units. The importer errors there and so does this.

    `limit` mirrors `tessera build --limit`: keep source rows with `entity_id < limit`. **Passing
    it is not an optimisation** — see [`_row_groups_worth_reading`].

    The per-row arithmetic is vectorised in numpy rather than written as the loop the rest of this
    oracle prefers. That is a deliberate exception to "definitions, not algorithms": the quantities
    are the same quantities, and a Python loop over even the 250,000-row prefix — let alone the
    groups a coarser statistic fails to exclude — costs minutes per test session. The definitions
    themselves (`fixed32`, the interleave) stay scalar in `morton.py`; what is vectorised here is
    only the extraction.
    """
    import pyarrow.parquet as pq  # local: keeps the module's import surface to what it always uses

    reader = pq.ParquetFile(path)
    names = set(reader.schema_arrow.names)
    if "entity_id" not in names:
        raise ValueError(f"{path}: points file has no `entity_id` column")

    x_min, x_max, y_min, y_max = extent
    if "x" in names and "y" in names:
        columns = ["entity_id", "x", "y"]
        morton_branch = False
    elif "morton" in names:
        if extent != (0.0, 65536.0, 0.0, 65536.0):
            raise ValueError(
                f"{path}: a Morton points file is only meaningful under the identity extent "
                f"[0, 65536), where cell(v) = v; this bundle declares {extent}"
            )
        columns = ["entity_id", "morton"] + (["residual"] if "residual" in names else [])
        morton_branch = True
    else:
        raise ValueError(f"{path}: points file has neither `x`/`y` nor `morton`")

    qx: dict[int, int] = {}
    qy: dict[int, int] = {}
    for group in _row_groups_worth_reading(reader, limit):
        table = reader.read_row_group(group, columns=columns)
        source_ids = table.column("entity_id").to_numpy(zero_copy_only=False).astype(np.uint64)
        if limit is not None:
            keep = source_ids < np.uint64(limit)
            if not keep.any():
                continue
        else:
            keep = slice(None)
        source_ids = source_ids[keep]

        if morton_branch:
            hi = table.column("morton").to_numpy(zero_copy_only=False).astype(np.uint64)[keep]
            lo = (
                table.column("residual").to_numpy(zero_copy_only=False).astype(np.uint64)[keep]
                if "residual" in columns
                else np.zeros(len(source_ids), dtype=np.uint64)
            )
            codes = (hi << np.uint64(32)) | lo
            axis_x = _compact64(codes)
            axis_y = _compact64(codes >> np.uint64(1))
        else:
            xs = table.column("x").to_numpy(zero_copy_only=False).astype(np.float64)[keep]
            ys = table.column("y").to_numpy(zero_copy_only=False).astype(np.float64)[keep]
            axis_x = _fixed32_vec(xs, x_min, x_max)
            axis_y = _fixed32_vec(ys, y_min, y_max)

        qx.update(zip(source_ids.tolist(), axis_x.tolist()))
        qy.update(zip(source_ids.tolist(), axis_y.tolist()))

    return SourceGeometry(qx=qx, qy=qy)


def _fixed32_vec(v: np.ndarray, vmin: float, vmax: float) -> np.ndarray:
    """[`morton.fixed32`] over an array. Pinned against the scalar definition by
    `test_differential`'s byte-for-byte position check, which compares what this produces against
    what the engine stored, so a divergence in the clamp or the rounding fails there."""
    scaled = np.floor((v - vmin) / (vmax - vmin) * 4294967296.0)
    return np.clip(scaled, 0.0, 4294967295.0).astype(np.uint32).astype(np.uint64)


def _compact64(code: np.ndarray) -> np.ndarray:
    """Gather the even bits of a 64-bit interleave into a 32-bit axis — the array form of
    `morton.deinterleave64`'s per-axis half."""
    x = code & np.uint64(0x5555555555555555)
    x = (x | (x >> np.uint64(1))) & np.uint64(0x3333333333333333)
    x = (x | (x >> np.uint64(2))) & np.uint64(0x0F0F0F0F0F0F0F0F)
    x = (x | (x >> np.uint64(4))) & np.uint64(0x00FF00FF00FF00FF)
    x = (x | (x >> np.uint64(8))) & np.uint64(0x0000FFFF0000FFFF)
    x = (x | (x >> np.uint64(16))) & np.uint64(0x00000000FFFFFFFF)
    return x


@dataclass
class Permutation:
    """`permutation.bin`: entity id -> row id (or absent), for one view's single segment."""

    bound: int
    slots: np.ndarray  # uint32, length == bound; PERMUTATION_ABSENT where entity has no row

    def row_of(self, entity_id: int) -> int | None:
        if entity_id >= self.bound:
            return None
        row = int(self.slots[entity_id])
        return None if row == PERMUTATION_ABSENT else row


class Bundle:
    """A verified, opened bundle: `CURRENT` -> `MANIFEST.json` -> `SEGMENTS-0.json` -> segments.

    Phase 1 has exactly one partition (`default`) and, per view, exactly one segment — matching
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

        # `identity` (contracts r6, docs/evidence/memos/2026-07-30-tessera-id-construction.md
        # §2): the per-deployment key and the §13.3 shard prefix `tessera_id` is built
        # under. A bundle that *does* carry `identity` is read strictly, per the memo's
        # fail-closed rule: bad construction/rounds/key/shard_id/idset all refuse, none
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
            if "idset" not in identity_obj:
                raise ValueError("manifest `identity` object is missing `idset`")
            self.idset: int | None = identity_obj["idset"]
        else:
            self.identity_key = None
            self.identity_shard_id = None
            self.idset = None

        # Phase 1: exactly one partition, "default".
        partition_dir = self.prefix_dir / "partitions" / "default"
        segments_path = partition_dir / "SEGMENTS-0.json"
        self.segments_manifest = json.loads(segments_path.read_bytes())
        self._verify_files(self.segments_manifest.get("files", {}))

        self._partition_dir = partition_dir
        self._segment_cache: dict[str, Segment] = {}
        self._permutation_cache: dict[str, Permutation] = {}
        # Derived per-view columns: pure functions of geometry and of the permutation, so they
        # never vary with a mask and can safely be held for the bundle's lifetime. See
        # `row_morton_codes` for why the Morton one is recomputed rather than read.
        self._morton_cache: dict[str, list[int]] = {}
        self._entity_list_cache: dict[str, list[int]] = {}

        # Dictionary: term_id (ordinal) -> descriptor bytes.
        self.dictionary: list[bytes] = []
        for extent_entry in self.segments_manifest.get("dict_extents", []):
            dict_path = self.prefix_dir / extent_entry["path"]
            self.dictionary.extend(_read_dictionary(dict_path))
        self.descriptor_to_term_id = {d: i for i, d in enumerate(self.dictionary)}

        self._pairs_path_cache: Path | None = None

        # The source geometry a driver hands in (`attach_source_geometry`). `None` until then,
        # and every geometry re-derivation refuses rather than falling back to the stored
        # columns: a fallback is exactly the tautology this input exists to prevent, and one
        # that only fires when the harness forgot to wire it up would be invisible.
        self.source_geometry: SourceGeometry | None = None
        self._position_cache: dict[str, list[int]] = {}

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

    def segment_dir(self, view_id: str) -> Path:
        for seg in self.segments_manifest["segments"]:
            if seg["view"] == view_id:
                return (
                    self._partition_dir
                    / "views"
                    / view_id
                    / "segments"
                    / seg["seg_id"]
                )
        raise KeyError(f"no segment for view '{view_id}'")

    def segment(self, view_id: str) -> Segment:
        if view_id not in self._segment_cache:
            seg_dir = self.segment_dir(view_id)
            perm_path = seg_dir.parent.parent / "permutation.bin"
            self._segment_cache[view_id] = _read_segment(seg_dir, perm_path)
        return self._segment_cache[view_id]

    def attach_source_geometry(self, source: SourceGeometry) -> None:
        """Hand the oracle the points file this bundle was built from (see [`SourceGeometry`]).

        A method rather than a constructor argument because `conformance.md` §1's layering rule is
        that the definitional modules never *find* an input; a driver supplies it. Attaching a
        second, different source after codes have been derived would silently mix two geometries,
        so the derived caches are dropped here.
        """
        self.source_geometry = source
        self._morton_cache.clear()
        self._position_cache.clear()

    def _require_source(self) -> SourceGeometry:
        if self.source_geometry is None:
            raise ValueError(
                "this Bundle has no source geometry attached: `columns.arrow` stores a residual, "
                "not coordinates, so a geometry re-derivation would only be reading the build's "
                "own answer back. Call `attach_source_geometry(read_source_geometry(points, "
                "bundle.extent))` from the driver."
            )
        return self.source_geometry

    def row_source_ids(self, view_id: str) -> list[int]:
        """Every row's **source-corpus** id — the join the source geometry is keyed by.

        Three hops, none of which reads a geometry column: row → `entity_id` (`permutation.bin`,
        the only key-independent bridge between the two spaces), → `external_id`
        (`entities/external-ids-<k>.arrow`, 8 bytes little-endian of the source id, which is the
        build's stated convention), → source id. That makes every geometry differential depend on
        the external-ID sidecar, which contracts §0.4 does not yet list in the Phase-1 conformance
        burden and must; a bundle built without `--mint-external-ids` cannot be checked this way
        at all, which is why the harness's fixture passes the flag.
        """
        if view_id not in self._position_cache:
            self._position_cache[view_id] = [
                int.from_bytes(self.external_id_of(entity_id), "little")
                for entity_id in self.row_entity_ids(view_id)
            ]
        return self._position_cache[view_id]

    def row_position_codes(self, view_id: str) -> list[int]:
        """Every row's full 64-bit position code, **recomputed from the source geometry**.

        The §7.2 oracle needs to know which tile a row is in, and the wire comparison needs the
        exact position. Both come from here, and neither reads `morton.u32` or `residual`: the
        oracle and the engine are supposed to be independent on precisely this path, so a build
        that emitted a wrong column and then sorted and served consistently by its own wrong
        values must fail the differential rather than agree with itself.

        Computed once per view and held, because it is a pure function of geometry and does not
        vary with the mask — unlike anything in `viewport.Selection`, which is rebuilt per mask on
        purpose.
        """
        if view_id not in self._morton_cache:
            source = self._require_source()
            codes = []
            for source_id in self.row_source_ids(view_id):
                qx, qy = source.position(source_id)
                cell_code, residual = morton_mod.split32(qx, qy)
                codes.append((cell_code << 32) | residual)
            self._morton_cache[view_id] = codes
        return self._morton_cache[view_id]

    def row_morton_codes(self, view_id: str) -> list[int]:
        """Every row's 32-bit Morton **cell** code: the high half of [`row_position_codes`].

        Same independence, and the same single derivation — a cell is a prefix of a position, so
        deriving it separately would only create a way for the two to disagree.
        """
        return [code >> 32 for code in self.row_position_codes(view_id)]

    def row_entity_ids(self, view_id: str) -> list[int]:
        """Every row's entity id as a plain Python list — the permutation-derived, key-independent
        direction (I4: permissions live in entity space, geometry in row space, and the two are
        related only by the explicit permutation).

        A list rather than the `Segment`'s numpy array because every caller tests membership of a
        Python `set` per row, and `int(numpy.uint64)` per test dominates the mask-composition pass
        that is the oracle's only real cost.
        """
        if view_id not in self._entity_list_cache:
            seg = self.segment(view_id)
            self._entity_list_cache[view_id] = [int(e) for e in seg.entity_id]
        return self._entity_list_cache[view_id]

    def verify_identity_cross_check(self, view_id: str, sample: int = 200) -> None:
        """The only test that catches a key/column disagreement (Task 12 brief, Step 2): for
        a sample of rows, `identity.forward(shard, entity_of_row[r]) == tessera_id[r]`, where
        `entity_of_row` came from the permutation (key-independent) and `tessera_id` came
        from the stored column. Requires a post-r6 bundle."""
        if self.identity_key is None:
            raise ValueError("bundle has no `identity` object in MANIFEST (pre-r6 bundle)")
        seg = self.segment(view_id)
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

    def derive_row_order(self, view_id: str) -> np.ndarray:
        """Re-derive row order from `(source-recomputed morton, forward(identity.key,
        identity.shard_id, entity_id))` ascending, with no further tiebreak (the
        priority-as-identity-prefix fold; `tessera_id` is already unique so nothing else is
        needed to break ties). Row order is therefore key-dependent, where it previously
        was not -- this reads `identity.key` and `identity.shard_id` from MANIFEST, which
        `Bundle.__init__` already parses.

        Delegates to `row_order_from_geometry`, the module-level, key-dependent
        re-derivation -- computed from the *source* geometry and the permutation-derived
        `entity_id`,
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
        seg = self.segment(view_id)
        return row_order_from_geometry(
            self.identity_key,
            self.identity_shard_id,
            seg.entity_id,
            self.row_morton_codes(view_id),
        )

    def permutation(self, view_id: str) -> Permutation:
        if view_id not in self._permutation_cache:
            path = self._partition_dir / "views" / view_id / "permutation.bin"
            self._permutation_cache[view_id] = _read_permutation(path)
        return self._permutation_cache[view_id]

    def pairs_path(self, view_id: str = "default") -> Path:
        # Phase 1 has one partition; pairs.parquet lives at partitions/default/terms/pairs.parquet
        # regardless of view (postings/pairs are entity-space, not view-scoped).
        return self._partition_dir / "terms" / "pairs.parquet"

    def _external_id_runs(self) -> list[str]:
        """`SEGMENTS-<n>.json`'s external-ID extent paths, under the name the writer actually uses.

        **One accessor because there were four call sites and every one had the name wrong.** They
        read `external_id_extents`, which is the Phase 1 *plan*'s spelling; contracts §2.4 and
        `tessera-store` have always written **`external_id_runs`**. Each site defaulted with
        `.get(key, [])`, so the mismatch produced not an error but an empty list — the sidecar
        silently absent, `_known_entity_ids` yielding nothing, the locator check raising "bundle
        names no external-id extents" about a bundle that names several, and `external_id_of`
        raising `KeyError` on every entity.

        So it refuses rather than defaulting. A bundle genuinely built without `--mint-external-ids`
        has no runs and that is legitimate — `external_id_of` on such a bundle is a caller error
        either way — but *silence* is what let a renamed key survive unnoticed, and the empty list
        is indistinguishable from the absent key that caused this.
        """
        runs = self.segments_manifest.get("external_id_runs")
        if runs is None:
            raise KeyError(
                "SEGMENTS manifest has no 'external_id_runs' key (contracts §2.4). If this "
                "bundle predates the field, it predates the reader too"
            )
        return runs

    def external_id_of(self, entity_id: int) -> bytes:
        """Invert `entities/external-ids-0.arrow` (sorted by external_id bytes) to find the
        external id for a given entity id — needed to address `/control/changes` at a specific
        entity (the tessera-build convention: 8 bytes little-endian of the source corpus id)."""
        if not hasattr(self, "_entity_to_external"):
            mapping: dict[int, bytes] = {}
            for rel in self._external_id_runs():
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
        """The locator's real path: memo §7 gives it a fixed name (`ext-locator.u32`, singular,
        no `<k>` suffix) *alongside the extents*, which the build writes under
        `partitions/<phash>/entities/`. It is derived from the first extent's own
        prefix-relative path -- exactly as `tessera-store`'s
        `ExternalIdSidecar::deferred_from_manifest` derives it -- rather than assumed, because
        the earlier `prefix_dir / "entities" / ...` guess named a path that never exists, and
        the caller's `if locator_path.exists()` guard then turned the whole check into a no-op.
        """
        extents = self._external_id_runs()
        if not extents:
            raise ValueError("bundle names no external-id extents, so it has no locator either")
        first = extents[0]
        rel = first.rsplit("/", 1)[0] + "/ext-locator.u32" if "/" in first else "ext-locator.u32"
        return self.prefix_dir / rel

    def sidecar_round_trips(self, sample: int = 50) -> None:
        """For a sample of entities: `external_id_of(e)` resolves back to `e` through the
        sorted extents, **and** that entity's `ext-locator.u32` slot names the same key. Both
        directions of a mapping stored once sorted by key and once indexed by entity; they must
        agree or `/control/changes` addresses the wrong item (memo §7).

        The locator is **required**, not probed for: a bundle that names external-id extents
        names a locator too (contracts §2.4 r6). The former `if locator_path.exists()` guard
        silently skipped the only part of this method that checked the locator at all -- and,
        paired with a locator path that never existed, made a green run mean nothing.
        """
        entities = list(getattr(self, "_entity_to_external", {}) or {})
        if not entities:
            self.external_id_of(next(iter(self._known_entity_ids())))  # populate the cache
            entities = list(self._entity_to_external)
        rng = np.random.default_rng(20260730)
        n = min(sample, len(entities))
        if n == 0:
            return
        chosen = rng.choice(entities, size=n, replace=False)

        # Read once, outside the loop: the locator (raw u32s, no header) and the concatenated
        # sorted key list the locator's ordinals index into.
        locator_path = self._ext_locator_path()
        locator = np.fromfile(locator_path, dtype="<u4")
        concatenated = self._concatenated_external_keys()

        for entity_id in chosen:
            entity_id = int(entity_id)
            ext = self.external_id_of(entity_id)
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
            if slot >= len(concatenated):
                raise ValueError(
                    f"entity {entity_id}'s locator ordinal {slot} is past the "
                    f"{len(concatenated)} concatenated external-id rows"
                )
            # The load-bearing comparison: the key the LOCATOR names against the key the sorted
            # extents name. Comparing `_entity_to_external[e]` with `external_id_of(e)`, as this
            # method used to, compares one dict against itself.
            via_locator = concatenated[slot]
            if via_locator != ext:
                raise ValueError(
                    f"sidecar round-trip failed for entity {entity_id}: the locator's ordinal "
                    f"{slot} names {via_locator!r}, the sorted extents name {ext!r}"
                )

    def _concatenated_external_keys(self) -> list[bytes]:
        """Every extent's `external_id` column, in extent order -- the row space the locator's
        ordinals index into (contracts §2.4 r6: "that entity's ordinal in the concatenated sorted
        external-ID extents")."""
        keys: list[bytes] = []
        for rel in self._external_id_runs():
            with ipc.open_file(self.prefix_dir / rel) as reader:
                table = reader.read_all()
            keys.extend(table.column("external_id").to_pylist())
        return keys

    def _known_entity_ids(self):
        for rel in self._external_id_runs():
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
    morton_codes: list[int] | np.ndarray,
) -> np.ndarray:
    """The single, module-level row-order re-derivation (finding 5, task-5 review): sort
    ascending by `(morton, forward(key, shard_id, entity_id))`, with the codes coming from
    `Bundle.row_morton_codes` -- i.e. from the source geometry and the permutation-derived
    entity id, never from the stored `morton`/`tessera_id` columns, so a build that emits a
    wrong column but sorts consistently by its own wrong values does not pass this check.

    It takes codes rather than coordinates because the source is no longer required to hold
    coordinates: a Morton-sourced corpus holds cell indices, and quantising *those* against an
    extent would be the importer's own refused mistake. The one derivation of a code lives in
    `Bundle.row_position_codes`; this function orders by it.

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
    if len(morton_codes) != n:
        raise ValueError(
            f"row_order_from_geometry: {n} entity ids but {len(morton_codes)} morton codes"
        )
    mortons = np.empty(n, dtype=np.uint64)
    tesseras = np.empty(n, dtype=np.uint64)
    for i in range(n):
        morton_code, tessera_id = identity_mod.row_sort_key(
            key, shard_id, int(entity_ids[i]), int(morton_codes[i])
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
    transient (brief), so the scan over `perm.slots` is chunked in views of 2**24 and only
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
    residual = table.column("residual").to_numpy(zero_copy_only=False).astype(np.uint32)

    morton_bytes = morton_path.read_bytes()
    morton = np.frombuffer(morton_bytes, dtype="<u4")
    row_count = len(residual)
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
        residual=residual,
        morton=morton,
        row_count=row_count,
        tessera_id=tessera_id,
    )


def _read_postings_array(path: Path) -> pa.Array:
    with ipc.open_file(path) as reader:
        table = reader.read_all()
    return table.column("posting").combine_chunks()
