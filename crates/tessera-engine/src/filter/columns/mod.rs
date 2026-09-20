mod evaluate;
mod open;
pub(in crate::filter) mod successor;

pub use open::PartitionExtents;
pub(crate) use open::{open_entity_terms_stack, open_record_stack};

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use tessera_filter::{ColumnPostings, RecordStack, RecordValue, SortedDict, ValueColumn};

use super::declared::{Family, Placement};
use super::error::ComposeError;

/// One bundle's filter columns, keyed by declared column name, plus the record-blob and
/// entity-terms stacks, which share this type's lifecycle. Opened once per generation, not per
/// request.
///
/// Holds every column the build wrote a value column for, including a `visibility = "derived"`
/// category that is not declared filterable, refused by [`FilterColumns::resolve`] exactly as an
/// undeclared column: a caller error, where an unresolvable value is an empty operand.
pub struct FilterColumns {
    pub(in crate::filter) columns: BTreeMap<String, Column>,
    /// The evaluation space(s) each filterable column affords, including a rendered category with
    /// no entity-space layers, which [`FilterColumns::columns`] cannot represent.
    pub(in crate::filter) placements: BTreeMap<String, Placement>,
    /// The access mode this generation was opened with, so a successor opens its own extents the
    /// same way.
    pub(in crate::filter) access: tessera_filter::Access,
    /// The record blob's base, present when the schema has a blob-resident column, plus every
    /// flush extent named.
    pub(in crate::filter) records: Arc<RecordStack>,
    /// The entity to term transpose's base plus every flush extent named, read by drill-down and
    /// the write path's join arm.
    pub(in crate::filter) entity_terms: Arc<tessera_store::EntityTermsStack>,
}

// Hand-written since `RecordStack` carries no `Debug` of its own.
impl std::fmt::Debug for FilterColumns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterColumns")
            .field("columns", &self.columns)
            .field("placements", &self.placements)
            .finish_non_exhaustive()
    }
}

impl Default for FilterColumns {
    fn default() -> Self {
        FilterColumns {
            columns: BTreeMap::new(),
            placements: BTreeMap::new(),
            access: tessera_filter::Access::Read,
            records: Arc::new(empty_record_stack()),
            entity_terms: Arc::new(tessera_store::EntityTermsStack::empty()),
        }
    }
}

/// One indexed column's value layers, base first: a read view over what
/// [`FilterColumns::value_layers`] holds. Disjoint in entity space, so no entity has a value in
/// two layers and visiting order decides nothing.
#[derive(Clone, Copy)]
pub struct ValueLayers<'a> {
    layers: &'a [Layer],
}

impl<'a> ValueLayers<'a> {
    /// The build's own column, `None` for a column declared entirely after the build.
    pub fn base(&self) -> Option<&'a ValueColumn> {
        self.layers
            .iter()
            .find(|layer| layer.values_rel.is_none())
            .map(|layer| layer.values.as_ref())
    }

    /// The flush extents, oldest first.
    pub fn extents(&self) -> impl Iterator<Item = &'a ValueColumn> {
        self.layers
            .iter()
            .filter(|layer| layer.values_rel.is_some())
            .map(|layer| layer.values.as_ref())
    }
}

/// A stack of zero layers, infallible since `RecordStack::open` touches no file when given
/// nothing to open.
pub(in crate::filter) fn empty_record_stack() -> RecordStack {
    RecordStack::open(None, &[], tessera_filter::Access::Read)
        .expect("a record stack over no layers opens without IO")
}

/// How a category operand is answered on one column, decided at open from the declaration alone,
/// never per request or from a statistic: the postings' work is a function of the value named,
/// which is a disclosure under `visibility = "derived"` and a published fact under
/// `visibility = "public"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::filter) enum Route {
    /// Every operand is answered by scanning the value column, layer by layer.
    Scan,
    /// `eq` and `in` over the base are answered by intersecting the derived postings; every other
    /// operand, and every extent layer, is scanned.
    Postings,
}

