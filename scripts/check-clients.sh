#!/usr/bin/env bash
# The TypeScript half of the gate.
#
# **Why this exists.** One rename — `ead7e90`, "a slice is a view, everywhere" — touched 21 files
# under `clients/` and shipped three defects: two distinct app-state fields collapsed onto one
# name, a `.slice()` call on a typed array renamed to `.view()`, and a `const view` shadowing a
# module-level one so every request threw before its initialiser ran. None was caught, because the
# gate was Rust and Python only, and each cost a separate investigation to find. A client that does
# not compile is not a smaller failure than a crate that does not compile.
#
# The operator scripts are plain `.mjs` and are checked with `checkJs`, not merely parsed: the third
# defect above is TS2448, "block-scoped variable used before its declaration", which a syntax check
# passes and a typecheck catches.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v npm >/dev/null 2>&1; then
  echo "check-clients: npm is not on PATH. The TypeScript client is part of the gate; install" >&2
  echo "  Node (>=20) and re-run, or run the other three gate commands and say in your report" >&2
  echo "  that this one did not run — a skipped check reported as a pass is how the three" >&2
  echo "  defects above reached main." >&2
  exit 1
fi

if [ ! -d clients/ts/node_modules ]; then
  echo "check-clients: clients/ts/node_modules is absent. Run:" >&2
  echo "    npm --prefix clients/ts ci" >&2
  exit 1
fi

echo "check-clients: typechecking core, viewer, spike and the operator scripts"
npm --prefix clients/ts run typecheck --silent

echo "check-clients: running the client test suites"
npm --prefix clients/ts test --silent

echo "check-clients: ok"
