#!/usr/bin/env bash
# The before and after runs for how a run reads its key filter: the bench built against the run
# reader at 72d87c112 (before), at dd78dddd5 (prev) and as committed (now), run on the largest run a
# unique index writes and on 233 million keys, with no filter and at 10:7. Each run waits until no
# serve benchmark or bundle build is running on the host. Writes
# raw-reads-<step>-<entries>-<round>.txt here, ROUNDS rounds of the three builds in turn.
#
#   BEFORE=... PREV=... NOW=... SCRATCH=/some/dir/on/the/disc bash run-reads.sh
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
SCRATCH=${SCRATCH:?a scratch directory on the disc the bundles live on}
for round in $(seq 1 "${ROUNDS:-3}"); do
  for entries in 67108864 233000000; do
    for step in before prev now; do
      case $step in
        before) bin=${BEFORE:?the bench built against the reader at 72d87c112} ;;
        prev) bin=${PREV:?the bench built against the reader at dd78dddd5} ;;
        now) bin=${NOW:?the bench built against the reader as committed} ;;
      esac
      while pgrep -f '[i]nteractive_bench|[s]erve_battery|[m]osaica build' > /dev/null; do
        sleep 30
      done
      out="$HERE/raw-reads-$step-$entries-$round.txt"
      echo "binary: $bin $(sha256sum < "$bin" | cut -c1-16)" > "$out"
      RAYON_NUM_THREADS=4 nice -n 10 "$bin" --dir "$SCRATCH" --entries "$entries" --keys dense \
        --filters 0,10:7 --lookups 2000 --cold-lookups 300 --batch 10000 >> "$out"
    done
  done
done
