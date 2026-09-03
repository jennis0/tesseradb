#!/usr/bin/env python3
"""Two bundle trees, file by file, with the manifest's clock excused.

    compare_bundles.py A B

Every file under both roots must be present in both and byte-identical, except
`MANIFEST.json`, which is compared as JSON with `created_at` removed at every depth —
that is the one field a rebuild is allowed to move — and `CURRENT`, which carries that
manifest's digest and therefore moves with it. `CURRENT` is excused only when the manifest
beside it differed by nothing else; a `CURRENT` that differs on its own is a real
difference. Anything else that differs is printed with the first offset at which it does,
and the exit status is 1.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def strip_clock(node):
    if isinstance(node, dict):
        return {k: strip_clock(v) for k, v in node.items() if k != "created_at"}
    if isinstance(node, list):
        return [strip_clock(v) for v in node]
    return node


def files(root: Path) -> dict[str, Path]:
    return {str(p.relative_to(root)): p for p in sorted(root.rglob("*")) if p.is_file()}


def main() -> int:
    a_root, b_root = Path(sys.argv[1]), Path(sys.argv[2])
    a, b = files(a_root), files(b_root)
    bad = 0
    # `CURRENT` is the digest of the manifest beside it, so it is judged after the manifest
    # it names: sorting puts every `MANIFEST.json` before the `CURRENT` at the root.
    clock_only = set()
    for name in sorted(set(a) | set(b), key=lambda n: (Path(n).name == "CURRENT", n)):
        if name not in a or name not in b:
            print(f"only in {'A' if name in a else 'B'}: {name}")
            bad += 1
            continue
        x, y = a[name].read_bytes(), b[name].read_bytes()
        if x == y:
            continue
        if Path(name).name == "MANIFEST.json":
            if strip_clock(json.loads(x)) == strip_clock(json.loads(y)):
                print(f"manifest differs only in created_at: {name}")
                clock_only.add(Path(name).parent.name)
                continue
        if Path(name).name == "CURRENT" and json.loads(x).get("prefix") in clock_only:
            print(f"CURRENT carries the digest of a manifest that moved only its clock: {name}")
            continue
        at = next((i for i in range(min(len(x), len(y))) if x[i] != y[i]), min(len(x), len(y)))
        print(f"DIFFERS: {name} ({len(x)} vs {len(y)} bytes, first at {at})")
        bad += 1
    print(f"{len(set(a) | set(b))} files compared, {bad} differ")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
