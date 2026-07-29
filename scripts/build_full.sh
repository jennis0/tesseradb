#!/usr/bin/env bash
# Task 16, Step 2: build the 10^9-item bundle — the exit-criteria measurement's fixture.
#
# No --limit: the full data/scaled/geometry.parquet / pairs/categories-subclass.pairs.parquet
# corpus, onto the identity extent (geometry.parquet stores Morton-derivable grid coordinates
# already quantised to the 0..65536 grid — Task 8's read_points Morton branch requires this exact
# extent, shared-context constraint 7).
#
# Disk is tight (shared-context / handover notes): check free space first and abort rather than
# run partway and fail with a half-written bundle. The bundle itself is ~60 GB; this does not
# delete anything — if there isn't room, stop and let the operator decide what to clear.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-/tmp/tessera-1e9}"
MIN_FREE_GB=65

avail_kb=$(df --output=avail -k "$(dirname "$OUT")" 2>/dev/null | tail -1)
if [[ -z "$avail_kb" ]]; then
  avail_kb=$(df --output=avail -k "$ROOT" | tail -1)
fi
avail_gb=$((avail_kb / 1024 / 1024))
echo "Free space check: ${avail_gb} GiB available (need >= ${MIN_FREE_GB} GiB for the ~60 GB bundle)."
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
/usr/bin/time -v "$BIN" build \
  --points "$ROOT/data/scaled/geometry.parquet" \
  --pairs "$ROOT/data/scaled/pairs/categories-subclass.pairs.parquet" \
  --out "$OUT" \
  --extent 0,65536,0,65536 \
  --slice s0
