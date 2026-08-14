#!/usr/bin/env bash
#
# Bring up the local demo: `tessera serve` over one bundle per scale, plus the viewer.
#
# There is no hosted demo and nothing here deploys anywhere (README: "Deploying it → not yet").
# This is the localhost development shape, and two things about it are development-only: the
# session credential reaches the browser bundle, and `serve.dev_cors_origins` is typed in to let it
# talk. Neither is anything to copy into an integration — see client-interaction §7.
#
#   ./run_demo.sh                 # build what is missing, serve every scale, open the viewer
#   ./run_demo.sh --scale 2m4     # one scale only. Repeatable; the order is the picker's order
#   ./run_demo.sh --bundle PATH   # serve a bundle you already have, on its own
#   ./run_demo.sh --no-viewer     # servers only (for curl, the golden capture, the smoke script)
#   ./run_demo.sh --rebuild       # discard and rebuild the demo bundles
#
# ## The two scales, and why they differ
#
#     scale   items        prose indexed        bundle
#     2m4     2,422,486    title + abstract     ~1.4 GB
#     25m     25,200,000   title                ~3 GB
#
# Abstracts stop at the small scale because they are 954 characters against a title's 73: the record
# blob holding 25.2M of them is ~26 GB, and its index ~4 GB on top. The viewer draws its filter
# controls from `/v1/meta`, so the abstract box is simply absent on the large bundle — the honest
# rendering of a column that is not there, and not a case the client special-cases.
#
# Both are built from `data/demo/`, which `probes/build_demo_datasets.py` writes. Run that first if
# the directory is missing; it needs `probes/build_prose.py`'s output, which is a scan of the raw
# arXiv snapshot.
#
# ## Filtering
#
# The controls come from `/v1/meta`'s `filter_operands` — six columns on the small bundle:
# `archive` and `primary_category` (tick a value or several: any of them matches), `submitted_at`
# and `author_count` (a range), and `title` and `abstract` (words to match).
#
# **Watch `visible` against `matched` in the Counts panel while you filter.** `visible` is the mask —
# what this principal may see in the drawn region — and it must not move. `matched` is what the
# filter admits. A filter that moved `visible` would be reporting the size of a set the principal was
# never granted; that the two are separate numbers is the whole point of showing both.
#
# **Prose search is measured and the modes differ by an order of magnitude.** On the 2.4M bundle,
# against a ~660,000-item mask:
#
#     title, "all words"        3-8 ms
#     abstract, "all words"     2-4 ms
#     title, "exact phrase"     ~1,600 ms
#     abstract, "exact phrase"  ~900 ms
#
# Phrase pays for adjacency — it re-reads positions per candidate — so it is the mode to reach for
# deliberately rather than the one to leave selected. "all words" is the default for that reason.
#
# ## Watching the replica work
#
# The viewer holds what it has fetched, and buys a margin beyond the screen while the view is
# still. The **Last request** panel reports `tiles from cache` (of the region the view wanted, how
# much needed no request) and `replica held`; the drawer beneath it opens on `prefetched ahead`,
# `— of which provisional` (marks borrowed from another zoom level while this one loads — drawn like
# any other mark, and deliberately carrying no counts: the count channel is what stops a superset
# being read as density) and the server's stage breakdown.
#
# `http://localhost:5173/?prefetch=0` turns look-ahead off and leaves the cache on. That is the A/B
# the measurements use: the cache decides what a request is *answered from*, look-ahead decides
# what is *asked for*, and they are worth judging separately.
#
# `?dataset=25m` opens straight onto a scale, skipping the switch.
#
# ## Recording a session — `?trace=1`
#
# `http://localhost:5173/?trace=1` records what you did, what the client did about it, and how long
# each frame took, into a file you can hand to someone else. It exists because the browser-free
# harness (`probes/2026-08-09-client-pipeline/`) cannot see the three costs a user actually feels:
# GPU upload, frame scheduling, and the delay between an input and the paint answering it.
#
# A bar appears bottom-left. **Press `m`, or click `mark`, the moment something feels wrong** — the
# trace then carries your judgement rather than leaving it to be inferred from the numbers, and that
# marker is the first thing a reader looks for. Then `download trace`.
#
# What it records: frame gaps past 20 ms (smooth runs cost one counter, not one record per frame),
# long tasks, pointer and wheel input, every request and its arrival, splitting a response into
# bands, deriving a frame, the slab sync, and the gap from handing deck.gl its layers to the next
# frame — which is the upload. Plus a header naming the GPU: a trace whose renderer says SwiftShader
# was software-rasterised and its timings mean nothing.
#
# Off unless the parameter is present, down to not installing the observers — an instrument that
# runs inside the frame loop it is timing has to cost nothing when it is not wanted.
#
# Two debug knobs answer the one question the platform will not. `painted` — handing deck.gl its
# layers to the next frame — is the largest remaining cost, and deck's own `gpuTime` reads zero
# here because the GPU timer query extension is absent under ANGLE, so the GPU half cannot be
# measured directly. Measure it by difference, same gesture each time:
#
#   ?trace=1&radius=0.6     marks a third the area. If `painted` falls, the cost is fill rate.
#   ?trace=1&pickable=0     no per-instance picking colours. If `painted` falls, it is that buffer.
#
# Both change what is drawn or what can be clicked. They are instruments, not settings.
#
# Three things that will otherwise waste your time:
#
#   - **Zoom in before panning.** At zoom 0 the whole world is on screen and the view box clamps,
#     so panning changes nothing and every gesture is answered without a request. Nothing is wrong;
#     there is just nothing to fetch.
#   - **The defaults are now the hard case** — every term, the largest mark budget, coloured by
#     `archive` — because the narrow principal saturates at almost any depth and exercises none of
#     the machinery worth watching. Drop to a single term to see the *opposite* extreme; do not
#     expect the cache or the budget to do anything there.
#   - **Let the depth budget settle.** Marks-per-tile calibrates over the first few interactions
#     and only ever goes deeper; while it is moving, each view lands at a depth nothing is held at,
#     so the cache reads cold for reasons that have nothing to do with the cache.
#
# `clients/ts/viewer/smoke-cache.mjs` and `smoke-lookahead.mjs` do all of the above headlessly and
# print the numbers, against a server this script has already started.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"
REPO="$PWD"

