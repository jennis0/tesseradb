"""Read a Mosaica bundle directly off disk (Reference Sheet R4, contracts §2.1-§2.3).

Independent of `mosaica-store`: this module re-parses `CURRENT`/`MANIFEST.json`/
`SEGMENTS-<n>.json`, `permutation.bin`, `morton.u32`, `cuts.u32`, `postings.arrow` and
`columns.arrow` from
their byte-level definitions, verifying every file digest the manifests name along the way. Tag-1
postings records are read via `pyroaring.BitMap.deserialize`, which reads the portable Roaring
format directly — this doubles as the cross-implementation portable-format check the brief calls
for.
"""

from __future__ import annotations

import hashlib
import json
import struct
import zlib
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc
from pyroaring import BitMap

from . import identity as identity_mod
from . import morton as morton_mod

#: The separator between a group's name and a view's key in a view id — `quarter:2026-Q3`
#: (`views.md` §3.2). Transcribed from the design rather than imported from the Rust, like
#: everything else here; `Bundle.view_dir` is the one place it is split on.
GROUP_SEPARATOR = ":"

PERMUTATION_MAGIC = b"MSPM"
# The two-level paged form (contracts 2.6). Version 1 was the flat array it replaced, and this
# reader refuses that on the version field alone.
PERMUTATION_VERSION = 3
PERMUTATION_ABSENT = 0xFFFF_FFFF
PERMUTATION_PAGE_SHIFT = 16
PERMUTATION_PAGE_ENTRIES = 1 << PERMUTATION_PAGE_SHIFT
PERMUTATION_PAGE_ABSENT = 0xFFFF_FFFF
PERMUTATION_HEADER_LEN = 24
PERMUTATION_PAGE_ALIGN = 4096

# Finding 6 (task-5 review): an absent MANIFEST `identity` object is, per the memo, "a
# typed reader error, not a default... it does not acquire a minted key, a zero key or a
# legacy path" (docs/evidence/memos/2026-07-30-tessera-id-construction.md §2). The fallback
# below violates that rule on purpose, as a temporary scaffold: no bundle in this checkout
# carries an `identity` object yet, because mosaica-build/mosaica-store have not been
# repointed at the mosaica_id column (only mosaica-types/identity.rs has landed as of
# Task 5/12). REMOVE THIS FALLBACK the moment Task 6/7 land build-side `identity` emission
# -- at that point every bundle this oracle reads is post-r6 and an absent `identity`
# object must raise, full stop.
PRE_R6_IDENTITY_FALLBACK_REMOVE_AT = (
    "Task 6/7: mosaica-build/mosaica-store emitting MANIFEST `identity` and the "
    "`mosaica_id` column"
)


def _sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@dataclass
class Segment:
    """One (partition, view, seg_id)'s row-space geometry.

    Contracts r6 replaced the `entity_id` column in `columns.arrow` with `mosaica_id`
    (`docs/evidence/memos/2026-07-30-tessera-id-construction.md`): `mosaica_id` is now read
    directly off the row, and `entity_id` is *derived* -- either by inverting it through
    `identity.invert` (pure, no file I/O) when the bundle carries an `identity` key, or, for
    a pre-r6 bundle that still stores `entity_id` directly, read as before. Exactly one of
    `mosaica_id`/`entity_id` is the bundle's stored column; the other is always derived, and
    both are kept on `Segment` so callers (`viewport.py`'s entity-space mask membership,
    `test_byte_scan.py`'s entity-ID sweep) do not need to know which.
    """

    entity_id: np.ndarray  # uint64, row order (stored pre-r6, DERIVED post-r6)
    residual: np.ndarray  # uint32, row order (the position's low half, from columns.arrow)
    morton: np.ndarray  # uint32, row order (raw sorted codes from morton.u32)
    row_count: int
    mosaica_id: np.ndarray | None = None  # uint64, row order (stored post-r6; absent pre-r6)
    cuts: np.ndarray | None = None  # uint32, ascending (raw row starts from cuts.u32)

    def cut_starts(self) -> np.ndarray:
        """Where each occupied leaf Morton cell's rows begin, **recomputed from `morton`**.

        Not a read of `cuts.u32`: that file is a pure function of the Morton column, so the second
        reader's job is to derive it and compare, exactly as it derives the position codes rather
        than reading them back (`Bundle.row_position_codes`). A build that wrote the index from
        something other than the column it indexes is what this catches, and reading the index
        would catch nothing.
        """
        if self.row_count == 0:
            return np.empty(0, dtype=np.uint32)
        changes = np.flatnonzero(self.morton[1:] != self.morton[:-1]) + 1
        return np.concatenate(([0], changes)).astype(np.uint32)

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

    #: The unique attribute the file names its items by, which keys `qx` and `qy`.
    field: str
    qx: dict[int, int]
    qy: dict[int, int]

    def position(self, value: int) -> tuple[int, int]:
        if value not in self.qx:
            raise KeyError(
                f"source geometry has no row whose {self.field} is {value}: the points file "
                "handed to the oracle is not the one this bundle was built from"
            )
        return self.qx[value], self.qy[value]


