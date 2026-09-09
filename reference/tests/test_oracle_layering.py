"""Three properties of `oracle/` that are claimed in prose and are cheap to check.

All were prose-only until a review pointed out that prose does not fail. None is about the engine;
all are about the oracle staying the kind of thing a differential can be trusted to.

The third arrived after the first was found to be a hand-maintained allowlist: it named twelve
modules where the package held fourteen, and the two it omitted included `text`, which is both a
definitional value oracle and the one module that reaches a shipped binary. A check that skips the
module most able to break the property it enforces is a check that cannot fail. So the module set
is now **derived from the directory** and every file must be classified — a new module is a red
test until someone places it, rather than a silent exemption.
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
# system under test. `text` belongs to it — it holds no dictionary and no ordinals, and the
# `tessera tokenise` subprocess it makes is a declared echo (decision 0070) reached by a path it is
# handed, not an import of a driver.
DEFINITIONAL = (
    "viewport",
    "occupancy",
    "mask",
    "morton",
    "identity",
    "bundle",
    "wire",
    "filters",
    "record_blob",
    "text",
)
DRIVERS = ("harness", "journal")
FIXTURE_BUILDERS = ("catalogue", "canary_fixture", "label_fixture", "multiview")
CLASSIFIED = set(DEFINITIONAL) | set(DRIVERS) | set(FIXTURE_BUILDERS)

# Distribution name -> the name it is imported as, where they differ.
IMPORT_NAME = {"pyroaring": "pyroaring"}
STDLIB = set(sys.stdlib_module_names)


def _modules() -> set[str]:
    """Every module in `oracle/`, read from the directory rather than from a list."""
    return {path.stem for path in ORACLE.glob("*.py") if path.stem != "__init__"}


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


def test_every_module_is_classified():
    """No module may sit outside the three groups, because the check below iterates one of them.

    The failure this prevents is not a violation but an omission: a module absent from all three
    lists is checked by nothing, and the omission is invisible — the suite stays green and the
    package looks enforced. It had already happened to two of fourteen. Deriving the set from the
    directory makes adding a module a decision about which kind of thing it is, taken at the point
    the module is written, by the person who knows.
    """
    present = _modules()
    unclassified = present - CLASSIFIED
    assert not unclassified, (
        f"oracle/{{{','.join(sorted(unclassified))}}}.py is in no group, so the layering check "
        "below does not see it. Place each in DEFINITIONAL, DRIVERS or FIXTURE_BUILDERS — see "
        "oracle/__init__.py for what the three mean."
    )
    stale = CLASSIFIED - present
    assert not stale, (
        f"{sorted(stale)} is classified here but no longer exists in oracle/ — a name that "
        "matches no file enforces nothing"
    )


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
