#!/usr/bin/env bash
# The write side at several shards (sharding.md §8.3 and §8.4): for each shard count, the ingest
# cycle at half the corpus with its write cycle, each compaction recorded with its shard, seconds
# and peak memory, then the churn. A shard count N runs at a `[shards] size` of the corpus's items
# over N, rounded up, written into the base build's declaration and declared on every served copy.
#
#   cargo build --release -p mosaica-cli
#   (cd <ladder>/<corpus> && mosaica check && mosaica build)
#   LADDER=<ladder> OUT=<dir> ITEMS=<the corpus's items> bash run.sh cycle|churn|all
#
# CORPUS names the rung (geonames by default) and SHARDS the counts (1 2 8 32 128 by default; Tree
# of Life is run at 1 4 16). Each run writes its JSON under $OUT/<corpus>-<N>/.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$HERE/../..
LADDER=${LADDER:?the ladder directory holding the corpus}
OUT=${OUT:?a directory for the results}
ITEMS=${ITEMS:?the corpus items, which each shard count divides}
CORPUS=${CORPUS:-geonames}
SHARDS=${SHARDS:-1 2 8 32 128}
BINARY=${BINARY:-$REPO/target/release/mosaica}
RUNG=$LADDER/$CORPUS

size_for() {
  echo $(((ITEMS + $1 - 1) / $1))
}

cycle() {
  local n=$1 dir=$OUT/$CORPUS-$1
  mkdir -p "$dir/work"
  echo "== cycle $CORPUS at $n shards, size $(size_for "$n") ($(cut -d' ' -f1-3 /proc/loadavg))" >&2
  python3 -m test_corpora.common.ingest_cycle --rung-dir "$RUNG" --work "$dir/work" \
    --binary "$BINARY" --all-in-bundle "$RUNG/bundle" --fraction 0.5 --write-cycle \
    --shard-size "$(size_for "$n")" --out "$dir/cycle.json" >"$dir/cycle.log" 2>&1
}

churn() {
  local n=$1 dir=$OUT/$CORPUS-$1
  mkdir -p "$dir/work"
  echo "== churn $CORPUS at $n shards, size $(size_for "$n") ($(cut -d' ' -f1-3 /proc/loadavg))" >&2
  python3 -m test_corpora.common.ingest_cycle --rung-dir "$RUNG" --work "$dir/work" \
    --binary "$BINARY" --all-in-bundle "$RUNG/bundle" --churn-rounds 10 \
    --shard-size "$(size_for "$n")" --out "$dir/churn.json" >"$dir/churn.log" 2>&1
}

cd "$REPO"
case ${1:-all} in
  cycle) for n in $SHARDS; do cycle "$n"; done ;;
  churn) for n in $SHARDS; do churn "$n"; done ;;
  all)
    for n in $SHARDS; do
      cycle "$n"
      churn "$n"
    done
    ;;
  *)
    echo "usage: run.sh cycle|churn|all" >&2
    exit 1
    ;;
esac
echo "== done" >&2
