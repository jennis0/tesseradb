"""``tesseradb`` — Tessera's Python package (decision 0095).

The base install is ``authorise`` and ``Token`` and depends on nothing outside the standard
library. ``pip install tesseradb[widget]`` adds anywidget and the notebook widget, ``Map``; the
SDK and the in-process instance join this package later.

``Map`` is imported on first use rather than at import time, so the base install does not need
anywidget to ``import tesseradb``.
"""

from __future__ import annotations

from ._auth import Token, authorise

__all__ = ["Map", "Token", "authorise", "__version__"]
__version__ = "0.1.0"


def __getattr__(name: str):
    if name == "Map":
        try:
            from .widget import Map
        except ImportError as e:  # anywidget absent
            raise ImportError(
                "tesseradb.Map needs the widget extra: pip install 'tesseradb[widget]'"
            ) from e
        return Map
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
