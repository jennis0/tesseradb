use std::sync::Arc;

use arrow::array::Array;
use tessera_engine::member_key::{KeyColumn, KEY_TYPES};
use tessera_lifecycle::{BatchArtifacts, BatchEdge, BatchMembership};

use super::DecodeError;

/// A column named for a declared layer, by the layer's own `name`, and what its cells mean.
pub(super) struct MembershipColumn<'a> {
    layer: &'a str,
    /// The key of the view the column's artifacts are in, on a group-scoped layer.
    view: Option<String>,
    meaning: tessera_types::layer::ListMeaning,
    cells: KeyCells<'a>,
    /// The column's keys, or a list's elements, read by the rule a build reads a member table by.
    keys: KeyColumn<'a>,
}

/// A key column's shape: one artifact per row, or a list of them.
enum KeyCells<'a> {
    Scalar,
    /// A `List`, whose rows may differ in length, as a lineage's do.
    Variable(&'a arrow::array::ListArray),
    /// A `FixedSizeList`, every row of the arity its own type states.
    Fixed(&'a arrow::array::FixedSizeListArray),
}

impl KeyCells<'_> {
    fn is_list(&self) -> bool {
        !matches!(self, KeyCells::Scalar)
    }

    /// The range of the keys one row occupies, or `None` where the row names no artifact: a null
    /// cell and an empty list both mean the point is in no artifact.
    fn entries(&self, row: usize) -> Option<std::ops::Range<usize>> {
        let (start, end) = match self {
            KeyCells::Scalar => (row, row + 1),
            KeyCells::Variable(list) => {
                if list.is_null(row) {
                    return None;
                }
                let offsets = list.value_offsets();
                (offsets[row] as usize, offsets[row + 1] as usize)
            }
            KeyCells::Fixed(list) => {
                if list.is_null(row) {
                    return None;
                }
                let start = list.value_offset(row) as usize;
                (start, start + list.value_length() as usize)
            }
        };
        (start != end).then_some(start..end)
    }
}

/// Reads one column named for a layer. A `FixedSizeList` states its arity in its type, so it is
/// checked once here, and a nested layer refuses one; a plain list's arity is checked row by row
/// in [`MembershipTally::read`].
pub(super) fn membership_column<'a>(
    body_name: &str,
    name: &'a str,
    column: &'a Arc<dyn Array>,
    declaration: &tessera_types::layer::LayerDeclaration,
    view_in: &dyn Fn(&str) -> Option<String>,
) -> Result<MembershipColumn<'a>, DecodeError> {
    use arrow::array::{FixedSizeListArray, ListArray};
    use arrow::datatypes::DataType;

    // A predicate layer's membership is evaluated per request, so there is nothing to store.
    if declaration.membership != tessera_types::layer::MembershipSource::Enumerated {
        return Err(DecodeError(format!(
            "{body_name}: column '{name}' names a layer whose membership is evaluated per \
             request, so it has no stored membership to write; leave the column out"
        )));
    }

    // A group-scoped layer's keys are a set per view, so the batch's view says which set.
    let view = match declaration.scope.group() {
        None => None,
        Some(group) => Some(view_in(group).ok_or_else(|| {
            DecodeError(format!(
                "{body_name}: column '{name}' names a layer scoped to group '{group}' and this \
                 batch names no view of it; name one in x-tessera-view"
            ))
        })?),
    };
    let meaning = declaration.list_meaning();
    let (cells, values): (KeyCells<'a>, &'a dyn Array) = match column.data_type() {
        DataType::List(_) => {
            let list = column
                .as_any()
                .downcast_ref::<ListArray>()
                .expect("a List column downcasts to a ListArray");
            (KeyCells::Variable(list), list.values().as_ref())
        }
        DataType::FixedSizeList(_, size) => {
            match meaning {
                tessera_types::layer::ListMeaning::Lineage => {
                    return Err(DecodeError(format!(
                        "{body_name}: column '{name}' is a fixed-size list of {size} but that \
                         layer is declared nested; send each point's lineage as a variable-length \
                         list"
                    )))
                }
                tessera_types::layer::ListMeaning::Levelled { levels, .. }
                    if *size as usize != levels =>
                {
                    return Err(DecodeError(format!(
                        "{body_name}: column '{name}' is a fixed-size list of {size} but that \
                         layer declares {levels} levels; send one entry per level"
                    )))
                }
                _ => {}
            }
            let list = column
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .expect("a FixedSizeList column downcasts to a FixedSizeListArray");
            (KeyCells::Fixed(list), list.values().as_ref())
        }
        _ => (KeyCells::Scalar, column.as_ref()),
    };
    let keys = KeyColumn::new(values).ok_or_else(|| {
        DecodeError(format!(
            "{body_name}: column '{name}' names a layer and carries {:?}; send its keys as \
             {KEY_TYPES}",
            values.data_type()
        ))
    })?;
    Ok(MembershipColumn {
        layer: name,
        view,
        meaning,
        cells,
        keys,
    })
}

