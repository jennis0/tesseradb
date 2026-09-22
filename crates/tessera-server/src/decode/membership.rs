use std::sync::Arc;

use arrow::array::Array;
use tessera_lifecycle::{BatchArtifacts, BatchEdge, BatchMembership};

use crate::error::ApiError;

/// A column named for a declared layer, and what its cells mean.
///
/// **Named for the layer, exactly as an attribute column is named for the attribute** — the
/// declaration's own `name`, never an acquisition-side spelling. `fields` on `[layer.members]` maps
/// a *file's* column name onto the canonical meaning and is build-only for that reason
/// (`configuration.md` §2): a deployment that never builds has no file to map from, and a name a
/// running node had to be told about could not be checked against anything.
pub(super) struct MembershipColumn<'a> {
    layer: &'a str,
    meaning: tessera_types::layer::ListMeaning,
    cells: KeyCells<'a>,
}

/// A key column's shape: one artifact per row, or a list of them.
enum KeyCells<'a> {
    Scalar(&'a dyn Array),
    /// A `List`, whose rows may differ in length — which is what a lineage is.
    Variable(&'a arrow::array::ListArray),
    /// A `FixedSizeList`, every row of the arity its own type states.
    Fixed(&'a arrow::array::FixedSizeListArray),
}

impl KeyCells<'_> {
    /// The element array a row's entries are read out of — the column itself where it is a scalar.
    fn values(&self) -> &dyn Array {
        match self {
            KeyCells::Scalar(array) => *array,
            KeyCells::Variable(list) => {
                let values: &Arc<dyn Array> = list.values();
                values.as_ref()
            }
            KeyCells::Fixed(list) => {
                let values: &Arc<dyn Array> = list.values();
                values.as_ref()
            }
        }
    }

    /// Whether the cells are lists — a scalar carries one artifact per row and has no arity to
    /// disagree with a declaration.
    fn is_list(&self) -> bool {
        !matches!(self, KeyCells::Scalar(_))
    }

    /// The range of `values()` one row occupies, or `None` where the row named no artifact at all.
    ///
    /// **A null cell and an empty list are the whole row's "in no artifact"**, which is the scalar
    /// rule applied to a cell that holds no key: a point may be in no artifact at any resolution,
    /// and a clusterer that emitted nothing for it is the ordinary way of saying so.
    fn entries(&self, row: usize) -> Option<std::ops::Range<usize>> {
        let (start, end) = match self {
            KeyCells::Scalar(_) => (row, row + 1),
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

/// The key one cell names, or `None` where it names no artifact.
///
/// **Text or an integer, and `null` or `-1` means this point is in no artifact**
/// (`artifacts-from-points.md` §2). An integer key is read as its decimal spelling, so `3` and `"3"`
/// name one artifact — the rule is [`tessera_types::layer::integer_key`]'s, which is also what a
/// build reads a member table's key column by, because a membership spelled two ways must not
/// resolve two ways.
fn member_key_at(values: &dyn Array, index: usize) -> Option<String> {
    use arrow::array::{
        Int16Array, Int32Array, Int64Array, Int8Array, StringArray, UInt16Array, UInt32Array,
        UInt64Array, UInt8Array,
    };
    if values.is_null(index) {
        return None;
    }
    let any = values.as_any();
    let integer: i128 = if let Some(a) = any.downcast_ref::<StringArray>() {
        return Some(a.value(index).to_string());
    } else if let Some(a) = any.downcast_ref::<Int8Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<Int16Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<Int32Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<Int64Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<UInt8Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<UInt16Array>() {
        a.value(index) as i128
    } else if let Some(a) = any.downcast_ref::<UInt32Array>() {
        a.value(index) as i128
    } else {
        // The last arm is `u64` and the fallthrough is unreachable: `membership_column` refuses any
        // other element type before a row is read.
        any.downcast_ref::<UInt64Array>()?.value(index) as i128
    };
    tessera_types::layer::integer_key(integer)
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
) -> Result<MembershipColumn<'a>, ApiError> {
    use arrow::array::{FixedSizeListArray, ListArray};
    use arrow::datatypes::DataType;

    // A predicate layer's membership is *evaluated* per request, so there is nothing for a column
    // to say: a stored answer beside a live predicate is what the artifact store refuses a
    // publication for, and this is the same refusal one step earlier, where the batch can still be
    // rejected without effect.
    if declaration.membership != tessera_types::layer::MembershipSource::Enumerated {
        return Err(ApiError::Contract(format!(
            "{body_name}: column '{name}' names a layer whose membership is evaluated per \
             request rather than enumerated — there is no stored membership for a point to join, \
             and one written beside the predicate would diverge from it at the first write"
        )));
    }

    let meaning = declaration.list_meaning();
    let cells = match column.data_type() {
        DataType::List(_) => KeyCells::Variable(
            column
                .as_any()
                .downcast_ref::<ListArray>()
                .expect("a List column downcasts to a ListArray"),
        ),
        DataType::FixedSizeList(_, size) => {
            match meaning {
                tessera_types::layer::ListMeaning::Lineage => {
                    return Err(ApiError::Contract(format!(
                        "{body_name}: column '{name}' is a fixed-size list of {size} and that \
                         layer is declared nested, whose lineage is as deep as each point's own \
                         branch — a fixed arity is one entry per level, which is the stacked and \
                         tiered shape"
                    )))
                }
                tessera_types::layer::ListMeaning::Levelled { levels, .. }
                    if *size as usize != levels =>
                {
                    return Err(ApiError::Contract(format!(
                        "{body_name}: column '{name}' is a fixed-size list of {size} and that \
                         layer declares {levels} levels. Entry k is the artifact at level k, so \
                         the two counts are one number written twice"
                    )))
                }
                _ => {}
            }
            KeyCells::Fixed(
                column
                    .as_any()
                    .downcast_ref::<FixedSizeListArray>()
                    .expect("a FixedSizeList column downcasts to a FixedSizeListArray"),
            )
        }
        _ => KeyCells::Scalar(column.as_ref()),
    };
    let element = cells.values().data_type().clone();
    if !matches!(
        element,
        DataType::Utf8
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
    ) {
        return Err(ApiError::Contract(format!(
            "{body_name}: column '{name}' names a layer and carries {element:?}; a member key is \
             text or an integer — an integer key is read as its decimal spelling, so `3` and \
             \"3\" name one artifact"
        )));
    }
    Ok(MembershipColumn {
        layer: name,
        meaning,
        cells,
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
    rows_of: std::collections::BTreeMap<(String, u32, String), Vec<u32>>,
    edges: std::collections::BTreeSet<(String, u32, String, String)>,
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
    ) -> Result<(), ApiError> {
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
                return Err(ApiError::Contract(format!(
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
        let values = column.cells.values();
        let keys: Vec<Option<(u32, String)>> = entries
            .enumerate()
            .map(|(position, index)| {
                member_key_at(values, index).map(|key| (column.meaning.level_of(position), key))
            })
            .collect();
        for (level, key) in keys.iter().flatten() {
            let at = self
                .rows_of
                .entry((column.layer.to_string(), *level, key.clone()))
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
                .map(|((layer, level, key), rows)| BatchMembership {
                    layer,
                    level,
                    key,
                    rows,
                })
                .collect(),
            edges: self
                .edges
                .into_iter()
                .map(|(layer, level, child, parent)| BatchEdge {
                    layer,
                    level,
                    child,
                    parent,
                })
                .collect(),
        }
    }
}
