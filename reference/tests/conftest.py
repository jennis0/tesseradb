"""Spawns a real `tessera serve` process against the /tmp/tessera-250k fixture bundle and gives
tests an HTTP-only handle to it.

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

from pathlib import Path

import pytest

from oracle.harness import (  # noqa: F401 (re-exported for tests importing directly from here)
    OPERATOR_CREDENTIAL,
    SESSION_CREDENTIAL,
    Server,
    ensure_cli_built,
    ensure_fixture_bundle,
    spawn_server,
    stop_server,
)

BUNDLE_ROOT = Path("/tmp/tessera-250k")


@pytest.fixture(scope="session")
def bundle_root() -> Path:
    ensure_fixture_bundle(BUNDLE_ROOT)
    return BUNDLE_ROOT


@pytest.fixture(scope="session")
def server(tmp_path_factory, bundle_root):
    ensure_cli_built()
    tmp_dir = tmp_path_factory.mktemp("tessera-serve")
    srv, proc = spawn_server(bundle_root, tmp_dir)
    yield srv
    stop_server(proc)
