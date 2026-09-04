#!/usr/bin/env bash
# The measurement sequence for this probe: one process per step, single-threaded, under
# `nice -n 10`, each step refusing to start with under 16 GB available (the box is shared).
# JSON lines land in `runs/<step>.jsonl`, the readable log beside it; `collate.py` folds them
# into `result.json` and prints the README's tables.
#
#   bash run.sh smoke        # small sizes, to check the harness
#   bash run.sh linearity    # (a)
#   bash run.sh shards       # (b), one mask at 10^8
#   bash run.sh tokens       # (b), 10^4 tokens held at once, one shape per process
#   bash run.sh all
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-/home/joe/code/tessera/target/release/epoch_shard_projection}
OUT=${OUT:-$HERE/runs}
DIR=${DIR:-${TMPDIR:-/tmp}/epoch-shard-projection-$$}
mkdir -p "$OUT" "$DIR"

# Wait for a quiet window: no compiler running and the one-minute load under 5, for up to ten
# minutes. The binary runs at nice 10 and another agent's build runs at nice 0, so a step that
# starts during a build measures the scheduler; each sample also records its own preemptions
# and CPU time, which is how a contaminated sample is recognised afterwards.
quiet() {
  for _ in $(seq 1 40); do
    if [ "$(pgrep -c -x rustc)" = 0 ] && awk '{exit !($1 < 5.0)}' /proc/loadavg; then
      return
    fi
    sleep 15
  done
  echo "no quiet window in ten minutes; running anyway: $(cat /proc/loadavg)" >&2
}

run() {
  local name=$1
  shift
  quiet
  echo "== $name: $* (load $(cut -d' ' -f1-3 /proc/loadavg))" >&2
  free -g | sed -n 2p >&2
  nice -n 10 "$BIN" --dir "$DIR" "$@" >"$OUT/$name.jsonl" 2>"$OUT/$name.log"
  tail -n +1 "$OUT/$name.log" | grep -v '^#' >&2 || true
}

# Every size twice, in separate processes and not back to back, because one process's figure
# at a size differed from another's by half on this shared box; `collate.py` shows both and
# fits the lower.
linearity() {
  for pass in a b; do
    run linear-1e6-$pass --part linearity --rows 1000000
    run linear-1e7-$pass --part linearity --rows 10000000
    run linear-1e8-$pass --part linearity --rows 100000000 --mask scattered,contiguous
    run linear-4e8-$pass --part linearity --rows 400000000
  done
}

shards() {
  run shards --part shards
}

tokens() {
  run tokens-one --part tokens --token-shape one
  run tokens-sharded --part tokens --token-shape sharded
}

case ${1:-all} in
  smoke)
    run smoke --part all --rows 1000000,10000000 --mask scattered,contiguous \
      --shard-rows 8000000 --tokens 200
    ;;
  linearity) linearity ;;
  shards) shards ;;
  tokens) tokens ;;
  all)
    linearity
    shards
    tokens
    ;;
  *)
    echo "usage: run.sh smoke|linearity|shards|tokens|all" >&2
    exit 1
    ;;
esac

rmdir "$DIR" 2>/dev/null || true
