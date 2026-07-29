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


# NB: deliberately no shared session-scoped `server` fixture here (code review flagged the
# previous one as dead weight — nothing in this suite used it). Every test module needs a server
# spawned with non-default arguments (a file-backed log for the byte-scan, a private fixed
# cache/WAL path for restart-replay, two independent bundles for the canary scaffold), so each
# module defines its own `spawn_server(...)` fixture rather than sharing one generic instance.
