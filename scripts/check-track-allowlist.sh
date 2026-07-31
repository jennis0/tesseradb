#!/usr/bin/env bash
# scripts/check-track-allowlist.sh — stage 2.1 file ownership, enforced (plan rule 3).
#
# Four tracks work in parallel off the seam commit and must own disjoint files. This reads
# `.claude/track-allowlist.toml`, diffs the working branch (or the index) against the merge base,
# and fails naming every changed file the named track does not own.
#
# It is a *check*, not a merge policy: a file outside the list means stop and report to the
# controller, who either reassigns it or sequences the two tracks. Editing the allowlist to make
# this pass is the failure mode it exists to prevent.
#
#   check-track-allowlist.sh <track>              # branch: merge base -> working tree
#   check-track-allowlist.sh <track> --staged     # index only (pre-commit)
#   check-track-allowlist.sh <track> --base <ref> # compare against something other than main
#
# `<track>` is a section name from the allowlist without the `track.` prefix: a, b, c, t.
set -euo pipefail

usage() {
  echo "usage: $0 <track> [--staged] [--base <ref>]" >&2
  echo "  <track>  one of the [track.*] sections in .claude/track-allowlist.toml" >&2
  exit 2
}

[ $# -ge 1 ] || usage
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

repo_root="$(git rev-parse --show-toplevel)"
allowlist="$repo_root/.claude/track-allowlist.toml"
[ -f "$allowlist" ] || { echo "FAIL: no allowlist at $allowlist" >&2; exit 1; }

# Every quoted string under [shared] and under [track.<track>]. A tiny awk state machine rather
# than a TOML parser: the file's shape is fixed and this script must run with nothing installed.
patterns_for() { # patterns_for <section-name, e.g. "shared" or "track.b">
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

mapfile -t patterns < <(patterns_for "shared"; patterns_for "track.$track")
# `shared` alone is not an allowlist: an unknown track name would otherwise pass everything the
# shared section permits and refuse everything else, which reads like a real result.
mapfile -t track_patterns < <(patterns_for "track.$track")
if [ ${#track_patterns[@]} -eq 0 ]; then
  echo "FAIL: no [track.$track] section in $allowlist" >&2
  awk '/^\[track\./ { gsub(/[][]/, ""); sub(/^track\./, ""); printf "  known track: %s\n", $0 }' "$allowlist" >&2
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

violations=()
for file in "${changed[@]}"; do
  [ -n "$file" ] || continue
  allowed=0
  for pattern in "${patterns[@]}"; do
    # shellcheck disable=SC2053  # right-hand side is a glob on purpose
    if [[ "$file" == $pattern ]]; then allowed=1; break; fi
  done
  [ "$allowed" -eq 1 ] || violations+=("$file")
done

if [ ${#violations[@]} -gt 0 ]; then
  echo "FAIL: track '$track' touched ${#violations[@]} file(s) it does not own ($scope):" >&2
  printf '  %s\n' "${violations[@]}" >&2
  echo "Stop and report to the controller (plan rule 3). Do not widen the allowlist to pass." >&2
  exit 1
fi

echo "ok: track '$track' — ${#changed[@]} changed path(s), all within its allowlist ($scope)"
