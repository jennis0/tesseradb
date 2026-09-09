#!/usr/bin/env bash
# Build one ladder corpus with two binaries and compare what each cost and what each wrote.
#
# Prints, per binary, the `layers` and `manifests` stage records out of `--stage-timings-json`, and
# the artifact pass's own figure out of the line `artifact_pass::report` writes — the pass runs
# inside the `manifests` stage and is most of it, which is what makes the second number necessary.
#
# It then compares the two bundles file by file. `MANIFEST.json` and `CURRENT` are expected to
# differ — the first carries `created_at`, the second the build's own id — and the comparison is
# §4c of `docs/ingest-campaign.md`'s.
#
#   BEFORE=path AFTER=path CORPUS=dir WORK=dir REPEATS=1 PAIRS=0 run.sh
#
# CORPUS is a prepared ladder corpus directory: its own `tessera.toml`, `corpus.toml`, sources and
# `.env`. PAIRS=1 drops `--no-oracle-pairs`, so the comparison covers `pairs.parquet` too.
set -euo pipefail

work="${WORK:?WORK is the scratch directory}"
corpus="${CORPUS:?CORPUS is a prepared ladder corpus directory}"
repeats="${REPEATS:-1}"
pairs=(--no-oracle-pairs); [ "${PAIRS:-0}" = 1 ] && pairs=()
name="$(basename "$corpus")"

mkdir -p "$work"
set -a; . "$corpus/.env"; set +a

for run in $(seq 1 "$repeats"); do
 for which in before after; do
  binary="$([ "$which" = before ] && echo "$BEFORE" || echo "$AFTER")"
  out="$work/bundle-$name-$which"
  rm -rf "$out"
  ( cd "$corpus" && /usr/bin/time -v "$binary" build \
      --deployment tessera.toml --config corpus.toml --out "$out" "${pairs[@]}" \
      --stage-timings --stage-timings-json "$work/stages-$name-$which.json" \
      > "$work/build-$name-$which.log" 2> "$work/time-$name-$which.log" )
  python3 - "$work/stages-$name-$which.json" "$work/time-$name-$which.log" "$name" "$which" <<'PY'
import json, re, sys

stages = json.load(open(sys.argv[1]))
by = {s["stage"]: s for s in stages}
text = open(sys.argv[2]).read()
# The artifact pass reports its own wall time; it runs inside `manifests` and the rest of that
# stage is the bundle's digest and the manifest write.
pass_ms = re.search(r"artifact layouts, chosen from the bundle's own row space \((\d+) ms\)", text)
whole = re.search(r"Elapsed \(wall clock\) time \(h:mm:ss or m:ss\): ([0-9:.]+)", text)
total = sum(s["wall_s"] for s in stages)
peak = max(s["peak_rss_kib"] for s in stages) / 1048576
apass = int(pass_ms.group(1)) / 1000 if pass_ms else float("nan")
print(
    f"{sys.argv[3]:12s} {sys.argv[4]:6s} "
    f"layers {by['layers']['wall_s']:7.1f} s  "
    f"manifests {by['manifests']['wall_s']:7.1f} s (artifact pass {apass:6.1f} s)  "
    f"stages {total:7.1f} s  wall {whole.group(1) if whole else '?':>8s}  peak {peak:5.2f} GiB"
)
PY
 done
done
diff -rq "$work/bundle-$name-before" "$work/bundle-$name-after" \
  | grep -v -E "MANIFEST\.json|CURRENT" || true
python3 - "$work/bundle-$name-before/v00000/MANIFEST.json" "$work/bundle-$name-after/v00000/MANIFEST.json" <<'PY'
import json, sys

a, b = (json.load(open(p)) for p in sys.argv[1:3])
differ = sorted(k for k in set(a) | set(b) if a.get(k) != b.get(k))
print(f"MANIFEST.json fields that differ: {differ}")
PY
echo "$name: bundles compared (only MANIFEST.json's created_at and CURRENT may differ)"
