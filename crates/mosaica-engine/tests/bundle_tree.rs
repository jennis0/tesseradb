//! **Where a bundle's files are.** The root holds `CURRENT`, the prefixes and the operator's
//! `reports/`. A prefix holds `MANIFEST.json`, the term dictionary under `dictionary/`, and each
//! partition's directory, which holds its side-manifests and its shards; every other file is under
//! a shard's directory. Written against the paths themselves rather than the layout that derives
//! them.

mod common;
#[path = "homes/fixture.rs"]
mod fixture;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use common::*;
use fixture::*;
use mosaica_engine::{Engine, EngineConfig, IngestRequest};
use mosaica_lifecycle::wal::WalScalar;
use mosaica_lifecycle::IngestRow;
use mosaica_types::EntityId;

/// Whether a prefix-relative path is where a file of a prefix may be.
fn in_place(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        ["MANIFEST.json"] | ["dictionary", _, ..] | ["partitions", _, "shards", "0", _, ..] => true,
        ["partitions", _, name] => name
            .strip_prefix("SEGMENTS-")
            .and_then(|n| n.strip_suffix(".json"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        _ => false,
    }
}

/// Every file under `dir`, relative to `base`.
fn files_under(base: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(base, &path, out);
        } else {
            out.push(path.strip_prefix(base).unwrap().to_string_lossy().into_owned());
        }
    }
}

/// Every key of a `files` map in `value`, and every string naming something under `prefix_dir`.
fn named(value: &serde_json::Value, prefix_dir: &Path, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(s) if !s.is_empty() && prefix_dir.join(s).exists() => {
            out.insert(s.clone());
        }
        serde_json::Value::Array(items) => {
            for item in items {
                named(item, prefix_dir, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if let ("files", serde_json::Value::Object(files)) = (key.as_str(), item) {
                    out.extend(files.keys().cloned());
                }
                named(item, prefix_dir, out);
            }
        }
        _ => {}
    }
}

fn assert_tree(root: &Path, after: &str) {
    let mut misplaced = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_prefix = entry.path().is_dir()
            && name
                .strip_prefix('v')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !is_prefix {
            if name != "CURRENT" && name != "reports" {
                misplaced.push(name);
            }
            continue;
        }
        let mut files = Vec::new();
        files_under(&entry.path(), &entry.path(), &mut files);
        misplaced.extend(
            files
                .into_iter()
                .filter(|rel| !in_place(rel))
                .map(|rel| format!("{name}/{rel}")),
        );
    }
    assert!(
        misplaced.is_empty(),
        "out of place after {after}: {misplaced:#?}"
    );

    let prefix_dir = root.join(current_prefix(root));
    let mut manifests = vec![prefix_dir.join("MANIFEST.json")];
    for partition in std::fs::read_dir(prefix_dir.join("partitions")).unwrap() {
        for entry in std::fs::read_dir(partition.unwrap().path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                manifests.push(path);
            }
        }
    }
    let mut paths = BTreeSet::new();
    for manifest in &manifests {
        let json = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        named(&json, &prefix_dir, &mut paths);
    }
    let wrong: Vec<&String> = paths.iter().filter(|rel| !in_place(rel)).collect();
    assert!(
        wrong.is_empty(),
        "named out of place after {after}: {wrong:#?}"
    );
}

fn engine_at(tmp: &Path, root: &Path) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            coalesce_width: Some(2),
            max_merged_segment_bytes: None,
            compaction: mosaica_engine::CompactionSchedule::off(),
            ..config_uncapped()
        },
    )
    .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine.set_merge_for_test(true);
    engine
}

fn send(engine: &Engine, batch: &str, row: IngestRow) {
    let mut body_hash = [0u8; 32];
    body_hash[..batch.len()].copy_from_slice(batch.as_bytes());
    engine
        .ingest(IngestRequest {
            batch_id: batch.to_string(),
            body_hash,
            view: Some("s0".to_string()),
            rows: vec![row],
            artifacts: Default::default(),
            strict: false,
            mosaica_id_column: false,
        })
        .unwrap_or_else(|e| panic!("{batch} is accepted: {e}"));
}

/// A row with every value omitted.
fn bare(mosaica_id: Option<mosaica_types::MosaicaId>) -> IngestRow {
    IngestRow {
        mosaica_id,
        labels: None,
        position: None,
        scalars: vec![WalScalar::Null; DECLARED],
        scoped: vec![WalScalar::Null; 2],
        omitted: (0..DECLARED + 2).collect(),
    }
}

