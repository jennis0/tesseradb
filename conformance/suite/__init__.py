"""The correctness suite's shared foundation: the read battery and response canonicalisation.

This is build-order row 1 of `docs/design/correctness-suite.md` §14 — the one definition of *what
we ask* (§3's battery, `suite.battery`) and *what an answer is* (§12.2's canonical forms,
`suite.canonical`) that the suite's three mechanisms are specified to share. Stage invariance
compares recorded batteries, total verification checks their rows, and the census counts against
their tiling; three mechanisms sharing one definition cannot drift apart, and there is one thing
to extend when a surface is added.

⊘ Rows 2 onward are not built: no driver, no stages, no entitlements, no `/control/status`
barriers. What exists here today is the battery definition, the canonicalisation, a recorder that
issues a battery against a live server, and the tests that pin them — including the property the
whole suite rests on, that recording one battery twice against unchanged state compares equal.

The canary comparator (`conformance/tests/test_canary.py`) is refactored onto `suite.canonical`
rather than keeping its own copy. §12.2 is explicit about why there must be exactly one
implementation: a control that exercises a second code path proves that path instead — the failure
mode that comparator itself demonstrated one level up, when a control written beside an inline
comparison would have tested the copy rather than the comparator.
"""

from __future__ import annotations

import sys
from pathlib import Path

# `suite.canonical` decodes the viewport wire format with `oracle.wire` — one strict decoder for
# every Python reader, per that module's own doc — and `oracle` lives under `reference/`, a sibling
# tree. Inserted here as well as in `conformance/conftest.py` so `import suite` works on its own,
# not only under pytest.
_REFERENCE = Path(__file__).resolve().parents[2] / "reference"
if str(_REFERENCE) not in sys.path:
    sys.path.insert(0, str(_REFERENCE))

from .battery import (  # noqa: E402
    Absent,
    Battery,
    Categories,
    Item,
    Meta,
    Query,
    Recorded,
    Region,
    Viewport,
    build_battery,
    record,
    record_one,
)
from .canonical import Batches, Canonical, Json, Streamed, canonicalise_viewport  # noqa: E402

__all__ = [
    "Absent",
    "Batches",
    "Battery",
    "Canonical",
    "Categories",
    "Item",
    "Json",
    "Meta",
    "Query",
    "Recorded",
    "Region",
    "Streamed",
    "Viewport",
    "build_battery",
    "canonicalise_viewport",
    "record",
    "record_one",
]
