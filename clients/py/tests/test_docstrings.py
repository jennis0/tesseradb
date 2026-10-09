"""Every name the package exports, and every type its calls return, is documented.

The Python reference under `docs/reference/python/` is rendered from the docstrings, and it leaves
out a member with no docstring of its own. So each exported name, each public method and property
of an exported class, and the same of every class one of those returns, carries a docstring of its
own, and the reference renders each of them. A class's public attributes, whether set in
`__init__`, declared as dataclass fields or declared in the class body, are named in the docstring
of the class that defines them, which the reference renders with the class.
"""

from __future__ import annotations

import ast
import dataclasses
import inspect
import itertools
import re
import textwrap
import typing
from pathlib import Path

import pytest

import mosaica

#: The reference pages, which a checkout carries and an sdist does not.
REFERENCE = Path(__file__).resolve().parents[3] / "docs" / "reference" / "python"

#: Attributes left out on purpose. `Database.blocks` is public today, and whether it stays public
#: is awaiting a decision, so it is neither required nor refused a place in the docs.
UNDECIDED = {"mosaica._database.Database.blocks"}


def type_checking_names() -> dict:
    """The names the package imports for annotations only, which a return type may use."""
    return {"Map": mosaica.Map}


def in_package(cls: type) -> bool:
    return cls.__module__.startswith("mosaica")


def members(cls: type) -> dict:
    """The public methods and properties of `cls` that the package defines, by name."""
    found = {}
    for name in dir(cls):
        if name.startswith("_"):
            continue
        owner = next((klass for klass in cls.__mro__ if name in vars(klass)), None)
        if owner is None or not in_package(owner):
            continue
        value = vars(owner)[name]
        if isinstance(value, property):
            found[name] = value.fget
        elif isinstance(value, (staticmethod, classmethod)):
            found[name] = value.__func__
        elif inspect.isfunction(value):
            found[name] = value
    return found


def assigned_in_init(cls: type) -> set:
    """The public names `cls.__init__` assigns on `self`, where the package writes it. A
    dataclass's generated `__init__` has no source, and its fields are read from annotations."""
    init = vars(cls).get("__init__")
    if not inspect.isfunction(init) or init.__code__.co_filename.startswith("<"):
        return set()
    tree = ast.parse(textwrap.dedent(inspect.getsource(init)))
    names = set()
    for node in ast.walk(tree):
        targets = node.targets if isinstance(node, ast.Assign) else [getattr(node, "target", None)]
        for target in targets:
            if (
                isinstance(target, ast.Attribute)
                and isinstance(target.value, ast.Name)
                and target.value.id == "self"
                and not target.attr.startswith("_")
            ):
                names.add(target.attr)
    return names


def attributes(cls: type) -> dict:
    """The public attributes of `cls`, each with the package class that defines it."""
    found = {}
    for owner in reversed(cls.__mro__):
        if not in_package(owner):
            continue
        names = assigned_in_init(owner)
        annotated = vars(owner).get("__annotations__", {})
        names |= {name for name in annotated if not name.startswith("_")}
        names |= {
            name
            for name, value in vars(owner).items()
            if not name.startswith("_")
            and not inspect.isfunction(value)
            and not isinstance(value, (property, staticmethod, classmethod))
        }
        for name in names:
            if name not in members(cls):
                found[name] = owner
    return found


def package_classes(hint) -> list:
    """The package's classes a type annotation names, however deeply nested."""
    found = [hint] if inspect.isclass(hint) and in_package(hint) else []
    for argument in typing.get_args(hint):
        found += package_classes(argument)
    return found


def returned(function) -> list:
    """The package's classes a function's return annotation names."""
    hints = typing.get_type_hints(function, localns=type_checking_names())
    return package_classes(hints.get("return"))


def walk() -> tuple[dict, dict]:
    """What the reference documents. The exported names, the public methods and properties of
    each class among them, and every class those return or a returned dataclass holds, by path;
    and each public attribute of those classes, by path, with the class that defines it."""
    queue = [getattr(mosaica, name) for name in mosaica.__all__ if name != "__version__"]
    found: dict = {}
    fields: dict = {}
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
        for name, owner in attributes(thing).items():
            fields[f"{path}.{name}"] = owner
    return found, {path: owner for path, owner in fields.items() if path not in UNDECIDED}


def has_docstring(thing) -> bool:
    """Whether `thing` has a docstring of its own, which a dataclass writes for itself if not."""
    text = thing.__doc__
    if dataclasses.is_dataclass(thing) and text and text.startswith(f"{thing.__name__}("):
        return False
    return bool(text and text.strip())


def rendered() -> dict:
    """Each `::: path` directive on the reference pages, with the members it renders: `None` for
    all of them, or the set of names its `members:` option lists."""
    found = {}
    for page in sorted(REFERENCE.glob("*.md")):
        lines = page.read_text(encoding="utf-8").splitlines()
        for at, line in enumerate(lines):
            if not line.startswith("::: "):
                continue
            chosen = None
            for option in itertools.takewhile(lambda one: one.startswith(" "), lines[at + 1 :]):
                if not option.strip().startswith("members:"):
                    continue
                value = option.split(":", 1)[1].strip()
                if value == "false":
                    chosen = set()
                    continue
                listed = re.fullmatch(r"\[(.*)\]", value)
                assert listed, f"{page.name}: read `members: {value}` as false or [a, b]"
                chosen = {name.strip() for name in listed.group(1).split(",") if name.strip()}
            found[line[4:].strip()] = chosen
    return found


def test_every_exported_name_and_returned_type_has_a_docstring():
    pytest.importorskip("anywidget")
    found, _ = walk()
    missing = sorted(path for path, thing in found.items() if not has_docstring(thing))
    assert not missing, f"no docstring of their own, so the reference leaves them out: {missing}"


def test_every_public_attribute_is_named_in_its_class_docstring():
    pytest.importorskip("anywidget")
    _, fields = walk()
    missing = sorted(
        path
        for path, owner in fields.items()
        if f"`{path.rpartition('.')[2]}`" not in (owner.__doc__ or "")
    )
    assert not missing, f"named in no docstring of the class that defines them: {missing}"


def test_the_reference_renders_every_exported_name_and_returned_type():
    if not REFERENCE.is_dir():
        pytest.skip(f"{REFERENCE} is not in this checkout")
    pytest.importorskip("anywidget")
    directives = rendered()
    found, fields = walk()

    def shown(path: str) -> bool:
        parent, _, name = path.rpartition(".")
        chosen = directives.get(parent, set())
        return path in directives or chosen is None or name in chosen

    missing = sorted(path for path in found if not shown(path))
    owners = {f"{owner.__module__}.{owner.__qualname__}" for owner in fields.values()}
    missing += sorted(owner for owner in owners if owner not in directives)
    assert not missing, f"no `::: path` in {REFERENCE} renders these: {missing}"
