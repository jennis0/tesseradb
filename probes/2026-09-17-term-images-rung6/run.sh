#!/usr/bin/env bash
# The four session projection routes, timed against each other over one bundle.
#
#   bash probes/2026-09-17-term-images-rung6/run.sh <bundle> <results dir> [probe args...]
#
# With no probe args the eleven principals of `docs/evidence/memos/2026-09-14-term-images.md` §3
# are run: the five country ladder principals, `all` from `country-terms.txt` beside the bundle
# (the rung directory is the bundle's parent), 300 years drawn uniformly, and species drawn
# size-weighted at 1,000, 10,000 and 100,000. The two draws read the bundle's own dictionary, so
# the same line runs at the 64-part rung and at the whole corpus. Any probe args given replace
# that list entirely.
#
# CAP and SWAP wrap the probe in `systemd-run --user --scope --collect -p MemoryMax=$CAP
# -p MemorySwapMax=$SWAP`, which needs no root on a box whose memory controller is delegated to
# the user slice. The rung 6 form is
#
#   CAP=24G SWAP=2G bash probes/2026-09-17-term-images-rung6/run.sh data/ladder/gbif-terms/bundle out/rung6
#
# and the script refuses a `*/gbif*/bundle` without CAP: those bundles are around 200 GiB and a
# process that maps one uncapped takes the box down. The 64-part rungs run uncapped.
#
# The probe runs in its own session under `setsid` and at `nice -n 10`. Nothing here matches a
# process by name or signals one; the probe's pid is printed so it can be stopped by pid.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tree="$(cd "$here/../.." && pwd)"

given="${1:?the bundle root, the directory holding CURRENT}"
out="${2:?the results directory}"
shift 2

# The cap is checked against the path as given, before anything opens it: a bundle that is not
# there yet must still be refused without CAP, or the guard would be one that only fires once the
# thing it guards exists.
cap="${CAP:-}"
swap="${SWAP:-2G}"
case "$given" in
  */gbif/bundle|*/gbif/bundle/|*/gbif-terms/bundle|*/gbif-terms/bundle/)
    [ -n "$cap" ] || {
      echo "refusing $given without CAP: a whole-corpus bundle is never opened uncapped" >&2
      exit 1
    } ;;
esac

bundle="$(cd "$given" && pwd)" || { echo "no bundle at $given" >&2; exit 1; }
[ -f "$bundle/CURRENT" ] || { echo "no CURRENT in $bundle" >&2; exit 1; }
rung="$(dirname "$bundle")"

bin="${BIN:-${CARGO_TARGET_DIR:-$tree/target}/release/route_probe}"
[ -x "$bin" ] || {
  echo "missing $bin; build with: cargo build --release -p mosaica-bench --bin route_probe" >&2
  exit 1
}

if [ "$#" -eq 0 ]; then
  terms="$rung/country-terms.txt"
  ranks="$rung/country-ranks.json"
  for f in "$terms" "$ranks"; do
    [ -f "$f" ] || { echo "no $f; pass the probe args explicitly" >&2; exit 1; }
  done
  # The compartment ladder is composed from the rung's own ranks by `serve_battery.py`'s rule
  # rather than written down, so it runs at a rung whose country vocabulary is a prefix of the
  # whole corpus's. At rung 6 it produces the sets the evidence memo names.
  set -- \
    --ranks-file "$ranks" \
    --country-ladder 0.01 \
    --country-ladder 0.05 \
    --country-ladder 0.10 \
    --country-ladder 0.25 \
    --country-ladder 0.50 \
    --terms-file "$terms" \
    --year-uniform 300 \
    --species-weighted 1000 \
    --species-weighted 10000 \
    --species-weighted 100000
fi

mkdir -p "$out"
commit="$(git -C "$tree" rev-parse HEAD 2>/dev/null || echo unknown)"
results="$out/results.json"

cmd=("$bin" --bundle "$bundle" --view "${VIEW:-geo}" --seed "${SEED:-20260917}" \
     --commit "$commit" --out "$results" "$@")
if [ -n "$cap" ]; then
  cmd=(systemd-run --user --scope --collect -p "MemoryMax=$cap" -p "MemorySwapMax=$swap" \
       nice -n 10 "${cmd[@]}")
else
  cmd=(nice -n 10 "${cmd[@]}")
fi

log="$out/route_probe.log"
echo "bundle  $bundle"
echo "results $results"
echo "cap     ${cap:-none}"
echo "log     $log"
setsid nohup "${cmd[@]}" > "$log" 2>&1 < /dev/null &
pid=$!
echo "pid     $pid"
wait "$pid"
status=$?
echo "exit    $status"
exit "$status"
