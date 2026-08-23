#!/usr/bin/env bash
# **Superseded by `run-remeasure.sh`** (2026-08-21). The routes this script measured admitted the
# settled half without a masked candidacy probe, and its fixture could not have shown that: read
# the README's "corrections after review" before quoting anything in `data/` that came from here.
# The headline ladder, re-run after the extent test landed. One configuration per process.
set -u
BIN=./target/release/artifact_serving_scale
D=probes/2026-08-20-artifact-serving-scale/data
mkdir -p "$D"
run () {
  echo "=== $5"
  /usr/bin/time -v "$BIN" --rows "$1" --artifacts "$2" --members "$3" --arm "$4" \
    > "$D/$5.csv" 2> "$D/$5.log"
  grep -E "^#|Maximum resident" "$D/$5.log"
}
run 100000000   100000 1000 runs r1e8-a1e5
run 100000000  1000000  100 runs r1e8-a1e6
run 100000000 10000000   10 runs r1e8-a1e7