# `data/` is gitignored, so it exists in the primary checkout and not in a worktree. Point
# TESSERA_DATA at wherever the fixtures actually live.
DATA="${TESSERA_DATA:-$REPO/data}"
DEV="$REPO/clients/ts/.dev"                     # gitignored: cache, WAL, and the built bundles
ENV_LOCAL="$REPO/clients/ts/viewer/.env.local"  # gitignored
# Served by Vite from `public/`, and gitignored: it names what is actually running, which is a fact
# about this invocation rather than about the repository. The viewer fetches it at startup, so
# restarting against a different set of scales needs no rebuild — and, unlike the `presets.json` it
# replaces, nothing tracked is rewritten by running the demo.
DATASETS="$REPO/clients/ts/viewer/public/datasets.json"

# Vite is `strictPort`, and this origin is what gets written into `dev_cors_origins`. A silent
# fallback to 5174 would surface as a CORS failure that reads like a broken server.
VITE_PORT=5173

export TESSERA_SESSION_CRED="${TESSERA_SESSION_CRED:-dev-session-credential}"
export TESSERA_OPERATOR_CRED="${TESSERA_OPERATOR_CRED:-dev-operator-credential}"

bundle_override=""
run_viewer=1
rebuild=0
scales=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --scale)      scales+=("$2"); shift 2 ;;
    --bundle)     bundle_override="$2"; shift 2 ;;
    --no-viewer)  run_viewer=0; shift ;;
    --rebuild)    rebuild=1; shift ;;
    -h|--help)    sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)            echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ ${#scales[@]} -eq 0 ]] && scales=(2m4 25m)

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

# One trap for every child. Without it a Ctrl-C in the foreground viewer leaves `tessera serve`
# holding the WALs and the listeners, and the next run fails on a bound port rather than on the
# thing that actually went wrong.
SERVE_PIDS=()
cleanup() {
  for pid in "${SERVE_PIDS[@]:-}"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
  done
  for pid in "${SERVE_PIDS[@]:-}"; do
    [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT INT TERM

say "building the release binary"
cargo build --release -p tessera-cli

# Per scale: items, the label the picker shows, and the port triple. Ports are fixed per scale
# rather than allocated, so a `curl` in a second terminal keeps working across restarts and the
# smoke scripts need no discovery step.
items_of()   { case "$1" in 2m4) echo 2422486 ;; 25m) echo 25200000 ;; *) echo 0 ;; esac; }
prose_of()   { case "$1" in 2m4) echo '"title","abstract"' ;; 25m) echo '"title"' ;; *) echo '' ;; esac; }
viewer_of()  { case "$1" in 2m4) echo 37585 ;; 25m) echo 37586 ;; *) echo 0 ;; esac; }
session_of() { case "$1" in 2m4) echo 49303 ;; 25m) echo 49304 ;; *) echo 0 ;; esac; }
control_of() { case "$1" in 2m4) echo 45721 ;; 25m) echo 45722 ;; *) echo 0 ;; esac; }

