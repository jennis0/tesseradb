//! The dictionary is generation-scoped (§3.2).
//!
//! A flush promotes each novel descriptor to a durable dictionary ordinal and publishes the
//! assignment as a `dict_extents` entry. That is only expressible if the dictionary can be
//! republished with a generation, which it could not while it was bound at `Engine::open`.

mod common;

use std::sync::Arc;

use common::*;
use tessera_authz::{Dict, DictWriter};
use tessera_types::TermId;
use tessera_engine::GeometryPublication;

/// Ordinals are stable across an extension: a term that resolved to 3 before still does, or
/// every session authorised before the flush is now evaluating against a different term.
///
/// This is §3.4's premise 3 in its structural form — the equality of a patch and a rebuild
/// rests on a session's `satisfied` naming the same terms after a flush as before it.
#[test]
fn extending_a_dict_preserves_every_existing_ordinal() {
    let dir = tempfile::TempDir::new().unwrap();
    let dict = dict_in(&dir.path().join("base"), &[b"a", b"b", b"c"]);
    let extent = extent_in(&dir.path().join("ext"), &[b"d", b"e"]);

    let extended = dict.load_extending(&extent).unwrap();

    for (i, d) in [b"a".as_slice(), b"b", b"c"].iter().enumerate() {
        assert_eq!(extended.lookup(d), Some(TermId::new(i as u32)), "{d:?}");
    }
    assert_eq!(extended.lookup(b"d"), Some(TermId::new(3)));
    assert_eq!(extended.lookup(b"e"), Some(TermId::new(4)));
    assert_eq!(extended.len(), 5);
}

/// The write path resolves against the generation's dictionary too, so a descriptor a flush has
/// promoted stops being minted a fresh extension id on every later arrival.
///
/// Extension ids count down from `u32::MAX` and are unsatisfiable by construction; a promoted
/// ordinal is an ordinary dictionary term. The difference between them is the whole of what
/// promotion buys, and this is where the write path is observed to see it.
#[test]
fn the_write_path_resolves_against_the_generations_dict() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_publishing(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );

    let novel = vec![b"novel".to_vec()];
    let before = engine.resolve_terms(&novel);
    assert!(
        before[0].raw() > 200_000_000,
        "an unpromoted descriptor gets an unsatisfiable extension id, not an ordinal"
    );

    let live = engine.generation();
    let promoted_ordinal = live.dict.len();
    let extended = Arc::new(
        live.dict
            .load_extending(&extent_in(&tmp.path().join("promoted"), &[b"novel"]))
            .unwrap(),
    );
    engine
        .publish_geometry(GeometryPublication::within_prefix(
            live.prefix.clone(),
            live.segments_version + 1,
            live.watermark,
            Arc::clone(&live.bundle),
            extended,
            Vec::new(),
        ))
        .unwrap();

    assert_eq!(
        engine.resolve_terms(&novel),
        vec![TermId::new(promoted_ordinal)],
        "after the promotion the write path resolves the durable ordinal"
    );
}

/// Authorise reads the generation's dictionary, not a process-lifetime one — so a descriptor
/// promoted by a publication is satisfiable by the sessions authorised after it.
///
/// A promoted ordinal sits at or above the base postings' term count, so this only works because
/// a term no file carries reads as an empty posting rather than an error (§5.2) — the property
/// that makes a sparse delta tier possible at all.
#[test]
fn authorise_resolves_against_the_generations_dict() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    let engine = open_engine_publishing(
        &root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );

    // "0" is in the fixture's dictionary; "novel" is not, so it drops out of `satisfied`.
    let credential = br#"{"terms": ["0", "novel"]}"#.to_vec();
    let before = engine.authorise(&credential).unwrap();
    assert_eq!(
        before.satisfied.len(),
        1,
        "an unknown descriptor is simply unsatisfied, never an error"
    );

    let live = engine.generation();
    let extended = Arc::new(
        live.dict
            .load_extending(&extent_in(&tmp.path().join("promoted"), &[b"novel"]))
            .unwrap(),
    );
    engine
        .publish_geometry(GeometryPublication::within_prefix(
            live.prefix.clone(),
            live.segments_version + 1,
            live.watermark,
            Arc::clone(&live.bundle),
            extended,
            Vec::new(),
        ))
        .unwrap();

    let after = engine.authorise(&credential).unwrap();
    assert_eq!(
        after.satisfied.len(),
        2,
        "a promoted descriptor is satisfiable by a session authorised after the publication"
    );
    assert_eq!(
        before.satisfied.len(),
        1,
        "and the session authorised before it is untouched — `satisfied` is never re-resolved, \
         which is what §3.4's patch-equals-a-rebuild rests on"
    );
}

fn dict_in(dir: &std::path::Path, descriptors: &[&[u8]]) -> Dict {
    Dict::load(&extent_in(dir, descriptors)).unwrap()
}

fn extent_in(dir: &std::path::Path, descriptors: &[&[u8]]) -> Vec<std::path::PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    let mut writer = DictWriter::new(dir);
    for d in descriptors {
        writer.intern(d);
    }
    writer.finish().unwrap()
}
