#!/usr/bin/env bash
# Which of the two routes a blob-resident string column should take, and what decides it.
#
# Part one times both routes over each slice on the whole box, sampling the bundle root while the
# build runs. Part two repeats one slice under cgroup memory caps with the build's own plan held
# fixed, so the only thing that moves is how much of the arena the page cache holds. Part three
# prints what the derived rule chooses, by running each slice to its pre-flight and stopping it.
#
#   ARENA=path EXTENTS=path DERIVED=path LADDER=dir WORK=dir \
#     bash probes/2026-09-10-blob-resident-strings/routes.sh
#
# ARENA and EXTENTS are this branch's binary with `build` calling `pipeline::build` with
# `ExtentRoute::Arena` and `ExtentRoute::Extents` rather than `Derived`; DERIVED is it unmodified.
# All three must be named so `ps -o comm=` reads `tessera` in them: the sampler refuses a pid whose
# command is not the build.
#
# SLICES is the row-group counts to slice and time, CAPS the MemoryMax values for part two and
# CAP_SLICE the slice it runs over. WORK wants ~40 GB free on the same filesystem as the builds.
set -uo pipefail
arena="${ARENA:?ARENA is the arena-route binary}"
extents="${EXTENTS:?EXTENTS is the extent-route binary}"
derived="${DERIVED:?DERIVED is the unmodified binary}"
ladder="${LADDER:?LADDER is data/ladder}"
work="${WORK:?WORK is the scratch directory}"
disk="$(cd "$(dirname "$0")/../2026-09-10-build-disk" && pwd)"
slices="${SLICES:-25 50 100 200}"
caps="${CAPS:-8G 6G 5G 4G}"
cap_slice="${CAP_SLICE:-200}"
cap_budget="${CAP_BUDGET:-4g}"
mkdir -p "$work"
export TESSERA_IDENTITY_KEY="${TESSERA_IDENTITY_KEY:-000102030405060708090a0b0c0d0e0f}"

# Foreign load either side of every run. A build of this size is itself a load average of ten, so
# the one-minute average says nothing; what matters is whether anything that is not this probe was
# running, and that is what this prints.
foreign() {
  ps -eo pcpu,comm --no-headers --sort=-pcpu \
    | awk '$1 > 20 && $2 !~ /claude|tessera|python3/ {printf "%s@%s%% ", $2, $1}'
}

for g in $slices; do
  [ -d "$work/gbif-${g}m" ] || python3 "$disk/slice.py" "$g" "$work/gbif-${g}m" "$ladder/gbif"
done

# ---- part one: both routes on the whole box ---------------------------------------------------
for g in $slices; do
  for which in arena extents; do
    binary="$([ "$which" = arena ] && echo "$arena" || echo "$extents")"
    out="$work/run-${g}m-${which}"
    printf '%-6s %-8s before=[%s] ' "${g}m" "$which" "$(foreign)"
    start=$(date +%s.%N)
    python3 "$disk/sample.py" "$work/gbif-${g}m" "$out" "$binary"
    printf '  wall %.1f s  after=[%s]\n' "$(echo "$(date +%s.%N) - $start" | bc)" "$(foreign)"
    grep -h '^columns: ' "$out/build.log" | sed 's/^/    /'
    python3 - "$out/stages.json" <<'PY'
import json, sys
rows = json.load(open(sys.argv[1]))
want = {"attribute_tail", "filter_postings", "record_blob"}
parts = " ".join(f"{r['stage']}={r['wall_s']:.1f}" for r in rows if r["stage"] in want)
print(f"    stages total={sum(r['wall_s'] for r in rows):.1f}s  {parts}  "
      f"peak_rss={max(r['peak_rss_kib'] for r in rows) // 1024} MiB")
PY
    rm -rf "$out/bundle"
  done
done

# ---- part two: both routes under a memory cap -------------------------------------------------
# `--memory-budget` is held at CAP_BUDGET for every run, so the batch stride, the bands and the
# dictionary plans are one build across the sweep. A cap under that budget is below what the plan's
# own anonymous memory needs and the build is OOM-killed whichever route it took, which is the
# result and not a failure of the run.
corpus="$work/gbif-${cap_slice}m"
set -a; . "$corpus/.env"; set +a
for cap in $caps; do
  for which in arena extents; do
    binary="$([ "$which" = arena ] && echo "$arena" || echo "$extents")"
    out="$work/cap-${cap_slice}m-${which}-${cap}"
    rm -rf "$out"; mkdir -p "$out"
    printf '%-6s %-8s cap=%-5s before=[%s] ' "${cap_slice}m" "$which" "$cap" "$(foreign)"
    start=$(date +%s.%N)
    systemd-run --user --scope --quiet -p MemoryMax="$cap" -p MemorySwapMax=0 -- \
      "$binary" build --deployment "$corpus/tessera.toml" --config "$corpus/corpus.toml" \
        --out "$out/bundle" --memory-budget "$cap_budget" --no-oracle-pairs \
        --stage-timings --stage-timings-json "$out/stages.json" > "$out/out.log" 2>&1
    code=$?
    printf 'exit %s  wall %.1f s  after=[%s]\n' \
      "$code" "$(echo "$(date +%s.%N) - $start" | bc)" "$(foreign)"
    [ -f "$out/stages.json" ] && python3 - "$out/stages.json" <<'PY'
import json, sys
rows = json.load(open(sys.argv[1]))
want = {"attribute_tail", "filter_postings", "record_blob"}
print("    " + " ".join(f"{r['stage']}={r['wall_s']:.1f}" for r in rows if r["stage"] in want))
PY
    rm -rf "$out/bundle"
  done
done

# ---- part three: what the derived rule chooses ------------------------------------------------
# The routing and disk lines are printed at the plan, so the build is stopped as soon as they are.
for g in $slices; do
  out="$work/fc-${g}m"
  rm -rf "$out"; mkdir -p "$out"
  set -a; . "$work/gbif-${g}m/.env"; set +a
  "$derived" build --deployment "$work/gbif-${g}m/tessera.toml" \
    --config "$work/gbif-${g}m/corpus.toml" --out "$out/bundle" > "$out/out.log" 2>&1 &
  pid=$!
  case "$(ps -o comm= -p "$pid" 2>/dev/null)" in
    *tessera*) ;;
    *) kill "$pid" 2>/dev/null; echo "the pid is not the build" >&2; continue ;;
  esac
  while kill -0 "$pid" 2>/dev/null; do
    grep -q '^disk: ' "$out/out.log" 2>/dev/null && { kill "$pid"; break; }
    sleep 0.5
  done
  wait "$pid" 2>/dev/null
  printf '%-6s ' "${g}m"
  grep -h '^columns: ' "$out/out.log" || echo "no routing printed"
  rm -rf "$out/bundle"
done
