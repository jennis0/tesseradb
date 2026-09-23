use std::sync::Arc;

use arrow::array::Array;
use tessera_engine::member_key::{KeyColumn, KEY_TYPES};
use tessera_lifecycle::{BatchArtifacts, BatchEdge, BatchMembership};

use super::DecodeError;

/// A column named for a declared layer, and what its cells mean.
///
/// **Named for the layer, exactly as an attribute column is named for the attribute** — the
/// declaration's own `name`, never an acquisition-side spelling. `fields` on `[layer.members]` maps
/// a *file's* column name onto the canonical meaning and is build-only for that reason
/// (`configuration.md` §2): a deployment that never builds has no file to map from, and a name a
/// running node had to be told about could not be checked against anything.
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
    /// A `List`, whose rows may differ in length — which is what a lineage is.
    Variable(&'a arrow::array::ListArray),
    /// A `FixedSizeList`, every row of the arity its own type states.
    Fixed(&'a arrow::array::FixedSizeListArray),
}

impl KeyCells<'_> {
    /// Whether the cells are lists — a scalar carries one artifact per row and has no arity to
    /// disagree with a declaration.
    fn is_list(&self) -> bool {
        !matches!(self, KeyCells::Scalar)
    }

    /// The range of the keys one row occupies, or `None` where the row named no artifact at all.
    ///
    /// **A null cell and an empty list are the whole row's "in no artifact"**, which is the scalar
    /// rule applied to a cell that holds no key: a point may be in no artifact at any resolution,
    /// and a clusterer that emitted nothing for it is the ordinary way of saying so.
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

/// Read one column named for a layer, at the shapes that layer may carry.
///
/// **Two refusals, and both are the declaration and the data disagreeing about arity.** A
/// `FixedSizeList` states its length in its own type, so a levelled layer's is checked once here; a
/// nested layer's lineage is as deep as each point's own branch and has no fixed arity at all. A
/// plain list states its length a row at a time, and that check is in the row loop — reading the
/// type alone would refuse every producer whose Arrow binding writes a plain list, which is most of
/// them.
pub(super) fn membership_column<'a>(
    body_name: &str,
    name: &'a str,
    column: &'a Arc<dyn Array>,
    declaration: &tessera_types::layer::LayerDeclaration,
    view_in: &dyn Fn(&str) -> Option<String>,
) -> Result<MembershipColumn<'a>, DecodeError> {
    use arrow::array::{FixedSizeListArray, ListArray};
    use arrow::datatypes::DataType;

    // A predicate layer's membership is *evaluated* per request, so there is nothing for a column
    // to say: a stored answer beside a live predicate is what the artifact store refuses a
    // publication for, and this is the same refusal one step earlier, where the batch can still be
    // rejected without effect.
    if declaration.membership != tessera_types::layer::MembershipSource::Enumerated {
        return Err(DecodeError(format!(
            "{body_name}: column '{name}' names a layer whose membership is evaluated per \
             request rather than enumerated — there is no stored membership for a point to join, \
             and one written beside the predicate would diverge from it at the first write"
        )));
    }

    // A group-scoped layer's keys are a set per view, so the batch's view says which set.
    let view = match declaration.scope.group() {
        None => None,
        Some(group) => Some(view_in(group).ok_or_else(|| {
            DecodeError(format!(
                "{body_name}: column '{name}' names a layer scoped to group '{group}', whose keys \
                 are a set per view, and this batch names no view of it; name one in \
                 x-tessera-view"
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
                        "{body_name}: column '{name}' is a fixed-size list of {size} and that \
                         layer is declared nested, whose lineage is as deep as each point's own \
                         branch — a fixed arity is one entry per level, which is the stacked and \
                         tiered shape"
                    )))
                }
                tessera_types::layer::ListMeaning::Levelled { levels, .. }
                    if *size as usize != levels =>
                {
                    return Err(DecodeError(format!(
                        "{body_name}: column '{name}' is a fixed-size list of {size} and that \
                         layer declares {levels} levels. Entry k is the artifact at level k, so \
                         the two counts are one number written twice"
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
            "{body_name}: column '{name}' names a layer and carries {:?}; a member key is {KEY_TYPES}, \
             an integer key being read as its decimal spelling, so `3` and \"3\" name one artifact",
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

/// What one batch's membership columns said, gathered as the executor takes it.
///
/// **Keyed by `(layer, level, key)` and carrying row positions**, because a batch's entity ids do
/// not exist until its commit window closes. The executor resolves the key to an ordinal at
/// admission and turns the positions into entities after the assignment — see
/// [`tessera_lifecycle::BatchMembership`].
#[derive(Default)]
pub(super) struct MembershipTally {
    rows_of: std::collections::BTreeMap<(String, u32, Option<String>, String), Vec<u32>>,
    edges: std::collections::BTreeSet<(String, u32, Option<String>, String, String)>,
}

impl MembershipTally {
    /// One row's cells of one membership column.
    ///
    /// `offset` is where this batch's rows start in the request's own numbering, since an Arrow IPC
    /// stream may carry several record batches and the executor indexes one flat list of rows.
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
        // **The declaration and the data must agree — where the data is a list.** A stacked or
        // tiered layer's list is one entry per declared level, that being what makes entry k mean
        // level k, so a row of any other length is a lineage against a levelled declaration and
        // guessing which of the two the caller meant would store a hierarchy they did not write.
        //
        // **A scalar is not a short list**: it names one artifact at level 0, which is exactly what
        // a member table with no `level` column means on a levelled layer. Applying the arity check
        // to it would make the two entry points read one spelling two ways, which is the drift this
        // whole column is written against.
        if let Some(levels) = column.meaning.arity().filter(|_| column.cells.is_list()) {
            if entries.len() != levels {
                return Err(DecodeError(format!(
                    "{body_name}: column '{}' names {} artifacts at row {row} and the layer \
                     declares {levels} levels. A stacked or tiered layer's column is one entry \
                     per level, nullable where the point is in no artifact at that resolution",
                    column.layer,
                    entries.len(),
                )));
            }
        }
        // **Each entry carries its own position**, so the edge below reads the child's level off
        // the entry rather than searching for its key — a lineage may legitimately name one key
        // twice, and a search would then charge the edge to the wrong level.
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
            // A row naming one artifact twice — a lineage that repeats a key — joins it once.
            let index = (offset + row) as u32;
            if at.last() != Some(&index) {
                at.push(index);
            }
        }
        if column.meaning.declares_edges() {
            // The adjacency is `tessera_types::layer::parent_edges`' — the same function a build
            // reads a member table's list column by, which is what keeps the two entry points from
            // inferring different trees from one file.
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
