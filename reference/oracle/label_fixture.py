"""The I3 containment fixture: one corpus, three labels, and principals a single entity apart.

**I3 is *labels gate on `M_auth`*, and the property has a sharp edge**: a viewer is served a
content when they contain its generating set **entirely**, and otherwise the artifact is absent —
not the artifact with its description missing, which is the in-between state
[decision 0076](../../docs/decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)
forbids. A fixture that tests it with two principals a hundred entities apart tests nothing sharp:
either could be failing for a dozen reasons. So this one is built backwards from the edge.

**One entity carries the whole difference.** Source id `SPLIT` (30) carries `EDGE_TERM` and nothing
else; every other member of the widest generating set carries `CORE_TERM`. A principal holding both
terms contains that set entire; a principal holding `CORE_TERM` alone is short of it by **exactly
one member**, and by nothing else — their masked counts over the same artifact differ by one, which
is asserted rather than assumed.

**Three labels, so absence has a control beside it.** They differ only in which generating sets
their ranked contents were drawn from:

| key | rank 0 generated from | rank 1 | `CORE`'s answer |
|---|---|---|---|
| `l-whole` | `0..=SPLIT` | — | **absent** — one member short of its only content |
| `l-ranked` | `0..=SPLIT` | `0..=9` | rank 1's text — the narrower variant it does contain |
| `l-core` | `0..=9` | — | rank 0's text — served exactly as the wider principal is |

`l-core` is what makes `l-whole`'s absence mean containment: the same principal, in the same
response, is served an artifact of the same layer over the same membership. Without it, a
containment check deleted altogether would still leave `CORE` seeing nothing and the test green for
the wrong reason.

**Nothing else may withhold.** The layer is `public`, its artifact gate is `inherited` and its
`require_member_visibility` is `none` — so the existence criterion, the layer gate and the artifact
gate all pass for every principal here, and containment is the only test left that can fail.

**Deliberately not the mask catalogue.** That fixture is built backwards from the selection
invariant's adversarial mask shapes and is shared by six modules; adding an artifact layer to it
would put artifact entities into a corpus whose byte-scan reasoning is written against a stated
entity-id floor. A small corpus of its own is also what makes "exactly one member" a fact a reader
can check by reading, which is the same argument `canary_fixture` makes for the canary.
"""

from __future__ import annotations

import random
import subprocess
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from .harness import CLI_BIN, REPO_ROOT, build_env, ensure_cli_built, write_deployment

N_ITEMS = 200
VIEW_ID = "s0"
EXTENT_MAX = 65536.0
SEED = 20260820

# One fixed identity key, as the canary fixture uses: `tessera_id` is a keyed permutation, so a
# minted-per-build key would make every recorded identifier a different number for no reason.
LABEL_ID_KEY_HEX = "000102030405060708090a0b0c0d0e0f"

# The terms, as the exploded `(entity_id, term_id)` relation carries them. A session names them by
# their decimal spelling, which is the descriptor the build interns.
CORE_TERM = 1
EDGE_TERM = 2
OTHER_TERM = 3

#: The one entity that carries `EDGE_TERM` and nothing else — the whole difference between the two
#: principals below, and therefore the whole of what this fixture is about.
SPLIT = 30

#: The widest generating set: `0..=SPLIT`. `CORE` holds every member of it but the last.
WIDE_SET = list(range(SPLIT + 1))
#: The narrow generating set, entirely inside `CORE_TERM`.
NARROW_SET = list(range(10))
#: Every label's membership, which is deliberately the same for all three and wider than any
#: generating set: an absence here is never explicable by an empty membership.
MEMBERS = list(range(60))

#: The three principals, by the terms they hold.
WHOLE_TERMS = [str(CORE_TERM), str(EDGE_TERM)]
CORE_TERMS = [str(CORE_TERM)]
OTHER_TERMS = [str(OTHER_TERM)]

LAYER = "topics/x"


def terms_of(source_id: int) -> list[int]:
    """Which terms a point carries. Ids at and above 100 carry none and take the declared
    `public` default, so the map is populated for every principal and no artifact's absence can be
    read as an empty viewport."""
    if source_id == SPLIT:
        return [EDGE_TERM]
    if source_id < 60:
        return [CORE_TERM]
    if source_id < 100:
        return [OTHER_TERM]
    return []


def visible_to(terms: list[str]) -> set[int]:
    """The source ids a principal holding `terms` can see — the fixture's own answer, computed from
    the planting rules rather than from anything the engine said."""
    held = {int(t) for t in terms}
    return {
        source_id
        for source_id in range(N_ITEMS)
        if not terms_of(source_id) or held & set(terms_of(source_id))
    }


