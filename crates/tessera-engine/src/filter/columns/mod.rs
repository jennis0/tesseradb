mod evaluate;
mod open;
pub(in crate::filter) mod successor;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use tessera_filter::{Codes, ColumnPostings, RecordStack, RecordValue, SortedDict, ValueColumn};

use super::declared::{Family, Placement};
use super::error::ComposeError;

/// One bundle's filter columns, keyed by declared column name — plus the record-blob stack,
/// which shares this type's lifecycle rather than its name.
///
/// Opened once per generation, not per request.
///
/// **Membership is not filterability.** The map holds every column the build wrote a value column
/// for — which includes a `visibility = "derived"` category that is not declared filterable, since
/// its postings are what `/v1/categories` derives value visibility from. [`FilterColumns::resolve`]
/// gates on a column's declared filterability rather than on presence, so such a column is refused
/// exactly as an undeclared one is: an *undeclared* column is a caller error, where an unresolvable
/// *value* is an empty operand (`filter-surface.md` §2.1).
///
/// **Why the record blob rides here.** The stack (records §3) is not a filter column — no query
/// ever reads it (records §3's rule: a column the scan reads is never compressed; the blob is
/// never read by a query) — but it is opened from the same manifests, at the same two sites, and
/// it belongs to the published prefix exactly as the value columns do: carried forward by every
/// flush successor, replaced whole at a fold's prefix rotation. Housing it here gives it that
/// lifecycle without a second prefix-tracking mechanism to keep correct; the alternative was a
/// parallel field threaded through every generation constructor for a reader only drill-down
/// takes.
pub struct FilterColumns {
    pub(in crate::filter) columns: BTreeMap<String, Column>,
    /// The evaluation space(s) each filterable column affords — including a rendered category
    /// with no entity-space layers at all, which [`FilterColumns::columns`] cannot represent.
    pub(in crate::filter) placements: BTreeMap<String, Placement>,
    /// The access mode this generation was opened with, so a successor composing a flush's
    /// extents opens them the same way. A generation that mapped its columns and read its
    /// successor's would be two cost models in one bundle.
    pub(in crate::filter) access: tessera_filter::Access,
    /// The record blob: the build's base (present iff the compiled schema has a blob-resident
    /// column) plus every flush extent the manifest names. Empty — zero layers — when neither
    /// exists, which answers `fields_of` with an ordinary absence.
    pub(in crate::filter) records: Arc<RecordStack>,
    /// The entity→term transpose: the build's base plus every flush extent the manifest names
    /// (contracts §2.4). It rides here for the record blob's reasons exactly — opened from the
    /// same manifests at the same two sites, carried forward by every flush successor, replaced
    /// whole at a fold's prefix rotation — and it is read by the same two callers a record is:
    /// the drill-down (intersected with the session's satisfied set, decision 0114) and the write
    /// path's join arm (`views.md` §4).
    pub(in crate::filter) entity_terms: Arc<tessera_store::EntityTermsStack>,
}

// Hand-written because `RecordStack` carries no `Debug` of its own (it is a stack of mapped
// artefacts); the columns and placements are the parts worth printing.
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

/// One indexed column's value layers, base first — a **read** view over what
/// [`FilterColumns::value_layers`] holds.
///
/// **The split into base and extents is the whole reason this type exists.** An artifact layer's
/// row-addressed membership is written over the *base* row space and survives a flush because an
/// append moves no bit it holds (`RowSpace::project_base`); the entities a flush published since sit
/// above that base and are exactly the ones the **extent** layers hold values for. So a reader that
/// wants both halves has to be able to ask for each, and one that asks for all of them at once —
/// which is what a fold's rewrite wants — takes both.
///
/// **Disjoint in entity space by I9**, so no entity has a value in two layers and the order they
/// are visited decides nothing — which is what makes the base/extent split a partition of the
/// column rather than a filter over it.
#[derive(Clone, Copy)]
pub struct ValueLayers<'a> {
    layers: &'a [Layer],
}