/// One column as it is served: what the declaration says about it, and the layers it is served
/// from. Built only by [`Column::values`] and [`Column::text`].
#[derive(Debug, Clone)]
pub(in crate::filter) struct Column {
    /// This column's position in the manifest's `declared_scalars`, the record blob's addressing
    /// key a blob row tags each field by. Never serialised.
    declared_index: usize,
    /// Declared `index = true`. A column may be held without being filterable: a
    /// `visibility = "derived"` category owes membership postings whatever its `index` says.
    filterable: bool,
    /// The family whose rules this column's values are read by, carried so a layer's storage can
    /// be checked against its declaration (see [`check_dictionary_pairing`]).
    family: Family,
    layers: ColumnLayers,
}

/// What a column is served from: a value column per layer, or a text index per layer, never both
/// and never neither. `Arc` per layer because a publication builds the next generation's columns
/// from the live ones, and the base is a memory map of a multi-gigabyte file.
#[derive(Debug, Clone)]
enum ColumnLayers {
    /// The base column, then one layer per flush since, plus the derived postings if owed.
    Values {
        layers: Vec<Layer>,
        /// Every entity any layer holds a value for, checked in [`Column::push_extent`].
        covered: Bitmap,
        /// The base build's per-value postings; `/v1/categories` reads membership from these
        /// even on a `derived` column the filter route ignores them for.
        postings: Option<Arc<ColumnPostings>>,
        route: Route,
    },
    /// A `text` column's layers: the base build's index first, then one per flush extent. A
    /// [`Layer`] is a value column and its dictionary, and this family has neither: its terms are
    /// postings and its prose is a blob row. Disjoint in entity space, so `match` unions across
    /// them with no coverage check to keep.
    Text {
        layers: Vec<TextLayer>,
        /// The analyser this column was indexed with. A query is analysed with it, keeping the
        /// two token streams the same one.
        analyser: Arc<tessera_analyse::Analyser>,
    },
}

impl Column {
    /// A column served from value columns, holding its base alone. Every extent arrives through
    /// [`Column::push_extent`].
    pub(in crate::filter) fn values(
        declared_index: usize,
        filterable: bool,
        family: Family,
        base: Option<Layer>,
        postings: Option<Arc<ColumnPostings>>,
        route: Route,
    ) -> Column {
        let covered = base
            .as_ref()
            .map(|layer| layer.values.present())
            .unwrap_or_default();
        Column {
            declared_index,
            filterable,
            family,
            layers: ColumnLayers::Values {
                layers: base.into_iter().collect(),
                covered,
                postings,
                route,
            },
        }
    }

    /// A column served from a text index.
    fn text(
        declared_index: usize,
        filterable: bool,
        analyser: Arc<tessera_analyse::Analyser>,
        layers: Vec<TextLayer>,
    ) -> Column {
        Column {
            declared_index,
            filterable,
            family: Family::Text,
            layers: ColumnLayers::Text { layers, analyser },
        }
    }

    /// The value layers, base first, empty for a text column.
    pub(in crate::filter) fn value_layers(&self) -> &[Layer] {
        match &self.layers {
            ColumnLayers::Values { layers, .. } => layers,
            ColumnLayers::Text { .. } => &[],
        }
    }

    /// The text layers, oldest first, empty for a value column.
    fn text_layers(&self) -> &[TextLayer] {
        match &self.layers {
            ColumnLayers::Text { layers, .. } => layers,
            ColumnLayers::Values { .. } => &[],
        }
    }

    /// The base build's derived postings, where this column has them.
    pub(in crate::filter) fn postings(&self) -> Option<&ColumnPostings> {
        match &self.layers {
            ColumnLayers::Values { postings, .. } => postings.as_deref(),
            ColumnLayers::Text { .. } => None,
        }
    }

