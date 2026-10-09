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
OUT="${1:-/tmp/mosaica-1e9}"
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

BIN="$ROOT/target/release/mosaica"
if [[ ! -x "$BIN" ]]; then
  echo "Building release binary..."
  (cd "$ROOT" && cargo build --release -p mosaica-cli)
fi

# The declaration this build compiles: one view over the scaled geometry, its points' labels in the
# exploded relation beside it, and the identity extent the Morton branch requires. One attribute,
# the unique `id` the geometry and the relation name each point by; the geometry file carries no
# other. Written into `data/scaled/` rather than checked in because the paths `[sources]` writes
# sit there, and a path there is relative to the document declaring it (configuration.md §3).
CONFIG="$ROOT/data/scaled/build-full.config.toml"
cat > "$CONFIG" <<'TOML'
[sources]
geometry = "geometry.parquet"
labels   = "pairs/categories-subclass.pairs.parquet"

[defaults]
source     = "geometry"

# The dense `entity_id` the geometry and every label relation name each point by.
[[attribute]]
name   = "id"
type   = "u32"
unique = true
field  = "entity_id"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 65536.0 }
source           = "geometry"
point_visibility = { source = "labels", default = "public" }
TOML

# The deployment file both verbs read. Generated per machine and never committed, so its paths are
# absolute; `--out` still overrides `[bundle].path` for an operator who names one.
DEPLOYMENT="${TMPDIR:-/tmp}/mosaica-build-full.mosaica.toml"
cat > "$DEPLOYMENT" <<TOML
[bundle]
path  = "$OUT"
cache = "$OUT.cache"
wal   = "$OUT.wal"

[build]
schema = "$CONFIG"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
TOML

echo "Building the 10^9 bundle at $OUT (no --limit)..."
/usr/bin/time -v "$BIN" build \
  --deployment "$DEPLOYMENT" \
  --out "$OUT"
