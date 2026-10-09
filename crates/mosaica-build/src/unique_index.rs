//! The build's unique indexes: one per column declared `unique`, written as base runs of
//! `(key, entity)` entries under `partitions/<p>/entities/unique/<attribute>/`.
//!
//! Each column's values are pushed through a bucketed spill sort under a share of the build's
//! memory budget ([`tessera_store::unique::UniqueSpill`]), which reports every key more than one
//! entity holds as it writes. The identity rule has refused every row that would give an item a
//! value another holds ([`crate::ids`]), so a key held twice here is a fault in the build and
//! refuses it.

use std::path::{Path, PathBuf};

use tessera_spatial::ScalarValue;
use tessera_store::unique::{index_dir_rel, key_of, KeyKind, UniqueKey, UniqueSpill, WrittenUniqueRun};

use crate::column::EntityColumn;
use crate::config::{Attribute, Schema};
use crate::error::{BuildError, Result};

/// The most of the build's memory budget one column's spill holds, as a share and a ceiling, and
/// the least it is given however little its stage has left.
const UNIQUE_BUDGET_SHARE: u64 = 4;
const UNIQUE_BUDGET_MIN: u64 = 4 << 20;
const UNIQUE_BUDGET_MAX: u64 = 4 << 30;

/// What a walk over a unique column's values hands each present value to: the entity, the key,
/// and the text of a keyword.
type Visit<'v> = dyn FnMut(u32, UniqueKey, Option<&str>) -> Result<()> + 'v;

/// Where one unique column's values are read from, entity by entity, in ascending order.
pub(crate) enum UniqueSource<'a> {
    /// The column held in entity order.
    Column(&'a EntityColumn),
    /// A string column the join spilled as record-blob extents.
    Extents(&'a crate::extents::OpenExtents),
    /// The linear build's items in entity order, and the column's position in their scalars.
    Items(&'a [tessera_spatial::TilerItem], usize),
}

impl UniqueSource<'_> {
    /// Every present value, as `(entity, key, text)`; `text` is the value for a keyword and
    /// `None` otherwise.
    fn walk(
        &self,
        attribute: &Attribute,
        visit: &mut Visit<'_>,
    ) -> Result<()> {
        let keyed = |value: &ScalarValue| key_of(attribute.ty, value);
        match self {
            UniqueSource::Column(column) => {
                for entity in column.present_entities() {
                    match column.str_at(entity) {
                        Some(text) => visit(entity as u32, UniqueKey::keyword(text), Some(text))?,
                        None => {
                            if let Some(key) = keyed(&column.value_at(entity)) {
                                visit(entity as u32, key, None)?;
                            }
                        }
                    }
                }
                Ok(())
            }
            UniqueSource::Extents(extents) => extents.for_each_live_record(&mut |entity, text| {
                visit(entity, UniqueKey::keyword(text), Some(text))
            }),
            UniqueSource::Items(items, column) => {
                for (entity, item) in items.iter().enumerate() {
                    let Some(value) = item.scalars.get(*column) else {
                        continue;
                    };
                    let Some(key) = keyed(value) else {
                        continue;
                    };
                    let text = match value {
                        ScalarValue::Utf8(text) => Some(text.as_str()),
                        _ => None,
                    };
                    visit(entity as u32, key, text)?;
                }
                Ok(())
            }
        }
    }
}

/// The spill's memory for one column under a build budget of `budget` bytes, where the rest of
/// its stage leaves `room`: the room, up to the share and the ceiling.
pub(crate) fn spill_budget(budget: u64, room: u64) -> usize {
    (budget / UNIQUE_BUDGET_SHARE)
        .min(UNIQUE_BUDGET_MAX)
        .min(room)
        .max(UNIQUE_BUDGET_MIN) as usize
}

/// Write the index of every unique column in `schema`, reading each through `source_of` and sorting
/// it in `spill_bytes` of memory, and fail on a column holding one value for two entities. Returns
/// each column's runs, in key order with disjoint ranges; a column with no values has none. The
/// runs are fsynced.
pub(crate) fn write_unique_indexes<'a>(
    prefix_dir: &Path,
    partition: &str,
    schema: &Schema,
    source_of: impl Fn(usize) -> UniqueSource<'a>,
    scratch: &Path,
    spill_bytes: usize,
) -> Result<Vec<(String, Vec<WrittenUniqueRun>)>> {
    let mut out = Vec::new();
    for (column, attribute) in schema.attributes.iter().enumerate() {
        if !attribute.unique {
            continue;
        }
        let kind = KeyKind::of(attribute.ty).ok_or_else(|| {
            BuildError::Invalid(format!(
                "attribute '{}': `unique` applies to keyword, integer and timestamp columns",
                attribute.name
            ))
        })?;
        let source = source_of(column);
        let store = |e: tessera_store::StoreError| BuildError::Invalid(e.to_string());
        let mut spill = UniqueSpill::create(kind, scratch, spill_bytes).map_err(store)?;
        source.walk(attribute, &mut |entity, key, _| spill.push(key, entity).map_err(store))?;
        let dir = prefix_dir.join(index_dir_rel(partition, &attribute.name));
        std::fs::create_dir_all(&dir).map_err(|e| BuildError::io(&dir, e))?;
        let mut duplicates = 0u64;
        let runs = spill
            .finish(&dir, "base", |_| duplicates += 1)
            .map_err(store)?;
        if duplicates > 0 {
            for run in &runs {
                let _ = std::fs::remove_file(&run.path);
            }
            return Err(BuildError::Invalid(format!(
                "attribute '{}': {duplicates} value(s) are held by more than one item, and the \
                 identity rule refuses every row that would give an item a value another holds. \
                 The build is at fault; report it with the declaration",
                attribute.name
            )));
        }
        let paths: Vec<PathBuf> = runs.iter().map(|run| run.path.clone()).collect();
        tessera_store::fsync_written(&paths).map_err(store)?;
        out.push((attribute.name.clone(), runs));
    }
    Ok(out)
}
