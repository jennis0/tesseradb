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

**Served, since 2026-08-07.** The build gained declared-column support, so `fx_key` is declared in
[`SCHEMA_TOML`], compiled into `MANIFEST.declared_scalars`, written into `columns.arrow` and
carried in the points batch. It was planted in the points parquet long before that, against the
day the gap closed; `conformance/tests/test_mask_catalogue.py` held a strict xfail throughout,
which is what made the closure a test that flipped rather than a gap somebody had to remember.

## Reuse is decided by a stamped recipe, not by a predicate over the artefact

The bundle is built once per machine at a fixed path and reused. **What may be reused is decided
by comparing the full input set against a `FIXTURE.json` receipt written beside the bundle** — see
[`recipe`] — and never by inspecting the bundle for properties a reader happens to think of. That
distinction is the whole mechanism, and it is here because the predicate form failed twice: a
hand-maintained "does it look right" test is an allowlist that has to be extended in step with
every new build input, and the input that is *not* on it is exactly the one that goes silently
wrong. The silent direction is the dangerous one — change `SEED` or `ONE_TILE_TX` and `verify()`
still passes, because it re-derives geometry from the *bundle*, while every planted `fx_key` and
every geometric claim in the test suite is computed from the *new* corpus. Bundle and corpus
diverge with nothing failing.

