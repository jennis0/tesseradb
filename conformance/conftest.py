"""Shared fixtures for `conformance/tests` — the Phase 1 invariant conformance suite (Task 15).

Reuses `reference/oracle`'s bundle/mask/viewport modules and `oracle.harness`'s server-spawning
machinery (Task 14's differential harness, refactored for reuse rather than copy-pasted — see
`reference/oracle/harness.py`'s module doc). `reference/` is added to `sys.path` explicitly
because this suite lives in a sibling directory to it, not inside `reference/`'s own package
root.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "reference"))

from oracle.harness import ensure_fixture_bundle  # noqa: E402

# Task 16, Step 4: `TESSERA_BUNDLE_ROOT` override for the one-off 10^9 exit-criteria run — see
# `reference/tests/conftest.py`'s matching comment. Unset, this is unchanged: the same 250k
# fixture `reference/tests` uses, reused not rebuilt.
_ENV_BUNDLE_ROOT = os.environ.get("TESSERA_BUNDLE_ROOT")
BUNDLE_ROOT = Path(_ENV_BUNDLE_ROOT) if _ENV_BUNDLE_ROOT else Path("/tmp/tessera-250k")


@pytest.fixture(scope="session")
def bundle_root() -> Path:
    if _ENV_BUNDLE_ROOT:
        assert BUNDLE_ROOT.joinpath("CURRENT").exists(), (
            f"TESSERA_BUNDLE_ROOT={BUNDLE_ROOT} set but no bundle found there"
        )
        return BUNDLE_ROOT
    ensure_fixture_bundle(BUNDLE_ROOT)
    return BUNDLE_ROOT


# NB: deliberately no shared session-scoped `server` fixture over `bundle_root` (code review
# flagged the previous one as dead weight — nothing in this suite used it). Every test module
# against that fixture needs a server spawned with non-default arguments (a file-backed log for
# the byte-scan, a private fixed cache/WAL path for restart-replay, two independent bundles for
# the canary scaffold), so each defines its own.
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
    from oracle.bundle import Bundle  # noqa: PLC0415

    return Bundle(catalogue_bundle_root)


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
