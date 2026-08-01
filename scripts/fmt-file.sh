#!/usr/bin/env bash
# scripts/fmt-file.sh — format the Rust files you named, and nothing else.
#
# ## Why this is not just `rustfmt`
#
# This tree is not `cargo fmt`-clean, so the convention is to format the files a change touches
# rather than the workspace. But `rustfmt <file>` is **not** a single-file operation: given a file
# that declares modules, it follows every `mod` declaration and rewrites the whole subtree in
# place. Running it on a crate's `lib.rs` therefore reformats the entire crate, including files
# owned by someone else's branch. The damage is silent — the exit status is 0 and rustfmt names
# nothing it touched — and it has only ever been noticed by an author reading `git status` before
# staging.
#
#   fmt-file.sh crates/tessera-engine/src/select.rs [more files...]
#   fmt-file.sh --check crates/tessera-engine/src/select.rs   # report, change nothing
#   fmt-file.sh --selftest
#
# ## How the promise is kept, and how it is proved
#
# Two independent mechanisms, because the first one alone would be an assertion:
#
#   1. `--config skip_children=true` tells rustfmt not to descend into `mod` targets.
#   2. Every Rust file in the repository is snapshotted before the run and compared after. If
#      anything outside the named set moved, this script **restores it from the snapshot and
#      fails**, rather than reporting success over a change nobody asked for.
#
# The second exists because the first is a rustfmt option this script cannot verify by inspection:
# an option that is silently ignored — by a newer toolchain, a different channel, or a config file
# arriving in the tree — would reinstate exactly the failure this script exists to prevent, and
# would do so quietly. The snapshot converts that from a silent regression into a refusal. The
# `--selftest` proves the guard fires by deliberately disabling mechanism 1 and checking that
# mechanism 2 catches it; a guard that has never been observed to fail is not known to work.
#
# Restoration is from the snapshot, not from git, so uncommitted work in a collaterally-formatted
# file survives the refusal.
set -uo pipefail

usage() {
  echo "usage: $0 [--check] <file.rs> [file.rs...]" >&2
  echo "       $0 --selftest" >&2
  exit 2
}

# Set only by --selftest, to run rustfmt without the skip_children guard and so demonstrate that
# the snapshot comparison catches a recursion this script did not intend. Never set it by hand.
: "${FMT_FILE_UNSAFE_NO_SKIP:=0}"

# The style guide rustfmt applies is edition-dependent, so a file must be formatted against its own
# crate's edition or the result is a reformat nobody asked for. Read it from the nearest ancestor
# manifest that states one.
edition_for() { # edition_for <file>
  local dir
  dir="$(cd "$(dirname "$1")" && pwd)"
  while [ "$dir" != "/" ]; do
    if [ -f "$dir/Cargo.toml" ]; then
      local edition
      edition="$(sed -n 's/^edition[[:space:]]*=[[:space:]]*"\([0-9]*\)".*/\1/p' "$dir/Cargo.toml" | head -1)"
      [ -n "$edition" ] && { echo "$edition"; return; }
    fi
    dir="$(dirname "$dir")"
  done
  echo 2021
}

# Every Rust file git knows about, tracked or not, ignoring what `.gitignore` excludes. Untracked
# files are included because a file rustfmt drags in may well be one a worker has not staged yet —
# that is the common shape of the accident, not an unusual one.
all_rust_files() { # all_rust_files <repo_root>
  git -C "$1" ls-files -z '*.rs'
  git -C "$1" ls-files -z --others --exclude-standard '*.rs'
}

# One `<path>\t<digest>` line per Rust file, sorted whole-line in the C locale. The field order is
# path-first and the sort is over the whole line because the two snapshots are compared with
# `comm`, which is only correct on input sorted the way it compares: digest-first output sorted by
# path is *not*, and silently reports files that never moved.
hash_all_rust_files() { # hash_all_rust_files <repo_root>
  (cd "$1" && all_rust_files . | xargs -0 sha256sum 2>/dev/null) \
    | awk '{ digest = $1; sub(/^[^ ]+  /, ""); print $0 "\t" digest }' \
    | LC_ALL=C sort
}

# ---------------------------------------------------------------------------- the run

