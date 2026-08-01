#!/usr/bin/env bash
# Run a benchmark under an exclusive, machine-wide slot.
#
# **Why this exists.** Several tracks work this repo in parallel worktrees, and a benchmark is
# only meaningful against a quiet box. One fix round measured `tile_sweep_k0` at 3.55 ms and
# `compose` at 99 ns — +50% and +43% — because three other tracks were compiling at load average
# 12.1. It caught that and discarded the run, but only because it happened to look; the next
# worker will not. An ad-hoc "check the load average" instruction is
# not a mechanism, and the viewport-bench regression memo
# (docs/evidence/memos/2026-07-31-viewport-bench-regression.md) records how expensive a
# misattributed measurement is to unpick afterwards.
#
# Two guarantees, in order:
#   1. **Exclusion** — one bench at a time across every worktree, via `flock` on a fixed path.
#      A waiting run blocks rather than measuring through someone else's build.
#   2. **Quiet** — once the slot is held, wait for the 1-minute load average to fall below a
#      threshold, so a build that has just released the lock is not still draining.
#
# Neither makes a shared dev box into a measurement rig. They make the common, cheap failure
# (measuring through a concurrent build) hard to hit by accident.
#
# Usage:
#   scripts/bench-slot.sh cargo bench -p tessera-engine
#   BENCH_SLOT_MAX_LOAD=1.5 BENCH_SLOT_TIMEOUT=1800 scripts/bench-slot.sh ./target/release/tessera-bench ...
#
# Environment:
#   BENCH_SLOT_MAX_LOAD   1-minute loadavg to wait for before starting  (default 2.0)
#   BENCH_SLOT_TIMEOUT    seconds to wait for the lock                  (default 3600)
#   BENCH_SLOT_SETTLE     seconds to wait for load between polls        (default 30)
#   BENCH_SLOT_SKIP_LOAD  set to 1 to take the lock and start at once
#
# Exit codes: the benchmark's own, or 75 (EX_TEMPFAIL) if the lock could not be taken in time.

set -uo pipefail

LOCK_FILE="${BENCH_SLOT_LOCK:-/tmp/tessera-bench.slot.lock}"
MAX_LOAD="${BENCH_SLOT_MAX_LOAD:-2.0}"
TIMEOUT="${BENCH_SLOT_TIMEOUT:-3600}"
SETTLE="${BENCH_SLOT_SETTLE:-30}"

if [ "$#" -eq 0 ]; then
  echo "usage: ${BASH_SOURCE[0]##*/} <command> [args...]" >&2
  exit 2
fi

if ! command -v flock >/dev/null 2>&1; then
  echo "bench-slot: flock(1) not found — refusing to run unserialised." >&2
  echo "            Install util-linux, or set BENCH_SLOT_SKIP_LOAD=1 and serialise by hand." >&2
  exit 75
fi

exec 9>"$LOCK_FILE" || { echo "bench-slot: cannot open $LOCK_FILE" >&2; exit 75; }

if ! flock --exclusive --wait "$TIMEOUT" 9; then
  echo "bench-slot: another benchmark held the slot for more than ${TIMEOUT}s — not measuring." >&2
  echo "            A measurement taken alongside one is worse than none: it will be believed." >&2
  exit 75
fi

echo "bench-slot: slot acquired ($LOCK_FILE)" >&2

if [ "${BENCH_SLOT_SKIP_LOAD:-0}" != "1" ]; then
  # Two consecutive readings, so a load average still falling from a just-finished build does not
  # let a run start on the way down.
  quiet_readings=0
  waited=0
  while [ "$quiet_readings" -lt 2 ]; do
    load="$(awk '{print $1}' /proc/loadavg 2>/dev/null || echo 0)"
    if awk -v l="$load" -v m="$MAX_LOAD" 'BEGIN { exit !(l < m) }'; then
      quiet_readings=$((quiet_readings + 1))
    else
      quiet_readings=0
      if [ "$waited" -ge "$TIMEOUT" ]; then
        echo "bench-slot: load stayed above $MAX_LOAD for ${waited}s (now $load) — measuring anyway." >&2
        echo "            RECORD THIS with the numbers; they are not comparable to a quiet-box run." >&2
        break
      fi
      echo "bench-slot: loadavg $load >= $MAX_LOAD, waiting ${SETTLE}s" >&2
    fi
    [ "$quiet_readings" -lt 2 ] && { sleep "$SETTLE"; waited=$((waited + SETTLE)); }
  done
  echo "bench-slot: loadavg $(awk '{print $1}' /proc/loadavg) — starting" >&2
fi

"$@"
status=$?
echo "bench-slot: released (exit $status)" >&2
exit "$status"
