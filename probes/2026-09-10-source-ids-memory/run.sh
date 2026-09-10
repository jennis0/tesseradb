#!/usr/bin/env bash
# What pass one costs in memory as a function of the row count, and whether the bundle moved.
#
# Part one runs `tessera build` over prefixes of a points file with two binaries, samples
# /proc/<pid>/status beside each run, and stops the build as soon as `--stage-timings` prints the
# `source_ids` line — the stage's own peak is that line's, and everything after it would take
# hours at these row counts. It prints, per binary and row count, the stage's wall time, the
# process high-water mark and the largest **anonymous** resident set the sampler saw, which is the
# figure that decides whether a machine can hold the build.
#
# Part two builds a whole ladder corpus with each binary and compares the two bundles file by
# file (`docs/ingest-campaign.md` §4c): only `MANIFEST.json`'s `created_at` and `CURRENT` may
# differ.
#
#   BEFORE=path AFTER=path PREFIX_CORPUS=dir LIMITS="2e7 ..." IDENTITY_CORPUS=dir WORK=dir run.sh
#
# PREFIX_CORPUS wants a points file whose `entity_id` ascends with the file, so `--limit` is a
# prefix of it and the row groups past the limit are pruned by their statistics; GBIF's is.
# IDENTITY_CORPUS is any prepared ladder corpus. Both want their own `tessera.toml`, `corpus.toml`,
# sources and `.env`.
set -uo pipefail

work="${WORK:?WORK is the scratch directory}"
prefix_corpus="${PREFIX_CORPUS:?PREFIX_CORPUS is a corpus whose entity_id ascends with its points file}"
identity_corpus="${IDENTITY_CORPUS:?IDENTITY_CORPUS is a prepared ladder corpus directory}"
limits="${LIMITS:-20000000 200000000 1000000000}"
mkdir -p "$work"

# ---- part one: the stage's peak against the row count ---------------------------------------
# The build is started **not** inside a subshell, so `$!` is the process the sampler reads: a
# wrapper's `/proc/<pid>/status` would report a shell's few megabytes and read as a pass.
set -a; . "$prefix_corpus/.env"; set +a
printf '%-7s %-14s %8s %10s %10s %10s\n' binary rows stage_s hwm_MiB anon_MiB ids_MiB
for limit in $limits; do
  for which in before after; do
    binary="$([ "$which" = before ] && echo "$BEFORE" || echo "$AFTER")"
    out="$work/prefix-$which"
    rm -rf "$out"
    "$binary" build --deployment "$prefix_corpus/tessera.toml" --out "$out" \
      --limit "$limit" --no-oracle-pairs --stage-timings \
      > "$work/prefix-$which.log" 2> "$work/prefix-$which.err" &
    pid=$!
    case "$(ps -o comm= -p "$pid" 2>/dev/null)" in
      *tessera*) ;;
      *) echo "the pid is not the build; refusing to report its memory" >&2; exit 1 ;;
    esac
    ( while [ -r "/proc/$pid/status" ]; do
        awk '/^VmHWM:|^RssAnon:/{printf "%s ", $2} END{print ""}' "/proc/$pid/status"
        sleep 0.2
      done ) > "$work/prefix-$which.rss" 2>/dev/null &
    sampler=$!
    # Stop as soon as the stage has reported: its peak is on that line and the rest of the build
    # is hours at these row counts.
    while kill -0 "$pid" 2>/dev/null; do
      grep -q '^stage       source_ids' "$work/prefix-$which.err" 2>/dev/null && { kill "$pid"; break; }
      sleep 0.2
    done
    wait "$pid" 2>/dev/null
    wait "$sampler" 2>/dev/null
    rm -rf "$out"
    awk -v w="$which" -v n="$limit" -v s="$work/prefix-$which.err" '
      {if($1>h)h=$1; if($2>a)a=$2}
      END{
        while ((getline line < s) > 0)
          if (line ~ /^stage       source_ids/) { split(line, f, " "); wall=f[3]; }
        printf "%-7s %-14s %8s %10.0f %10.0f %10.0f\n", w, n, wall, h/1024, a/1024, n*8/1048576
      }' "$work/prefix-$which.rss"
  done
done

# ---- part two: the bundle is the same one ----------------------------------------------------
name="$(basename "$identity_corpus")"
set -a; . "$identity_corpus/.env"; set +a
for which in before after; do
  binary="$([ "$which" = before ] && echo "$BEFORE" || echo "$AFTER")"
  out="$work/bundle-$name-$which"
  rm -rf "$out"
  ( cd "$identity_corpus" && "$binary" build --deployment tessera.toml --config corpus.toml \
      --out "$out" --stage-timings > "$work/build-$name-$which.log" 2>&1 )
done
diff -rq "$work/bundle-$name-before" "$work/bundle-$name-after" \
  | grep -v -E "MANIFEST\.json|CURRENT" || true
python3 - "$work/bundle-$name-before/v00000/MANIFEST.json" "$work/bundle-$name-after/v00000/MANIFEST.json" <<'PY'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
print("MANIFEST.json fields that differ:", sorted(k for k in set(a) | set(b) if a.get(k) != b.get(k)))
PY
echo "$name: bundles compared (only MANIFEST.json's created_at and CURRENT may differ)"
