#!/usr/bin/env bash
#
# Build and serve the corpus `arxiv-corpus.ipynb` writes.
#
# The notebook produces build inputs; this turns them into a running server, so the whole path from
# raw arXiv to a masked viewport is two commands rather than a copy-pasted invocation.
#
#   notebooks/run-corpus.sh                  # build if needed, serve, and open the viewer
#   notebooks/run-corpus.sh --notebook       # run the notebook first
#   notebooks/run-corpus.sh --sample 200000  # with --notebook: how many papers
#   notebooks/run-corpus.sh --rebuild        # discard the bundle and build it again
#   notebooks/run-corpus.sh --build-only     # build and stop
#   notebooks/run-corpus.sh --no-viewer      # servers only, for curl
#   notebooks/run-corpus.sh --out DIR        # read build inputs from somewhere else
#
# **This is the localhost development shape and nothing here deploys anywhere.** Two things about
# it are development-only, exactly as `run_demo.sh` says of its own: the session credential is
# minted here and printed, and the planes bind to loopback with no TLS. Neither is anything to
# copy into an integration (`client-interaction.md` §7).
#
# ## What you get
#
# Five layers over the corpus, and the three shapes among them are the point:
#
#   clusters/kmeans    flat, { count = 50 }         — the control: every cluster a peer
#   clusters/hdbscan   nested, { fraction = 0.05 }  — the condensed tree, whose children do not
#                                                     exhaust their parents
#   taxonomy/arxiv     tiered, two levels           — arXiv's own classification, with containment
#                                                     edges running between the levels
#   topics/kmeans      labels attached to the first
#   topics/hdbscan     labels attached to the second
#
# A viewport against the nested layer returns a **cut**: pass artifact_budget to trade depth for
# count, and where a parent and a child both clear the bar the layer's prune_children decides which
# you get. A budget does not climb the tiered layer's edges — its resolution is the client choosing
# a level, and its edges are structure to draw with rather than a ladder to coarsen along.
#
# Every artifact carries the identifier of its parent **where that parent is in the same response**,
# so the viewer nests what it lists and lights a subtree when you open one.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

DATA="${TESSERA_DATA:-/home/joe/code/tessera/data}"
OUT="${TESSERA_NOTEBOOK_OUT:-$DATA/notebook}"
BUNDLE="${TESSERA_NOTEBOOK_BUNDLE:-bundles/notebook}"
VIEWER_PORT="${TESSERA_NOTEBOOK_VIEWER_PORT:-38901}"
SESSION_PORT="${TESSERA_NOTEBOOK_SESSION_PORT:-38902}"
CONTROL_PORT="${TESSERA_NOTEBOOK_CONTROL_PORT:-38903}"
SAMPLE="${TESSERA_NOTEBOOK_SAMPLE:-200000}"
VITE_PORT="${TESSERA_NOTEBOOK_VITE_PORT:-5173}"

run_notebook=0 rebuild=0 build_only=0 run_viewer=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --notebook)   run_notebook=1; shift ;;
    --rebuild)    rebuild=1; shift ;;
    --build-only) build_only=1; shift ;;
    --no-viewer)  run_viewer=0; shift ;;
    --sample)     SAMPLE="$2"; shift 2 ;;
    --out)        OUT="$2"; shift 2 ;;
    --bundle)     BUNDLE="$2"; shift 2 ;;
    -h|--help)    sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'; exit 0 ;;
    *)            echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

# ------------------------------------------------------------------------------- the notebook
if [[ $run_notebook -eq 1 ]]; then
  [[ -x notebooks/.venv/bin/jupyter ]] || {
    echo "notebooks/.venv is missing — see notebooks/README.md" >&2; exit 1; }
  say "running the notebook at sample $SAMPLE"
  TESSERA_DATA="$DATA" TESSERA_NOTEBOOK_OUT="$OUT" TESSERA_NOTEBOOK_SAMPLE="$SAMPLE" \
    notebooks/.venv/bin/jupyter nbconvert --to notebook --execute \
      --output /tmp/arxiv-corpus-executed.ipynb notebooks/arxiv-corpus.ipynb
