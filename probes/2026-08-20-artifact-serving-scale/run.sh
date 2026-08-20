#!/usr/bin/env bash
# The campaign's sweep. One configuration per process — the residency campaign's rule, and it
# applies to time as well: a second population built in a warm allocator measures a machine state
# rather than a design.
set -u
BIN=./target/release/artifact_serving_scale
D=probes/2026-08-20-artifact-serving-scale/data
mkdir -p "$D"

# The artifact-count axis at a fixed corpus, with members scaled so the layer partitions it —
# which is what a clustering over a corpus is. This is the axis the target names.
run () { # rows artifacts members tag
  echo "=== $4"
  /usr/bin/time -v "$BIN" --rows "$1" --artifacts "$2" --members "$3" \
    > "$D/$4.csv" 2> "$D/$4.log"
  grep -E "^#|Maximum resident" "$D/$4.log"
}

run 100000000  100000   1000 r1e8-a1e5
run 100000000  1000000   100 r1e8-a1e6
run 100000000 10000000    10 r1e8-a1e7