# `--bundle` is the escape hatch: one server, one entry in the picker, no build.
if [[ -n "$bundle_override" ]]; then
  [[ -d "$bundle_override" ]] || { echo "no such bundle: $bundle_override" >&2; exit 1; }
  scales=(custom)
  items_of()   { echo 0; }
  prose_of()   { echo ''; }
  viewer_of()  { echo 37585; }
  session_of() { echo 49303; }
  control_of() { echo 45721; }
fi

bundle_of() {
  if [[ -n "$bundle_override" ]]; then echo "$bundle_override"; else echo "$DEV/bundle-$1"; fi
}

# ------------------------------------------------------------------------------------ the builds

build_scale() {
  local scale="$1" bundle points schema
  bundle="$(bundle_of "$scale")"
  points="$DATA/demo/points-$scale.parquet"
  schema="$DATA/demo/schema-$scale.toml"

  [[ $rebuild -eq 1 ]] && rm -rf "$bundle"
  if [[ -d "$bundle" ]]; then
    say "reusing $scale at $bundle (--rebuild to discard it)"
    return
  fi

  for f in "$points" "$schema" "$DATA/demo/archive.parquet" \
           "$DATA/demo/primary_category.parquet" \
           "$DATA/scaled/pairs/categories-subclass.pairs.parquet"; do
    [[ -f "$f" ]] || {
      echo "missing fixture: $f" >&2
      echo "build the demo inputs first:" >&2
      echo "  reference/.venv/bin/python probes/build_prose.py \\" >&2
      echo "      ~/.cache/kagglehub/datasets/Cornell-University/arxiv/versions/296/arxiv-metadata-oai-snapshot.json \\" >&2
      echo "      $DATA/corpus.parquet $DATA/demo/prose.parquet" >&2
      echo "  reference/.venv/bin/python probes/build_demo_datasets.py --data $DATA" >&2
      echo "or set TESSERA_DATA to the checkout holding data/ (it is gitignored, so a worktree" >&2
      echo "has none), or pass --bundle PATH to serve a bundle you already have." >&2
      exit 1
    }
  done

  say "building the $scale bundle ($(items_of "$scale") items)"
  mkdir -p "$DEV"
  # `--limit` matches the points file: the pairs file covers a larger corpus, and an unlimited
  # build refuses rather than silently dropping the entities it cannot place.
  # `--mint-id-key` starts a throwaway identity lineage, which is right for a demo bundle and
  # wrong for anything else — every `tessera_id` it mints is meaningless outside this directory,
  # and in particular means nothing to the *other* scale's bundle.
  ./target/release/tessera build \
    --points "$points" \
    --pairs  "$DATA/scaled/pairs/categories-subclass.pairs.parquet" \
    --schema "$schema" \
    --values "archive=$DATA/demo/archive.parquet" \
    --values "primary_category=$DATA/demo/primary_category.parquet" \
    --out "$bundle" --limit "$(items_of "$scale")" \
    --extent 0,65536,0,65536 --slice s0 \
    --mint-external-ids --mint-id-key --no-oracle-pairs
}

if [[ -z "$bundle_override" ]]; then
  for scale in "${scales[@]}"; do build_scale "$scale"; done
fi

# ----------------------------------------------------------------------------------- the servers