The receipt is therefore written **last**, after the build subprocess returns, and deleted before
a rebuild starts: an interrupted or failed build leaves no receipt, so the next run rebuilds. And
the corpus parquet is written only on the build path — writing it unconditionally is what let the
two diverge in the first place.
"""

from __future__ import annotations

import json
import random
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from . import morton
from .bundle import Bundle
from .harness import CLI_BIN, REPO_ROOT, ensure_cli_built, read_recipe, write_recipe

# Where the corpus and its bundle live between runs. A fixed path, like `/tmp/tessera-250k`, so
# the build is paid once per machine rather than once per session — `build_catalogue_bundle`
# reuses whatever is already there if it was built from this module's current inputs.
DEFAULT_WORK_DIR = Path("/tmp/tessera-catalogue")

EXTENT = (0.0, 65536.0, 0.0, 65536.0)
EXTENT_ARG = "0,65536,0,65536"
SLICE_ID = "s0"
SEED = 20260731

POINTS_NAME = "catalogue-points.parquet"
PAIRS_NAME = "catalogue-pairs.parquet"
SCHEMA_NAME = "catalogue-schema.toml"

# The declaration that makes `fx_key` a served column (per-point-attributes §4.2). Written beside
# the points parquet on the build path and bound with `--schema`.
#
# **`u64` and not a category**, deliberately: `fx_key` is 64 random bits with no vocabulary and no
# presentation, and declaring a category would need a value set enumerating every item — the
# fixture would then be maintaining a 1:1 vocabulary to say nothing. `used_for = ["render"]` is
# what puts it in `columns.arrow` and therefore in the points batch, which is the join this exists
# for.
#
# Its content is part of [`recipe`] rather than only its filename: the receipt reduces paths to
# basenames, so a changed declaration under an unchanged name would otherwise reuse a bundle built
# against the old one — the silent divergence this module's doc warns about, arriving by a new
# route.
SCHEMA_TOML = """\
[[attribute]]
name     = "fx_key"
type     = "u64"
used_for = ["render"]
"""

# The whole map, as `(x0, y0, x1, y1)` — the request's bbox order, which is **not** the order
# `Bundle.extent` uses for the same four numbers (`(x_min, x_max, y_min, y_max)`). Writing the
# extent's order into a request produces a degenerate bbox at `x = 0` that still enumerates one
# tile column per depth, so the request succeeds, the counts agree with an oracle asking the same
# wrong question, and the differential silently compares almost nothing. Defined once, here,
# because it cost a debugging round and the failure is invisible — two copies means one copy
# without this comment.
FULL_VIEWPORT = (0.0, 0.0, 65536.0, 65536.0)

# One fixed identity key, never minted. A minted key is independent per build, and `tessera_id`
# is both the storage sort key and §7.2's selection order — so two builds under two keys draw
# different samples from the same corpus, which would make every point-set comparison across a
# rebuild vacuous. (`canary_fixture.py` records the same reasoning at greater length; it is the
# same trap.)
CATALOGUE_ID_KEY_HEX = "0f0e0d0c0b0a09080706050403020100"

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


# The layout. Sizes are quoted as coverage of the 150,000-item corpus the list sums to.
#
#   filler_head  65,500   43.67%   bulk, so `boundary` lands astride 65,536
#   boundary        100    0.07%   65,500..65,600 — straddles the first container boundary
#   single            1             the single-item mask
#   sparse           15    0.01%   the sparsest catalogue member
#   cross_lo      3,750    2.50%   below §7.2's ~5% direct-evaluation crossover
#   cross_hi     11,250    7.50%   cross_lo ∪ cross_hi = 10.00%, above it
#   one_tile        250    0.17%   geometry confined to one depth-6 tile
#   filler_tail  59,134   39.42%   spans the second container boundary, 131,072
#   high_tail    10,000    6.67%   140,000..150,000 — every member above the byte-scan's floor
#
# `high_tail` is the one block with no `MaskCase`. It exists so that a grant set can put entity ids
# **on both sides of a grant boundary while every one of them is above `SAFE_ID_FLOOR`**, which is
# what `conformance/tests/test_byte_scan.py` needs and what no other layout here can supply: the
# floor is 100,000, blocks are the grantable unit, and before this block existed `filler_tail` was
# the only block straddling the floor — so every entity id above it was admitted together or denied
# together, and the scan's "admitted *and* denied ids above the floor" precondition was
# unsatisfiable on this corpus. Splitting the tail rather than giving the byte-scan a third corpus
# keeps one designed entity-ID layout in the suite. `filler_tail` was resized to compensate, so
# `N_ITEMS`, every other block's entity range, and both container claims are unchanged.
#
# **APPEND-ONLY, and this is load-bearing rather than a style preference.** A block's position in
# this list is two things at once: `_build_blocks` hands out term IDs by index, and it lays the
# entity ranges out end to end in the same order. Inserting a block anywhere but the end therefore
# renumbers every later block's *term* and moves every later block's *entity range* — which
# silently re-points `boundary` away from 65,536, moves `filler_tail` off 131,072, and changes
# every case's `tessera_id` ordering, i.e. §7.2's served set. `verify()` catches the term
# renumbering loudly; it cannot catch "the case no longer straddles what it was designed to
# straddle" beyond the two boundary claims it checks by name. Append, and resize `filler_tail` to
# compensate.
_LAYOUT: list[tuple[str, int]] = [
    ("filler_head", 65_500),
    ("boundary", 100),
    ("single", 1),
    ("sparse", 15),
    ("cross_lo", 3_750),
    ("cross_hi", 11_250),
    ("one_tile", 250),
    ("filler_tail", 59_134),
    ("high_tail", 10_000),
]


# The corpus size, **derived** rather than declared. Two constraints fix the number the layout is
# built to: it must exceed 2¹⁶ by enough that a block can straddle 65,536 with real entities either
# side *and* a second container boundary (131,072) falls inside the corpus; and the §7.2 oracle is
# a literal row-by-row definition, so every doubling doubles the differential's run time. 150,000
# satisfies both — three Roaring containers, and a full pass costs tens of milliseconds. Derived
# because the two were separate constants that had to be edited in step, and a `_LAYOUT` that no
# longer sums to `N_ITEMS` is a corpus with a gap in it.
N_ITEMS = sum(size for _, size in _LAYOUT)


def _build_blocks() -> dict[str, Block]:
    blocks: dict[str, Block] = {}
    cursor = 0
    for term_id, (name, size) in enumerate(_LAYOUT):
        blocks[name] = Block(name=name, start=cursor, stop=cursor + size, term_id=term_id)
        cursor += size
    return blocks


BLOCKS: dict[str, Block] = _build_blocks()

CONTAINER_SIZE = 1 << 16

# The entity id above which `conformance/tests/test_byte_scan.py` can tell a leaked id from a
# legitimate small integer the harness itself emits — ports, `k`, zoom, status codes, none of which
# exceed 65,535. It lives here, not with the scan, because it is a constraint on **this layout**:
# `high_tail` is placed to satisfy it and `verify()` refuses a corpus that stops doing so. The scan
# asserts its own floor matches this one, so the two cannot drift apart.
HIGH_ID_FLOOR = 100_000


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

    Adding a member costs three edits, and each is deliberate: append to `_LAYOUT` (append-only —
    see the note there — resizing `filler_tail` to compensate), add a `_case(...)` here, and add
    the name to `test_the_catalogue_covers_the_properties_the_design_names`. The third is not
    duplication to be factored out: that test transcribes conformance design §2's list, and a
    version that imported the names from here could not notice a case being deleted. Its docstring
    argues the point at the site where somebody would be tempted.

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
        # **The crossover pair is prospective, and that is the right order to build it in.** §7.2
        # puts the direct-evaluation/candidate-list boundary at roughly 5% coverage, but Phase 1
        # has no candidate-list route at all — both cases go through direct evaluation today, so
        # the pair currently proves only that the same route computes the same definition at two
        # coverages. Building the fixture before the route is deliberate: the pair is what a
        # candidate-list implementation would be measured against on the day it lands, and a
        # catalogue that acquired its crossover cases *after* the route would be a catalogue
        # designed around the implementation it is meant to test.
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

# The RNG streams are **separate and independently seeded**, so the planted keys are a pure
# function of `SEED` alone and not of how many draws the geometry happens to take. [`fx_keys`] can
# then be recomputed by anything that needs the mapping — a test, a reuse path — without writing
# the corpus, and without the recomputation having to mirror `write_corpus`'s draw order to stay
# correct. A shared stream made "recompute the keys" and "rewrite the corpus" the same operation,
# which is how the parquet came to be rewritten on the reuse path.
_FX_SEED = SEED
_GEOMETRY_SEED = SEED ^ 0x9E3779B9


def fx_keys() -> list[int]:
    """The planted join scalars, `source_id -> fx_key`, as a pure function of `SEED`.

    Drawn from a seeded RNG and rejected on collision; see the module doc for why it must NOT be a
    function of the entity ID. This is *the* definition — `write_corpus` plants exactly this list.
    """
    return _draw_fx_keys(random.Random(_FX_SEED))


def ingest_fx_keys(n: int) -> list[int]:
    """`fx_key` values for items **ingested at runtime**, disjoint from the planted ones.

    A declared column must be present in every ingest batch (contracts §2.2): the scalar tail is
    read back positionally, so an omitted column shifts every later scalar rather than defaulting.
    Once the catalogue declares `fx_key`, a conformance test that ingests has to supply one, and
    the value is the fixture's to choose — an ingested item is not in [`fx_keys`], whose list is
    the corpus.

    **Its own seeded stream, and rejected against the planted set.** Deriving one from the external
    id would make it an encoding of an identifier the test chose, and re-using a planted key would
    make two items answer to one join value — the mapping `fx_key` exists to be. Separate streams
    are this module's existing discipline (see [`fx_keys`]), for the same reason: the values stay a
    pure function of `SEED` and not of how many draws anything else took.
    """
    planted = set(fx_keys())
    rng = random.Random(SEED ^ 0x5851_F42D_4C95_7F2D)
    seen: set[int] = set()
    keys: list[int] = []
    for _ in range(n):
        while True:
            k = rng.getrandbits(64)
            if k not in planted and k not in seen:
                break
        seen.add(k)
        keys.append(k)
    return keys


def _draw_fx_keys(rng: random.Random) -> list[int]:
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
    `fx_key`s indexed by source id.

    **Called only on the build path.** It used to run unconditionally, including when the bundle
    was about to be reused — so a changed `SEED` or `ONE_TILE_TX` rewrote the corpus under a bundle
    that was never rebuilt from it. See the module doc.
    """
    work_dir.mkdir(parents=True, exist_ok=True)
    fx = fx_keys()
    geometry = _geometry(random.Random(_GEOMETRY_SEED))

    points_path = work_dir / POINTS_NAME
    pairs_path = work_dir / PAIRS_NAME

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


