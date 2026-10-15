//! `write_manifest_json` and `write_current` — the two bundle-artefact writers pass 5 of a fold
//! needs and `mosaica-build`'s `write_manifests` now calls instead of writing the bytes itself
//! (compaction §10's rule paragraph).
//!
//! These tests exercise the writers exactly as a caller must: build a minimal but real bundle's
//! segment/permutation/`SEGMENTS-0.json` with the tests' fixture, then write `MANIFEST.json` and
//! `CURRENT` through the two functions under test, then open the result with the real reader —
//! never a hand-parsed assertion of either file's shape, matching `bundle_read.rs`'s own voice.

use std::fs;
use std::path::Path;

use mosaica_store::layout::{partition_dir, partitions_dir};
use mosaica_store::manifest::{Manifest, SegmentsManifest};
use mosaica_store::manifest_write::{write_current, write_manifest_json};
use mosaica_store::{open_bundle, read_publication, write_publication, StoreError};
use mosaica_types::ShardId;

mod fixture;
use fixture::{hex_sha256, shard_files};

const SHARD0: mosaica_store::ShardLayout<'static> =
    mosaica_store::ShardLayout::new("default", ShardId::ZERO);

/// Build the one-partition, one-view, one-segment fixture `MANIFEST.json` needs to sit beside
/// for `open_bundle` to succeed — everything short of the two files this module's functions
/// write. Returns the assembled (but not yet written) [`Manifest`] and the prefix directory it
/// belongs under.
///
/// `created_at` is a parameter, not a constant, purely so
/// [`write_manifest_json_digest_changes_when_the_manifest_changes`] can build two otherwise
/// identical manifests that differ in exactly one field.
fn build_fixture(root: &Path, n: u64, created_at: &str) -> (Manifest, std::path::PathBuf) {
    let prefix_dir = root.join("v00000");
    fixture::publish_seg0(&prefix_dir, fixture::items_for(0, n));
    (fixture::manifest(created_at), prefix_dir)
}

/// `write_manifest_json` writes bytes the reader can open, and the digest it hands back is the
/// digest of exactly those bytes — not a value computed some other way. If the function hashed
/// something other than what it wrote (a stale in-memory copy, a second serialisation that
/// legally differs from the first, a partial write it failed to catch), `write_current` would
/// name a digest `open_bundle`'s check rejects and this test would see that rejection, not a
/// silent wrong answer.
#[test]
fn write_manifest_json_then_write_current_round_trips_through_open_bundle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest, prefix_dir) = build_fixture(dir.path(), 64, "2026-08-06T00:00:00Z");

    let digest = write_manifest_json(&prefix_dir, &manifest).expect("write_manifest_json");
    write_current(dir.path(), "v00000", &digest).expect("write_current");

    let bundle = open_bundle(dir.path()).expect("open_bundle over a bundle these writers built");
    assert_eq!(bundle.manifest.bundle_format, mosaica_types::BUNDLE_FORMAT);
    assert_eq!(bundle.only_shard().record.high_water, 64);

    let view = bundle.only_shard().views.get("main").expect("main view");
    assert_eq!(view.segments.len(), 1);
    assert_eq!(view.segments[0].row_count, 64);
}

/// The digest `write_manifest_json` returns is the SHA-256 of the bytes actually sitting on
/// disc afterwards — read back independently here, outside the function under test — and those
/// bytes are byte-for-byte `serde_json::to_vec_pretty` of the manifest. Pinning the exact
/// serialiser matters beyond this one test: `mosaica-build` wrote every bundle on disc today
/// with `to_vec_pretty`, and swapping in `to_vec` (same JSON value, different bytes, different
/// digest) would silently change every future bundle's identity against every bundle already
/// written — the trap the task brief calls out by name.
#[test]
fn write_manifest_json_returns_the_digest_of_the_exact_bytes_on_disc() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest, prefix_dir) = build_fixture(dir.path(), 8, "2026-08-06T00:00:00Z");

    let digest = write_manifest_json(&prefix_dir, &manifest).expect("write_manifest_json");

    let on_disc = fs::read(prefix_dir.join("MANIFEST.json")).expect("read MANIFEST.json");
    assert_eq!(
        digest,
        hex_sha256(&on_disc),
        "digest must match the bytes on disc"
    );

    let pretty = serde_json::to_vec_pretty(&manifest).expect("independent serialisation");
    assert_eq!(
        on_disc, pretty,
        "MANIFEST.json's bytes must be exactly to_vec_pretty of the manifest, matching \
         mosaica-build's serialiser"
    );
}

