//! What `Engine::open` does, and what it refuses: the allocator seed it takes from durable
//! state, the plugin and manifest it insists agree, the sidecar it must not read, and the
//! `EngineConfig` values it will not serve under.

mod common;

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use sha2::Digest;
use tessera_engine::{Engine, EngineConfig, EngineError};
use tessera_lifecycle::wal::{Wal, WalRecord};
use tessera_plugin::{Passthrough, Plugin};
use tessera_store::StoreError;

use common::*;

/// `Engine::open` seeds the I9 allocator at `max(manifest high-water, WAL high-water)` — here,
/// with an empty WAL, that is exactly the bundle's `entity_id_high_water` (== `N_ITEMS`, the
/// bootstrap build's dense `0..n` assignment).
#[test]
fn engine_open_seeds_the_allocator_from_the_manifest_high_water() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    assert_eq!(engine.allocator_high_water(), N_ITEMS);
}

/// S6: `Allocator::try_new`'s own doc calls it "the check that belongs at open", and `Engine::open`
/// is open — it must refuse a seed at or above `u32::MAX` before any ingest, rather than let the
/// first allocation surface it as an opaque exhaustion error. The seed comes from durable state
/// this process did not write (MANIFEST's high-water, or a replayed WAL lease), so a hand-edited or
/// corrupt value has to fail closed here.
#[test]
fn engine_open_refuses_an_out_of_range_allocator_seed() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // A replayed row at the top of the u32 space — `high_water_from` folds
    // `entity_id + 1` into the seed, so this is the WAL-side half of the seeding rule, reached
    // without touching MANIFEST's digest. The refusal fires before replay resolves anything, so
    // the row's other fields never matter.
    let wal_path = tmp.path().join("wal.log");
    {
        let (mut wal, _initial) = Wal::open(&wal_path).unwrap();
        wal.append(&WalRecord::IngestBatch {
            batch_id: "over-the-top".to_string(),
            body_hash: [0u8; 32],
            rows: vec![tessera_lifecycle::WalRow {
                external_id: None,
                entity_id: tessera_types::EntityId::new(u32::MAX as u64 - 1),
                view: "s0".to_string(),
                join: false,
                descriptors: Vec::new(),
                x: 0.5,
                y: 0.5,
                scalars: Vec::new(),
                scoped: Vec::new(),
            }],
        })
        .unwrap();
        wal.fsync().unwrap();
    }

    let opened = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &wal_path,
        Passthrough::new(),
        config(),
    );
    let Err(err) = opened else {
        panic!("a seed at u32::MAX must be refused at open, not at the first ingest");
    };
    assert!(
        matches!(err, EngineError::Malformed(ref d) if d.contains("allocator")),
        "expected a typed refusal naming the allocator, got {err:?}"
    );
}

/// A `Passthrough` in every respect but the hash it declares — the shape of a plugin whose label
/// rule was changed and whose identity was bumped with it.
struct RelabellingPlugin;

impl tessera_plugin::Plugin for RelabellingPlugin {
    fn terms_of_labels(
        &self,
        labels: &[tessera_plugin::Descriptor],
    ) -> Result<Vec<tessera_plugin::Descriptor>, tessera_plugin::PluginError> {
        Passthrough::new().terms_of_labels(labels)
    }

    fn terms_of_auth(
        &self,
        auth_data: &[u8],
    ) -> Result<Vec<tessera_plugin::Descriptor>, tessera_plugin::PluginError> {
        Passthrough::new().terms_of_auth(auth_data)
    }

    fn present_terms(
        &self,
        descriptors: &[tessera_plugin::Descriptor],
    ) -> Result<Vec<String>, tessera_plugin::PluginError> {
        Passthrough::new().present_terms(descriptors)
    }

    fn declared_bounds(&self) -> tessera_plugin::DeclaredBounds {
        Passthrough::new().declared_bounds()
    }

    fn data_plugin_hash(&self) -> String {
        // Derived from the real one so this stays a *different* value however the identity moves,
        // rather than a literal that could one day collide with the passthrough's own.
        format!("{}ff", &Passthrough::new().data_plugin_hash()[2..])
    }

    fn auth_plugin_hash(&self) -> String {
        Passthrough::new().auth_plugin_hash()
    }
}

/// Rewrite `MANIFEST.json`'s `data_plugin_hash` to `value` and re-point `CURRENT` at the new
/// digest, so the bundle still verifies and the hash is the only thing that changed.
fn rewrite_data_plugin_hash(bundle_root: &Path, value: serde_json::Value) {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle_root.join("CURRENT")).unwrap()).unwrap();
    let prefix = current["prefix"].as_str().unwrap().to_string();

    let manifest_path = bundle_root.join(&prefix).join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["data_plugin_hash"] = value;
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    std::fs::write(&manifest_path, &bytes).unwrap();

    let digest = sha2::Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(
        bundle_root.join("CURRENT"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "prefix": prefix,
            "manifest_digest": hex,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// **A bundle may only be served by the plugin that labelled it.** `MANIFEST.json` records the
/// build's `data_plugin_hash` for exactly this check, and nothing downstream of open would notice
/// its absence: postings written under one label rule are read back intact and resolved against
/// another, so every item is mislabelled and no error is raised anywhere. `Engine::open` is the
/// only place the recorded hash and the serving plugin meet.
///
/// The check is the *data* hash alone. The auth module's hash keys the mask cache and is not in
/// the manifest, so there is no equivalent open-time enforcement for it.
#[test]
fn engine_open_refuses_a_plugin_whose_data_hash_is_not_the_bundles() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // The same bundle opens under the plugin that built it — without this the case would pass on
    // a fixture that was broken for some unrelated reason.
    Engine::open(
        &bundle_root,
        &tmp.path().join("cache-ok"),
        &tmp.path().join("wal-ok.log"),
        Passthrough::new(),
        config(),
    )
    .expect("the bundle opens under the plugin that built it");

    let opened = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        RelabellingPlugin,
        config(),
    );
    let Err(err) = opened else {
        panic!("a bundle labelled by another plugin must be refused, not served mislabelled");
    };
    let EngineError::Malformed(detail) = err else {
        panic!("expected a typed Malformed refusal, got {err:?}");
    };
    assert!(
        detail.contains(&Passthrough::new().data_plugin_hash())
            && detail.contains(&RelabellingPlugin.data_plugin_hash()),
        "the refusal must name BOTH hashes so an operator can tell which end is wrong: {detail}"
    );
}

