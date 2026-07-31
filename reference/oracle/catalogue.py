"""The adversarial mask catalogue — a corpus built so that each mask *shape* is reachable.

Conformance design §2 names the catalogue: "empty; single item; ~0.01% coverage; 100%;
straddling the ~5% crossover; **container-boundary masks, achieved by deliberately sparse entity
allocation**; all-in-one-tile; watermark-straddling; overlay-heavy; post-deletion states at every
ledger stage." This module supplies the build-time half of that list. The states that need a
running write path (watermark-straddling, post-deletion at every ledger stage) are driven through
the control plane on top of this corpus by `oracle.journal.AckedJournal`, not baked into it.

**Why a purpose-built corpus rather than the 250k Phase 0 fixture.** A viewer's mask is the union
of the postings of the terms its token grants (§6.3). So the only masks a test can actually put in
front of the engine are unions of posting lists — an arbitrary entity set is *not* reachable, and
a catalogue defined as "pick 15 entities" would be a fiction the server never sees. The corpus
below is therefore designed backwards from the catalogue: every case is one term (or a small union
of terms) whose posting list **is** the intended entity set, exactly.

## How the entity IDs are pinned

Entity IDs are not the fixture's to choose — `tessera-build` assigns them, permanently (I9), in
**term-signature order**: items are sorted by their sorted term-ID list, ties broken by source ID,
and each item's entity ID is its position in that order (§11.1; `tessera_build::signature_sort_key`).
Term IDs themselves are interned in first-appearance order over the source-ID-sorted points.

The corpus exploits both rules rather than working around them:

* every item carries **exactly one** term, so its signature is a one-element list;
* the terms are laid out in **contiguous blocks of source ID**, block `g` carrying term `g`.

First appearance then interns term `g` as term ID `g`; the signature sort orders the blocks by `g`
and, within a block, by source ID; so the assignment collapses to the identity, `entity_id ==
source_id`, and each block is a **contiguous entity-ID range whose bounds the fixture chose**.

That is what makes the container-boundary case constructible at all. Roaring's cost model is
O(containers touched), and a container is a 2¹⁶ span of entity space, so a mask that never crosses
a multiple of 65,536 exercises no container arithmetic whatsoever. `boundary` is a 100-entity block
placed astride 65,536 deliberately; `filler_tail` spans 131,072 for the same reason at 100%
coverage.

**None of that is assumed at test time.** `verify()` re-derives every block from the built bundle's
own postings and refuses if any of it came out differently — a build whose assignment rule changed
must fail here loudly rather than silently hand the suite a catalogue that no longer straddles
anything.

## `fx_key`

Conformance design §2 and decision 4: every fixture item carries a unique planted **declared
scalar**, `fx_key`, served in the points batch. It is the legitimate handle→item join — the oracle
identifies a served point without a reverse map, an extra endpoint, or an external ID on the viewer
plane, and therefore with **no I10 tension**: entity IDs never cross the trust boundary, and this
is how the oracle names an item without one.

`fx_key` is drawn from a seeded RNG and is deliberately **not a function of the entity ID**. That
is load-bearing, not tidiness: a join key derived from the entity ID would *be* an encoding of the
entity ID, so serving it would be the very disclosure I10 forbids and the byte-scanner's sweep
exists to catch. The fixture holds the mapping because it planted it; nothing recovers `fx_key`
from an entity ID or the reverse.

**Not yet served — see the report.** `tessera-build` writes `declared_scalars: []` into MANIFEST
and `scalars: Vec::new()` onto every tiler item, so a *built* bundle carries no declared-scalar
column and the engine has none to serve. The column is planted in the points parquet here so the
fixture is complete the moment the build gains support; `conformance/tests/test_mask_catalogue.py`
carries a strict xfail that will fail the day it starts working, so the gap cannot be forgotten.
"""

from __future__ import annotations

import json
import random
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from . import morton
from .bundle import Bundle
from .harness import CLI_BIN, REPO_ROOT, ensure_cli_built

# Where the corpus and its bundle live between runs. A fixed path, like `/tmp/tessera-250k`, so
# the build is paid once per machine rather than once per session — `build_catalogue_bundle`
# reuses whatever is already there if it is the right shape.
DEFAULT_WORK_DIR = Path("/tmp/tessera-catalogue")

EXTENT = (0.0, 65536.0, 0.0, 65536.0)
EXTENT_ARG = "0,65536,0,65536"
SLICE_ID = "s0"
SEED = 20260731

