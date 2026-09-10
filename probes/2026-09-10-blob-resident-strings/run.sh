#!/usr/bin/env bash
# What routing a blob-resident string column through the record blob's extents costs and saves.
#
# Part one builds five ladder corpora with each binary and compares the two bundles file by file
# (`docs/ingest-campaign.md` §4c): only `MANIFEST.json`'s `created_at` and the `CURRENT` that
# digests it may differ, and the manifest is then compared field by field.
#
# Part two slices prefixes of the GBIF ladder corpus, builds each with both binaries and samples
# the bundle root file by file while it runs: the peak, and what stood at it. The slicer and the
# sampler are `probes/2026-09-10-build-disk/`'s, reused rather than rebuilt.
#
# Part three prints each binary's own disk forecast, by running the build until the pre-flight has
# spoken and then stopping it — the forecast is arithmetic over the corpus and does not need the
# build that follows. It runs over the slices and over the whole 3,495,729,729-row corpus, which
# is rung 6 and is not built.
#
#   BEFORE=path AFTER=path LADDER=dir WORK=dir bash probes/2026-09-10-blob-resident-strings/run.sh
#
# Each part can be turned off, so a box can be left quiet for the one being measured: IDENTITY is
# the corpus list part one builds (`none` for no corpus), SLICES the row-group counts parts two
# and three slice (`none` for none), RUNG6 whether part three reads the whole corpus to its
# pre-flight, which costs a pass over 3,495,729,729 rows.
#
# LADDER is `data/ladder`. WORK wants ~80 GB free, on the same filesystem as the builds. BEFORE
# and AFTER must be named so that `ps -o comm=` reads `tessera` in them: both the sampler and the
# forecast below refuse a pid whose command is not the build, and a binary copied to `before` is
# not one.
set -uo pipefail
before="${BEFORE:?BEFORE is the binary to measure against}"
after="${AFTER:?AFTER is the binary to measure}"
ladder="${LADDER:?LADDER is data/ladder}"
work="${WORK:?WORK is the scratch directory}"
disk="$(cd "$(dirname "$0")/../2026-09-10-build-disk" && pwd)"
# Not `GROUPS`: bash keeps the caller's group ids in that name and a default would
# expand to one of them.
slices="${SLICES:-25 50 100 200}"
rung6="${RUNG6:-1}"
mkdir -p "$work"
# A corpus with no `.env` of its own builds under this one, which is a fixture key and not a
# deployment's: what the comparison needs is that both binaries use the same lineage.
export TESSERA_IDENTITY_KEY="${TESSERA_IDENTITY_KEY:-000102030405060708090a0b0c0d0e0f}"

# ---- part one: the same bundle, byte for byte ------------------------------------------------
for name in ${IDENTITY:-gbif-64p multiview treeoflife-1m medcpt-1m geonames}; do
  [ "$name" = none ] && continue
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

# ---- part two: the peak, and what stood at it ------------------------------------------------
for g in $slices; do
  [ "$g" = none ] && continue
  [ -d "$work/gbif-${g}m" ] || python3 "$disk/slice.py" "$g" "$work/gbif-${g}m" "$ladder/gbif"
done
for g in $slices; do
  [ "$g" = none ] && continue
  for which in before after; do
    binary="$([ "$which" = before ] && echo "$before" || echo "$after")"
    printf '%-6s %-7s ' "${g}m" "$which"
    python3 "$disk/sample.py" "$work/gbif-${g}m" "$work/run-${g}m-${which}" "$binary"
    rm -rf "$work/run-${g}m-${which}/bundle"
  done
done

# ---- part three: what each binary's pre-flight says -------------------------------------------
forecast() {
  local out="$3"
  rm -rf "$out"; mkdir -p "$out"
  set -a; . "$1/.env"; set +a
  "$2" build --deployment "$1/tessera.toml" --config "$1/corpus.toml" \
    --out "$out/bundle" "${@:4}" > "$out/out.log" 2>&1 &
  local pid=$!
  # The pid the loop below waits on is the build's own: a wrapper shell's would fall through at
  # once and record a forecast that was never printed.
  case "$(ps -o comm= -p "$pid" 2>/dev/null)" in
    *tessera*) ;;
    *) kill "$pid" 2>/dev/null; echo "the pid is not the build" >&2; return 1 ;;
  esac
  while kill -0 "$pid" 2>/dev/null; do
    grep -q '^disk: ' "$out/out.log" 2>/dev/null && { kill "$pid"; break; }
    sleep 0.5
  done
  wait "$pid" 2>/dev/null
  grep '^disk: ' "$out/out.log" || echo "no forecast printed"
  rm -rf "$out/bundle"
}
for g in $slices; do
  [ "$g" = none ] && continue
  for which in before after; do
    binary="$([ "$which" = before ] && echo "$before" || echo "$after")"
    printf '%-6s %-7s ' "${g}m" "$which"
    forecast "$work/gbif-${g}m" "$binary" "$work/fc-${g}m-${which}"
  done
done
# Rung 6, whole: read to the pre-flight and stopped there. The build is not run.
[ "$rung6" = 1 ] || exit 0
for which in before after; do
  binary="$([ "$which" = before ] && echo "$before" || echo "$after")"
  printf '%-6s %-7s ' rung6 "$which"
  forecast "$ladder/gbif" "$binary" "$work/fc-rung6-${which}"
done
