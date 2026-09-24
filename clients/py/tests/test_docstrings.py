"""Every name the package exports, and every type its calls return, is documented.

The Python reference under `docs/reference/python/` is rendered from the docstrings, and it leaves
out a member with no docstring of its own. So each exported name, each public method and property
of an exported class, and the same of every class one of those returns, carries a docstring of its
own, and the reference renders each of them.
"""

from __future__ import annotations

import dataclasses
import inspect
import itertools
import re
import typing
from pathlib import Path

import pytest

import tesseradb

#: The reference pages, which a checkout carries and an sdist does not.
REFERENCE = Path(__file__).resolve().parents[3] / "docs" / "reference" / "python"


def type_checking_names() -> dict:
    """The names the package imports for annotations only, which a return type may use."""
    return {"Map": tesseradb.Map}


def members(cls: type) -> dict:
    """The public methods and properties of `cls` that the package defines, by name."""
    found = {}
    for name in dir(cls):
        if name.startswith("_"):
            continue
        owner = next((klass for klass in cls.__mro__ if name in vars(klass)), None)
        if owner is None or not owner.__module__.startswith("tesseradb"):
            continue
        value = vars(owner)[name]
        if isinstance(value, property):
            found[name] = value.fget
        elif isinstance(value, (staticmethod, classmethod)):
            found[name] = value.__func__
        elif inspect.isfunction(value):
            found[name] = value
    return found


def package_classes(hint) -> list:
    """The package's classes a type annotation names, however deeply nested."""
    found = [hint] if inspect.isclass(hint) and hint.__module__.startswith("tesseradb") else []
    for argument in typing.get_args(hint):
        found += package_classes(argument)
    return found


def returned(function) -> list:
    """The package's classes a function's return annotation names."""
    hints = typing.get_type_hints(function, localns=type_checking_names())
    return package_classes(hints.get("return"))


def documented() -> dict:
    """Everything the reference documents, by its path: the exported names, the public members
    of each class among them, and every class those return or a returned dataclass holds."""
    queue = [getattr(tesseradb, name) for name in tesseradb.__all__ if name != "__version__"]
    found: dict = {}
    while queue:
        thing = queue.pop()
        path = f"{thing.__module__}.{thing.__qualname__}"
        if path in found:
            continue
        found[path] = thing
        if not inspect.isclass(thing):
            queue += returned(thing)
            continue
        if dataclasses.is_dataclass(thing):
            for hint in typing.get_type_hints(thing).values():
                queue += package_classes(hint)
        for name, member in members(thing).items():
            found[f"{path}.{name}"] = member
            queue += returned(member)
    return found


def has_docstring(thing) -> bool:
    """Whether `thing` has a docstring of its own, which a dataclass writes for itself if not."""
    text = thing.__doc__
    if dataclasses.is_dataclass(thing) and text and text.startswith(f"{thing.__name__}("):
        return False
    return bool(text and text.strip())


def rendered() -> dict:
    """Each `::: path` directive on the reference pages, and whether it renders the members."""
    found = {}
    for page in sorted(REFERENCE.glob("*.md")):
        lines = page.read_text(encoding="utf-8").splitlines()
        for at, line in enumerate(lines):
            if not line.startswith("::: "):
                continue
            options = itertools.takewhile(lambda option: option.startswith(" "), lines[at + 1 :])
            alone = any(re.fullmatch(r"\s*members:\s*(false|\[\])\s*", one) for one in options)
            found[line[4:].strip()] = not alone
    return found


def test_every_exported_name_and_returned_type_has_a_docstring():
    pytest.importorskip("anywidget")
    missing = sorted(path for path, thing in documented().items() if not has_docstring(thing))
    assert not missing, f"no docstring of their own, so the reference leaves them out: {missing}"


def test_the_reference_renders_every_exported_name_and_returned_type():
    if not REFERENCE.is_dir():
        pytest.skip(f"{REFERENCE} is not in this checkout")
    pytest.importorskip("anywidget")
    directives = rendered()

    def shown(path: str) -> bool:
        return path in directives or directives.get(path.rpartition(".")[0], False)

    missing = sorted(path for path in documented() if not shown(path))
    assert not missing, f"no `::: path` in {REFERENCE} renders these: {missing}"
