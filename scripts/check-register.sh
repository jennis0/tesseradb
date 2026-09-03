#!/usr/bin/env bash
# Flags the vocabulary docs/agents/writing.md's Register section forbids, in the files that have
# been rewritten to that standard. Files not yet rewritten are counted but do not fail; add a path
# to STRICT when its rewrite lands.
#
#   bash scripts/check-register.sh          # strict files fail on any hit; the rest are counted
#   bash scripts/check-register.sh --all    # list every hit everywhere
set -u
cd "$(dirname "$0")/.."

# docs/agents/writing.md is the source of the list and quotes it, so it is checked by neither set.
# docs/decisions/README.md repeats the decisions' titles and is counted with them.
STRICT=(CLAUDE.md docs/README.md docs/agents/README.md docs/system docs/guide docs/developer docs/reference)
LOOSE=(README.md docs/design docs/decisions docs/evidence docs/roadmap.md docs/ingest-campaign.md docs/guides
       docs/agents/design-process.md docs/agents/epic-lifecycle.md docs/agents/parallel-work.md)

PATTERNS=$(cat <<'EOF'
—
\bload-bearing\b
\bdischarg(e|es|ed|ing)\b
\bobligations?\b
\bowe[sd]?\b
\bthe (lane|seam|frontier|spine)\b
\bwhich is (exactly|precisely|the point)\b
\bby construction\b
\bdeliberately\b
\bprecisely\b
\bsilently\b
\bquietly\b
\bcatastrophic\b
\bimportantly\b
\bcrucially\b
\bnote that\b
\bworth (noting|stating)\b
\bthe (real|key) (question|thing|insight)\b
\bthe failure (mode )?(this|it) (exists|is there) to prevent\b
\barguably\b
\bin practice\b
\btends to\b
\bto some extent\b
\bis not [a-z ]{1,30}, it is\b
\bnot an? [a-z]+, but an?\b
\bcaught in review\b
\bdo not rediscover\b
\bis the deliverable\b
\bpostings?\b
\bdescriptors?\b
\b(the )?ladder\b
\brungs?\b
\bcampaign\b
\bepics?\b
\bto measure\b
EOF
)

REGEX=$(printf '%s\n' "$PATTERNS" | paste -sd'|')

hits() {  # hits <path>...: every matching line as file:line:text; backticked spans are not checked
  local p f
  for p in "$@"; do
    [ -e "$p" ] || continue
    find "$p" -name '*.md' -type f | sort | while read -r f; do
      sed 's/`[^`]*`//g' "$f" | grep -niE -e "$REGEX" | sed "s|^|$f:|"
    done
  done
}

strict=$(hits "${STRICT[@]}")
if [ -n "$strict" ]; then
  echo "$strict"
  echo "check-register: hits in files held to the register (above)"
  exit 1
fi

if [ "${1:-}" = "--all" ]; then
  hits "${LOOSE[@]}"
else
  for p in "${LOOSE[@]}"; do
    [ -e "$p" ] || continue
    printf '%6d  %s\n' "$(hits "$p" | wc -l)" "$p"
  done | sort -rn
  echo "check-register: strict files clean; counts above are files not yet rewritten"
fi
