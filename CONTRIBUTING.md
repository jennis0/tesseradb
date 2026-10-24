# Contributing to Mosaica

Thank you for considering a contribution. This page says how to build Mosaica, how to run the
checks a change must pass, and how to propose the change.

## What you need

- Rust stable (`rust-toolchain.toml` names the channel).
- Node 22.15 or later, for the TypeScript packages and the Python wheel's build hook.
- Python 3.10 or later, with `uv` or `venv`.
- Tens of gigabytes of free disc for a full test build. A build without debug info is much
  smaller: set `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

## Building

```bash
cargo build --release -p mosaica    # the binary, at target/release/mosaica
npm --prefix clients/ts ci          # the TypeScript workspace
```

`clients/py/check.sh` makes its own virtualenv and builds the Python packages, including the
`mosaica-native` wheel, which runs cargo.

## The checks

A change must pass all of these before it is merged:

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-test-reachability.sh --quick
bash scripts/check-clients.sh
bash clients/py/check.sh
```

Read the totals from `cargo test`: with `--no-fail-fast` every test binary runs, so a failure in an
early binary does not hide the count of the rest. `check-clients.sh` needs Playwright's Chromium
(`cd clients/ts && npx playwright install chromium`). The continuous integration workflow
(`.github/workflows/ci.yml`) runs the same checks, the conformance suite and `cargo deny`.

`bash scripts/install-hooks.sh` installs a pre-commit hook that refuses a commit in which `rustfmt`
reformatted files the change did not touch.

## How the code is organised

[CLAUDE.md](CLAUDE.md) describes the layout, the security boundary and the conventions for code,
comments and tests. [docs/system/overview.md](docs/system/overview.md) describes the system and
[docs/writing.md](docs/writing.md) is the style for prose, in British spelling. A change that
alters what a viewer can see must keep every claim in
[docs/system/security.md](docs/system/security.md).

## Proposing a change

1. Open an issue first for anything larger than a small fix, so the approach can be agreed before
   the work.
2. Make the change on a branch, with tests through the public surface: what is stored, what is
   served, and what survives a restart.
3. A capability added to one surface (the HTTP API, the TypeScript client, the Python client or the
   command line) is added to all four, starting with the HTTP API.
4. Open a pull request that says what changed and why, and which checks you ran.

Report a suspected vulnerability privately, as [SECURITY.md](SECURITY.md) describes, and not in an
issue.

Contributions are licensed under the Apache License 2.0, the licence of the project.
