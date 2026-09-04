#!/bin/bash
# Arms 2 and 3 again, one process at a time on an otherwise idle machine (the first runs
# overlapped the arm-1 sweep; those logs are kept under logs/overlapped/).
P=/home/joe/code/tessera/probes/2026-09-02-value-suggestion
D=/home/joe/code/tessera/data/ladder/probe-suggest
B=$P/suggestprobe/target/release/suggestprobe
$B arm2 $D 10000000 zipf > $P/logs/arm2-zipf.log 2>&1
$B arm2 $D 10000000 uniform > $P/logs/arm2-uniform.log 2>&1
$B arm3 $D 10000000 zipf > $P/logs/arm3-zipf.log 2>&1
$B arm3 $D 10000000 uniform > $P/logs/arm3-uniform.log 2>&1
echo QUIET_DONE > $P/logs/quiet.done
