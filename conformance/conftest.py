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

from oracle.harness import ensure_cli_built, ensure_fixture_bundle, spawn_server, stop_server  # noqa: E402

BUNDLE_ROOT = Path("/tmp/tessera-250k")  # same fixture bundle reference/tests uses — reused, not rebuilt


@pytest.fixture(scope="session")
def bundle_root() -> Path:
    ensure_fixture_bundle(BUNDLE_ROOT)
    return BUNDLE_ROOT


@pytest.fixture(scope="session")
def server(tmp_path_factory, bundle_root):
    ensure_cli_built()
    tmp_dir = tmp_path_factory.mktemp("tessera-serve-conformance")
    srv, proc = spawn_server(bundle_root, tmp_dir)
    yield srv
    stop_server(proc)
