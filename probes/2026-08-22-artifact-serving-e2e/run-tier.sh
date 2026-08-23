#!/usr/bin/env bash
# One corpus tier, end to end: fixture, census, grid, concurrency, fold, ingest, collation.
#
#   bash probes/2026-08-22-artifact-serving-e2e/run-tier.sh <label> <n> [step ...]
#
# e.g. `run-tier.sh 1e7 10000000` or `run-tier.sh 2.5e8 250000000 grid concurrency`.
#
# **The order is not arbitrary.** Census, grid and concurrency read the bundle and leave it as they
# found it; fold and ingest write to it. So the read-only steps come first, and a tier whose fold
# step has run cannot have its grid re-measured without rebuilding — the bundle now carries rows
# the earlier steps did not see.
#
# **One tier on disk at a time.** `fixture.py` deletes the materialised inputs the moment the
# bundle verifies, and every step that writes checks a 10 GB floor first and stops rather than
# filling the disk. Run tiers in ascending size, and delete the previous tier's work directory
# before starting the next.
#
# Build everything release **before** running this — a compile beside a measurement is a different
# machine, and the campaign's timing-sensitive arms are the ones that would notice:
#
#   cargo build --release --workspace --bins
#   cargo build --release -p tessera-bench
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
LABEL="${1:?usage: run-tier.sh <label> <n> [step ...]}"
N="${2:?usage: run-tier.sh <label> <n> [step ...]}"
shift 2

WORK="${CAMPAIGN_WORK:-/home/joe/.cache/tessera-campaign}/t$LABEL"
LOGS="$WORK/logs"
ITERATIONS="${ITERATIONS:-9}"
LOAD_SECONDS="${LOAD_SECONDS:-45}"
SESSIONS="${SESSIONS:-32}"
mkdir -p "$LOGS"

want () {
  step="$1"; shift
  [ $# -eq 0 ] && return 0
  case " $* " in *" $step "*) return 0;; esac
  return 1
}

step () {
  name="$1"; shift
  echo "=== $LABEL / $name"
  ( cd "$HERE" && timeout "${STEP_TIMEOUT:-86400}" python3 -u "$@" ) \
    > "$LOGS/$name.log" 2>&1 \
    || { echo "!!! $name FAILED — see $LOGS/$name.log" >&2; tail -20 "$LOGS/$name.log" >&2; }
  tail -6 "$LOGS/$name.log"
}

want fixture "$@" && step fixture fixture.py --work "$WORK" --n "$N"
want census "$@" && step census census.py --work "$WORK" --n "$N"
want grid "$@" && step grid grid.py --work "$WORK" --iterations "$ITERATIONS"
want concurrency "$@" && step concurrency concurrency.py --work "$WORK" \
  --levels 1 8 32 128 --seconds "$LOAD_SECONDS"
want fold "$@" && step fold fold_under_load.py --work "$WORK" --n "$N" \
  --sessions "$SESSIONS" --settle 30
want ingest "$@" && step ingest ingest_during_serving.py --work "$WORK" --n "$N" \
  --sessions "$SESSIONS" --seconds 120
want collate "$@" && ( cd "$HERE" && python3 collate.py --work "$WORK" --tier "$LABEL" )

echo
df -h "$WORK" | tail -1
