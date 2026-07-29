#!/usr/bin/env bash
# Sets up the independent Python differential oracle's virtualenv (Task 14).
# Deliberately isolated from the Rust workspace: Python here is a test-only consumer
# (never a component — see CLAUDE.md "Working method").
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../reference"
uv venv --python 3.12 .venv
uv pip install --python .venv/bin/python pyroaring pyarrow polars numpy pytest requests
