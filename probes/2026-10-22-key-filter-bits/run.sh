#!/usr/bin/env bash
# The probe's runs: one run of record-number keys at each corpus's run size, and one of keys
# spread over the u64 range, each written once per key filter shape. Writes raw-<name>.txt here;
# tables.py reads them.
#
#   CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --release -p mosaica-bench --bin key_index_scale
#   SCRATCH=/some/dir/on/the/disc bash run.sh && python3 tables.py
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$HERE/../../target/release}
SCRATCH=${SCRATCH:?a scratch directory on the disc the bundles live on}
SHAPES=0,6:4,6:7,8:5,8:6,8:7,10:7,12:7,16:7
run() {
  local name=$1; shift
  RAYON_NUM_THREADS=4 nice -n 10 "$BIN/key_index_scale" --dir "$SCRATCH" --filters "$SHAPES" \
    --lookups 2000 --cold-lookups 300 --batch 10000 "$@" > "$HERE/raw-$name.txt"
}
# GeoNames: 13.5 million items, one run at one shard.
run geonames-13.5m --entries 13500000 --keys dense
# The largest run a unique index writes (RUN_MAX_ENTRIES, 2^26): every base run of Tree of Life
# and GBIF.
run run-max-67m --entries 67108864 --keys dense
# Tree of Life: 233 million items as one run, to show what a run four times the largest costs.
run treeoflife-233m --entries 233000000 --keys dense
# The largest run again with keys spread over the u64 range, which pack wider.
run run-max-67m-random --entries 67108864 --keys random