/// `CURRENT` as `write_current` writes it is exactly what `open_bundle`'s reader expects —
/// checked by feeding it a `MANIFEST.json` written the plain way (not through
/// `write_manifest_json`, to isolate `write_current` from its sibling) and going through the
/// real reader rather than re-parsing `CURRENT`'s bytes by hand. A wrong field name, a wrong
/// prefix join, or a rename that landed the file somewhere other than `<bundle_root>/CURRENT`
/// would all surface here as `open_bundle` failing to find or parse it.
#[test]
fn write_current_is_exactly_what_open_bundle_expects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest, prefix_dir) = build_fixture(dir.path(), 16, "2026-08-06T00:00:00Z");

    let bytes = serde_json::to_vec_pretty(&manifest).expect("serialise");
    fs::write(prefix_dir.join("MANIFEST.json"), &bytes).expect("write MANIFEST.json directly");
    let digest = hex_sha256(&bytes);

    write_current(dir.path(), "v00000", &digest).expect("write_current");

    let bundle = open_bundle(dir.path()).expect("open_bundle must accept write_current's CURRENT");
    assert_eq!(bundle.only_shard().record.high_water, 16);
}

/// A wrong `manifest_digest` in `CURRENT` must be refused, not silently accepted — the property
/// `write_current` exists to make impossible to misuse by construction (its only two arguments
/// are the prefix and the digest, so a caller cannot pass a mismatched pair by accident the way
/// hand-assembling the JSON would allow) and the property this test pins from the outside: even
/// handed a digest that does not match the bytes on disc, `write_current` writes exactly what it
/// was told, and `open_bundle` is what catches the mismatch.
#[test]
fn write_current_with_a_wrong_digest_is_caught_by_open_bundle_not_silently_served() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (manifest, prefix_dir) = build_fixture(dir.path(), 4, "2026-08-06T00:00:00Z");
    write_manifest_json(&prefix_dir, &manifest).expect("write_manifest_json");

    // A syntactically valid but wrong digest — 64 hex zeros, which is not this MANIFEST.json's
    // SHA-256.
    write_current(dir.path(), "v00000", &"0".repeat(64)).expect("write_current");

    match open_bundle(dir.path()) {
        Err(StoreError::ManifestDigestMismatch { .. }) => {}
        other => panic!("expected ManifestDigestMismatch, got {other:?}"),
    }
}

/// Two manifests that differ in one field digest differently, or a fold that changed the corpus
/// would leave every session's fragment cache believing nothing had rotated.
#[test]
fn write_manifest_json_digest_changes_when_the_manifest_changes() {
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    let (manifest_a, prefix_a) = build_fixture(dir_a.path(), 10, "2026-08-06T00:00:00Z");
    let (manifest_b, prefix_b) = build_fixture(dir_b.path(), 10, "2026-08-06T00:00:01Z");

    let digest_a = write_manifest_json(&prefix_a, &manifest_a).expect("write a");
    let digest_b = write_manifest_json(&prefix_b, &manifest_b).expect("write b");

    assert_ne!(
        digest_a, digest_b,
        "manifests differing only in created_at must digest differently"
    );

    // And the identity manifest, written twice, is stable: the digest is a pure function of
    // the manifest's content, not of when or how many times it is written.
    let digest_a_again = write_manifest_json(&prefix_a, &manifest_a).expect("write a again");
    assert_eq!(digest_a, digest_a_again);
}

