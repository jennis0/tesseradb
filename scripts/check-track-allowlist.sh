#!/usr/bin/env bash
# scripts/check-track-allowlist.sh — parallel-track file ownership, enforced (decision 0010).
#
# Four tracks work in parallel off the seam commit and must own disjoint files. This reads
# `.claude/track-allowlist.toml`, diffs the working branch (or the index) against the merge base,
# and fails naming every changed file the named track does not own.
#
# It is a *check*, not a merge policy: a file outside the list means stop and report to the
# controller, who either reassigns it or sequences the two tracks. Editing the allowlist to make
# this pass is the failure mode it exists to prevent.
#
# **Two verdicts, not one.** `[frozen]` is consulted BEFORE the allowlist and refuses for every
# track, with its own message; `[shared]` ∪ `[track.<t>]` is the allowlist proper. The distinction
# is load-bearing: a frozen path listed under `[shared]` reads as a freeze while behaving as a
# universal permit, which is what it did before `[frozen]` was consulted first.
#
#   check-track-allowlist.sh <track>              # branch: merge base -> working tree
#   check-track-allowlist.sh <track> --staged     # index only (pre-commit)
#   check-track-allowlist.sh <track> --base <ref> # compare against something other than main
#   check-track-allowlist.sh --audit              # overlaps, dead patterns, the unclaimed set
#   check-track-allowlist.sh --selftest           # the matcher's own table of cases
#
# `<track>` is a section name from the allowlist without the `track.` prefix: a, b, c, t.
set -euo pipefail

usage() {
  echo "usage: $0 <track> [--staged] [--base <ref>]" >&2
  echo "       $0 --audit | --selftest" >&2
  echo "  <track>  one of the [track.*] sections in .claude/track-allowlist.toml" >&2
  exit 2
}

