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
#   ./run_demo.sh --scale notebook # the notebook corpus: the HDBSCAN tree, its topics, the taxonomy
#   ./run_demo.sh --scale notebook-2m4  # the same pipeline over the whole corpus, plus toponymy
#   ./run_demo.sh --bundle PATH   # serve a bundle you already have, on its own
#   ./run_demo.sh --no-viewer     # servers only (for curl, the golden capture, the smoke script)
#   ./run_demo.sh --rebuild       # discard and rebuild the demo bundles
#
# ## The five scales, and why they differ
#
#     scale         items          prose indexed        bundle
#     notebook      50,000         title + abstract     ~35 MB
#     notebook-2m4  2,422,486      title + abstract     ~2.3 GB
#     2m4        2,422,486      title + abstract     ~1.4 GB
#     25m        25,200,000     title                ~2.7 GB
#     250m       250,000,000    none                 ~5 GB
#     1b         1,000,000,000  none                 ~20 GB
#
# `notebook` is the odd one out, and it is in the picker for what the other four lack. They
# publish one flat k-means layer; `notebook` is `data/notebook/`, which `notebooks/arxiv-corpus.ipynb`
# writes — a uniform sample of the same corpus carrying **five declared layers**: k-means flat
# under a `{ count = 50 }` floor, HDBSCAN's condensed tree nested under `{ fraction = 0.05 }`, a
# TF-IDF topic label attached to every cluster of each, and arXiv's own classification as a tiered
# layer whose two levels are joined by containment edges. It builds in seconds. Its principals are
# written in the corpus's own vocabulary — an arXiv category is a term — where the others' are the
# integer ids of a synthetic dictionary, so its presets read `math.AG` rather than `14`. The item
# count is whatever the notebook sampled, read from the points file rather than tabled here.
# `notebook-2m4` is `data/notebook-2m4-live/`: the whole corpus through the same pipeline, with a
# fourth clustering — `clusters/toponymy`, four tiered levels named by a language model — and a
# label layer over it. It builds in minutes rather than seconds.
#
# The two large scales are worth starting deliberately rather than by default: between them they
# are ~135 GB of bundle and the better part of an afternoon to build. `--scale 2m4 --scale 25m`
# is the pair to reach for while working on the client.
#
# Abstracts stop at the small scale because they are 954 characters against a title's 73: the record
# blob holding 25.2M of them is ~26 GB, and its index ~4 GB on top. The viewer draws its filter
# controls from `/v1/meta`, so the abstract box is simply absent on the large bundle — the honest
# rendering of a column that is not there, and not a case the client special-cases.
#
# The four arXiv scales are built from `data/demo/`, which `probes/build_demo_datasets.py` writes. Run that first if
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
build_only=0
scales=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --scale)      scales+=("$2"); shift 2 ;;
    --bundle)     bundle_override="$2"; shift 2 ;;
    --no-viewer)  run_viewer=0; shift ;;
    --build-only) build_only=1; shift ;;
    --rebuild)    rebuild=1; shift ;;
    -h|--help)    sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)            echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
