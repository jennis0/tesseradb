#!/usr/bin/env bash
# Whether the build's memory and its writes stay bounded as the row count grows.
#
# Builds prefixes of the GBIF ladder corpus with two binaries, samples each build's process and
# its bundle root beside it (`sample.py`), and prints per-stage memory, write amplification and
# fault figures (`report.py`). Each bundle is deleted as soon as its run has been sampled, so
# the disk holds one bundle at a time.
#
# A prefix is `--limit N` over the points, and the membership file cut to the same prefix
# (`limit_members.py`): `--limit` is a prefix of the points alone, and a membership row naming
# an entity past it is refused by the layer publication. The cut files are kept in WORK and
# reused across binaries and runs.
#
#   BEFORE=path AFTER=path LADDER=dir WORK=dir \
#   LIMITS="16299326 30104813 64657133 125789091" bash probes/2026-09-12-bounded-assembly/run.sh
#
# LADDER is `data/ladder`, holding a prepared `gbif` whose `.env` carries the identity key.
# WORK wants room for the largest single bundle, on the same filesystem as the builds. The
# limits are the ladder's rungs; `LIMITS="2000000"` is a short run that exercises the scripts.
#
# The build runs in its own session under `setsid`, and this script kills only the pid it
# started. Nothing here matches a process by name: another session's `tessera` is not ours.
set -uo pipefail
before="${BEFORE:?BEFORE is the binary to measure against}"
after="${AFTER:?AFTER is the binary to measure}"
ladder="${LADDER:?LADDER is data/ladder}"
work="${WORK:?WORK is the scratch directory}"
limits="${LIMITS:-16299326 30104813 64657133 125789091}"
budget="${BUDGET:-24g}"
here="$(cd "$(dirname "$0")" && pwd)"
corpus="$ladder/gbif"
[ -d "$corpus" ] || { echo "no corpus at $corpus" >&2; exit 1; }
mkdir -p "$work"

# The identity key and the corpus's credentials. Exported for the build only.
set -a; . "$corpus/.env"; set +a

one_build() {
  local which="$1" limit="$2" binary="$3"
  local out="$work/$which/$limit" members="$work/members-$limit.parquet"
  rm -rf "$out"; mkdir -p "$out"
  local pidfile="$out/pid" rcfile="$out/rc"

  # The wrapper writes the build's own pid and, when it ends, its exit code. `setsid` puts the
  # build in a new session so nothing this script does to its own group reaches it.
  setsid bash -c '
    pidfile="$1"; rcfile="$2"; shift 2
    "$@" & child=$!
    echo "$child" > "$pidfile"
    wait "$child"
    echo "$?" > "$rcfile"
  ' _ "$pidfile" "$rcfile" \
    "$binary" build \
      --deployment "$corpus/tessera.toml" --config "$corpus/corpus.toml" \
      --out "$out/bundle" --limit "$limit" --file "taxonomy=$members" \
      --stage-timings --stage-timings-json "$out/stages.json" \
      --no-oracle-pairs --memory-budget "$budget" \
    > "$out/build.log" 2>&1 &

  local waited=0 pid=
  while [ -z "$pid" ]; do
    [ -s "$pidfile" ] && pid="$(cat "$pidfile")"
    waited=$((waited + 1))
    [ "$waited" -gt 200 ] && { echo "the build never reported a pid" >&2; return 1; }
    [ -z "$pid" ] && sleep 0.1
  done
  # The sampler reads /proc for this pid alone, so a pid that is not the build would report
  # another process's memory as the build's.
  case "$(ps -o comm= -p "$pid" 2>/dev/null)" in
    *tessera*) ;;
    *) echo "pid $pid is not the build; refusing to sample it" >&2; return 1 ;;
  esac

  current_pid="$pid"
  python3 "$here/sample.py" "$pid" "$out/bundle" "$out/proc.tsv" &
  local sampler=$!
  while [ ! -s "$rcfile" ]; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
  wait "$sampler" 2>/dev/null
  local rc; rc="$(cat "$rcfile" 2>/dev/null || echo unknown)"
  local samples; samples="$(($(wc -l < "$out/proc.tsv") - 1))"
  echo "$which limit=$limit exit=$rc samples=$samples"
  [ "$rc" = 0 ] || tail -5 "$out/build.log"
  current_pid=
  rm -rf "$out/bundle"
  return 0
}

trap '[ -n "${current_pid:-}" ] && kill "$current_pid" 2>/dev/null' INT TERM

for limit in $limits; do
  [ -s "$work/members-$limit.parquet" ] || \
    python3 "$here/limit_members.py" "$corpus/members-taxonomy.parquet" "$limit" \
      "$work/members-$limit.parquet"
  one_build before "$limit" "$before"
  one_build after "$limit" "$after"
  echo
  echo "# $limit rows"
  echo
  python3 "$here/report.py" "before=$work/before/$limit" "after=$work/after/$limit" \
    | tee "$work/report-$limit.md"
  echo
done
