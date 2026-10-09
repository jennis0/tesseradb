"""The two compiled artifacts `mosaica` reaches for, and where they are.

`mosaica` is pure Python and depends on this distribution under a platform marker, so
`pip install mosaica` gets the binary by default and an unsupported platform still installs
pure. `pip install mosaica --no-deps` opts out, exactly: the base install has no other
dependency.

The extension is packaged as `mosaica_native._mosaica` rather than as a bare `_mosaica.abi3.so`
beside it, because a top-level underscore name would be claimed across the whole environment by
one distribution. Importing this package aliases it to the bare name the crate's `#[pymodule]`
function declares, so `import _mosaica` works after `import mosaica_native` and either spelling
finds the same module object.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

from . import _mosaica as _mosaica

sys.modules.setdefault("_mosaica", _mosaica)

_here = Path(__file__).resolve().parent
_binary = _here / ("mosaica.exe" if os.name == "nt" else "mosaica")


def binary_path() -> str:
    """The `mosaica` executable this wheel carries."""
    if not _binary.exists():
        raise FileNotFoundError(
            f"mosaica-native carries no executable at {_binary}. Reinstall it with "
            "`pip install --force-reinstall mosaica-native`"
        )
    return str(_binary)


__all__ = ["binary_path"]
