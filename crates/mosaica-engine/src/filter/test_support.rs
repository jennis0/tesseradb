use std::collections::BTreeMap;
use std::sync::Arc;

use croaring::Bitmap;
use mosaica_filter::{Codes, ColumnPostings, SortedDict, SortedDictWriter, ValueColumn};

use super::columns::{empty_record_stack, Column, FilterColumns, Layer, Route};
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

/// An extent as a publication hands one over. Only the column name, the values path and the two
/// readers are composed from; the rest of the entry is what the manifest carries.
pub(super) fn opened(
    column: &str,
    values_rel: &str,
    values: Arc<ValueColumn>,
    dict: Option<Arc<SortedDict>>,
) -> super::columns::successor::OpenedExtent {
    super::columns::successor::OpenedExtent {
        extent: mosaica_store::manifest::AttrExtent {
            column: column.to_string(),
            view: None,
            incarnation: None,
            values: values_rel.to_string(),
            presence: format!("{values_rel}.roaring"),
            dict: dict.is_some().then(|| format!("{values_rel}.dict")),
            postings: None,
            offsets: None,
        },
        values,
        dict,
    }
}

pub(super) fn set(entities: &[u32]) -> Bitmap {
    Bitmap::of(entities)
}

pub(super) fn members(bitmap: &Bitmap) -> Vec<u32> {
    bitmap.iter().collect()
}

/// A keyword column of one or more layers, each with its own dictionary: the shape
/// [`FilterColumns::open`] builds from a manifest, assembled here without one.
pub(super) fn keyword_column(
    name: &str,
    layers: Vec<(Option<&str>, Arc<ValueColumn>, Arc<SortedDict>)>,
) -> FilterColumns {
    let mut layers = layers.into_iter();
    let (base_rel, base_values, base_dict) = layers.next().expect("a column has at least a base");
    assert!(base_rel.is_none(), "the first layer is the base");
    let mut column = Column::values(
        0,
        true,
        Family::Keyword,
        Some(Layer {
            values_rel: None,
            values: base_values,
            dict: Some(base_dict),
        }),
        None,
        Route::Scan,
    );
    for (values_rel, values, dict) in layers {
        let values_rel = values_rel.expect("every layer after the base is an extent");
        column
            .push_extent(name, values_rel, values, Some(dict))
            .expect("the extents of a fixture are disjoint and bring their dictionaries");
    }
    let mut columns = BTreeMap::new();
    columns.insert(name.to_string(), column);
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
        access: mosaica_filter::Access::Read,
        records: Arc::new(empty_record_stack()),
        entity_terms: Arc::new(mosaica_store::EntityTermsStack::empty()),
        unique: BTreeMap::new(),
    }
}

/// A `u16` category column `c`: a base over entities `0..base.len()` with its postings where
/// `postings` says so, and one extent over `extent`'s entities.
pub(crate) fn category_column(
    dir: &std::path::Path,
    base: &[u16],
    postings: bool,
    extent: &[(u32, u16)],
) -> FilterColumns {
    let base_values = Arc::new(ValueColumn::universal(Codes::U16(base.to_vec().into())));
    let postings = postings.then(|| {
        let mut by_code: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (entity, &code) in base.iter().enumerate() {
            if code != 0 {
                by_code
                    .entry(u32::from(code))
                    .or_default()
                    .push(entity as u32);
            }
        }
        let path = dir.join("postings.arrow");
        let records: Vec<(u32, Vec<u32>)> = by_code.into_iter().collect();
        mosaica_authz::write_delta_tier_at(&path, &records, 32).expect("postings write");
        Arc::new(ColumnPostings::open_keyed(&path).expect("postings open"))
    });
    let mut held = Column::values(
        0,
        true,
        Family::Category,
        Some(Layer {
            values_rel: None,
            values: base_values,
            dict: None,
        }),
        postings,
        Route::Scan,
    );
    let codes: Vec<u16> = extent.iter().map(|&(_, code)| code).collect();
    let entities: Vec<u32> = extent.iter().map(|&(entity, _)| entity).collect();
    held.push_extent(
        "c",
        "extent-1",
        Arc::new(
            ValueColumn::partial(Codes::U16(codes.into()), Bitmap::of(&entities))
                .expect("one code per entity"),
        ),
        None,
    )
    .expect("the extent is disjoint from the base");
    let mut columns = FilterColumns::default();
    columns.columns.insert("c".to_string(), held);
    columns
}
