#!/usr/bin/env bash
# scripts/install-hooks.sh — give the stage-2.1 ownership check its teeth (plan rule 3).
#
# The allowlist and `check-track-allowlist.sh` were landed by the seam commit and nothing ran
# them: no hook, no CI. Both the security and the quality lens found that independently (Task 0
# gate, F2) — an allowlist a worker has to remember to consult is the "intention" plan rule 3 says
# it must not be.
#
# Run once per clone:
#
#     bash scripts/install-hooks.sh
#
# ## Why the track is a file and not a git config value
#
# Hooks are **shared across worktrees**: `git rev-parse --git-common-dir` is the same directory for
# `main` and for every `.claude/worktrees/*`, and `core.hooksPath` is a repo-level config, so one
# `pre-commit` script serves all five stage-2.1 branches. It therefore cannot know which track it
# is running for — that fact belongs to the *worktree*. Hence `.claude/track`: one gitignored line,
# written per worktree by whoever creates it.
#
# A worktree with no marker (`main`, the controller's own checkout, a scratch worktree) is not
# checked. That is deliberate: the controller commits across track boundaries by definition, and a
# hook that refused would teach everyone to pass `--no-verify`, which disables it for the tracks it
# does police too.
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
# Installed by scripts/install-hooks.sh — stage 2.1 file ownership (plan rule 3).
#
# Hooks are shared across worktrees, so the track comes from a per-worktree marker: `.claude/track`,
# one line, gitignored. No marker means "not a track worktree" and this hook does nothing — see the
# installer's doc for why that is deliberate rather than a hole.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
marker="$repo_root/.claude/track"
check="$repo_root/scripts/check-track-allowlist.sh"

[ -f "$marker" ] || exit 0
[ -f "$check" ] || exit 0

track="$(tr -d '[:space:]' < "$marker")"
[ -n "$track" ] || exit 0

if ! bash "$check" "$track" --staged; then
  echo >&2
  echo "pre-commit: refused by the track allowlist (.claude/track says '$track')." >&2
  echo "Committing with --no-verify does not make the file yours: stop and report." >&2
  exit 1
fi
HOOK
chmod +x "$hook"

echo "ok: installed $hook"
if [ -f "$repo_root/.claude/track" ]; then
  echo "ok: this worktree is track '$(tr -d '[:space:]' < "$repo_root/.claude/track")'"
else
  echo "note: no .claude/track in this worktree, so the hook will not check anything here."
  echo "      A track worktree writes it at creation:  echo c > .claude/track"
fi
