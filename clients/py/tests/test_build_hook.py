"""The wheel's build hook, with npm faked: what it runs, when it runs `npm ci`, what it copies."""

import importlib.util
import pathlib
import subprocess

import pytest

HOOK = pathlib.Path(__file__).resolve().parents[1] / "hatch_build.py"
spec = importlib.util.spec_from_file_location("hatch_build", HOOK)
hatch_build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hatch_build)


def workspace(tmp_path, *, installed: bool, lock_newer: bool = False):
    ts = tmp_path / "ts"
    (ts / "components" / "dist").mkdir(parents=True)
    (ts / "package.json").write_text("{}")
    (ts / "package-lock.json").write_text("{}")
    if installed:
        stamp = ts / "node_modules" / ".package-lock.json"
        stamp.parent.mkdir()
        stamp.write_text("{}")
        if lock_newer:
            import os
            os.utime(stamp, (1, 1))
    return ts


def fake_run(ts):
    ran = []

    def run(cmd, cwd, check):
        ran.append(cmd[1:])
        if cmd[1] == "run":
            (ts / "components" / "dist" / "tessera-components.js").write_text("bundle")
            (ts / "components" / "dist" / "tessera-components.js.sri").write_text("sha384-x")
        return subprocess.CompletedProcess(cmd, 0)

    return ran, run


def test_builds_and_copies_the_bundle_skipping_npm_ci_when_current(tmp_path, monkeypatch):
    ts = workspace(tmp_path, installed=True)
    ran, run = fake_run(ts)
    monkeypatch.setattr(hatch_build.shutil, "which", lambda name: "/usr/bin/npm")
    static = tmp_path / "py" / "tesseradb" / "static"
    copied = hatch_build.build_bundle(ts, static, run=run, log=lambda m: None)
    assert ran == [["run", "build", "-w", "@tesseradb/components"]]
    assert [p.name for p in copied] == ["tessera-components.js", "tessera-components.js.sri"]
    assert (static / "tessera-components.js").read_text() == "bundle"


def test_runs_npm_ci_when_node_modules_is_absent_or_stale(tmp_path, monkeypatch):
    monkeypatch.setattr(hatch_build.shutil, "which", lambda name: "/usr/bin/npm")
    for kw in ({"installed": False}, {"installed": True, "lock_newer": True}):
        ts = workspace(tmp_path / str(kw), **kw)
        ran, run = fake_run(ts)
        hatch_build.build_bundle(ts, tmp_path / "static", run=run, log=lambda m: None)
        assert ran[0] == ["ci"]


def test_no_npm_is_a_named_refusal(tmp_path, monkeypatch):
    monkeypatch.setattr(hatch_build.shutil, "which", lambda name: None)
    with pytest.raises(RuntimeError, match="npm is not on PATH"):
        hatch_build.build_bundle(workspace(tmp_path, installed=True), tmp_path / "s", log=lambda m: None)


def test_a_build_that_produced_nothing_is_refused(tmp_path, monkeypatch):
    monkeypatch.setattr(hatch_build.shutil, "which", lambda name: "/usr/bin/npm")
    ts = workspace(tmp_path, installed=True)
    with pytest.raises(RuntimeError, match="produced no"):
        hatch_build.build_bundle(ts, tmp_path / "s", run=lambda *a, **k: None, log=lambda m: None)
