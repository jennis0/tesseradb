#!/usr/bin/env bash
# Build the benchmark fixture bundles: three scales x the label sets reachable at each.
#
# A "scale" is a prefix filter on the SOURCE entity id (`--limit`, `entity_id < limit`), never a
# separate corpus file — probes/dataset.md §5 rule 1. Geometry is label-independent and shared;
# only the pairs file changes per label set.
#
# Scales are 250k / 2.42M / 25M (owner decision 2026-07-30; 250M and 1e9 are out of scope).
#
# `hiterms*` covers `entity_id < 10^7` only. At 25M that would leave entities 10M-25M carrying no
# terms at all — invisible to every principal, 60% of the corpus dark — so those three run at the
# two smaller scales only. This is the hole probes/dataset.md §5 rule 3 warns about.
#
# Idempotent: a bundle whose CURRENT exists is skipped, so a killed run resumes by re-invocation.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURES="${FIXTURES:-/tmp/tessera-bench/fixtures}"
TESSERA="$ROOT/target/release/tessera"
GEOMETRY="$ROOT/data/scaled/geometry.parquet"
PAIRS_DIR="$ROOT/data/scaled/pairs"
LOG_DIR="${LOG_DIR:-/tmp/tessera-bench/logs}"

# The morton input branch REQUIRES the identity extent (tessera-build/src/input.rs: the scaled
# corpus stores morton codes, not coordinates, and any other extent would silently re-quantise).
EXTENT="0,65536,0,65536"

# The same fixed non-degenerate key the engine/build fixture tests use, so a bundle built here is
# comparable with one built by the test suite. Fine on a command line for a throwaway benchmark
# fixture; never do this for a deployment (contracts §2.2).
KEY="000102030405060708090a0b0c0d0e0f"

SCALES=(250000 2422486 25000000)
ALL_SETS=(categories-archive categories-subclass hash-flat surnames hiterms hiterms-ov0.5 hiterms-ov0.9)
# Entity cap per label set; 0 means uncapped.
declare -A CAP=( [hiterms]=10000000 [hiterms-ov0.5]=10000000 [hiterms-ov0.9]=10000000 )

mkdir -p "$FIXTURES" "$LOG_DIR"

[[ -x "$TESSERA" ]] || { echo "missing $TESSERA — run: cargo build --release -p tessera-cli" >&2; exit 1; }
[[ -f "$GEOMETRY" ]] || { echo "missing $GEOMETRY" >&2; exit 1; }

built=0; skipped=0; failed=0
started_all=$(date +%s)

for scale in "${SCALES[@]}"; do
  for set_name in "${ALL_SETS[@]}"; do
    cap="${CAP[$set_name]:-0}"
    if [[ "$cap" != "0" && "$scale" -gt "$cap" ]]; then
      echo "SKIP  scale=$scale set=$set_name — label set covers entity_id < $cap only"
      continue
    fi

    pairs="$PAIRS_DIR/$set_name.pairs.parquet"
    [[ -f "$pairs" ]] || { echo "SKIP  scale=$scale set=$set_name — no $pairs"; continue; }

    out="$FIXTURES/$scale/$set_name"
    if [[ -f "$out/CURRENT" ]]; then
      echo "HAVE  scale=$scale set=$set_name"
      skipped=$((skipped + 1)); continue
    fi

    log="$LOG_DIR/build-$scale-$set_name.log"
    echo "BUILD scale=$scale set=$set_name -> $out"
    rm -rf "$out"; mkdir -p "$out"
    started=$(date +%s)
    if "$TESSERA" build \
        --points "$GEOMETRY" --pairs "$pairs" --out "$out" \
        --extent "$EXTENT" --slice s0 --limit "$scale" \
        --id-key "$KEY" --epoch 1 >"$log" 2>&1; then
      elapsed=$(( $(date +%s) - started ))
      bytes=$(du -sb "$out" | cut -f1)
      echo "  ok  ${elapsed}s  $(numfmt --to=iec "$bytes")"
      built=$((built + 1))
    else
      echo "  FAIL — see $log"; tail -5 "$log" | sed 's/^/      /'
      failed=$((failed + 1))
    fi
  done
done

# Alias the 2.4M categories-subclass bundle to the path `benches/viewport.rs` and the ignored
# `latency_sanity_at_2_4m_p99_under_50ms` test both hardcode, so existing tooling keeps working
# against the same bytes rather than building a second copy.
canonical="$FIXTURES/2422486/categories-subclass"
if [[ -f "$canonical/CURRENT" && ! -e /tmp/tessera-2m4 ]]; then
  ln -s "$canonical" /tmp/tessera-2m4 && echo "LINK  /tmp/tessera-2m4 -> $canonical"
fi

echo "----"
echo "built=$built skipped=$skipped failed=$failed in $(( $(date +%s) - started_all ))s"
du -sh "$FIXTURES" 2>/dev/null
exit $(( failed > 0 ? 1 : 0 ))
