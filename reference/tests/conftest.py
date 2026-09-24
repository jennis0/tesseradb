"""Spawns a real `tessera serve` process against the 250k fixture bundle and gives tests an
HTTP-only handle to it.

This is the "server spawned via a fixture" the task brief calls for: the harness runs the actual
`tessera` binary (built via `cargo build --release -p tessera-cli`), not an in-process shortcut,
and talks to it only over HTTP/the filesystem — never by calling into `tessera-engine` directly
(Python is a consumer, never a component).

The actual spawn/build machinery lives in `oracle.harness` (Task 15's refactor) so
`conformance/tests` can reuse it without copy-paste; this file only wires up the fixtures this
suite's tests already depend on, with the same names, defaults and lifetime as before the
refactor.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from oracle.harness import (  # noqa: F401 (re-exported for tests importing directly from here)
    OPERATOR_CREDENTIAL,
    SESSION_CREDENTIAL,
    Server,
    ensure_cli_built,
    ensure_fixture_bundle,
    fixture_dir,
    spawn_server,
    stop_server,
)

# Task 16, Step 4: `TESSERA_BUNDLE_ROOT` lets the differential/conformance suites be pointed at
# the 10^9 bundle for the one-off exit-criteria run, without touching the 250k default any other
# invocation (CI, everyday `pytest`) relies on. When set, the bundle is assumed to already exist
# (a 10^9 build is never implicitly triggered by a test run) -- `ensure_fixture_bundle` is only
# called for the default small fixture.
_ENV_BUNDLE_ROOT = os.environ.get("TESSERA_BUNDLE_ROOT")


@pytest.fixture(scope="session")
def bundle_root() -> Path:
    if _ENV_BUNDLE_ROOT:
        root = Path(_ENV_BUNDLE_ROOT)
        assert root.joinpath("CURRENT").exists(), (
            f"TESSERA_BUNDLE_ROOT={root} set but no bundle found there"
        )
        return root
    root = fixture_dir("250k") / "bundle"
    ensure_fixture_bundle(root)
    return root


@pytest.fixture(scope="session")
def server(tmp_path_factory, bundle_root):
    """A server with theta SATURATED — selection reduces to "serve every visible row up to the cap".

    Right for the suites that assert masking, counts and wire shape: with theta live, every point-set
    assertion would also depend on design §7.2's threshold clause, so a masking bug and a theta
    arithmetic bug would be indistinguishable.

    **It is the wrong configuration for the density rule itself**, and using it alone was a real gap:
    under saturation the oracle's `theta_cut` returns None, `c_theta` becomes `len(visible)`, and the
    differential's point-set comparison degenerates to "serve everything visible" — it would pass
    identically against an engine that anchored theta on the PRE-OVERLAY projection (the I2 breach
    §7.2 exists to prevent), counted occupied tiles over the wrong mask, or omitted the threshold
    clause outright. Use `density_server` for those.
    """
    ensure_cli_built()
    tmp_dir = tmp_path_factory.mktemp("tessera-serve")
    srv, proc = spawn_server(bundle_root, tmp_dir)
    yield srv
    stop_server(proc)


@pytest.fixture(scope="session")
def density_server(tmp_path_factory, bundle_root):
    """A server with theta LIVE — the configuration design §7.2's density rule actually ships in.

    `theta_target_marks = 16` against the 250k fixture puts theta_0 at 16/250000, so a full-extent
    viewport straddles all three clauses as zoom varies: the floor binds at deep zoom where tiles
    hold few visible rows, the threshold governs the middle, and the cap binds at shallow zoom. That
    is what makes the differential's point-set comparison discriminating rather than vacuous.

    `k_max_marks` is left at the harness default so the cap does not truncate before theta has had a
    chance to bind; the point here is the threshold arithmetic, not the cap.
    """
    ensure_cli_built()
    tmp_dir = tmp_path_factory.mktemp("tessera-serve-density")
    srv, proc = spawn_server(bundle_root, tmp_dir, theta_target_marks=16)
    yield srv
    stop_server(proc)
