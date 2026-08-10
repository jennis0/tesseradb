"""Shared fixtures for `conformance/tests` — the Phase 1 invariant conformance suite (Task 15).

Reuses `reference/oracle`'s bundle/mask/viewport modules and `oracle.harness`'s server-spawning
machinery (Task 14's differential harness, refactored for reuse rather than copy-pasted — see
`reference/oracle/harness.py`'s module doc). `reference/` is added to `sys.path` explicitly
because this suite lives in a sibling directory to it, not inside `reference/`'s own package
root.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "reference"))

# **Every fixture in this suite is synthesised from a seed.** Nothing here reads the Phase 0
# corpus, so the suite runs from a clean checkout with no external data — which is what lets it run
# in CI at all. The byte-scan and the restart-replay module used to build from a 250,000-item
# prefix of that corpus; both now use the catalogue below, and each states in its own module doc
# what the change cost it. `reference/tests` still uses the Phase 0 corpus and is a separate
# question.
#
# There is deliberately no shared session-scoped `server` fixture (code review flagged the previous
# one as dead weight — nothing used it). Every module needing a non-catalogue server needs it
# spawned with non-default arguments — a file-backed log for the byte-scan, a private fixed
# cache/WAL path for restart-replay, two independent bundles for the canary — so each defines its
# own.
#
# The **catalogue** fixtures below are shared, and for the opposite reason: the adversarial mask
# catalogue is one designed corpus with one designed entity-ID layout, and two modules asking for
# two builds of it would be two different `tessera_id` orderings of the same items.


@pytest.fixture(scope="session")
def catalogue_bundle_root() -> Path:
    """The adversarial mask catalogue's bundle, built once per machine at a fixed path."""
    from oracle.catalogue import build_catalogue_bundle  # noqa: PLC0415

    root, _fx = build_catalogue_bundle()
    return root


@pytest.fixture(scope="session")
def catalogue_bundle(catalogue_bundle_root: Path):
    """The catalogue bundle with its source geometry attached.

    Attached, not optional: `columns.arrow` stores a residual rather than coordinates, so the
    oracle's geometry comes from the points file the build consumed and a driver has to supply it
    (`harness.open_bundle_with_source`).
    """
    from oracle.catalogue import catalogue_points_path  # noqa: PLC0415
    from oracle.harness import open_bundle_with_source  # noqa: PLC0415

    return open_bundle_with_source(catalogue_bundle_root, catalogue_points_path())


@pytest.fixture(scope="session")
def catalogue_filter_columns():
    """The catalogue's filter columns as the **fixture** planted them — the filter oracle's input.

    Built from `oracle.catalogue`'s pure generation functions (`department_of`, `title_of`) and
    the declaration's own key→code pinning, never from the bundle's `attrs/` artefact: the
    differential's independence is that the oracle knows what each entity was *given* while the
    engine serves what the build *stored* (see `oracle/filters.py`'s module doc). Entity id ==
    source id for this corpus, an equality `verify()` re-derives rather than assumes.
    """
    from oracle import catalogue as cat  # noqa: PLC0415
    from oracle.filters import CategoryColumn, StringColumn  # noqa: PLC0415

    return {
        "department": CategoryColumn(
            values={
                e: key
                for e in range(cat.N_ITEMS)
                if (key := cat.department_of(e)) is not None
            },
            codes=dict(cat.DEPARTMENT_CODES),
        ),
        # The `public` counterpart: the same definition, over the column whose operands the engine
        # answers from its derived postings rather than by scanning (decision 0061). The oracle has
        # one evaluation and the engine has two, which is what makes the routed answer testable.
        "archive": CategoryColumn(
            values={
                e: key for e in range(cat.N_ITEMS) if (key := cat.archive_of(e)) is not None
            },
            codes=dict(cat.ARCHIVE_CODES),
        ),
        "title": StringColumn(
            values={
                e: text for e in range(cat.N_ITEMS) if (text := cat.title_of(e)) is not None
            }
        ),
    }


@pytest.fixture(scope="session")
def catalogue_server(tmp_path_factory, catalogue_bundle_root: Path):
    """θ **saturated** — selection reduces to "serve every visible row up to the cap".

    Right for the clauses that are not θ: the floor, the cap, and the ordering. With θ live every
    point-set assertion also depends on the threshold clause, so a selection-ordering bug and a θ
    arithmetic bug become indistinguishable. `catalogue_density_server` is the other half.
    """
    from oracle.harness import spawn_server, stop_server  # noqa: PLC0415

    tmp_dir = tmp_path_factory.mktemp("catalogue-serve")
    srv, proc = spawn_server(catalogue_bundle_root, tmp_dir)
    yield srv
    stop_server(proc)


@pytest.fixture(scope="session")
def catalogue_capped_server(tmp_path_factory, catalogue_bundle_root: Path):
    """`k_max_marks = 128` — §7.2's *K*<sub>max</sub>, at the value the design actually names.

    Every other server in this suite defaults it to 1,000,000 (`harness.write_config`), which is
    deliberate for them — a suite asserting masking or wire shape should not have its point sets
    truncated by a cap it did not choose — and leaves `cap = min(k, K_max)` reducing to `k` for
    every request the suite makes. The cap clause is then dead in every conformance run: an engine
    that ignored `k_max_marks` entirely, or read `max_k` as the cap, would pass. §7.2 warns about
    precisely that conflation ("*K*<sub>max</sub> is an **overplot** ceiling and is deliberately
    not the same knob as the machine ceiling"), so one server exists to make it live. `max_k` is
    left at its default here on purpose: the two knobs must be *different* for the test to tell
    which one an engine read.
    """
    from oracle.harness import spawn_server, stop_server  # noqa: PLC0415

    tmp_dir = tmp_path_factory.mktemp("catalogue-serve-capped")
    srv, proc = spawn_server(catalogue_bundle_root, tmp_dir, k_max_marks=128)
    yield srv
    stop_server(proc)


@pytest.fixture(scope="session")
def catalogue_density_server(tmp_path_factory, catalogue_bundle_root: Path):
    """θ **live** — the configuration §7.2's density rule actually ships in.

    `theta_target_marks = 16` against the 150,000-item catalogue puts `P_0` at 16/V_total for each
    case's own V_total, which differs by four orders of magnitude across the catalogue — that
    spread is the point, since θ's anchor is a per-viewer quantity and a catalogue that only ever
    exercised one anchor would not test it.
    """
    from oracle.harness import spawn_server, stop_server  # noqa: PLC0415

    tmp_dir = tmp_path_factory.mktemp("catalogue-serve-density")
    srv, proc = spawn_server(catalogue_bundle_root, tmp_dir, theta_target_marks=16)
    yield srv
    stop_server(proc)
