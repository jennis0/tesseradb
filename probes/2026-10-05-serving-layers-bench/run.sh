#!/usr/bin/env bash
# Rounds of the interactive bench against each binary, old and new alternately, with the store's
# prefetch off and on, each on a fresh server over the same bundle with its pages evicted first,
# then a restart over the kept cache. Before each round it waits, up to 20 minutes, for the
# one-minute load average to fall under 2, and records the load at each run's start and end.
#
#   bash probes/2026-10-05-serving-layers-bench/run.sh <old tessera> <old core index.js> <new tessera> \
#       [rounds] [first round] [prefix]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
old_bin=$1 old_core=$2 new_bin=$3 rounds=${4:-3} first=${5:-1} prefix=${6:-}
deployment=/home/joe/code/tessera/data/ladder/gbif-64p/bench-stage5
bundle=$(python3 -c "import tomllib,sys; d=tomllib.load(open(sys.argv[1]+'/tessera.toml','rb')); print(d['bundle']['path'])" "$deployment")
bundle=$(cd "$deployment" && realpath "$bundle")
mkdir -p "$here/runs"

evict() {
  python3 - "$bundle" <<'PY'
import os, sys
for root, _, files in os.walk(sys.argv[1]):
    for name in files:
        fd = os.open(os.path.join(root, name), os.O_RDONLY)
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        os.close(fd)
PY
}

quiet() {
  for _ in $(seq 1 120); do
    awk '{exit !($1 < 2)}' /proc/loadavg && return 0
    sleep 10
  done
}

cd "$repo"
for i in $(seq "$first" $((first + rounds - 1))); do
  quiet
  for mode in off on; do
    for side in old new; do
      if [ "$side" = old ]; then bin=$old_bin core=(--core "$old_core"); else bin=$new_bin core=(); fi
      if [ "$mode" = on ]; then pf=(--prefetch); else pf=(); fi
      name="$prefix$side-$mode-$i"
      evict
      uptime > "$here/runs/$name.load"
      python3 -m test_corpora.common.interactive_bench --deployment "$deployment" \
        --start --reopen --binary "$bin" "${core[@]}" "${pf[@]}" --scratch "$deployment/scratch-$side" \
        --targets 0.01,0.25,1 --per-tile 50 --out "$here/runs/$name.json" \
        > "$here/runs/$name.txt" 2> "$here/runs/$name.log"
      uptime >> "$here/runs/$name.load"
    done
  done
done
