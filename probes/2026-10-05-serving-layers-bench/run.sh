#!/usr/bin/env bash
# Three runs of the interactive bench against each binary, alternating old and new, each on a fresh
# server over the same bundle with its pages evicted first, then a restart over the kept cache.
#
#   bash probes/2026-10-05-serving-layers-bench/run.sh <old tessera> <old core index.js> <new tessera> [repeats]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
old_bin=$1 old_core=$2 new_bin=$3 repeats=${4:-3}
deployment=/home/joe/code/tessera/data/ladder/gbif-64p/bench-stage5
bundle=/home/joe/code/tessera/data/ladder/gbif-64p/bundle-stage5
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

cd "$repo"
for i in $(seq 1 "$repeats"); do
  for side in old new; do
    if [ "$side" = old ]; then bin=$old_bin core=(--core "$old_core"); else bin=$new_bin core=(); fi
    evict
    uptime > "$here/runs/$side-$i.load"
    python3 -m test_corpora.common.interactive_bench --deployment "$deployment" \
      --start --reopen --binary "$bin" "${core[@]}" --scratch "$deployment/scratch-$side" \
      --targets 0.01,0.25,1 --per-tile 50 --out "$here/runs/$side-$i.json" \
      > "$here/runs/$side-$i.txt" 2> "$here/runs/$side-$i.log"
    uptime >> "$here/runs/$side-$i.load"
  done
done
