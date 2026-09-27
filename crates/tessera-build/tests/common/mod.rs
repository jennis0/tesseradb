//! What several build tests share.

use std::collections::HashMap;
use std::path::Path;

use tessera_store::unique::{UniqueIndexes, UniqueKey};

/// The entity each of `values` names in the `u64` unique column `attribute` of the bundle built at
/// `root`. Entity ids are signature-sorted, so a source row's value is how a test finds the item
/// the row became.
#[allow(dead_code)]
pub fn entities_of(
    root: &Path,
    attribute: &str,
    values: impl IntoIterator<Item = u64>,
) -> HashMap<u64, u32> {
    let bundle = tessera_store::read::open_bundle(root).expect("the built bundle opens");
    let (phash, partition) = bundle.partitions.iter().next().expect("one partition");
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT")).unwrap();
    let prefix = root.join(current["prefix"].as_str().expect("CURRENT names a prefix"));
    let indexes = UniqueIndexes::open(&bundle.manifest, &partition.manifest, &prefix, None)
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
/// [`entities_of`] to find each item by. Appended, so every declared column keeps its position.
#[allow(dead_code)]
pub fn with_id(mut schema: tessera_build::config::Schema) -> tessera_build::config::Schema {
    schema.attributes.push(tessera_build::config::Attribute {
        field: Some("entity_id".to_string()),
        name: "id".to_string(),
        title: None,
        ty: tessera_spatial::tiler::ScalarType::U64,
        analyser: None,
        vocabulary: None,
        value_set: None,
        index: false,
        render: false,
        unique: true,
    });
    schema
}
