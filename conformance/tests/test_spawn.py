"""Starting `mosaica serve` when a port it was given is taken before it binds."""

from __future__ import annotations

import socket

import requests
from oracle import harness


def test_a_server_given_a_taken_port_is_started_on_fresh_ones(
    catalogue_bundle_root, tmp_path, monkeypatch
):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as held:
        held.bind(("127.0.0.1", 0))
        held.listen()
        taken = held.getsockname()[1]
        handed = iter([None, None, taken])  # viewer, session, control
        free = harness.free_port
        monkeypatch.setattr(harness, "free_port", lambda: next(handed, None) or free())

        server, proc = harness.spawn_server(catalogue_bundle_root, tmp_path)
        try:
            assert server.control_base != f"http://127.0.0.1:{taken}"
            requests.get(f"{server.viewer_base}/healthz", timeout=5).raise_for_status()
        finally:
            harness.stop_server(proc)
