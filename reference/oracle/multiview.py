"""The multi-view corpus — one entity space, four row spaces, one label per entity.

`views.md` §11 names what the conformance suite owes this design: "a two-view differential: the
oracle answers per view; the pinned-leaf cases". This module is the build-time half of that — the
corpus, its declaration and its stamped build — and it is deliberately a **second** fixture beside
`catalogue.py` rather than a widening of it: the catalogue's whole construction is backwards from
the mask shapes conformance §2 enumerates, and adding views to it would change every entity id in
every mask case for the sake of a question that is not about mask shape at all.

## What the corpus is designed to make assertable

The design's rule (§1) is a factoring, and the differential is a test of the factoring:

> Entity space is the invariant plane; a view owns everything downstream of the permutation and
> nothing upstream of it.

So the corpus is built so that each half of that sentence has something to disagree about.

* **Four views over one entity space.** `world`, a plain view holding every entity; and
  `quarter`, a group of three views (form A — one points file per view, §3.1), each holding a
  *different subset*. A response that ignored view membership, or that answered every view from
  one row space, disagrees with the oracle on the first tile it draws.
* **A different frame per view.** `world` quantises against the grid's own `[0, 65536)²`;
  the group quantises against `[-40, 40]²`. Decision 0040 puts the extent on the view, and two
  views sharing a bundle-wide frame is exactly the reading that used to be possible — with a
  single frame, every one of the group's positions decodes to a corner cell and no count moves.
* **A different position per entity per view**, drawn independently per view. Identity is
  entity-space and position is row-space; the two are only testably distinct where they disagree.
* **Real access labels, and masks that differ per principal.** Every entity carries exactly one
  compartment term, `cc0`…`cc5`, in contiguous blocks of source id — `catalogue.py`'s own trick,
  and for its reason: an entity's id is its position in **signature order** (§11.1), a one-term
  signature makes each block a contiguous entity range, and a mask is then a union of blocks.
  The label travels in the `access` **column of every file the entity appears in**, byte-identical,
  which is what `views.md` §4 requires and what the build refuses a disagreement in.
* **Membership decorrelated from the compartments.** A quarter's members are picked by a
  residue of the source id and a compartment is a contiguous range of it, so every compartment
  spans every quarter and every quarter spans every compartment. Without that, `served(view)`
  and `mask ∩ members(view)` would agree for the wrong reason and the differential would prove
  nothing. [`verify`] asserts it rather than leaving it stated here.
* **One group-scoped attribute, `sentiment`** (§5): an entity-space column per view of the group,
  each with its own presence bitmap, and about a fifth of each view's entities carrying no value
  in it. That is the pinned-leaf row's operand, and the presence holes are what make "an entity
  with no value in the pinned view never matches" a case rather than a sentence.
* **One entity-scoped declared scalar, `fx_key`**, rendered — the same handle→item join the
  catalogue plants and for the same reason (see its module doc's `fx_key` section): the oracle
  names a served point without a reverse map and with no I10 tension. It is drawn from a seeded
  RNG and is not a function of the entity id.
* **One group-scoped category, `mood`** (§5), over a closed `public` vocabulary: the second family
  a scope can carry, and the one whose per-view artefact is a set of postings rather than a value
  column. An entity's value differs by quarter and about a sixth of each view's entities carry
  none, so reading the wrong view's column is observable in both directions.

## What it deliberately does not carry

**No gate.** Every `visibility` here is `public`, because the gate is unbuilt (`views.md` §6) and
the visible-view set is another track's. A fixture carrying a gated view would be a corpus whose
expected answers no implementation can produce.

**No layers, no keyword or text column, and no `derived` vocabulary.** Each is covered against the
catalogue, in entity space, where views change nothing about them. A **`derived`** value set would
be the one addition that said something new here — `/v1/categories` derives a scoped category's
list from *that view's* postings, so two views of one group offer two lists — and it is not here:
it needs a value-list verb on the harness and a second derivation in the oracle, and the channel
itself (C11) is already covered in entity space. Named rather than left out in silence.

**No second partition.** Phase 1 has one, and I13b's row says so.

## Sentiment values are exact in `f32`, on purpose

Every planted value is a multiple of 1/1024 in `(-1, 1)`, so the parquet round trip through `f32`
is lossless and the oracle's Python floats compare exactly against the engine's. A uniform draw
would put the differential's verdict on the last bit of a rounding, which is a test of `numpy`
rather than of the filter.

## Reuse is decided by a stamped recipe

Identical in construction to `catalogue.py`'s, and identical in reasoning — see its module doc.
The receipt records every input the bundle is a function of, is written last, and is deleted
before a rebuild starts.
"""

