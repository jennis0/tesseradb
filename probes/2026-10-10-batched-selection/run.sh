#!/usr/bin/env bash
# The probe's runs: contiguous requests of 3,000 and 256 tiles, with tiles that abut as a segment's
# do and with each part jittered on its own as in probes/2026-10-09-shard-read-costs/. Prints
# nothing; tables.py reads medians.csv, which extract.py builds from the JSON this writes.
#
#   CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --release -p mosaica-bench --bin epoch_shard_treemap_mask
#   OUT=/tmp/batched bash run.sh && python3 extract.py /tmp/batched > medians.csv
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$HERE/../../target/release}
OUT=${OUT:?a directory for the JSON}
mkdir -p "$OUT"
for layout in abutting jittered; do
  for tiles in 3000 256; do
    flag=""; [ "$layout" = abutting ] && flag="--abutting"
    "$BIN/epoch_shard_treemap_mask" --shards 1,8,32,100 --scratch "$OUT/scratch" --layout viewport \
      --tiles "$tiles" --samples 3 --min-depth 6 --run-heavy-pct 0 $flag \
      --ops count,count_ranges,select_tiles,select_parts,select_batched \
      --out "$OUT/$layout-$tiles.json" > /dev/null
  done
done
