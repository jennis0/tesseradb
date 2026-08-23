#!/usr/bin/env bash
# The bracket re-run: `dispersed` at blocks 6, 8, 10, 12 with doubled iterations.
#
# **What it is for.** `probes/2026-08-20-artifact-serving-scale/README.md` records a negative
# result it could not explain: the layout sweep's `blocks = 8` point breaks a trend that is
# otherwise clean. Whole-map and worst cells run 61/103, 111/126 and 159/174 ms at 2, 4 and 6
# blocks per artifact — and 8 comes back *below* 6, with the fixture's own statistics saying
# nothing is wrong with it. Either the trend is not monotone in this band, or that run met
# something on the box. Four points across and through the anomaly, with the iteration count
# doubled so a single unlucky pass cannot carry a median, is what separates the two.
#
# It matters because the layout heuristic's threshold sits in this band and is currently
# provisional (the delivery record's Stage 6 row: "the layout thresholds are provisional pending
# the bracket re-run").
#
# Run on an otherwise idle box, between tiers — never beside a campaign measurement or a compile.
#
#   bash probes/2026-08-22-artifact-serving-e2e/run-bracket.sh
#
# `RUNS` (default 3) processes per configuration, one configuration per process, on the
# residency campaign's rule: a second population built in a warm allocator measures a machine
# state rather than a design. `collate.py` in the 2026-08-20 directory folds the runs into the
# median table, because this is that campaign's own arm re-run and its collation is what the
# earlier four points were read through.
set -u

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$REPO/target/release/artifact_serving_scale"
HERE="$(cd "$(dirname "$0")" && pwd)"
D="$HERE/data"
COLLATE="$REPO/probes/2026-08-20-artifact-serving-scale/collate.py"
mkdir -p "$D"
[ -x "$BIN" ] || {
  echo "build first: cargo build --release -p tessera-bench --bin artifact_serving_scale" >&2
  exit 1
}

RUNS=${RUNS:-3}
# Doubled from the original sweep's default of 3. The anomaly is a median that moved; more passes
# per process is the cheapest thing that could make it stop moving.
ITERS=${ITERS:-6}

for b in 6 8 10 12; do
  tag="bracket-a1e6-blocks$b"
  for r in $(seq 1 "$RUNS"); do
    echo "=== $tag run $r"
    /usr/bin/time -v "$BIN" --rows 100000000 --artifacts 1000000 --members 100 \
      --arm dispersed --blocks "$b" --iters "$ITERS" \
      --only index,settled,grouped,hoisted \
      --costs-out "$D/$tag-run$r-costs.csv" \
      > "$D/$tag-run$r.csv" 2> "$D/$tag-run$r.log" \
      || { echo "!!! $tag run $r FAILED — see $D/$tag-run$r.log" >&2; tail -3 "$D/$tag-run$r.log" >&2; }
    grep -E "^#|Maximum resident" "$D/$tag-run$r.log" || true
  done
  python3 "$COLLATE" "$D" "$tag"
done

echo
echo "medians:"
for b in 6 8 10 12; do
  echo "--- blocks $b"
  head -3 "$D/bracket-a1e6-blocks$b-medians.csv" 2>/dev/null || echo "  (no median file)"
done