run_format() { # run_format <check-only:0|1> <file>...
  local check_only="$1"; shift

  local repo_root
  repo_root="$(git rev-parse --show-toplevel)" || {
    echo "FAIL: not inside a git repository — this script needs one to snapshot against" >&2
    exit 1
  }

  local -a targets=()
  local file abs rel
  for file in "$@"; do
    [ -f "$file" ] || { echo "FAIL: no such file: $file" >&2; exit 1; }
    case "$file" in
      *.rs) ;;
      *) echo "FAIL: not a Rust file: $file" >&2; exit 1 ;;
    esac
    abs="$(cd "$(dirname "$file")" && pwd)/$(basename "$file")"
    rel="${abs#"$repo_root"/}"
    [ "$rel" != "$abs" ] || { echo "FAIL: $file is outside $repo_root" >&2; exit 1; }
    targets+=("$rel")
  done

  # Deliberately not `local`: the EXIT trap runs after this function has returned, so a local
  # would be unset by the time the trap expands it.
  snapshot="$(mktemp -d)"
  trap 'rm -rf "$snapshot"' EXIT

  all_rust_files "$repo_root" | tar --null -C "$repo_root" -cf "$snapshot/before.tar" -T - 2>/dev/null || {
    echo "FAIL: could not snapshot the tree; refusing to format without one" >&2
    exit 1
  }
  local before after
  before="$(hash_all_rust_files "$repo_root")"

  local status=0
  local -a rustfmt_args=()
  [ "$FMT_FILE_UNSAFE_NO_SKIP" = "1" ] || rustfmt_args+=(--config skip_children=true)
  [ "$check_only" = "1" ] && rustfmt_args+=(--check)

  for rel in "${targets[@]}"; do
    local edition
    edition="$(edition_for "$repo_root/$rel")"
    if ! (cd "$repo_root" && rustfmt --edition "$edition" "${rustfmt_args[@]}" "$rel"); then
      status=1
    fi
  done

  after="$(hash_all_rust_files "$repo_root")"

  # Everything that moved, minus what we were asked to move. Anything left is collateral.
  local -a collateral=()
  local line changed
  while IFS= read -r line; do
    changed="${line%$'\t'*}"
    changed="${changed#./}"
    local is_target=0
    for rel in "${targets[@]}"; do
      [ "$changed" = "$rel" ] && is_target=1
    done
    [ "$is_target" = "1" ] || collateral+=("$changed")
  done < <(comm -13 <(echo "$before") <(echo "$after") 2>/dev/null)

  if [ ${#collateral[@]} -gt 0 ]; then
    (cd "$repo_root" && tar -xf "$snapshot/before.tar" -- "${collateral[@]}") \
      && echo "restored ${#collateral[@]} file(s) from the pre-run snapshot" >&2
    echo "FAIL: rustfmt reached ${#collateral[@]} file(s) outside the ones named:" >&2
    printf '  %s\n' "${collateral[@]}" >&2
    echo "This means rustfmt descended into module declarations despite skip_children — the" >&2
    echo "toolchain or a rustfmt.toml in the tree has changed the behaviour this script relies" >&2
    echo "on. The files above have been restored. Do not format by hand until it is understood." >&2
    exit 1
  fi

  if [ "$status" -ne 0 ]; then
    [ "$check_only" = "1" ] && exit 1
    echo "FAIL: rustfmt reported an error on at least one file" >&2
    exit 1
  fi

  if [ "$check_only" = "1" ]; then
    echo "ok: ${#targets[@]} file(s) already formatted"
  else
    echo "ok: formatted ${#targets[@]} file(s); no other Rust file in the tree moved"
  fi
}

# ---------------------------------------------------------------------------- --selftest

run_selftest() {
  # Not `local`, for the same reason as `snapshot` above: the EXIT trap outlives the function.
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  local failures=0 checked=0
  expect() { # expect <description> <condition-result:0|1>
    checked=$((checked + 1))
    if [ "$2" = "0" ]; then
      echo "  ok   $1"
    else
      echo "  FAIL $1" >&2
      failures=$((failures + 1))
    fi
  }

  plant() { # plant <dir>
    rm -rf "$1"; mkdir -p "$1/src"
    git -C "$1" init -q
    cat > "$1/Cargo.toml" <<'EOF'
[package]
name = "selftest"
version = "0.0.0"
edition = "2021"
EOF
    printf 'pub mod child;\npub fn  parent( )->u32{1}\n' > "$1/src/lib.rs"
    printf 'pub fn  child_fn( )->u32{2}\n' > "$1/src/child.rs"
    git -C "$1" add -A >/dev/null 2>&1
  }

  # 1. The promise: formatting the parent leaves the child exactly as it was.
  local a="$tmp/a"
  plant "$a"
  local child_before
  child_before="$(cat "$a/src/child.rs")"
  (cd "$a" && bash "$SCRIPT" src/lib.rs) >/dev/null 2>&1
  expect "formatting a module-declaring parent leaves its children untouched" \
    "$([ "$(cat "$a/src/child.rs")" = "$child_before" ] && echo 0 || echo 1)"
  expect "the named file is actually formatted" \
    "$(grep -q 'pub fn parent() -> u32' "$a/src/lib.rs" && echo 0 || echo 1)"

  # 2. The guard: with the skip disabled, rustfmt does recurse — and this script must refuse and
  #    put the child back. Without this case the snapshot comparison is a claim, not a mechanism.
  local b="$tmp/b"
  plant "$b"
  child_before="$(cat "$b/src/child.rs")"
  local out rc
  out="$(cd "$b" && FMT_FILE_UNSAFE_NO_SKIP=1 bash "$SCRIPT" src/lib.rs 2>&1)"; rc=$?
  expect "an unguarded run is refused, not reported as success" \
    "$([ "$rc" -ne 0 ] && echo 0 || echo 1)"
  expect "the refusal names the file that was reached" \
    "$(echo "$out" | grep -q 'src/child.rs' && echo 0 || echo 1)"
  expect "the collaterally-formatted file is restored to its pre-run content" \
    "$([ "$(cat "$b/src/child.rs")" = "$child_before" ] && echo 0 || echo 1)"

  # 3. --check reports without writing.
  local c="$tmp/c"
  plant "$c"
  local parent_before
  parent_before="$(cat "$c/src/lib.rs")"
  (cd "$c" && bash "$SCRIPT" --check src/lib.rs) >/dev/null 2>&1
  expect "--check leaves the named file unmodified" \
    "$([ "$(cat "$c/src/lib.rs")" = "$parent_before" ] && echo 0 || echo 1)"

  if [ "$failures" -gt 0 ]; then
    echo "FAIL: $failures of $checked selftest case(s) failed" >&2
    exit 1
  fi
  echo "ok: $checked selftest case(s) — the skip holds, and the snapshot guard catches it when it does not"
}

# ---------------------------------------------------------------------------- dispatch

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

command -v rustfmt >/dev/null 2>&1 || { echo "FAIL: rustfmt not on PATH" >&2; exit 1; }

[ $# -ge 1 ] || usage

case "$1" in
  --selftest) [ $# -eq 1 ] || usage; run_selftest; exit 0 ;;
  --check) shift; [ $# -ge 1 ] || usage; run_format 1 "$@" ;;
  --*) usage ;;
  *) run_format 0 "$@" ;;
esac