from __future__ import annotations

import hashlib
import random
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from .bundle import Bundle
from .harness import (
    CLI_BIN,
    REPO_ROOT,
    build_env,
    ensure_cli_built,
    read_recipe,
    write_deployment,
    write_recipe,
)

#: Where the corpus and its bundle live between runs — a fixed path, so the build is paid once per
#: machine rather than once per session.
DEFAULT_WORK_DIR = Path("/tmp/tessera-multiview")

#: 6,144 entities in six compartments of 1,024. Small on purpose: every assertion here is a whole
#: pass over a view's rows in Python, made once per (principal, view) pair, and the corpus's job is
#: to have four row spaces rather than to be large. The catalogue is where size lives.
N_ITEMS = 6144
COMPARTMENTS = 6
BLOCK = N_ITEMS // COMPARTMENTS

#: The plain view. Its frame is the grid's own, so a tile prefix is a cell index and an expected
#: answer can be stated in cells.
WORLD_VIEW = "world"
WORLD_EXTENT = (0.0, 65536.0, 0.0, 65536.0)

#: The group and its views. Three, so that a pin can name a view that is neither the first nor
#: the last — an off-by-one in pin resolution is invisible against two.
GROUP = "quarter"
QUARTER_KEYS = ("2026-Q1", "2026-Q2", "2026-Q3")
GROUP_EXTENT = (-40.0, 40.0, -40.0, 40.0)

#: Every view's id, in manifest order.
VIEW_IDS = (WORLD_VIEW, *(f"{GROUP}:{key}" for key in QUARTER_KEYS))

SEED = 20260831
_FX_SEED = SEED + 1
_GEOMETRY_SEED = SEED + 2

#: The identity key, fixed rather than minted. `tessera_id` is both the storage sort key and
#: §7.2's selection order, so a minted key would draw a different sample from the same corpus at
#: every rebuild — `catalogue.py` records the same trap at greater length.
MULTIVIEW_ID_KEY_HEX = "1f1e1d1c1b1a19181716151413121110"

POINTS_NAME = "multiview-world.parquet"
SCHEMA_NAME = "multiview-config.toml"
DEPLOYMENT_NAME = "multiview-tessera.toml"


def quarter_points_name(key: str) -> str:
    return f"multiview-{key}.parquet"


# ---------------------------------------------------------------------------------------------
# The corpus, as pure functions of a source id
# ---------------------------------------------------------------------------------------------


def compartment_of(source_id: int) -> str:
    """`cc0`…`cc5` — the access term this item's label is, one per item.

    Contiguous blocks of source id, so that signature order makes each compartment a contiguous
    **entity** range (see the module doc). The value is the descriptor a session authorises
    against and the string the `access` column carries in every file this item appears in.
    """
    return f"cc{source_id // BLOCK}"


#: A quarter's membership, as a residue of the source id. Decorrelated from the compartments by
#: construction — a compartment is a contiguous range of 1,024 and each of these is a residue —
#: and checked by [`verify`] rather than trusted.
_MEMBERSHIP = {
    "2026-Q1": lambda i: i % 3 != 0,
    "2026-Q2": lambda i: i % 5 != 0,
    "2026-Q3": lambda i: i % 2 == 0,
}


