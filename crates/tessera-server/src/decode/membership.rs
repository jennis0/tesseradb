use std::sync::Arc;

use arrow::array::{Array, UInt32Array};
use tessera_engine::member_key::MemberColumn;
use tessera_lifecycle::{BatchArtifacts, BatchEdge, BatchMembership};

use super::DecodeError;

/// A column named for a declared layer, by the layer's own `name`, and what its cells mean.
pub(super) struct MembershipColumn<'a> {
    layer: &'a str,
    /// The key of the view the column's artifacts are in, on a group-scoped layer.
    view: Option<String>,
    /// The column's keys, read by the rule a build reads a member table by.
    column: MemberColumn<'a>,
}

/// Reads one column named for a layer.
pub(super) fn membership_column<'a>(
    body_name: &str,
    name: &'a str,
    column: &'a Arc<dyn Array>,
    declaration: &tessera_types::layer::LayerDeclaration,
    view_in: &dyn Fn(&str) -> Option<String>,
) -> Result<MembershipColumn<'a>, DecodeError> {
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
    let column = MemberColumn::new(column.as_ref(), declaration.list_meaning())
        .map_err(|e| DecodeError(format!("{body_name}: column '{name}' names a layer and {e}")))?;
    Ok(MembershipColumn {
        layer: name,
        view,
        column,
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
    /// One row's cells of one membership column. `levels` is the batch's `level` column, which
    /// places a scalar key. `offset` is where this record batch's rows start in the request, since
    /// an Arrow IPC stream may carry several.
    pub(super) fn read(
        &mut self,
        body_name: &str,
        membership: &MembershipColumn<'_>,
        levels: Option<&UInt32Array>,
        row: usize,
        offset: usize,
    ) -> Result<(), DecodeError> {
        let column = &membership.column;
        let entries = column.entries(row).map_err(|e| {
            DecodeError(format!(
                "{body_name}: row {}, column '{}' {e}",
                offset + row,
                membership.layer
            ))
        })?;
        let Some(entries) = entries else {
            return Ok(());
        };
        // Each entry keeps its position, so its level is not found by searching for its key: a
        // lineage may name one key twice.
        let keys: Vec<Option<(u32, String)>> = entries
            .enumerate()
            .map(|(position, index)| {
                column
                    .keys
                    .member_key_at(index)
                    .map(|key| (column.level_at(levels, row, position), key))
            })
            .collect();
        for (level, key) in keys.iter().flatten() {
            let at = self
                .rows_of
                .entry((
                    membership.layer.to_string(),
                    *level,
                    membership.view.clone(),
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
                    membership.layer.to_string(),
                    *level,
                    membership.view.clone(),
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
