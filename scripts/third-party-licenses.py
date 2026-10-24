#!/usr/bin/env python3
"""Write THIRD_PARTY_LICENSES: the licence notices of every third-party crate the workspace depends
on outside its tests, which the binary archives, the mosaica-native wheel and the container image
carry. Run it after a change to Cargo.lock.

    python3 scripts/third-party-licenses.py           write the file
    python3 scripts/third-party-licenses.py --check   fail when the committed file is stale

Needs cargo-about 0.9 (`cargo install --locked --features cli cargo-about`), configured by
`about.toml`. cargo-about reads each crate's own licence files (`--frozen` keeps it offline), so the
output depends on Cargo.lock alone. The workspace's own crates are left out: LICENSE covers them,
and listing them would put the release version into this file.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "THIRD_PARTY_LICENSES"

HEADER = """\
Third-party software in Mosaica

Every third-party crate the Mosaica workspace depends on outside its tests, which includes
everything compiled into the mosaica binary, the mosaica-native Python wheel and the container
image. Each licence text below lists the crates it covers, and the NOTICE files of the crates that
ship one follow the licence texts.
"""


def render() -> str:
    subprocess.run(["cargo", "fetch", "--locked"], cwd=ROOT, check=True)
    about = json.loads(subprocess.run(
        ["cargo", "about", "generate", "--workspace", "--frozen", "--fail", "--format", "json"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout)
    sections, counts = [], {}
    for licence in about["licenses"]:
        users = sorted({(u["crate"]["name"], u["crate"]["version"]) for u in licence["used_by"]
                        if not u["crate"]["name"].startswith("mosaica")})
        if not users:
            continue
        counts.setdefault(licence["name"], set()).update(name for name, _ in users)
        used_by = "".join(f"  {name} {version}\n" for name, version in users)
        sections.append(f"{'=' * 80}\n{licence['name']} ({licence['id']})\n\nUsed by:\n{used_by}\n"
                        f"{licence['text'].strip()}\n")
    # Apache-2.0 §4(d): a crate's NOTICE file travels with every redistributed copy. cargo-about
    # reads licence files only, so the NOTICE files are read from each crate's source here.
    notices = []
    for krate in sorted(about["crates"], key=lambda c: (c["package"]["name"], c["package"]["version"])):
        package = krate["package"]
        if package["name"].startswith("mosaica"):
            continue
        source = Path(package["manifest_path"]).parent
        for notice in sorted(source.iterdir()):
            if notice.is_file() and notice.name.upper().startswith("NOTICE"):
                notices.append(f"{'=' * 80}\nNOTICE of {package['name']} {package['version']}\n\n"
                               f"{notice.read_text(errors='replace').strip()}\n")
    overview = "".join(
        f"{name}: {len(crates)} crate{'' if len(crates) == 1 else 's'}\n"
        for name, crates in sorted(counts.items(), key=lambda kv: (-len(kv[1]), kv[0]))
    )
    # Some crates ship their licence with CRLF line endings; the file has LF throughout.
    return (f"{HEADER}\n{overview}\n" + "\n".join(sections + notices)).replace("\r\n", "\n")


def main(argv: list[str]) -> int:
    text = render()
    if argv == ["--check"]:
        if not OUT.exists() or OUT.read_bytes() != text.encode():
            print("third-party-licenses: THIRD_PARTY_LICENSES is stale. Run: "
                  "python3 scripts/third-party-licenses.py", file=sys.stderr)
            return 1
        print("third-party-licenses: THIRD_PARTY_LICENSES is current")
        return 0
    if argv:
        print(__doc__, file=sys.stderr)
        return 2
    OUT.write_bytes(text.encode())
    print(f"third-party-licenses: wrote {OUT.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
