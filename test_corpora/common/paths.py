"""Where the three kinds of file live, so moving one is an environment variable.

Staged sources are the publisher's own bytes, read-only, on the share. Derived files are what a
`prepare.py` writes and what `tessera build` reads, and are regenerable. Declarations live in
git beside the script that produces their inputs.

Never build or serve a bundle from the share: it is SMB, so a page fault is a network round
trip. Staged sources are read from it once; everything else is local.
"""

from __future__ import annotations

import os
from pathlib import Path

#: The publisher's own bytes, `<dataset>/<vintage>/`, read-only.
STAGED_ROOT = Path(os.environ.get("TESSERA_STAGED", "/mnt/nas/joe/tessera/datasets"))

_REPO_ROOT = Path(__file__).resolve().parents[2]

#: Everything a `prepare.py` writes, and every bundle built from it.
LADDER_ROOT = Path(os.environ.get("TESSERA_LADDER", _REPO_ROOT / "data" / "ladder"))


def staged(dataset: str, vintage: str) -> Path:
    """One staged acquisition. Raises if the vintage is not present."""
    path = STAGED_ROOT / dataset / vintage
    if not path.is_dir():
        available = (
            sorted(p.name for p in (STAGED_ROOT / dataset).iterdir())
            if (STAGED_ROOT / dataset).is_dir()
            else []
        )
        raise FileNotFoundError(
            f"no staged {dataset} at vintage {vintage!r} under {STAGED_ROOT}"
            + (f"; vintages present: {', '.join(available)}" if available else "")
        )
    return path


def ladder(rung: str) -> Path:
    """This rung's derived directory, created if absent."""
    path = LADDER_ROOT / rung
    path.mkdir(parents=True, exist_ok=True)
    return path