impl<'a> ValueLayers<'a> {
    /// The build's own column — the one whose entities the base row space covers. `None` where the
    /// column arrived entirely in flush extents, which is a column declared after the build.
    pub fn base(&self) -> Option<&'a ValueColumn> {
        self.layers
            .iter()
            .find(|layer| layer.values_rel.is_none())
            .map(|layer| layer.values.as_ref())
    }

    /// The flush extents, oldest first — the entities published since the base was written.
    pub fn extents(&self) -> impl Iterator<Item = &'a ValueColumn> {
        self.layers
            .iter()
            .filter(|layer| layer.values_rel.is_some())
            .map(|layer| layer.values.as_ref())
    }
}

/// A stack of zero layers — what a schema with no blob-resident column and no extents owns.
/// Infallible: `RecordStack::open` touches no file when given nothing to open.
pub(in crate::filter) fn empty_record_stack() -> RecordStack {
    RecordStack::open(None, &[], tessera_filter::Access::Read)
        .expect("a record stack over no layers opens without IO")
}

/// How a category operand is answered on one column — decided at open from the declaration alone.
///
/// **Not a tuning knob and not a per-request choice.** See this module's header and decision 0063:
/// the postings' work is a function of the value named, which is a disclosure under
/// `visibility = "derived"` and a published fact under `visibility = "public"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::filter) enum Route {
    /// Every operand is answered by scanning the value column, layer by layer.
    Scan,
    /// `eq` and `in` over the base are answered by intersecting the derived postings; every other
    /// operand, and every extent layer, is scanned.
    Postings,
}

/// One column as it is served: what the declaration says about it, and the layers it is served
/// from.
///
/// Built only by [`Column::values`] and [`Column::text`], which is what keeps the two kinds of
/// column from being written out half each.
#[derive(Debug, Clone)]
pub(in crate::filter) struct Column {
    /// This column's position in the manifest's `declared_scalars`.
    ///
    /// **An index internal, and the record blob's addressing key**: a blob row tags each field by
    /// this number rather than by name, so a route that must read a column's *value* out of the
    /// blob — the phrase verify — has no other way to pick its field out of a row. Never
    /// serialised anywhere; drill-down resolves the same tag against the same list.
    declared_index: usize,
    /// **Declared `index = true`.** A column may be held here without being filterable: a
    /// `visibility = "derived"` category owes membership postings whatever its `index` says
    /// (`filter-index.md` §2.3), and `/v1/categories` reads them from here. [`FilterColumns::resolve`]
    /// refuses such a column exactly as it refuses an undeclared one, so holding it opens no
    /// operand the schema did not declare.
    filterable: bool,
    /// The family whose rules this column's values are read by — carried so a layer's storage can
    /// be checked against its declaration rather than inferred from it. A `keyword` column's layers
    /// each owe a dictionary, and [`Column::push_extent`] refuses one that arrives without it:
    /// a `u32` ordinal column read as though it were a category's codes would answer every string
    /// predicate with the empty set, which under-reports silently rather than failing.
    family: Family,
    layers: ColumnLayers,
}

/// What a column is served from — a value column per layer, or a text index per layer. Never both
/// and never neither: the half a column does not have is absent rather than empty.
///
/// `Arc` per layer because a publication builds the next generation's columns from the live ones —
/// the base is a memory map of a multi-gigabyte file, and the flush that added one entity must not
/// re-open it.
#[derive(Debug, Clone)]
enum ColumnLayers {
    /// The build's base column, then one layer per flush that has published since (see this
    /// module's header), plus the derived postings where the column has them.
    Values {
        layers: Vec<Layer>,
        /// Every entity any layer holds a value for. Kept so a new extent's disjointness can be
        /// checked in one bitmap operation — see [`Column::push_extent`] — rather than trusted.
        covered: Bitmap,
        /// The base build's per-value postings, where the column has them: every category column
        /// whose vocabulary is `derived`, and every category column declared filterable.
        ///
        /// Held whatever the route, because the membership question `/v1/categories` asks is
        /// answered from these on a `derived` column that the *filter* route deliberately does not
        /// use them for.
        postings: Option<Arc<ColumnPostings>>,
        route: Route,
    },
    /// A `text` column's layers: the base build's index first, then one per flush extent, oldest
    /// first. A [`Layer`] is a value column and its dictionary, and this family has neither — its
    /// terms are postings and its prose is a blob row.
    ///
    /// **Disjoint in entity space by I9**, so a `match` unions across them and order decides
    /// nothing; there is no coverage check to keep, because an entity id is never reused and no
    /// two layers can hold the same entity's terms.
    Text {
        layers: Vec<TextLayer>,
        /// The analyser this column was **indexed** with, resolved from the manifest's recorded
        /// identity rather than from a default. A query is analysed with it, which is what makes
        /// the two token streams the same one.
        analyser: Arc<tessera_analyse::Analyser>,
    },
}

