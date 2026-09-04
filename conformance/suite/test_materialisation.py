"""The materialised corpus against its own declaration — one checkable invariant, and why it is
the one worth checking.

`Corpus::config_toml` is a **constant**: the declaration every build of this fixture reads names
a fixed set of sources, and it names them whether or not anything writes them. The shim in
[`verification.materialise_corpus`] is the other half of that pair — a hand-written list of
writer calls, in a different crate, maintained by a different hand. Nothing links the two, so the
declaration can grow a source and the shim not grow the call that writes it; the build is then
the first thing to notice, and what it says is that a file is missing, several hundred lines away
from the list that stopped matching.

That is not hypothetical. Five artifact sources were declared on 2026-08-21 and the shim was
never taught to write them, so `tessera build` exited 1 inside a module-scoped fixture and every
test that fixture feeds — including both of its negative controls — errored with a
`CalledProcessError` carrying no reason, for nine days.

**The invariant is that every source the declaration names is a file the materialiser wrote**,
and it is checkable because both sides are enumerable: the sources come out of the generated
`tessera.toml`'s `[sources]` table, and the files are whatever is on disk beside it. The check
costs one materialisation and no build, so it fails in seconds where the build failure took a
server spawn, and it fails naming the file rather than an exit code.
"""

from __future__ import annotations

import tomllib
from pathlib import Path

import pytest

from .verification import materialise_corpus

#: Small: this checks a file set, not any value in it, so the corpus only has to be large enough
#: that every declared relation has rows to write. The materialisation is the whole cost.
SEED = 20260830
N = 1024


@pytest.fixture(scope="module")
def materialised(tmp_path_factory) -> Path:
    out = tmp_path_factory.mktemp("materialisation") / "corpus"
    materialise_corpus(SEED, N, out)
    return out


def test_every_declared_source_is_a_file_the_shim_wrote(materialised: Path) -> None:
    """The module doc's invariant. A source that names no file is the shim and
    `Corpus::config_toml` having diverged, and the failure says so."""
    declaration = tomllib.loads((materialised / "config.toml").read_text())
    sources = declaration.get("sources", {})
    assert sources, "the declaration names no sources at all — it is not the corpus's"
    missing = sorted(
        f"{name} = {path!r}" for name, path in sources.items() if not (materialised / path).exists()
    )
    assert not missing, (
        "the materialised corpus is missing "
        + ("a source " if len(missing) == 1 else f"{len(missing)} sources ")
        + "its own declaration names:\n  - "
        + "\n  - ".join(missing)
        + "\n`Corpus::config_toml` and `verification._SHIM_MAIN` have diverged: the declaration "
        "names a file no writer call in the shim produces. Add the writer to the shim (the "
        "artifact sources are one call, `Corpus::write_artifact_fixtures`) rather than removing "
        "the source. Left unfixed this surfaces as `tessera build` exiting 1 inside a "
        "module-scoped fixture, with the reason swallowed by `capture_output`."
    )


def test_the_declaration_names_the_sources_this_fixture_is_built_from(materialised: Path) -> None:
    """A guard on the check above, not a second copy of it: the test would pass vacuously against
    a declaration that had lost its `[sources]` table down to `points` alone, so the two relations
    every other module in this package rests on are named here as well."""
    sources = tomllib.loads((materialised / "config.toml").read_text()).get("sources", {})
    assert {"points", "pairs"} <= set(sources)