def _build_argv(work_dir: Path, bundle_root: Path) -> list[str]:
    """The `tessera build` invocation, in one place so [`recipe`] records what is actually run.

    `--mint-external-ids` is passed because the overlay-heavy states address individual items over
    `/control/changes`, which takes an `external_id`. It is off by default in the product for a
    good reason (contracts §2.4 forbids manufacturing one for an item whose caller supplied none),
    and passing it here is a statement that *this fixture's* items do have caller-supplied ids —
    the source corpus is synthesised by this module, so they do.
    """
    return [
        str(CLI_BIN),
        "build",
        "--points",
        str(work_dir / POINTS_NAME),
        "--pairs",
        str(work_dir / PAIRS_NAME),
        "--schema",
        str(work_dir / SCHEMA_NAME),
        "--extent",
        EXTENT_ARG,
        "--slice",
        SLICE_ID,
        "--out",
        str(bundle_root),
        "--mint-external-ids",
        "--id-key",
        CATALOGUE_ID_KEY_HEX,
    ]


def recipe(work_dir: Path, bundle_root: Path) -> dict:
    """**Every input the built bundle is a function of.** Stamped beside the bundle; a mismatch is
    a rebuild.

    The list is the point, so it is written out rather than computed: the corpus is a function of
    `_LAYOUT` (which fixes both the term IDs and the entity ranges), `SEED` (geometry and the
    planted `fx_key`s), the `ONE_TILE_*` constants, and the CLI arguments — of which `--id-key` is
    the one that decides `tessera_id`, and therefore §7.2's entire served order. Anything that
    lands here later must be added; a recipe that omits an input is a reuse test that pins the
    suite to the older fixture, which is the failure this replaced.

    Absolute paths are reduced to their basenames so the stamp is comparable across checkouts and
    worktrees — the path a fixture was built from is not a property of the fixture, and including
    it would force a rebuild per worktree for no reason.
    """
    argv = _build_argv(work_dir, bundle_root)[1:]  # the binary's own path is not an input
    return {
        # 3: the bundle gained a declared `fx_key` column (2026-08-07). The `schema` key below
        # would force a rebuild on its own; the version moves too, because a receipt that merely
        # *gained* a key is one an older reader would compare unequal for the right reason by
        # accident rather than by rule.
        "recipe_version": 3,
        "layout": [list(entry) for entry in _LAYOUT],
        "n_items": N_ITEMS,
        "seed": SEED,
        "fx_seed": _FX_SEED,
        "geometry_seed": _GEOMETRY_SEED,
        "extent": EXTENT_ARG,
        "slice": SLICE_ID,
        "one_tile": [ONE_TILE_DEPTH, ONE_TILE_TX, ONE_TILE_TY],
        "id_key": CATALOGUE_ID_KEY_HEX,
        # The declaration's *content*, not just its filename. `build_argv` below reduces paths to
        # basenames, so an edited `SCHEMA_TOML` under an unchanged name would leave the receipt
        # identical and reuse a bundle whose columns no longer match the declaration.
        "schema": SCHEMA_TOML,
        "build_argv": [Path(a).name if a.startswith("/") else a for a in argv],
    }


