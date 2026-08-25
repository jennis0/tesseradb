"""The wheel's build hook: build the components' single-file bundle and package it.

Design client-components §7 and decision 0095: ``pip install tesseradb[widget]`` from PyPI needs
no Node, because the bundle is built **here**, at wheel-build time, and shipped inside the wheel
under ``tesseradb/static/``. Nothing built is committed — 1.7 MB would otherwise land in git on
every change to the components — so a developer installing from a checkout needs Node, and this
hook is what runs it.

What it runs, in ``clients/ts``: ``npm ci`` when ``node_modules`` is absent or older than the
lockfile, then ``npm run build -w @tesseradb/components``. ``npm ci`` is skipped when the install
is current because it deletes and recreates ``node_modules`` every time — tens of seconds and
hundreds of megabytes — and the gate runs this hook on every pass; the lockfile's mtime against
``node_modules/.package-lock.json`` is npm's own currency test.

The hook runs for the wheel target only, editable installs included (hatchling builds those as
wheels), so ``pip install -e .[widget]`` from the checkout leaves the bundle in the source tree's
``tesseradb/static/`` where the package finds it. An sdist carries no bundle; building a wheel
from one needs the TypeScript workspace beside it, which an sdist does not contain — install the
wheel, or build from the repository.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

BUNDLE = "tessera-components.js"
FILES = (BUNDLE, BUNDLE + ".sri")


def npm_install_is_current(ts: Path) -> bool:
    stamp = ts / "node_modules" / ".package-lock.json"
    lock = ts / "package-lock.json"
    return stamp.exists() and lock.exists() and stamp.stat().st_mtime >= lock.stat().st_mtime


def build_bundle(ts: Path, static: Path, *, run=subprocess.run, log=print) -> list[Path]:
    """Build the bundle in the workspace at ``ts`` and copy it into ``static``; returns what was copied."""
    npm = shutil.which("npm")
    if npm is None:
        raise RuntimeError(
            "tesseradb[widget]'s bundle is built with Node at wheel-build time and npm is not on "
            "PATH. Install Node (>= 20) to build from this checkout, or install the published wheel."
        )
    if not npm_install_is_current(ts):
        log(f"tesseradb: npm ci in {ts}")
        run([npm, "ci"], cwd=ts, check=True)
    log(f"tesseradb: npm run build -w @tesseradb/components in {ts}")
    run([npm, "run", "build", "-w", "@tesseradb/components"], cwd=ts, check=True)
    dist = ts / "components" / "dist"
    static.mkdir(parents=True, exist_ok=True)
    copied = []
    for name in FILES:
        src = dist / name
        if not src.exists():
            raise RuntimeError(f"the components build produced no {src}")
        shutil.copyfile(src, static / name)
        copied.append(static / name)
    return copied


class BundleHook(BuildHookInterface):
    PLUGIN_NAME = "custom"

    def initialize(self, version: str, build_data: dict) -> None:
        if self.target_name != "wheel":
            return
        root = Path(self.root)
        ts = root.parent / "ts"
        static = root / "tesseradb" / "static"
        if not (ts / "package.json").exists():
            raise RuntimeError(
                f"no TypeScript workspace at {ts}: the wheel is built from the repository checkout, "
                "where clients/ts sits beside clients/py; an sdist cannot build the bundle."
            )
        copied = build_bundle(ts, static, log=lambda m: print(m, file=sys.stderr))
        for path in copied:
            rel = path.relative_to(root)
            build_data.setdefault("force_include", {})[os.fspath(path)] = os.fspath(rel)
        build_data.setdefault("artifacts", []).extend(os.fspath(p.relative_to(root)) for p in copied)
