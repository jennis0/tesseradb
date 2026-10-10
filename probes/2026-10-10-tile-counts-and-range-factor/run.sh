#!/usr/bin/env bash
# The probe's two sequences. BUNDLE is the corpus bundle (README, "What ran"); BEFORE and AFTER hold
# release builds of viewport_sweep, and AFTER of aggregate_cost, at the commits the README names.
set -u
: "${BUNDLE:?}" "${BEFORE:?}" "${AFTER:?}"
cd "$(dirname "$0")/runs"

# The viewport, before and after, alternating, three rounds.
for r in 1 2 3; do
  for hi in 200 20; do
    for v in before after; do
      bin=$BEFORE; [ $v = after ] && bin=$AFTER
      $bin/viewport_sweep --fixture "$BUNDLE" --terms 0..$hi --samples 5 --max-depth 8 \
        > t$hi-$v-r$r.csv
    done
  done
done

# The aggregate's density cells forced by range and by the pass, two rounds.
declare -A TERMS=( [all]="" [t21]="$(seq -s, 0 20)" [t3]="0,1,2" [t1]="0" )
declare -A FACTOR=( [ranges]=0 [pass]=18446744073709551615 )
for r in 1 2; do
  for set in all t21 t3 t1; do
    for route in ranges pass; do
      t=(); [ -n "${TERMS[$set]}" ] && t=(--terms "${TERMS[$set]}")
      $AFTER/aggregate_cost --bundle "$BUNDLE" --view s0 \
        --polygon '0.40,0.40;0.62,0.40;0.62,0.61;0.40,0.61' --threads 4 --repeat 3 \
        --case density $(for d in 4 5 6 7 8 9 10 11 12; do echo --density-depth $d; done) \
        "${t[@]}" --range-factor ${FACTOR[$route]} > agg-$set-$route-r$r.jsonl
    done
  done
done