[ $# -ge 1 ] || usage

repo_root="$(git rev-parse --show-toplevel)"
allowlist="$repo_root/.claude/track-allowlist.toml"
[ -f "$allowlist" ] || { echo "FAIL: no allowlist at $allowlist" >&2; exit 1; }

# Every quoted string under a named section. A tiny awk state machine rather than a TOML parser:
# the file's shape is fixed and this script must run with nothing installed.
patterns_for() { # patterns_for <section-name, e.g. "shared", "frozen" or "track.b">
  awk -v want="$1" '
    /^[[:space:]]*\[/ {
      hdr = $0
      gsub(/^[[:space:]]*\[|\][[:space:]]*$/, "", hdr)
      in_section = (hdr == want)
      next
    }
    !in_section { next }
    /^[[:space:]]*#/ { next }
    {
      line = $0
      while (match(line, /"[^"]*"/)) {
        s = substr(line, RSTART + 1, RLENGTH - 2)
        if (s != "") print s
        line = substr(line, RSTART + RLENGTH)
      }
    }
  ' "$allowlist"
}

track_names() {
  awk '/^\[track\./ { gsub(/[][]/, ""); sub(/^track\./, ""); print }' "$allowlist"
}

# `[[ path == pattern ]]` with the right-hand side unquoted on purpose: it is a glob, and `*` spans
# `/` (bash globs in `[[ ]]` are not path-aware), which is why `crates/tessera-lifecycle/**` covers
# a whole subtree.
matches_any() { # matches_any <file> <pattern>...
  local file="$1"; shift
  local pattern
  for pattern in "$@"; do
    # shellcheck disable=SC2053  # right-hand side is a glob on purpose
    if [[ "$file" == $pattern ]]; then return 0; fi
  done
  return 1
}

# The whole ownership decision, in one place so `--selftest` exercises exactly what a real run
# does. Prints one of: frozen | allowed | unowned.
classify() { # classify <track> <file>
  local track="$1" file="$2"
  local -a frozen allowed
  mapfile -t frozen < <(patterns_for "frozen")
  mapfile -t allowed < <(patterns_for "shared"; patterns_for "track.$track")
  if matches_any "$file" ${frozen[@]+"${frozen[@]}"}; then
    echo frozen
  elif matches_any "$file" ${allowed[@]+"${allowed[@]}"}; then
    echo allowed
  else
    echo unowned
  fi
}

FROZEN_MESSAGE="this file is frozen for stage 2.1 — every track has its own file to add tests to, \
so needing to change this one usually means a seam was drawn in the wrong place. Stop and report \
to the controller."

# ---------------------------------------------------------------------------- --audit

# Prints what the allowlist claims, from three angles a reader cannot get by eye: which paths two
# tracks both claim, which patterns match nothing on disk, and what nobody claims at all. The Task
# 0 gate's C1, C2 and F8 were all visible from one of these three and were found by a human
# reading the file instead.
run_audit() {
  local -a tracks
  mapfile -t tracks < <(track_names)

  echo "== frozen (refused for every track) =="
  local -a frozen
  mapfile -t frozen < <(patterns_for "frozen")
  local pattern
  for pattern in ${frozen[@]+"${frozen[@]}"}; do
    echo "  $pattern"
  done

  echo
  echo "== per-track claims =="
  local track
  for track in "${tracks[@]}"; do
    echo "  track $track: $(patterns_for "track.$track" | wc -l) pattern(s)"
  done
  echo "  shared:  $(patterns_for shared | wc -l) pattern(s)"

  echo
  # Not automatically a defect: the plan arranges exactly one deliberate overlap
  # (`scripts/check-layers.sh` — "touched by A and B, split by rule, each track appending its own,
  # reviewed together at integration"). Anything else here is two tracks claiming one file, which
  # is what `[shared]` is for.
  echo "== overlaps (claimed by more than one track — expected: scripts/check-layers.sh only) =="
  local overlaps
  overlaps="$(for track in "${tracks[@]}"; do patterns_for "track.$track"; done | sort | uniq -d)"
  if [ -n "$overlaps" ]; then
    echo "$overlaps" | sed 's/^/  /'
  else
    echo "  (none)"
  fi

  echo
  # A pattern naming a file a track is supposed to CREATE is legitimately dead until it does.
  # The ones worth acting on are typos and paths that moved.
  echo "== dead patterns (match no tracked file — a typo, a moved path, or a file not yet created) =="
  local -a all_files
  mapfile -t all_files < <(git -C "$repo_root" ls-files)
  local dead=0
  local section
  for section in frozen shared $(for track in "${tracks[@]}"; do echo "track.$track"; done); do
    while IFS= read -r pattern; do
      local hit=0 file
      for file in "${all_files[@]}"; do
        if matches_any "$file" "$pattern"; then hit=1; break; fi
      done
      if [ "$hit" -eq 0 ]; then echo "  [$section] $pattern"; dead=1; fi
    done < <(patterns_for "$section")
  done
  [ "$dead" -eq 1 ] || echo "  (none)"

  echo
  echo "== unclaimed (matched by no section: every track that needs one must stop and report) =="
  local -a claimed
  mapfile -t claimed < <(patterns_for frozen; patterns_for shared;
                         for track in "${tracks[@]}"; do patterns_for "track.$track"; done)
  local unclaimed=0 file
  for file in "${all_files[@]}"; do
    if ! matches_any "$file" ${claimed[@]+"${claimed[@]}"}; then
      echo "  $file"
      unclaimed=$((unclaimed + 1))
    fi
  done
  echo "  ($unclaimed unclaimed of ${#all_files[@]} tracked files — a large number is EXPECTED;"
  echo "   the allowlist names what a track needs, not the whole tree. Read it for files a task"
  echo "   in the plan names.)"
}

# ---------------------------------------------------------------------------- --selftest

# The demonstration: a frozen path is refused for EVERY track,
# including tracks whose own section is otherwise permissive, and the ordinary verdicts still hold.
# Table-driven against `classify`, which is the same function the real run uses — a selftest over a
# reimplementation of the matcher would prove nothing about the matcher.
run_selftest() {
  local failures=0 checked=0
  check() { # check <track> <file> <expected>
    local got
    got="$(classify "$1" "$2")"
    checked=$((checked + 1))
    if [ "$got" != "$3" ]; then
      echo "  FAIL: track '$1' + '$2' -> $got, expected $3" >&2
      failures=$((failures + 1))
    fi
  }

  local track
  # 1. The freeze binds every track. This is the case that regressed: with these two paths in
  #    `[shared]`, every one of these expected `allowed` and the freeze meant its own opposite.
  for track in $(track_names); do
    check "$track" "crates/tessera-server/tests/http.rs" frozen
    check "$track" "crates/tessera-engine/tests/viewport.rs" frozen
  done
  # 2. A shared path is permitted for every track — the contrast that makes point 1 meaningful.
  for track in $(track_names); do
    check "$track" "crates/tessera-engine/src/session.rs" allowed
    check "$track" "Cargo.toml" allowed
    check "$track" "docs/evidence/memos/2026-07-30-viewport-hot-path-and-bundle-size-review.md" allowed
  done
  # 3. Each track's own files are accepted.
  check a "crates/tessera-store/src/read.rs" allowed
  check a "crates/tessera-engine/src/select.rs" allowed
  check b "crates/tessera-engine/src/write.rs" allowed
  check b "crates/tessera-lifecycle/src/command.rs" allowed
  check b "crates/tessera-server/src/error.rs" allowed
  check c "crates/tessera-engine/src/pins.rs" allowed
  check c "crates/tessera-engine/src/cache.rs" allowed
  check c "crates/tessera-engine/src/viewport.rs" allowed
  check c "crates/tessera-engine/src/single_flight.rs" allowed
  check c "crates/tessera-server/tests/http_engine_state.rs" allowed
  check t "reference/oracle/viewport.py" allowed
  # 4. And another track's files are not — the check's whole purpose.
  check a "crates/tessera-engine/src/write.rs" unowned
  check b "crates/tessera-engine/src/pins.rs" unowned
  check c "crates/tessera-server/src/control.rs" unowned
  check t "crates/tessera-engine/src/select.rs" unowned
  # 5. A path in no section at all is unowned, for everyone.
  for track in $(track_names); do
    check "$track" "crates/tessera-build/src/lib.rs" unowned
  done

  if [ "$failures" -gt 0 ]; then
    echo "FAIL: $failures of $checked selftest case(s) disagree with the allowlist" >&2
    exit 1
  fi
  echo "ok: $checked selftest case(s) — the freeze refuses every track, and each track's own files pass"
}

# ---------------------------------------------------------------------------- dispatch

case "$1" in
  --audit) [ $# -eq 1 ] || usage; run_audit; exit 0 ;;
  --selftest) [ $# -eq 1 ] || usage; run_selftest; exit 0 ;;
  --*) usage ;;
esac

track="$1"; shift
mode="branch"
base="${TRACK_ALLOWLIST_BASE:-main}"
while [ $# -gt 0 ]; do
  case "$1" in
    --staged) mode="staged"; shift ;;
    --branch) mode="branch"; shift ;;
    --base) [ $# -ge 2 ] || usage; base="$2"; shift 2 ;;
    *) usage ;;
  esac
