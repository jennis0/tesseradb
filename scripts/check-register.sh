#!/usr/bin/env bash
# Flags the vocabulary docs/writing.md's Register section forbids, in the files that have
# been rewritten to that standard. Files not yet rewritten are counted but do not fail; add a path
# to STRICT when its rewrite lands.
#
#   bash scripts/check-register.sh          # strict files fail on any hit; the rest are counted
#   bash scripts/check-register.sh --all    # list every hit everywhere
set -u
cd "$(dirname "$0")/.."

# docs/writing.md is the source of the list and quotes it, so it is checked by neither set.
# docs/system belongs in STRICT and is counted here until its remaining hits are rewritten.
STRICT=(CLAUDE.md docs/README.md docs/index.md docs/start docs/guides/index.md docs/reference docs/developer)
LOOSE=(docs/system README.md docs/openapi docs/guides/views.md docs/roadmap.md docs/outstanding.md docs/ingest-campaign.md)

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
\bin this (guide|tutorial|section|page)\b
\blet['’]s\b
\bwe will\b
\bwe['’]ll\b
\bseamless(ly)?\b
\brobust\b
\bpowerful\b
\bleverag(e|es|ed|ing)\b
\butili[sz](e|es|ed|ing)\b
\bdelve
\bsimply\b
\bjust\b
\beasily\b
\beffortless(ly)?\b
\bunder the hood\b
\bout of the box\b
\ba (wide|broad) range of\b
\bwhether you(['’]re| are)\b
\bit['’]s worth\b
\bgame.chang
\bstreamline
\bempower
\bunlock
\bdive (in|into|deeper)\b
\bin (summary|conclusion)\b
\bto summari[sz]e\b
\bkey takeaways?\b
\bcomprehensive\b
\bensure that\b
EOF
)

REGEX=$(printf '%s\n' "$PATTERNS" | paste -sd'|')

hits() {  # hits <path>...: every matching line as file:line:text; backticked spans and link targets are not checked
  local p f
  for p in "$@"; do
    [ -e "$p" ] || continue
    find "$p" -name '*.md' -type f | sort | while read -r f; do
      sed -e 's/`[^`]*`//g' -e 's/\](\([^)]*\))/]/g' "$f" | grep -niE -e "$REGEX" | sed "s|^|$f:|"
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
