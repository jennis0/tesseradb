#!/usr/bin/env bash
# The row-major layout against the artifact-major one, swept over the artifact count. The claim
# being tested is that the row-major route is **flat** in it, so the ladder is the measurement.
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
run 100000000     1000 100 partition r1e8-p1e3
run 100000000    10000 100 partition r1e8-p1e4
run 100000000   100000 100 partition r1e8-p1e5
run 100000000  1000000 100 partition r1e8-p1e6