fi

[[ -f "$OUT/points.parquet" ]] || {
  echo "no build inputs at $OUT — run with --notebook first, or pass --out" >&2; exit 1; }

# ------------------------------------------------------------------------------- the deployment
#
# **One file, read by both verbs.** `tessera build` and `tessera serve` each find it and take
# nothing else: it says where the bundle goes, where the declaration is, and — for the server —
# what to listen on. Generated per run rather than committed, which is why its paths are absolute;
# a `source` inside the declaration is relative to the declaration and stays portable.
#
# The credentials are minted here rather than defaulted: `tessera serve` refuses to start without
# one, and refuses an inline value in the config outright, so there is nowhere for a shared secret
# to be left behind.
work="$(mktemp -d)"
config="$work/tessera.toml"
export TESSERA_SESSION_CRED="${TESSERA_SESSION_CRED:-$(head -c 24 /dev/urandom | base64)}"
export TESSERA_OPERATOR_CRED="${TESSERA_OPERATOR_CRED:-$(head -c 24 /dev/urandom | base64)}"

mkdir -p "$(dirname "$BUNDLE")"
bundle_abs="$(cd "$(dirname "$BUNDLE")" && pwd)/$(basename "$BUNDLE")"
cat > "$config" <<EOF
[bundle]
path  = "$bundle_abs"
cache = "$work/cache"
wal   = "$work/wal.log"

[build]
schema = "$OUT/schema.toml"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:$VIEWER_PORT"
session = "127.0.0.1:$SESSION_PORT"
control = "127.0.0.1:$CONTROL_PORT"
max_k = 5000
session_credential_env = "TESSERA_SESSION_CRED"
operator_credential_env = "TESSERA_OPERATOR_CRED"
# Development-only, exactly as run_demo.sh says of its own: the browser bundle talks to the two
# planes directly, so the dev origin is named here. Nothing to copy into an integration.
dev_cors_origins = ["http://localhost:$VITE_PORT"]
EOF

# ------------------------------------------------------------------------------- the build
[[ $rebuild -eq 1 ]] && rm -rf "$BUNDLE"
if [[ ! -d "$BUNDLE" ]]; then
  say "building $BUNDLE"
  cargo build --release -p tessera-cli

  # **No flag but the identity decision.** Every source, the frame and the layer set are in the
  # declaration, so there is no `--extent` here to disagree with the coordinates the notebook
  # wrote and no source path to bind twice.
  #
  # Read the frame report it prints: the data's own bounds beside the frame, how much of the
  # 65,536² grid the corpus occupies, how many points clamp onto the frame's edge, and what share
  # of points keep a position of their own. A corpus folded into one corner still builds cleanly,
  # and this is where you see that it has.
  ./target/release/tessera build --deployment "$config" --mint-id-key

  # **The build's own report on the hierarchy.** Every split that keeps members none of its
  # children hold is named here, which is what makes a cluster appearing without its children a
  # thing you read about rather than a thing you report.
  if [[ -f "$BUNDLE/reports/containment.json" ]]; then
    python3 - "$BUNDLE/reports/containment.json" <<'CONTAINMENT'
import json, sys
r = json.load(open(sys.argv[1]))
s = r["splits"]
print(f"\nhierarchy: {s['total']} splits, {s['non_covering']} of them keep members no child holds")
if r["violations"]:
    print(f"  {len(r['violations'])} CONTAINMENT VIOLATIONS — a child holds what its parent does not")
for row in s["by_stray_members"][:3]:
    if row["stray_members"]:
        print(f"  {row['parent']}: {row['stray_members']:,} of {row['members']:,} "
              f"held by none of its {row['children']} children")
CONTAINMENT
  fi
else
  say "serving the existing $BUNDLE (--rebuild to discard it)"
