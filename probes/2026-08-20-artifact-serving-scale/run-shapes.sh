#!/usr/bin/env bash
# The membership-shape axis. Locality is what decides every cost here, and the three arms bracket
# it: a clustering is somewhere, a boundary is somewhere larger, an attribute predicate is
# everywhere. One configuration per process.
set -u
BIN=./target/release/artifact_serving_scale
D=probes/2026-08-20-artifact-serving-scale/data
mkdir -p "$D"
run () { # rows artifacts members arm tag
  echo "=== $5"
  /usr/bin/time -v "$BIN" --rows "$1" --artifacts "$2" --members "$3" --arm "$4" \
    > "$D/$5.csv" 2> "$D/$5.log"
  grep -E "^#|Maximum resident" "$D/$5.log"
}
# Where does a layer with no locality stop fitting a budget? The scaling ladder answers it.
run 100000000     1000 100 scattered r1e8-s1e3-scattered
run 100000000    10000 100 scattered r1e8-s1e4-scattered
run 100000000   100000 100 scattered r1e8-s1e5-scattered
run 100000000  1000000 100 scattered r1e8-s1e6-scattered
# And the middle shape, at the count a coarse level really has.
run 100000000    10000 100 regions   r1e8-a1e4-regions
run 100000000  1000000 100 regions   r1e8-a1e6-regions
