#!/usr/bin/env python3
"""The release version: `[workspace.package] version` in the root Cargo.toml, and every copy of it.

    python3 scripts/version.py              print the version
    python3 scripts/version.py set 0.2.0    write 0.2.0 into Cargo.toml and every copy
    python3 scripts/version.py check        fail when a copy differs from Cargo.toml
    python3 scripts/version.py check --tag v0.2.0
                                            also fail when the tag is not v<version> or
                                            CHANGELOG.md has no section for the version
    python3 scripts/version.py notes        print CHANGELOG.md's section for the version

The copies are the workspace crates' exact pins in Cargo.toml and their entries in Cargo.lock, both
pyprojects and the `mosaica-native==` pin, `mosaica.__version__`, and the four published npm
packages, their dependencies on one another and their entries in package-lock.json.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$")
NPM_PACKAGES = {
    "core": "@mosaicajs/client",
    "deck": "@mosaicajs/deck",
    "components": "@mosaicajs/components",
    "react": "@mosaicajs/react",
}
TS = ROOT / "clients" / "ts"


def read(rel: str) -> str:
    return (ROOT / rel).read_text()


def workspace_version() -> str:
    section = re.search(r"^\[workspace\.package\]\n(.*?)(?=^\[)", read("Cargo.toml"), re.M | re.S)
    found = section and re.search(r'^version = "([^"]+)"$', section.group(1), re.M)
    if not found:
        sys.exit('version.py: Cargo.toml has no `version = "..."` under [workspace.package].')
    return found.group(1)


# Each text copy: the file, a pattern whose group 1 is the version, and how many matches there
# must be (None for one or more).
TEXT_COPIES = [
    ("Cargo.toml", r'^version = "([^"]+)"$', 1),
    ("Cargo.toml", r'^mosaica-[a-z-]+ = \{ path = "crates/[a-z-]+", version = "=([^"]+)" \}$', None),
    ("Cargo.lock", r'^name = "mosaica(?:-[a-z-]+)?"\nversion = "([^"]+)"$', None),
    ("clients/py/pyproject.toml", r'^version = "([^"]+)"$', 1),
    ("clients/py/pyproject.toml", r'"mosaica-native==([^;"]+);', 1),
    ("clients/py-native/pyproject.toml", r'^version = "([^"]+)"$', 1),
    ("clients/py/mosaica/__init__.py", r'^__version__ = "([^"]+)"$', 1),
]


def text_copies() -> list[tuple[str, str]]:
    """(where, version) for every text copy."""
    out = []
    for rel, pattern, count in TEXT_COPIES:
        matches = list(re.finditer(pattern, read(rel), re.M))
        if not matches or (count is not None and len(matches) != count):
            sys.exit(f"version.py: {rel} has {len(matches)} matches for {pattern}; expected "
                     f"{count or 'one or more'}. Fix the file or the pattern in this script.")
        for m in matches:
            line = read(rel).count("\n", 0, m.start(1)) + 1
            out.append((f"{rel}:{line}", m.group(1)))
    return out


def npm_specs(manifest: dict):
    """(section, name, spec) for each dependency of `manifest` on a published package."""
    for section in ("dependencies", "peerDependencies", "devDependencies"):
        for name, spec in manifest.get(section, {}).items():
            if name in NPM_PACKAGES.values():
                yield section, name, spec


def npm_copies() -> list[tuple[str, str]]:
    out = []
    lock = json.loads((TS / "package-lock.json").read_text())
    for directory, name in NPM_PACKAGES.items():
        manifest = json.loads((TS / directory / "package.json").read_text())
        for where, entry in ((f"clients/ts/{directory}/package.json", manifest),
                             (f"clients/ts/package-lock.json [{directory}]",
                              lock["packages"][directory])):
            out.append((f"{where} version", entry.get("version", "")))
            for section, dep, spec in npm_specs(entry):
                out.append((f"{where} {section}.{dep}", spec.lstrip("^~")))
    return out


def check(tag: str | None) -> int:
    version = workspace_version()
    wrong = [(where, found) for where, found in text_copies() + npm_copies() if found != version]
    for where, found in wrong:
        print(f"version.py: {where} says {found}; Cargo.toml says {version}.", file=sys.stderr)
    if wrong:
        print(f"version.py: run `python3 scripts/version.py set {version}` to write the version "
              "into every copy.", file=sys.stderr)
    failed = bool(wrong)
    if tag is not None:
        if tag != f"v{version}":
            print(f"version.py: the tag is {tag} and Cargo.toml says {version}; tag v{version} or "
                  "set the version to match the tag.", file=sys.stderr)
            failed = True
        if notes(version) is None:
            print(f"version.py: CHANGELOG.md has no `## [{version}]` section; add one before "
                  "tagging.", file=sys.stderr)
            failed = True
    if not failed:
        print(f"version.py: every copy says {version}")
    return 1 if failed else 0


def set_version(version: str) -> None:
    if not SEMVER.match(version):
        sys.exit(f"version.py: {version} is not a semantic version such as 0.2.0 or 0.2.0-rc.1.")
    for rel, pattern, _ in TEXT_COPIES:
        if rel == "Cargo.lock":
            continue
        path = ROOT / rel
        text = path.read_text()
        path.write_text(re.sub(
            pattern, lambda m: m.group(0)[: m.start(1) - m.start(0)] + version
            + m.group(0)[m.end(1) - m.start(0):], text, flags=re.M))

    def rewrite(entry: dict) -> None:
        if "version" in entry:
            entry["version"] = version
        for section, dep, spec in list(npm_specs(entry)):
            entry[section][dep] = spec[: len(spec) - len(spec.lstrip("^~"))] + version

    for directory in NPM_PACKAGES:
        path = TS / directory / "package.json"
        manifest = json.loads(path.read_text())
        rewrite(manifest)
        path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n")
    lock_path = TS / "package-lock.json"
    lock = json.loads(lock_path.read_text())
    for directory in NPM_PACKAGES:
        rewrite(lock["packages"][directory])
    lock_path.write_text(json.dumps(lock, indent=2, ensure_ascii=False) + "\n")

    # The workspace members' entries in Cargo.lock; nothing outside the workspace moves.
    subprocess.run(["cargo", "update", "--workspace", "--offline"], cwd=ROOT, check=True)
    check(None)


def notes(version: str) -> str | None:
    """CHANGELOG.md's section for `version`, without its heading, or None."""
    path = ROOT / "CHANGELOG.md"
    if not path.exists():
        return None
    m = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## |\Z)", path.read_text(),
                  re.M | re.S)
    return m.group(1).strip() + "\n" if m else None


def main(argv: list[str]) -> int:
    if not argv:
        print(workspace_version())
        return 0
    if argv[0] == "set" and len(argv) == 2:
        set_version(argv[1])
        return 0
    if argv[0] == "check" and len(argv) == 1:
        return check(None)
    if argv[0] == "check" and len(argv) == 3 and argv[1] == "--tag":
        return check(argv[2])
    if argv[0] == "notes" and len(argv) == 1:
        text = notes(workspace_version())
        if text is None:
            print(f"version.py: CHANGELOG.md has no `## [{workspace_version()}]` section.",
                  file=sys.stderr)
            return 1
        sys.stdout.write(text)
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