# One fixed identity key, never minted. A minted key is independent per build, and `tessera_id`
# is both the storage sort key and §7.2's selection order — so two builds under two keys draw
# different samples from the same corpus, which would make every point-set comparison across a
# rebuild vacuous. (`canary_fixture.py` records the same reasoning at greater length; it is the
# same trap.)
CATALOGUE_ID_KEY_HEX = "0f0e0d0c0b0a09080706050403020100"

# The corpus size. Two constraints fix it: it must exceed 2¹⁶ by enough that a block can straddle
# 65,536 with real entities either side *and* a second container boundary (131,072) falls inside
# the corpus; and the §7.2 oracle is a literal row-by-row definition, so every doubling doubles
# the differential's run time. 150,000 satisfies both — three Roaring containers, and a full pass
# costs tens of milliseconds.
N_ITEMS = 150_000

# The depth-6 tile the `one_tile` block is confined to. Depth 6 splits each axis into 64 columns
# of 1,024 grid cells; column 17 is an arbitrary interior choice, away from both the origin and
# the Morton-maximal corner the canary occupies.
ONE_TILE_DEPTH = 6
ONE_TILE_TX = 17
ONE_TILE_TY = 17


@dataclass(frozen=True)
class Block:
    """One contiguous entity-ID range, carrying exactly one term.

    `term_id` is both the source term id (so the `builtin:passthrough` descriptor is
    `str(term_id)`) and — by the interning argument in the module doc — the bundle's own term ID.
    `verify()` checks that rather than trusting it.
    """

    name: str
    start: int
    stop: int
    term_id: int

    @property
    def size(self) -> int:
        return self.stop - self.start

    @property
    def descriptor(self) -> str:
        return str(self.term_id)

    @property
    def entities(self) -> set[int]:
        return set(range(self.start, self.stop))


# The layout. Sizes are chosen against N_ITEMS = 150,000 and are quoted as coverage below.
#
#   filler_head  65,500   43.67%   bulk, so `boundary` lands astride 65,536
#   boundary        100    0.07%   65,500..65,600 — straddles the first container boundary
#   single            1             the single-item mask
#   sparse           15    0.01%   the sparsest catalogue member
#   cross_lo      3,750    2.50%   below §7.2's ~5% direct-evaluation crossover
#   cross_hi     11,250    7.50%   cross_lo ∪ cross_hi = 10.00%, above it
#   one_tile        250    0.17%   geometry confined to one depth-6 tile
#   filler_tail  69,134   46.09%   spans the second container boundary, 131,072
_LAYOUT: list[tuple[str, int]] = [
    ("filler_head", 65_500),
    ("boundary", 100),
    ("single", 1),
    ("sparse", 15),
    ("cross_lo", 3_750),
    ("cross_hi", 11_250),
    ("one_tile", 250),
    ("filler_tail", 69_134),
]


def _build_blocks() -> dict[str, Block]:
    blocks: dict[str, Block] = {}
    cursor = 0
    for term_id, (name, size) in enumerate(_LAYOUT):
        blocks[name] = Block(name=name, start=cursor, stop=cursor + size, term_id=term_id)
        cursor += size
    assert cursor == N_ITEMS, f"_LAYOUT sums to {cursor}, not N_ITEMS={N_ITEMS}"
    return blocks


BLOCKS: dict[str, Block] = _build_blocks()

CONTAINER_SIZE = 1 << 16


@dataclass(frozen=True)
class MaskCase:
    """One catalogue member: the property it attacks, the grant that reaches it, and the entity
    set that grant must produce.

    `grants` are descriptors as `POST /session/authorise` takes them. `entities` is what the
    engine's mask must come out as — checked against the bundle's own postings by `verify()`, so a
    case can never silently drift into testing a different shape from the one it is named for.
    """

    name: str
    attacks: str
    grants: tuple[str, ...]
    entities: frozenset[int]

    @property
    def coverage(self) -> float:
        return len(self.entities) / N_ITEMS

    @property
    def containers(self) -> set[int]:
        """The Roaring containers this mask touches — `entity_id >> 16`. The measured cost model
        is O(containers touched), so this is the quantity the container-boundary case exists to
        move (CLAUDE.md, "bitmap operations cost O(containers touched), not O(cardinality)")."""
        return {e >> 16 for e in self.entities}