fi

[[ $build_only -eq 1 ]] && { say "built; not serving (--build-only)"; exit 0; }

# ------------------------------------------------------------------------------- the server
#
# The same `tessera.toml` the build read: a build's output path and a server's bundle path are one
# value seen from two sides, so a server can never open a bundle some other invocation wrote.

# **Refuse a port someone else holds, rather than talking to whatever is on it.** Without this the
# readiness poll below is satisfied by a *stale server from an earlier run*: the new one exits on a
# bind failure, the poll succeeds against the old one, and every request afterwards goes to a bundle
# and a credential nobody chose. That is not hypothetical — it is what happened while this script
# was being written, and it presents as an authorisation failure three steps later.
for port in "$VIEWER_PORT" "$SESSION_PORT" "$CONTROL_PORT"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
    exec 3>&-
    echo "port $port is already in use — stop what is on it, or set the port explicitly" >&2
    echo "  TESSERA_NOTEBOOK_VIEWER_PORT / _SESSION_PORT / _CONTROL_PORT" >&2
    exit 1
  fi
done

say "starting tessera serve on :$VIEWER_PORT"
./target/release/tessera serve --deployment "$config" &
serve_pid=$!
trap 'kill "$serve_pid" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

for _ in $(seq 1 300); do
  curl -sf "http://127.0.0.1:$VIEWER_PORT/readyz" >/dev/null 2>&1 && break
  kill -0 "$serve_pid" 2>/dev/null || { echo "tessera serve exited during startup" >&2; exit 1; }
  sleep 0.2
done

# --------------------------------------------------------------------- how to ask it something
# The terms are arXiv categories, so a principal is written in the vocabulary of the corpus.
#
# **The pair is a set of categories against one of them**, rather than a wide category against a
# narrow one, and the difference matters for what it demonstrates. Two unrelated categories see
# two disjoint sets of clusters, which shows nothing; a subset relationship puts *the same
# clusters* in front of both principals with a different count beside each — which is the thing to
# see. A category narrow enough to be interesting on its own is also narrow enough to fall under
# the layer's 5% floor everywhere, and its viewer is served nothing at all.
#
# **The frame comes from the service, not from a constant.** A viewport's bbox is in the corpus's
# own coordinates, and the view was quantised against the box `extent = "auto"` fitted to them —
# so the full-extent box is whatever `/v1/meta` publishes and never a number written here.
# One value per line and read with `mapfile`, rather than a line of words: two of these values are
# prose and a word-split would silently shift every field after them.
mapfile -t facts < <(python3 - "$OUT" "$SESSION_PORT" "$VIEWER_PORT" <<'TOPTERMS'
import base64, collections, json, os, pathlib, sys, urllib.request
import pyarrow.parquet as pq

out, session_port, viewer_port = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
counts = collections.Counter(
    t for cats in pq.read_table(out / "points.parquet", columns=["categories"])
                     .column("categories").to_pylist()
    for t in (cats or []))
top = [t for t, _ in counts.most_common(8)]

auth = base64.b64encode(json.dumps({"terms": top}).encode()).decode()
req = urllib.request.Request(
    f"http://127.0.0.1:{session_port}/session/authorise",
    data=json.dumps({"auth_data": auth}).encode(),
    headers={"content-type": "application/json",
             "authorization": f"Bearer {os.environ['TESSERA_SESSION_CRED']}"})
token = json.loads(urllib.request.urlopen(req, timeout=60).read())["token"]
meta = json.loads(urllib.request.urlopen(urllib.request.Request(
    f"http://127.0.0.1:{viewer_port}/v1/meta",
    headers={"authorization": f"Bearer {token}"}), timeout=60).read())
q = meta["quantisation"]

for value in (",".join(f'"{t}"' for t in top),
              top[0],
              " + ".join(top[:4]) + f" and {len(top) - 4} more",
              q["x_min"], q["y_min"], q["x_max"], q["y_max"]):
    print(value)
TOPTERMS
)
broad_list="${facts[0]}" narrow_name="${facts[1]}" broad_names="${facts[2]}"
bbox="[${facts[3]},${facts[4]},${facts[5]},${facts[6]}]"
x_min="${facts[3]}" y_min="${facts[4]}" x_max="${facts[5]}" y_max="${facts[6]}"