    /// The value layers, for a coalesce to replace, `None` for a text column.
    fn value_layers_mut(&mut self) -> Option<&mut Vec<Layer>> {
        match &mut self.layers {
            ColumnLayers::Values { layers, .. } => Some(layers),
            ColumnLayers::Text { .. } => None,
        }
    }

    /// The text layers, for a flush to append to or a coalesce to replace, `None` for a value
    /// column.
    fn text_layers_mut(&mut self) -> Option<&mut Vec<TextLayer>> {
        match &mut self.layers {
            ColumnLayers::Text { layers, .. } => Some(layers),
            ColumnLayers::Values { .. } => None,
        }
    }

    /// Add one flush's extent, refusing an entity an earlier layer already holds a value for:
    /// layers are disjoint in entity space, checked here rather than assumed. A keyword extent
    /// must bring its own dictionary ([`check_dictionary_pairing`]): scanned as codes without one
    /// it would answer every string predicate with the empty set.
    pub(in crate::filter) fn push_extent(
        &mut self,
        column: &str,
        values_rel: &str,
        extent: Arc<ValueColumn>,
        dict: Option<Arc<SortedDict>>,
    ) -> Result<(), ComposeError> {
        check_dictionary_pairing(self.family, column, dict.is_some())?;
        let ColumnLayers::Values {
            layers, covered, ..
        } = &mut self.layers
        else {
            return Err(ComposeError::UnknownColumn {
                column: column.to_string(),
            });
        };
        let present = extent.present();
        if covered.and_cardinality(&present) != 0 {
            return Err(ComposeError::Overlap {
                column: column.to_string(),
            });
        }
        *covered |= present;
        layers.push(Layer {
            values_rel: Some(values_rel.to_string()),
            values: extent,
            dict,
        });
        Ok(())
    }
}

/// One `text` layer: its own dictionary, its own postings over that dictionary, and the entities
/// it holds a value for. The three travel together: an ordinal is a position in this dictionary,
/// so postings read against another layer's terms would answer from the wrong words.
#[derive(Debug, Clone)]
struct TextLayer {
    dict: Arc<SortedDict>,
    postings: Arc<ColumnPostings>,
    /// The entities this layer holds a value for, stored rather than derived from the postings:
    /// text analysing to no terms, an empty string or pure punctuation, carries a value and
    /// appears in no posting. Read by the coalesce's replacement rule
    /// ([`FilterColumns::with_coalesced`]) to check a replacement stands for exactly the entities
    /// its inputs did. Empty for the base, which writes no presence file, so this is a layer's
    /// coverage, not the column's.
    present: Bitmap,
    /// The manifest path that named this layer, or `None` for the base build's index.
    dict_rel: Option<String>,
}

/// One text layer's two halves, checked against each other before the layer is served: a
/// postings file short of its dictionary would report every word past the gap as carried by
/// nobody, an under-report with no symptom.
impl TextLayer {
    /// A text column's base index: the token dictionary and the positional postings over it. The
    /// base writes no presence file of its own, so nothing here can say which entities carry a
    /// value. See [`TextLayer::present`].
    fn open_base(
        column: &str,
        dir: &Path,
        access: tessera_filter::Access,
    ) -> Result<TextLayer, ComposeError> {
        text_layer(
            SortedDict::open_dir(dir, access)?,
            ColumnPostings::open(
                &dir.join("postings.arrow"),
                access != tessera_filter::Access::Read,
            )?,
            column,
            "base",
            Bitmap::new(),
            None,
        )
    }

    /// One published text layer, from the three files the manifest names it by. `dict_rel` is the
    /// manifest's own path, the layer's identity.
    fn open(
        column: &str,
        dict_rel: &str,
        dict: &Path,
        postings: &Path,
        presence: &Path,
        access: tessera_filter::Access,
    ) -> Result<TextLayer, ComposeError> {
        text_layer(
            SortedDict::open(dict, access)?,
            ColumnPostings::open(postings, access != tessera_filter::Access::Read)?,
            column,
            dict_rel,
            Bitmap::deserialize::<croaring::Portable>(&std::fs::read(presence)?),
            Some(dict_rel.to_string()),
        )
    }
}

