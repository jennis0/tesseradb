"""The client reaches every viewer-plane operation the HTTP contract publishes.

The HTTP API comes first and the other three surfaces are built on it, so an operation the
contract carries and the client does not is a capability Python cannot reach at all. The contract
file is read here rather than a list kept beside it: a list would be the thing that goes stale.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from tesseradb import Selection, Viewer

CONTRACT = Path(__file__).resolve().parents[3] / "docs" / "openapi" / "tessera.yaml"


def viewer_operations(contract: Path) -> set[str]:
    """Every `operationId` the contract tags `viewer`."""
    operations = set()
    tagged = False
    for line in contract.read_text(encoding="utf-8").splitlines():
        if re.match(r"^ {2}/", line):
            tagged = False
        stripped = line.strip()
        if stripped.startswith("tags:"):
            tagged = "viewer" in stripped
        elif stripped.startswith("operationId:") and tagged:
            operations.add(stripped.split(":", 1)[1].strip())
    return operations


#: The operations Python reaches under another name: a selection's count and sample are the
#: viewport route, and a category listing given a prefix is the suggestion route.
REACHED_AS = {
    "viewport": [(Selection, "count"), (Selection, "sample")],
    "suggestCategoryValues": [(Viewer, "categories")],
}


def methods_of(operation: str) -> list[tuple[type, str]]:
    """Where Python reaches an `operationId`: by `REACHED_AS`, or as its snake-case `Viewer` method."""
    return REACHED_AS.get(
        operation, [(Viewer, re.sub(r"(?<!^)([A-Z])", r"_\1", operation).lower())]
    )


def test_the_contract_is_where_this_reads_it():
    """A parity test that found no operations would pass by finding nothing."""
    if not CONTRACT.exists():
        pytest.skip(f"{CONTRACT} is not in this checkout")
    assert "viewport" in viewer_operations(CONTRACT)


def test_every_viewer_plane_operation_is_reached_from_python():
    """Four surfaces, one set of core capabilities: what the contract serves, Python asks for."""
    if not CONTRACT.exists():
        pytest.skip(f"{CONTRACT} is not in this checkout")
    missing = sorted(
        f"{operation} ({owner.__name__}.{name})"
        for operation in viewer_operations(CONTRACT)
        for owner, name in methods_of(operation)
        if not callable(getattr(owner, name, None))
    )
    assert not missing, "the viewer plane serves these and Python has no method for them: " + (
        ", ".join(missing)
    )