def _case(name: str, attacks: str, block_names: list[str]) -> MaskCase:
    blocks = [BLOCKS[n] for n in block_names]
    entities: set[int] = set()
    for b in blocks:
        entities |= b.entities
    return MaskCase(
        name=name,
        attacks=attacks,
        grants=tuple(b.descriptor for b in blocks),
        entities=frozenset(entities),
    )


def catalogue() -> list[MaskCase]:
    """The catalogue, each member named for the property it attacks (brief item 1).

    `overlay_heavy` is deliberately absent: it is not a *shape* of the build-time mask but a state
    of the overlay, so it is a base case plus a journal of acked control operations
    (`oracle.journal.AckedJournal`). `overlay_heavy_base()` names the case it is built on.
    """
    return [
        MaskCase(
            name="empty",
            attacks="the zero-visibility principal — every count and every selection must be empty, "
            "and θ's anchor V_total is 0, which §7.2's closed form divides by",
            grants=(),
            entities=frozenset(),
        ),
        _case(
            "single_item",
            "the floor clause with nothing to floor: one visible entity, so |vis(T)| < k_min in "
            "the one tile it occupies and 0 everywhere else",
            ["single"],
        ),
        _case(
            "sparse_0_01pct",
            "the sparsest principal — 0.01% coverage, where §7.2 says direct evaluation is the "
            "only affordable route and a candidate list would visit thousands of nodes",
            ["sparse"],
        ),
        _case(
            "full_100pct",
            "100% coverage — the cap clause binds nearly everywhere, and the mask spans every "
            "container in the corpus",
            [name for name, _ in _LAYOUT],
        ),
        _case(
            "crossover_below",
            "2.5% coverage, just below §7.2 r18's ~5% direct-evaluation boundary",
            ["cross_lo"],
        ),
        _case(
            "crossover_above",
            "10% coverage, just above the ~5% boundary — the same geometry either side of the "
            "crossover, so a route that changes there must still compute the same definition",
            ["cross_lo", "cross_hi"],
        ),
        _case(
            "container_boundary",
            "a mask astride a 2¹⁶ entity-ID boundary: two Roaring containers for 100 entities. At "
            "10⁴ dense IDs no mask straddles anything, so this case does not occur by accident — "
            "it is the only member that exercises container arithmetic at all",
            ["boundary"],
        ),
        _case(
            "all_in_one_tile",
            "every visible item inside a single depth-6 tile — one tile carries the whole mask and "
            "every other tile in the viewport is empty",
            ["one_tile"],
        ),
    ]


def overlay_heavy_base() -> MaskCase:
    """The case `AckedJournal`-driven overlay states are layered on: large enough that a few
    hundred denies is a real perturbation and not a rounding error, small enough to address item
    by item over the control plane."""
    return _case(
        "overlay_heavy_base",
        "the base for the overlay-heavy state — deletes, suppressions and predicate changes "
        "composed on top, per I1's (token_mask \\ L) ∪ direct_eval(L)",
        ["cross_lo"],
    )


# ---------------------------------------------------------------------------------------------
# fx_key
# ---------------------------------------------------------------------------------------------

# The planted join scalars, `source_id -> fx_key`. Drawn from a seeded RNG and rejected on
# collision; see the module doc for why it must NOT be a function of the entity ID.
def _fx_keys(rng: random.Random) -> list[int]:
    seen: set[int] = set()
    keys: list[int] = []
    for _ in range(N_ITEMS):
        while True:
            k = rng.getrandbits(64)
            if k not in seen:
                break
        seen.add(k)
        keys.append(k)
    return keys


# ---------------------------------------------------------------------------------------------
# Corpus synthesis and build
# ---------------------------------------------------------------------------------------------