def in_quarter(source_id: int, key: str) -> bool:
    return _MEMBERSHIP[key](source_id)


def quarter_members(key: str) -> list[int]:
    """The source ids in one quarter, ascending — the view's population."""
    return [i for i in range(N_ITEMS) if in_quarter(i, key)]


def _hash(*parts: object) -> int:
    return int(hashlib.sha256(":".join(str(p) for p in parts).encode()).hexdigest()[:16], 16)


def sentiment_of(source_id: int, key: str) -> float | None:
    """The group-scoped value for one `(entity, view)`, or `None` where the view holds none.

    `None` twice over, and the two are different facts the differential must keep apart: an item
    that is not in the quarter at all has no row there, and an item that is has a value or an
    absence in the presence bitmap (decision 0064). This function answers the second only where
    the first says the item is present.

    Every value is a multiple of 1/1024, so `f32` holds it exactly (module doc).
    """
    if not in_quarter(source_id, key):
        return None
    h = _hash("sentiment", key, source_id)
    if h % 5 == 0:
        return None
    return ((h >> 8) % 2001 - 1000) / 1024.0


#: `mood`'s value set, and the codes the declaration pins them at. Closed and `public`, so the
#: value list is authored and the differential's subject is the per-view *column* rather than the
#: per-view list (module doc).
MOOD_CODES = {"calm": 11, "tense": 22, "wild": 33}


def mood_of(source_id: int, key: str) -> str | None:
    """The group-scoped **category** value for one `(entity, view)`, or `None` where there is none.

    The same two `None`s [`sentiment_of`] keeps apart: an item outside the quarter has no row
    there, and an item inside it either carries a value or carries the presence bitmap's absence
    (decision 0064). Decorrelated from `sentiment_of` by a different hash input, so a reader that
    confused the two families' columns is caught rather than accidentally right.
    """
    if not in_quarter(source_id, key):
        return None
    h = _hash("mood", key, source_id)
    if h % 6 == 0:
        return None
    return sorted(MOOD_CODES)[(h >> 8) % len(MOOD_CODES)]


def fx_keys() -> list[int]:
    """One unique 64-bit join key per item, indexed by source id.

    Drawn from a seeded RNG and deliberately **not** a function of the entity id: a key derived
    from one would be an encoding of it, and serving it would be the disclosure I10 forbids.
    """
    rng = random.Random(_FX_SEED)
    return rng.sample(range(1, 1 << 63), N_ITEMS)


def _geometry(view_id: str) -> list[tuple[float, float]]:
    """One view's positions, uniform over that view's own frame.

    Per view and independently drawn: a view owns its positions, and a corpus that reused one
    layout would let an engine answer every view from one row space and still agree.

    A margin of one grid cell inside each edge, so the `f32` round trip through parquet cannot
    push a point onto or across the frame's boundary — the build reports edge placements and the
    fixture would rather have none than reason about them.
    """
    x_min, x_max, y_min, y_max = (
        WORLD_EXTENT if view_id == WORLD_VIEW else GROUP_EXTENT
    )
    mx = (x_max - x_min) / 65536.0
    my = (y_max - y_min) / 65536.0
    rng = random.Random(_hash(_GEOMETRY_SEED, view_id) & 0xFFFF_FFFF)
    return [
        (rng.uniform(x_min + mx, x_max - mx), rng.uniform(y_min + my, y_max - my))
        for _ in range(N_ITEMS)
    ]


# ---------------------------------------------------------------------------------------------
# The declaration
# ---------------------------------------------------------------------------------------------


