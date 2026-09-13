//! Side-manifest numbering (write-path §1.2): a publication's `n` clears every
//! `SEGMENTS-<n>.json` on disc, not only the ones the manifest it was built from names.
//!
//! The number is allocated from a counter on the executor thread, and a counter is above what
//! *that* executor has written. Which numbers are taken is a property of the filenames present, and
//! the two part company while a second writer holds the same bundle root — the state a restart
//! passes through, and the state an unpublished compaction prefix leaves behind. A publication at a
//! number already on disc is refused by the format boundary, its files are orphans, and the tick
//! re-plans: with both writers seeded from one disc state and advancing in lockstep, every re-plan
//! is refused in turn and nothing the node accepts is ever published.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use tessera_engine::{Engine, EngineConfig};

fn partition_dir(root: &Path, prefix: &str) -> PathBuf {
    root.join(prefix).join("partitions").join("default")
}

/// Every `n` a `SEGMENTS-<n>.json` in `dir` is named with, ascending.
fn side_manifest_numbers(dir: &Path) -> Vec<u64> {
    let mut found: Vec<u64> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_prefix("SEGMENTS-")
                .and_then(|rest| rest.strip_suffix(".json"))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .collect();
    found.sort_unstable();
    found
}

/// **A side-manifest a second writer left is never written through, and never blocks the node.**
///
/// The file planted here is what a second executor over this bundle root publishes while this one
/// is open: the number it took is on disc, and nothing this engine holds names it. A flush that
/// allocated from its own counter alone would take that number, be refused at the `hard_link`, and
/// discard a segment it had already written — and the next tick would do it again, because the
/// counter it re-planned from is the one that produced the refused number.
///
/// **Mutation:** drop the disc scan from `Executor::allocate_manifest_n` and the flush below never
/// publishes.
#[test]
fn a_flush_publishes_above_a_side_manifest_a_second_writer_left() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        // Far enough out that the publication observed below is the requested flush's doing.
        EngineConfig {
            flush_max_age_secs: 3600,
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(8)
        .expect("the executor starts once");

    let prefix = engine.generation().prefix.clone();
    let dir = partition_dir(&root, &prefix);
    let present = side_manifest_numbers(&dir);
    assert_eq!(present, vec![0], "the build published SEGMENTS-0.json");

    // The second writer's publication: a manifest at the number this executor would otherwise take
    // next. Its bytes are the build's, which is what a writer publishing complete current state
    // from the same base would have written.
    let planted = present.last().unwrap() + 1;
    std::fs::copy(
        dir.join("SEGMENTS-0.json"),
        dir.join(format!("SEGMENTS-{planted}.json")),
    )
    .unwrap();

    let row = tessera_lifecycle::UnallocatedRow {
        external_id: Some(b"above-the-planted-manifest".to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 0.5,
        y: 0.5,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], "planted-batch".to_string(), [3u8; 32])
        .expect("the row is accepted");
    engine.request_flush();

    let deadline = Instant::now() + Duration::from_secs(30);
    while engine.generation().segments_version == 0 {
        assert!(
            Instant::now() < deadline,
            "the flush never published: {} failure(s), side-manifests {:?}",
            engine.write_executor_stats().flush_failures,
            side_manifest_numbers(&dir)
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(
        engine.write_executor_stats().flush_failures,
        0,
        "the flush published without being refused once"
    );
    let after = side_manifest_numbers(&dir);
    assert_eq!(
        after,
        vec![0, planted, planted + 1],
        "the publication took the number above the one on disc"
    );
    assert_eq!(
        std::fs::read(dir.join(format!("SEGMENTS-{planted}.json"))).unwrap(),
        std::fs::read(dir.join("SEGMENTS-0.json")).unwrap(),
        "the planted manifest is untouched"
    );
}

/// **The seed clears every file, not every file a manifest names.**
///
/// The side-manifest planted here is in a partition directory `MANIFEST.json` does not name, which
/// is where the difference between the two rules shows: a seed taken from the partitions the
/// manifest carries never looks in it, and the number is free for the taking. The disc is what says
/// which numbers are used.
#[test]
fn an_executor_seeds_above_a_side_manifest_no_manifest_names() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let prefix = {
        let engine = open_engine(
            &root,
            &tmp.path().join("cache"),
            &tmp.path().join("wal-probe.log"),
        );
        engine.generation().prefix.clone()
    };
    let dir = partition_dir(&root, &prefix);
    let unnamed = root.join(&prefix).join("partitions").join("unnamed");
    std::fs::create_dir_all(&unnamed).unwrap();
    std::fs::copy(dir.join("SEGMENTS-0.json"), unnamed.join("SEGMENTS-9.json")).unwrap();

    let mut engine = Engine::open(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        EngineConfig {
            flush_max_age_secs: 3600,
            ..config()
        },
    )
    .expect("engine opens");
    engine
        .start_write_executor(8)
        .expect("the executor starts once");

    let row = tessera_lifecycle::UnallocatedRow {
        external_id: Some(b"after-the-seed".to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 0.5,
        y: 0.5,
        scalars: Vec::new(),
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    };
    engine
        .accept_ingest(vec![row], "after-the-seed-batch".to_string(), [5u8; 32])
        .expect("the row is accepted");
    engine.request_flush();

    let deadline = Instant::now() + Duration::from_secs(30);
    while side_manifest_numbers(&dir).len() < 2 {
        assert!(
            Instant::now() < deadline,
            "the flush never published: {} failure(s), side-manifests {:?}",
            engine.write_executor_stats().flush_failures,
            side_manifest_numbers(&dir)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        side_manifest_numbers(&dir),
        vec![0, 10],
        "the publication took the number above every file present, wherever it sits"
    );
}
