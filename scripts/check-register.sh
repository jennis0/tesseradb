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
STRICT=(CLAUDE.md docs/README.md docs/index.md docs/system docs/start docs/guides docs/reference docs/developer)
# Files under a STRICT directory that are counted with LOOSE until they are rewritten.
NOT_STRICT=(docs/guides/views.md)
LOOSE=(README.md docs/openapi docs/guides/views.md docs/roadmap.md docs/outstanding.md docs/ingest-campaign.md)
# The pages a user of Tessera reads, which USER_PATTERNS also apply to.
USER_PAGES=(docs/index.md docs/start docs/guides docs/reference)

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

USER_PATTERNS=$(cat <<'EOF'
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
USER_REGEX="$REGEX|$(printf '%s\n' "$USER_PATTERNS" | paste -sd'|')"

listed() {  # listed <file> <path>...: whether the file is one of the paths or under one
  local f=$1 p
  shift
  for p in "$@"; do
    case "$f" in "$p" | "$p"/*) return 0 ;; esac
  done
  return 1
}

# Every matching line as file:line:text. Fenced blocks, backticked spans and link targets are not
# checked; a fenced line is blanked so the line numbers still match the file.
hits() {  # hits <path>...
  local p f regex
  for p in "$@"; do
    [ -e "$p" ] || continue
    find "$p" -name '*.md' -type f | sort | while read -r f; do
      if [ "${SKIP_NOT_STRICT:-}" = 1 ] && listed "$f" "${NOT_STRICT[@]}"; then continue; fi
      regex=$REGEX
      listed "$f" "${USER_PAGES[@]}" && regex=$USER_REGEX
      awk '/^[[:space:]]*(```|~~~)/ { fenced = !fenced; print ""; next } fenced { print ""; next } { print }' "$f" |
        sed -e 's/`[^`]*`//g' -e 's/\](\([^)]*\))/]/g' | grep -niE -e "$regex" | sed "s|^|$f:|"
    done
  done
}

strict=$(SKIP_NOT_STRICT=1 hits "${STRICT[@]}")
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