/// **Fail closed on a manifest that does not say what labelled it.** An empty `data_plugin_hash`
/// is a mismatch, not a pass: a bundle that names no labelling rule cannot be shown to have been
/// labelled by this plugin, and treating "unknown" as agreement would exempt exactly the
/// hand-written or half-migrated manifest the check exists for.
#[test]
fn engine_open_refuses_a_manifest_whose_data_plugin_hash_is_empty() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    rewrite_data_plugin_hash(&bundle_root, serde_json::json!(""));

    let opened = Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        Passthrough::new(),
        config(),
    );
    let Err(err) = opened else {
        panic!("an empty manifest hash must be refused, not treated as agreement");
    };
    assert!(
        matches!(err, EngineError::Malformed(ref d) if d.contains("<empty>")),
        "the refusal must say the manifest names nothing, not print a blank: {err:?}"
    );
}

/// Residency proxy: `Engine::open` must never touch the external-id sidecar (the per-extent
/// laziness guarantee, contracts §0.3 deviation 9) — this is a proxy, not the memory-residency
/// measurement itself.
///
/// **The files are removed from disk before the engine opens**, and `is_open()` alone would not be
/// a test of this: it reports only the sidecar's own `OnceLock` state, which a `verify_files` pass
/// reading and SHA-256'ing every extent and the locator — the whole 18.9 GB sequential read the
/// deviation exists to remove — never sets. Deleting the files makes any read of them, at any layer,
/// a hard failure of `Engine::open`, which is the property actually claimed.
#[test]
fn engine_open_does_not_touch_the_sidecar() {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );

    // Every sidecar file the build wrote, named from MANIFEST's own `files` map rather than
    // guessed, so this cannot silently check nothing if the layout moves.
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle_root.join("CURRENT")).unwrap()).unwrap();
    let prefix_dir = bundle_root.join(current["prefix"].as_str().unwrap());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(prefix_dir.join("MANIFEST.json")).unwrap()).unwrap();
    let sidecar_files: Vec<PathBuf> = manifest["files"]
        .as_object()
        .unwrap()
        .keys()
        // The external-ID sidecar's own files, not everything under `entities/`: the entity→term
        // transpose lives there too (contracts §2.4) and is deliberately opened *at* `Engine::open`
        // like the record blob, so deleting it would make this test assert the opposite posture for
        // an artefact it is not about.
        .filter(|rel| rel.contains("/entities/external-ids") || rel.contains("/entities/ext-locator"))
        .map(|rel| prefix_dir.join(rel))
        .collect();
    assert!(
        sidecar_files.len() >= 2,
        "fixture must write at least an extent and the locator, found {sidecar_files:?}"
    );
    // The digests stay in MANIFEST — the deviation defers verification, it does not drop it.
    for path in &sidecar_files {
        let rel = path.strip_prefix(&prefix_dir).unwrap().to_string_lossy();
        let rel = rel.replace('\\', "/");
        assert!(
            manifest["files"][&rel]["sha256"].is_string(),
            "{rel} must keep its digest in MANIFEST so the sidecar can verify it at first touch"
        );
        std::fs::remove_file(path).unwrap();
    }

    let engine = open_engine(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
    );
    assert!(
        !engine.external_id_sidecar_is_open(),
        "Engine::open must not touch the external-id sidecar"
    );

    // Deferred, not dropped: the first resolution *does* reach for the file, and fails closed
    // because it is gone.
    let err = engine
        .resolve_external_id(&source_id_key(0))
        .expect_err("the first resolution must reach the (now absent) extent and fail closed");
    assert!(
        matches!(
            err,
            StoreError::InvalidSidecar { .. } | StoreError::Io { .. }
        ),
        "expected a typed sidecar/IO error, got {err:?}"
    );
}

fn open(config: EngineConfig) -> Result<Engine, EngineError> {
    let tmp = TempDir::new().unwrap();
    let bundle_root = tmp.path().join("bundle");
    build_fixture(
        &bundle_root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Engine::open(
        &bundle_root,
        &tmp.path().join("cache"),
        &tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config,
    )
}

#[test]
fn a_config_that_draws_nothing_in_a_sparse_tile_does_not_open() {
    let refused = open(EngineConfig { k_min: 0, ..config() });
    assert!(matches!(refused, Err(EngineError::ConfigRefused(_))));
}

#[test]
fn a_config_whose_merge_or_coalesce_could_never_select_does_not_open() {
    for width in [0, 1] {
        let refused = open(EngineConfig { tier_width: Some(width), ..config() });
        assert!(matches!(refused, Err(EngineError::ConfigRefused(_))), "tier_width {width}");
        let refused = open(EngineConfig { coalesce_width: Some(width), ..config() });
        assert!(matches!(refused, Err(EngineError::ConfigRefused(_))), "coalesce_width {width}");
    }
}

#[test]
fn the_smallest_lawful_values_open() {
    let opened = open(EngineConfig {
        k_min: 1,
        tier_width: Some(2),
        coalesce_width: Some(2),
        ..config()
    });
    assert!(opened.is_ok());
}
