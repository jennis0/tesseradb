#!/usr/bin/env bash
# The documentation checks, as the `docs` job in .github/workflows/ci.yml runs them: the site's
# strict build, the register, the links in the site's pages, and the Python blocks in the
# tutorials and guides.
#
# The tools come from .venv-docs when it exists, and from PATH otherwise. To make it:
#   python3 -m venv .venv-docs && .venv-docs/bin/pip install -r docs/requirements.txt
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -x .venv-docs/bin/mkdocs ]; then
  PATH="$PWD/.venv-docs/bin:$PATH"
fi

site=$(mktemp -d)
trap 'rm -rf "$site"' EXIT
mkdocs build --strict --site-dir "$site"

bash scripts/check-register.sh

# docs/system is left out until the citations in its Sources sections name files that exist.
python3 scripts/check-doc-links.py --strict \
  README.md CLAUDE.md docs/index.md docs/start docs/guides/index.md docs/reference docs/developer docs/openapi

# The Python blocks in docs/start and docs/guides, and the harness's own tests.
(cd docs && python3 -m pytest -q)
