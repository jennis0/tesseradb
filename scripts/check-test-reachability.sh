#!/usr/bin/env bash
# scripts/check-test-reachability.sh — which tests CI actually runs, versus which tests exist.
#
# **Why this exists.** A test that does not run is indistinguishable, from the outside, from a test
# that passes: both are silence. Three mechanisms in this repository make a test stop running
# without anyone deleting it, and none of them is visible in a diff.
#
#   1. `#[ignore]`. CI's `cargo test --workspace` skips them, by design — several take minutes.
#      An ignore WITHOUT a reason string is the defect this script refuses: nobody reading it can
#      tell "slow, run it before a release" from "broken, someone silenced it".
#   2. `#[cfg(feature = "...")]` on a test, where the feature is off in the crate's own build and
#      on in the workspace build. Cargo's resolver-2 unifies features across a `--workspace`
#      invocation, so such a test compiles under `cargo test --workspace` and VANISHES under
#      `cargo test -p <crate>` — the command a developer runs while working on that crate. The
#      manifests of tessera-engine, tessera-lifecycle and tessera-server already record that
#      unification as measured rather than assumed; this script is where the consequence for tests
#      is counted.
#   3. A test binary that no longer builds under the narrower selection at all.
#
# The answer is derived from `cargo test -- --list`, which asks the compiled harness what it would
# run, rather than from a grep over `#[test]`, which cannot see any of the three.
#
# `--quick` is in the gate (CLAUDE.md's gate block, and the `rust` job in
# `.github/workflows/ci.yml`, where it reuses that job's build). It skips the per-crate sweep
# (step 3) and keeps steps 1 and 2. The full run stays out: the sweep re-resolves features once per
# workspace member, so it recompiles the world several times over. Run it when a feature gate or a
# dev-dependency changes.
#
# Exit non-zero on: a bare `#[ignore]`. Everything else is reported and does not fail — a test that
# exists only under `--workspace` is a fact the operator dispositions, not a build to block.
#
# **Why a refusal here does not contradict CLAUDE.md's "report loudly and do not block a build".**
# That rule governs operator-facing behaviour over real data — joins, coverage, extents — where a
# refusal moves the cost onto a caller who often cannot act on it. This is a developer-facing lint
# over the repository's own source, and whoever trips it can satisfy it in the same edit: write the
# reason you already know. It is the same class as `clippy -D warnings`, which this repository
# already blocks on.
set -euo pipefail

cd "$(dirname "$0")/.."

quick=0
if [ "${1:-}" = "--quick" ]; then quick=1; shift; fi
only="$*"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# `--list` names every test the harness would run, ignored ones included and undistinguished;
# `--list --ignored` names the ignored subset. The difference is what a plain `cargo test` runs.
# Names are deduplicated: two crates may name a test the same thing, and set arithmetic below
# needs unique keys. The undeduplicated total is printed beside it so the gap is visible rather
# than surprising.
list_tests() { # list_tests <cargo selection args...>
  cargo test "$@" -- --list 2>/dev/null | sed -n 's/: test$//p' | sort -u
}
list_ignored() { # list_ignored <cargo selection args...>
  cargo test "$@" -- --list --ignored 2>/dev/null | sed -n 's/: test$//p' | sort -u
}

echo "== 1. what exists, and what the per-PR gate executes"
cargo test --workspace -- --list 2>/dev/null | grep -c ': test$' > "$work/raw" || true
list_tests --workspace > "$work/ws"
list_ignored --workspace > "$work/ws-ignored"
exist=$(wc -l < "$work/ws")
ignored=$(wc -l < "$work/ws-ignored")
echo "  test entries the workspace harnesses list: $(cat "$work/raw")"
echo "  distinct test names among them:            $exist"
echo "  of those, #[ignore]d (CI skips):           $ignored"
echo "  executed by \`cargo test --workspace\`:     $((exist - ignored))"

echo
echo "== 2. #[ignore] without a reason string"
# `#[ignore]` with nothing after it. A reason is `#[ignore = "..."]`; both forms are on one line.
bare=$(grep -rn --include=*.rs -E '^\s*#\[ignore\]\s*$' crates/ || true)
if [ -n "$bare" ]; then
  echo "$bare" | sed 's/^/  BARE IGNORE: /'
  bare_count=$(echo "$bare" | wc -l)
else
  bare_count=0
fi
echo "  bare ignores: $bare_count"

if [ "$quick" = 1 ]; then
  echo
  echo "== 3. skipped (--quick)"
  [ "$bare_count" -eq 0 ] || exit 1
  exit 0
fi

echo
echo "== 3. tests reachable only under --workspace"
# One `cargo test -p <crate>` per member, which is what a developer working on that crate runs.
# A test present in the workspace listing and absent from its own crate's listing exists only
# because some OTHER member's default features unified onto this crate.
if [ -n "$only" ]; then
  members="$only"
else
  members=$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys; print("\n".join(p["name"] for p in json.load(sys.stdin)["packages"]))' \
    | sort)
fi
only_ws=0
for m in $members; do
  list_tests -p "$m" > "$work/pkg" || true
  # Restrict the workspace listing to names this crate's own listing could have carried. Test
  # names are not crate-qualified in `--list` output, so compare the whole sets and report the
  # workspace-only names that the per-crate build of THIS crate was expected to produce; a name
  # belonging to another crate is filtered out by taking the intersection with the crate's
  # source-visible test names.
  comm -23 "$work/ws" "$work/pkg" > "$work/missing"
  # Keep only names whose final segment appears as an `fn` in this crate's sources — enough to
  # attribute a workspace-only test to the crate that lost it, without parsing the harness.
  #
  # **And then drop the ones this crate did not lose.** `--list` names are not crate-qualified, so
  # a name carried by two members appears once in the deduplicated workspace listing under each
  # module path it has. A crate whose own listing already contains that final segment did not lose
  # anything — the missing spelling belongs to the other member. Without this the source grep alone
  # reports a false positive for every duplicated test name, which it did:
  # `an_undersized_bound_does_not_livelock` exists in both `tessera-cache/src` and
  # `tessera-engine/tests/cache.rs`, and the bare spelling from the second was attributed to the
  # first, whose own listing carries it under `tests::`.
  #
  # The trade is deliberate: a crate that genuinely lost a test *and* still lists another test of
  # the same final segment is filtered out too. That is the rarer error, and this is a diagnostic —
  # a false positive here costs an investigation, which is what it cost.
  while read -r t; do
    fn="${t##*::}"
    grep -qs -E "(^|::)${fn}\$" "$work/pkg" && continue
    if grep -rqs --include=*.rs -E "fn ${fn}\s*\(" "crates/$m" 2>/dev/null; then
      echo "  ONLY UNDER --workspace: $m :: $t"
      only_ws=$((only_ws + 1))
    fi
  done < "$work/missing"
done
echo "  tests reachable only under --workspace: $only_ws"

[ "$bare_count" -eq 0 ] || exit 1