def _schema_toml() -> str:
    """The whole declaration, built from this module's own constants so each is stated once.

    Written out rather than assembled from a template file because the declaration is **part of
    the recipe** — an edited extent or an added attribute under an unchanged filename would
    otherwise reuse a bundle the corpus no longer matches (`catalogue.py`'s module doc).
    """
    sources = [f'{WORLD_VIEW:<16}= "{POINTS_NAME}"']
    for key in QUARTER_KEYS:
        sources.append(f'{_source_name(key):<16}= "{quarter_points_name(key)}"')

    quarters = "".join(
        f"""
[[view_group.view]]
key    = "{key}"
source = "{_source_name(key)}"
label  = "{key}"
"""
        for key in QUARTER_KEYS
    )

    mood_values = "".join(f"  {key} = {code}\n" for key, code in sorted(MOOD_CODES.items()))

    return f"""\
# The multi-view conformance corpus (`reference/oracle/multiview.py`). Generated — edit the
# module, not this file.

[sources]
{chr(10).join(sources)}

[defaults]
source          = "{WORLD_VIEW}"
entity_id_field = "entity_id"
# The anchor (decision 0112): within a signature group, entity ids order by the item's Morton code
# in *this* view. Explicit because the declaration carries several views and the ids are permanent
# (I9) — reordering the blocks below must not silently re-key a rebuild.
allocation_view = "{WORLD_VIEW}"

# The plain view: every entity, quantised against the grid's own frame.
[[view]]
name             = "{WORLD_VIEW}"
extent           = {{ x = [{WORLD_EXTENT[0]}, {WORLD_EXTENT[1]}], \
y = [{WORLD_EXTENT[2]}, {WORLD_EXTENT[3]}] }}
source           = "{WORLD_VIEW}"
visibility       = "public"
point_visibility = {{ field = "access", default = "public" }}

# The group, form A: one points file per view. Its frame is **not** the plain view's — decision
# 0040 puts the extent on the view, and a differential over two views sharing one frame would not
# notice a reader that took either for the other.
[[view_group]]
name             = "{GROUP}"
extent           = {{ x = [{GROUP_EXTENT[0]}, {GROUP_EXTENT[1]}], \
y = [{GROUP_EXTENT[2]}, {GROUP_EXTENT[3]}] }}
visibility       = "public"
point_visibility = {{ field = "access", default = "public" }}
metadata         = {{ label = "text" }}
{quarters}
# The handle→item join, entity-scoped and rendered (per-point-attributes §4.2).
[[attribute]]
name   = "fx_key"
type   = "u64"
render = true
source = "{WORLD_VIEW}"

# The group-scoped attribute (`views.md` §5): one entity-space column per view of the group, read
# from each view's own points file. No `source` — an attribute may declare one, and then its
# `fields.view` says which view each row's value is for; reading each view's own file is the
# shape that needs no discriminator.
[[attribute]]
name  = "sentiment"
type  = "f32"
scope = {{ group = "{GROUP}" }}
index = true

# The scoped **category** beside it: the same scope, a different family. Its per-view artefact is
# a set of postings over the codes below rather than a value column, which is the whole reason it
# is here — a reader that served the numeric family correctly and this one from another view's
# postings passes every case above.
[[vocabulary]]
name       = "mood"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
{mood_values}
[[attribute]]
name       = "mood"
type       = "category"
vocabulary = "mood"
scope      = {{ group = "{GROUP}" }}
index      = true
"""


def _source_name(key: str) -> str:
    return "q_" + key.replace("-", "_").lower()


SCHEMA_TOML = _schema_toml()


# ---------------------------------------------------------------------------------------------
# Writing the corpus, and building it
# ---------------------------------------------------------------------------------------------


