#!/bin/bash
# For each bundle root (dir containing CURRENT), report total and the artifact-related subtrees.
for cur in $(find /home/user/code/tessera/data -name CURRENT -type f 2>/dev/null); do
  root=$(dirname "$cur")
  tot=$(du -sb "$root" 2>/dev/null | cut -f1)
  m=$(du -sb $root/*/partitions/*/members 2>/dev/null | awk '{s+=$1} END{print s+0}')
  ti=$(du -sb $root/*/partitions/*/tile-index 2>/dev/null | awk '{s+=$1} END{print s+0}')
  rc=$(du -sb $root/*/partitions/*/row-column 2>/dev/null | awk '{s+=$1} END{print s+0}')
  cp=$(du -sb $root/*/partitions/*/containment 2>/dev/null | awk '{s+=$1} END{print s+0}')
  ar=$(du -sb $root/*/partitions/*/attrs/record/extents 2>/dev/null | awk '{s+=$1} END{print s+0}')
  echo -e "$root\t$tot\t$m\t$ti\t$rc\t$cp\t$ar"
done
