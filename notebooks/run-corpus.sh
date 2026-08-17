#!/usr/bin/env bash
#
# Build and serve the corpus `arxiv-corpus.ipynb` writes.
#
# The notebook produces build inputs; this turns them into a running server, so the whole path from
# raw arXiv to a masked viewport is two commands rather than a copy-pasted invocation.
#
#   notebooks/run-corpus.sh                  # build if needed, then serve
#   notebooks/run-corpus.sh --notebook       # run the notebook first, then build and serve
#   notebooks/run-corpus.sh --sample 200000  # with --notebook: how many papers
#   notebooks/run-corpus.sh --rebuild        # discard the bundle and build it again
#   notebooks/run-corpus.sh --build-only     # build and stop
#   notebooks/run-corpus.sh --out DIR        # read build inputs from somewhere else
#
# **This is the localhost development shape and nothing here deploys anywhere.** Two things about
# it are development-only, exactly as `run_demo.sh` says of its own: the session credential is
# minted here and printed, and the planes bind to loopback with no TLS. Neither is anything to
# copy into an integration (`client-interaction.md` §7).
#
# ## What you get
#
# Four layers over the corpus, and the pair of them is the point:
#
#   clusters/kmeans    flat, `min_visible = 50`      — the control
#   clusters/hdbscan   a tree, `min_fraction = 0.05` — the condensed tree, whose children do not
#                                                      exhaust their parents
#   topics/kmeans      labels attached to the first
#   topics/hdbscan     labels attached to the second
#
# A viewport against the treed layer returns a **cut**: where a parent and a child are both visible
# to you, the child is what you get. Pass `artifact_budget` to trade depth for count.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

DATA="${TESSERA_DATA:-/home/joe/code/tessera/data}"
OUT="${TESSERA_NOTEBOOK_OUT:-$DATA/notebook}"
BUNDLE="${TESSERA_NOTEBOOK_BUNDLE:-bundles/notebook}"
VIEWER_PORT="${TESSERA_NOTEBOOK_VIEWER_PORT:-38901}"
SESSION_PORT="${TESSERA_NOTEBOOK_SESSION_PORT:-38902}"
CONTROL_PORT="${TESSERA_NOTEBOOK_CONTROL_PORT:-38903}"
SAMPLE="${TESSERA_NOTEBOOK_SAMPLE:-200000}"

run_notebook=0 rebuild=0 build_only=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --notebook)   run_notebook=1; shift ;;
    --rebuild)    rebuild=1; shift ;;
    --build-only) build_only=1; shift ;;
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

# ------------------------------------------------------------------------------- the build
[[ $rebuild -eq 1 ]] && rm -rf "$BUNDLE"
if [[ ! -d "$BUNDLE" ]]; then
  say "building $BUNDLE"
  cargo build --release -p tessera-cli
  ./target/release/tessera build \
    --points "$OUT/points.parquet" \
    --pairs "$OUT/pairs.parquet" \
    --schema "$OUT/schema.toml" \
    --layers "$OUT/layers.toml" \
    --artifacts "$OUT/artifacts.parquet" \
    --artifact-members "$OUT/members.parquet" \
    --values "archive=$OUT/archive.parquet" \
    --values "primary_category=$OUT/primary_category.parquet" \
    --out "$BUNDLE" --slice s0 --extent 0,65536,0,65536 --mint-id-key

  # **The build's own report on the hierarchy.** Every split that keeps members none of its
  # children hold is named here, which is what makes a cluster appearing without its children a
  # thing you read about rather than a thing you report.
  if [[ -f "$BUNDLE/reports/containment.json" ]]; then
    python3 - "$BUNDLE/reports/containment.json" <<'PY'
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
PY
  fi
else
  say "serving the existing $BUNDLE (--rebuild to discard it)"
fi

[[ $build_only -eq 1 ]] && { say "built; not serving (--build-only)"; exit 0; }

# ------------------------------------------------------------------------------- the server
# Minted per run rather than defaulted: `tessera serve` refuses to start without one, and refuses
# an inline value in the config outright, so there is nowhere for a shared secret to be left behind.
export TESSERA_SESSION_CRED="${TESSERA_SESSION_CRED:-$(head -c 24 /dev/urandom | base64)}"
export TESSERA_OPERATOR_CRED="${TESSERA_OPERATOR_CRED:-$(head -c 24 /dev/urandom | base64)}"

work="$(mktemp -d)"
config="$work/serve.toml"
cat > "$config" <<EOF
[bundle]
path = "$BUNDLE"
cache = "$work/cache"
wal = "$work/wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer = "127.0.0.1:$VIEWER_PORT"
session = "127.0.0.1:$SESSION_PORT"
control = "127.0.0.1:$CONTROL_PORT"
max_k = 5000
session_credential_env = "TESSERA_SESSION_CRED"
operator_credential_env = "TESSERA_OPERATOR_CRED"
EOF

say "starting tessera serve on :$VIEWER_PORT"
./target/release/tessera serve -c "$config" &
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
read -r broad_list narrow_id narrow_name broad_names <<<"$(python3 - "$OUT" <<'PY'
import sys, pathlib, collections
import pyarrow.parquet as pq
out = pathlib.Path(sys.argv[1])
terms = pq.read_table(out / "terms.parquet").to_pydict()
name_of = dict(zip(terms["term_id"], terms["descriptor"]))
counts = collections.Counter(pq.read_table(out / "pairs.parquet").column("term_id").to_pylist())
top = [t for t, _ in counts.most_common(8)]
print(",".join(f'"{t}"' for t in top),
      top[0], name_of[top[0]],
      "+".join(name_of[t] for t in top[:4]) + f"+{len(top) - 4} more")
PY
)"

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
    -d '{"auth_data":"'\$(printf '{"terms":["$narrow_id"]}' | base64 -w0)'"}' | jq -r .token)

  # broad is $broad_names; narrow is $narrow_name alone

Then ask the same question as each, and compare the count beside one cluster:

  curl -s -X POST http://127.0.0.1:$VIEWER_PORT/v1/viewport \\
    -H "authorization: Bearer \$BROAD" -H 'content-type: application/json' \\
    -d '{"slice":"s0","zoom":0,"bbox":[0,65536,0,65536],"k":1,
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

Ctrl-C to stop.
EOF

wait "$serve_pid"
