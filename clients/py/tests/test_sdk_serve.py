"""The supervisor: the deployment file, the announce line, and stopping the child by its pid (§7).

The announce line is read from a fake child here, a Python script printing what `tessera serve`
prints, so the parser is covered without a bundle and without the binary.
"""

import os
import subprocess
import sys
import textwrap

import pytest

from tesseradb import _instance

ANNOUNCE = (
    '{"event": "listening", "viewer": "127.0.0.1:38001", "session": "127.0.0.1:38002", '
    '"control": "127.0.0.1:38003"}'
)


def child(script: str) -> subprocess.Popen:
    return subprocess.Popen(
        [sys.executable, "-c", textwrap.dedent(script)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )


def test_the_announce_line_is_read_past_the_childs_own_logging():
    process = child(
        f"""
        import sys
        print("INFO the allocator's arena count is capped")
        print("INFO the engine adopted the prefix's derived artifact structures")
        print({ANNOUNCE!r})
        sys.stdout.flush()
        input()
        """
    )
    try:
        listening = _instance.read_announce(process.stdout, 10, lambda: "")
        assert listening.viewer == "127.0.0.1:38001"
        assert listening.session == "127.0.0.1:38002"
        assert listening.control == "127.0.0.1:38003"
    finally:
        _instance.stop(process)


def test_a_unix_control_plane_is_carried_through_as_written():
    line = (
        '{"event":"listening","viewer":"127.0.0.1:1","session":"127.0.0.1:2",'
        '"control":"unix:/run/tessera/control.sock"}'
    )
    process = child(f"""
        import sys
        print({line!r}); sys.stdout.flush(); input()
        """)
    try:
        assert _instance.read_announce(process.stdout, 10, lambda: "").control == (
            "unix:/run/tessera/control.sock"
        )
    finally:
        _instance.stop(process)


def test_a_child_that_announces_nothing_times_out_carrying_its_stderr():
    process = child(
        """
        import sys
        print("INFO listening on something the SDK cannot read")
        print("refused to start: config io error", file=sys.stderr)
        sys.stdout.flush(); sys.stderr.flush()
        input()
        """
    )
    try:
        errors = _instance._drain(process.stderr)
        with pytest.raises(_instance.ServeRefused) as refusal:
            _instance.read_announce(process.stdout, 1.0, lambda: "".join(errors))
        assert '"event": "listening"' in str(refusal.value)
        assert "config io error" in str(refusal.value)
    finally:
        _instance.stop(process)


def test_a_listening_line_missing_an_address_is_refused_rather_than_guessed():
    process = child("""
        import sys
        print('{"event": "listening", "viewer": "127.0.0.1:1"}'); sys.stdout.flush(); input()
        """)
    try:
        with pytest.raises(_instance.ServeRefused, match="session, control"):
            _instance.read_announce(process.stdout, 5, lambda: "")
    finally:
        _instance.stop(process)


def test_a_child_that_exits_is_not_waited_out():
    process = child("print('INFO nothing to say')")
    with pytest.raises(_instance.ServeRefused):
        _instance.read_announce(process.stdout, 30, lambda: "")


def test_stop_kills_by_pid_and_leaves_nothing_running():
    process = child("input()")
    pid = process.pid
    _instance.stop(process)
    assert process.poll() is not None
    with pytest.raises(OSError):
        os.kill(pid, 0)
    assert pid not in _instance._running


def test_the_deployment_file_names_three_loopback_planes_at_port_zero(tmp_path):
    path = _instance.write_deployment(tmp_path, ["http://localhost:5173"])
    text = path.read_text()
    assert 'viewer = "127.0.0.1:0"' in text
    assert 'session = "127.0.0.1:0"' in text
    assert 'control = "127.0.0.1:0"' in text
    assert "[disclosure]\ntoken_max_lifetime = 3600" in text
    assert 'cors_origins = ["http://localhost:5173"]' in text
    assert 'module = "builtin:passthrough"' in text
    assert (tmp_path / ".tessera" / "cache").is_dir()


def test_the_secrets_are_generated_once_and_owner_only(tmp_path):
    session, identity = _instance.secrets_for(tmp_path)
    assert len(identity) == 32 and int(identity, 16) >= 0
    for name in ("session.cred", "operator.cred", "identity.key", "identity.toml"):
        assert oct((tmp_path / ".tessera" / name).stat().st_mode)[-3:] == "600"
    # The directory too, and each file is created owner-only rather than narrowed afterwards.
    assert oct((tmp_path / ".tessera").stat().st_mode)[-3:] == "700"
    again = _instance.secrets_for(tmp_path)
    assert again == (session, identity)


def test_the_binary_is_found_at_tessera_bin(tmp_path, monkeypatch):
    binary = tmp_path / "tessera"
    binary.write_text("")
    monkeypatch.setenv("TESSERA_BIN", str(binary))
    assert _instance.find_binary() == (str(binary), "TESSERA_BIN")
    monkeypatch.setenv("TESSERA_BIN", str(tmp_path / "absent"))
    with pytest.raises(Exception, match="does not exist"):
        _instance.find_binary()