def write_corpus(work_dir: Path) -> None:
    """Write the four points files.

    **Called only on the build path**, like the catalogue's: writing it unconditionally is what
    once let a corpus and the bundle built from it diverge with nothing failing.
    """
    work_dir.mkdir(parents=True, exist_ok=True)
    fx = fx_keys()
    access = [compartment_of(i) for i in range(N_ITEMS)]

    world = _geometry(WORLD_VIEW)
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N_ITEMS), type=pa.uint64()),
                "x": pa.array([p[0] for p in world], type=pa.float32()),
                "y": pa.array([p[1] for p in world], type=pa.float32()),
                # The label, as a descriptor the build interns and a session authorises against.
                "access": pa.array(access, type=pa.string()),
                "fx_key": pa.array(fx, type=pa.uint64()),
            }
        ),
        work_dir / POINTS_NAME,
    )

    for key in QUARTER_KEYS:
        members = quarter_members(key)
        positions = _geometry(f"{GROUP}:{key}")
        pq.write_table(
            pa.table(
                {
                    "entity_id": pa.array(members, type=pa.uint64()),
                    "x": pa.array([positions[i][0] for i in members], type=pa.float32()),
                    "y": pa.array([positions[i][1] for i in members], type=pa.float32()),
                    # **Byte-identical to `world`'s**, which `views.md` §4 requires and the build
                    # refuses a disagreement in. Carried rather than omitted because a view of a
                    # group declares its own `point_visibility` and reads it from its own file.
                    "access": pa.array([access[i] for i in members], type=pa.string()),
                    # The group-scoped value, one column per view. A null is the presence
                    # bitmap's absence, not a zero.
                    "sentiment": pa.array(
                        [sentiment_of(i, key) for i in members], type=pa.float32()
                    ),
                    # The scoped category's own column, keys rather than codes: the build mints
                    # against the declaration's pinning, which is what `MOOD_CODES` records.
                    "mood": pa.array([mood_of(i, key) for i in members], type=pa.string()),
                }
            ),
            work_dir / quarter_points_name(key),
        )


def points_path(view_id: str, work_dir: Path | None = None) -> Path:
    """The points file one view was built from — the oracle's source geometry for that view.

    One per view, because a view owns its positions: a driver attaches each in turn
    (`Bundle.attach_source_geometry(..., view_id=...)`), and a single attachment would answer
    three of the four views against the fourth one's layout.
    """
    work_dir = DEFAULT_WORK_DIR if work_dir is None else work_dir
    if view_id == WORLD_VIEW:
        return work_dir / POINTS_NAME
    group, _, key = view_id.partition(":")
    if group != GROUP or key not in QUARTER_KEYS:
        raise KeyError(f"this corpus declares no view '{view_id}'")
    return work_dir / quarter_points_name(key)


def extent_of(view_id: str) -> tuple[float, float, float, float]:
    """The frame the declaration gives a view — what a source file is read against.

    The **declaration's**, not the manifest's: a driver reading the frame back out of the bundle
    to decode the file the bundle was built from would be asking the artefact under test what its
    own inputs meant.
    """
    return WORLD_EXTENT if view_id == WORLD_VIEW else GROUP_EXTENT


def _build_argv(work_dir: Path, bundle_root: Path) -> list[str]:
    """The `tessera build` invocation, in one place so [`recipe`] records what is run.

    `--mint-external-ids` because every join the differential makes — planted value to served
    point — goes through the external-id sidecar, which is the only key-independent bridge from a
    row back to the source id the fixture planted against.
    """
    return [
        str(CLI_BIN),
        "build",
        "--deployment",
        str(work_dir / DEPLOYMENT_NAME),
        "--out",
        str(bundle_root),
        "--mint-external-ids",
    ]


def recipe(work_dir: Path, bundle_root: Path) -> dict:
    """Every input the built bundle is a function of. Stamped beside it; a mismatch is a rebuild.

    Written out rather than computed, for the reason `catalogue.py`'s says at length: a recipe
    that omits an input is a reuse test that pins the suite to the older fixture.
    """
    argv = _build_argv(work_dir, bundle_root)[1:]
    return {
        "recipe_version": 1,
        "n_items": N_ITEMS,
        "compartments": COMPARTMENTS,
        "views": list(VIEW_IDS),
        "world_extent": list(WORLD_EXTENT),
        "group_extent": list(GROUP_EXTENT),
        "membership": {key: len(quarter_members(key)) for key in QUARTER_KEYS},
        "seed": SEED,
        "fx_seed": _FX_SEED,
        "geometry_seed": _GEOMETRY_SEED,
        "id_key": MULTIVIEW_ID_KEY_HEX,
        # The declaration's *content*, since the extents, the roster and both attributes live in
        # it and an edit under an unchanged filename would otherwise reuse a stale bundle.
        "schema": SCHEMA_TOML,
        "build_argv": [Path(a).name if a.startswith("/") else a for a in argv],
    }


