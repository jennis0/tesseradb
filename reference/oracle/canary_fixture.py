"""The I2 canary fixture: three bundles differing only in one item, and in who can see it.

Synthesises tiny points+pairs parquet sets for `test_canary.py` and builds three bundles from them
via the CLI: **canary-free** (the corpus alone), **canary** (plus one item carrying a term nobody
tested holds), and **visible** (plus the same item carrying a term they do). The third is the
comparator's positive control — §4.4 — and without it the canary comparison is pass-only with no
proof it can fail. Deliberately independent of the 250k Phase 0 corpus: a
small, from-scratch synthetic dataset makes it easy to reason that the canary's term is genuinely
held by *no* tested principal, and keeps both builds fast enough to run twice in one test.

# The canary allocation rules — there are FIVE

The canary test compares two fixture states and asserts that **no aggregate moves by any amount**.
That assertion is only about disclosure if the canary's presence has zero *legitimate* influence on
anything else. Otherwise the comparator flags fixture perturbation and calls it a leak. Conformance
design §2 states four rules; the stage-2.1 plan review added a fifth.

1. **Canary entity IDs are allocated after all real IDs.** Entity IDs are assigned in
   term-signature order and are permanent (I9); an ID inserted in the middle shifts every later
   item's ID, and `tessera_id` is a keyed permutation of `(shard_id, entity_id)`, so every shifted
   item gets a new identity — and §7.2 selects the lowest identities in a tile. A displaced ID
   therefore changes the *sample*, everywhere, legitimately.
2. **Coordinates at the Morton-maximal corner.** Rows are stored in `(morton, tessera_id)` order,
   so an item anywhere else shifts the row IDs of everything after it. `(65535.9, 65535.9)`
   quantises to cell 65535 on both axes (contracts §2.5: `v == max` lands in the top cell), which
   is the maximum representable code, so the canary sorts last and shifts nothing.
3. **Canary terms are interned last.** Term IDs are assigned in first-appearance order over the
   source-ID-sorted points, and the *signature* that orders entity-ID assignment is a sorted list
   of term IDs — so a canary term interned early would renumber other terms and reorder the
   assignment, which is rule 1 again by another route.
4. **Canaries belong to no cluster node and no generating set.** A canary inside a generating set
   makes a label *legitimately* withheld in the canary state (containment fails), and one inside a
   node perturbs build-time geometry. Either would make the comparator flag fixture perturbation
   as disclosure. **Vacuous in Phase 1 and stated rather than skipped:** the build emits no cluster
   membership bitmaps and no generating sets, so there is nothing for the canary to be in. It
   becomes load-bearing the moment §7.5/§7.6 land, which is why it is written down now.
5. **Canaries are allocated in their own commit window.** *(Added at the stage-2.1 plan review.)*
   Rules 1 and 3 are stated as global properties — "after all real IDs" — but stage 2.1's Task 7a
   makes signature-sorted assignment **window-scoped** (§11.1 r23: "within each batch and only
   within one"). Inside a shared window the canary is sorted by its signature against its
   window-mates, not against the corpus, so "after all real IDs" silently becomes "somewhere in the
   middle of this window" and rule 1 is lost without anything failing. Giving the canary its own
   window restores the global property, because windows are assigned in order.

   Implemented here rather than described: both builds pass `--batch-items N_BASE_ITEMS`, which is
   the build-side name for the same mechanism. The canary-free corpus is exactly one window; the
   canary corpus is that same window plus a second window holding the canary alone. The base
   items' assignment is therefore bit-identical between the two builds *by construction*, not by
   luck. **This rule must exist before Task 7a merges.**

`verify_allocation_rules` checks 1–3 and 5 as a single, stronger property: the canary bundle's
stored rows are the canary-free bundle's rows, unchanged, plus one row at the end. Nothing else can
be true if any of the four were violated.
"""

from __future__ import annotations

import random
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from .harness import CLI_BIN, REPO_ROOT, build_env, ensure_cli_built, write_deployment

N_BASE_ITEMS = 400
N_TERMS = 6
EXTENT = "0,65536,0,65536"
VIEW_ID = "s0"
SEED = 20260729
# One fixed identity key for BOTH bundles. See the build-args comment below for why an independent
# per-bundle key made the point-set comparison vacuous under identity-ordered selection. The value
# is arbitrary but must be a real key (`IdentityKey::from_hex` refuses degenerate ones); it is the
# same canonical vector the Rust fixture tests use.
CANARY_ID_KEY_HEX = "000102030405060708090a0b0c0d0e0f"

# The canary's own term id — deliberately one past the base terms, and never granted to any
# session `test_canary.py` authorises.
CANARY_TERM_ID = N_TERMS

# The **visible** state's extra item carries a base term instead, so a tested principal can see it.
# That single change is what makes the comparator's positive control a control: the visible state is
# built exactly like the canary state — same corner, same commit window, same key, allocated last —
# so the only thing that differs between "the comparator must agree" and "the comparator must
# disagree" is whether the extra item is inside the principal's `M_auth`. A control that also
# differed in placement or allocation would prove the comparator notices *something*, which is not
# the claim; the claim is that it notices a visible item.
VISIBLE_TERM_ID = 0

