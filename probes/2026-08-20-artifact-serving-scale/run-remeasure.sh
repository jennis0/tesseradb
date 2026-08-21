#!/usr/bin/env bash
# The re-measurement, after the adversarial review — **every figure in the design comes from here
# and not from the earlier scripts in this directory**, which measured routes that skipped the
# masked candidacy test on a fixture that could not have shown it.
#
# Run it on a quiet box. Three runs per configuration, one configuration per process (the residency
# campaign's rule, and it applies to time as well: a second population built in a warm allocator
# measures a machine state rather than a design). `collate.py` folds the runs into a median table.
#
#   bash probes/2026-08-20-artifact-serving-scale/run-remeasure.sh [step ...]
#
# With no arguments it runs every step in order. Named steps run only those, so a box that cannot
# hold the 10⁹ arms can still produce the rest.
#
# ## What changed, and why the old numbers are superseded
#
# - routes `grouped` and `hoisted` now pay a masked candidacy probe for **every** candidate. They
#   were admitting the settled half on the containment partition alone, so their headline figures
#   omitted the work they exist to price. Expect both to be **slower**, most at whole-map zoom.
# - the fixture's generating sets are decorrelated from the mask groups and from the membership, so
#   containment no longer implies visibility by construction;
# - artifacts carry ranked contents (two by default), and the served rank is asserted;
# - members default to `rows / artifacts`, which is what a layer covering the corpus means;
# - the grid reports the median of its iterations with the spread beside it, not the best of them.
#
# ## Durations and resident size
#
# The durations below are **estimates**, scaled from the pre-correction campaign on the same box and
# corrected upward for the restored candidacy probe and the assertions; they are not measurements
# and the first real run should replace them. `/usr/bin/time -v` records the truth per step, in
# `<tag>.log`. RSS is dominated by the entity-space records and the row-entity table, both linear in
# `rows`, so the 10⁹ steps are the ones a smaller box cannot take.
set -u

BIN=./target/release/artifact_serving_scale
HERE="$(cd "$(dirname "$0")" && pwd)"
D="$HERE/data"
mkdir -p "$D"
[ -x "$BIN" ] || { echo "build first: cargo build --release -p tessera-bench --bin artifact_serving_scale" >&2; exit 1; }

RUNS=${RUNS:-3}

# run <tag> <args...> — three runs, each its own process, each with its own build-cost record.
run () {
  tag="$1"; shift
  for r in $(seq 1 "$RUNS"); do
    echo "=== $tag run $r"
    /usr/bin/time -v "$BIN" "$@" --costs-out "$D/$tag-run$r-costs.csv" \
      > "$D/$tag-run$r.csv" 2> "$D/$tag-run$r.log" \
      || { echo "!!! $tag run $r FAILED — see $D/$tag-run$r.log" >&2; tail -3 "$D/$tag-run$r.log" >&2; }
    grep -E "^#|Maximum resident" "$D/$tag-run$r.log" || true
  done
  python3 "$HERE/collate.py" "$D" "$tag"
}

want () { # want <step> <selected...> — with nothing selected every step is wanted
  step="$1"; shift
  [ $# -eq 0 ] && return 0
  case " $* " in *" $step "*) return 0;; esac
  return 1
}

STEPS="$*"
set -- $STEPS

# ---------------------------------------------------------------- 1. the grid at 10⁸ points
# The artifact-count axis at a fixed corpus, members at the rule so the layer partitions it. All
# routes at 10⁵ and 10⁶; at 10⁷ the shipped loop alone is ~24 s a call, so it is dropped there and
# the three smaller scales carry that ladder.
# ~25 min, ~6 GB · ~40 min, ~7 GB · ~90 min, ~14 GB  (estimates)
if want r1e8 "$@"; then
  run r1e8-a1e5 --rows 100000000 --artifacts 100000  --members 1000 --coverage
  run r1e8-a1e6 --rows 100000000 --artifacts 1000000 --members 100  --coverage
  run r1e8-a1e7 --rows 100000000 --artifacts 10000000 --members 10 \
      --only early,index,index+session,settled,grouped,hoisted
fi

