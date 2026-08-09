//! `write_manifest_json` and `write_current` — the two bundle-artefact writers pass 5 of a fold
//! needs and `tessera-build`'s `write_manifests` now calls instead of writing the bytes itself
//! (compaction §10's rule paragraph).
//!
//! These tests exercise the writers exactly as a caller must: build a minimal but real bundle's
//! segment/permutation/`SEGMENTS-0.json` by hand (the same fixture shape `bundle_read.rs` uses),
//! then write `MANIFEST.json` and `CURRENT` through the two functions under test, then open the
//! result with the real reader — never a hand-parsed assertion of either file's shape, matching
//! `bundle_read.rs`'s own voice.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use tessera_spatial::tiler::{sort_batch, TilerItem};
use tessera_spatial::{fixed32, Bounds};
use tessera_store::manifest::{
    IdentityDescriptor, Manifest, PartitionDescriptor, Quantisation, SegmentDescriptor,
    SegmentsManifest, SliceDescriptor,
};
use tessera_store::manifest_write::{write_current, write_manifest_json};
use tessera_store::write::{write_permutation, write_segment};
use tessera_store::write_segments_manifest;
use tessera_store::{open_bundle, StoreError};
use tessera_types::{EntityId, TesseraId, IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

/// A synthetic `tessera_id`-shaped value for test fixtures — see `bundle_read.rs`'s copy of the
/// same helper for why full splitmix64 output rather than a raw seed.
fn synthetic_tessera_id(seed: u64) -> TesseraId {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    TesseraId::new(z)
}

fn unit_extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Build the one-partition, one-slice, one-segment fixture `MANIFEST.json` needs to sit beside
/// for `open_bundle` to succeed — everything short of the two files this module's functions
/// write. Returns the assembled (but not yet written) [`Manifest`] and the prefix directory it
/// belongs under.
///
/// `created_at` is a parameter, not a constant, purely so
/// [`write_manifest_json_digest_changes_when_the_manifest_changes`] can build two otherwise
/// identical manifests that differ in exactly one field.
fn build_fixture(root: &Path, n: u64, created_at: &str) -> (Manifest, std::path::PathBuf) {
    let extent = unit_extent();
    let mut items: Vec<TilerItem> = (0..n)
        .map(|entity_id| TilerItem {
            tessera_id: synthetic_tessera_id(entity_id),
            qx: fixed32(((entity_id * 37) % 100) as f64 / 100.0, 0.0, 1.0),
            qy: fixed32(((entity_id * 61) % 100) as f64 / 100.0, 0.0, 1.0),
            scalars: vec![],
        })
        .collect();
    let mut entity_ids: Vec<EntityId> = (0..n).map(EntityId::new).collect();
    let codes = sort_batch(&mut items, &mut entity_ids);

    let prefix_dir = root.join("v00000");
    let partition_dir = prefix_dir.join("partitions").join("default");
    let slice_dir = partition_dir.join("slices").join("main");
    let seg_dir = slice_dir.join("segments").join("seg0");
    fs::create_dir_all(&seg_dir).expect("mkdir seg_dir");

    write_segment(&seg_dir, &items, &codes, &[]).expect("write_segment");
    write_permutation(&slice_dir.join("permutation.bin"), &entity_ids, n)
        .expect("write_permutation");

    let file_digest = |path: &Path| {
        let bytes = fs::read(path).expect("read for digest");
        tessera_store::manifest::FileDigest {
            size: bytes.len() as u64,
            sha256: hex_sha256(&bytes),
        }
    };
    let mut segments_files = BTreeMap::new();
    segments_files.insert(
        "partitions/default/slices/main/permutation.bin".to_string(),
        file_digest(&slice_dir.join("permutation.bin")),
    );
    segments_files.insert(
        "partitions/default/slices/main/segments/seg0/columns.arrow".to_string(),
        file_digest(&seg_dir.join("columns.arrow")),
    );
    segments_files.insert(
        "partitions/default/slices/main/segments/seg0/morton.u32".to_string(),
        file_digest(&seg_dir.join("morton.u32")),
    );

    let segments_manifest = SegmentsManifest {
        watermark: n,
        entity_id_high_water: n,
        segments: vec![SegmentDescriptor {
            slice: "main".to_string(),
            seg_id: "seg0".to_string(),
            row_count: items.len() as u32,
            entity_lo: 0,
            entity_hi: n.saturating_sub(1),
        }],
        deltas: vec![],
        dict_extents: vec![],
        attr_extents: Vec::new(),
        external_id_runs: vec![],
        locator_extents: vec![],
        tombstones: vec![],
        deny: vec![],
        vocabulary_extensions: vec![],
        files: segments_files,
    };
    fs::write(
        partition_dir.join("SEGMENTS-0.json"),
        serde_json::to_vec_pretty(&segments_manifest).expect("serialise SEGMENTS-0"),
    )
    .expect("write SEGMENTS-0.json");

    let manifest = Manifest {
        bundle_format: 1,
        created_at: created_at.to_string(),
        data_plugin_hash: "builtin:passthrough:1".to_string(),
        declared_bounds: serde_json::json!({}),
        declared_scalars: vec![],
        vocabularies: vec![],
        small_term_threshold: 32,
        quantisation: Quantisation {
            x_min: extent.x_min,
            x_max: extent.x_max,
            y_min: extent.y_min,
            y_max: extent.y_max,
        },
        entity_id_high_water: n,
        identity: IdentityDescriptor {
            construction: IDENTITY_CONSTRUCTION.to_string(),
            rounds: IDENTITY_ROUNDS,
            key: "0123456789abcdef0123456789abcdef".to_string(),
            shard_id: 0,
            idset: 1,
        },
        slices: vec![SliceDescriptor {
            id: "main".to_string(),
            display_name: "Main".to_string(),
        }],
        partitions: vec![PartitionDescriptor {
            phash: "default".to_string(),
            required_terms: vec![],
        }],
        provenance: serde_json::json!({}),
        files: BTreeMap::new(),
    };

    (manifest, prefix_dir)
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
    assert_eq!(bundle.manifest.bundle_format, 1);
    assert_eq!(bundle.manifest.entity_id_high_water, 64);
    assert_eq!(bundle.manifest.identity.idset, 1);

    let partition = bundle.partitions.get("default").expect("default partition");
    let slice = partition.slices.get("main").expect("main slice");
    assert_eq!(slice.segments.len(), 1);
    assert_eq!(slice.segments[0].row_count, 64);
}

/// The digest `write_manifest_json` returns is the SHA-256 of the bytes actually sitting on
/// disc afterwards — read back independently here, outside the function under test — and those
/// bytes are byte-for-byte `serde_json::to_vec_pretty` of the manifest. Pinning the exact
/// serialiser matters beyond this one test: `tessera-build` wrote every bundle on disc today
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
         tessera-build's serialiser"
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
    assert_eq!(bundle.manifest.entity_id_high_water, 16);
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

/// The property the fold's identity rotation depends on (compaction §4's `bundle_identity`,
/// carried on `Generation::fragments`): two manifests that differ in even one field must
/// digest differently, or a fold that changed the corpus would leave every session's fragment
/// cache believing nothing had rotated.
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

/// An empty side-manifest. `watermark` is the field the refuse-to-replace test varies, so the
/// two writers' manifests are distinguishable in the committed bytes.
fn manifest_fixture() -> SegmentsManifest {
    SegmentsManifest {
        watermark: 0,
        entity_id_high_water: 0,
        segments: Vec::new(),
        deltas: Vec::new(),
        dict_extents: Vec::new(),
        attr_extents: Vec::new(),
        external_id_runs: Vec::new(),
        locator_extents: Vec::new(),
        tombstones: Vec::new(),
        deny: Vec::new(),
        vocabulary_extensions: vec![],
        files: BTreeMap::new(),
    }
}

/// **Obligation 11: a side-manifest is never replaced.**
///
/// A side-manifest is complete current state for its partition, not a diff, so a second write
/// at the same `n` does not merge with the first — before refuse-to-replace it *replaced* it,
/// with a manifest built from the same base and naming only its own segment, so the winner's
/// acked and published rows went missing from what a restart opens.
///
/// Asserted at the filesystem operation rather than through two slices, because
/// `tessera build` emits one and `dispatch_flushes` now sends one plan: the collision is a
/// property of the write, and this is the guard at the artefact that stands behind the rule at
/// the caller.
///
/// **Mutation:** restore `std::fs::rename` and the second write succeeds, silently.
#[test]
fn a_side_manifest_is_never_replaced() {
    let tmp = tempfile::TempDir::new().unwrap();
    let prefix_dir = tmp.path();
    std::fs::create_dir_all(prefix_dir.join("partitions").join("default")).unwrap();

    let mut first = manifest_fixture();
    first.watermark = 11;
    write_segments_manifest(prefix_dir, "default", 4, &first).expect("the first write commits");

    let mut second = manifest_fixture();
    second.watermark = 22;
    let refused = write_segments_manifest(prefix_dir, "default", 4, &second)
        .expect_err("the second write at the same n must be refused");
    assert!(
        refused.to_string().contains("never replaced"),
        "the refusal must say what it is: {refused}"
    );

    let committed: SegmentsManifest = serde_json::from_slice(
        &std::fs::read(
            prefix_dir
                .join("partitions")
                .join("default")
                .join("SEGMENTS-4.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        committed.watermark, 11,
        "the first writer's manifest is intact — the loser overwrote nothing"
    );
    assert!(
        !prefix_dir
            .join("partitions")
            .join("default")
            .join("SEGMENTS-4.json.tmp")
            .exists(),
        "and the refused write left no temporary behind"
    );
}