impl Column {
    /// A column served from value columns, holding its base alone — what a build's column opens
    /// as, and what a column declared at a running service opens as with no base at all
    /// (`ingest.md` §6.3). Every extent arrives through [`Column::push_extent`].
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

    /// A column served from a text index: its analyser, and the layers the manifest names — the
    /// base build's index first where there is one, then one per published extent.
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

    /// The value layers, base first — empty for a text column, which has none.
    pub(in crate::filter) fn value_layers(&self) -> &[Layer] {
        match &self.layers {
            ColumnLayers::Values { layers, .. } => layers,
            ColumnLayers::Text { .. } => &[],
        }
    }

    /// The text layers, oldest first — empty for a value column, which has none.
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

    /// The value layers, to be replaced by a coalesce — `None` for a text column.
    fn value_layers_mut(&mut self) -> Option<&mut Vec<Layer>> {
        match &mut self.layers {
            ColumnLayers::Values { layers, .. } => Some(layers),
            ColumnLayers::Text { .. } => None,
        }
    }

    /// The text layers, to be appended to by a flush or replaced by a coalesce — `None` for a
    /// value column.
    fn text_layers_mut(&mut self) -> Option<&mut Vec<TextLayer>> {
        match &mut self.layers {
            ColumnLayers::Text { layers, .. } => Some(layers),
            ColumnLayers::Values { .. } => None,
        }
    }

    /// Add one flush's extent, refusing an entity an earlier layer already holds a value for.
    ///
    /// **The refusal is what keeps a layered column a function.** Entity ids are permanent and
    /// issued from the high-water (**I9**), so an extent's entities belong to no earlier layer and
    /// the overlap is unreachable — which is exactly why it is checked here rather than reasoned
    /// about at the call site: if I9 ever failed, the symptom would be an entity matching two
    /// values at once and a filter naming either returning it, with nothing to notice.
    ///
    /// **A keyword extent must bring its own dictionary, and one that does not is refused rather
    /// than composed** ([`check_dictionary_pairing`]). Its values are ordinals into a dictionary
    /// this flush minted, so a layer without one has no reading at all: scanned as codes it would
    /// answer every string predicate with the empty set, and resolved against the base's keys it
    /// would return another value's entities.
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
/// it holds a value for.
///
/// **The three travel together because an ordinal is a position in *this* dictionary.** Postings
/// read against another layer's terms would answer every `match` from the wrong words with no
/// symptom, which is why the manifest names them as one record.
#[derive(Debug, Clone)]
struct TextLayer {
    dict: Arc<SortedDict>,
    postings: Arc<ColumnPostings>,
    /// The entities this layer holds a value for. **Stored rather than derived from the postings**:
    /// text that analyses to no terms — an empty string, a field of pure punctuation — carries a
    /// value and appears in no posting.
    ///
    /// **Read by the coalesce's replacement rule** ([`FilterColumns::with_coalesced`]): a
    /// coalesced layer must stand for exactly the entities its inputs did, and presence is the only
    /// thing that says so. Not read by the fold, which rebuilds the base by merging postings and
    /// subtracting the deleted set and needs no coverage; not by `match`, which unions across
    /// layers that are disjoint by **I9**.
    ///
    /// ⊘ **Empty for the base**, which writes no presence file — so it is a layer's coverage and
    /// not the column's. The day a text column gains a presence predicate the base owes one too
    /// ([#123](https://github.com/jennis0/tessera-index/issues/123)).
    present: Bitmap,
    /// The manifest path that named this layer, or `None` for the base build's index — the identity
    /// a coalesce or fold names a layer by, for [`Layer::values_rel`]'s reason.
    dict_rel: Option<String>,
}

/// One text layer's two halves, checked against each other before the layer is served.
///
/// **Posting *i* is term *i*'s, and nothing else says so.** A postings file short of its dictionary
/// answers `None` for every ordinal past the gap — `PostingsReader::posting_at` returns it rather
/// than refusing — so a truncated layer would report every word after the gap as carried by
/// nobody: an under-report with no symptom, which is the shape this codebase refuses everywhere
/// else. The fold makes the same check on its inputs before merging them, and a reader that did not
/// would be trusting an artefact the writer's own consumer will not.
impl TextLayer {
    /// A text column's **base** index, from the column's own directory: the token dictionary and
    /// the positional postings over it. Positional, not keyed: a token ordinal is a dense position
    /// in this dictionary, where a category's code is a scattered vocabulary entry (§2.5).
    ///
    /// The base writes no presence file of its own — the build writes none and the fold therefore
    /// writes none — so nothing here can say which entities carry a value. See [`TextLayer::present`].
    fn open_base(
        column: &str,
        dir: &Path,
        access: tessera_filter::Access,
    ) -> Result<TextLayer, ComposeError> {
        text_layer(
            SortedDict::open_dir(dir, access)?,
            ColumnPostings::open(&dir.join("postings.arrow"), access != tessera_filter::Access::Read)?,
            column,
            "base",
            Bitmap::new(),
            None,
        )
    }

