#!/usr/bin/env bash
# The measurement sequence for this probe, one process per step, run one after another so that no
# two single-threaded steps share the machine. Each step writes its JSON and the readable tables
# into this directory, and its progress log into `logs/`.
#
#   bash run.sh treemap      # the sweep's mask operations, three tile layouts
#   bash run.sh projection   # Permutation::project per leaf
#   bash run.sh all
#
# Build first:
#   CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --release -p mosaica-bench \
#     --bin epoch_shard_treemap_mask --bin epoch_shard_projection
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$HERE/../../target/release}
SCRATCH=${SCRATCH:-${TMPDIR:-/tmp}/shard-read-costs-$$}
# The projection bench refuses to start below this much available memory, in GB.
FLOOR=${FLOOR:-10}
mkdir -p "$HERE/logs" "$HERE/runs" "$SCRATCH"

treemap() {
  local name=$1
  shift
  echo "== treemap $name: $* ($(cut -d' ' -f1-3 /proc/loadavg))" >&2
  "$BIN/epoch_shard_treemap_mask" --shards 1,8,32,100 --scratch "$SCRATCH/treemap" \
    --out "$HERE/treemap-$name.json" "$@" >"$HERE/treemap-$name.md" 2>"$HERE/logs/treemap-$name.log"
}

projection() {
  local name=$1
  shift
  echo "== projection $name: $* ($(cut -d' ' -f1-3 /proc/loadavg))" >&2
  "$BIN/epoch_shard_projection" --dir "$SCRATCH/projection" --min-available-gb "$FLOOR" "$@" \
    >"$HERE/runs/$name.jsonl" 2>"$HERE/logs/projection-$name.log"
}

treemap_all() {
  # The 2026-09-04 configuration, at N = 32 and 100 as well, with the figures walk.
  treemap random-256 --layout random --tiles 256 --figures-artifacts 1000000
  # A screen's request: a contiguous block of tiles.
  treemap viewport-3000 --layout viewport --tiles 3000 --samples 3
  treemap viewport-256 --layout viewport --tiles 256 --samples 3
}

projection_all() {
  for rows in 1000000 10000000 100000000 400000000; do
    projection "linear-$rows" --part linearity --rows "$rows"
  done
  for n in 8 32 100; do
    projection "shards-$n" --part shards --shards "$n"
    projection "shards-$n-scratch" --part shards --shards "$n" --scratch
  done
  projection tokens-one --part tokens --token-shape one
  projection tokens-sharded --part tokens --token-shape sharded
}

case ${1:-all} in
  treemap) treemap_all ;;
  projection) projection_all ;;
  all)
    treemap_all
    projection_all
    ;;
  *)
    echo "usage: run.sh treemap|projection|all" >&2
    exit 1
    ;;
esac
rm -rf "$SCRATCH"
echo "== done" >&2