def _write_points(path: Path) -> None:
    rng = random.Random(SEED)
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array(range(N_ITEMS), type=pa.uint64()),
                "x": pa.array(
                    [rng.uniform(0.0, EXTENT_MAX) for _ in range(N_ITEMS)], type=pa.float32()
                ),
                "y": pa.array(
                    [rng.uniform(0.0, EXTENT_MAX) for _ in range(N_ITEMS)], type=pa.float32()
                ),
            }
        ),
        path,
    )


def _write_pairs(path: Path) -> None:
    rows = [(e, t) for e in range(N_ITEMS) for t in terms_of(e)]
    pq.write_table(
        pa.table(
            {
                "entity_id": pa.array([r[0] for r in rows], type=pa.uint64()),
                "term_id": pa.array([r[1] for r in rows], type=pa.uint32()),
            }
        ),
        path,
    )


def _member_rows() -> list[tuple[str, int | None, int]]:
    """One row per `(artifact, rank, entity)`. A null rank is the artifact's own membership; rank
    `k` is the generating set of `contents[k]` (`configuration.md` §1, `[layer.members]`)."""
    rows: list[tuple[str, int | None, int]] = []
    for key in ("l-whole", "l-ranked", "l-core"):
        rows.extend((key, None, e) for e in MEMBERS)
    rows.extend(("l-whole", 0, e) for e in WIDE_SET)
    rows.extend(("l-ranked", 0, e) for e in WIDE_SET)
    rows.extend(("l-ranked", 1, e) for e in NARROW_SET)
    rows.extend(("l-core", 0, e) for e in NARROW_SET)
    return rows


def _write_members(path: Path) -> None:
    rows = _member_rows()
    pq.write_table(
        pa.table(
            {
                "key": pa.array([r[0] for r in rows], type=pa.string()),
                "rank": pa.array([r[1] for r in rows], type=pa.uint32()),
                "entity": pa.array([r[2] for r in rows], type=pa.uint64()),
            }
        ),
        path,
    )


#: The declaration, in full. The three artifacts are inline because they are three rows and the
#: file they would otherwise live in would carry nothing this document does not already say; their
#: generating sets are not, because those are 91 rows and belong in the member source the surface
#: provides for exactly that (`configuration.md` §1).
CONFIG_TOML = f"""
[sources]
points  = "points.parquet"
pairs   = "pairs.parquet"
members = "members.parquet"

[[view]]
name             = "{VIEW_ID}"
extent           = {{ x = [0.0, {EXTENT_MAX}], y = [0.0, {EXTENT_MAX}] }}
source           = "points"
point_visibility = {{ source = "pairs", default = "public" }}

[[layer]]
name = "{LAYER}"
title = "topics"
views = ["{VIEW_ID}"]
membership = "enumerated"
visibility = "public"
artifact_visibility = {{ default = "inherited" }}
require_member_visibility = "none"
hierarchy = {{ kind = "flat", prune_children = false }}
artifacts = [
  {{ key = "l-whole",  contents = [["drawn from the whole set"]] }},
  {{ key = "l-ranked", contents = [["drawn from the whole set"], ["drawn from the core alone"]] }},
  {{ key = "l-core",   contents = [["drawn from the core alone"]] }},
]

  [layer.members]
  source = "members"

  [[layer.content.supplied]]
  name = "topic"
  type = "text"
  require_member_visibility = "all"
"""


def build_label_bundle(work_dir: Path) -> Path:
    """Write the corpus and its declaration under `work_dir` and build the bundle; return its root.

    `--mint-external-ids` is passed because the cache half of §4.4's row addresses one entity over
    `/control/changes`, which takes an external id. This fixture's items are synthesised here, so
    saying they have caller-supplied ids is a statement about this corpus and not a manufactured
    one (contracts §2.4).
    """
    ensure_cli_built()
    work_dir.mkdir(parents=True, exist_ok=True)
    _write_points(work_dir / "points.parquet")
    _write_pairs(work_dir / "pairs.parquet")
    _write_members(work_dir / "members.parquet")
    config = work_dir / "labels.toml"
    config.write_text(CONFIG_TOML)

    bundle = work_dir / "bundle"
    deployment = write_deployment(work_dir / "tessera.toml", bundle=bundle, schema=config)
    subprocess.run(
        [
            str(CLI_BIN),
            "build",
            "--deployment",
            str(deployment),
            "--out",
            str(bundle),
            "--mint-external-ids",
        ],
        cwd=REPO_ROOT,
        env=build_env(LABEL_ID_KEY_HEX),
        check=True,
    )
    return bundle
