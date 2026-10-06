#!/usr/bin/env bash
# Rounds of the interactive bench, old and new binaries alternately, with the store's prefetch off
# and then on, each on a fresh server whose bundle's pages the bench evicts, then a restart over
# the kept cache. A prefetch-on run takes the same round's prefetch-off run as its reference, so
# the prefetch's work is kept out of its points and layers. Before each run it waits, up to 20
# minutes, for the one-minute load average to fall under 2, and it records the load at each run's
# start and end.
#
#   DEPLOYMENT=<deployment dir> [SIDES="old new"] [MODES="off on"] \
#     bash probes/2026-10-05-serving-layers-bench/run.sh <old tessera> <old core index.js> <new tessera> \
#       [rounds] [first round] [prefix]
#
# SIDES names the binaries to run: "new" alone takes "-" for the old binary and core.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
old_bin=$1 old_core=$2 new_bin=$3 rounds=${4:-3} first=${5:-1} prefix=${6:-}
deployment=${DEPLOYMENT:?set DEPLOYMENT to the deployment directory the bench serves}
sides=${SIDES:-old new} modes=${MODES:-off on}
mkdir -p "$here/runs"

quiet() {
  for _ in $(seq 1 120); do
    awk '{exit !($1 < 2)}' /proc/loadavg && return 0
    sleep 10
  done
}

cd "$repo"
for i in $(seq "$first" $((first + rounds - 1))); do
  for mode in $modes; do
    for side in $sides; do
      if [ "$side" = old ]; then bin=$old_bin core=(--core "$old_core"); else bin=$new_bin core=(); fi
      reference="$here/runs/$prefix$side-off-$i.json"
      if [ "$mode" = on ]; then
        pf=(--prefetch)
        if [ -f "$reference" ]; then pf+=(--reference "$reference"); fi
      else
        pf=()
      fi
      name="$prefix$side-$mode-$i"
      quiet
      uptime > "$here/runs/$name.load"
      python3 -m test_corpora.common.interactive_bench --deployment "$deployment" \
        --start --reopen --binary "$bin" "${core[@]}" "${pf[@]}" --scratch "$deployment/bench-scratch-$side" \
        --targets 0.01,0.25,1 --per-tile 50 --out "$here/runs/$name.json" \
        > "$here/runs/$name.txt" 2> "$here/runs/$name.log"
      uptime >> "$here/runs/$name.load"
    done
  done
done
