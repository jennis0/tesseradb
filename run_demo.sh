#!/usr/bin/env bash
#
# Bring up the local demo: `tessera serve` over a bundle with an attribute tail, plus the viewer.
#
# There is no hosted demo and nothing here deploys anywhere (README: "Deploying it → not yet").
# This is the localhost development shape, and two things about it are development-only: the
# session credential reaches the browser bundle, and `serve.dev_cors_origins` is typed in to let it
# talk. Neither is anything to copy into an integration — see client-interaction §7.
#
#   ./run_demo.sh                 # build the demo fixture if needed, serve it, open the viewer
#   ./run_demo.sh --wide          # the 19-column tail instead: every declared type, 5x the bytes
#   ./run_demo.sh --bundle PATH   # serve a bundle you already have
#   ./run_demo.sh --no-viewer     # server only (for curl, the golden capture, the smoke script)
#   ./run_demo.sh --rebuild       # discard and rebuild the demo bundle
#
# ## Watching the replica work
#
# The viewer holds what it has fetched, and buys a margin beyond the screen while the view is
# still. Four rows in the **Last request** panel report it: `tiles from cache` (of the region the
# view wanted, how much needed no request), `replica held`, `prefetched ahead`, and
# `— of which provisional` (marks borrowed from another zoom level while this one loads — drawn
# faded, and deliberately carrying no counts).
#
# `http://localhost:5173/?prefetch=0` turns look-ahead off and leaves the cache on. That is the A/B
# the measurements use: the cache decides what a request is *answered from*, look-ahead decides
# what is *asked for*, and they are worth judging separately.
#
# Three things that will otherwise waste your time:
#
#   - **Zoom in before panning.** At zoom 0 the whole world is on screen and the view box clamps,
#     so panning changes nothing and every gesture is answered without a request. Nothing is wrong;
#     there is just nothing to fetch.
#   - **Switch off the default principal.** It is the narrow one — 1,366 visible items — which
#     saturates at almost any depth, so the budget never binds and the cache has little to do. Pick
#     `everything` or a broad term to see it work.
#   - **Let the depth budget settle.** Marks-per-tile calibrates over the first few interactions
#     and only ever goes deeper; while it is moving, each view lands at a depth nothing is held at,
#     so the cache reads cold for reasons that have nothing to do with the cache.
#
# `clients/ts/viewer/smoke-cache.mjs` and `smoke-lookahead.mjs` do all of the above headlessly and
# print the numbers, against a server this script has already started.
#
# **Colour needs a bundle with a schema**, so this cannot default to `data/bench-fixtures/2m4`:
# that carries no attribute tail and would leave the colour-by control with nothing to offer. It
# builds from `data/scaled/attrs/schema.toml` — two categories and three numerics, which is every
# encoding the viewer has.
#
# **`--wide` is for coverage, not for looking at.** Its nineteen columns exist to exercise the whole
# declared type set, and they cost what that implies. Measured on this corpus, same request, same
# served set of ~1.02e6 marks:
#
#     columns   response    server time
#     0         17.2 MB      77- 111 ms
#     5         34.2 MB     221- 266 ms
#     19        80.3 MB     543-1054 ms
#
# The tail is the whole difference: 16.8 B/point with no columns, 78.4 B/point with nineteen. Every
# declared column is sent on every response — `POST /v1/viewport` has no column projection — so a
# viewer colouring by one column still pays for all of them.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"
REPO="$PWD"

# `data/` is gitignored, so it exists in the primary checkout and not in a worktree. Point
# TESSERA_DATA at wherever the fixtures actually live.
DATA="${TESSERA_DATA:-$REPO/data}"
DEV="$REPO/clients/ts/.dev"                     # gitignored: cache, WAL, and the built bundle
BUNDLE=""   # set below, from the chosen schema
CONFIG="$REPO/clients/ts/dev-server.toml"       # gitignored
ENV_LOCAL="$REPO/clients/ts/viewer/.env.local"  # gitignored

VIEWER_ADDR=127.0.0.1:37585
SESSION_ADDR=127.0.0.1:49303
CONTROL_ADDR=127.0.0.1:45721
# Vite is `strictPort`, and this origin is what gets written into `dev_cors_origins`. A silent
# fallback to 5174 would surface as a CORS failure that reads like a broken server.
VITE_PORT=5173

export TESSERA_SESSION_CRED="${TESSERA_SESSION_CRED:-dev-session-credential}"
export TESSERA_OPERATOR_CRED="${TESSERA_OPERATOR_CRED:-dev-operator-credential}"

bundle_override=""
run_viewer=1
rebuild=0
wide=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bundle)     bundle_override="$2"; shift 2 ;;
    --no-viewer)  run_viewer=0; shift ;;
    --rebuild)    rebuild=1; shift ;;
    --wide)       wide=1; shift ;;
    -h|--help)    sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)            echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# The lean schema is the default because it is the one worth looking at; see the header's table.
if [[ $wide -eq 1 ]]; then
  SCHEMA_NAME=schema-wide.toml; POINTS_NAME=points-wide.parquet; BUNDLE="$DEV/bundle-wide"
else
  SCHEMA_NAME=schema.toml;      POINTS_NAME=points.parquet;      BUNDLE="$DEV/bundle-lean"
fi

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

