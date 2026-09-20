use std::collections::BTreeMap;
use std::sync::Arc;

use croaring::Bitmap;
use tessera_filter::{Codes, SortedDict, SortedDictWriter, ValueColumn};

use super::columns::{empty_record_stack, FilterColumns, Layer, Layers, Route};
use super::declared::{Family, Placement};

/// A dictionary over already-sorted distinct keys, read back from memory.
pub(super) fn dict(keys: &[&str]) -> Arc<SortedDict> {
    let mut bytes = Vec::new();
    let mut writer = SortedDictWriter::new(&mut bytes).expect("a writer opens");
    for key in keys {
        writer.push(key).expect("keys ascend strictly");
    }
    writer.finish().expect("the dictionary closes");
    Arc::new(SortedDict::from_vec(bytes).expect("the dictionary reads back"))
}

/// An ordinal column every entity carries a value in: entity id is the slot.
pub(super) fn universal(ordinals: &[u32]) -> Arc<ValueColumn> {
    Arc::new(ValueColumn::universal(Codes::U32(ordinals.to_vec().into())))
}

/// An ordinal column only `entities` carry a value in, in ascending entity order.
pub(super) fn partial(entities: &[u32], ordinals: &[u32]) -> Arc<ValueColumn> {
    Arc::new(
        ValueColumn::partial(Codes::U32(ordinals.to_vec().into()), Bitmap::of(entities))
            .expect("one ordinal per present entity"),
    )
}

pub(super) fn set(entities: &[u32]) -> Bitmap {
    Bitmap::of(entities)
}

pub(super) fn members(bitmap: &Bitmap) -> Vec<u32> {
    bitmap.iter().collect()
}

/// A keyword column of one or more layers, each with its own dictionary — the shape
/// [`FilterColumns::open`] builds from a manifest, assembled here without one.
pub(super) fn keyword_column(
    name: &str,
    layers: Vec<(Option<&str>, Arc<ValueColumn>, Arc<SortedDict>)>,
) -> FilterColumns {
    let mut covered = Bitmap::new();
    let layers: Vec<Layer> = layers
        .into_iter()
        .map(|(values_rel, values, dict)| {
            covered |= values.present();
            Layer {
                values_rel: values_rel.map(str::to_string),
                values,
                dict: Some(dict),
            }
        })
        .collect();
    let mut columns = BTreeMap::new();
    columns.insert(
        name.to_string(),
        Layers {
            declared_index: 0,
            layers,
            covered,
            filterable: true,
            postings: None,
            analyser: None,
            text: Vec::new(),
            route: Route::Scan,
            family: Family::Keyword,
        },
    );
    let mut placements = BTreeMap::new();
    placements.insert(
        name.to_string(),
        Placement {
            entity: true,
            row: false,
            family: Family::Keyword,
        },
    );
    FilterColumns {
        columns,
        placements,
        access: tessera_filter::Access::Read,
        records: Arc::new(empty_record_stack()),
        entity_terms: Arc::new(tessera_store::EntityTermsStack::empty()),
    }
}
