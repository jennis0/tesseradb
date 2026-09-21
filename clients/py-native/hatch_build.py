"""The platform wheel's build hook: cargo builds both artifacts and they are copied into the package.

Two artifacts from two crates. `tessera-cli` produces the `tessera` executable; `tessera-python`
produces the `_tessera` extension module, whose cargo output is named `lib_tessera.so` (`.dylib`,
`.dll`) and must be renamed to the name an interpreter will import. `crates/tessera-python/check.sh`
does the same rename for its own tests.

Why hatchling and not maturin. Maturin builds one crate: either a cdylib as an extension module or
a bin as a script, not both, and its `include` copies data files without running cargo for them and
without a reliable executable bit. Either way the executable's cargo invocation would sit outside
maturin, at which point maturin is only writing the metadata — which hatchling does here, beside the
hook `clients/py/hatch_build.py` already establishes in this tree.

Cross-building, three environment variables, all unset for a host build:

- `TESSERA_NATIVE_TARGET` is passed to `--target`. It may carry cargo-zigbuild's glibc suffix
  (`x86_64-unknown-linux-gnu.2.28`), which names the version to link against but not the output
  directory, so the suffix is dropped when looking for the artifacts.
- `TESSERA_NATIVE_CARGO` is the cargo subcommand, `build` or `zigbuild`.
- `TESSERA_NATIVE_PLATFORM` is the wheel's platform tag, which a cross-build must give because the
  host's tag would be a lie. A host build derives it from `sysconfig`.

The binary is stripped by rustc, through `CARGO_PROFILE_RELEASE_STRIP`, rather than by a `strip` on
PATH — a cross-build has no stripper for the target it just produced. 45 MiB unstripped, 34 MiB
stripped, 12 MiB deflated in the wheel.
"""

from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import sysconfig
from pathlib import Path

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

# `abi3-py310` in tessera-python's Cargo.toml: one object serves every Python from 3.10 up.
ABI_TAG = "cp310-abi3"


def is_windows(target: str | None) -> bool:
    return "windows" in target if target else sys.platform == "win32"


def is_macos(target: str | None) -> bool:
    return "apple" in target if target else sys.platform == "darwin"


def artifacts(target: str | None) -> list[tuple[str, str]]:
    """The (cargo output name, name in the package) pairs for this target."""
    if is_windows(target):
        return [("tessera.exe", "tessera.exe"), ("_tessera.dll", "_tessera.pyd")]
    built = "lib_tessera.dylib" if is_macos(target) else "lib_tessera.so"
    return [("tessera", "tessera"), (built, "_tessera.abi3.so")]


def platform_tag() -> str:
    return sysconfig.get_platform().replace("-", "_").replace(".", "_")


def target_directory(repo: Path) -> Path:
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=repo, check=True, capture_output=True, text=True,
    )
    return Path(json.loads(out.stdout)["target_directory"])


def build(repo: Path, package: Path, target: str | None, *, run=subprocess.run, log=print) -> list[Path]:
    """Build both crates and copy their artifacts into ``package``; returns what was copied."""
    if shutil.which("cargo") is None:
        raise RuntimeError(
            "tesseradb-native carries two compiled artifacts and cargo is not on PATH. Install a "
            "Rust toolchain to build this wheel, or install the published one."
        )
    subcommand = os.environ.get("TESSERA_NATIVE_CARGO") or "build"
    command = ["cargo", subcommand, "--release", "-p", "tessera-cli", "-p", "tessera-python"]
    if target:
        command += ["--target", target]
    log(f"tesseradb-native: {' '.join(command)} in {repo}")
    run(command, cwd=repo, check=True, env={**os.environ, "CARGO_PROFILE_RELEASE_STRIP": "symbols"})

    out = target_directory(repo)
    out = out / target.split(".", 1)[0] / "release" if target else out / "release"
    package.mkdir(parents=True, exist_ok=True)
    copied = []
    for built, name in artifacts(target):
        src = out / built
        if not src.exists():
            raise RuntimeError(f"the cargo build produced no {src}")
        dst = package / name
        shutil.copyfile(src, dst)
        dst.chmod(dst.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        copied.append(dst)
    return copied


class NativeHook(BuildHookInterface):
    PLUGIN_NAME = "custom"

    def initialize(self, version: str, build_data: dict) -> None:
        if self.target_name != "wheel":
            return
        root = Path(self.root)
        repo = root.parent.parent
        if not (repo / "crates" / "tessera-python" / "Cargo.toml").exists():
            raise RuntimeError(
                f"no Rust workspace at {repo}: the wheel is built from the repository checkout, "
                "where clients/py-native sits two levels below the workspace root; an sdist cannot "
                "build the artifacts."
            )
        target = os.environ.get("TESSERA_NATIVE_TARGET") or None
        copied = build(
            repo, root / "tesseradb_native", target, log=lambda m: print(m, file=sys.stderr)
        )

        build_data["pure_python"] = False
        build_data["tag"] = f"{ABI_TAG}-{os.environ.get('TESSERA_NATIVE_PLATFORM') or platform_tag()}"
        for path in copied:
            rel = path.relative_to(root)
            build_data.setdefault("force_include", {})[os.fspath(path)] = os.fspath(rel)
        build_data.setdefault("artifacts", []).extend(os.fspath(p.relative_to(root)) for p in copied)