/// **Obligation 11: a side-manifest is never replaced.**
///
/// A side-manifest is complete current state for its partition, not a diff, so a second write
/// at the same `n` does not merge with the first — before refuse-to-replace it *replaced* it,
/// with a manifest built from the same base and naming only its own segment, so the winner's
/// acked and published rows went missing from what a restart opens.
///
/// Asserted at the filesystem operation rather than through two views, because
/// `mosaica build` emits one and `dispatch_flushes` now sends one plan: the collision is a
/// property of the write, and this is the guard at the artefact that stands behind the rule at
/// the caller.
///
/// **Mutation:** restore `std::fs::rename` and the second write succeeds, silently.
#[test]
fn a_side_manifest_is_never_replaced() {
    let tmp = tempfile::TempDir::new().unwrap();
    let prefix_dir = tmp.path();
    std::fs::create_dir_all(partition_dir(prefix_dir, "default")).unwrap();

    let mut first = SegmentsManifest::empty();
    first.only_shard_mut().watermark = 11;
    write_publication(prefix_dir, "default", 4, &first, None).expect("the first write commits");

    let mut second = SegmentsManifest::empty();
    second.only_shard_mut().watermark = 22;
    let refused = write_publication(prefix_dir, "default", 4, &second, None)
        .expect_err("the second write at the same n must be refused");
    assert!(
        matches!(
            refused,
            mosaica_store::StoreError::SideManifestExists { .. }
        ),
        "a collision is its own error, not a filesystem fault: {refused}"
    );
    assert!(
        refused.to_string().contains("SEGMENTS-4.json"),
        "the refusal must name the file an operator has to look at: {refused}"
    );

    let committed = read_publication(prefix_dir, "default", 4).unwrap().manifest;
    assert_eq!(
        committed.only_shard().watermark,
        11,
        "the first writer's manifest is intact — the loser overwrote nothing"
    );
    let shard_dir = prefix_dir.join(SHARD0.rel());
    assert!(
        !shard_dir.join("SEGMENTS-4.json.tmp").exists(),
        "and the refused write left no temporary behind"
    );
}

/// **The floor an allocator clears: every `SEGMENTS-<n>.json` present, wherever it sits.**
///
/// The refusal above is the guard at the artefact; this is the number that keeps it from firing.
/// What a writer must not reuse is a *filename*, and a manifest names the files of its own
/// publication and no side-manifest at all — so the set of numbers taken is read off the tree,
/// across every prefix (an unpublished compaction prefix holds its own) and every partition
/// directory (`MANIFEST.json` need not name them all).
#[test]
fn the_side_manifest_floor_is_every_file_present() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    assert_eq!(
        mosaica_store::highest_side_manifest_n(root).unwrap(),
        None,
        "an empty tree takes nothing"
    );

    let live = partitions_dir(&root.join("v00000"));
    std::fs::create_dir_all(live.join("default")).unwrap();
    std::fs::write(live.join("default").join("SEGMENTS-3.json"), b"{}").unwrap();
    // Not a candidate at any n: the name does not end in `.json`, and a crashed write leaves one.
    std::fs::write(live.join("default").join("SEGMENTS-40.json.tmp"), b"{}").unwrap();
    assert_eq!(
        mosaica_store::highest_side_manifest_n(root).unwrap(),
        Some(3)
    );

    // A partition directory no manifest need name.
    std::fs::create_dir_all(live.join("other")).unwrap();
    std::fs::write(live.join("other").join("SEGMENTS-7.json"), b"{}").unwrap();
    assert_eq!(
        mosaica_store::highest_side_manifest_n(root).unwrap(),
        Some(7)
    );

    // The prefix an in-flight compaction is building, which `CURRENT` does not name.
    let pending = partition_dir(&root.join("v00001"), "default");
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::write(pending.join("SEGMENTS-11.json"), b"{}").unwrap();
    assert_eq!(
        mosaica_store::highest_side_manifest_n(root).unwrap(),
        Some(11)
    );

    // A padded name raises the floor rather than refusing: the reader refuses it, and a writer
    // asking which numbers may be taken is told that 12 may be.
    std::fs::write(pending.join("SEGMENTS-012.json"), b"{}").unwrap();
    assert_eq!(
        mosaica_store::highest_side_manifest_n(root).unwrap(),
        Some(12)
    );
}

