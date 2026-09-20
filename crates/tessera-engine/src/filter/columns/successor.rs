use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use tessera_filter::{ColumnPostings, RecordExtentPaths, RecordStack, SortedDict, ValueColumn};

use super::open::{open_scoped_column, runtime_layers};
use super::{
    check_dictionary_pairing, record_open_error, text_layer, unknown_filter_column, FilterColumns,
    Layer,
};
use crate::filter::{
    owes_value_column, scoped_column_name, scoped_has_value_column, scoped_is_filterable, Family,
    Placement, PIN,
};

/// The three files one published text extent names, resolved to paths.
#[derive(Debug, Clone)]
pub struct TextExtentPaths {
    pub column: String,
    /// **The manifest path, which is the layer's identity** — what `text_extents` lists and what a
    /// later coalesce names its inputs by. Distinct from `dict` below, which is that path resolved
    /// against the prefix directory: a composition storing the resolved one would leave the two
    /// producers of a layer (a flush's, and `open`'s at startup) naming the same layer differently,
    /// and a coalesce's lookup would find the layer after a restart and miss it after a flush.
    pub dict_rel: String,
    pub dict: std::path::PathBuf,
    pub postings: std::path::PathBuf,
    pub presence: std::path::PathBuf,
}

/// One column's window of extents, and the coalesced extent that replaces them.
///
/// Named by the manifest's own paths on both sides, which is what makes the replace ABA-safe
/// against the flushes that published while the pass ran: a `seg_id` and the paths derived from it
/// are never reused (contracts §2.1), so a path still listed at publication is still the same bytes.
#[derive(Debug, Clone)]
pub struct CoalescedWindow {
    pub column: String,
    /// The consumed extents' values paths, as `attr_extents` names them.
    pub consumed: Vec<String>,
    /// The coalesced extent's values path.
    pub values_rel: String,
    pub values: Arc<ValueColumn>,
    /// The dictionary the coalesced values are ordinals into — a keyword column's merged
    /// dictionary, `None` for every other family. It arrives beside the values it numbers, as a
    /// flush's [`PublishedExtent`] carries its own, and [`FilterColumns::with_coalesced`] installs
    /// the two as one [`Layer`] or refuses: a keyword window without one has no reading, and a
    /// dictionary on another family's window means the pass and the schema disagree about what
    /// the values are.
    pub dict: Option<Arc<SortedDict>>,
}

/// One text column's window of extents, and the coalesced extent that replaces them.
///
/// Named by dictionary path on both sides — a text layer's identity, and the never-reused one
/// [`CoalescedWindow`] takes for its own reason. The replacement carries its three files rather
/// than an opened layer because a text layer is composed from paths wherever it enters, the flush's
/// extents included ([`FilterColumns::with_extents`]).
#[derive(Debug, Clone)]
pub struct CoalescedTextWindow {
    /// The consumed extents' dictionary paths, as `text_extents` names them.
    pub consumed: Vec<String>,
    /// The replacement, named exactly as a flush's extent is — the column included.
    pub paths: TextExtentPaths,
}

/// One flushed extent, as publication hands it over: the column it extends, the prefix-relative
/// path of its values, the opened column, and — for a keyword — the dictionary those values are
/// ordinals into.
///
/// **The dictionary travels with the values or not at all**, which is why this is one tuple rather
/// than two arguments that could disagree: an extent's ordinals are positions in its own
/// dictionary and name nothing against any other (`records-and-search.md` §4.3), so composition
/// refuses a half. On disc the same pairing is `AttrExtent`'s single record.
pub type PublishedExtent = (String, String, Arc<ValueColumn>, Option<Arc<SortedDict>>);

impl FilterColumns {
    /// Add one flush's extent to the column it names, refusing a name this composition does not
    /// hold a value column for — see [`Column::push_extent`] for what the column itself refuses.
    pub(in crate::filter) fn compose(
        &mut self,
        column: &str,
        values_rel: &str,
        extent: Arc<ValueColumn>,
        dict: Option<Arc<SortedDict>>,
    ) -> std::io::Result<()> {
        let Some(held) = self.columns.get_mut(column) else {
            return Err(unknown_filter_column(column));
        };
        held.push_extent(column, values_rel, extent, dict)
    }