def _row_groups_worth_reading(reader, column: str, limit: int | None) -> list[int]:
    """Row groups that may hold a row whose `column` is below `limit`, by their own statistics.

    Mirrors the importer's own row-group filter (`mosaica_build::input`), and for the same
    reason: the fixture corpus is 10⁹ rows and the fixture bundle a 250,000-row prefix, so a
    reader that visits every group to find a prefix is not slow, it is unusable. Returns **all**
    groups when there is no limit or no usable statistic — the filter may never drop a group it
    cannot prove is excluded.
    """
    metadata = reader.metadata
    if limit is None:
        return list(range(metadata.num_row_groups))
    column = reader.schema_arrow.names.index(column)
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
    *,
    field: str,
    column: str | None = None,
) -> SourceGeometry:
    """Read a points Parquet into [`SourceGeometry`], mirroring the importer's three schemas.

    Independently derived from `contracts` §2.5 and the importer's documented branches, not from
    the Rust — which is the whole point of an oracle. The branches, checked in this order:

    1. `x` + `y` — coordinates quantised against `extent` by `fixed32`, the one place
       quantisation happens.
    2. `morton` + `residual` — already in this form; the two words are reassembled rather than
       converted. Full 32 bits per axis.
    3. `morton` — 16 bits per axis, widened with a zero residual, because that is genuinely all
       the file says about the point.

    Each row is keyed by its value of `field`, the unique attribute its rows name items by, read from
    `column` (the attribute's `field` in the declaration; its name where that is not set).

    Both Morton branches require the identity extent `[0, 65536)`, where `cell(v) = v`; under any
    other extent the cell indices would be re-quantised as though they were coordinates in that
    extent's units. The importer errors there and so does this.

    `limit` mirrors `mosaica build --limit`: keep source rows whose value of `field` is below it.
    **Passing it is not an optimisation** — see [`_row_groups_worth_reading`].

    The per-row arithmetic is vectorised in numpy rather than written as the loop the rest of this
    oracle prefers. That is a deliberate exception to "definitions, not algorithms": the quantities
    are the same quantities, and a Python loop over even the 250,000-row prefix — let alone the
    groups a coarser statistic fails to exclude — costs minutes per test session.

    **What is shared with `morton.py` and what is not.** The interleave is shared: `_compact64`
    below is the array form of the same bit gather, and the scalar `split32` is `morton`'s. The
    **quantiser is not** — `_fixed32_vec` is a second implementation of `morton.fixed32`'s
    clamp-and-floor, and `morton.fixed32` is not called anywhere on this path. So a reader should
    not take the definition to be the thing running here; what runs is a copy of it, and what
    disciplines the copy is stated at `_fixed32_vec` itself.
    """
    import pyarrow.parquet as pq  # local: keeps the module's import surface to what it always uses

    column = field if column is None else column
    reader = pq.ParquetFile(path)
    names = set(reader.schema_arrow.names)
    if column not in names:
        raise ValueError(f"{path}: points file has no `{column}` column")

    x_min, x_max, y_min, y_max = extent
    if "x" in names and "y" in names:
        columns = [column, "x", "y"]
        morton_branch = False
    elif "morton" in names:
        if extent != (0.0, 65536.0, 0.0, 65536.0):
            raise ValueError(
                f"{path}: a Morton points file is only meaningful under the identity extent "
                f"[0, 65536), where cell(v) = v; this bundle declares {extent}"
            )
        columns = [column, "morton"] + (["residual"] if "residual" in names else [])
        morton_branch = True
    else:
        raise ValueError(f"{path}: points file has neither `x`/`y` nor `morton`")

    qx: dict[int, int] = {}
    qy: dict[int, int] = {}
    for group in _row_groups_worth_reading(reader, column, limit):
        table = reader.read_row_group(group, columns=columns)
        values = table.column(column).to_numpy(zero_copy_only=False).astype(np.uint64)
        if limit is not None:
            keep = values < np.uint64(limit)
            if not keep.any():
                continue
        else:
            keep = slice(None)
        values = values[keep]

        if morton_branch:
            hi = table.column("morton").to_numpy(zero_copy_only=False).astype(np.uint64)[keep]
            lo = (
                table.column("residual").to_numpy(zero_copy_only=False).astype(np.uint64)[keep]
                if "residual" in columns
                else np.zeros(len(values), dtype=np.uint64)
            )
            codes = (hi << np.uint64(32)) | lo
            axis_x = _compact64(codes)
            axis_y = _compact64(codes >> np.uint64(1))
        else:
            xs = table.column("x").to_numpy(zero_copy_only=False).astype(np.float64)[keep]
            ys = table.column("y").to_numpy(zero_copy_only=False).astype(np.float64)[keep]
            axis_x = _fixed32_vec(xs, x_min, x_max)
            axis_y = _fixed32_vec(ys, y_min, y_max)

        qx.update(zip(values.tolist(), axis_x.tolist()))
        qy.update(zip(values.tolist(), axis_y.tolist()))

    return SourceGeometry(field=field, qx=qx, qy=qy)