def _geometry(rng: random.Random) -> list[tuple[float, float]]:
    """Uniform over the extent, except `one_tile`, which is confined to one depth-6 tile.

    Uniform placement is what keeps the crossover cases meaningful. §7.2's ~5% crossover is a
    statement about a *tile's* coverage, and the catalogue can only choose a mask's coverage of
    the whole corpus; the two coincide only while geometry is independent of the mask. Scattering
    every block over the whole extent makes that so by construction — and it is also what the
    Phase 0 measurement found real masks to be (results §5: "realistic masks are essentially
    scattered under Morton order").
    """
    cell = 1 << (16 - ONE_TILE_DEPTH)
    tile_x0, tile_x1 = ONE_TILE_TX * cell, (ONE_TILE_TX + 1) * cell
    tile_y0, tile_y1 = ONE_TILE_TY * cell, (ONE_TILE_TY + 1) * cell
    one_tile = BLOCKS["one_tile"]

    out: list[tuple[float, float]] = []
    for source_id in range(N_ITEMS):
        if one_tile.start <= source_id < one_tile.stop:
            # A margin of one grid cell either side, so float32 rounding at the parquet round
            # trip cannot push a point across the tile edge.
            x = rng.uniform(tile_x0 + 1.0, tile_x1 - 1.0)
            y = rng.uniform(tile_y0 + 1.0, tile_y1 - 1.0)
        else:
            x = rng.uniform(0.0, 65536.0)
            y = rng.uniform(0.0, 65536.0)
        out.append((x, y))
    return out


def _term_of(source_id: int) -> int:
    for block in BLOCKS.values():
        if block.start <= source_id < block.stop:
            return block.term_id
    raise AssertionError(f"source id {source_id} is in no block")


def write_corpus(work_dir: Path) -> tuple[Path, Path, list[int]]:
    """Write `catalogue-points.parquet` and `catalogue-pairs.parquet`; return them and the planted
    `fx_key`s indexed by source id."""
    rng = random.Random(SEED)
    fx = _fx_keys(rng)
    geometry = _geometry(rng)

    points_path = work_dir / "catalogue-points.parquet"
    pairs_path = work_dir / "catalogue-pairs.parquet"

    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N_ITEMS), type=pa.uint64()),
                "x": pa.array([g[0] for g in geometry], type=pa.float32()),
                "y": pa.array([g[1] for g in geometry], type=pa.float32()),
                # Planted, and currently ignored by the build — see the module doc's `fx_key`
                # section. Written anyway so the fixture is whole the day the build reads it.
                "fx_key": pa.array(fx, type=pa.uint64()),
            }
        ),
        points_path,
    )
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N_ITEMS), type=pa.uint64()),
                "term_id": pa.array([_term_of(i) for i in range(N_ITEMS)], type=pa.uint32()),
            }
        ),
        pairs_path,
    )
    return points_path, pairs_path, fx


def build_catalogue_bundle(work_dir: Path | None = None) -> tuple[Path, list[int]]:
    """Synthesise the corpus and build it; return `(bundle_root, fx_keys)`.

    Reused rather than rebuilt when a bundle is already present at the expected path — a
    `CURRENT` plus a MANIFEST carrying `identity` is the same reuse test `ensure_fixture_bundle`
    applies, and for the same reason (a pre-r6 bundle is one `tessera serve` refuses to open).

    `--mint-external-ids` is passed because the overlay-heavy states address individual items over
    `/control/changes`, which takes an `external_id`. It is off by default in the product for a
    good reason (contracts §2.4 forbids manufacturing one for an item whose caller supplied none),
    and passing it here is a statement that *this fixture's* items do have caller-supplied ids —
    the source corpus is synthesised by this module, so they do.
    """
    ensure_cli_built()
    work_dir = DEFAULT_WORK_DIR if work_dir is None else work_dir
    work_dir.mkdir(parents=True, exist_ok=True)
    bundle_root = work_dir / "bundle-catalogue"
    _, _, fx = write_corpus(work_dir)
    if _is_usable_bundle(bundle_root):
        return bundle_root, fx

    subprocess.run(
        [
            str(CLI_BIN),
            "build",
            "--points",
            str(work_dir / "catalogue-points.parquet"),
            "--pairs",
            str(work_dir / "catalogue-pairs.parquet"),
            "--extent",
            EXTENT_ARG,
            "--slice",
            SLICE_ID,
            "--out",
            str(bundle_root),
            "--mint-external-ids",
            "--id-key",
            CATALOGUE_ID_KEY_HEX,
        ],
        cwd=REPO_ROOT,
        check=True,
    )
    return bundle_root, fx


def _is_usable_bundle(bundle_root: Path) -> bool:
    try:
        current = json.loads((bundle_root / "CURRENT").read_text())
        manifest = json.loads((bundle_root / current["prefix"] / "MANIFEST.json").read_text())
        return "identity" in manifest
    except (OSError, KeyError, ValueError):
        return False