done

mapfile -t frozen_patterns < <(patterns_for "frozen")
mapfile -t patterns < <(patterns_for "shared"; patterns_for "track.$track")
# `shared` alone is not an allowlist: an unknown track name would otherwise pass everything the
# shared section permits and refuse everything else, which reads like a real result.
mapfile -t track_patterns < <(patterns_for "track.$track")
if [ ${#track_patterns[@]} -eq 0 ]; then
  echo "FAIL: no [track.$track] section in $allowlist" >&2
  track_names | sed 's/^/  known track: /' >&2
  exit 1
fi

if [ "$mode" = "staged" ]; then
  mapfile -t changed < <(git diff --cached --name-only)
  scope="staged changes"
else
  merge_base="$(git merge-base HEAD "$base" 2>/dev/null || true)"
  if [ -z "$merge_base" ]; then
    echo "FAIL: no merge base between HEAD and '$base' — pass --base <ref>" >&2
    exit 1
  fi
  # Two-dot against the merge base, so this covers committed, staged AND unstaged work in one
  # command: a track that has not committed yet is still a track that touched the file.
  mapfile -t changed < <(git diff --name-only "$merge_base"; git ls-files --others --exclude-standard)
  scope="branch work since $(git rev-parse --short "$merge_base")"
fi

frozen_hits=()
violations=()
for file in "${changed[@]}"; do
  [ -n "$file" ] || continue
  # Frozen first, and it is not an allowlist entry: no `[shared]`/`[track.*]` pattern can rescue a
  # path named here.
  if matches_any "$file" ${frozen_patterns[@]+"${frozen_patterns[@]}"}; then
    frozen_hits+=("$file")
  elif ! matches_any "$file" ${patterns[@]+"${patterns[@]}"}; then
    violations+=("$file")
  fi
done

status=0
if [ ${#frozen_hits[@]} -gt 0 ]; then
  echo "FAIL: track '$track' changed ${#frozen_hits[@]} FROZEN file(s) ($scope):" >&2
  printf '  %s\n' "${frozen_hits[@]}" >&2
  echo "$FROZEN_MESSAGE" >&2
  status=1
fi
if [ ${#violations[@]} -gt 0 ]; then
  echo "FAIL: track '$track' touched ${#violations[@]} file(s) it does not own ($scope):" >&2
  printf '  %s\n' "${violations[@]}" >&2
  echo "Stop and report to the controller (plan rule 3). Do not widen the allowlist to pass." >&2
  status=1
fi
[ "$status" -eq 0 ] || exit 1

echo "ok: track '$track' — ${#changed[@]} changed path(s), all within its allowlist ($scope)"