    /// One **published** text layer, from the three files the manifest names it by — a flush's
    /// extent and a coalesce's replacement alike. `dict_rel` is the manifest's own path, which is
    /// the layer's identity.
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
/// **The path is carried because a coalesce replaces layers by name** (`filter-index.md` §5.2). A
/// `ValueColumn` has no identity of its own, so a pass that consumed eight of a column's extents
/// could not otherwise say which eight of the live generation's layers its output stands for —
/// and matching them by presence instead would be circular, since presence equality is exactly the
/// property [`FilterColumns::with_coalesced`] is checking.
///
/// `None` is the build's base column, which is named in `MANIFEST.files` rather than in
/// `attr_extents` and which no entity-space pass may take (`crate::coalesce`'s module doc).
#[derive(Debug, Clone)]
pub(in crate::filter) struct Layer {
    pub(in crate::filter) values_rel: Option<String>,
    pub(in crate::filter) values: Arc<ValueColumn>,
    /// The layer's own sorted dictionary, for a `keyword` column and nothing else.
    ///
    /// **Held beside the values it numbers, because an ordinal has no meaning apart from them.**
    /// The base and every extent mint their own ordinals, so pairing them here is what makes
    /// "resolve the needle in *this* layer's dictionary, then scan *this* layer's ordinals" the
    /// only expressible order of operations — the alternative, a dictionary per column, would read
    /// an extent's ordinals against the base's keys and recolour the layer with no symptom
    /// (`tessera_filter::SortedDict`'s module doc; records §7).
    pub(in crate::filter) dict: Option<Arc<SortedDict>>,
}

/// The request path's two modes, and only those: `MappedSequential` is the fold's and is
/// deliberately unreachable from here (decision 0052).
fn request_access(mmap: bool) -> tessera_filter::Access {
    if mmap {
        tessera_filter::Access::Mapped
    } else {
        tessera_filter::Access::Read
    }
}

/// A record-blob open failure, in the refusal this opener speaks. Fail-closed either way:
/// a missing, short or malformed layer refuses the whole open (records §3), never "those
/// entities have no record".
fn record_open_error(e: tessera_filter::RecordError) -> ComposeError {
    match e {
        tessera_filter::RecordError::Io(io) => ComposeError::Io(io),
        malformed => ComposeError::RecordUnreadable(malformed.to_string()),
    }
}

/// The one rule under which a layer may join a column: a keyword layer brings its own dictionary,
/// and no other family's layer brings one.
///
/// **This is where "no ordinal is resolved against a dictionary other than the one that minted
/// it" is enforced**, for every route a layer takes into the live composition — a flush's extent
/// ([`FilterColumns::compose`]) and a coalesce's replacement ([`FilterColumns::with_coalesced`])
/// both pass through here, and both then build one [`Layer`] from the pair. A [`Layer`] is the
/// only thing a scan or a drill-down reads a dictionary from, and it is constructed nowhere a
/// dictionary could arrive apart from the values it numbers. A keyword layer without one is
/// refused rather than scanned as codes, which would answer every string predicate with the empty
/// set; a dictionary on another family's layer is refused because the caller and the schema
/// disagree about what the values are.
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
    /// The record-blob stack this prefix serves drill-down from — see this type's doc for why it
    /// lives here. Empty (zero layers) when the schema has no blob-resident column and no flush
    /// has published an extent.
    pub fn records(&self) -> &RecordStack {
        &self.records
    }

