# Several agents at once

This repo routinely runs four or five agents in parallel against the same tree. That works
because file ownership is enforced rather than intended, and because one role holds the
invariant-bearing decisions instead of distributing them.

## Roles

**Controller.** Splits the work, writes the briefs, dispatches, reviews what comes back, and
rules on anything an implementer stops on. The controller does not implement — the moment it
does, it stops being able to review its own work honestly.

**Implementer.** Executes one brief inside one worktree, owning a disjoint set of files. Its
contract is: do the brief, verify it, report — and **stop and report** rather than reach outside
its allowlist, guess at an invariant question, or edit the rules that constrain it.

**Reviewer.** Has no stake in the work being right. For invariant-bearing changes, use several
reviewers with different lenses rather than several with the same lens.

Verify what an implementer reports rather than accepting the summary. The summary is what it
believes it did.

## Isolation

Each track gets a git worktree under `.claude/worktrees/<name>` on its own branch, plus a
one-line marker file naming its track:

```bash
git worktree add .claude/worktrees/phase2-track-c -b phase2/track-c <seam-sha>
echo c > .claude/worktrees/phase2-track-c/.claude/track
```

All tracks branch from a single **seam commit** so their diffs compose.

## File ownership, enforced

`.claude/track-allowlist.toml` declares who owns what. `scripts/check-track-allowlist.sh` diffs
the branch (or the index) against the merge base and fails naming every changed file the track
does not own. `scripts/install-hooks.sh` wires it into `pre-commit`. Run the installer once per
clone.

Three sections, and the order they are consulted in is the point:

- **`[frozen]`** is checked **first** and refuses for **every** track. Needing to edit a frozen
  file is usually the signal that a seam was drawn in the wrong place.
- **`[shared]`** permits for every track.
- **`[track.<t>]`** permits for one.

A frozen path listed under `[shared]` reads as a freeze and behaves as a universal permit. That
exact bug shipped once and was caught at a stage gate; the two sections exist separately because
of it.

The marker is a gitignored file, not a git config value, because hooks are shared across
worktrees — `git rev-parse --git-common-dir` is the same directory for all of them, so one hook
serves every branch and cannot otherwise know which track it is running for. A worktree with no
marker is not checked at all; that is deliberate, because the controller commits across track
boundaries by definition and a hook that refused would teach everyone to pass `--no-verify`.

**Editing the allowlist to make the check pass is the failure it exists to prevent.** It is a
controller decision, recorded with its reason, never a worker's convenience. `--audit` reports
overlaps, dead patterns and the unclaimed set; `--selftest` checks the matcher itself.

## The gate every task passes

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-track-allowlist.sh <track>
python3 scripts/check-doc-links.py
```

`check-layers.sh` is the mechanical half of the architecture: forbidden dependency edges, the
no-tokio-in-engine-or-store rule, I4 (no cross-space ID conversions), and I10 (no `EntityId` in
the wire payload). It is a grep-based approximation of spec rules — passing it is necessary and
nowhere near sufficient.

Evidence before assertion. Run the commands and read the output before saying anything passed.

## Stop-and-report

An implementer stops rather than proceeding when it hits any of these:

- a file outside its allowlist;
- a question whose answer would set an invariant, a guarantee, or the shape of something the
  owner has not decided;
- two rules that contradict each other across track boundaries;
- a finding that is real but out of scope — report it, do not fold it silently into a fix round.

A stop-and-report is a success. Every one of these in this repo's history turned out to be a
genuine seam problem, and the ones that were nearly guessed at instead were the expensive ones.

## Ledgers

Each track appends to its own `progress-<track>.md` in the gitignored SDD workspace: what
completed, what was deferred and why, what findings were raised, what the controller ruled.
These are working notes with a short life.

**They are not the record.** Anything in a ledger that will still matter next month must be
extracted before the campaign closes — decisions to `docs/decisions/`, measurements to
`docs/evidence/memos/`, leftovers to issues. See [`epic-lifecycle.md`](epic-lifecycle.md). This
repo once had fifteen owner decisions and controller rulings that existed nowhere but on one
laptop's untracked files.
