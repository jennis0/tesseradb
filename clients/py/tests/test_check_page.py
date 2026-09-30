"""One check, one page, whichever half of the SDK ran it.

The declaration check runs in this process through the `_tessera` extension module where it is
installed, and through `tessera check` where it is not. Both render the page in `tessera-build`,
so the bytes cannot depend on which one a machine has. A test with either half missing is skipped
rather than passed.
"""

import subprocess
from pathlib import Path

import pytest

from conftest import binary
from tesseradb import _instance

#: A declaration that names no file: legal, and the normal state for a corpus written through the
#: service. It needs nothing staged, so the two paths can be compared anywhere.
DECLARED_AND_EMPTY = """
[[view]]
name             = "s0"
extent           = { min = -25.0, max = 25.0 }
point_visibility = { default = "public" }

[[vocabulary]]
name       = "severity"
visibility = "public"
value_set  = "closed"
width      = "u8"
values     = ["low", "high"]

[[attribute]]
name       = "severity"
type       = "category"
vocabulary = "severity"
"""

#: The same, naming a Parquet file that is not there: the page with findings on it.
NAMES_A_MISSING_FILE = """
[sources]
points = "points.parquet"

[[view]]
name             = "s0"
extent           = { min = -25.0, max = 25.0 }
source           = "points"
point_visibility = { default = "public" }
"""


def deployment(directory: Path, declaration: str) -> str:
    """A deployment file over `declaration`, written under `directory`."""
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "schema.toml").write_text(declaration, encoding="utf-8")
    (directory / "tessera.toml").write_text(
        "[bundle]\n"
        'path = "bundle"\ncache = "cache"\nwal = "wal.log"\n\n'
        '[build]\nschema = "schema.toml"\n\n'
        '[plugin]\nmodule = "builtin:passthrough"\n\n'
        "[disclosure]\ntoken_max_lifetime = 3600\n",
        encoding="utf-8",
    )
    return str(directory / "tessera.toml")


@pytest.mark.parametrize(
    "declaration,clean",
    [(DECLARED_AND_EMPTY, True), (NAMES_A_MISSING_FILE, False)],
    ids=["clean", "with findings"],
)
def test_the_extension_module_and_the_binary_print_one_page(tmp_path, declaration, clean):
    extension = _instance.find_extension()
    if extension is None:
        pytest.skip("no _tessera extension module: only one of the two paths is here")
    tessera = binary()
    path = deployment(tmp_path / "db", declaration)

    done = subprocess.run(
        [tessera, "check", "--deployment", path], capture_output=True, text=True
    )
    assert (done.returncode == 0) is clean, done.stdout + done.stderr

    result = extension.check(path)
    assert result.ok is clean
    assert result.page == done.stdout + done.stderr
