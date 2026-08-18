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
# default): the operator must pass one of --carry-id-key-from <bundle> / --id-key-file <path> /
# --id-key <32 hex> / --mint-id-key as trailing arguments. This script's own refusal below is a
# convenience so an operator who forgets gets a one-line message rather than a build that dies
# after `df` and input reads; the binary's own N-1 refusal is the actual gate.
IDENTITY_ARGS=("$@")
if (( ${#IDENTITY_ARGS[@]} == 0 )); then
  echo "ERROR: no identity-key argument given." >&2
  echo "Pass one of --carry-id-key-from <bundle> / --id-key-file <path> / --id-key <32 hex> / --mint-id-key" >&2
  echo "as trailing arguments; this script deliberately supplies NO default (plan N-1)." >&2
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

echo "Building the 10^9 bundle at $OUT (no --limit)..."
# --mint-external-ids keeps this bundle byte-comparable with the pre-flag 10^9 builds and the
# bench fixtures (memo 2026-07-30 §3.2 D1 — the default build is spec-conformant and mints none).
/usr/bin/time -v "$BIN" build \
  --points "$ROOT/data/scaled/geometry.parquet" \
  --pairs "$ROOT/data/scaled/pairs/categories-subclass.pairs.parquet" \
  --out "$OUT" \
  --extent 0,65536,0,65536 \
  --view s0 \
  --mint-external-ids \
  "${IDENTITY_ARGS[@]}"
