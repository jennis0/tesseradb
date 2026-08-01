#!/usr/bin/env bash
# scripts/check-stray-reformat.sh — refuse a commit that carries files nobody meant to reformat.
#
# ## What goes wrong
#
# This tree is not `cargo fmt`-clean, so the convention is to format individual files. But
# `rustfmt <file>` follows the file's `mod` declarations and rewrites everything they reach: run it
# on a crate's `lib.rs` and the whole crate is reformatted in place, exit status 0, nothing named.
# The result is a commit — often on a branch that owns none of those files — whose diff is mostly
# other people's code with its whitespace moved. `scripts/fmt-file.sh` formats one file without
# recursing; this check is what notices when it was not used.
#
#   check-stray-reformat.sh              # the staged set (what the pre-commit hook runs)
#   check-stray-reformat.sh --selftest   # the decision's own table of cases
#
# ## What is actually checkable, and what is not
#
# A pre-commit hook sees a staged diff, never the command that produced it, so "was rustfmt run on
# a parent module" is **not** observable. What is observable is that a file's staged change is
# *exactly* what rustfmt produces from its committed content and nothing else — no semantic edit
# rode along with it. Call that a bare reformat. It is decided by formatting the committed blob and
# comparing bytes, so it neither guesses nor approximates: a file with any real change in it fails
# the comparison and is never reported.
#
# A bare reformat on its own is a legitimate, common act — it is what the convention asks a worker
# to do. **The refusal therefore triggers on two or more**, because that is the shape recursion
# produces and a hand-formatted file does not. The threshold is the whole reason the check is quiet
# enough to leave switched on: a check that fires on ordinary work teaches everyone to pass
# `--no-verify`, which also disables the ownership check sharing this hook.
#
# The cost of the threshold, stated rather than hidden: a recursion that reaches exactly one file —
# a parent already formatted, with a single unformatted child — is **not** caught. It is the one
# case where the signature and ordinary work are genuinely indistinguishable from a diff.
#
# ## Getting past it deliberately
#
# `ALLOW_BULK_REFORMAT=1 git commit ...`. Use that, never `--no-verify`: it declares this one
# intent and leaves every other hook in force.
set -uo pipefail

usage() {
  echo "usage: $0 [--staged]" >&2
  echo "       $0 --selftest" >&2
  exit 2
}

# rustfmt's style guide is edition-dependent, so a blob must be formatted against its own crate's
# edition or every file would look reformatted.
edition_for() { # edition_for <repo_root> <repo-relative path>
  local repo_root="$1" dir
  dir="$(dirname "$1/$2")"
  while [ "$dir" != "/" ] && [ "${#dir}" -ge "${#repo_root}" ]; do
    if [ -f "$dir/Cargo.toml" ]; then
      local edition
      edition="$(sed -n 's/^edition[[:space:]]*=[[:space:]]*"\([0-9]*\)".*/\1/p' "$dir/Cargo.toml" | head -1)"
      [ -n "$edition" ] && { echo "$edition"; return; }
    fi
    dir="$(dirname "$dir")"
  done
  echo 2021
}

# True when the staged content of <path> is byte-for-byte what rustfmt makes of its committed
# content — a change that moved formatting and nothing else.
#
# rustfmt is fed on **stdin** here, not by path: given a path it would descend into the file's
# module declarations and format the working tree as a side effect of running a read-only check.
is_bare_reformat() { # is_bare_reformat <repo_root> <path>
  local repo_root="$1" path="$2" head staged formatted edition
  head="$(git show "HEAD:$path" 2>/dev/null)" || return 1   # newly added: nothing to compare
  staged="$(git show ":$path" 2>/dev/null)" || return 1
  [ "$head" != "$staged" ] || return 1
  edition="$(edition_for "$repo_root" "$path")"
  formatted="$(printf '%s\n' "$head" | rustfmt --edition "$edition" --emit stdout 2>/dev/null)" || return 1
  [ "$formatted" = "$staged" ]
}

# Which file's `mod` declarations reach <path>, if any — the diagnosis that turns "these files
# changed" into "this is what you ran it on". Resolves the two standard layouts: a module `x`
# declared in `dir/{lib,main,mod}.rs` lives at `dir/x.rs` or `dir/x/mod.rs`.
declaring_parent() { # declaring_parent <path>
  local path="$1" dir stem parent
  dir="$(dirname "$path")"
  stem="$(basename "$path" .rs)"
  [ "$stem" = "mod" ] && { stem="$(basename "$dir")"; dir="$(dirname "$dir")"; }
  for parent in "$dir/lib.rs" "$dir/main.rs" "$dir/mod.rs" "$(dirname "$dir")/$(basename "$dir").rs"; do
    [ -f "$parent" ] || continue
    [ "$parent" = "$path" ] && continue
    if grep -Eq "^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]+$stem[[:space:]]*;" "$parent"; then
      echo "$parent"
      return
    fi
  done
}

# ---------------------------------------------------------------------------- the check

