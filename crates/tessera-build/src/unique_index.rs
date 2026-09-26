//! The build's unique indexes: one per column declared `unique`, written as base runs of
//! `(key, entity)` entries under `partitions/<p>/entities/unique/<attribute>/`.
//!
//! Each column's values are pushed through a bucketed spill sort under a share of the build's
//! memory budget ([`tessera_store::unique::UniqueSpill`]), which reports every key more than one
//! entity holds as it writes. A duplicate refuses the build, naming how many values are held twice
//! and up to ten of them. A keyword's key is a hash, so its text is found by walking the column's
//! values a second time, which happens only on the way to a refusal.

use std::path::{Path, PathBuf};

use tessera_spatial::ScalarValue;
use tessera_store::unique::{
    duplicates_message, index_dir_rel, key_of, KeyKind, UniqueKey, UniqueSpill, WrittenUniqueRun,
};

use crate::column::EntityColumn;
use crate::config::{Attribute, Schema};
use crate::error::{BuildError, Result};

/// The share of the build's memory budget one column's spill holds, and its floor and ceiling.
const UNIQUE_BUDGET_SHARE: u64 = 4;
const UNIQUE_BUDGET_MIN: u64 = 64 << 20;
const UNIQUE_BUDGET_MAX: u64 = 4 << 30;

/// How many values a refusal names.
const EXAMPLES: usize = 10;

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
        visit: &mut dyn FnMut(u32, UniqueKey, Option<&str>) -> Result<()>,
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

/// The spill's memory for one column under a build budget of `budget` bytes.
pub(crate) fn spill_budget(budget: u64) -> usize {
    (budget / UNIQUE_BUDGET_SHARE).clamp(UNIQUE_BUDGET_MIN, UNIQUE_BUDGET_MAX) as usize
}

/// Write the index of every unique column in `schema`, reading each through `source_of`, and
/// refuse a column holding one value for two entities. Returns each column's runs, in key order
/// with disjoint ranges; a column with no values has none. The runs are fsynced.
pub(crate) fn write_unique_indexes<'a>(
    prefix_dir: &Path,
    partition: &str,
    schema: &Schema,
    source_of: impl Fn(usize) -> UniqueSource<'a>,
    scratch: &Path,
    budget: u64,
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
        let mut spill = UniqueSpill::create(kind, scratch, spill_budget(budget)).map_err(store)?;
        source.walk(attribute, &mut |entity, key, _| spill.push(key, entity).map_err(store))?;
        let dir = prefix_dir.join(index_dir_rel(partition, &attribute.name));
        std::fs::create_dir_all(&dir).map_err(|e| BuildError::io(&dir, e))?;
        let mut duplicates = 0u64;
        let mut named: Vec<UniqueKey> = Vec::new();
        let runs = spill
            .finish(&dir, "base", |d| {
                duplicates += 1;
                if named.len() < EXAMPLES {
                    named.push(d.key);
                }
            })
            .map_err(store)?;
        if duplicates > 0 {
            for run in &runs {
                let _ = std::fs::remove_file(&run.path);
            }
            let examples = describe(&source, attribute, kind, &named)?;
            return Err(BuildError::Invalid(duplicates_message(
                &attribute.name,
                duplicates,
                &examples,
            )));
        }
        let paths: Vec<PathBuf> = runs.iter().map(|run| run.path.clone()).collect();
        tessera_store::fsync_written(&paths).map_err(store)?;
        out.push((attribute.name.clone(), runs));
    }
    Ok(out)
}

/// The values of `keys`, as a refusal names them.
fn describe(
    source: &UniqueSource<'_>,
    attribute: &Attribute,
    kind: KeyKind,
    keys: &[UniqueKey],
) -> Result<Vec<String>> {
    let mut texts: std::collections::HashMap<UniqueKey, String> = std::collections::HashMap::new();
    if kind == KeyKind::Keyword {
        source.walk(attribute, &mut |_, key, text| {
            if let Some(text) = text {
                if keys.contains(&key) && !texts.contains_key(&key) {
                    texts.insert(key, format!("'{text}'"));
                }
            }
            Ok(())
        })?;
    }
    Ok(keys
        .iter()
        .map(|key| match (*key, kind) {
            (UniqueKey::Int(k), KeyKind::Unsigned) => k.to_string(),
            (UniqueKey::Int(k), _) => tessera_store::key_index::signed_value(k).to_string(),
            (UniqueKey::Keyword(_), _) => texts.get(key).cloned().unwrap_or_default(),
        })
        .collect())
}