cat <<EOF

$(printf '\033[1mready\033[0m')

  viewer   http://127.0.0.1:$VIEWER_PORT
  session  http://127.0.0.1:$SESSION_PORT
  control  http://127.0.0.1:$CONTROL_PORT

Paste this into the shell you are going to ask from — the credential is minted per run, so it
exists only in this process until you do. It is printed because this is localhost development;
a real deployment's session credential never leaves the operator's hands.

  export TESSERA_SESSION_CRED='$TESSERA_SESSION_CRED'

Mint two tokens — the terms are arXiv categories, and the narrow principal holds one of the
broad principal's eight, so the two are looking at the same clusters:

  BROAD=\$(curl -s -X POST http://127.0.0.1:$SESSION_PORT/session/authorise \\
    -H "authorization: Bearer \$TESSERA_SESSION_CRED" -H 'content-type: application/json' \\
    -d '{"auth_data":"'\$(printf '{"terms":[$broad_list]}' | base64 -w0)'"}' | jq -r .token)

  NARROW=\$(curl -s -X POST http://127.0.0.1:$SESSION_PORT/session/authorise \\
    -H "authorization: Bearer \$TESSERA_SESSION_CRED" -H 'content-type: application/json' \\
    -d '{"auth_data":"'\$(printf '{"terms":["$narrow_name"]}' | base64 -w0)'"}' | jq -r .token)

  # broad is $broad_names; narrow is $narrow_name alone

Then ask the same question as each, and compare the count beside one cluster:

  curl -s -X POST http://127.0.0.1:$VIEWER_PORT/v1/viewport \\
    -H "authorization: Bearer \$BROAD" -H 'content-type: application/json' \\
    -d '{"view":"s0","zoom":0,"bbox":$bbox,"k":1,
         "layers":["clusters/hdbscan"],"artifact_budget":20}' --output -

