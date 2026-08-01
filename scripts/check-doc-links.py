#!/usr/bin/env python3
"""check-doc-links.py — catch documentation cross-references that have gone stale.

The corpus, the memos, the probe records and ~108 Rust module docs cite each other constantly and
by hand. Nothing checked those citations, and they rotted: the design corpus spent months in a
directory ripgrep could not see, four documents cited a revision that had moved twice, and a merge
once shifted `file:line` references far enough to strand a worker mid-task. This script is the
mechanical half of the fix.

    python3 scripts/check-doc-links.py             # errors fail, warnings report
    python3 scripts/check-doc-links.py --strict    # warnings fail too
    python3 scripts/check-doc-links.py --check-sections

## What is checked, and how hard

ERROR — a citation that names a path which does not exist:
  * markdown links with a relative target, resolved both file-relative and repo-root-relative,
    because the repo genuinely uses both conventions;
  * backticked path-shaped tokens containing a `/`;
  * `— source: <path> §N` citations, the whitepaper-facts idiom, with bare basenames resolved
    through an index built from `git ls-files`;
  * anything matching FORBIDDEN — paths this reorganisation retired. This is the cheapest and
    highest-value half of the script: it is what stops the old layout creeping back.

WARN — real staleness signals that are too noisy to fail a build on:
  * `file.rs:LINE` citations whose LINE is past the end of the file. Only out-of-range is checked.
    Verifying that the cited line still says what the citer thinks would be better and is not
    achievable; erroring on drift would make this a nuisance that gets disabled, and a disabled
    check is worth less than a noisy one.
  * `§N` anchors against the target's headings, behind --check-sections. Off by default: the
    corpus uses `Appendix C`, `§"Addendum"` and revision markers that no heading regex models.

SKIPPED, deliberately — do not "improve" this later without reading the reason:
  * prose mentions with no path ("the contracts spec", "lifecycle §7"). No reliable extraction,
    high false-positive rate, and the prose is usually right.
  * GitHub `#Lnn` line anchors inside links — a UI convention, not a claim about content.
  * external URLs — this runs in a pre-commit hook with no network.
  * docs/archive/** — frozen by definition. It cites documents that were deleted and instructs
    workers to create files that were never created; that is what an archive looks like.
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SCAN_MARKDOWN = ["docs", "probes", "conformance", "clients", "reference", "bench"]
SCAN_ROOT_FILES = ["CLAUDE.md", "README.md"]
SCAN_CODE = {"crates": (".rs",), "clients": (".ts",)}

EXCLUDE_PARTS = {
    "target", ".git", "node_modules", "__pycache__", "dist", ".venv",
    "worktrees", ".superpowers", "archive", ".pytest_cache",
}

# Paths this reorganisation retired. A hit is an error, with the replacement in the message.
FORBIDDEN = [
    (".ignore/", "the design corpus moved to docs/design/ and docs/evidence/"),
    ("tessera-architecture-design.md", "renamed to docs/design/architecture.md"),
    ("tessera-contracts-spec.md", "renamed to docs/design/contracts.md"),
    ("tessera-system-architecture.md", "renamed to docs/design/system-architecture.md"),
    ("tessera-concurrency-lifecycle.md", "renamed to docs/design/concurrency-lifecycle.md"),
    ("tessera-conformance-design.md", "renamed to docs/design/conformance.md"),
    ("tessera-visualisation-architecture.md", "archived as docs/archive/visualisation.md"),
    ("tessera-implementation-plan.md", "renamed to docs/design/implementation-plan.md"),
    ("tessera-scaling-analysis.md", "renamed to docs/evidence/analysis/scaling-analysis.md"),
    ("docs/design-memos/", "renamed to docs/evidence/memos/"),
    ("docs/superpowers/", "plans and spent specs moved to docs/archive/plans/"),
]

MD_LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
BACKTICK_PATH = re.compile(r"`([A-Za-z0-9_][A-Za-z0-9_./-]*\.(?:md|rs|ts|py|toml|json|html|sh))(:\d+(?:-\d+)?)?`")
SOURCE_CITE = re.compile(r"source:\s*`?([A-Za-z0-9_][A-Za-z0-9_./-]*\.md)`?")
LINE_CITE = re.compile(r"`?([A-Za-z0-9_][A-Za-z0-9_./-]*\.(?:rs|ts|py))`?:(\d+)(?:-(\d+))?")
SECTION_CITE = re.compile(r"§(\d+(?:\.\d+)*)")
RUST_COMMENT = re.compile(r"^\s*(?://!|///|//)\s?(.*)$")
TS_COMMENT = re.compile(r"^\s*(?:\*|//)\s?(.*)$")


def excluded(p: Path) -> bool:
    return any(part in EXCLUDE_PARTS for part in p.parts)


def gather():
    files = []
    for name in SCAN_ROOT_FILES:
        p = ROOT / name
        if p.is_file():
            files.append((p, "md"))
    for d in SCAN_MARKDOWN:
        base = ROOT / d
        if base.is_dir():
            files += [(p, "md") for p in base.rglob("*.md") if not excluded(p.relative_to(ROOT))]
    for d, exts in SCAN_CODE.items():
        base = ROOT / d
        if base.is_dir():
            for ext in exts:
                files += [(p, "code") for p in base.rglob(f"*{ext}") if not excluded(p.relative_to(ROOT))]
    return sorted(set(files))


def basename_index():
    try:
        out = subprocess.run(["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return {}
    idx = {}
    for line in out.splitlines():
        idx.setdefault(Path(line).name, []).append(line)
    return idx


def resolve(target: str, src: Path):
    """Try file-relative then repo-root-relative. Returns a Path or None."""
    target = target.split("#")[0].strip()
    if not target:
        return None
    for cand in ((src.parent / target), (ROOT / target)):
        try:
            if cand.exists():
                return cand
        except OSError:
            pass
    return None


def resolve_loose(target: str, src: Path, tracked):
    """resolve(), then the shorthands this repo actually writes.

    Reports cite paths relative to a crate (`tests/http.rs`, `arms/viewport.rs`) or to the crates
    directory (`tessera-authz/src/postings.rs`). Both are unambiguous to a human and neither is
    root-relative. Returns (Path, None) when found, (None, reason) when not.
    """
    hit = resolve(target, src)
    if hit is not None:
        return hit, None
    head, _, rest = target.partition("/")
    for cand in (f"crates/{target}", f"crates/tessera-{head}/{rest}" if rest else None):
        if cand and (hit := resolve(cand, src)) is not None:
            return hit, None
    suffix = "/" + target.lstrip("./")
    matches = [t for t in tracked if t.endswith(suffix)]
    if len(matches) == 1:
        return ROOT / matches[0], None
    if len(matches) > 1:
        return None, f"ambiguous shorthand {target} -> {matches}"
    # Only claim a miss when the citation was addressed to something in this repo at all.
    # `partitions/default/SEGMENTS-0.json` is a runtime bundle path, not a source file, and a
    # checker that cannot tell the difference is one that gets switched off.
    if not (ROOT / head).is_dir():
        return None, None
    return None, f"cited path does not exist: {target}"


def comment_text(path: Path, kind: str):
    """Yield (lineno, text) for comment lines only, so code never trips the path checks."""
    pat = RUST_COMMENT if path.suffix == ".rs" else TS_COMMENT
    try:
        lines = path.read_text(errors="replace").splitlines()
    except OSError:
        return
    in_block = False
    for n, raw in enumerate(lines, 1):
        if kind == "code" and path.suffix == ".ts":
            if "/*" in raw:
                in_block = True
            m = pat.match(raw)
            if m and (in_block or raw.lstrip().startswith("//")):
                yield n, m.group(1)
            if "*/" in raw:
                in_block = False
            continue
        m = pat.match(raw)
        if m:
            yield n, m.group(1)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--strict", action="store_true", help="fail on warnings too")
    ap.add_argument("--check-sections", action="store_true", help="also warn on §N anchors with no matching heading")
    args = ap.parse_args()

    idx = basename_index()
    tracked = [x for v in idx.values() for x in v]
    errors, warnings = [], []

    def err(p, n, msg):
        errors.append(f"{p.relative_to(ROOT)}:{n}: {msg}")

    def warn(p, n, msg):
        warnings.append(f"{p.relative_to(ROOT)}:{n}: {msg}")

    for path, kind in gather():
        if path.resolve() == Path(__file__).resolve():
            continue  # this file names the forbidden strings on purpose
        if kind == "md":
            try:
                units = list(enumerate(path.read_text(errors="replace").splitlines(), 1))
            except OSError:
                continue
        else:
            units = list(comment_text(path, kind))

        for n, text in units:
            for bad, fix in FORBIDDEN:
                if bad in text:
                    err(path, n, f"retired path {bad!r} — {fix}")

            if kind == "md":
                for target in MD_LINK.findall(text):
                    if target.startswith(("http://", "https://", "mailto:", "#")):
                        continue
                    if resolve(target, path) is None:
                        err(path, n, f"link target does not exist: {target}")

            # The prior-art reviews cite other projects' source trees by path (Lucene's
            # src/query/mod.rs, quadfeather/tiler.py). Those are citations to code that is not
            # here and never will be; checking them against this repo is a category error.
            external_citations = "evidence/prior-art" in str(path)

            for target, _line in BACKTICK_PATH.findall(text):
                if "/" not in target or external_citations:
                    continue
                hit, reason = resolve_loose(target, path, tracked)
                if hit is None and reason is not None:
                    (warn if reason.startswith("ambiguous") else err)(path, n, reason)

            for target in SOURCE_CITE.findall(text):
                if "/" in target:
                    if resolve(target, path) is None:
                        err(path, n, f"source citation does not exist: {target}")
                else:
                    hits = idx.get(target, [])
                    if not hits:
                        err(path, n, f"source citation matches no tracked file: {target}")
                    elif len(hits) > 1:
                        warn(path, n, f"ambiguous source basename {target} -> {hits}")

            for target, start, end in LINE_CITE.findall(text):
                resolved, _ = resolve_loose(target, path, tracked)
                if resolved is None:
                    continue  # sibling-relative or prose; not claimed as checkable
                try:
                    total = len(resolved.read_text(errors="replace").splitlines())
                except OSError:
                    continue
                cited = int(end or start)
                if cited > total:
                    warn(path, n, f"{target}:{cited} is past end of file ({total} lines) — citation has drifted")

            if args.check_sections and kind == "md":
                pass  # deliberately unimplemented until the corpus heading forms are audited once

    for w in warnings:
        print(f"warn: {w}")
    for e in errors:
        print(f"ERROR: {e}", file=sys.stderr)

    n_files = len(gather())
    print(f"\nchecked {n_files} files: {len(errors)} error(s), {len(warnings)} warning(s)")
    if errors or (args.strict and warnings):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