# One trap for both children. Without it a Ctrl-C in the foreground viewer leaves `tessera serve`
# holding the WAL and the listeners, and the next run fails on a bound port rather than on the
# thing that actually went wrong.
SERVE_PID=""
cleanup() {
  [[ -n "$SERVE_PID" ]] && kill "$SERVE_PID" 2>/dev/null || true
  wait "$SERVE_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

say "building the release binary"
cargo build --release -p tessera-cli

if [[ -n "$bundle_override" ]]; then
  BUNDLE="$bundle_override"
  [[ -d "$BUNDLE" ]] || { echo "no such bundle: $BUNDLE" >&2; exit 1; }
  echo "serving $BUNDLE"
  if [[ ! -s "$BUNDLE/v00000/MANIFEST.json" ]] || ! grep -q '"declared_scalars": *\[[^]]' "$BUNDLE"/v*/MANIFEST.json 2>/dev/null; then
    echo "note: this bundle declares no per-item columns, so the colour-by control will be empty."
  fi
else
  [[ $rebuild -eq 1 ]] && rm -rf "$BUNDLE"
  if [[ -d "$BUNDLE" ]]; then
    say "reusing the demo bundle at $BUNDLE (--rebuild to discard it)"
  else
    ATTRS="$DATA/scaled/attrs"
    PAIRS="$DATA/scaled/pairs/categories-subclass.pairs.parquet"
    for f in "$ATTRS/$POINTS_NAME" "$ATTRS/$SCHEMA_NAME" \
             "$ATTRS/archive.parquet" "$ATTRS/primary_category.parquet" "$PAIRS"; do
      [[ -f "$f" ]] || {
        echo "missing fixture: $f" >&2
        echo "set TESSERA_DATA to the checkout holding data/ (it is gitignored, so a worktree has none)," >&2
        echo "or pass --bundle PATH to serve a bundle you already have." >&2
        exit 1
      }
    done

    say "building the demo bundle (2.4M items, $([[ $wide -eq 1 ]] && echo 19 || echo 5) declared columns)"
    mkdir -p "$DEV"
    # `--limit` matches the points file: the pairs file covers a larger corpus, and an unlimited
    # build refuses rather than silently dropping the entities it cannot place.
    # `--mint-id-key` starts a throwaway identity lineage, which is right for a demo bundle and
    # wrong for anything else — every `tessera_id` it mints is meaningless outside this directory.
    ./target/release/tessera build \
      --points "$ATTRS/$POINTS_NAME" \
      --pairs  "$PAIRS" \
      --schema "$ATTRS/$SCHEMA_NAME" \
      --values "archive=$ATTRS/archive.parquet" \
      --values "primary_category=$ATTRS/primary_category.parquet" \
      --out "$BUNDLE" --limit 2422486 \
      --extent 0,65536,0,65536 --slice s0 \
      --mint-external-ids --mint-id-key
  fi
fi

say "writing $CONFIG"
mkdir -p "$DEV"
cat > "$CONFIG" <<EOF
[bundle]
path = "$BUNDLE"
cache = "$DEV/cache"
wal = "$DEV/wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
min_visible_members = 10
token_max_lifetime = 3600

[serve]
viewer = "$VIEWER_ADDR"
session = "$SESSION_ADDR"
control = "$CONTROL_ADDR"
max_k = 5000
session_credential_env = "TESSERA_SESSION_CRED"
operator_credential_env = "TESSERA_OPERATOR_CRED"
dev_cors_origins = ["http://localhost:$VITE_PORT"]
EOF

cat > "$ENV_LOCAL" <<EOF
VITE_TESSERA_VIEWER_URL=http://$VIEWER_ADDR
VITE_TESSERA_SESSION_URL=http://$SESSION_ADDR
VITE_TESSERA_SESSION_CREDENTIAL=$TESSERA_SESSION_CRED
EOF

say "starting tessera serve"
./target/release/tessera serve -c "$CONFIG" &
SERVE_PID=$!

# Poll `/readyz` rather than sleeping: it is 2.3's verified + pinned + plugin-loaded + workers-ready
# gate, so a 200 means the bundle actually opened. A fixed sleep would race the mmap and the
# fragment cache on a cold start and report a broken demo.
printf 'waiting for readyz'
for _ in $(seq 1 120); do
  if curl -sf "http://$VIEWER_ADDR/readyz" >/dev/null 2>&1; then ready=1; break; fi
  kill -0 "$SERVE_PID" 2>/dev/null || { echo; echo "tessera serve exited during startup" >&2; exit 1; }
  printf '.'; sleep 1
done
echo
[[ "${ready:-}" == 1 ]] || { echo "server did not become ready" >&2; exit 1; }
echo "ready — viewer plane on http://$VIEWER_ADDR"

if [[ $run_viewer -eq 0 ]]; then
  say "server only; Ctrl-C to stop"
  wait "$SERVE_PID"
  exit 0
fi

cd "$REPO/clients/ts"
[[ -d node_modules ]] || { say "npm ci"; npm ci; }

# The term dictionary is per bundle, so presets measured against one are meaningless against
# another. This REWRITES a tracked file; `git checkout -- clients/ts/viewer/presets.json` puts the
# committed one back.
say "measuring principals (rewrites the tracked clients/ts/viewer/presets.json)"
node scripts/measure-principals.mjs \
  --viewer "http://$VIEWER_ADDR" --session "http://$SESSION_ADDR" --terms 0..200

say "viewer on http://localhost:$VITE_PORT — Ctrl-C to stop both"
echo "Pick a column in the Colour panel: primary_category for the palette, submitted_at for the"
echo "ramp. --wide adds every other declared type, including a per_viewer category (refused:"
echo "its gate is specified and not built)."
echo
echo "To watch the replica: pick a broad principal, zoom in a few notches, then pan away and back."
echo "The return trip should need no request — see 'tiles from cache' under Last request."
echo "http://localhost:$VITE_PORT/?prefetch=0 turns look-ahead off, cache still on, for comparison."
npm run dev -w @tessera/viewer
