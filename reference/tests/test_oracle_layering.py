"""Two properties of `oracle/` that are claimed in prose and are cheap to check.

Both were prose-only until a review pointed out that prose does not fail. Neither is about the
engine; both are about the oracle staying the kind of thing a differential can be trusted to.
"""

from __future__ import annotations

import ast
import pathlib
import sys
import tomllib

ORACLE = pathlib.Path(__file__).resolve().parents[1] / "oracle"
PYPROJECT = ORACLE.parent / "pyproject.toml"

# `oracle/__init__.py` divides the package into three. The definitional group is the one a
# differential's independence rests on: it must reach nothing that spawns, drives or mutates the
# system under test.
DEFINITIONAL = ("viewport", "mask", "morton", "identity", "bundle", "wire")
DRIVERS = ("harness", "journal")
FIXTURE_BUILDERS = ("catalogue", "canary_fixture")

# Distribution name -> the name it is imported as, where they differ.
IMPORT_NAME = {"pyroaring": "pyroaring"}
STDLIB = set(sys.stdlib_module_names)


def _imports(path: pathlib.Path) -> set[str]:
    """Top-level module names imported by `path`, including inside functions."""
    tree = ast.parse(path.read_text())
    names: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            names.update(alias.name.split(".")[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom):
            if node.level:  # a relative import: `.bundle`, `. import morton`
                if node.module:
                    names.add("." + node.module.split(".")[0])
                names.update("." + alias.name for alias in node.names)
            elif node.module:
                names.add(node.module.split(".")[0])
    return names


def test_the_definitional_modules_reach_no_driver():
    """The group that *is* the oracle must not import the group that mutates the system.

    `oracle/__init__.py` argues that the three kinds of module here need not be three packages,
    because the boundary worth enforcing is a property rather than a directory. This is that
    property. A definitional module that reached `harness` or `journal` would mean the second
    implementation of record had acquired an opinion about the thing it is measuring — and it is
    the kind of import that arrives one convenience at a time.
    """
    forbidden = {f".{name}" for name in DRIVERS} | {f".{name}" for name in FIXTURE_BUILDERS}
    for module in DEFINITIONAL:
        reached = _imports(ORACLE / f"{module}.py") & forbidden
        assert not reached, (
            f"oracle/{module}.py imports {sorted(reached)}, which spawns or mutates the system "
            "under test. See oracle/__init__.py: the definitional group is what the differential's "
            "independence rests on."
        )


def test_every_third_party_import_is_a_declared_dependency():
    """`reference/.venv` must be reproducible from `pyproject.toml` alone.

    This is a standing track — the suite outlives the machine it was written on — and the package
    declared **no** dependencies at all while importing four. The failure mode is not subtle (a
    fresh clone collects zero tests) but it is silent on any machine that already has them, which
    is every machine anyone develops on.
    """
    declared = tomllib.loads(PYPROJECT.read_text())["project"]["dependencies"]
    declared_imports = {IMPORT_NAME.get(name, name) for name in declared}

    used: set[str] = set()
    for path in sorted(ORACLE.glob("*.py")):
        used |= {name for name in _imports(path) if not name.startswith(".")}
    third_party = {name for name in used if name not in STDLIB}

    missing = third_party - declared_imports
    assert not missing, (
        f"oracle/ imports {sorted(missing)}, which pyproject.toml does not declare — "
        "`reference/.venv` is not reproducible from a fresh clone"
    )
