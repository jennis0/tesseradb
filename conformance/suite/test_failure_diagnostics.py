"""What a failed walk writes about its server: every thread of a live server, and its log's tail.

CI once lost a walk to a server that sent an aggregate's headers and then nothing for 60 s, and
the job log held neither the server's log nor its threads.
"""

from __future__ import annotations

import os
import subprocess
import sys

import pytest
from oracle import catalogue as cat

from .driver import Build, Stage, SuiteHarness, poll, run_plan, server_diagnostics
from .test_stage_invariance import BBOX, GRANTS, K

pytestmark = pytest.mark.writes_nothing

HANGS = """
import threading, time
for _ in range(2):
    threading.Thread(target=threading.Event().wait, daemon=True).start()
time.sleep(600)
"""


@pytest.fixture
def hung(tmp_path):
    """A three-thread process that never returns, and a 40 KiB log ending in a known line."""
    proc = subprocess.Popen([sys.executable, "-c", HANGS])  # nosemgrep
    poll(
        lambda: len(os.listdir(f"/proc/{proc.pid}/task")) == 3,
        "the hanging process never started its threads",
        timeout=10.0,
    )
    log = tmp_path / "server.log"
    log.write_text("the log's first line\n" + "x" * 40_000 + "\nthe log's last line\n")
    yield proc.pid, log
    proc.kill()
    proc.wait()


def _tids(pid: int) -> list[str]:
    return os.listdir(f"/proc/{pid}/task")


def test_a_hung_process_is_reported_by_its_threads_and_its_log_tail(hung):
    pid, log = hung
    text = server_diagnostics(pid, log)
    assert "the log's last line" in text
    assert "the log's first line" not in text
    has_backtrace = "#0 " in text
    has_states = all(tid in text for tid in _tids(pid))
    assert has_backtrace or has_states, text


def test_with_no_tool_to_attach_every_thread_is_listed_by_state(hung, tmp_path, monkeypatch):
    pid, log = hung
    monkeypatch.setenv("PATH", str(tmp_path / "empty"))
    text = server_diagnostics(pid, log)
    for tid in _tids(pid):
        assert tid in text
    assert "the log's last line" in text


class _Fails(Stage):
    label = "fails"

    def __init__(self) -> None:
        self.raised = RuntimeError("the stage failed")

    def apply(self, h: SuiteHarness) -> None:
        raise self.raised


def test_a_walk_that_fails_under_a_live_server_writes_its_threads_and_log_before_raising(
    tmp_path_factory, private_catalogue_bundle, capfd
):
    h = SuiteHarness(
        bundle_root=private_catalogue_bundle("failure-diagnostics"),
        run_dir=tmp_path_factory.mktemp("failure-diagnostics-run"),
        grants=GRANTS,
        view_id=cat.VIEW_ID,
        bbox=BBOX,
        k=K,
    )
    stage = _Fails()
    try:
        with pytest.raises(RuntimeError) as raised:
            run_plan(h, [Build(), stage])
        pid = h.proc.pid
        assert h.proc.poll() is None
    finally:
        h.stop()
    assert raised.value is stage.raised
    err = capfd.readouterr().err
    assert "=== stage `fails` failed" in err
    assert f"--- threads of mosaica serve (pid {pid})" in err
    assert f"bytes of {h.log_path}" in err


def test_a_walk_that_fails_before_any_server_writes_nothing_it_does_not_have(tmp_path, capfd):
    h = SuiteHarness(
        bundle_root=tmp_path,
        run_dir=tmp_path,
        grants=GRANTS,
        view_id=cat.VIEW_ID,
        bbox=BBOX,
        k=K,
    )
    stage = _Fails()
    with pytest.raises(RuntimeError) as raised:
        run_plan(h, [stage])
    assert raised.value is stage.raised
    err = capfd.readouterr().err
    assert "=== stage `fails` failed" in err
    assert "threads of mosaica serve" not in err