# Extreme corner of the quantisation extent (contracts §2.5: v == max lands in the top cell).
CANARY_X = 65535.9
CANARY_Y = 65535.9


def _base_dataset(rng: random.Random) -> tuple[list[tuple[int, float, float]], list[tuple[int, int]]]:
    points = []
    pairs = []
    for entity_id in range(N_BASE_ITEMS):
        x = rng.uniform(0.0, 65536.0)
        y = rng.uniform(0.0, 65536.0)
        points.append((entity_id, x, y))
        # Each item carries 1-2 of the N_TERMS base terms.
        n_terms = rng.choice([1, 1, 2])
        terms = rng.sample(range(N_TERMS), n_terms)
        for t in terms:
            pairs.append((entity_id, t))
    return points, pairs


def _write_points(path: Path, points: list[tuple[int, float, float]]) -> None:
    table = pa.table(
        {
            "entity_id": pa.array([p[0] for p in points], type=pa.uint64()),
            "x": pa.array([p[1] for p in points], type=pa.float32()),
            "y": pa.array([p[2] for p in points], type=pa.float32()),
        }
    )
    pq.write_table(table, path)


def _write_pairs(path: Path, pairs: list[tuple[int, int]]) -> None:
    table = pa.table(
        {
            "entity_id": pa.array([p[0] for p in pairs], type=pa.uint64()),
            "term_id": pa.array([p[1] for p in pairs], type=pa.uint32()),
        }
    )
    pq.write_table(table, path)


def build_canary_states(work_dir: Path) -> tuple[Path, Path, Path]:
    """Write the three synthetic input sets and build all three bundles under `work_dir`.

    Returns `(canary_free, canary, visible)`.

    **Three states, because two cannot prove a comparator works.** The first two are the pair
    conformance §4.2 describes: identical but for one item carrying a term no tested principal
    holds, which every response must be blind to. The third is §4.4's positive control — the same
    construction with the extra item carrying a term principals *do* hold, so every comparison it
    takes part in must **fail**. Without it the comparator is pass-only: broken in any way that
    makes it always agree, it would report green for ever and nothing would notice.
    """
    ensure_cli_built()
    rng = random.Random(SEED)
    points, pairs = _base_dataset(rng)

    free_points_path = work_dir / "free-points.parquet"
    free_pairs_path = work_dir / "free-pairs.parquet"
    _write_points(free_points_path, points)
    _write_pairs(free_pairs_path, pairs)

    extra_point = (N_BASE_ITEMS, CANARY_X, CANARY_Y)
    canary_points_path = work_dir / "canary-points.parquet"
    canary_pairs_path = work_dir / "canary-pairs.parquet"
    _write_points(canary_points_path, points + [extra_point])
    _write_pairs(canary_pairs_path, pairs + [(N_BASE_ITEMS, CANARY_TERM_ID)])

    # The visible state differs from the canary state in exactly one cell of one input file: the
    # term the extra item carries.
    visible_points_path = work_dir / "visible-points.parquet"
    visible_pairs_path = work_dir / "visible-pairs.parquet"
    _write_points(visible_points_path, points + [extra_point])
    _write_pairs(visible_pairs_path, pairs + [(N_BASE_ITEMS, VISIBLE_TERM_ID)])

    free_bundle = work_dir / "bundle-free"
    canary_bundle = work_dir / "bundle-canary"
    visible_bundle = work_dir / "bundle-visible"

    import subprocess

    # One view, its frame, its geometry and the relation its labels are in — the whole declaration
    # this fixture needs. Three corpora share it, so each build overrides the two sources by their
    # object keys (`configuration.md` §8); the declaration itself is written once.
    config_path = work_dir / "canary-config.toml"
    x_min, x_max, y_min, y_max = EXTENT.split(",")
    config_path.write_text(
        f'[[view]]\nname = "{VIEW_ID}"\n'
        f"extent = {{ x = [{x_min}, {x_max}], y = [{y_min}, {y_max}] }}\n"
        'source = "points.parquet"\n'
        'point_visibility = { source = "pairs.parquet", default = "public" }\n'
    )

    for points_path, pairs_path, out_dir in (
        (free_points_path, free_pairs_path, free_bundle),
        (canary_points_path, canary_pairs_path, canary_bundle),
        (visible_points_path, visible_pairs_path, visible_bundle),
    ):
        deployment = write_deployment(
            work_dir / f"{out_dir.name}-tessera.toml", bundle=out_dir, schema=config_path
        )
        subprocess.run(
            [
                str(CLI_BIN),
                "build",
                "--deployment",
                str(deployment),
                "--file",
                f"view:{VIEW_ID}={points_path}",
                "--file",
                f"view:{VIEW_ID}:point_visibility={pairs_path}",
                "--out",
                str(out_dir),
                # Contracts r6 refuses to build unless a human names the identity key's lineage.
                #
                # **Both bundles must carry the SAME key, and this is load-bearing, not tidiness.**
                # `--mint-id-key` was passed here originally, which minted an *independent random
                # key per bundle*. `tessera_id = FPE_key(shard_id || entity_id)`, so under two keys
                # the two bundles' identities are unrelated — and since design §7.2 selects the
                # lowest identities in a tile, the two bundles necessarily draw different samples
                # whatever the corpus. That made the point-set half of this canary vacuous the
                # moment selection stopped being position-based: it could only ever have compared
                # two unrelated permutations. Contracts §2.6 states the property directly — "row
                # order is key-dependent: a key rotation reorders tied rows".
                #
                # Fixing the key isolates the variable this canary is actually about: the presence
                # of one extra item carrying an ungranted term. It travels in the environment
                # below, never in an argv: there is no flag that takes a key.
                # Allocation rule 5 (see the module doc): the canary gets its own commit window.
                # `--batch-items` is the build-side name for the window §11.1 r23 scopes
                # signature-sorted assignment to. At `N_BASE_ITEMS` the canary-free corpus is
                # exactly one window and the canary corpus is that window plus a second holding
                # the canary alone — so the base items' entity IDs, and therefore their
                # `tessera_id`s and their row order, are identical between the two builds by
                # construction. Both builds pass it because the value is identity-bearing: two
                # bundles built with different batch sizes are two different permanent
                # assignments of the same corpus.
                "--batch-items",
                str(N_BASE_ITEMS),
            ],
            cwd=REPO_ROOT,
            env=build_env(CANARY_ID_KEY_HEX),
            check=True,
        )

    return free_bundle, canary_bundle, visible_bundle