# ---------------------------------------------------------------------------------------------
# Verification — the catalogue's own claims, re-derived from the built bundle
# ---------------------------------------------------------------------------------------------


@dataclass
class VerificationReport:
    """What `verify()` established, so a test can assert on it rather than re-deriving it."""

    row_count: int = 0
    blocks: dict[str, set[int]] = field(default_factory=dict)
    failures: list[str] = field(default_factory=list)


def verify(bundle: Bundle) -> VerificationReport:
    """Re-derive every claim this module makes from the bundle itself, and collect the failures.

    Deliberately returns rather than raises, so one test reports every way the catalogue has
    drifted instead of the first. What is checked:

    1. the corpus has `N_ITEMS` rows;
    2. each block's descriptor interned to the term ID the module assumed;
    3. each block's postings are **exactly** its intended contiguous entity range — this is the
       whole `entity_id == source_id` argument, checked rather than trusted;
    4. `container_boundary` really touches two containers with entities either side of 65,536;
    5. `full_100pct` really touches three;
    6. `all_in_one_tile`'s rows really share one depth-6 tile;
    7. the crossover cases really sit either side of 5%.
    """
    report = VerificationReport()
    seg = bundle.segment(SLICE_ID)
    report.row_count = seg.row_count
    if seg.row_count != N_ITEMS:
        report.failures.append(f"segment has {seg.row_count} rows, expected {N_ITEMS}")

    for block in BLOCKS.values():
        term_id = bundle.term_id_of(block.descriptor.encode("ascii"))
        if term_id is None:
            report.failures.append(f"block {block.name}: descriptor {block.descriptor!r} not interned")
            continue
        if term_id != block.term_id:
            report.failures.append(
                f"block {block.name}: descriptor {block.descriptor!r} interned as term {term_id}, "
                f"not {block.term_id} — the first-appearance interning argument no longer holds"
            )
        postings = set(int(e) for e in bundle.postings(term_id).tolist())
        report.blocks[block.name] = postings
        if postings != block.entities:
            report.failures.append(
                f"block {block.name}: postings are not the contiguous range "
                f"[{block.start}, {block.stop}) — got {len(postings)} entities in "
                f"[{min(postings, default=-1)}, {max(postings, default=-1)}]. The build's "
                f"signature-sorted assignment (§11.1) no longer collapses to the identity for "
                f"this corpus, so no catalogue member straddles what it claims to."
            )

    boundary = report.blocks.get("boundary", set())
    if {e >> 16 for e in boundary} != {0, 1}:
        report.failures.append(
            f"container_boundary touches containers {{{sorted({e >> 16 for e in boundary})}}}, "
            "not {0, 1} — the case exercises no container arithmetic"
        )
    if not (any(e < CONTAINER_SIZE for e in boundary) and any(e >= CONTAINER_SIZE for e in boundary)):
        report.failures.append("container_boundary has no entities on both sides of 65,536")

    everything: set[int] = set()
    for postings in report.blocks.values():
        everything |= postings
    if {e >> 16 for e in everything} != {0, 1, 2}:
        report.failures.append(
            f"full coverage touches containers {sorted({e >> 16 for e in everything})}, not "
            "{0, 1, 2}"
        )

    one_tile = report.blocks.get("one_tile", set())
    tiles = _tiles_of(bundle, one_tile, ONE_TILE_DEPTH)
    if len(tiles) != 1:
        report.failures.append(
            f"all_in_one_tile's entities occupy {len(tiles)} depth-{ONE_TILE_DEPTH} tiles, not 1"
        )

    lo = len(report.blocks.get("cross_lo", set())) / N_ITEMS
    hi = (len(report.blocks.get("cross_lo", set())) + len(report.blocks.get("cross_hi", set()))) / N_ITEMS
    if not lo < 0.05 < hi:
        report.failures.append(
            f"the crossover cases no longer straddle 5%: below={lo:.4%} above={hi:.4%}"
        )

    return report


def _tiles_of(bundle: Bundle, entities: set[int], depth: int) -> set[int]:
    """The depth-`depth` tiles the given entities' rows fall in, recomputed from geometry."""
    seg = bundle.segment(SLICE_ID)
    shift = 32 - 2 * depth
    out: set[int] = set()
    for row in range(seg.row_count):
        if int(seg.entity_id[row]) in entities:
            code = morton.morton_of(float(seg.x[row]), float(seg.y[row]), bundle.extent)
            out.add(code >> shift)
    return out
