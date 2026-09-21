#!/usr/bin/env bash
# The Python package's half of the gate: `tesseradb`'s tests, and the wheel's build hook run for
# real — a wheel built from this checkout, with the components' bundle inside it.
#
# Why the hook is exercised here rather than trusted: the hook is the only thing standing between
# `pip install tesseradb[widget]` and a widget with no JavaScript in it, and a hook that fails
# silently (npm absent, a dist path renamed, hatchling dropping the untracked file) produces a wheel
# that installs cleanly and raises at the first `Map()`. So this builds the wheel and reads its
# listing. It needs Python >= 3.10, `uv` or `python3 -m venv`, Node for the hook — the same Node
# `check-clients.sh` already needs — and a cargo toolchain for the companion wheel below.
#
# The demo notebooks are in the pytest step: `tests/test_sdk_examples.py` executes the cells of
# `examples/notebook_marimo.py` against a real build and a real server, and checks the Jupyter twin
# beside it, so the walk a reader is pointed at cannot drift from the package. It skips, naming
# what is missing, where `data/notebook/` or the `tessera` binary is absent.
#
# The venv is `clients/py/.venv` (gitignored); it is made on the first run and reused after. Set
# TESSERADB_CHECK_FRESH=1 to rebuild it.
set -euo pipefail

cd "$(dirname "$0")"

if [ -n "${TESSERADB_CHECK_FRESH:-}" ]; then rm -rf .venv; fi

py=.venv/bin/python
if [ ! -x "$py" ]; then
  echo "check-python: making clients/py/.venv"
  if command -v uv >/dev/null 2>&1; then
    uv venv -q .venv
  else
    python3 -m venv .venv
  fi
fi

install() {
  if command -v uv >/dev/null 2>&1; then
    uv pip install -q --python "$py" "$@"
  else
    "$py" -m pip install -q "$@"
  fi
}

# `tesseradb` depends on the companion platform wheel, `tesseradb-native`, which carries the
# `tessera` binary and the `_tessera` extension module. It is not published, so from a checkout it
# comes from beside this package — and its own hook runs cargo, which is why this script is no
# longer cargo-free. An editable install leaves both artifacts in `clients/py-native`'s source
# tree, where the package finds them.
echo "check-python: installing tesseradb-native editable (runs cargo)"
install -e ../py-native

# An editable install runs the build hook (hatchling builds an editable as a wheel), so this is
# the first run of the hook: `tesseradb/static/tessera-components.js` exists after it or the
# install failed. `hatchling` and `build` are the wheel step's tools.
echo "check-python: installing tesseradb[dev] editable (runs the build hook)"
install -e '.[dev]' hatchling build ruff
if [ ! -s tesseradb/static/tessera-components.js ]; then
  echo "check-python: the build hook left no bundle at tesseradb/static/tessera-components.js" >&2
  exit 1
fi

echo "check-python: ruff"
"$py" -m ruff check .

echo "check-python: pytest"
"$py" -m pytest -q

# The wheel itself, and its listing: the bundle must be inside it and must be the one just built.
echo "check-python: building the wheel"
rm -rf dist
"$py" -m build --wheel --no-isolation --outdir dist . >/dev/null
wheel=$(ls dist/tesseradb-*.whl)
if ! "$py" - "$wheel" <<'PY'
import sys, zipfile, hashlib, pathlib
wheel = sys.argv[1]
with zipfile.ZipFile(wheel) as z:
    names = z.namelist()
    want = "tesseradb/static/tessera-components.js"
    if want not in names:
        print(f"check-python: {wheel} does not contain {want}", file=sys.stderr)
        sys.exit(1)
    packaged = z.read(want)
    built = pathlib.Path("../ts/components/dist/tessera-components.js").read_bytes()
    if hashlib.sha384(packaged).digest() != hashlib.sha384(built).digest():
        print("check-python: the wheel's bundle differs from the components' dist", file=sys.stderr)
        sys.exit(1)
    print(f"check-python: {pathlib.Path(wheel).name} holds {want} ({len(packaged)} bytes)")
PY
then exit 1; fi

echo "check-python: ok"