fn text_layer(
    dict: SortedDict,
    postings: ColumnPostings,
    column: &str,
    which: &str,
    present: Bitmap,
    dict_rel: Option<String>,
) -> Result<TextLayer, ComposeError> {
    if dict.len() != postings.record_count() {
        return Err(ComposeError::TermsAndPostingsDisagree {
            column: column.to_string(),
            layer: which.to_string(),
            terms: dict.len(),
            postings: postings.record_count(),
        });
    }
    Ok(TextLayer {
        dict: Arc::new(dict),
        postings: Arc::new(postings),
        present,
        dict_rel,
    })
}

/// One layer of a column, and the manifest entry it came from.
///
/// The path is carried because a coalesce replaces layers by name, since a `ValueColumn` has no
/// identity of its own. `None` is the build's base column, named in `MANIFEST.files` rather than
/// in `attr_extents`.
#[derive(Debug, Clone)]
pub(in crate::filter) struct Layer {
    pub(in crate::filter) values_rel: Option<String>,
    pub(in crate::filter) values: Arc<ValueColumn>,
    /// The layer's own sorted dictionary, for a `keyword` column and nothing else, held beside
    /// the values it numbers: the base and every extent mint their own ordinals, so a dictionary
    /// per column would read an extent's ordinals against the base's keys and recolour the layer
    /// with no symptom.
    pub(in crate::filter) dict: Option<Arc<SortedDict>>,
}

/// The request path's two modes, and only those: `MappedSequential` is the fold's and is never
/// returned here.
fn request_access(mmap: bool) -> tessera_filter::Access {
    if mmap {
        tessera_filter::Access::Mapped
    } else {
        tessera_filter::Access::Read
    }
}

/// A record-blob open failure, in the refusal this opener speaks. Fail-closed either way: a
/// missing, short or malformed layer refuses the whole open, never "those entities have no
/// record".
fn record_open_error(e: tessera_filter::RecordError) -> ComposeError {
    match e {
        tessera_filter::RecordError::Io(io) => ComposeError::Io(io),
        malformed => ComposeError::RecordUnreadable(malformed.to_string()),
    }
}

/// The one rule under which a layer may join a column, for a flush's extent
/// ([`FilterColumns::compose`]) and a coalesce's replacement ([`FilterColumns::with_coalesced`])
/// alike: a keyword layer brings its own dictionary, refused rather than scanned as codes if it
/// does not, and no other family's layer brings one, refused because the caller and the schema
/// would disagree about what the values are.
fn check_dictionary_pairing(
    family: Family,
    column: &str,
    has_dict: bool,
) -> Result<(), ComposeError> {
    match (family == Family::Keyword, has_dict) {
        (true, false) => Err(ComposeError::KeywordWithoutDictionary {
            column: column.to_string(),
        }),
        (false, true) => Err(ComposeError::DictionaryOnOtherFamily {
            column: column.to_string(),
        }),
        _ => Ok(()),
    }
}

impl FilterColumns {
    /// The record-blob stack this prefix serves drill-down from. Empty when the schema has no
    /// blob-resident column and no flush has published an extent.
    pub fn records(&self) -> &RecordStack {
        &self.records
    }

    /// The entity→term transpose this prefix answers a label question from.
    pub fn entity_terms(&self) -> &tessera_store::EntityTermsStack {
        &self.entity_terms
    }

    /// The access mode every layer of this generation was opened with.
    pub(crate) fn access(&self) -> tessera_filter::Access {
        self.access
    }

    /// How many layers the record blob's stack holds: the base, if owed, plus one per extent.
    pub fn record_layers(&self) -> usize {
        self.records.layer_count()
    }