    /// The entity→term transpose this prefix answers a label question from — see this type's doc
    /// for why it lives here.
    pub fn entity_terms(&self) -> &tessera_store::EntityTermsStack {
        &self.entity_terms
    }

    /// The access mode every layer of this generation was opened with — what a re-derived stack
    /// must be opened with too, or a publication would swap a mapped reader for a read one under
    /// a live request.
    pub(crate) fn access(&self) -> tessera_filter::Access {
        self.access
    }

    /// How many layers the record blob's stack holds — the base, if the schema has a
    /// blob-resident column, plus one per published extent. Operator- and test-facing: nothing on
    /// the wire carries it, and it is how a coalesce's publication can be asserted to have
    /// *shrunk* the live stack rather than merely the manifest.
    pub fn record_layers(&self) -> usize {
        self.records.layer_count()
    }

    /// How many record-blob rows drill-down and the join rule have decoded through this
    /// generation's stack — [`RecordStack::reads`]. Test-facing: the reader that answers a
    /// column's absence from the segment schema is asserted never to move it (`ingest.md` §6.3).
    pub fn record_reads(&self) -> u64 {
        self.records.reads()
    }

    /// The route affordances of one filterable column, or `None` where the column is not
    /// filterable at all — the same distinction [`FilterColumns::resolve`] refuses on.
    pub fn placement(&self, column: &str) -> Option<Placement> {
        self.placements.get(column).copied()
    }

    /// Does any **flushed** `text` layer of `column` hold prose for `entity`?
    ///
    /// **The scoped cell arm's fail-closed source for a text family** (`views.md` §5, decision
    /// 0116). A text column has no per-entity value to read back — its extent is a dictionary,
    /// postings and this presence bitmap — so the cell arm cannot compare a supplied string with
    /// a stored one across a flush boundary. What it can establish is *occupancy*, and that is
    /// what this answers: a joining row supplying prose for a cell some layer already holds prose
    /// for is refused rather than admitted unchecked, because admitting it would write a second
    /// text layer stamped with the same view and `match` unions across layers silently.
    ///
    /// ⊘ **The base is not covered, and cannot be**: it writes no presence file at all
    /// ([`TextLayer::present`]'s own marker, issue #123), so a cell whose only prose came from the
    /// build reads as empty here. Closing that needs the base's presence bitmap, not a change to
    /// this rule.
    pub(crate) fn text_present(&self, column: &str, entity: u32) -> bool {
        let Some(column) = self.columns.get(column) else {
            return false;
        };
        column
            .text_layers()
            .iter()
            .any(|layer| layer.present.contains(entity))
    }