def build_multiview_bundle(work_dir: Path | None = None) -> Path:
    """Synthesise the corpus and build it; return the bundle root.

    Reused rather than rebuilt only when the receipt beside the bundle matches [`recipe`] exactly.
    """
    work_dir = DEFAULT_WORK_DIR if work_dir is None else work_dir
    work_dir.mkdir(parents=True, exist_ok=True)
    bundle_root = work_dir / "bundle-multiview"
    wanted = recipe(work_dir, bundle_root)

    if _is_usable_bundle(bundle_root, wanted):
        return bundle_root

    ensure_cli_built()
    # Receipt first (removed), bundle second: a build interrupted anywhere between leaves a state
    # the next run rebuilds rather than reuses.
    write_recipe(bundle_root, None)
    if bundle_root.exists():
        shutil.rmtree(bundle_root)
    write_corpus(work_dir)
    (work_dir / SCHEMA_NAME).write_text(SCHEMA_TOML)
    write_deployment(
        work_dir / DEPLOYMENT_NAME, bundle=bundle_root, schema=work_dir / SCHEMA_NAME
    )
    subprocess.run(
        _build_argv(work_dir, bundle_root),
        cwd=REPO_ROOT,
        env=build_env(MULTIVIEW_ID_KEY_HEX),
        check=True,
    )
    write_recipe(bundle_root, wanted)
    return bundle_root


def _is_usable_bundle(bundle_root: Path, wanted: dict) -> bool:
    """The receipt matches, the bundle under it reads, and nothing has been published into it
    since the build — `catalogue.py`'s three gates, for its reasons."""
    import json  # noqa: PLC0415 — only the reuse test reads these files

    if read_recipe(bundle_root) != wanted:
        return False
    try:
        current = json.loads((bundle_root / "CURRENT").read_text())
        prefix_dir = bundle_root / current["prefix"]
        manifest = json.loads((prefix_dir / "MANIFEST.json").read_text())
        if manifest.get("identity") is None:
            return False
        for partition in (prefix_dir / "partitions").iterdir():
            if len(list(partition.glob("SEGMENTS-*.json"))) > 1:
                return False
        return True
    except (OSError, KeyError, ValueError):
        return False


# ---------------------------------------------------------------------------------------------
# The fixture's own preconditions, checked against the built bundle
# ---------------------------------------------------------------------------------------------


@dataclass
class VerificationReport:
    """What [`verify`] re-derived, for a test to assert on and a reader to see."""

    views: dict[str, int]
    compartment_entities: dict[str, set[int]]
    decorrelation: dict[tuple[str, str], tuple[int, int]]


