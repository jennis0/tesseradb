#!/usr/bin/env bash
# The TypeScript half of the gate: typecheck every package and the operator scripts, run the unit
# suites, run core's live test against a real `tessera serve`, then the components' browser suite
# in headless Chromium.
#
# The operator scripts are plain `.mjs` and are checked with `checkJs`, so a block-scoped variable
# used before its declaration (TS2448) fails here rather than at run time.
#
# The live test builds `data/notebook/` with the `tessera` binary. Where either is missing it skips
# each test and prints the reason, as the Python suite's server tests do.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v npm >/dev/null 2>&1; then
  echo "check-clients: npm is not on PATH. Install Node (>=20) and re-run, or say in your report" >&2
  echo "  that this check did not run." >&2
  exit 1
fi

if [ ! -d clients/ts/node_modules ]; then
  echo "check-clients: clients/ts/node_modules is absent. Run:" >&2
  echo "    npm --prefix clients/ts ci" >&2
  exit 1
fi

echo "check-clients: typechecking every package and the operator scripts"
npm --prefix clients/ts run typecheck --silent

echo "check-clients: running the client test suites"
npm --prefix clients/ts test --silent

echo "check-clients: running core's live test"
npm --prefix clients/ts/core run test:live --silent

echo "check-clients: running the components' browser suite"
if ! (cd clients/ts && node -e "require('playwright').chromium.launch().then((b) => b.close())" >/dev/null 2>&1); then
  echo "check-clients: Playwright cannot launch Chromium, which the components' browser suite" >&2
  echo "  needs. Install it with: (cd clients/ts && npx playwright install chromium)" >&2
  exit 1
fi
npm --prefix clients/ts/components run test:browser --silent

echo "check-clients: ok"