/// Through a build, a layer's publication, flushes carrying new index keys and an edit, a merge, a
/// coalesce, a fold and a restart, every file is in its place and every path a manifest names is.
#[test]
fn every_file_is_global_or_its_partitions_or_in_its_shard() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    assert_tree(&root, "the build");

    let engine = engine_at(tmp.path(), &root);
    publish(&engine, &root);
    let x = EntityId::new(source_to_new_map(&root, "v00000")[&X]);
    let mut edit = bare(Some(engine.mosaica_id_of(x).unwrap()));
    edit.scalars[SCORE_AT] = WalScalar::I32(555);
    edit.omitted.retain(|at| *at != SCORE_AT);
    send(&engine, "edit", edit);
    publish_buffered(&engine);
    let stats = engine.write_executor_stats();
    for i in 0..4 {
        let mut row = bare(None);
        row.labels = Some(vec![format!("novel-{i}").into_bytes()]);
        row.position = Some((10.0 + f64::from(i), 20.0));
        send(&engine, &format!("new-{i}"), row);
        publish_buffered(&engine);
    }
    tick_until(
        &engine,
        "a merge and a coalesce",
        Duration::from_secs(60),
        || {
            let now = engine.write_executor_stats();
            now.merges > stats.merges && now.coalesces > stats.coalesces
        },
    );
    assert_tree(
        &root,
        "a layer's publication, flushes, a merge and a coalesce",
    );

    fold(&engine);
    assert_tree(&root, "a fold");
    drop(engine);
    let engine = engine_at(tmp.path(), &root);
    assert_tree(&root, "a restart");
    drop(engine);
}

/// Each view's base, row-to-entity table and field tallies, opened. The reader opens a view
/// without any of the three, so one written where the reader does not look goes unread.
fn assert_bases(root: &Path, after: &str) {
    let bundle = mosaica_store::open_bundle(root).expect("the bundle opens");
    for view in VIEWS {
        let data = &bundle.partitions[PARTITION].only_shard().views[view];
        assert!(
            data.row_space.base_rows() > 0,
            "{view} has no base after {after}"
        );
        assert!(
            data.row_space.row_entity().is_some(),
            "{view} has no row-to-entity table after {after}"
        );
        assert!(
            data.field_tallies.is_some(),
            "{view} has no field tallies after {after}"
        );
    }
}

#[test]
fn a_built_or_folded_view_opens_with_its_base_row_entities_and_field_tallies() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    assert_bases(&root, "the build");
    let engine = engine_at(tmp.path(), &root);
    let mut row = bare(None);
    row.labels = Some(vec![b"0".to_vec()]);
    row.position = Some((10.0, 20.0));
    send(&engine, "new", row);
    publish_buffered(&engine);
    fold(&engine);
    drop(engine);
    assert_bases(&root, "a fold");
}

/// The bundle's shard size, and each shard's number and high water, as an open reads them.
fn shards_of(root: &Path) -> (u64, Vec<(u32, u64)>) {
    let bundle = mosaica_store::open_bundle(root).expect("the bundle opens");
    let partition = &bundle.partitions[PARTITION];
    let shards = partition
        .shards
        .iter()
        .map(|shard| (shard.id.raw(), shard.record.high_water))
        .collect();
    (partition.shard_size, shards)
}

/// **The shard's record and the shard size survive a flush, a fold and a restart.** Every
/// publication restates the size, and the shard's high water only rises.
#[test]
fn the_shard_record_and_the_shard_size_survive_a_flush_a_fold_and_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = build_homes(tmp.path());
    let (size, built) = shards_of(&root);
    assert_eq!(size, mosaica_types::SHARD_SIZE_DEFAULT);
    assert_eq!(built.len(), 1);
    assert_eq!(built[0].0, 0);
    let new_row = |i: u32| {
        let mut row = bare(None);
        row.labels = Some(vec![b"0".to_vec()]);
        row.position = Some((10.0 + f64::from(i), 20.0));
        row
    };

    let engine = engine_at(tmp.path(), &root);
    send(&engine, "new-0", new_row(0));
    publish_buffered(&engine);
    let flushed = shards_of(&root);
    assert_eq!(flushed.0, mosaica_types::SHARD_SIZE_DEFAULT, "after a flush");
    assert_eq!(flushed.1, vec![(0, built[0].1 + 1)], "after a flush");

    fold(&engine);
    assert_eq!(shards_of(&root), flushed, "after a fold");
    drop(engine);

    let engine = engine_at(tmp.path(), &root);
    assert_eq!(shards_of(&root), flushed, "after a restart");
    send(&engine, "new-1", new_row(1));
    publish_buffered(&engine);
    assert_eq!(
        shards_of(&root),
        (mosaica_types::SHARD_SIZE_DEFAULT, vec![(0, built[0].1 + 2)]),
        "after a flush that follows the restart"
    );
    drop(engine);
}
