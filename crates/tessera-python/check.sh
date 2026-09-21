#!/usr/bin/env bash
# The extension module's own gate: build it, import it, and run `tests/binding.py` against it.
#
# Separate from `cargo test` because a Python extension module has no Rust test target — the
# object is linked with libpython's symbols left undefined, which is what an interpreter resolves
# at import and what an executable cannot be linked with. So the module's behaviour is tested
# where it is used, from Python.
#
# No wheel and no packaging: the object is copied to `_tessera.so` in a scratch directory and put
# on `PYTHONPATH`. Needs Python >= 3.10 and a cargo toolchain, nothing else.
set -euo pipefail

cd "$(dirname "$0")/../.."

echo "check-python-binding: building"
cargo build -p tessera-python

lib=$(find "${CARGO_TARGET_DIR:-target}/debug" -maxdepth 1 -name 'lib_tessera.so' -o \
      -maxdepth 1 -name 'lib_tessera.dylib' | head -n 1)
if [ -z "$lib" ]; then
  echo "check-python-binding: no extension module was built" >&2
  exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$lib" "$stage/_tessera.so"

echo "check-python-binding: $(python3 -V), module $(du -h "$stage/_tessera.so" | cut -f1)"
PYTHONPATH="$stage" python3 -m unittest discover -s crates/tessera-python/tests -p 'binding.py' -v
