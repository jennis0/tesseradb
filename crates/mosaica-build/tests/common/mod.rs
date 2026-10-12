//! What several build tests share.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mosaica_build::BuildArgs;
use mosaica_store::unique::{UniqueIndexes, UniqueKey};
use mosaica_store::ShardLayout;
use mosaica_types::{IdentityKey, ShardId};

/// The built bundle's one shard, in the one partition a build writes.
#[allow(dead_code)]
pub const SHARD: ShardLayout<'static> = ShardLayout::new("default", ShardId::ZERO);

/// The identity key every build test builds under.
#[allow(dead_code)]
pub fn test_key() -> IdentityKey {
    IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").unwrap()
}

/// A build into `out` under [`test_key`], writing no oracle pairs, with every input empty. A test
/// sets what it varies with struct update syntax.
#[allow(dead_code)]
pub fn build_args(out: &Path) -> BuildArgs {
    BuildArgs {
        views: Vec::new(),
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.to_path_buf(),
        limit: None,
        shard_size: None,
        strict: false,
        identity_key: test_key(),
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    }
}

/// The prefix `CURRENT` names in the bundle at `out`.
#[allow(dead_code)]
pub fn current_prefix(out: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

/// The directory of the prefix `CURRENT` names, which every [`SHARD`] path is relative to.
#[allow(dead_code)]
pub fn prefix_dir(out: &Path) -> PathBuf {
    out.join(current_prefix(out))
}

/// [`SHARD`]'s directory in the prefix `CURRENT` names.
#[allow(dead_code)]
pub fn shard_dir(out: &Path) -> PathBuf {
    SHARD.dir(&prefix_dir(out))
}

/// The one segment a build writes for view `s0`.
#[allow(dead_code)]
pub fn segment_dir(out: &Path) -> PathBuf {
    prefix_dir(out).join(SHARD.segment_rel("s0", "seg-0"))
}

/// The entity each of `values` names in the `u64` unique column `attribute` of the bundle built at
/// `root`. Entity ids are signature-sorted, so a source row's value is how a test finds the item
/// the row became.
#[allow(dead_code)]
pub fn entities_of(
    root: &Path,
    attribute: &str,
    values: impl IntoIterator<Item = u64>,
) -> HashMap<u64, u32> {
    let bundle = mosaica_store::read::open_bundle(root).expect("the built bundle opens");
    let (phash, partition) = bundle.partitions.iter().next().expect("one partition");
    let indexes = UniqueIndexes::open(
        &bundle.manifest,
        &partition.manifest,
        &partition.only_shard().record,
        &prefix_dir(root),
        None,
    )
    .unwrap_or_else(|e| panic!("partition {phash}'s unique indexes open: {e}"));
    let index = indexes
        .get(attribute)
        .unwrap_or_else(|| panic!("'{attribute}' is a unique column"));
    let values: Vec<u64> = values.into_iter().collect();
    let keys: Vec<UniqueKey> = values.iter().map(|v| UniqueKey::unsigned(*v)).collect();
    index
        .lookup(&keys)
        .expect("the index reads")
        .into_iter()
        .map(|(at, entity)| (values[at], entity))
        .collect()
}

/// `schema` with a unique `id` column over the files' `entity_id` appended, for
/// [`entities_of`] to find each item by, unless it declares one. Appended, so every declared
/// column keeps its position.
#[allow(dead_code)]
pub fn with_id(mut schema: mosaica_build::config::Schema) -> mosaica_build::config::Schema {
    if schema.attributes.iter().any(|a| a.name == "id") {
        return schema;
    }
    schema.attributes.push(mosaica_build::config::Attribute {
        field: Some("entity_id".to_string()),
        name: "id".to_string(),
        title: None,
        ty: mosaica_spatial::tiler::ScalarType::U64,
        analyser: None,
        vocabulary: None,
        value_set: None,
        index: false,
        render: false,
        unique: true,
    });
    schema
}

/// A declaration of one unique attribute, `id`, over the files' `entity_id`, and its values read
/// from the points file at `points`.
#[allow(dead_code)]
pub fn id_attributes(
    points: &Path,
) -> (
    mosaica_build::config::Schema,
    Vec<mosaica_build::config::AttributeSource>,
) {
    let schema = with_id(Default::default());
    let sources = mosaica_build::config::AttributeSource::over(points, &schema);
    (schema, sources)
}

/// Every file of two bundles, compared byte for byte — `MANIFEST.json` with its wall-clock
/// `created_at` blanked, and `CURRENT`, which is nothing but that manifest's digest, skipped with
/// it. Every digest the manifest records for every other file is compared verbatim, so a
/// membership that differed by one entity fails here.
#[allow(dead_code)]
pub fn assert_bundles_identical(left: &Path, right: &Path, what: &str) {
    fn collect(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(root, root, &mut out);
        out
    }
    let (a, b) = (collect(left), collect(right));
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "{what}: the two bundles do not contain the same files"
    );
    for (name, left_bytes) in &a {
        let right_bytes = &b[name];
        if name.ends_with("MANIFEST.json") {
            let normalise = |bytes: &[u8]| {
                let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                value["created_at"] = serde_json::Value::Null;
                value
            };
            assert_eq!(
                normalise(left_bytes),
                normalise(right_bytes),
                "{what}: MANIFEST.json differs (ignoring created_at)"
            );
            continue;
        }
        if name == "CURRENT" {
            continue;
        }
        assert_eq!(
            left_bytes, right_bytes,
            "{what}: {name} is not byte-identical"
        );
    }
    assert!(
        a.len() > 6,
        "{what}: expected a full bundle, found {}",
        a.len()
    );
}