/// **The floor follows a symlinked prefix and a symlinked partition directory, and a broken link
/// is not a failure.**
///
/// The entry's own file type says "symlink" where the target is the directory the numbers live in,
/// so a scan driven off `file_type` would walk past a whole prefix and hand a writer a number that
/// is already a file. A link to nothing holds no numbers and must not stop a node starting.
#[test]
fn the_side_manifest_floor_follows_symlinked_directories() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    // The real tree, outside the bundle root: `v00000` and one partition reach it only by link.
    let elsewhere = tmp.path().join("elsewhere");
    let real_partition = partition_dir(&elsewhere.join("v00000"), "default");
    std::fs::create_dir_all(&real_partition).unwrap();
    std::fs::write(real_partition.join("SEGMENTS-5.json"), b"{}").unwrap();
    let bundle = root.join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("v00000"), bundle.join("v00000")).unwrap();
    assert_eq!(
        mosaica_store::highest_side_manifest_n(&bundle).unwrap(),
        Some(5),
        "a symlinked prefix directory is walked"
    );

    // A symlinked partition directory inside a real prefix.
    let real_second = elsewhere.join("second");
    std::fs::create_dir_all(&real_second).unwrap();
    std::fs::write(real_second.join("SEGMENTS-8.json"), b"{}").unwrap();
    let live = root.join("live");
    std::fs::create_dir_all(partitions_dir(&live.join("v00000"))).unwrap();
    std::os::unix::fs::symlink(&real_second, partition_dir(&live.join("v00000"), "second"))
        .unwrap();
    assert_eq!(
        mosaica_store::highest_side_manifest_n(&live).unwrap(),
        Some(8),
        "a symlinked partition directory is walked"
    );

    // A link to nothing: skipped, not an error.
    std::os::unix::fs::symlink(elsewhere.join("gone"), live.join("v00001")).unwrap();
    assert_eq!(
        mosaica_store::highest_side_manifest_n(&live).unwrap(),
        Some(8),
        "a broken link holds no numbers and is not a failure"
    );
}

/// The pruner keeps the newest side-manifests and the highest `n` above all, whatever wrote it,
/// and leaves a directory it cannot read whole.
#[test]
fn pruning_keeps_the_newest_manifests_and_never_the_lowest() {
    let tmp = tempfile::TempDir::new().unwrap();
    let prefix_dir = tmp.path().join("v00000");
    let dir = partition_dir(&prefix_dir, "default");
    fs::create_dir_all(&dir).unwrap();

    let names = || {
        let mut found: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    };

    // Fewer than the number kept: nothing to prune.
    for n in [0u64, 1] {
        fs::write(dir.join(format!("SEGMENTS-{n}.json")), b"{}").unwrap();
    }
    assert_eq!(
        mosaica_store::prune_superseded_segments_manifests(&prefix_dir, "default").unwrap(),
        0
    );
    assert_eq!(names(), vec!["SEGMENTS-0.json", "SEGMENTS-1.json"]);

    for n in [2u64, 3, 40] {
        fs::write(dir.join(format!("SEGMENTS-{n}.json")), b"{}").unwrap();
    }
    assert_eq!(
        mosaica_store::prune_superseded_segments_manifests(&prefix_dir, "default").unwrap(),
        2
    );
    assert_eq!(
        names(),
        vec!["SEGMENTS-2.json", "SEGMENTS-3.json", "SEGMENTS-40.json"],
        "the three highest stay, and 40 — the number a writer's floor comes from — above all"
    );

    // A name this reader would not open is a name it will not delete around, either: the
    // directory is left as it stands.
    fs::write(dir.join("SEGMENTS-05.json"), b"{}").unwrap();
    assert!(mosaica_store::prune_superseded_segments_manifests(&prefix_dir, "default").is_err());
    assert_eq!(names().len(), 4);
}