run_check() {
  local repo_root
  repo_root="$(git rev-parse --show-toplevel)" || exit 1

  if [ "${ALLOW_BULK_REFORMAT:-0}" = "1" ]; then
    echo "check-stray-reformat: ALLOW_BULK_REFORMAT=1 — bulk reformat declared, not checked"
    exit 0
  fi

  local -a staged=()
  mapfile -t staged < <(git diff --cached --name-only --diff-filter=M -- '*.rs')
  if [ ${#staged[@]} -eq 0 ]; then
    echo "ok: no modified Rust files staged"
    exit 0
  fi

  local -a bare=()
  local path
  for path in "${staged[@]}"; do
    [ -n "$path" ] || continue
    is_bare_reformat "$repo_root" "$path" && bare+=("$path")
  done

  if [ ${#bare[@]} -lt 2 ]; then
    echo "ok: ${#staged[@]} modified Rust file(s) staged, ${#bare[@]} of them formatting-only"
    exit 0
  fi

  echo "FAIL: ${#bare[@]} staged Rust files changed in formatting only:" >&2
  local parent
  for path in "${bare[@]}"; do
    parent="$(declaring_parent "$path")"
    if [ -n "$parent" ]; then
      echo "  $path   (a module of $parent)" >&2
    else
      echo "  $path" >&2
    fi
  done
  cat >&2 <<'MSG'

Nothing changed in these files but their layout. That is what `rustfmt <parent>` does: it follows
`mod` declarations and rewrites every file they reach, silently and with exit status 0.

  - To format one file without touching its modules:  scripts/fmt-file.sh <file>
  - To drop these from the commit:                    git restore --staged --worktree <file>...
  - If the bulk reformat is deliberate:               ALLOW_BULK_REFORMAT=1 git commit ...

Do not use --no-verify: it also disables the file-ownership check that shares this hook.
MSG
  exit 1
}

# ---------------------------------------------------------------------------- --selftest

run_selftest() {
  # Not `local`: the EXIT trap runs after this function has returned.
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  local failures=0 checked=0
  # Plants a repository, stages <case>, and asserts the verdict.
  check() { # check <description> <expected: pass|refuse> <stage-fn>
    local description="$1" expected="$2" stage_fn="$3"
    checked=$((checked + 1))
    local dir="$tmp/case-$checked"
    rm -rf "$dir"; mkdir -p "$dir/src"
    git -C "$dir" init -q
    git -C "$dir" config user.email t@example.com
    git -C "$dir" config user.name t
    cat > "$dir/Cargo.toml" <<'EOF'
[package]
name = "selftest"
version = "0.0.0"
edition = "2021"
EOF
    # Committed state: a parent declaring two modules, all three badly formatted.
    printf 'pub mod one;\npub mod two;\npub fn  root( )->u32{0}\n' > "$dir/src/lib.rs"
    printf 'pub fn  one_fn( )->u32{1}\n' > "$dir/src/one.rs"
    printf 'pub fn  two_fn( )->u32{2}\n' > "$dir/src/two.rs"
    git -C "$dir" add -A >/dev/null
    git -C "$dir" commit -qm base >/dev/null

    "$stage_fn" "$dir"
    git -C "$dir" add -A >/dev/null

    local rc=0
    (cd "$dir" && bash "$SCRIPT") >/dev/null 2>&1 || rc=$?
    local got=pass
    [ "$rc" -ne 0 ] && got=refuse
    if [ "$got" = "$expected" ]; then
      echo "  ok   $description"
    else
      echo "  FAIL $description — expected $expected, got $got" >&2
      failures=$((failures + 1))
    fi
  }

  # The accident: rustfmt run on the parent, which reaches both modules. Three bare reformats.
  stage_recursed() { rustfmt --edition 2021 "$1/src/lib.rs"; }
  # The convention working as intended: exactly one file formatted, on purpose.
  stage_one_file() { rustfmt --edition 2021 --config skip_children=true "$1/src/lib.rs"; }
  # Real work: a semantic change, unformatted, in two files.
  stage_real_edit() {
    printf 'pub fn  one_fn( )->u32{11}\n' > "$1/src/one.rs"
    printf 'pub fn  two_fn( )->u32{22}\n' > "$1/src/two.rs"
  }
  # Real work that was then formatted — the diff carries a semantic change, so it is not bare.
  stage_edit_then_format() {
    printf 'pub fn  one_fn( )->u32{11}\n' > "$1/src/one.rs"
    printf 'pub fn  two_fn( )->u32{22}\n' > "$1/src/two.rs"
    rustfmt --edition 2021 --config skip_children=true "$1/src/one.rs"
    rustfmt --edition 2021 --config skip_children=true "$1/src/two.rs"
  }
  # A new module: no committed blob to compare against, so it can never be a bare reformat.
  stage_new_file() {
    printf 'pub mod one;\npub mod two;\npub mod three;\npub fn  root( )->u32{0}\n' > "$1/src/lib.rs"
    printf 'pub fn  three_fn( )->u32{3}\n' > "$1/src/three.rs"
  }
  # The declared escape hatch must still let the accident's own shape through.
  stage_recursed_allowed() { ALLOW_BULK_REFORMAT=1; export ALLOW_BULK_REFORMAT; rustfmt --edition 2021 "$1/src/lib.rs"; }

  check "rustfmt on a module-declaring parent is refused"            refuse stage_recursed
  check "formatting one file on purpose passes"                      pass   stage_one_file
  check "an ordinary multi-file edit passes"                         pass   stage_real_edit
  check "editing then formatting the same files passes"              pass   stage_edit_then_format
  check "adding a new module passes"                                 pass   stage_new_file
  check "ALLOW_BULK_REFORMAT=1 declares the intent and passes"       pass   stage_recursed_allowed
  unset ALLOW_BULK_REFORMAT

  if [ "$failures" -gt 0 ]; then
    echo "FAIL: $failures of $checked selftest case(s) disagree with the check" >&2
    exit 1
  fi
  echo "ok: $checked selftest case(s) — the recursion is refused and ordinary work is not"
}

# ---------------------------------------------------------------------------- dispatch

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

command -v rustfmt >/dev/null 2>&1 || {
  echo "check-stray-reformat: rustfmt not on PATH — not checking" >&2
  exit 0
}

case "${1:---staged}" in
  --staged) [ $# -le 1 ] || usage; run_check ;;
  --selftest) [ $# -eq 1 ] || usage; run_selftest; exit 0 ;;
  *) usage ;;
esac