def catalogue_points_path(work_dir: Path | None = None) -> Path:
    """The points Parquet [`build_catalogue_bundle`] built from — the oracle's source geometry.

    Exposed because the oracle now recomputes geometry from the build's *input* rather than from
    `columns.arrow`, and the catalogue is the one corpus whose input this module owns.
    """
    return (DEFAULT_WORK_DIR if work_dir is None else work_dir) / POINTS_NAME


def build_catalogue_bundle(work_dir: Path | None = None) -> tuple[Path, list[int]]:
    """Synthesise the corpus and build it; return `(bundle_root, fx_keys)`.

    Reused rather than rebuilt only when the receipt beside the bundle matches [`recipe`] exactly.
    See the module doc for why the reuse test is a stamped recipe and not a predicate over the
    artefact.
    """
    work_dir = DEFAULT_WORK_DIR if work_dir is None else work_dir
    work_dir.mkdir(parents=True, exist_ok=True)
    bundle_root = work_dir / "bundle-catalogue"
    wanted = recipe(work_dir, bundle_root)

    if _is_usable_bundle(bundle_root, wanted):
        return bundle_root, fx_keys()

    ensure_cli_built()
    # The receipt goes first and the bundle second, so that a build interrupted anywhere in
    # between leaves a state the next run rebuilds rather than reuses.
    write_recipe(bundle_root, None)
    if bundle_root.exists():
        shutil.rmtree(bundle_root)
    write_corpus(work_dir)
    # Written on the build path only, beside the corpus and for the same reason: writing it
    # unconditionally is what let bundle and corpus diverge before.
    (work_dir / SCHEMA_NAME).write_text(SCHEMA_TOML)
    subprocess.run(_build_argv(work_dir, bundle_root), cwd=REPO_ROOT, check=True)
    write_recipe(bundle_root, wanted)
    return bundle_root, fx_keys()