/// Publication `n` of two shards, each with its own watermark, over `previous`.
fn publish_two(
    prefix_dir: &Path,
    n: u64,
    watermarks: [u64; 2],
    previous: Option<&mosaica_store::manifest::PublishedRefs>,
) -> mosaica_store::manifest::PublishedRefs {
    let mut manifest = SegmentsManifest::empty();
    fixture::add_shard_one(&mut manifest);
    for (record, watermark) in manifest.shards.records.iter_mut().zip(watermarks) {
        record.watermark = watermark;
    }
    write_publication(prefix_dir, "default", n, &manifest, previous).expect("published")
}

/// **A publication writes only the files that changed, then the partition file.** A shard whose
/// record is unchanged keeps its file and number, and the partition file names it there.
#[test]
fn a_publication_writes_only_the_shard_it_changed_and_the_partition_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let prefix_dir = tmp.path();
    let first = publish_two(prefix_dir, 1, [5, 0], None);
    assert_eq!((first.shards[0].n, first.shards[1].n), (1, 1));

    let second = publish_two(prefix_dir, 2, [5, 3], Some(&first));
    assert_eq!(
        (second.shards[0].n, second.shards[1].n),
        (1, 2),
        "shard 0 is unchanged and keeps its file; shard 1 is written at the new number"
    );
    assert_eq!(shard_files(prefix_dir, 0), vec![1]);
    assert_eq!(shard_files(prefix_dir, 1), vec![1, 2]);

    let third = publish_two(prefix_dir, 3, [5, 3], Some(&second));
    assert_eq!(
        (third.shards[0].n, third.shards[1].n),
        (1, 2),
        "nothing changed"
    );
    assert_eq!(shard_files(prefix_dir, 1), vec![1, 2]);
    let published = read_publication(prefix_dir, "default", 3).unwrap();
    assert_eq!(published.refs, third);
    let watermarks: Vec<u64> = published
        .manifest
        .shards
        .records
        .iter()
        .map(|r| r.watermark)
        .collect();
    assert_eq!(watermarks, vec![5, 3]);
    assert_eq!(published.manifest.shards.next_id, 2);
}

/// **A shard file a crash left uncommitted raises the floor, and the next publication's prune
/// removes it.** Staged at `n` and never committed, it is named by no partition file: an open
/// serves `n - 1`, the next publication takes `n + 1`, and after it the orphan is gone.
#[test]
fn an_orphan_shard_file_raises_the_floor_and_is_pruned() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let (manifest_json, prefix_dir) = build_fixture(root, 20, "2026-10-15T00:00:00Z");
    let digest = write_manifest_json(&prefix_dir, &manifest_json).unwrap();
    write_current(root, "v00000", &digest).unwrap();

    let mut manifest = read_publication(&prefix_dir, "default", 0).unwrap();
    manifest.manifest.only_shard_mut().high_water += 1;
    let orphaned = mosaica_store::stage_publication(
        &prefix_dir,
        "default",
        1,
        &manifest.manifest,
        Some(&manifest.refs),
    )
    .unwrap();
    assert_eq!(
        shard_files(&prefix_dir, 0),
        vec![0, 1],
        "the changed shard's file was staged"
    );
    drop(orphaned);

    assert_eq!(
        mosaica_store::highest_side_manifest_n(root).unwrap(),
        Some(0)
    );
    assert_eq!(
        mosaica_store::highest_published_n(root).unwrap(),
        Some(1),
        "the staged shard file takes its number"
    );
    let bundle = open_bundle(root).expect("the committed publication opens");
    assert_eq!(bundle.partitions["default"].segments_n, 0);
    assert!(!bundle.partitions["default"].stepped_down());

    manifest.manifest.only_shard_mut().high_water += 1;
    write_publication(
        &prefix_dir,
        "default",
        2,
        &manifest.manifest,
        Some(&manifest.refs),
    )
    .unwrap();
    assert_eq!(shard_files(&prefix_dir, 0), vec![0, 1, 2]);
    mosaica_store::prune_superseded_segments_manifests(&prefix_dir, "default").unwrap();
    assert_eq!(
        shard_files(&prefix_dir, 0),
        vec![0, 2],
        "the orphan is gone, and the files the kept partition files name stay"
    );
    assert_eq!(
        open_bundle(root).unwrap().partitions["default"].segments_n,
        2
    );
}