def _fixed32_vec(v: np.ndarray, vmin: float, vmax: float) -> np.ndarray:
    """[`morton.fixed32`] over an array — a second implementation of it, not a call to it.

    What pins it is `test_differential`'s byte-for-byte position check, and that check compares
    what this produces against **what the engine stored**, not against `morton.fixed32`. So a
    divergence in the clamp or the rounding surfaces as an oracle-vs-engine disagreement rather
    than as a silent pass — which is a real pin, but a pin against the implementation and not
    against the definition. The two are known to differ on NaN alone today: the scalar raises,
    this produces an undefined `uint32`.
    """
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
    """`permutation.bin`: entity id -> row id (or absent), for one view's single segment.

    The **two-level paged** form (contracts 2.6): a directory over pages of 2**16 consecutive
    entity ids, an absent page meaning every entity in it has no row. A dense view has every
    page present and is the flat array of earlier revisions; a sparse one -- a group's view
    holding a fraction of entity space -- stores only the pages it lands in.

    This is the second reader of the format, and it is deliberately not a port of the engine's:
    it decodes the directory itself and refuses a non-canonical one, so a producer that wrote
    the pages in some other order would fail here rather than round-trip through one reader's
    assumptions.
    """

    bound: int
    directory: np.ndarray  # uint32, one entry per page: payload slot or PERMUTATION_PAGE_ABSENT
    pages: np.ndarray  # uint32, (present_count, 2**16), in slot order

    @classmethod
    def from_slots(cls, slots: np.ndarray | list[int]) -> "Permutation":
        """A permutation over `[0, len(slots))` from a flat entity->row array.

        For callers that hold the mapping the flat way -- tests, and anything reasoning about
        small bounds. The paging is a storage property, so building one this way is not a
        second encoding: it produces exactly the pages the file would carry.
        """
        flat = np.asarray(slots, dtype="<u4")
        bound = int(len(flat))
        page_count = -(-bound // PERMUTATION_PAGE_ENTRIES)
        padded = np.full(page_count * PERMUTATION_PAGE_ENTRIES, PERMUTATION_ABSENT, dtype="<u4")
        padded[:bound] = flat
        by_page = padded.reshape(page_count, PERMUTATION_PAGE_ENTRIES)
        present = [p for p in range(page_count) if (by_page[p] != PERMUTATION_ABSENT).any()]
        directory = np.full(page_count, PERMUTATION_PAGE_ABSENT, dtype="<u4")
        for slot, page in enumerate(present):
            directory[page] = slot
        pages = (
            by_page[present]
            if present
            else np.zeros((0, PERMUTATION_PAGE_ENTRIES), dtype="<u4")
        )
        return cls(bound=bound, directory=directory, pages=pages)

    def page_of(self, page: int) -> np.ndarray | None:
        """The 2**16 slots of `page`, or None where the page is absent."""
        if page >= len(self.directory):
            return None
        slot = int(self.directory[page])
        return None if slot == PERMUTATION_PAGE_ABSENT else self.pages[slot]

    def present_pages(self) -> list[tuple[int, np.ndarray]]:
        """Every present page as `(first entity id, slots)`, ascending."""
        out = []
        for page in range(len(self.directory)):
            slots = self.page_of(page)
            if slots is not None:
                out.append((page * PERMUTATION_PAGE_ENTRIES, slots))
        return out

    def row_of(self, entity_id: int) -> int | None:
        if entity_id >= self.bound:
            return None
        slots = self.page_of(entity_id >> PERMUTATION_PAGE_SHIFT)
        if slots is None:
            return None
        row = int(slots[entity_id & (PERMUTATION_PAGE_ENTRIES - 1)])
        return None if row == PERMUTATION_ABSENT else row


class Bundle:
    """A verified, opened bundle: `CURRENT` -> `MANIFEST.json` -> `SEGMENTS-0.json` -> segments.

    Phase 1 has exactly one partition (`default`) and, per view, exactly one segment — matching
    the build's own scope (mosaica-build's module doc).
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

        # The quantisation frame is the *view's*, not the bundle's (decision 0040): two views of
        # one bundle may quantise differently, so there is no bundle-wide extent to read. A
        # manifest whose view omits it is malformed and refuses here, as the Rust reader does.
        self.views = {view["id"]: view for view in self.manifest["views"]}
        for view_id, view in self.views.items():
            if "quantisation" not in view:
                raise ValueError(f"view '{view_id}' declares no quantisation extent")

        # `identity` (contracts r6, docs/evidence/memos/2026-07-30-tessera-id-construction.md
        # §2): the bundle's key and the §13.3 shard prefix `mosaica_id` is built
        # under. A bundle that *does* carry `identity` is read strictly, per the memo's
        # fail-closed rule: bad construction/rounds/key/shard_id all refuse, none
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
        else:
            self.identity_key = None
            self.identity_shard_id = None

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
        # Per-view source geometry, for a multi-view bundle. A view owns everything downstream of
        # the permutation (`views.md` §1), positions included, so the same entity has a different
        # position in each view and there is no one points file to attach. A bundle with one view
        # attaches one source and never touches this map.
        self._view_geometry: dict[str, SourceGeometry] = {}
        self._position_cache: dict[str, list[int]] = {}
        self._unique_cache: dict[str, dict[int, int]] = {}

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

    def extent_of(self, view_id: str) -> tuple[float, float, float, float]:
        """The frame a view's positions are quantised against, as `(x_min, x_max, y_min, y_max)`.

        Per view and never bundle-wide (decision 0040). An unknown view raises rather than
        falling back: a tile prefix decoded against another view's frame names different ground,
        and nothing downstream would notice.
        """
        try:
            q = self.views[view_id]["quantisation"]
        except KeyError:
            raise KeyError(f"the manifest declares no view '{view_id}'") from None
        return (q["x_min"], q["x_max"], q["y_min"], q["y_max"])

    @property
    def extent(self) -> tuple[float, float, float, float]:
        """The sole declared view's frame, for a caller that has no view id to hand.

        Raises where the bundle declares more than one, because there is then no answer: the
        extent belongs to the view (decision 0040), and picking the first would decode the
        second's positions against ground they do not sit on. Use [`extent_of`] there.
        """
        if len(self.views) != 1:
            raise ValueError(
                f"this bundle declares {len(self.views)} views, so it has no single extent; "
                "ask `extent_of(view_id)` for the one you mean"
            )
        return self.extent_of(next(iter(self.views)))

    def view_dir(self, view_id: str) -> Path:
        """`views/<view>/`, or `views/<group>/<key>/` — the one place a view id becomes a path.

        **Nested rather than joined**, because `:` is not a path character everywhere: the id a
        request names and the manifest carries is `group:key`, and the directory is two components
        (`views.md` §3.2; `mosaica_store::view_path`). A single joined component was what this
        oracle laid down while every bundle had one plain view, and it named a directory no
        multi-view build writes — so the failure would have been a missing file rather than a
        wrong answer, which is the safe direction and still the wrong path.
        """
        path = self._partition_dir / "views"
        for component in view_id.split(GROUP_SEPARATOR, 1):
            path = path / component
        return path

    def segment_dir(self, view_id: str) -> Path:
        for seg in self.segments_manifest["segments"]:
            if seg["view"] == view_id:
                return self.view_dir(view_id) / "segments" / seg["seg_id"]
        raise KeyError(f"no segment for view '{view_id}'")

    def segment(self, view_id: str) -> Segment:
        if view_id not in self._segment_cache:
            seg_dir = self.segment_dir(view_id)
            perm_path = seg_dir.parent.parent / "permutation.bin"
            self._segment_cache[view_id] = _read_segment(seg_dir, perm_path)
        return self._segment_cache[view_id]

    def attach_source_geometry(
        self, source: SourceGeometry, *, view_id: str | None = None
    ) -> None:
        """Hand the oracle the points file this bundle was built from (see [`SourceGeometry`]).

        A method rather than a constructor argument because `conformance.md` §1's layering rule is
        that the definitional modules never *find* an input; a driver supplies it. Attaching a
        second, different source after codes have been derived would silently mix two geometries,
        so the derived caches are dropped here.

        `view_id` names the view the file is the geometry **of**. A multi-view corpus has one
        points file per view — a view owns its positions and its frame, and the same entity sits
        somewhere different in each (`views.md` §1) — so a driver calls this once per view and the
        oracle answers each view from its own. Omitting it attaches the source for every view that
        has none of its own, which is what a single-view bundle's driver has always done.
        """
        if view_id is None:
            self.source_geometry = source
        else:
            self._view_geometry[view_id] = source
        self._morton_cache.clear()
        self._position_cache.clear()

    def _require_source(self, view_id: str | None = None) -> SourceGeometry:
        source = self._view_geometry.get(view_id) if view_id is not None else None
        if source is None:
            source = self.source_geometry
        if source is None:
            raise ValueError(
                "this Bundle has no source geometry attached"
                + (f" for view '{view_id}'" if view_id is not None else "")
                + ": `columns.arrow` stores a residual, "
                "not coordinates, so a geometry re-derivation would only be reading the build's "
                "own answer back. Call `attach_source_geometry(read_source_geometry(points, "
                "bundle.extent_of(view)), view_id=view)` from the driver."
            )
        return source

    def row_join_values(self, view_id: str) -> list[int]:
        """Every row's value of the unique field the view's source geometry is keyed by.

        Two hops, neither of which reads a geometry column: row → `entity_id` (`permutation.bin`,
        the only key-independent bridge between the two spaces), → the value the field's unique
        index holds for that entity ([`unique_entities`]).
        """
        if view_id not in self._position_cache:
            field = self._require_source(view_id).field
            value_of = {entity: value for value, entity in self.unique_entities(field).items()}
            self._position_cache[view_id] = [
                value_of[entity_id] for entity_id in self.row_entity_ids(view_id)
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
            source = self._require_source(view_id)
            codes = []
            for value in self.row_join_values(view_id):
                qx, qy = source.position(value)
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
        a sample of rows, `identity.forward(shard, entity_of_row[r]) == mosaica_id[r]`, where
        `entity_of_row` came from the permutation (key-independent) and `mosaica_id` came
        from the stored column. Requires a post-r6 bundle."""
        if self.identity_key is None:
            raise ValueError("bundle has no `identity` object in MANIFEST (pre-r6 bundle)")
        seg = self.segment(view_id)
        if seg.mosaica_id is None:
            raise ValueError("segment has no stored mosaica_id column (pre-r6 bundle)")
        rng = np.random.default_rng(20260730)
        n = min(sample, seg.row_count)
        rows = rng.choice(seg.row_count, size=n, replace=False) if seg.row_count else np.array([])
        for row in rows:
            entity_id = int(seg.entity_id[row])
            expected = int(seg.mosaica_id[row])
            got = identity_mod.forward(self.identity_key, self.identity_shard_id, entity_id)
            if got != expected:
                raise ValueError(
                    f"identity cross-check failed at row {row}: entity {entity_id} forwards to "
                    f"{got:#x}, stored mosaica_id is {expected:#x}"
                )

    def mosaica_id_of(self, entity_id: int) -> int:
        """Pure function, no file read (memo §6): `forward(identity.key, identity.shard_id,
        entity_id)`. Requires a post-r6 bundle (`identity` present in MANIFEST)."""
        if self.identity_key is None:
            raise ValueError("bundle has no `identity` object in MANIFEST (pre-r6 bundle)")
        return identity_mod.forward(self.identity_key, self.identity_shard_id, entity_id)

    def derive_row_order(self, view_id: str) -> np.ndarray:
        """Re-derive row order from `(source-recomputed morton, forward(identity.key,
        identity.shard_id, entity_id))` ascending, with no further tiebreak (the
        priority-as-identity-prefix fold; `mosaica_id` is already unique so nothing else is
        needed to break ties). Row order is therefore key-dependent, where it previously
        was not -- this reads `identity.key` and `identity.shard_id` from MANIFEST, which
        `Bundle.__init__` already parses.

        Delegates to `row_order_from_geometry`, the module-level, key-dependent
        re-derivation -- computed from the *source* geometry and the permutation-derived
        `entity_id`,
        **never from the stored `morton`/`mosaica_id` columns** (finding 5): a build that
        emitted a wrong `mosaica_id` column and sorted consistently by its own wrong
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
            path = self.view_dir(view_id) / "permutation.bin"
            self._permutation_cache[view_id] = _read_permutation(path)
        return self._permutation_cache[view_id]

    def pairs_path(self, view_id: str = "default") -> Path:
        # Phase 1 has one partition; pairs.parquet lives at partitions/default/terms/pairs.parquet
        # regardless of view (postings/pairs are entity-space, not view-scoped).
        return self._partition_dir / "terms" / "pairs.parquet"

    def unique_entities(self, attribute: str) -> dict[int, int]:
        """Every value of an integer unique attribute and the entity holding it, read from the
        field's index runs (`unique_indexes` in `SEGMENTS-<n>.json`) by [`read_key_run`].

        A value held by two entities raises: a built bundle's index holds one entity per value,
        and a second is either a defective index or a bundle written to since, whose deletions
        this reader does not apply.
        """
        if attribute not in self._unique_cache:
            declared = {d["name"]: d for d in self.manifest["declared_scalars"]}
            if attribute not in declared or not declared[attribute].get("unique"):
                raise KeyError(f"the bundle declares no unique attribute '{attribute}'")
            arrow_type = declared[attribute]["arrow_type"]
            if arrow_type.startswith("u"):
                signed = False
            elif arrow_type.startswith(("i", "timestamp")):
                signed = True
            else:
                raise ValueError(
                    f"'{attribute}' is a {arrow_type}, whose index keys are hashes; only an "
                    "integer attribute's values can be read back from its index"
                )
            (index,) = [
                i for i in self.segments_manifest["unique_indexes"] if i["attribute"] == attribute
            ]
            runs = [run["path"] for run in index["base"]] + list(index["live"])
            entities: dict[int, int] = {}
            for rel in runs:
                for key, entity in read_key_run(self.prefix_dir / rel):
                    value = key - (1 << 63) if signed else key
                    if value in entities:
                        raise ValueError(
                            f"'{attribute}' = {value} is held by entities {entities[value]} and "
                            f"{entity}"
                        )
                    entities[value] = entity
            self._unique_cache[attribute] = entities
        return self._unique_cache[attribute]

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


KEY_RUN_MAGIC = b"MSKEYRUN"
KEY_RUN_VERSION = 3
KEY_RUN_PAGE = 4096


def _page_checksum_holds(page: bytes) -> bool:
    return zlib.crc32(page[: KEY_RUN_PAGE - 4]) == struct.unpack_from("<I", page, KEY_RUN_PAGE - 4)[0]


def read_key_run(path: Path) -> list[tuple[int, int]]:
    """One key run file's `(key, entity)` entries, in file order, every checksum checked.

    The layout (`mosaica_store::key_index`): a 4096-byte header page — magic, format version,
    key width, entry count, page count, smallest and largest key, then a CRC-32 of those 64
    bytes — and one 4096-byte page per group of entries. A page holds its entry count (u16), the
    bit width of its gaps (u8), its first key, the gap from each key to the next packed least
    significant bit first, then each entry's entity (u32), and a CRC-32 of its first 4092 bytes
    in its last four. Every integer is little-endian.
    """
    data = path.read_bytes()
    if data[:8] != KEY_RUN_MAGIC:
        raise ValueError(f"{path}: not a key run")
    version, width, count, pages = struct.unpack_from("<IIQQ", data, 8)
    if version != KEY_RUN_VERSION:
        raise ValueError(f"{path}: key run format {version}, this reader reads {KEY_RUN_VERSION}")
    if zlib.crc32(data[:64]) != struct.unpack_from("<I", data, 64)[0]:
        raise ValueError(f"{path}: the header fails its checksum")
    entries: list[tuple[int, int]] = []
    for p in range(pages):
        page = data[KEY_RUN_PAGE * (1 + p) : KEY_RUN_PAGE * (2 + p)]
        if not _page_checksum_holds(page):
            raise ValueError(f"{path}: page {p} fails its checksum")
        n, bits = struct.unpack_from("<HB", page, 0)
        key = int.from_bytes(page[3 : 3 + width], "little")
        gap_end = 3 + width + ((n - 1) * bits + 7) // 8
        gaps = int.from_bytes(page[3 + width : gap_end], "little")
        entities = struct.unpack_from(f"<{n}I", page, gap_end)
        for i, entity in enumerate(entities):
            if i:
                key += (gaps >> ((i - 1) * bits)) & ((1 << bits) - 1)
            entries.append((key, entity))
    if len(entries) != count:
        raise ValueError(f"{path}: the header counts {count} entries and the pages hold {len(entries)}")
    if any(a >= b for a, b in zip(entries, entries[1:])):
        raise ValueError(f"{path}: entries are not in strictly ascending (key, entity) order")
    return entries


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
    entity id, never from the stored `morton`/`mosaica_id` columns, so a build that emits a
    wrong column but sorts consistently by its own wrong values does not pass this check.

    It takes codes rather than coordinates because the source is no longer required to hold
    coordinates: a Morton-sourced corpus holds cell indices, and quantising *those* against an
    extent would be the importer's own refused mistake. The one derivation of a code lives in
    `Bundle.row_position_codes`; this function orders by it.

    `Bundle.derive_row_order` and `test_identity.py`'s
    `test_row_order_is_morton_then_mosaica_id_ascending` both call this function rather
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
    mosaica_ids = np.empty(n, dtype=np.uint64)
    for i in range(n):
        morton_code, mosaica_id = identity_mod.row_sort_key(
            key, shard_id, int(entity_ids[i]), int(morton_codes[i])
        )
        mortons[i] = morton_code
        mosaica_ids[i] = mosaica_id
    # np.lexsort sorts by the LAST key primary -- (morton, mosaica_id) ascending means
    # mosaica_id is the secondary (fastest-varying) key, morton primary.
    return np.lexsort((mosaica_ids, mortons))


def _read_permutation(path: Path) -> Permutation:
    """Decode the paged `permutation.bin`, refusing anything non-canonical.

    The canonical encoding is what makes the file a function of the mapping: slots number
    `0..present_count` in ascending page order. A permuted directory would serve every page
    under some other page's rows -- every lookup wrong, none out of range -- so it is checked
    here rather than trusted, exactly as the engine's reader checks it.
    """
    data = path.read_bytes()
    if data[0:4] != PERMUTATION_MAGIC:
        raise ValueError(f"{path}: bad permutation magic {data[0:4]!r}")
    (version,) = struct.unpack_from("<H", data, 4)
    if version != PERMUTATION_VERSION:
        raise ValueError(f"{path}: unsupported permutation version {version}")
    (page_shift,) = struct.unpack_from("<H", data, 6)
    if page_shift != PERMUTATION_PAGE_SHIFT:
        raise ValueError(f"{path}: page shift {page_shift}, expected {PERMUTATION_PAGE_SHIFT}")
    (bound,) = struct.unpack_from("<Q", data, 8)
    (page_count,) = struct.unpack_from("<I", data, 16)
    (present_count,) = struct.unpack_from("<I", data, 20)
    if page_count != -(-bound // PERMUTATION_PAGE_ENTRIES):
        raise ValueError(f"{path}: {page_count} pages do not cover bound {bound}")
    directory = np.frombuffer(
        data, dtype="<u4", count=page_count, offset=PERMUTATION_HEADER_LEN
    )
    named = [int(s) for s in directory if int(s) != PERMUTATION_PAGE_ABSENT]
    if named != list(range(present_count)):
        raise ValueError(
            f"{path}: the directory is not canonical -- slots must ascend with page index and "
            f"number 0..{present_count}"
        )
    directory_end = PERMUTATION_HEADER_LEN + page_count * 4
    payload = -(-directory_end // PERMUTATION_PAGE_ALIGN) * PERMUTATION_PAGE_ALIGN
    if any(data[directory_end:payload]):
        raise ValueError(f"{path}: the padding before the payload is not zero")
    expected = payload + present_count * PERMUTATION_PAGE_ENTRIES * 4
    if len(data) != expected:
        raise ValueError(f"{path}: file is {len(data)} bytes, expected {expected}")
    pages = np.frombuffer(
        data, dtype="<u4", count=present_count * PERMUTATION_PAGE_ENTRIES, offset=payload
    ).reshape(present_count, PERMUTATION_PAGE_ENTRIES)
    return Permutation(bound=bound, directory=directory, pages=pages)


def _entity_of_rows(perm: Permutation, rows: np.ndarray) -> dict[int, int]:
    """row -> entity, by inverting `permutation.bin`'s entity_to_row for TOUCHED rows only.

    Contracts r6 removed the entity_id column from `columns.arrow`; the permutation is the
    only key-independent artefact relating the two spaces (§5.1, I4). Computed for the rows
    asked about rather than materialised whole: a full inversion at 10^9 needs ~17-20 GB
    transient (brief), so the scan runs a page at a time -- 2**16 entities, which is the unit
    the file already stores -- and only entries landing in `rows` are collected. An absent page
    is not scanned at all, so a sparse view costs its population rather than its bound.
    """
    wanted = np.asarray(rows, dtype=np.uint32)
    remaining = set(int(r) for r in wanted)
    result: dict[int, int] = {}
    for base, window in perm.present_pages():
        if not remaining:
            break
        matches = np.isin(window, wanted)
        if not matches.any():
            continue
        for offset in np.nonzero(matches)[0]:
            row_val = int(window[offset])
            if row_val in remaining:
                result[row_val] = base + int(offset)
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

    # `cuts.u32` (contracts §2.6) — the Morton column's run-length index, mapped by the engine and
    # walked by selection. Read raw here; `Segment.cut_starts` derives what it should hold.
    cuts_path = seg_dir / "cuts.u32"
    cuts = np.frombuffer(cuts_path.read_bytes(), dtype="<u4")

    column_names = set(table.schema.names)
    if "mosaica_id" in column_names:
        # Post-r6: columns.arrow stores mosaica_id; entity_id is DERIVED via the
        # permutation (the key-independent direction, memo §7 -- "there is no
        # mosaica_id -> entity sidecar").
        mosaica_id = table.column("mosaica_id").to_numpy(zero_copy_only=False).astype(np.uint64)
        if perm_path is None or not perm_path.exists():
            raise ValueError(
                f"{seg_dir}: columns.arrow has mosaica_id but no permutation.bin was given to "
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
        mosaica_id = None
    else:
        raise ValueError(f"{seg_dir}: columns.arrow has neither entity_id nor mosaica_id")

    return Segment(
        entity_id=entity_id,
        residual=residual,
        morton=morton,
        row_count=row_count,
        mosaica_id=mosaica_id,
        cuts=cuts,
    )


def _read_postings_array(path: Path) -> pa.Array:
    with ipc.open_file(path) as reader:
        table = reader.read_all()
    return table.column("posting").combine_chunks()