def _is_usable_bundle(bundle_root: Path, wanted: dict) -> bool:
    """The receipt matches, and there is a readable post-r6 bundle under it.

    The receipt is the test; the structural check below is a cheap second gate against a bundle
    that was damaged *after* its receipt was written (a truncated `/tmp`, a half-deleted tree) —
    a case the receipt cannot see. Both are tolerant of anything unreadable: what cannot be
    confirmed is rebuilt, because being wrong in that direction costs a build and being wrong in
    the other hands every test a fixture nobody asked for.
    """
    if read_recipe(bundle_root) != wanted:
        return False
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
    4. `container_boundary` really spans more than one Roaring container — entities either side of
       a multiple of 65,536, which is the only reason the case exists;
    5. `full_100pct` spans every container the corpus reaches;
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

    # Both container claims are derived from `N_ITEMS` rather than written out, so that growing the
    # corpus is one edit and not one edit plus two constants nobody remembers are here. The claims
    # themselves are unchanged: `boundary` must span more than one container (any boundary, not
    # container 0/1 in particular), and full coverage must span every container the corpus reaches.
    boundary = report.blocks.get("boundary", set())
    boundary_containers = {e >> 16 for e in boundary}
    if len(boundary_containers) < 2:
        report.failures.append(
            f"container_boundary touches containers {sorted(boundary_containers)} — fewer than "
            "two, so the case exercises no container arithmetic at all"
        )
    everything: set[int] = set()
    for postings in report.blocks.values():
        everything |= postings
    expected_containers = set(range(((N_ITEMS - 1) >> 16) + 1)) if N_ITEMS else set()
    if {e >> 16 for e in everything} != expected_containers:
        report.failures.append(
            f"full coverage touches containers {sorted({e >> 16 for e in everything})}, not "
            f"{sorted(expected_containers)} — every container the corpus reaches"
        )

    # `high_tail`'s reason for existing, checked rather than described. The byte-scan needs a grant
    # set whose admitted *and* denied entity ids both reach above `HIGH_ID_FLOOR`; that holds only
    # while `high_tail` sits entirely above the floor and some other block still reaches above it
    # too. Resizing `filler_tail` breaks the second half silently — the block boundaries move, no
    # posting check notices, and the byte-scan's precondition assertion fires far from the cause.
    high_tail = report.blocks.get("high_tail", set())
    if not high_tail:
        # Checked rather than skipped. `blocks.get(..., set())` followed by `if high_tail` would
        # make deleting or renaming the block a silent pass — and `outside_above` below would still
        # be satisfied by `filler_tail`, so `verify()` would report green on a corpus that cannot
        # supply the byte-scan's grant set at all.
        report.failures.append(
            "high_tail is absent from the corpus, so no grant set can put entity ids above "
            f"{HIGH_ID_FLOOR} on both sides of it — see the layout note"
        )
    if high_tail and min(high_tail) < HIGH_ID_FLOOR:
        report.failures.append(
            f"high_tail starts at {min(high_tail)}, below the byte-scan floor {HIGH_ID_FLOOR} — "
            "its members are no longer all separable from the small integers the harness emits"
        )
    outside_above = {e for e in everything - high_tail if e >= HIGH_ID_FLOOR}
    if not outside_above:
        report.failures.append(
            f"no block outside high_tail reaches above {HIGH_ID_FLOOR}, so a grant of high_tail "
            "leaves nothing denied above the floor and the byte-scan can only check one direction"
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
    codes = bundle.row_morton_codes(SLICE_ID)
    shift = 32 - 2 * depth
    out: set[int] = set()
    for row in range(seg.row_count):
        if int(seg.entity_id[row]) in entities:
            out.add(codes[row] >> shift)
    return out