/// Pruning removes a shard file once no kept partition file names it, and keeps one an older
/// kept partition file still names.
#[test]
fn pruning_keeps_every_shard_file_a_kept_partition_file_names() {
    let tmp = tempfile::TempDir::new().unwrap();
    let prefix_dir = tmp.path();
    let mut previous = publish_two(prefix_dir, 1, [1, 0], None);
    for n in 2..=5 {
        previous = publish_two(prefix_dir, n, [n, 0], Some(&previous));
    }
    assert_eq!(shard_files(prefix_dir, 0), vec![1, 2, 3, 4, 5]);
    assert_eq!(shard_files(prefix_dir, 1), vec![1]);
    mosaica_store::prune_superseded_segments_manifests(prefix_dir, "default").unwrap();
    assert_eq!(shard_files(prefix_dir, 0), vec![3, 4, 5]);
    assert_eq!(
        shard_files(prefix_dir, 1),
        vec![1],
        "every kept partition file names shard 1's first file"
    );
}

#[test]
fn the_shard_and_space_fields_round_trip() {
    use mosaica_store::manifest::{
        EntitySet, LayerSpaceRecord, OwedDeletions, ScopedColumn, SliceVersion,
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let prefix_dir = tmp.path();
    let space = mosaica_types::LayerSpace::new(3).unwrap();
    let mut manifest = SegmentsManifest::empty();
    manifest.shards.next_id = 5;
    let record = manifest.only_shard_mut();
    record.state = mosaica_types::ShardState::Sealed;
    record.identity = mosaica_store::manifest::shard_identity(&[9; 32], record.id);
    record.scoped_columns.push(ScopedColumn {
        column: "mood".to_string(),
        view: "quarter:2026-Q3".to_string(),
        incarnation: 2,
    });
    record.runtime_bases.push("score".to_string());
    record.slice_versions.push(SliceVersion {
        space,
        level: 1,
        version: 4,
    });
    let deleted: croaring::Bitmap = [7u32, 9].into_iter().collect();
    let mut layer = LayerSpaceRecord::empty(space);
    layer.tombstones = EntitySet::of(&deleted);
    layer.owed.push(OwedDeletions {
        shard: ShardId::ZERO,
        artifacts: EntitySet::of(&deleted),
    });
    manifest.layer_spaces.push(layer);

    let refs = write_publication(prefix_dir, "default", 1, &manifest, None).unwrap();
    assert!(!refs.shards[0].denies && refs.layer_spaces[0].denies);
    let read = read_publication(prefix_dir, "default", 1).unwrap().manifest;
    assert_eq!(read.shards.next_id, 5);
    let back = read.only_shard();
    let written = manifest.only_shard();
    assert_eq!(back.state, written.state);
    assert_eq!(back.identity, written.identity);
    assert_eq!(back.scoped_columns, written.scoped_columns);
    assert_eq!(back.runtime_bases, written.runtime_bases);
    assert_eq!(back.slice_versions, written.slice_versions);
    let owed = &read.layer_spaces[0].owed;
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].shard, ShardId::ZERO);
    assert_eq!(owed[0].artifacts.entities().unwrap().to_vec(), vec![7, 9]);
}
