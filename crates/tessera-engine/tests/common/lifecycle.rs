//! The log and the published prefix, as a test reaches them from outside the engine: which
//! members the write-ahead log has on disk, removing the lot, and what `CURRENT` names.

#![allow(dead_code)]

use std::path::Path;

/// The log's members on disk, sorted — `wal.log`'s siblings, `<stem>-NNNNNN.log`, and not the
/// path itself.
pub fn wal_members(wal: &Path) -> Vec<String> {
    let dir = wal.parent().expect("the log has a directory");
    let stem = wal
        .file_stem()
        .expect("the log has a stem")
        .to_string_lossy()
        .to_string();
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .expect("the log's directory exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(&format!("{stem}-")) && n.ends_with(".log"))
        .collect();
    found.sort();
    found
}

/// Delete every member of the log, failing if there was none — a case that means to reopen
/// without the log proves nothing when there was nothing to remove.
pub fn remove_the_whole_log(wal: &Path) {
    let dir = wal.parent().expect("the log has a directory");
    let stem = wal.file_stem().expect("the log has a stem").to_owned();
    let mut removed = 0usize;
    for entry in std::fs::read_dir(dir)
        .expect("the log's directory exists")
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!("{}-", stem.to_string_lossy())) {
            std::fs::remove_file(entry.path()).expect("a log member is removable");
            removed += 1;
        }
    }
    assert!(
        removed > 0,
        "no log member was found to delete — the test would prove nothing"
    );
}

/// The prefix `CURRENT` names at the bundle root.
pub fn current_prefix(root: &Path) -> String {
    let current: tessera_store::manifest::CurrentPointer =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT is readable"))
            .expect("CURRENT parses");
    current.prefix
}
