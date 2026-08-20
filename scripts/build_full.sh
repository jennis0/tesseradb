#!/usr/bin/env bash
# Build the 10^9-item bundle — the p99 measurement's fixture.
#
# No --limit: the full data/scaled/geometry.parquet / pairs/categories-subclass.pairs.parquet
# corpus, onto the identity extent (geometry.parquet stores Morton-derivable grid coordinates
# already quantised to the 0..65536 grid, and `input::read_points`'s Morton branch requires this
# exact extent).
#
# Disk is tight on the machine this was written for: check free space first and abort rather than
# run partway and fail with a half-written bundle. Post-r5 (morton u32) the bundle is ~45 GB, down
# from ~60 GB; this does not delete anything — if there isn't room, stop and let the operator
# decide what to clear.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-/tmp/tessera-1e9}"
shift || true
# Identity-key forwarding, not a default (plan N-1 — this script deliberately supplies NO
# default): the operator either exports TESSERA_IDENTITY_KEY or passes one of
# --carry-id-key-from <bundle> / --identity-file <path> / --mint-id-key as trailing arguments.
# This script's own refusal below is a convenience so an operator who forgets gets a one-line
# message rather than a build that dies after `df` and input reads; the binary's own N-1 refusal
# is the actual gate. There is no flag that takes a key: one on a command line reaches shell
# history, process listings and CI logs.
IDENTITY_ARGS=("$@")
if (( ${#IDENTITY_ARGS[@]} == 0 )) && [[ -z "${TESSERA_IDENTITY_KEY:-}" ]]; then
  echo "ERROR: no identity-key decision." >&2
  echo "Export TESSERA_IDENTITY_KEY, or pass one of --carry-id-key-from <bundle> /" >&2
  echo "--identity-file <path> / --mint-id-key as trailing arguments; this script deliberately" >&2
  echo "supplies NO default (plan N-1)." >&2
  exit 1
fi
MIN_FREE_GB=50

avail_kb=$(df --output=avail -k "$(dirname "$OUT")" 2>/dev/null | tail -1)
if [[ -z "$avail_kb" ]]; then
  avail_kb=$(df --output=avail -k "$ROOT" | tail -1)
fi
avail_gb=$((avail_kb / 1024 / 1024))
echo "Free space check: ${avail_gb} GiB available (need >= ${MIN_FREE_GB} GiB for the ~45 GB bundle)."
if (( avail_gb < MIN_FREE_GB )); then
  echo "ERROR: insufficient free space (${avail_gb} GiB < ${MIN_FREE_GB} GiB). Refusing to start the build." >&2
  echo "Delete scratch bundles under /tmp (never anything under data/) and re-run." >&2
  exit 1
fi

BIN="$ROOT/target/release/tessera"
if [[ ! -x "$BIN" ]]; then
  echo "Building release binary..."
  (cd "$ROOT" && cargo build --release -p tessera-cli)
fi

# The declaration this build compiles: one view over the scaled geometry, its points' labels in the
# exploded relation beside it, and the identity extent the Morton branch requires. No attributes —
# the geometry file carries none, so nothing declares a `[[attribute]]`. Written into
# `data/scaled/` rather than checked in because the paths `[sources]` writes sit there, and a path
# there is relative to the document declaring it (configuration.md §3).
CONFIG="$ROOT/data/scaled/build-full.config.toml"
cat > "$CONFIG" <<'TOML'
[sources]
geometry = "geometry.parquet"
labels   = "pairs/categories-subclass.pairs.parquet"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 65536.0 }
source           = "geometry"
point_visibility = { source = "labels", default = "public" }
TOML

# The deployment file both verbs read. Generated per machine and never committed, so its paths are
# absolute; `--out` still overrides `[bundle].path` for an operator who names one.
DEPLOYMENT="${TMPDIR:-/tmp}/tessera-build-full.tessera.toml"
cat > "$DEPLOYMENT" <<TOML
[bundle]
path  = "$OUT"
cache = "$OUT.cache"
wal   = "$OUT.wal"

[build]
schema = "$CONFIG"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
TOML

echo "Building the 10^9 bundle at $OUT (no --limit)..."
# --mint-external-ids keeps this bundle byte-comparable with the pre-flag 10^9 builds and the
# bench fixtures (memo 2026-07-30 §3.2 D1 — the default build is spec-conformant and mints none).
/usr/bin/time -v "$BIN" build \
  --deployment "$DEPLOYMENT" \
  --out "$OUT" \
  --mint-external-ids \
  "${IDENTITY_ARGS[@]}"
