#!/bin/bash
# Arm 1: one process per (structure, V, entry set) so peak RSS is that build's own.
set -u
P=/home/joe/code/tessera/probes/2026-09-02-value-suggestion
D=/home/joe/code/tessera/data/ladder/probe-suggest
B=$P/suggestprobe/target/release/suggestprobe
LOG=$P/logs/arm1.log
: > $LOG
for V in 100000 1000000 10000000; do
  for E in key words; do
    for S in btree arena arenaref fst dict; do
      $B arm1 $D $S $V $E >> $LOG 2>&1
    done
  done
done
for V in 1000000 10000000; do
  for S in btree arena fst dict; do
    $B arm1 $D $S $V key hex >> $LOG 2>&1
  done
done
echo ARM1_DONE >> $LOG
