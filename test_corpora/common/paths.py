"""Where the three kinds of file live, so that moving one is an environment variable.

**Staged** sources are the publisher's own bytes, read-only, on the share. **Derived** files are
what a `prepare.py` writes and what `tessera build` reads, and they are regenerable by definition.
**Declarations** live in git beside the script that produces their inputs, because the declaration
is the thing that gets reviewed.

The derived root is a variable rather than a path because the ladder's top two rungs do not fit on
this machine's root volume (`docs/evidence/memos/2026-08-27-ingest-campaign-plan.md` §4). When a
second volume appears, it is this one value that moves and not every script.

⊘ **Never build or serve against a bundle on the share.** It is SMB at ~67 MB/s, so a page fault is
a network round trip and any residency figure taken there measures the network. Staged sources are
read from it once; everything else is local.
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
    """One staged acquisition. Refuses a path that is not there rather than reading nothing."""
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
