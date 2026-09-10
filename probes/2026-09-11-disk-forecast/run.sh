#!/usr/bin/env bash
# The disk forecast against the peaks the 2026-09-10 probe measured, and the bundle either binary
# writes.
#
# Part one prints each binary's forecast over four prefixes of the GBIF ladder corpus, by running
# the build until the pre-flight has spoken and then stopping it: the forecast is arithmetic over
# the corpus and does not need the build that follows. The peaks it is read against are
# `probes/2026-09-10-build-disk/`'s, measured at the same four row counts.
#
# Part two builds two whole ladder corpora with each binary and compares the bundles file by file
# (`docs/ingest-campaign.md` §4c): only `MANIFEST.json`'s `created_at` and the `CURRENT` that
# digests it may differ.
#
#   BEFORE=path AFTER=path LADDER=dir WORK=dir bash probes/2026-09-11-disk-forecast/run.sh
#
# ROW_GROUPS and IDENTITY name the prefixes and the corpora; the defaults are the four row counts
# the peaks were measured at and the two corpora the bundles are compared over.
#
# The two binaries must be named so `ps -o comm=` can tell them from a wrapper shell — `tessera-*`.
# WORK wants ~15 GB on the same filesystem as the builds.
set -uo pipefail
before="${BEFORE:?BEFORE is the binary to measure against}"
after="${AFTER:?AFTER is the binary to measure}"
ladder="${LADDER:?LADDER is data/ladder}"
work="${WORK:?WORK is the scratch directory}"
here="$(cd "$(dirname "$0")" && pwd)"
slice="$here/../2026-09-10-build-disk/slice.py"
# Not `GROUPS`: bash sets that itself, to the caller's group ids, so a default would never be
# reached and the slice would be taken at whatever gid the user has.
groups="${ROW_GROUPS:-25 50 100 200}"
mkdir -p "$work"

# ---- part one: what each binary's pre-flight says ---------------------------------------------
forecast() {
  local corpus="$1" binary="$2" out="$3"
  rm -rf "$out"; mkdir -p "$out"
  set -a; . "$corpus/.env"; set +a
  "$binary" build --deployment "$corpus/tessera.toml" --config "$corpus/corpus.toml" \
    --out "$out/bundle" > "$out/out.log" 2>&1 &
  local pid=$!
  # The pid the loop waits on is the build's own: a wrapper shell's would fall through at once and
  # record a forecast that was never printed.
  case "$(ps -o comm= -p "$pid" 2>/dev/null)" in
    tessera*) ;;
    *) echo "the pid is not the build" >&2; return 1 ;;
  esac
  while kill -0 "$pid" 2>/dev/null; do
    grep -q '^disk: ' "$out/out.log" 2>/dev/null && { kill "$pid"; break; }
    sleep 0.5
  done
  wait "$pid" 2>/dev/null
  grep -E '^(columns|disk): ' "$out/out.log" || echo "no forecast printed"
  rm -rf "$out/bundle"
}
for g in $groups; do
  [ -d "$work/gbif-${g}m" ] || python3 "$slice" "$g" "$work/gbif-${g}m" "$ladder/gbif"
done
for g in $groups; do
  for which in before after; do
    binary="$([ "$which" = before ] && echo "$before" || echo "$after")"
    printf '%-6s %-7s ' "${g}m" "$which"
    forecast "$work/gbif-${g}m" "$binary" "$work/fc-${g}m-${which}"
  done
done

# ---- part two: the same bundle, byte for byte -------------------------------------------------
for name in ${IDENTITY:-treeoflife-1m medcpt-1m}; do
  corpus="$ladder/$name"
  [ -d "$corpus" ] || { echo "$name: absent"; continue; }
  [ -f "$corpus/.env" ] && { set -a; . "$corpus/.env"; set +a; }
  for which in before after; do
    binary="$([ "$which" = before ] && echo "$before" || echo "$after")"
    rm -rf "$work/$name-$which"
    ( cd "$corpus" && "$binary" build --deployment tessera.toml --config corpus.toml \
        --out "$work/$name-$which" > "$work/$name-$which.log" 2>&1 ) \
      || { echo "$name: $which build failed"; tail -5 "$work/$name-$which.log"; continue; }
  done
  diff -rq "$work/$name-before" "$work/$name-after" | grep -v -E "MANIFEST\.json|CURRENT"
  python3 - "$work/$name-before" "$work/$name-after" "$name" <<'PY'
import json, pathlib, sys
a, b, name = sys.argv[1], sys.argv[2], sys.argv[3]
fields = []
for pa in sorted(pathlib.Path(a).glob("v*/MANIFEST.json")):
    ja, jb = json.load(open(pa)), json.load(open(pathlib.Path(b) / pa.relative_to(a)))
    fields += [k for k in set(ja) | set(jb) if ja.get(k) != jb.get(k)]
files = sum(1 for p in pathlib.Path(a).rglob("*") if p.is_file())
print(f"{name}: {files} files, MANIFEST fields differing: {sorted(set(fields))}")
PY
  rm -rf "$work/$name-before" "$work/$name-after"
done
