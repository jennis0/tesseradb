#!/usr/bin/env bash
# Build one probe corpus with two binaries and compare what each cost and what each wrote.
#
# For every shape `make_corpus.py` knows, this builds the corpus twice — once with the binary at
# $BEFORE and once with the one at $AFTER — and prints, per shape and per binary, the
# `FilterPostings` stage's wall time, rows and peak RSS out of `--stage-timings-json`. The schema
# declares one indexed keyword column and nothing else, so that stage is the keyword emit.
#
# It then compares the two bundles file by file. `MANIFEST.json` and `CURRENT` are expected to
# differ — the first carries `created_at`, the second the build's own id — and the comparison is
# §4c of `docs/ingest-campaign.md`'s.
#
#   BEFORE=path AFTER=path WORK=dir ROWS=20000000 REPEATS=1 BUDGET= run.sh [shape...]
#
# Peak RSS is the stage record's own figure — `VmHWM`, read from /proc by the build, and so the
# whole process's high-water mark at the moment the stage ended rather than the stage's own
# allocation. BUDGET, where set, is passed as `--memory-budget`: the pass sizes its chunk off it,
# so it is what shows the bound working.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="${WORK:?WORK is the scratch directory}"
rows="${ROWS:-20000000}"
repeats="${REPEATS:-1}"
budget=(); [ -n "${BUDGET:-}" ] && budget=(--memory-budget "$BUDGET")
shapes=("${@:-distinct many few}")
export TESSERA_IDENTITY_KEY=0123456789abcdef0123456789abcdef

mkdir -p "$work"
cat > "$work/tessera.toml" <<'EOF'
[bundle]
path  = "bundle"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "corpus.toml"

[identity]
env = "TESSERA_IDENTITY_KEY"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
EOF

for shape in ${shapes[@]}; do
  corpus="$work/corpus-$shape"
  [ -f "$corpus/points.parquet" ] || python3 "$here/make_corpus.py" "$corpus" "$shape" "$rows"
  for run in $(seq 1 "$repeats"); do
   for which in before after; do
    binary="$([ "$which" = before ] && echo "$BEFORE" || echo "$AFTER")"
    out="$work/bundle-$shape-$which"
    rm -rf "$out"
    /usr/bin/time -v "$binary" build --deployment "$work/tessera.toml" \
        --config "$corpus/corpus.toml" --out "$out" --no-oracle-pairs "${budget[@]}" \
        --stage-timings --stage-timings-json "$work/stages-$shape-$which.json" \
        > "$work/build-$shape-$which.log" 2> "$work/time-$shape-$which.log"
    python3 - "$work/stages-$shape-$which.json" "$work/time-$shape-$which.log" "$shape" "$which" <<'PY'
import json, re, sys

stages = json.load(open(sys.argv[1]))
stage = next(s for s in stages if s["stage"] == "filter_postings")
# `peak_rss_kib` is VmHWM, which only rises — so the pass's own contribution to the build's peak
# is what it added to the highest mark any earlier stage had already set. `text_index` is charged
# out of this stage's own block and is reported before it, so the stages that precede this one in
# TIME are the ones before the first of the two.
cut = next(i for i, s in enumerate(stages) if s["stage"] in ("text_index", "filter_postings"))
before = max((s["peak_rss_kib"] for s in stages[:cut]), default=0)
rate = stage["rows"] / stage["wall_s"] if stage["wall_s"] else float("nan")
whole = re.search(r"Maximum resident set size \(kbytes\): (\d+)", open(sys.argv[2]).read())
print(
    f"{sys.argv[3]:9s} {sys.argv[4]:6s} filter_postings "
    f"{stage['wall_s']:8.2f} s  {rate:12,.0f} rows/s  "
    f"peak RSS {stage['peak_rss_kib'] / 1048576:6.2f} GiB "
    f"(+{(stage['peak_rss_kib'] - before) / 1048576:5.2f} over the stages before it), "
    f"build {int(whole.group(1)) / 1048576:6.2f} GiB"
)
PY
   done
  done
  diff -rq "$work/bundle-$shape-before" "$work/bundle-$shape-after" \
    | grep -v -E "MANIFEST\.json|CURRENT" || true
  echo "$shape: bundles compared (only MANIFEST.json and CURRENT may differ)"
done