def verify_allocation_rules(free_bundle: Path, canary_bundle: Path) -> list[str]:
    """Check allocation rules 1, 2, 3 and 5 as one property; return the failures.

    **The property:** the canary bundle's segment is the canary-free bundle's segment, row for row,
    plus exactly one row at the end, and that row is the canary at the Morton-maximal corner.

    This is stronger than checking the four rules separately and it is not a coincidence that one
    assertion covers them all. Every rule is a way of saying "the canary displaces nothing", and
    displacement is observable in exactly one place: the stored rows. A canary allocated in the
    middle of entity space (rule 1 or 5 broken) changes every later item's `tessera_id`, which is a
    sort key, so rows move. A canary term interned early (rule 3) renumbers signatures and does the
    same by another route. A canary anywhere but the maximal corner (rule 2) inserts a row in the
    middle. Each is caught here as a row-level difference, with no need to guess which rule failed
    — the diff says where.

    Rule 4 is not checked because in Phase 1 there is nothing to check: no cluster memberships and
    no generating sets are built. See the module doc.

    **Applies to the visible state too, and must.** `canary_bundle` names the argument for the case
    it was written for, but the positive control is only a control if its extra item displaces
    nothing either — a visible state that also perturbed allocation would make the comparator
    disagree for a reason that has nothing to do with visibility, which is the same fixture-
    perturbation failure in the opposite direction.
    """
    from .bundle import Bundle  # local: this module is imported by fixture builders that must not
    # pay for a bundle read they are not doing.

    failures: list[str] = []
    free = Bundle(free_bundle).segment(VIEW_ID)
    canary = Bundle(canary_bundle).segment(VIEW_ID)
    if free.tessera_id is None or canary.tessera_id is None:
        return ["a canary bundle has no stored tessera_id column (pre-r6 build)"]

    if canary.row_count != free.row_count + 1:
        failures.append(
            f"the canary bundle has {canary.row_count} rows against the canary-free bundle's "
            f"{free.row_count}; expected exactly one more"
        )
        return failures

    for row in range(free.row_count):
        # Stored columns on both sides, deliberately: the question here is whether the two
        # *builds* placed the same items at the same rows, not whether either agrees with the
        # source — so comparing what each wrote is exactly right, and an integer position makes
        # the comparison exact where the old `f32` pair made it approximate.
        if (
            free.stored_code(row) != canary.stored_code(row)
            or int(free.tessera_id[row]) != int(canary.tessera_id[row])
        ):
            failures.append(
                f"row {row} differs between the two bundles: the canary displaced a real item, so "
                f"one of allocation rules 1, 2, 3 or 5 is broken and the canary comparison would "
                f"be measuring fixture perturbation rather than disclosure. "
                f"free=(code={free.stored_code(row):#018x}, "
                f"tessera_id={int(free.tessera_id[row])}) "
                f"canary=(code={canary.stored_code(row):#018x}, "
                f"tessera_id={int(canary.tessera_id[row])})"
            )
            break

    last = canary.row_count - 1
    if int(canary.morton[last]) != 0xFFFF_FFFF:
        failures.append(
            f"the canary's row is not at the Morton-maximal corner (code "
            f"{int(canary.morton[last])} != {0xFFFF_FFFF}) — allocation rule 2"
        )
    if int(canary.entity_id[last]) != N_BASE_ITEMS:
        failures.append(
            f"the canary's entity id is {int(canary.entity_id[last])}, not {N_BASE_ITEMS} — it "
            "was not allocated after all real IDs (allocation rules 1 and 5)"
        )
    return failures
