//! Bundle discovery and the resume ledger.
//!
//! Fixtures are built by `scripts/bench_build_fixtures.sh` (a scale is a `--limit` prefix filter,
//! never a separate corpus file — `probes/dataset.md` §5 rule 1). This module finds them, and
//! records which cells have already been measured so a killed run resumes instead of restarting.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default fixture root, matching `scripts/bench_build_fixtures.sh`.
pub const DEFAULT_FIXTURES: &str = "/tmp/tessera-bench/fixtures";

/// `CURRENT`'s on-disk shape. Mirrors `tessera_store`'s private `CurrentPointer`; only `prefix`
/// is needed here, since `open_bundle` does the digest verification for anything that opens the
/// bundle properly.
#[derive(Debug, Deserialize)]
struct CurrentPointer {
    prefix: String,
}

/// One built bundle.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub scale: u64,
    pub label_set: String,
    pub root: PathBuf,
    /// The `CURRENT` prefix, e.g. `v00000`.
    pub prefix: String,
    pub bytes: u64,
}

impl Fixture {
    pub fn postings_path(&self) -> PathBuf {
        self.root
            .join(&self.prefix)
            .join("partitions/default/terms/postings.arrow")
    }
}

/// Every bundle under `root`, in (scale, label set) order.
///
/// Layout is `<root>/<scale>/<label-set>/`, with `CURRENT` naming the active prefix. A directory
/// without `CURRENT` is a partial build and is skipped rather than half-read.
pub fn discover(root: &Path) -> std::io::Result<Vec<Fixture>> {
    let mut out = Vec::new();
    let Ok(scales) = std::fs::read_dir(root) else {
        return Ok(out);
    };
    for scale_entry in scales.flatten() {
        let Ok(scale) = scale_entry.file_name().to_string_lossy().parse::<u64>() else {
            continue;
        };
        let Ok(sets) = std::fs::read_dir(scale_entry.path()) else {
            continue;
        };
        for set_entry in sets.flatten() {
            let bundle_root = set_entry.path();
            let current = bundle_root.join("CURRENT");
            if !current.exists() {
                continue;
            }
            // `CURRENT` is JSON (`{"prefix": ..., "manifest_digest": ...}`), matching
            // `tessera_store::read::open_bundle`'s `CurrentPointer`. Parsing it as a bare string
            // yields `{` as the prefix and every subsequent path silently misses.
            let Ok(pointer) = serde_json::from_slice::<CurrentPointer>(&std::fs::read(&current)?)
            else {
                continue;
            };
            let prefix = pointer.prefix;
            if prefix.is_empty() {
                continue;
            }
            out.push(Fixture {
                scale,
                label_set: set_entry.file_name().to_string_lossy().into_owned(),
                bytes: crate::metrics::dir_bytes(&bundle_root),
                root: bundle_root,
                prefix,
            });
        }
    }
    out.sort_by(|a, b| a.scale.cmp(&b.scale).then(a.label_set.cmp(&b.label_set)));
    Ok(out)
}

/// A completed cell, appended to `ledger.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub cell_id: String,
    pub status: String,
}

/// Append-only record of what has already been measured.
///
/// **Write the result first, then the ledger line.** A cell with a result but no ledger line is
/// simply re-run; a ledger line with no result would be a silently missing measurement that the
/// resume logic would never notice. Cheap correctness over clever recovery.
pub struct Ledger {
    path: PathBuf,
    done: HashSet<String>,
}

impl Ledger {
    pub fn open(run_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(run_dir)?;
        let path = run_dir.join("ledger.jsonl");
        let mut done = HashSet::new();
        if path.exists() {
            for line in BufReader::new(std::fs::File::open(&path)?).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(entry) = serde_json::from_str::<LedgerEntry>(&line) {
                    if entry.status == "ok" {
                        done.insert(entry.cell_id);
                    }
                }
            }
        }
        Ok(Ledger { path, done })
    }

    pub fn is_done(&self, cell_id: &str) -> bool {
        self.done.contains(cell_id)
    }

    pub fn mark(&mut self, cell_id: &str, status: &str) -> std::io::Result<()> {
        let entry = LedgerEntry {
            cell_id: cell_id.to_string(),
            status: status.to_string(),
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(&entry).unwrap())?;
        file.flush()?;
        if status == "ok" {
            self.done.insert(entry.cell_id);
        }
        Ok(())
    }

    /// How many cells a resumed run will skip — the matrix orchestrator's progress line.
    #[allow(dead_code)]
    pub fn completed(&self) -> usize {
        self.done.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn discover_parses_currents_json_shape_not_its_first_line() {
        // Regression: `CURRENT` is JSON, and reading its first line yields `{`, which makes every
        // derived path miss with a bare ENOENT and no clue why.
        let tmp = tempdir();
        fs::create_dir_all(tmp.join("250000/good")).unwrap();
        fs::write(
            tmp.join("250000/good/CURRENT"),
            r#"{"prefix": "v00000", "manifest_digest": "abc"}"#,
        )
        .unwrap();
        let found = discover(&tmp).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].prefix, "v00000");
        assert!(found[0]
            .postings_path()
            .ends_with("v00000/partitions/default/terms/postings.arrow"));
    }

    #[test]
    fn discover_skips_a_partial_build() {
        let tmp = tempdir();
        fs::create_dir_all(tmp.join("250000/good")).unwrap();
        fs::write(
            tmp.join("250000/good/CURRENT"),
            r#"{"prefix": "v00000", "manifest_digest": "abc"}"#,
        )
        .unwrap();
        // No CURRENT: a build that died partway. Reading it would measure a bundle that does not
        // exist as a coherent whole.
        fs::create_dir_all(tmp.join("250000/partial")).unwrap();

        let found = discover(&tmp).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label_set, "good");
        assert_eq!(found[0].prefix, "v00000");
        assert_eq!(found[0].scale, 250_000);
    }

    #[test]
    fn ledger_round_trips_and_only_ok_counts_as_done() {
        let tmp = tempdir();
        {
            let mut ledger = Ledger::open(&tmp).unwrap();
            ledger.mark("a/1", "ok").unwrap();
            ledger.mark("b/2", "failed").unwrap();
            assert!(ledger.is_done("a/1"));
            assert!(!ledger.is_done("b/2"));
        }
        // Reopened: a failed cell must be retried, not skipped.
        let reopened = Ledger::open(&tmp).unwrap();
        assert!(reopened.is_done("a/1"));
        assert!(!reopened.is_done("b/2"));
        assert_eq!(reopened.completed(), 1);
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "tessera-bench-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
