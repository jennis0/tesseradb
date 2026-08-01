#!/usr/bin/env bash
# scripts/install-hooks.sh — install the pre-commit checks. Run once per clone:
#
#     bash scripts/install-hooks.sh
#
# Two checks, both refusals a worker would otherwise have to remember to perform by hand:
#
#   - **File ownership** (`check-track-allowlist.sh`). Several branches work this tree at once and
#     must own disjoint files. Applies only to a worktree that declares a track.
#   - **Stray reformats** (`check-stray-reformat.sh`). `rustfmt <file>` follows the file's `mod`
#     declarations and rewrites everything they reach, silently. Applies everywhere.
#
# A check nobody runs is an intention, not a rule, which is why these are a hook rather than a line
# in a document. Re-run the installer after either check changes: the hook is a copy, not a link.
#
# ## Why the track is a file and not a git config value
#
# Hooks are **shared across worktrees**: `git rev-parse --git-common-dir` is the same directory for
# `main` and for every `.claude/worktrees/*`, and `core.hooksPath` is a repo-level config, so one
# `pre-commit` script serves every branch. It therefore cannot know which track it is running for —
# that fact belongs to the *worktree*. Hence `.claude/track`: one gitignored line, written per
# worktree by whoever creates it.
#
# A worktree with no marker (`main`, the controller's own checkout, a scratch worktree) is not
# ownership-checked. That is deliberate: the controller commits across track boundaries by
# definition, and a hook that refused would teach everyone to pass `--no-verify`, which disables it
# for the tracks it does police too. The reformat check has no such exemption — it is not
# track-specific, and an unintended reformat is unintended on any branch.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
common_dir="$(git rev-parse --git-common-dir)"
case "$common_dir" in
  /*) ;;
  *) common_dir="$repo_root/$common_dir" ;;
esac

hooks_path="$(git config --get core.hooksPath || true)"
if [ -n "$hooks_path" ]; then
  case "$hooks_path" in
    /*) hooks_dir="$hooks_path" ;;
    *) hooks_dir="$repo_root/$hooks_path" ;;
  esac
  echo "note: core.hooksPath is set; installing into $hooks_dir"
else
  hooks_dir="$common_dir/hooks"
fi

mkdir -p "$hooks_dir"
hook="$hooks_dir/pre-commit"

if [ -e "$hook" ] && ! grep -q 'check-track-allowlist.sh' "$hook" 2>/dev/null; then
  echo "FAIL: $hook already exists and is not this hook — move it aside and re-run" >&2
  exit 1
fi

cat > "$hook" <<'HOOK'
#!/usr/bin/env bash
# Installed by scripts/install-hooks.sh. Re-run that after changing either check.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
status=0

# 1. Stray reformats. Not track-specific, so no marker gates it.
reformat="$repo_root/scripts/check-stray-reformat.sh"
if [ -f "$reformat" ]; then
  bash "$reformat" --staged || status=1
fi

# 2. File ownership. Hooks are shared across worktrees, so the track comes from a per-worktree
# marker: `.claude/track`, one line, gitignored. No marker means "not a track worktree" — see the
# installer's doc for why that exemption is deliberate rather than a hole.
marker="$repo_root/.claude/track"
check="$repo_root/scripts/check-track-allowlist.sh"
if [ -f "$marker" ] && [ -f "$check" ]; then
  track="$(tr -d '[:space:]' < "$marker")"
  if [ -n "$track" ] && ! bash "$check" "$track" --staged; then
    echo >&2
    echo "pre-commit: refused by the track allowlist (.claude/track says '$track')." >&2
    echo "Committing with --no-verify does not make the file yours: stop and report." >&2
    status=1
  fi
fi

exit "$status"
HOOK
chmod +x "$hook"

echo "ok: installed $hook"
echo "     - scripts/check-stray-reformat.sh  (every worktree)"
if [ -f "$repo_root/.claude/track" ]; then
  echo "     - scripts/check-track-allowlist.sh (this worktree is track '$(tr -d '[:space:]' < "$repo_root/.claude/track")')"
else
  echo "     - scripts/check-track-allowlist.sh (inactive here: no .claude/track in this worktree)"
  echo "       A track worktree writes it at creation:  echo c > .claude/track"
fi
