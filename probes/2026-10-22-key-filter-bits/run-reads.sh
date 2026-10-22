#!/usr/bin/env bash
# The before and after runs for how a run reads its key filter: the bench built at each step, run
# on the largest run a unique index writes and on 233 million keys, with no filter and at 10:7.
# Each run waits until no serve benchmark or bundle build is running on the host. Writes
# raw-reads-<step>-<entries>-<round>.txt here, ROUNDS rounds of the four builds in turn.
#
#   BEFORE=... STEP1=... STEP2A=... STEP2=... SCRATCH=/some/dir/on/the/disc bash run-reads.sh
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
SCRATCH=${SCRATCH:?a scratch directory on the disc the bundles live on}
for round in $(seq 1 "${ROUNDS:-3}"); do
  for entries in 67108864 233000000; do
    for step in before step1 step2a step2; do
      case $step in
        before) bin=${BEFORE:?the bench built before the change} ;;
        step1) bin=${STEP1:?the bench built with the sequential read at open} ;;
        step2a) bin=${STEP2A:?the bench built to advise every filter page a batch probes} ;;
        step2) bin=${STEP2:?the bench built to advise only the pages the page cache lacks} ;;
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