# ---------------------------------------------------------------- 2. the grid at 10⁹ points
# **The target.** `members = rows / artifacts` — the earlier script used 10 at 10⁷ artifacts, which
# covers a tenth of the corpus and makes every candidacy figure proportionally cheap.
# ~3 h, ~22 GB · ~6 h, ~34 GB  (estimates; the second is the one that may not fit)
if want r1e9 "$@"; then
  run r1e9-a1e6 --rows 1000000000 --artifacts 1000000  --members 1000 \
      --only index,index+session,settled,grouped,hoisted
  run r1e9-a1e7 --rows 1000000000 --artifacts 10000000 --members 100 \
      --only index+session,settled,grouped,hoisted
fi

# ---------------------------------------------------------------- 3. the hierarchy
# The `nested` arm at 10⁶ nodes over both corpus sizes — the one arm whose tree *is* its geometry,
# and where the design's remaining bound (the masked count of a coarse node) lives.
# ~35 min, ~8 GB · ~2 h, ~24 GB  (estimates)
if want nested "$@"; then
  run r1e8-nested-a1e6 --rows 100000000  --artifacts 1000000 --arm nested --only grouped,hoisted,settled
  run r1e9-nested-a1e6 --rows 1000000000 --artifacts 1000000 --arm nested --only grouped,hoisted,settled
fi

# ---------------------------------------------------------------- 4. §4.2's parity comparison
# The shipped loop, the per-token route it was measured against, and the two build-time-partition
# routes that replaced it — the table decision 0093 rests on, which had no data file behind it.
# ~50 min, ~7 GB  (estimate)
if want parity "$@"; then
  run parity-r1e8-a1e6 --rows 100000000 --artifacts 1000000 --members 100 --parity
fi

# ---------------------------------------------------------------- 5. the shape with no locality
# The scattered arm direct, where `everywhere` is the whole population and the index settles
# nothing. The ladder is what says where it walls; the row-major routes are what removes the wall.
# ~10 min each, ~4–9 GB  (estimates)
if want scattered "$@"; then
  run r1e8-s1e4-scattered --rows 100000000 --artifacts 10000  --members 10000 --arm scattered
  run r1e8-s1e5-scattered --rows 100000000 --artifacts 100000 --members 1000  --arm scattered
  run r1e8-p1e5-partition --rows 100000000 --artifacts 100000 --members 1000  --arm partition
fi

# ---------------------------------------------------------------- 6. the blocks-per-artifact bracket
# **Nothing was ever measured between 1.6 and 10 blocks per artifact**, which is exactly where the
# layout heuristic's threshold sits. Four points across it, everything else held.
# ~15 min each, ~6 GB  (estimates)
if want bracket "$@"; then
  for b in 2 4 6 8; do
    run "r1e8-a1e6-blocks$b" --rows 100000000 --artifacts 1000000 --members 100 \
        --arm dispersed --blocks "$b" --only index,settled,grouped,hoisted
  done
fi

# ---------------------------------------------------------------- 7. the distinct-expression count
# The number decision 0093's storage claim needs, counted rather than asserted — over the fixture's
# own construction and over a uniform sample. Seconds, no memory.
#
# The real-corpus count is a separate run and needs a signature distribution: one carrier count per
# line, then
#     $BIN --expressions --artifacts 2400000 --signatures <file>
if want expressions "$@"; then
  "$BIN" --expressions --artifacts 2400000 > "$D/expressions.csv" 2> "$D/expressions.log"
  cat "$D/expressions.csv"
fi

# ---------------------------------------------------------------- 8. the fixture's own before/after
# `--legacy-fixture` reproduces the alignment the corrections removed: one 32-way partition doing
# every job, one content, generating sets drawn from the membership. Assertions **report** rather
# than fail there, so the log is the evidence that the earlier figures came from a fixture whose
# routes served artifacts the shipped loop withholds.
# ~10 min, ~7 GB  (estimate)
if want legacy "$@"; then
  run legacy-r1e8-a1e6 --rows 100000000 --artifacts 1000000 --members 100 \
      --legacy-fixture --only grouped,hoisted
  grep -c "legacy fixture:" "$D/legacy-r1e8-a1e6-run1.log" || true
fi

echo "done — medians in $D/*-medians.csv, every run in $D/*-all.csv"
