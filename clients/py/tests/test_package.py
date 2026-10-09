"""The package's own surface: what `import mosaica` gives, and what it says when it cannot.

`Map` and the database verbs are imported on first use. What is asserted here is that the names
`__all__` publishes resolve to the objects behind them, that a missing optional package is an
error naming what to install, and that pandas is never needed to import or use the package.
"""

from __future__ import annotations

import builtins
import sys

import pytest

import mosaica


def test_the_published_names_resolve_to_what_they_name():
    for name in mosaica.__all__:
        assert getattr(mosaica, name) is not None
    from mosaica import _database

    assert mosaica.create is _database.create
    assert mosaica.open is _database.open
    assert mosaica.Database is _database.Database


def test_a_name_the_package_does_not_carry_is_an_attribute_error():
    with pytest.raises(AttributeError):
        mosaica.nothing_of_that_name


def test_the_map_widget_is_imported_on_first_use():
    pytest.importorskip("anywidget")
    from mosaica.widget import Map

    assert mosaica.Map is Map


def hidden(monkeypatch, module: str) -> None:
    """Make `import <module>` fail, as it does where the extra is not installed."""
    for name in list(sys.modules):
        if name == module or name.startswith(module + "."):
            monkeypatch.delitem(sys.modules, name)
    for name in ("widget", "_database"):
        monkeypatch.delitem(sys.modules, f"mosaica.{name}", raising=False)
        # `from . import x` reads the attribute the first import left on the package.
        monkeypatch.delattr(mosaica, name, raising=False)
    imported = builtins.__import__

    def refuse(name, *rest):
        if name == module or name.startswith(module + "."):
            raise ImportError(f"no module named {name!r}")
        return imported(name, *rest)

    monkeypatch.setattr(builtins, "__import__", refuse)


def test_the_widget_without_anywidget_names_the_extra_to_install(monkeypatch):
    hidden(monkeypatch, "anywidget")
    with pytest.raises(ImportError) as why:
        mosaica.Map
    assert "mosaica[widget]" in str(why.value)


def test_the_sdk_without_pyarrow_names_what_to_install(monkeypatch):
    hidden(monkeypatch, "pyarrow")
    with pytest.raises(ImportError) as why:
        mosaica.create
    assert "pyarrow" in str(why.value)


def test_the_package_imports_and_loads_every_module_without_pandas(monkeypatch):
    hidden(monkeypatch, "pandas")
    for name in list(sys.modules):
        if name == "mosaica" or name.startswith("mosaica."):
            monkeypatch.delitem(sys.modules, name)
    import mosaica as fresh

    assert fresh.create and fresh.open and fresh.connect and fresh.Selection
    pytest.importorskip("anywidget")
    assert fresh.Map