    /// How many record-blob rows drill-down and the join rule have decoded through this stack.
    pub fn record_reads(&self) -> u64 {
        self.records.reads()
    }

    /// The route affordances of one filterable column, or `None` where the column is not
    /// filterable at all.
    pub fn placement(&self, column: &str) -> Option<Placement> {
        self.placements.get(column).copied()
    }

    /// Does any flushed `text` layer of `column` hold prose for `entity`? The scoped cell join's
    /// fail-closed source for a text family, since it cannot compare a supplied string with a
    /// stored one across a flush boundary; it checks occupancy instead. The base is not covered,
    /// since it writes no presence file, so a cell whose only prose came from the build reads as
    /// empty here.
    pub(crate) fn text_present(&self, column: &str, entity: u32) -> bool {
        let Some(column) = self.columns.get(column) else {
            return false;
        };
        column
            .text_layers()
            .iter()
            .any(|layer| layer.present.contains(entity))
    }

    /// The entity-space value `column` stores for `entity`, or `None` where no layer holds one:
    /// drill-down's entity-space home. Every column with a value column answers, filterable or
    /// not: a `derived` category with neither flag still stores its codes here.
    pub(crate) fn stored_value(&self, column: &str, entity: u32) -> Option<RecordValue> {
        let column = self.columns.get(column)?;
        for layer in column.value_layers() {
            let values = &layer.values;
            // The layers are disjoint in entity space, so the first layer holding the entity is
            // the only one.
            let read = match values.record_value_of(entity) {
                Some(read) => read,
                None => continue,
            };
            // A keyword's ordinal never crosses the trust boundary: drill-down is served the key
            // it names, decoded against this layer's dictionary, the only one that numbers it. A
            // dictionary that refuses leaves the field with no value, which narrows.
            if let Some(dict) = &layer.dict {
                let ordinal = values.value_of(entity)?.raw();
                let mut scratch = Vec::new();
                return dict
                    .key_of(ordinal, &mut scratch)
                    .ok()
                    .map(|key| RecordValue::Utf8(key.to_string()));
            }
            return Some(read);
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// How many layers this generation serves `column` from: the base plus one per live extent.
    pub fn layer_count(&self, column: &str) -> Option<usize> {
        self.columns.get(column).map(|c| c.value_layers().len())
    }

    /// How many text layers this generation serves `column` from, separate from
    /// [`FilterColumns::layer_count`] because a text column has no value layers at all.
    pub fn text_layer_count(&self, column: &str) -> Option<usize> {
        self.columns.get(column).map(|c| c.text_layers().len())
    }

    /// One indexed column's value layers, for a reader that wants the values themselves rather
    /// than a predicate over them. `None` where the column is not held here at all: every
    /// undeclared name and every column with no entity-space storage.
    pub(crate) fn value_layers(&self, column: &str) -> Option<ValueLayers<'_>> {
        self.columns.get(column).map(|column| ValueLayers {
            layers: column.value_layers(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_support::*;

    /// Drill-down is served the key, never the ordinal: decoded against the layer that holds the
    /// entity, so two layers' clashing numbering still comes out right.
    #[test]
    fn drill_down_reads_the_key_not_the_ordinal() {
        let columns = keyword_column(
            "sub",
            vec![
                (None, universal(&[0, 1]), dict(&["alpha", "beta"])),
                (
                    Some("extents/f1.arrow"),
                    partial(&[10], &[0]),
                    dict(&["beta", "gamma"]),
                ),
            ],
        );
        assert_eq!(
            columns.stored_value("sub", 1),
            Some(RecordValue::Utf8("beta".into()))
        );
        assert_eq!(
            columns.stored_value("sub", 10),
            Some(RecordValue::Utf8("beta".into()))
        );
        assert_eq!(columns.stored_value("sub", 7), None);
    }
}
