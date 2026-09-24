#!/usr/bin/env bash
# The documentation checks, as the `docs` job in .github/workflows/ci.yml runs them: the TypeScript
# and components reference generated from the client sources, the site's strict build, the
# register, the links in the site's pages, and the Python blocks in the tutorials and guides.
#
# The tools come from .venv-docs when it exists, and from PATH otherwise. To make it:
#   python3 -m venv .venv-docs && .venv-docs/bin/pip install -r docs/requirements.txt -c docs/constraints.txt
# The reference needs Node and the client workspace: npm --prefix clients/ts ci
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -x .venv-docs/bin/mkdocs ]; then
  PATH="$PWD/.venv-docs/bin:$PATH"
fi

if ! command -v node >/dev/null 2>&1; then
  echo "check-docs: node is not on PATH. The TypeScript reference is generated with it; install" >&2
  echo "  Node (>=20), then run: npm --prefix clients/ts ci" >&2
  exit 1
fi
# The generator's tools, which a node_modules installed before they were added lacks.
for tool in typedoc typedoc-plugin-markdown @custom-elements-manifest/analyzer; do
  if [ ! -d "clients/ts/node_modules/$tool" ]; then
    echo "check-docs: clients/ts/node_modules has no $tool, which generates the TypeScript" >&2
    echo "  reference. Install the client workspace's dependencies:" >&2
    echo "    npm --prefix clients/ts ci" >&2
    exit 1
  fi
done

# Every public export and every element member has a doc comment, and each element's comment names
# the events, slots, parts and tokens its code has. Then the pages under docs/reference/typescript/
# and docs/reference/components/, which the site's build includes.
npm --prefix clients/ts/components test --silent -- test/reference.test.ts
node clients/ts/scripts/reference.mjs

site=$(mktemp -d)
trap 'rm -rf "$site"' EXIT
mkdocs build --strict --site-dir "$site"

bash scripts/check-register.sh

# docs/system is left out until the citations in its Sources sections name files that exist, and
# docs/guides/views.md until it is rewritten, as check-register.sh leaves it out.
mapfile -t guides < <(find docs/guides -name '*.md' ! -path docs/guides/views.md | sort)
python3 scripts/check-doc-links.py --strict \
  README.md CLAUDE.md docs/index.md docs/start "${guides[@]}" docs/reference docs/developer docs/openapi

# The Python blocks in docs/start and docs/guides, and the harness's own tests.
(cd docs && python3 -m pytest -q)