    /// The entity-space value `column` stores for `entity`, at its storage type, or `None` where
    /// no layer holds one — drill-down's entity-space home (records §3).
    ///
    /// **Every column with a value column answers, filterable or not**: a `derived` category
    /// with neither flag still stores its codes here, and the caller has already established the
    /// *item* visible, which is exactly the membership condition §3.3 derives value visibility
    /// from — a visible entity carrying the value is the witness that offers it.
    ///
    /// The slot arithmetic is the presence-rank rule of `filter-index.md` §2.1, computed through
    /// the column's own public surface: the count of present entities strictly below this one is
    /// its slot, for a universal column (where it degenerates to the entity id) and a partial one
    /// alike. O(containers below the entity) per read — drill-down cadence, never per mark.
    pub(crate) fn stored_value(&self, column: &str, entity: u32) -> Option<RecordValue> {
        let column = self.columns.get(column)?;
        let probe = Bitmap::of(&[entity]);
        for layer in column.value_layers() {
            let values = &layer.values;
            if values.present_in(&probe).is_empty() {
                continue;
            }
            // **A keyword's ordinal never crosses the trust boundary**, so drill-down is served the
            // key it names rather than the number (records §4.3; **I10**). Decoded against *this*
            // layer's dictionary, which is the only one that numbers it. A dictionary that refuses
            // leaves the field with no value: under-reporting, which narrows, where the alternative
            // would publish an index internal.
            if let Some(dict) = &layer.dict {
                let ordinal = values.value_of(entity)?.raw();
                let mut scratch = Vec::new();
                return dict
                    .key_of(ordinal, &mut scratch)
                    .ok()
                    .map(|key| RecordValue::Utf8(key.to_string()));
            }
            // The layers are disjoint in entity space (I9, checked at compose), so the first
            // layer holding the entity is the only one.
            let slot = values
                .present_in(&Bitmap::from_range(0..entity))
                .cardinality() as usize;
            let read = match values.codes() {
                Codes::U8(v) => RecordValue::U8(v[slot]),
                Codes::U16(v) => RecordValue::U16(v[slot]),
                Codes::U32(v) => RecordValue::U32(v[slot]),
                Codes::U64(v) => RecordValue::U64(v[slot]),
                Codes::I8(v) => RecordValue::I8(v[slot]),
                Codes::I16(v) => RecordValue::I16(v[slot]),
                Codes::I32(v) => RecordValue::I32(v[slot]),
                Codes::I64(v) => RecordValue::I64(v[slot]),
                Codes::F32(v) => RecordValue::F32(v[slot]),
                Codes::F64(v) => RecordValue::F64(v[slot]),
            };
            return Some(read);
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// How many layers this generation serves `column` from — the base plus one per live extent.
    ///
    /// **This is where the coalesce's bound is realised, and the manifest is not.** A pass that
    /// edited the manifest without replacing the live layers would be a bound that arrives only at
    /// the next restart, which is exactly what `tessera_engine`'s tier-list assertion exists to
    /// catch on the delta axis.
    pub fn layer_count(&self, column: &str) -> Option<usize> {
        self.columns.get(column).map(|c| c.value_layers().len())
    }

    /// How many **text** layers this generation serves `column` from — the base plus one per live
    /// extent. A separate count because a text column has no value layers at all: its index is
    /// postings over terms and it owes no value column ([`FilterColumns::open`]), so
    /// [`FilterColumns::layer_count`] answers `1` for one however many flushes have published.
    ///
    /// Read for the coalesce's bound, on [`FilterColumns::layer_count`]'s argument: a pass that
    /// edited the manifest without replacing the live layers is a bound that arrives at the next
    /// restart.
    pub fn text_layer_count(&self, column: &str) -> Option<usize> {
        self.columns.get(column).map(|c| c.text_layers().len())
    }

    /// **One indexed column's value layers, for a reader that wants the values themselves rather
    /// than a predicate over them** — the attribute-predicate membership, which *is* the column
    /// (`design/artifact-serving-at-scale.md` §5.1).
    ///
    /// `None` where the column is not held here at all, which is every undeclared name and every
    /// column with no entity-space storage. A caller that finds none serves the layer with no
    /// column, which is the fail-closed answer: no artifact of it is a candidate anywhere.
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

    /// **Drill-down is served the key, never the ordinal** (records §4.3, **I10**): an ordinal is a
    /// position in one layer's dictionary and an index internal, so it does not cross the trust
    /// boundary. Decoded against the layer that holds the entity, which is what makes the two
    /// layers' clashing numbering come out right.
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