# `1b` is deliberately NOT in the default set, and the reason is the served side rather than the
# build: its hot columns alone are 11.16 GiB resident (the figure the build prints), so on a
# machine that does not have that to spare *on top of* the other scales, adding it to the picker
# makes every scale slower rather than adding one that works. Build it with
# `--scale 1b --build-only` and serve it by itself with `--scale 1b`.
[[ ${#scales[@]} -eq 0 ]] && scales=(2m4 25m 250m)

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

# `TESSERA_BIN` serves a binary built elsewhere — a second worktree sharing one machine's release
# build rather than paying for its own — and is otherwise this checkout's, built here.
if [[ -n "${TESSERA_BIN:-}" ]]; then
  BIN="$TESSERA_BIN"
  [[ -x "$BIN" ]] || { echo "TESSERA_BIN=$BIN is not an executable" >&2; exit 1; }
  say "using $BIN"
else
  say "building the release binary"
  cargo build --release -p tessera-cli
  BIN="./target/release/tessera"
fi

# Per scale: items, the label the picker shows, and the port triple. Ports are fixed per scale
# rather than allocated, so a `curl` in a second terminal keeps working across restarts and the
# smoke scripts need no discovery step.
items_of() {
  case "$1" in
    2m4) echo 2422486 ;; 25m) echo 25200000 ;; 250m) echo 250000000 ;; 1b) echo 1000000000 ;;
    # The notebook decides its own sample; the Parquet footer is the record of what it chose.
    notebook*) python3 -c "import pyarrow.parquet as pq, sys; print(pq.ParquetFile(sys.argv[1]).metadata.num_rows)" \
                "$(notebook_dir_of "$1")/points.parquet" ;;
    *) echo 0 ;;
  esac
}
prose_of()   { case "$1" in 2m4|notebook*) echo '"title","abstract"' ;; 25m) echo '"title"' ;; *) echo '' ;; esac; }
viewer_of()  { case "$1" in 2m4) echo 37585 ;; 25m) echo 37586 ;; 250m) echo 37587 ;; 1b) echo 37588 ;; notebook) echo 37589 ;; notebook-2m4) echo 37590 ;; *) echo 0 ;; esac; }
session_of() { case "$1" in 2m4) echo 49303 ;; 25m) echo 49304 ;; 250m) echo 49305 ;; 1b) echo 49306 ;; notebook) echo 49307 ;; notebook-2m4) echo 49308 ;; *) echo 0 ;; esac; }
control_of() { case "$1" in 2m4) echo 45721 ;; 25m) echo 45722 ;; 250m) echo 45723 ;; 1b) echo 45724 ;; notebook) echo 45725 ;; notebook-2m4) echo 45726 ;; *) echo 0 ;; esac; }
# The two notebook scales are directories the notebook wrote, each holding its points, its
# per-layer files and its own declaration; empty for every other scale.
notebook_dir_of() { case "$1" in notebook) echo "$DATA/notebook" ;; notebook-2m4) echo "$DATA/notebook-2m4-live" ;; *) echo '' ;; esac; }
# The declaration each scale is built from. The notebook writes its own beside its files, with
# every source path relative to it (configuration.md §3); the demo scales share one generator.
schema_of()  { case "$1" in notebook*) echo "$(notebook_dir_of "$1")/schema.toml" ;; *) echo "$DATA/demo/config-$1.toml" ;; esac; }

# `--bundle` is the escape hatch: one server, one entry in the picker, no build.
if [[ -n "$bundle_override" ]]; then
  [[ -d "$bundle_override" ]] || { echo "no such bundle: $bundle_override" >&2; exit 1; }
  scales=(custom)
  items_of()   { echo 0; }
  prose_of()   { echo ''; }
  schema_of()  { echo ''; }
  viewer_of()  { echo 37585; }
  session_of() { echo 49303; }
  control_of() { echo 45721; }
fi

bundle_of() {
  if [[ -n "$bundle_override" ]]; then echo "$bundle_override"; else echo "$DEV/bundle-$1"; fi
}

# ------------------------------------------------------------------------------------ the builds

# One `tessera.toml` per scale, written to the gitignored dev directory and used by **both**
# verbs: `tessera build` reads `[bundle].path` as its output and `[build].schema` as its
# declaration, and `tessera serve` opens exactly what the build wrote (configuration.md §3). The
# paths are absolute because this file is generated per machine and never committed — a `source` in
# the *declaration* is the one that has to travel.
write_deployment() {
  local scale="$1" bundle
  bundle="$(bundle_of "$scale")"
  mkdir -p "$DEV/$scale"
  cat > "$DEV/tessera-$scale.toml" <<EOF
[bundle]
path  = "$bundle"
cache = "$DEV/$scale/cache"
wal   = "$DEV/$scale/wal.log"

[build]
schema = "$(schema_of "$scale")"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer = "127.0.0.1:$(viewer_of "$scale")"
session = "127.0.0.1:$(session_of "$scale")"
control = "127.0.0.1:$(control_of "$scale")"
max_k = 5000
session_credential_env = "TESSERA_SESSION_CRED"
operator_credential_env = "TESSERA_OPERATOR_CRED"
dev_cors_origins = ["http://localhost:$VITE_PORT"]
EOF
}

