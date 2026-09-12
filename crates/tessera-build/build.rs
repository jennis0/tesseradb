//! Stamps the commit this binary was built from into `TESSERA_BUILD_COMMIT`.
//!
//! A build's log and `tessera --version` both print it. The campaign of 2026-09-12 spent two
//! hours and three-quarters on a binary seven commits behind the tree it was read against,
//! because nothing the run emitted said which source it came from.
//!
//! "unknown" where `git` is absent or the source is not a checkout — a tarball build is a build,
//! and refusing one to stamp a provenance string would be the wrong trade.

use std::path::Path;
use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=TESSERA_BUILD_COMMIT={commit}");
    // A commit change moves `HEAD` and the ref it names, so both are watched — and **where git
    // says they are**, not at `../../.git`. A worktree's `.git` is a *file* naming the real
    // directory, so the guessed paths do not exist there and the stamp went stale on every commit
    // that did not touch this crate, which is the failure the stamp exists to catch.
    for path in ["HEAD", "refs"] {
        let resolved = Command::new("git")
            .args(["rev-parse", "--git-path", path])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(resolved) = resolved else { continue };
        if Path::new(&resolved).exists() {
            println!("cargo:rerun-if-changed={resolved}");
        }
    }
}