start_scale() {
  local scale="$1" bundle config viewer_port session_port
  bundle="$(bundle_of "$scale")"
  viewer_port="$(viewer_of "$scale")"
  session_port="$(session_of "$scale")"
  config="$DEV/serve-$scale.toml"

  mkdir -p "$DEV/$scale"
  cat > "$config" <<EOF
[bundle]
path = "$bundle"
cache = "$DEV/$scale/cache"
wal = "$DEV/$scale/wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
min_visible_members = 10
token_max_lifetime = 3600

[serve]
viewer = "127.0.0.1:$viewer_port"
session = "127.0.0.1:$session_port"
control = "127.0.0.1:$(control_of "$scale")"
max_k = 5000
session_credential_env = "TESSERA_SESSION_CRED"
operator_credential_env = "TESSERA_OPERATOR_CRED"
dev_cors_origins = ["http://localhost:$VITE_PORT"]
EOF

  say "starting tessera serve for $scale on :$viewer_port"
  ./target/release/tessera serve -c "$config" &
  SERVE_PIDS+=($!)
  local pid=${SERVE_PIDS[-1]}

  # Poll `/readyz` rather than sleeping: it is 2.3's verified + pinned + plugin-loaded +
  # workers-ready gate, so a 200 means the bundle actually opened. A fixed sleep would race the
  # mmap and the fragment cache on a cold start and report a broken demo.
  printf 'waiting for %s readyz' "$scale"
  local ready=0
  for _ in $(seq 1 300); do
    if curl -sf "http://127.0.0.1:$viewer_port/readyz" >/dev/null 2>&1; then ready=1; break; fi
    kill -0 "$pid" 2>/dev/null || { echo; echo "tessera serve ($scale) exited during startup" >&2; exit 1; }
    printf '.'; sleep 1
  done
  echo
  [[ $ready == 1 ]] || { echo "$scale did not become ready" >&2; exit 1; }
  echo "ready — $scale viewer plane on http://127.0.0.1:$viewer_port"
}

for scale in "${scales[@]}"; do start_scale "$scale"; done

cat > "$ENV_LOCAL" <<EOF
VITE_TESSERA_VIEWER_URL=http://127.0.0.1:$(viewer_of "${scales[0]}")
VITE_TESSERA_SESSION_URL=http://127.0.0.1:$(session_of "${scales[0]}")
VITE_TESSERA_SESSION_CREDENTIAL=$TESSERA_SESSION_CRED
EOF

if [[ $run_viewer -eq 0 ]]; then
  say "servers only; Ctrl-C to stop"
  wait
  exit 0
fi

cd "$REPO/clients/ts"
[[ -d node_modules ]] || { say "npm ci"; npm ci; }

# ------------------------------------------------------------------------- the dataset document

# Presets are measured **per bundle**: a term id names a different set in each dictionary, so one
# shared list would mislabel every principal on whichever bundle it was not measured against.
say "measuring principals per dataset"
mkdir -p "$(dirname "$DATASETS")" "$DEV/presets"
RANKS="$DATA/scaled/pairs/categories-subclass.pairs.parquet.term-ranks.json"
for scale in "${scales[@]}"; do
  node scripts/measure-principals.mjs \
    --viewer "http://127.0.0.1:$(viewer_of "$scale")" \
    --session "http://127.0.0.1:$(session_of "$scale")" \
    --terms 0..200 --out "$DEV/presets/$scale.json" \
    ${RANKS:+$([[ -f "$RANKS" ]] && echo --ranks "$RANKS")}
done

{
  echo '{"datasets":['
  first=1
  for scale in "${scales[@]}"; do
    [[ $first -eq 1 ]] || echo ','
    first=0
    label="$scale"
    case "$scale" in
      2m4) label="arXiv 2.4M · titles + abstracts" ;;
      25m) label="arXiv 25M · titles" ;;
      custom) label="$bundle_override" ;;
    esac
    printf '{"id":"%s","label":"%s","items":%s,"prose":[%s],' \
      "$scale" "$label" "$(items_of "$scale")" "$(prose_of "$scale")"
    printf '"viewerUrl":"http://127.0.0.1:%s","sessionUrl":"http://127.0.0.1:%s","presets":' \
      "$(viewer_of "$scale")" "$(session_of "$scale")"
    cat "$DEV/presets/$scale.json"
    printf '}'
  done
  echo ']}'
} > "$DATASETS"
echo "wrote $DATASETS"

say "viewer on http://localhost:$VITE_PORT — Ctrl-C to stop everything"
echo "Opens on the broadest principal, the largest mark budget, coloured by archive."
echo
echo "Filters are in the left column, driven by what each bundle publishes as filterable."
echo "  Try: title 'all words' = quantum entanglement, then tick archive = quant-ph."
echo "  Watch 'visible' hold and 'matched' fall in Counts — the filter never moves the mask."
echo "  'exact phrase' is ~200x the cost of 'all words'; reach for it deliberately."
echo
echo "To watch the replica: zoom in a few notches, then pan away and back."
echo "To record a session for someone else: add ?trace=1, press m when it feels wrong, download."
echo "http://localhost:$VITE_PORT/?prefetch=0 turns look-ahead off, cache still on, for comparison."
npm run dev -w @tessera/viewer
