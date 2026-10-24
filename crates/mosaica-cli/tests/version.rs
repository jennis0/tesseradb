//! What `mosaica --version` prints, through the real binary: the release version, then the commit
//! the binary was built from in brackets where the build could name one.

use std::process::Command;

#[test]
fn version_prints_the_release_version_and_the_commit_it_was_built_from() {
    let out = Command::new(env!("CARGO_BIN_EXE_mosaica"))
        .arg("--version")
        .output()
        .expect("run mosaica --version");
    assert!(out.status.success());
    let printed = String::from_utf8(out.stdout).expect("utf-8");
    let printed = printed.trim_end();
    let release = format!("mosaica {}", env!("CARGO_PKG_VERSION"));
    let rest = printed
        .strip_prefix(&release)
        .unwrap_or_else(|| panic!("`{printed}` does not start with `{release}`"));
    if !rest.is_empty() {
        let commit = rest
            .strip_prefix(" (")
            .and_then(|r| r.strip_suffix(')'))
            .unwrap_or_else(|| panic!("`{printed}` has `{rest}` after the version"));
        assert!(
            (1..=9).contains(&commit.len()),
            "`{commit}` is not a short commit"
        );
    }
}