build_scale() {
  local scale="$1" bundle points config
  bundle="$(bundle_of "$scale")"
  config="$(schema_of "$scale")"
  points="$DATA/demo/points-$scale.parquet"
  [[ -n "$(notebook_dir_of "$scale")" ]] && points="$(notebook_dir_of "$scale")/points.parquet"

  [[ $rebuild -eq 1 ]] && rm -rf "$bundle"
  # `CURRENT` is written last, so its presence — not the directory's — is what says the build
  # finished. A build killed partway (an OOM, a Ctrl-C, a laptop lid) leaves `v00000` holding a
  # truncated segment, and reusing that directory serves a corpus with a hole in it rather than
  # failing: the bundle opens, `/readyz` passes, and the map is quietly missing whatever the
  # writer had not reached. Discard and rebuild instead.
  if [[ -d "$bundle" && -e "$bundle/CURRENT" ]]; then
    say "reusing $scale at $bundle (--rebuild to discard it)"
    return
  fi
  if [[ -d "$bundle" ]]; then
    say "discarding an incomplete $scale bundle at $bundle (no CURRENT — a build died partway)"
    rm -rf "$bundle"
  fi

  # The notebook's declaration names every file it reads, so the points and the declaration are
  # the two worth checking here; `tessera check` in the build reports the rest by name.
  local -a fixtures=("$points" "$config")
  [[ -n "$(notebook_dir_of "$scale")" ]] || fixtures+=("$DATA/demo/archive.parquet"
           "$DATA/demo/primary_category.parquet"
           "$DATA/scaled/pairs/categories-subclass.pairs.parquet")
  for f in "${fixtures[@]}"; do
    [[ -f "$f" ]] || {
      echo "missing fixture: $f" >&2
      if [[ -n "$(notebook_dir_of "$scale")" ]]; then
        echo "the notebook corpus is written by notebooks/arxiv-corpus.ipynb:" >&2
        echo "  notebooks/run-corpus.sh --notebook --build-only --out $(notebook_dir_of "$scale")" >&2
        exit 1
      fi
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
  write_deployment "$scale"

  # **Minting is dropped above 10⁸ items, and the reason is a pass the memory budget cannot
  # reach.** `pipeline.rs`' own table bounds three passes by the corpus rather than by the plan:
  # `source_ids` at 8N, minted external ids at 20N, and geometry + tiler + segment at 28N. Only
  # the per-batch and per-band rows answer to `--memory-budget`, so at 250,000,000 items the
  # minting pass alone is ~5 GB on top of the ~7 GB the geometry pass needs, and no batch size
  # moves either — measured, by four builds at four budgets all dying at the same 4.8 GB of
  # output. Minting is off by default in the binary for an unrelated and better reason
  # (contracts §2.4: an external id is not manufactured for an item whose caller supplied none),
  # and the demo wants none of it — the viewer addresses items by `tessera_id`. It stays on at
  # the small scales only to keep those bundles comparable with the bench fixtures.
  local mint_external="--mint-external-ids"
  if [[ "$(items_of "$scale")" -gt 100000000 ]]; then
    mint_external=""
    echo "minting no external ids at this scale (the 20N pass; see build_scale)"
  fi
  # Timed, and the peak RSS kept alongside the wall clock. A build that runs for hours at the
  # upper scales should say how long it took without the operator having to have watched it, and
  # peak RSS is the figure that decides whether the *next* scale fits on the machine at all —
  # which is not something the bundle on disk records. `/usr/bin/time -v`, not the shell builtin:
  # the builtin reports no memory.
  local t0=$SECONDS
  # `--limit` matches the points file: the pairs file covers a larger corpus, and an unlimited
  # build refuses rather than silently dropping the entities it cannot place. The notebook's
  # files are all one corpus, so it takes no limit.
  local -a limit=(--limit "$(items_of "$scale")")
  [[ -n "$(notebook_dir_of "$scale")" ]] && limit=()
  # `--mint-id-key` starts a throwaway identity lineage, which is right for a demo bundle and
  # wrong for anything else — every `tessera_id` it mints is meaningless outside this directory,
  # and in particular means nothing to the *other* scale's bundle. A real deployment puts its key
  # in the environment (`TESSERA_IDENTITY_KEY`, or a `.env` beside `tessera.toml`) instead.
  # `TESSERA_BUILD_MEMORY_BUDGET` (e.g. `8g`) caps the build's own structures. Unset, the binary
  # sizes its batches from `MemAvailable` at the moment it starts — which is the right default on
  # a machine doing nothing else, and wrong on one where the page cache for a 51 GB points file
  # and a 107 GB bundle is competing for the same pages. Naming a budget under the machine's RAM
  # buys spill instead of pressure, and the upper scales are where that trade is worth making.
  # `TESSERA_BUILD_MEMORY_MAX` (e.g. `9G`) runs the build inside a transient cgroup rather than
  # loose on the machine. This is not the same lever as the budget above and does not replace it:
  # the budget bounds the build's *own structures*, while the cgroup additionally charges the
  # **page cache** for a 51 GB points file and a 107 GB bundle against the same ceiling. Without
  # it, a build that honours its budget perfectly still drives the machine into reclaim, and the
  # observed failure is not an OOM kill — it is the box going unresponsive with nothing logged,
  # because the kernel never had a reason to kill anything. With it, the kernel reclaims this
  # cgroup's cache instead, and an interactive session stays answerable.
  #
  # `MemorySwapMax=0` is part of the same intent: swapping the build is what makes a box feel
  # dead, and failing inside the cgroup is the better outcome. The binary reads the limit too
  # (pipeline.rs `detect_memory_budget` consults cgroup v2 `memory.max`), so the plan it prints
  # already accounts for it.
  #
  # **`MemoryHigh` defaults to `infinity`, and setting it below what the build wants is worse than
  # setting nothing.** `memory.high` does not cap — it throttles, and the kernel will keep a
  # process just under it indefinitely rather than let it through. Measured at 250,000,000 items
  # with `MemoryHigh=7G`: 147,562 breaches, a cgroup stalled on memory 72% of every 60 seconds,
  # 449 seconds of CPU burnt in reclaim out of 466 elapsed, and **zero bytes of bundle written**.
  # Lifting it to `infinity` on the same running build resumed it at 449 MiB/min. `MemoryMax` is
  # the number to set; it reclaims this cgroup's page cache first and only kills if the anonymous
  # working set genuinely exceeds it, which is the protection actually wanted.
  local -a scope=()
  if [[ -n "${TESSERA_BUILD_MEMORY_MAX:-}" ]]; then
    scope=(systemd-run --user --scope -q --collect
           -p "MemoryMax=$TESSERA_BUILD_MEMORY_MAX"
           -p "MemoryHigh=${TESSERA_BUILD_MEMORY_HIGH:-infinity}"
           -p "MemorySwapMax=0")
  fi
  "${scope[@]}" \
  /usr/bin/time -v -o "$DEV/build-$scale.time" \
  "$BIN" build \
    --deployment "$DEV/tessera-$scale.toml" \
    "${limit[@]}" \
    ${TESSERA_BUILD_MEMORY_BUDGET:+--memory-budget "$TESSERA_BUILD_MEMORY_BUDGET"} \
    $mint_external --mint-id-key --no-oracle-pairs
  local peak
  peak=$(awk '/Maximum resident set size/ {printf "%.1f GiB", $NF / 1048576}' "$DEV/build-$scale.time")
  printf 'built %s in %dm%02ds, peak %s, %s on disk\n' \
    "$scale" "$(( (SECONDS - t0) / 60 ))" "$(( (SECONDS - t0) % 60 ))" \
    "$peak" "$(du -sh "$bundle" | cut -f1)" | tee -a "$DEV/build-times.txt"
}

if [[ -z "$bundle_override" ]]; then
  for scale in "${scales[@]}"; do build_scale "$scale"; done
fi

# `--build-only` exists for the scales whose build and whose serving have different appetites: the
# 10⁹ bundle is hours of work that wants the machine to itself, and starting a server on top of it
# the moment it lands is the opposite of what is wanted.
if [[ $build_only -eq 1 ]]; then
  say "built; not serving (--build-only)"
  exit 0
fi

# ----------------------------------------------------------------------------------- the servers

start_scale() {
  local scale="$1" viewer_port
  viewer_port="$(viewer_of "$scale")"

  # The same file the build was invoked against — `--bundle` skips the build, so it may not exist
  # yet, and rewriting it is idempotent either way.
  write_deployment "$scale"

  say "starting tessera serve for $scale on :$viewer_port"
  "$BIN" serve --deployment "$DEV/tessera-$scale.toml" &
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

cd "$REPO/clients/ts"
[[ -d node_modules ]] || { say "npm ci"; npm ci; }

# ------------------------------------------------------------------------- the dataset document

# Presets are measured **per bundle**: a term id names a different set in each dictionary, so one
# shared list would mislabel every principal on whichever bundle it was not measured against.
#
# The candidates differ by corpus. The demo scales' dictionary is synthetic and its terms are the
# integers `0..200`, ranked by `scripts/rank_terms.py` so coverage principals can be composed. The
# notebook's terms are arXiv categories, read off the points file's `categories` column; the ranks
# are counted from the same column here, in the same `[{term, pairs}]` shape, so the one script
# composes the same five bands over both — narrow, sparse, medium, heavy, full.
say "measuring principals per dataset"
mkdir -p "$(dirname "$DATASETS")" "$DEV/presets"
RANKS="$DATA/scaled/pairs/categories-subclass.pairs.parquet.term-ranks.json"
for scale in "${scales[@]}"; do
  terms="0..200"
  ranks="$RANKS"
  if [[ -n "$(notebook_dir_of "$scale")" ]]; then
    ranks="$DEV/presets/$scale.term-ranks.json"
    terms="$(python3 - "$(notebook_dir_of "$scale")/points.parquet" "$ranks" <<'CANDIDATES'
import collections, json, sys
import pyarrow.parquet as pq
counts = collections.Counter(
    t for cats in pq.read_table(sys.argv[1], columns=["categories"]).column("categories").to_pylist()
    for t in (cats or []))
ranked = [{"term": t, "pairs": n} for t, n in counts.most_common()]
open(sys.argv[2], "w").write(json.dumps(ranked) + "\n")
print(",".join(r["term"] for r in ranked))
CANDIDATES
)"
  fi
  node scripts/measure-principals.mjs \
    --viewer "http://127.0.0.1:$(viewer_of "$scale")" \
    --session "http://127.0.0.1:$(session_of "$scale")" \
    --terms "$terms" --out "$DEV/presets/$scale.json" \
    $([[ -f "$ranks" ]] && echo --ranks "$ranks")
done

# This run's entries, one per scale, in the order given: the viewer opens on the first.
fresh="$DEV/presets/datasets-this-run.json"
{
  echo '['
  first=1
  for scale in "${scales[@]}"; do
    [[ $first -eq 1 ]] || echo ','
    first=0
    label="$scale"
    case "$scale" in
      notebook) label="arXiv $(items_of notebook | sed ':a;s/\B[0-9]\{3\}\>/,&/;ta') · notebook corpus: HDBSCAN tree, topics, taxonomy" ;;
      notebook-2m4) label="arXiv $(items_of notebook-2m4 | sed ':a;s/\B[0-9]\{3\}\>/,&/;ta') · notebook pipeline, whole corpus: HDBSCAN tree, toponymy, topics, taxonomy" ;;
      2m4) label="arXiv 2.4M · titles + abstracts" ;;
      25m) label="arXiv 25M · titles" ;;
      250m) label="arXiv 250M · no prose" ;;
      1b) label="arXiv 1B · no prose" ;;
      custom) label="$bundle_override" ;;
    esac
    printf '{"id":"%s","label":"%s","items":%s,"prose":[%s],' \
      "$scale" "$label" "$(items_of "$scale")" "$(prose_of "$scale")"
    printf '"viewerUrl":"http://127.0.0.1:%s","sessionUrl":"http://127.0.0.1:%s","presets":' \
      "$(viewer_of "$scale")" "$(session_of "$scale")"
    cat "$DEV/presets/$scale.json"
    printf '}'
  done
  echo ']'
} > "$fresh"