/// What one batch's membership columns said, keyed by layer, level, view and key, with row
/// positions rather than entities, which do not exist until the batch's commit window closes.
#[derive(Default)]
pub(super) struct MembershipTally {
    rows_of: std::collections::BTreeMap<(String, u32, Option<String>, String), Vec<u32>>,
    edges: std::collections::BTreeSet<(String, u32, Option<String>, String, String)>,
}

impl MembershipTally {
    /// One row's cells of one membership column. `offset` is where this record batch's rows start
    /// in the request, since an Arrow IPC stream may carry several.
    pub(super) fn read(
        &mut self,
        body_name: &str,
        column: &MembershipColumn<'_>,
        row: usize,
        offset: usize,
    ) -> Result<(), DecodeError> {
        let Some(entries) = column.cells.entries(row) else {
            return Ok(());
        };
        // A list on a levelled layer has one entry per declared level, so another length is
        // refused rather than guessed. A scalar is not a short list: it names one artifact at
        // level 0, as a member table with no `level` column does.
        if let Some(levels) = column.meaning.arity().filter(|_| column.cells.is_list()) {
            if entries.len() != levels {
                return Err(DecodeError(format!(
                    "{body_name}: column '{}' names {} artifacts at row {row} but the layer \
                     declares {levels} levels; send one entry per level, null where the point is \
                     in no artifact at that level",
                    column.layer,
                    entries.len(),
                )));
            }
        }
        // Each entry keeps its position, so its level is not found by searching for its key: a
        // lineage may name one key twice.
        let keys: Vec<Option<(u32, String)>> = entries
            .enumerate()
            .map(|(position, index)| {
                column
                    .keys
                    .member_key_at(index)
                    .map(|key| (column.meaning.level_of(position), key))
            })
            .collect();
        for (level, key) in keys.iter().flatten() {
            let at = self
                .rows_of
                .entry((
                    column.layer.to_string(),
                    *level,
                    column.view.clone(),
                    key.clone(),
                ))
                .or_default();
            // A row naming one artifact twice joins it once.
            let index = (offset + row) as u32;
            if at.last() != Some(&index) {
                at.push(index);
            }
        }
        if column.meaning.declares_edges() {
            // The same adjacency function a build reads a member table's list column by.
            for ((_, parent), (level, child)) in tessera_types::layer::parent_edges(&keys) {
                self.edges.insert((
                    column.layer.to_string(),
                    *level,
                    column.view.clone(),
                    child.clone(),
                    parent.clone(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn into_artifacts(self) -> BatchArtifacts {
        BatchArtifacts {
            memberships: self
                .rows_of
                .into_iter()
                .map(|((layer, level, view, key), rows)| BatchMembership {
                    layer,
                    level,
                    view,
                    key,
                    rows,
                })
                .collect(),
            edges: self
                .edges
                .into_iter()
                .map(|(layer, level, view, child, parent)| BatchEdge {
                    layer,
                    level,
                    view,
                    child,
                    parent,
                })
                .collect(),
        }
    }
}
