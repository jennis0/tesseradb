#!/usr/bin/env bash
# Build one rung's identity bands, run the band route against the shipped selection, and report.
#
#   bash probes/2026-09-14-identity-bands/run.sh [<rung dir>] [<out dir>]
#
# The rung directory defaults to `data/ladder/gbif-64p`, the 25,846,007-row GBIF prefix. It is
# never `data/ladder/gbif`: that rung's bundle is 196 GiB and opening it is a decision, not a
# default. Pass it explicitly to measure it.
#
# The out directory holds this run's whole state — a rebuilt bundle, the band files, the probe's
# `results.json` and the report — and defaults to `$WORK` or a directory under `$TMPDIR`. The
# bundle is rebuilt there rather than read from the rung, because a rung's committed bundle may
# predate the cut index this route reads.
#
# Everything long is `nice -n 19`, and the builder and the probe run inside
# `systemd-run --user --scope -p MemoryMax -p MemorySwapMax`, which needs no root on a box whose
# memory controller is delegated to the user slice. Nothing here matches a process by name or
# signals one: another session's server is not ours.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tree="$(cd "$here/../.." && pwd)"
# `data/` is untracked, so in a git worktree it lives in the main checkout — the parent of the
# shared git directory.
main="$(dirname "$(git -C "$tree" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)")"

rung="${1:-}"
if [ -z "$rung" ]; then
  if [ -d "$tree/data/ladder/gbif-64p" ]; then rung="$tree/data/ladder/gbif-64p"; else rung="$main/data/ladder/gbif-64p"; fi
fi
out="${2:-${WORK:-${TMPDIR:-/tmp}/identity-bands}}"
bin="${BIN:-$tree/target/release}"
cap="${CAP:-8G}"
swap="${SWAP:-2G}"
budget="${BUDGET:-8g}"
targets="${TARGETS:-0.01,0.05,0.10,0.25,0.50,1.0}"
mark_budget="${MARK_BUDGET:-2000000}"
k_small="${K_SMALL:-30}"
conditions="${CONDITIONS:-cold,hot}"

[ -d "$rung" ] || { echo "no rung at $rung" >&2; exit 1; }
case "$rung" in
  */gbif) echo "refusing $rung: pass the rung-6 path deliberately, not as a default" >&2 ;;
esac
for prog in "$bin/tessera" "$bin/identity_bands_build" "$bin/identity_bands_probe"; do
  [ -x "$prog" ] || { echo "missing $prog — build with: CARGO_TARGET_DIR=$tree/target nice -n 19 cargo build --release -j 6 -p tessera-cli -p tessera-bench" >&2; exit 1; }
done
mkdir -p "$out"

# The identity key and the rung's credentials. The deployment file below names the variable; the
# value never goes in it (configuration.md).
set -a; . "$rung/.env"; set +a

ranks="${RANKS:-}"
if [ -z "$ranks" ]; then
  for candidate in "$rung/country-ranks.json" "$rung/branch-ranks.json"; do
    [ -f "$candidate" ] && ranks="$candidate" && break
  done
fi
[ -f "${ranks:-}" ] || { echo "no ranks file in $rung; set RANKS" >&2; exit 1; }

# ---- 1. The bundle, rebuilt into the out directory with this tree's own binary.
if [ ! -f "$out/bundle/CURRENT" ]; then
  cat > "$out/tessera.toml" <<EOF
# Written by probes/2026-09-14-identity-bands/run.sh. The rung's own deployment file names a
# bundle inside the rung and ports another session may be serving on; this one names a bundle,
# cache and WAL under this run's own directory.

[bundle]
path  = "$out/bundle"
cache = "$out/cache"
wal   = "$out/wal.log"

[build]
schema = "$rung/corpus.toml"

[plugin]
module = "builtin:passthrough"

[identity]
env = "TESSERA_IDENTITY_KEY"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:8241"
session = "127.0.0.1:8242"
control = "127.0.0.1:8243"
max_k   = 5000
session_credential_env  = "$(python3 - "$rung/tessera.toml" <<'PY'
import re, sys
print(re.search(r'session_credential_env\s*=\s*"([^"]+)"', open(sys.argv[1]).read()).group(1))
PY
)"
operator_credential_env = "$(python3 - "$rung/tessera.toml" <<'PY'
import re, sys
print(re.search(r'operator_credential_env\s*=\s*"([^"]+)"', open(sys.argv[1]).read()).group(1))
PY
)"
EOF
  echo "building $rung into $out/bundle"
  nice -n 19 "$bin/tessera" build --deployment "$out/tessera.toml" --config "$rung/corpus.toml" \
    --no-oracle-pairs --memory-budget "$budget" > "$out/build.log" 2>&1 \
    || { echo "the build failed; see $out/build.log" >&2; tail -5 "$out/build.log" >&2; exit 1; }
  tail -2 "$out/build.log"
fi

# ---- 2. The bands, from the view's one segment.
segments="$(find "$out/bundle" -name columns.arrow -printf '%h\n' | sort)"
count="$(printf '%s\n' "$segments" | grep -c . )"
if [ "$count" != 1 ]; then
  echo "the bundle holds $count segments; this probe evaluates one" >&2
  exit 1
fi
echo "bands from $segments"
systemd-run --user --scope --collect --quiet -p "MemoryMax=$cap" -p "MemorySwapMax=$swap" -- \
  nice -n 19 "$bin/identity_bands_build" --segment "$segments" --out "$out/bands" \
  > "$out/bands-build.log" 2>&1 \
  || { echo "the band build failed; see $out/bands-build.log" >&2; tail -20 "$out/bands-build.log" >&2; exit 1; }

# ---- 3. The principal ladder, composed by the battery's own greedy over the rung's ranks.
mapfile -t principals < <(
  cd "$tree" && python3 - "$ranks" "$out/bands/bands.json" "$targets" <<'PY'
import json, sys
from test_corpora.common.serve_battery import compose_ladder
ranks = json.load(open(sys.argv[1]))
rows = json.load(open(sys.argv[2]))["row_count"]
targets = [float(t) for t in sys.argv[3].split(",")]
for rung in compose_ladder(ranks, rows, targets):
    print(f"p{round(rung['target'] * 100)}=" + ",".join(rung["terms"]))
PY
)
[ "${#principals[@]}" -gt 0 ] || { echo "no principal ladder composed" >&2; exit 1; }
args=()
for principal in "${principals[@]}"; do
  args+=(--principal "$principal")
  echo "principal ${principal%%=*}: $(( $(grep -o ',' <<< "$principal" | wc -l) + 1 )) term(s)"
done

# ---- 4. The probe.
export TESSERA_PROBE_COMMIT="$(git -C "$tree" rev-parse --short HEAD 2>/dev/null)"
export TESSERA_PROBE_BOX="$(uname -sr) $(nproc) cores $(free -g | awk '/^Mem:/ {print $2}') GiB, cap $cap"
systemd-run --user --scope --collect --quiet -p "MemoryMax=$cap" -p "MemorySwapMax=$swap" -- \
  nice -n 19 "$bin/identity_bands_probe" \
    --bundle "$out/bundle" --bands "$out/bands" "${args[@]}" \
    --budget "$mark_budget" --k-small "$k_small" --conditions "$conditions" \
    --out "$out/results.json" 2>&1 | tee "$out/probe.log"

# ---- 5. The report.
python3 "$here/report.py" "$out/results.json" | tee "$out/report.md"
