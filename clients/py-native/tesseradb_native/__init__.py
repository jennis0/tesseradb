"""The two compiled artifacts `tesseradb` reaches for, and where they are.

`tesseradb` is pure Python and depends on this distribution under a platform marker, so
`pip install tesseradb` gets the binary by default and an unsupported platform still installs
pure. `pip install tesseradb --no-deps` opts out, exactly: the base install has no other
dependency.

The extension is packaged as `tesseradb_native._tessera` rather than as a bare `_tessera.abi3.so`
beside it, because a top-level underscore name would be claimed across the whole environment by
one distribution. Importing this package aliases it to the bare name the crate's `#[pymodule]`
function declares, so `import _tessera` works after `import tesseradb_native` and either spelling
finds the same module object.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

from . import _tessera as _tessera

sys.modules.setdefault("_tessera", _tessera)

_here = Path(__file__).resolve().parent
_binary = _here / ("tessera.exe" if os.name == "nt" else "tessera")


def binary_path() -> str:
    """The `tessera` executable this wheel carries."""
    if not _binary.exists():
        raise FileNotFoundError(
            f"tesseradb-native carries no executable at {_binary}. Reinstall it with "
            "`pip install --force-reinstall tesseradb-native`"
        )
    return str(_binary)


__all__ = ["binary_path"]