The response is a streamed frame sequence, not JSON — frame kind 5 carries the artifacts. The
TypeScript client decodes it (\`clients/ts\`), and \`probes/2026-08-18-condensed-tree/check.py\`
has a forty-line Python decoder if you want one to read.

Three things worth trying:

  * compare the two principals on a cluster they are both served. The identifier is the same and
    the count is not: on a 50 000-paper run the broad principal sees 116 members of one cluster
    where the narrow sees 66. Neither number is the cluster's own size, and neither viewer is told
    what that is.
  * count the clusters each is served. The narrow principal gets **fewer clusters**, not just
    smaller numbers: the ones where their share fell under the layer's 5% floor are absent, and
    absent identically to a cluster that was never published.
  * drop \`artifact_budget\` and count, then set it to 5. The cut climbs the tree rather than
    dropping clusters, so you get fewer and coarser — never a map with holes in it.

EOF

if [[ $run_viewer -eq 0 ]]; then
  say "servers only (--no-viewer); Ctrl-C to stop"
  wait "$serve_pid"
  exit 0
fi

# ------------------------------------------------------------------------------- the viewer
#
# Three things the browser bundle needs, none of which the bundle itself can supply.

cd clients/ts
[[ -d node_modules ]] || { say "npm ci"; npm ci; }

# **Where the clusters are on the map.** No geometry crosses the wire at this stage — a bounding box
# over full membership would disclose a cluster's extent by panning — so a demo has to get positions
# from the publisher, and this is that sidecar. It carries **position and nothing else**: an entry
# with no artifact in the response is not drawn, so it can never put a cluster on screen that the
# asking principal was not served, and it carries neither membership nor declared size.
say "placing clusters for the viewer"
python3 - "$OUT" viewer/public/clusters.json "$x_min" "$y_min" "$x_max" "$y_max" <<'PLACE'
import json, pathlib, sys, collections
import pyarrow.parquet as pq

out, dest = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
x_min, y_min, x_max, y_max = (float(v) for v in sys.argv[3:7])
points = pq.read_table(out / "points.parquet", columns=["entity_id", "x", "y"]).to_pydict()
xy = {e: (x, y) for e, x, y in zip(points["entity_id"], points["x"], points["y"])}
# The centroid of an artifact's **declared** membership, which is the publisher's own knowledge and
# not a served quantity. Rows carrying a rank are generating sets, not memberships, and are skipped.
#
# **One member source per layer**, named for the layer it belongs to — no file carries a layer
# column, there being no second layer's rows in it to tell apart.
acc = collections.defaultdict(lambda: [0.0, 0.0, 0])
for path in sorted(out.glob("*-members.parquet")):
    layer = path.name[: -len("-members.parquet")].replace("-", "/", 1)
    members = pq.read_table(path).to_pydict()
    for key, rank, entity in zip(members["key"], members["rank"], members["entity"]):
        if rank is not None:
            continue
        x, y = xy[entity]
        a = acc[(layer, key)]
        a[0] += x; a[1] += y; a[2] += 1

# **In cells, because that is what the viewer draws in.** The sidecar's coordinates are grid cells
# on the 65,536² frame, and the frame is the one the build fitted — read back off `/v1/meta` rather
# than assumed here, which is the same mistake in miniature that a hand-written extent was.
CELLS = 65536.0
sx, sy = CELLS / (x_max - x_min), CELLS / (y_max - y_min)

layers = collections.defaultdict(list)
for (layer, key), (px, py, n) in acc.items():
    layers[layer].append({"key": key,
                          "x": (px / n - x_min) * sx,
                          "y": (py / n - y_min) * sy})
dest.parent.mkdir(parents=True, exist_ok=True)
dest.write_text(json.dumps({"layers": layers}))
print(f"  {sum(len(v) for v in layers.values())} positions across {len(layers)} layers")
PLACE

# **Principals are measured against this bundle, never carried between bundles.** A term id names a
# different set in each dictionary, so a preset list measured elsewhere would mislabel every
# principal here.
say "measuring principals"
# **A principal is written in the corpus's own vocabulary.** The access terms are arXiv category
# names, so the candidates are the categories themselves — there is no `term_id` on this path and
# no sidecar mapping one back, the build's dictionary being the only place ids are decided.
terms="$(python3 - "$OUT" <<'CANDIDATES'
import collections, pathlib, sys
import pyarrow.parquet as pq
counts = collections.Counter(
    t for cats in pq.read_table(pathlib.Path(sys.argv[1]) / "points.parquet",
                                columns=["categories"]).column("categories").to_pylist()
    for t in (cats or []))
print(",".join(t for t, _ in counts.most_common(120)))
CANDIDATES
)"
node scripts/measure-principals.mjs \
  --viewer "http://127.0.0.1:$VIEWER_PORT" --session "http://127.0.0.1:$SESSION_PORT" \
  --terms "$terms" --out "$work/presets.json"

# **One composed principal, added to the measured list.** The measured presets are single terms,
# and a single term either sees the whole map (the few broad ones) or no clusters at all — its
# share of every cluster falls under the layer's floor. The interesting comparison is a *subset*:
# a viewer holding eight categories against one holding one of the eight sees the same clusters
# with a different count beside each, which is the thing this corpus exists to show.
python3 - "$OUT" "$work/presets.json" "$SESSION_PORT" "$VIEWER_PORT" "$bbox" <<'COMPOSED'
import collections, io, json, pathlib, struct, sys, base64, urllib.request, os
import pyarrow.ipc as ipc
import pyarrow.parquet as pq

out, presets = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
session_port, viewer_port, bbox = sys.argv[3], sys.argv[4], json.loads(sys.argv[5])
counts = collections.Counter(
    t for cats in pq.read_table(out / "points.parquet", columns=["categories"])
                     .column("categories").to_pylist()
    for t in (cats or []))
top = [t for t, _ in counts.most_common(8)]


def post(url, body, bearer):
    req = urllib.request.Request(
        url, data=json.dumps(body).encode(),
        headers={"content-type": "application/json", "authorization": f"Bearer {bearer}"})
    return urllib.request.urlopen(req, timeout=300).read()


# **Measured from the service, never derived here.** The visible-set size is what a zoom-0,
# full-extent call reports for the root tile; computing it from the pairs file instead would be
# this script's own arithmetic standing in for the system's answer.
cred = os.environ["TESSERA_SESSION_CRED"]
auth = base64.b64encode(json.dumps({"terms": top}).encode()).decode()
token = json.loads(post(f"http://127.0.0.1:{session_port}/session/authorise",
                        {"auth_data": auth}, cred))["token"]
body = post(f"http://127.0.0.1:{viewer_port}/v1/viewport",
            {"view": "s0", "zoom": 0, "bbox": bbox, "k": 1}, token)
visible, at = 0, 0
while at < len(body):
    kind = body[at]
    (length,) = struct.unpack_from("<I", body, at + 1)
    payload = body[at + 5: at + 5 + length]
    at += 5 + length
    if kind == 1:
        visible = sum(ipc.open_stream(io.BytesIO(payload)).read_all().to_pydict()["visible"])
        break

chosen = json.loads(presets.read_text())
chosen.insert(-1, {"label": f"eight categories (superset of {top[0]})",
                   "terms": top, "visible": visible})
presets.write_text(json.dumps(chosen, indent=2) + "\n")
print(f"  added a composed principal over {len(top)} terms, {visible:,} visible")
COMPOSED

items="$(python3 -c "
import pyarrow.parquet as pq
print(pq.ParquetFile('$OUT/points.parquet').metadata.num_rows)")"

mkdir -p viewer/public
{
  printf '{"datasets":[{"id":"notebook","label":"arXiv %s · notebook corpus","items":%s,' \
    "$items" "$items"
  printf '"prose":["title","abstract"],'
  printf '"viewerUrl":"http://127.0.0.1:%s","sessionUrl":"http://127.0.0.1:%s","presets":' \
    "$VIEWER_PORT" "$SESSION_PORT"
  cat "$work/presets.json"
  printf '}]}'
} > viewer/public/datasets.json

cat > viewer/.env.local <<EOF
VITE_TESSERA_VIEWER_URL=http://127.0.0.1:$VIEWER_PORT
VITE_TESSERA_SESSION_URL=http://127.0.0.1:$SESSION_PORT
VITE_TESSERA_SESSION_CREDENTIAL=$TESSERA_SESSION_CRED
EOF

say "viewer on http://localhost:$VITE_PORT — Ctrl-C to stop everything"
cat <<'EOF'
In the layer panel, switch between the three clustering shapes. k-means is flat and every cluster
is a peer. HDBSCAN is a tree: the panel indents each cluster under the one that contains it, and
clicking a cluster lights its whole subtree on the map while the rest stays drawn. The arXiv
taxonomy is levelled, and its indentation crosses a level — a subject class under its archive.

Then change principal, and watch two things move. A cluster's count changes without its identifier
changing, and clusters leave the panel entirely when your share of them drops under the layer's 5%
floor — indistinguishable, from here, from their never having been published.

**The tree changes shape too.** A narrow principal's clusters hang off several tops where a broad
one's hang off a single root: a share does not shrink downward, so a child can clear a bar its
parent misses, and what is left is pieces. A piece's top is drawn flush left, exactly as a cluster
with no parent at all is — nothing tells you a coarser one exists above it.
EOF
npm run dev -w @tessera/viewer