def verify(bundle: Bundle) -> VerificationReport:
    """Re-derive the corpus's claims **from the built bundle**, and refuse where they do not hold.

    Every claim this module's doc makes about shape is checked here rather than asserted in prose,
    because each of them is silent when it breaks: a corpus whose compartments correlated with its
    view membership would pass every differential in the suite vacuously.
    """
    entity_of = bundle.entities_by_source()
    if len(entity_of) != N_ITEMS:
        raise ValueError(f"the bundle holds {len(entity_of)} items, not {N_ITEMS}")

    views = {}
    for view_id in VIEW_IDS:
        views[view_id] = len(bundle.row_entity_ids(view_id))
    if views[WORLD_VIEW] != N_ITEMS:
        raise ValueError(f"'{WORLD_VIEW}' holds {views[WORLD_VIEW]} rows, not every entity")
    for key in QUARTER_KEYS:
        expected = len(quarter_members(key))
        got = views[f"{GROUP}:{key}"]
        if got != expected:
            raise ValueError(f"view '{GROUP}:{key}' holds {got} rows, not {expected}")

    # The compartments, as the *bundle's* postings hold them, joined back to the planted labels.
    compartments: dict[str, set[int]] = {}
    for c in range(COMPARTMENTS):
        name = f"cc{c}"
        term_id = bundle.term_id_of(name.encode())
        if term_id is None:
            raise ValueError(f"the dictionary holds no term '{name}'")
        compartments[name] = {int(e) for e in bundle.postings(term_id)}
        planted = {entity_of[i] for i in range(N_ITEMS) if compartment_of(i) == name}
        if compartments[name] != planted:
            raise ValueError(f"term '{name}' does not hold the entities the fixture planted in it")

    # The decorrelation the differential rests on: every compartment meets every quarter, and
    # every compartment also has members outside it. Either being empty makes an assertion pass
    # for a reason the test did not intend.
    decorrelation: dict[tuple[str, str], tuple[int, int]] = {}
    for name in compartments:
        for key in QUARTER_KEYS:
            inside = sum(
                1 for i in range(N_ITEMS) if compartment_of(i) == name and in_quarter(i, key)
            )
            outside = sum(
                1 for i in range(N_ITEMS) if compartment_of(i) == name and not in_quarter(i, key)
            )
            if inside == 0 or outside == 0:
                raise ValueError(
                    f"compartment '{name}' and view '{GROUP}:{key}' are not decorrelated "
                    f"({inside} in, {outside} out) — an assertion over them would pass vacuously"
                )
            decorrelation[(name, key)] = (inside, outside)

    return VerificationReport(
        views=views, compartment_entities=compartments, decorrelation=decorrelation
    )


def entity_of_fx_key(bundle: Bundle) -> dict[int, int]:
    """`fx_key -> entity id` — the handle→item join's last hop, which is the bundle's to answer.

    The fixture planted `fx_key` against a **source** id; the build decided which entity that
    source became. Joining through `Bundle.entities_by_source` is what makes each planted value
    the right entity's, and it is not the identity: entity ids tie-break on the anchor view's
    Morton code (decision 0073, decision 0112), so a block's entities are a permutation of its
    sources.
    """
    entity_of = bundle.entities_by_source()
    return {key: entity_of[source] for source, key in enumerate(fx_keys())}


def mood_columns(bundle: Bundle) -> dict[str, dict[int, str]]:
    """The scoped **category** family as the fixture planted it: `{view id: {entity: key}}`.

    [`sentiment_columns`]'s construction over the other family, and for its reason: built from
    [`mood_of`] and never from the bundle's postings, so the comparison is a differential.
    """
    entity_of = bundle.entities_by_source()
    return {
        f"{GROUP}:{key}": {
            entity_of[i]: value
            for i in range(N_ITEMS)
            if (value := mood_of(i, key)) is not None
        }
        for key in QUARTER_KEYS
    }


def sentiment_columns(bundle: Bundle) -> dict[str, dict[int, float]]:
    """The scoped family as the **fixture** planted it: `{view id: {entity: value}}`.

    Built from [`sentiment_of`], never from the bundle's `attrs/` artefact — that independence is
    what makes the pinned-leaf case a differential and not a transcription (`oracle/filters.py`'s
    module doc argues it at the class). Keyed by entity, planted by source; the join is the
    bundle's, exactly as `entity_of_fx_key`'s is.
    """
    entity_of = bundle.entities_by_source()
    columns: dict[str, dict[int, float]] = {}
    for key in QUARTER_KEYS:
        columns[f"{GROUP}:{key}"] = {
            entity_of[i]: value
            for i in range(N_ITEMS)
            if (value := sentiment_of(i, key)) is not None
        }
    return columns
