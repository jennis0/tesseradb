#!/usr/bin/env bash
# Stage 1's churn on GeoNames at one shard: ten rounds, each deleting half the corpus, ingesting the
# same rows again as new items, flushing and compacting. Writes the cycle's JSON and one JSON line a
# round; rounds.py turns the lines into rounds.csv and the table in the README.
#
#   cargo build --release -p mosaica-cli
#   MOSAICA_STAGED=<staged> MOSAICA_LADDER=<ladder> python3 -m test_corpora.geonames.prepare --vintage 2026-10-10
#   (cd <ladder>/geonames && mosaica check && mosaica build)
#   LADDER=<ladder> OUT=<dir> bash run.sh && python3 rounds.py <dir>/churn-geonames.jsonl > rounds.csv
#
# Where `systemd-run --user --scope` has no user bus, put a stand-in first on PATH that drops the
# scope's options and runs the command after `--`; the cycle then serves without a cgroup, so its
# eviction reaches the page cache only.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$HERE/../..
LADDER=${LADDER:?the ladder directory holding geonames/}
OUT=${OUT:?a directory for the results}
mkdir -p "$OUT/work"
cd "$REPO"
python3 -m test_corpora.common.ingest_cycle --rung-dir "$LADDER/geonames" --work "$OUT/work" \
  --binary "$REPO/target/release/mosaica" --all-in-bundle "$LADDER/geonames/bundle" \
  --churn-rounds 10 --out "$OUT/churn-geonames.json"