    /// This generation's columns with an attribute column declared at a running service added,
    /// at its position in the served schema (`ingest.md` §1.3, §6.3): an empty stack the next
    /// flush's extent composes onto, and the placement its flags afford. A column with no
    /// entity-space home (rendered or blob-resident and not indexed) takes a placement or nothing.
    pub(crate) fn with_runtime_column(
        &self,
        scalar: &tessera_store::manifest::DeclaredScalar,
        declared_index: usize,
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    ) -> std::io::Result<FilterColumns> {
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records: Arc::clone(&self.records),
            entity_terms: Arc::clone(&self.entity_terms),
        };
        let family = Family::of(scalar);
        let row = scalar.render && family.reaches_hot_column();
        let entity = if family == Family::Text {
            scalar.index
        } else {
            owes_value_column(scalar, vocabularies) && (scalar.index || row)
        };
        if row || entity {
            next.placements.insert(
                scalar.name.clone(),
                Placement {
                    entity,
                    row,
                    family,
                },
            );
        }
        if let Some(layers) = runtime_layers(scalar, declared_index, vocabularies)? {
            next.columns.insert(scalar.name.clone(), layers);
        }
        Ok(next)
    }

    /// This generation's columns with a **newly based** group-scoped column opened onto them —
    /// the first flush of a view a family had no column for (`views.md` §5).
    ///
    /// **Applied before the extents compose, and that order is the whole of it.** A flush of a
    /// view created since the build writes the family's base and its own extent in one unit; the
    /// extent composes onto a column, so the column has to exist first. A `(column, view)` pair
    /// this generation already holds is a no-op rather than a refusal — a re-publication reaching
    /// the same state — because the base is written once and named by its files, not by a counter.
    pub fn with_scoped_columns(
        &self,
        partition_dir: &Path,
        columns: &[(String, String, tessera_types::view::ViewIncarnation)],
        scoped: &[tessera_store::manifest::ScopedScalar],
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
        mmap: bool,
    ) -> std::io::Result<FilterColumns> {
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records: Arc::clone(&self.records),
            entity_terms: Arc::clone(&self.entity_terms),
        };
        for (column, view, incarnation) in columns {
            let Some(family) = scoped.iter().find(|f| f.name == *column) else {
                return std::io::Result::Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "a flush wrote a base for the group-scoped column family '{column}' under \
                         view '{view}', which this bundle does not declare"
                    ),
                ));
            };
            if !scoped_is_filterable(family) && !scoped_has_value_column(family) {
                continue;
            }
            if next.columns.contains_key(&scoped_column_name(column, view)) {
                continue;
            }
            let (name, placement, layers) = open_scoped_column(
                partition_dir,
                family,
                view,
                *incarnation,
                vocabularies,
                mmap,
            )?;
            next.placements.insert(name.clone(), placement);
            next.columns.insert(name, layers);
        }
        Ok(next)
    }

    /// This generation's columns with every group-scoped column of `views` removed: the
    /// successor generation's, after a drop. A column left standing is the one
    /// [`FilterColumns::with_scoped_columns`] would find present when the key is created again.
    pub(crate) fn without_scoped_columns(&self, views: &[String]) -> FilterColumns {
        let dead = |name: &String| {
            name.split_once(PIN)
                .is_some_and(|(_, view)| views.iter().any(|dropped| dropped == view))
        };
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records: Arc::clone(&self.records),
            entity_terms: Arc::clone(&self.entity_terms),
        };
        next.columns.retain(|name, _| !dead(name));
        next.placements.retain(|name, _| !dead(name));
        next
    }

    /// This generation's columns with one flush's extents added — the successor generation's.
    ///
    /// Cheap by construction: the base columns are `Arc`s, so a flush that published one entity
    /// clones pointers rather than re-opening a memory-mapped column per declared attribute. Each
    /// extent is `(column, values path, opened column)`, the path being what the manifest names it
    /// by and what a later coalesce replaces it by.
    ///
    /// A keyword column's extent carries the dictionary the flush minted beside the values it
    /// numbers, so the pair composes as one — see [`FilterColumns::compose`], which refuses either
    /// half without the other. Reopening the generation from the manifest reaches the same state,
    /// [`FilterColumns::open`] taking each extent's dictionary from `AttrExtent::dict`.
    pub fn with_extents(
        &self,
        extents: &[PublishedExtent],
        records: &[RecordExtentPaths],
        entity_terms: &[tessera_store::EntityTermsExtentPaths],
        texts: &[TextExtentPaths],
    ) -> std::io::Result<FilterColumns> {
        // The record blob's extent composes here for the same reason a filter extent does: the
        // manifest entry makes the bytes reachable to a *reopen*, and this process serves from the
        // stack it holds. A flush that published one and did not compose it would leave every
        // entity it flushed with its blob-resident fields silently absent from drill-down until the
        // next fold — an entity in no layer being the ordinary `Ok(None)`.
        let records = if records.is_empty() {
            Arc::clone(&self.records)
        } else {
            Arc::new(
                self.records
                    .with_extents(records, self.access)
                    .map_err(record_open_error)?,
            )
        };
        // The transpose's extent composes here for the record blob's reason, plus one of its own:
        // a flush's labels that no live stack holds leave the join rule's label arm unable to
        // compare against the batch that just landed, which is the arm's whole point.
        let entity_terms =
            if entity_terms.is_empty() {
                Arc::clone(&self.entity_terms)
            } else {
                Arc::new(self.entity_terms.with_extents(entity_terms).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?)
            };
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records,
            entity_terms,
        };
        // A text extent appends a layer: its own dictionary, its own postings, and the entities it
        // covers. Composed here for the same reason a filter extent is — a published layer the live
        // generation does not hold answers no `match` until the next fold.
        for text in texts {
            let Some(layers) = next
                .columns
                .get_mut(text.column.as_str())
                .and_then(super::Column::text_layers_mut)
            else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "a flush published a text extent for column '{}', which this generation \
                         does not hold",
                        text.column
                    ),
                ));
            };
            layers.push(text_layer(
                SortedDict::open(&text.dict, self.access)?,
                ColumnPostings::open(&text.postings, self.access != tessera_filter::Access::Read)?,
                &text.column,
                &text.dict_rel,
                Bitmap::deserialize::<croaring::Portable>(&std::fs::read(&text.presence)?),
                Some(text.dict_rel.clone()),
            )?);
        }
        for (column, values_rel, extent, dict) in extents {
            next.compose(column, values_rel, Arc::clone(extent), dict.clone())?;
        }
        Ok(next)
    }

    /// This generation's columns with each window of extents **replaced** by the one that carries
    /// their values — the successor generation's, after an entity-space coalesce (§5.2).
    ///
    /// **Replace is not append, and its correctness condition is a different one.** Appending
    /// checks the new layer is disjoint from what is covered; replacing N layers with one must
    /// check that the replacement's presence **equals** the union of the ones it consumes. Without
    /// that, `covered` drifts — the coalesced layer's entities are removed from it with the
    /// consumed layers and added back only as far as the replacement reaches — and every later
    /// disjointness check tests against the wrong coverage, silently. The merge's own
    /// duplicate-entity guard (`tessera_filter_write::coalesce_attr_extents`) makes the mismatch
    /// unreachable, which is exactly why it is cheap to verify and wrong to assume.
    ///
    /// A consumed layer this generation does not hold is a refusal rather than a no-op: the plan
    /// was made against a manifest, so a layer it names and this process cannot find means the two
    /// disagree about what the bundle is, and publishing on that basis would serve a column short
    /// of a window's worth of entities.
    ///
    /// **A keyword window arrives with the dictionary its coalesce minted, and the two are
    /// installed as one layer.** A coalesce merges the window's dictionaries and renumbers every
    /// ordinal, so the replacement's ordinals name positions in a dictionary no consumed layer
    /// held. [`CoalescedWindow::dict`] carries it beside the values, [`check_dictionary_pairing`]
    /// refuses a keyword window without one (and a dictionary on any other family's window), and
    /// the pair becomes a single [`Layer`] in one push — the consumed layers leave and the
    /// replacement enters in the same generation, so no generation ever holds the new ordinals
    /// beside an old dictionary or the old ordinals beside the new one. That is the composition's
    /// half of records §7's rule that a keyword layer's files swap atomically; `AttrExtent` is the
    /// manifest's half.
    ///
    /// **A text column takes the same rule through its own record.** A text layer's dictionary,
    /// postings and presence are one entry, replaced together, so the coalesced layer's new
    /// ordinals arrive with the dictionary that minted them and nothing outside the three files
    /// ever held one. `texts` carries those windows; the coverage equality above is checked for
    /// them too, against `TextLayer::present`, which is exactly what a flush extent stores and
    /// what makes the check expressible for this family.
    ///
    /// **The transpose is replaced whole rather than patched**, and `entity_terms` is the stack
    /// the caller re-derived from the rebased manifest — `None` where the axis did not run, in
    /// which case the live stack rides through unchanged. Its ordinals need no attention either
    /// way: they are dictionary positions, which `coalesce_dict_extents` preserves by construction
    /// (it replaces a contiguous window with the same records in the same order), so unlike a
    /// keyword column's they name the same terms after every coalesce.
    ///
    /// The coverage rule above holds for it too, and is checked the same way: the replacement's
    /// entity set must **equal** the live one's. A merge that lost a layer would leave the
    /// drill-down answering *unknown* for entities that carry labels, and the join rule's arm
    /// comparing against nothing — which is a `409` that does not fire.
    pub fn with_coalesced(
        &self,
        windows: &[CoalescedWindow],
        texts: &[CoalescedTextWindow],
        entity_terms: Option<Arc<tessera_store::EntityTermsStack>>,
        records: Option<Arc<RecordStack>>,
    ) -> std::io::Result<FilterColumns> {
        let entity_terms = match entity_terms {
            None => Arc::clone(&self.entity_terms),
            Some(next) => {
                let held = self.entity_terms.entity_set();
                let replacement = next.entity_set();
                if held != replacement {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "a coalesce's entity→term stack holds lists for {} entities where the one it \
                             replaces holds {}; replacing on that would answer 'unknown' for an \
                             entity that carries labels, which on the write path is a 409 that \
                             does not fire",
                            replacement.cardinality(),
                            held.cardinality()
                        ),
                    ));
                }
                next
            }
        };
        // **The record axis's stack is replaced, not carried through** — the same rule the
        // transpose above takes, and it had the same defect the transpose was fixed for: a
        // coalesce that folded a window of record extents into one edited the manifest and left
        // the live stack holding the layers it had consumed, so the running process kept probing
        // them until a restart while a reopen of the same bundle held one. Nothing served a wrong
        // answer — the layers are disjoint in entity space (I9), so an extra layer answers for
        // the entities it always answered for — but the cost the coalesce exists to remove stayed
        // until a restart removed it, and the process and its own manifest disagreed about what
        // it was serving from. `None` where the axis did not run, in which case the live stack
        // rides through untouched, which is the ordinary case.
        let records = match records {
            None => Arc::clone(&self.records),
            Some(next) => next,
        };
        let mut next = FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records,
            entity_terms,
        };
        for window in windows {
            let Some(held) = next.columns.get_mut(&window.column) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "a coalesce names column '{}', which this generation does not hold",
                        window.column
                    ),
                ));
            };
            check_dictionary_pairing(held.family, &window.column, window.dict.is_some())?;
            let Some(layers) = held.value_layers_mut() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "a coalesce names column '{}', which this generation does not hold",
                        window.column
                    ),
                ));
            };
            let mut union = Bitmap::new();
            for rel in &window.consumed {
                let Some(layer) = layers
                    .iter()
                    .find(|l| l.values_rel.as_deref() == Some(rel.as_str()))
                else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "a coalesce for column '{}' consumed extent '{rel}', which this \
                             generation holds no layer for",
                            window.column
                        ),
                    ));
                };
                union |= layer.values.present();
            }
            if union != window.values.present() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "the coalesced extent for column '{}' is present for {} entities where the \
                         {} layers it replaces cover {}; replacing on that would leave the \
                         column's coverage wrong and every later disjointness check testing \
                         against it",
                        window.column,
                        window.values.present().cardinality(),
                        window.consumed.len(),
                        union.cardinality()
                    ),
                ));
            }
            layers.retain(|l| {
                l.values_rel
                    .as_ref()
                    .is_none_or(|rel| !window.consumed.contains(rel))
            });
            // One push of one struct: the coalesced ordinals and the dictionary that numbers
            // them enter together, checked as a pair above, and the consumed layers left with
            // their own dictionaries in the `retain` above.
            layers.push(Layer {
                values_rel: Some(window.values_rel.clone()),
                values: Arc::clone(&window.values),
                dict: window.dict.clone(),
            });
            // `covered` is unchanged by construction — the equality above is what says so — so it
            // is neither recomputed nor adjusted here.
        }
        for window in texts {
            let column = &window.paths.column;
            let Some(layers) = next
                .columns
                .get_mut(column)
                .and_then(super::Column::text_layers_mut)
            else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("a coalesce names text column '{column}', which this generation does not hold"),
                ));
            };
            let mut union = Bitmap::new();
            for rel in &window.consumed {
                let Some(layer) = layers
                    .iter()
                    .find(|l| l.dict_rel.as_deref() == Some(rel.as_str()))
                else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "a coalesce for text column '{column}' consumed extent '{rel}', which \
                             this generation holds no layer for"
                        ),
                    ));
                };
                union |= &layer.present;
            }
            // Opened before anything is removed, so a replacement that will not open leaves the
            // consumed layers standing rather than a column short of a window's worth of terms.
            let replacement = text_layer(
                SortedDict::open(&window.paths.dict, self.access)?,
                ColumnPostings::open(
                    &window.paths.postings,
                    self.access != tessera_filter::Access::Read,
                )?,
                column,
                &window.paths.dict_rel,
                Bitmap::deserialize::<croaring::Portable>(&std::fs::read(&window.paths.presence)?),
                Some(window.paths.dict_rel.clone()),
            )?;
            // The attribute axis's replacement rule, and this family can state it because a text
            // extent stores presence: the replacement must stand for **exactly** the entities its
            // inputs did. A merge that dropped a layer answers every later `match` short of that
            // layer's documents, silently, and no cardinality anywhere else would move.
            if union != replacement.present {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "the coalesced text extent for column '{column}' is present for {} \
                         entities where the {} layers it replaces cover {}",
                        replacement.present.cardinality(),
                        window.consumed.len(),
                        union.cardinality()
                    ),
                ));
            }
            layers.retain(|l| {
                l.dict_rel
                    .as_ref()
                    .is_none_or(|rel| !window.consumed.contains(rel))
            });
            layers.push(replacement);
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_support::*;
    use crate::filter::FilterOperand;
    use tessera_filter::RecordValue;

    // -------------------------------------------------------------------------------------
    // The layer pairing, and what crosses the boundary
    // -------------------------------------------------------------------------------------

    /// A keyword extent arriving without its dictionary is refused rather than composed: scanned
    /// as codes it would answer every string predicate with the empty set, which under-reports
    /// with no symptom.
    #[test]
    fn a_keyword_extent_without_its_dictionary_is_refused() {
        let mut columns = keyword_column(
            "sub",
            vec![(None, universal(&[0, 1]), dict(&["alpha", "beta"]))],
        );
        let err = columns
            .compose("sub", "extents/f1.arrow", partial(&[10], &[0]), None)
            .unwrap_err();
        assert!(
            err.to_string().contains("carries no sorted dictionary"),
            "{err}"
        );
    }


    /// And the other direction: a dictionary for a column the schema does not call a keyword means
    /// the caller and the declaration disagree about what its values are.
    #[test]
    fn a_dictionary_on_a_non_keyword_extent_is_refused() {
        let mut columns = keyword_column(
            "sub",
            vec![(None, universal(&[0, 1]), dict(&["alpha", "beta"]))],
        );
        columns.columns.get_mut("sub").expect("the column").family = Family::Numeric;
        let err = columns
            .compose(
                "sub",
                "extents/f1.arrow",
                partial(&[10], &[0]),
                Some(dict(&["alpha"])),
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not declare the column a keyword"),
            "{err}"
        );
    }


    /// **A coalesced keyword window is installed with the dictionary its merge minted, as one
    /// layer, and every entity still reads its own key.** The merged dictionary numbers `beta` and
    /// `gamma` the other way round from either consumed layer, so a replacement resolved against
    /// a consumed layer's dictionary — or a consumed layer left behind beside the merged one —
    /// would answer another key's entities.
    ///
    /// The refusals are the same test's other half: the window without its dictionary is refused
    /// with the generation unchanged, and a dictionary on a window of a column the schema does not
    /// call a keyword is refused too — both through the one pairing rule a flush's extent passes.
    #[test]
    fn a_coalesced_keyword_window_installs_its_dictionary_beside_its_values() {
        let columns = keyword_column(
            "sub",
            vec![
                (None, universal(&[0, 1]), dict(&["alpha", "beta"])),
                // Extent 1: gamma = 0, over entity 10. Extent 2: beta = 0, over entity 11.
                (
                    Some("extents/f1.arrow"),
                    partial(&[10], &[0]),
                    dict(&["gamma"]),
                ),
                (
                    Some("extents/f2.arrow"),
                    partial(&[11], &[0]),
                    dict(&["beta"]),
                ),
            ],
        );
        let consumed = vec![
            "extents/f1.arrow".to_string(),
            "extents/f2.arrow".to_string(),
        ];
        // The merge's output: dictionary [beta, gamma], so 10 -> gamma is ordinal 1 and
        // 11 -> beta is ordinal 0 — neither consumed layer's numbering.
        let window = |dict: Option<Arc<SortedDict>>| CoalescedWindow {
            column: "sub".to_string(),
            consumed: consumed.clone(),
            values_rel: "coalesced/c-1/attrs/sub/values.arrow".to_string(),
            values: partial(&[10, 11], &[1, 0]),
            dict,
        };
        let next = columns
            .with_coalesced(&[window(Some(dict(&["beta", "gamma"])))], &[], None, None)
            .expect("a keyword window with its dictionary replaces its layers");
        assert_eq!(
            next.layer_count("sub"),
            Some(2),
            "base plus the one coalesced layer"
        );
        let candidate = set(&[0, 1, 10, 11]);
        let resolve = |columns: &FilterColumns, key: &str| {
            members(
                &columns
                    .resolve("sub", &FilterOperand::TextEquals(key.into()), &candidate)
                    .expect("answers"),
            )
        };
        assert_eq!(resolve(&next, "alpha"), vec![0]);
        assert_eq!(resolve(&next, "beta"), vec![1, 11]);
        assert_eq!(resolve(&next, "gamma"), vec![10]);
        assert_eq!(
            next.stored_value("sub", 10),
            Some(RecordValue::Utf8("gamma".into()))
        );
        assert_eq!(
            next.stored_value("sub", 11),
            Some(RecordValue::Utf8("beta".into()))
        );
        // And exactly what the consumed layers answered, key for key.
        for key in ["alpha", "beta", "gamma", "delta"] {
            assert_eq!(resolve(&next, key), resolve(&columns, key), "{key}");
        }

        // The half: a keyword window with no dictionary has no reading and is refused.
        let err = columns
            .with_coalesced(&[window(None)], &[], None, None)
            .expect_err("a keyword window without its dictionary is refused");
        assert!(
            err.to_string().contains("carries no sorted dictionary"),
            "{err}"
        );
        assert_eq!(
            columns.layer_count("sub"),
            Some(3),
            "the generation is untouched"
        );

        // The other direction: a dictionary on a column the schema does not call a keyword. The
        // same three layers under a column whose declared family is numeric.
        let mut numeric = keyword_column(
            "sub",
            vec![
                (None, universal(&[0, 1]), dict(&["alpha", "beta"])),
                (
                    Some("extents/f1.arrow"),
                    partial(&[10], &[0]),
                    dict(&["gamma"]),
                ),
                (
                    Some("extents/f2.arrow"),
                    partial(&[11], &[0]),
                    dict(&["beta"]),
                ),
            ],
        );
        numeric.columns.get_mut("sub").expect("the column").family = Family::Numeric;
        let err = numeric
            .with_coalesced(&[window(Some(dict(&["beta", "gamma"])))], &[], None, None)
            .expect_err("a dictionary on a non-keyword window is refused");
        assert!(
            err.to_string()
                .contains("does not declare the column a keyword"),
            "{err}"
        );
    }


    /// **A flush that lands between a coalesce's plan and its replace keeps its own dictionary.**
    /// The replace names the consumed layers by path, so an extent appended meanwhile is neither
    /// consumed nor renumbered: it stays a layer of its own, its ordinals read against the
    /// dictionary that minted them, beside the coalesced layer read against the merged one. The
    /// appended extent numbers `gamma` as ordinal 0 where the merged dictionary numbers it 1, so
    /// a replace that read either against the other would answer wrongly here.
    #[test]
    fn a_flush_landing_between_plan_and_replace_keeps_its_own_dictionary() {
        let planned = keyword_column(
            "sub",
            vec![
                (None, universal(&[0]), dict(&["alpha"])),
                (
                    Some("extents/f1.arrow"),
                    partial(&[10], &[0]),
                    dict(&["gamma"]),
                ),
                (
                    Some("extents/f2.arrow"),
                    partial(&[11], &[0]),
                    dict(&["beta"]),
                ),
            ],
        );
        // The flush's extent, composed onto the generation after the window was planned.
        let live = planned
            .with_extents(
                &[(
                    "sub".to_string(),
                    "extents/f3.arrow".to_string(),
                    partial(&[12], &[0]),
                    Some(dict(&["gamma"])),
                )],
                &[],
                &[],
                &[],
            )
            .expect("the flush's extent composes");
        let next = live
            .with_coalesced(
                &[CoalescedWindow {
                    column: "sub".to_string(),
                    consumed: vec!["extents/f1.arrow".into(), "extents/f2.arrow".into()],
                    values_rel: "coalesced/c-1/attrs/sub/values.arrow".to_string(),
                    values: partial(&[10, 11], &[1, 0]),
                    dict: Some(dict(&["beta", "gamma"])),
                }],
                &[],
                None,
                None,
            )
            .expect("the window still rebases: its layers are named by path");
        assert_eq!(
            next.layer_count("sub"),
            Some(3),
            "base, the coalesced layer, and the flush's own"
        );
        let candidate = set(&[0, 10, 11, 12]);
        let resolve = |key: &str| {
            members(
                &next
                    .resolve("sub", &FilterOperand::TextEquals(key.into()), &candidate)
                    .expect("answers"),
            )
        };
        assert_eq!(resolve("gamma"), vec![10, 12]);
        assert_eq!(resolve("beta"), vec![11]);
        assert_eq!(
            next.stored_value("sub", 12),
            Some(RecordValue::Utf8("gamma".into()))
        );
    }
}