# **Merged with what is already running, not overwritten.** Two invocations serve two sets of
# scales on their fixed ports — `--scale 2m4` in one terminal, `--scale notebook` in another — and
# the picker should offer both. An entry this run did not write is kept if its viewer plane still
# answers `/readyz` and dropped if it does not, so the document names what is actually up.
python3 - "$DATASETS" "$fresh" <<'MERGE'
import json, pathlib, sys, urllib.request
dest, fresh = pathlib.Path(sys.argv[1]), json.loads(pathlib.Path(sys.argv[2]).read_text())
mine = {d["id"] for d in fresh}
kept = []
if dest.exists():
    for d in json.loads(dest.read_text()).get("datasets", []):
        if d["id"] in mine:
            continue
        try:
            urllib.request.urlopen(d["viewerUrl"] + "/readyz", timeout=2)
            kept.append(d)
        except Exception:
            print(f"  dropping {d['id']}: nothing answers at {d['viewerUrl']}")
dest.write_text(json.dumps({"datasets": fresh + kept}, indent=2) + "\n")
print(f"  {', '.join(d['id'] for d in fresh)} written" + (f"; {', '.join(d['id'] for d in kept)} kept" if kept else ""))
MERGE
echo "wrote $DATASETS"

if [[ $run_viewer -eq 0 ]]; then
  say "servers only; Ctrl-C to stop"
  wait
  exit 0
fi

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
npm run dev -w @tesseradb/viewer
