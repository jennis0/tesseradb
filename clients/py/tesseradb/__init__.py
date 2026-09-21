"""``tesseradb``: Tessera's Python package.

The base install is ``authorise``, ``Token`` and ``connect`` — a hosted deployment's ``meta()``
and ``item()`` — and depends on nothing outside the standard library; ``Viewer.viewport`` needs
pyarrow and ``Viewer.map`` needs anywidget, each imported where it is used.

``pip install tesseradb[widget]`` adds anywidget and the notebook widget, ``Map``;
``pip install tesseradb[local]`` adds the SDK, which creates a Tessera database in a directory,
fills it from frames and files and commits it through the ``tessera`` binary.

``Map`` and the SDK's verbs are imported on first use rather than at import time, so the base
install needs neither anywidget nor pyarrow to ``import tesseradb``.
"""

from __future__ import annotations

from ._auth import Token, authorise, revoke
from ._refusal import Refusal
from ._viewer import Viewer, connect

__all__ = [
    "Map",
    "Refusal",
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
        except ImportError as e:  # anywidget absent
            raise ImportError(
                "tesseradb.Map needs the widget extra: pip install 'tesseradb[widget]'"
            ) from e
        return Map
    if name in _SDK:
        try:
            from . import _database
        except ImportError as e:  # pyarrow absent
            raise ImportError(
                "the tesseradb SDK needs the local extra: pip install 'tesseradb[local]'"
            ) from e
        return getattr(_database, _SDK[name])
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
