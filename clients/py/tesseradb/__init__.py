"""Tessera's Python package: read a Tessera database, map it in a notebook, and make one.

`pip install tesseradb` gives `connect`, `authorise`, `revoke` and `Token`, which need nothing
beyond the standard library. `Selection.sample` also needs pyarrow and `categories` needs pandas.
`pip install tesseradb[widget]` adds the notebook map, `Map`. `pip install tesseradb[local]` adds
`create` and `open`, which make a database in a directory from data frames and files.

`Map`, `create`, `open` and `Database` are loaded when first used, so `import tesseradb` works
without the optional packages.
"""

from __future__ import annotations

from ._auth import Token, authorise, revoke
from ._refusal import Refusal
from ._viewer import Sample, Selection, Viewer, connect

__all__ = [
    "Database",
    "Map",
    "Refusal",
    "Sample",
    "Selection",
    "Token",
    "Viewer",
    "authorise",
    "connect",
    "create",
    "open",
    "revoke",
    "__version__",
]
__version__ = "0.1.0"

_SDK = {"create": "create", "open": "open", "Database": "Database"}


def __getattr__(name: str):
    if name == "Map":
        try:
            from .widget import Map
        except ImportError as e:
            raise ImportError(
                "tesseradb.Map needs the widget extra: pip install 'tesseradb[widget]'"
            ) from e
        return Map
    if name in _SDK:
        try:
            from . import _database
        except ImportError as e:
            raise ImportError(
                "making a database needs the local extra: pip install 'tesseradb[local]'"
            ) from e
        return getattr(_database, _SDK[name])
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
