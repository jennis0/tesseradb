use std::path::Path;
use std::sync::Arc;

use croaring::Bitmap;
use tessera_filter::{RecordExtentPaths, RecordStack, SortedDict, ValueColumn};

use super::open::{open_scoped_column, runtime_layers};
use super::{check_dictionary_pairing, record_open_error, FilterColumns, Layer, TextLayer};
use crate::filter::error::ComposeError;
use crate::filter::{
    scoped_column_name, scoped_has_value_column, scoped_is_filterable, Placement, PIN,
};

/// The three files one published text extent names, resolved to paths.
#[derive(Debug, Clone)]
pub struct TextExtentPaths {
    pub column: String,
    /// The manifest path, the layer's identity, distinct from `dict` below, that path resolved
    /// against the prefix directory.
    pub dict_rel: String,
    pub dict: std::path::PathBuf,
    pub postings: std::path::PathBuf,
    pub presence: std::path::PathBuf,
}

/// One column's window of extents, and the coalesced extent that replaces them. Named by the
/// manifest's own paths on both sides, ABA-safe against flushes that published while the pass
/// ran: a `seg_id` and the paths derived from it are never reused.
#[derive(Debug, Clone)]
pub struct CoalescedWindow {
    /// The consumed extents' values paths, as `attr_extents` names them.
    pub consumed: Vec<String>,
    /// What replaces them: the entry the manifest takes, and the reader publication pushes.
    pub replacement: OpenedExtent,
}

/// One text column's window of extents, and the coalesced extent that replaces them, named by
/// dictionary path on both sides. The replacement carries its three files rather than an opened
/// layer because a text layer is composed from paths wherever it enters.
#[derive(Debug, Clone)]
pub struct CoalescedTextWindow {
    /// The consumed extents' dictionary paths, as `text_extents` names them.
    pub consumed: Vec<String>,
    /// The replacement, named exactly as a flush's extent is, the column included.
    pub paths: TextExtentPaths,
}

/// One extent as publication hands it over, a flush's and a coalesce's alike: the manifest entry
/// it becomes, the opened column, and, for a keyword, the dictionary those values are ordinals
/// into. The dictionary travels with the values or not at all: an extent's ordinals are positions
/// in its own dictionary and name nothing against any other, so composition refuses a half.
///
/// Opened on the pool, so publication is a pointer push on the executor thread and cannot fail on
/// IO after the manifest edit.
#[derive(Debug, Clone)]
pub struct OpenedExtent {
    pub extent: tessera_store::manifest::AttrExtent,
    pub values: Arc<ValueColumn>,
    pub dict: Option<Arc<SortedDict>>,
}

impl OpenedExtent {
    /// The key the column map holds this extent's layer under: the column's own for an
    /// entity-scoped extent, the resolved form where the entry names a view.
    fn column_name(&self) -> String {
        crate::filter::extent_column_name(&self.extent.column, self.extent.view.as_deref())
    }
}

impl FilterColumns {
    /// The next generation's columns before anything is added: this generation's, with every
    /// layer shared rather than re-opened. Cheap by construction, since the base columns and the
    /// two stacks are `Arc`s.
    fn successor(&self) -> FilterColumns {
        FilterColumns {
            columns: self.columns.clone(),
            placements: self.placements.clone(),
            access: self.access,
            records: Arc::clone(&self.records),
            entity_terms: Arc::clone(&self.entity_terms),
        }
    }

    /// Add one flush's extent to the column it names, refusing a name this composition does not
    /// hold a value column for, see [`Column::push_extent`] for what the column itself refuses.
    pub(in crate::filter) fn compose(
        &mut self,
        column: &str,
        values_rel: &str,
        extent: Arc<ValueColumn>,
        dict: Option<Arc<SortedDict>>,
    ) -> Result<(), ComposeError> {
        let Some(held) = self.columns.get_mut(column) else {
            return Err(ComposeError::UnknownColumn {
                column: column.to_string(),
            });
        };
        held.push_extent(column, values_rel, extent, dict)
    }

    /// Add one opened extent to the column its manifest entry names, the one push every producer
    /// takes: a flush's extent, a coalesce's replacement, and each entry [`FilterColumns::open`]
    /// reads at a restart.
    pub(in crate::filter) fn push_opened(
        &mut self,
        opened: &OpenedExtent,
    ) -> Result<(), ComposeError> {
        self.compose(
            &opened.column_name(),
            &opened.extent.values,
            Arc::clone(&opened.values),
            opened.dict.clone(),
        )
    }

    /// This generation's columns with an attribute column declared at a running service added: an
    /// empty stack the next flush's extent composes onto, and the placement its flags afford.
    pub(crate) fn with_runtime_column(
        &self,
        scalar: &tessera_store::manifest::DeclaredScalar,
        declared_index: usize,
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
    ) -> Result<FilterColumns, ComposeError> {
        let mut next = self.successor();
        if let Some(placement) = Placement::of(scalar, vocabularies) {
            next.placements.insert(scalar.name.clone(), placement);
        }
        if let Some(column) = runtime_layers(scalar, declared_index, vocabularies)? {
            next.columns.insert(scalar.name.clone(), column);
        }
        Ok(next)
    }

    /// This generation's columns with a newly based group-scoped column opened onto them: the
    /// first flush of a view a family had no column for. Applied before the extents compose,
    /// since the extent composes onto a column that must exist first. A `(column, view)` pair
    /// already held is a no-op, a re-publication reaching the same state.
    pub fn with_scoped_columns(
        &self,
        partition_dir: &Path,
        columns: &[(String, String, tessera_types::view::ViewIncarnation)],
        scoped: &[tessera_store::manifest::ScopedScalar],
        vocabularies: &[tessera_store::manifest::ManifestVocabulary],
        mmap: bool,
    ) -> Result<FilterColumns, ComposeError> {
        let mut next = self.successor();
        for (column, view, incarnation) in columns {
            let Some(family) = scoped.iter().find(|f| f.name == *column) else {
                return Err(ComposeError::UndeclaredScopedFamily {
                    column: column.clone(),
                    view: view.clone(),
                });
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
        let mut next = self.successor();
        next.columns.retain(|name, _| !dead(name));
        next.placements.retain(|name, _| !dead(name));
        next
    }

    /// This generation's columns with one flush's extents added, the successor generation's,
    /// cheap by construction since the base columns are `Arc`s.
    ///
    /// A keyword column's extent carries the dictionary the flush minted beside the values it
    /// numbers, so the pair composes as one (see [`FilterColumns::compose`], which refuses either
    /// half without the other).
    pub fn with_extents(
        &self,
        extents: &[OpenedExtent],
        records: &[RecordExtentPaths],
        entity_terms: &[tessera_store::EntityTermsExtentPaths],
        texts: &[TextExtentPaths],
    ) -> Result<FilterColumns, ComposeError> {
        // Composes here so this process serves from the stack it holds; uncomposed, the entities
        // flushed would have their blob-resident fields absent from drill-down until the fold.
        let records = if records.is_empty() {
            Arc::clone(&self.records)
        } else {
            Arc::new(
                self.records
                    .with_extents(records, self.access)
                    .map_err(record_open_error)?,
            )
        };
        // Composes here for the same reason, plus its own: uncomposed labels leave the join
        // rule's label arm unable to compare against the batch that just landed.
        let entity_terms =
            if entity_terms.is_empty() {
                Arc::clone(&self.entity_terms)
            } else {
                Arc::new(
                    self.entity_terms
                        .with_extents(entity_terms)
                        .map_err(|e| ComposeError::EntityTermsUnreadable(e.to_string()))?,
                )
            };
        let mut next = FilterColumns {
            records,
            entity_terms,
            ..self.successor()
        };
        // A text extent appends a layer: a published layer the live generation does not hold
        // answers no `match` until the next fold.
        for text in texts {
            let Some(layers) = next
                .columns
                .get_mut(text.column.as_str())
                .and_then(super::Column::text_layers_mut)
            else {
                return Err(ComposeError::UnknownColumn {
                    column: text.column.clone(),
                });
            };
            layers.push(TextLayer::open(
                &text.column,
                &text.dict_rel,
                &text.dict,
                &text.postings,
                &text.presence,
                self.access,
            )?);
        }
        for extent in extents {
            next.push_opened(extent)?;
        }
        Ok(next)
    }

    /// This generation's columns with each window of extents replaced by the one that carries
    /// their values, after an entity-space coalesce.
    ///
    /// Replacing N layers with one must check the replacement's presence equals the union of the
    /// ones it consumes, or `covered` drifts and later disjointness checks test against the wrong
    /// coverage. A consumed layer this generation does not hold is a refusal: the plan was made
    /// against a manifest, and a layer it names that this process cannot find means the two
    /// disagree about what the bundle is.
    ///
    /// A keyword window arrives with the dictionary its coalesce minted, installed as one layer:
    /// the merge renumbers every ordinal, so [`check_dictionary_pairing`] refuses a keyword
    /// window without one, and a dictionary on any other family's window. A text column takes the
    /// same rule through its own record, dictionary, postings and presence replaced together.
    ///
    /// The transpose is replaced whole; `None` where the axis did not run, in which case the live
    /// stack rides through unchanged. The coverage rule holds for it too: the replacement's
    /// entity set must equal the live one's.
    pub fn with_coalesced(
        &self,
        windows: &[CoalescedWindow],
        texts: &[CoalescedTextWindow],
        entity_terms: Option<Arc<tessera_store::EntityTermsStack>>,
        records: Option<Arc<RecordStack>>,
    ) -> Result<FilterColumns, ComposeError> {
        let entity_terms = match entity_terms {
            None => Arc::clone(&self.entity_terms),
            Some(next) => {
                let held = self.entity_terms.entity_set();
                let replacement = next.entity_set();
                if held != replacement {
                    return Err(ComposeError::EntityTermsCoverage {
                        replacement: replacement.cardinality(),
                        held: held.cardinality(),
                    });
                }
                next
            }
        };
        // Replaced, not carried through, for the transpose's reason. `None` where the axis did
        // not run, the ordinary case.
        let records = match records {
            None => Arc::clone(&self.records),
            Some(next) => next,
        };
        let mut next = FilterColumns {
            records,
            entity_terms,
            ..self.successor()
        };
        for window in windows {
            let replacement = &window.replacement;
            let column = replacement.column_name();
            let Some(held) = next.columns.get_mut(&column) else {
                return Err(ComposeError::UnknownColumn { column });
            };
            check_dictionary_pairing(held.family, &column, replacement.dict.is_some())?;
            let Some(layers) = held.value_layers_mut() else {
                return Err(ComposeError::UnknownColumn { column });
            };
            replace_window(layers, &column, &window.consumed, || {
                Ok(Layer {
                    values_rel: Some(replacement.extent.values.clone()),
                    values: Arc::clone(&replacement.values),
                    dict: replacement.dict.clone(),
                })
            })?;
        }
        for window in texts {
            let column = &window.paths.column;
            let Some(layers) = next
                .columns
                .get_mut(column)
                .and_then(super::Column::text_layers_mut)
            else {
                return Err(ComposeError::UnknownColumn {
                    column: column.clone(),
                });
            };
            replace_window(layers, column, &window.consumed, || {
                TextLayer::open(
                    column,
                    &window.paths.dict_rel,
                    &window.paths.dict,
                    &window.paths.postings,
                    &window.paths.presence,
                    self.access,
                )
            })?;
        }
        Ok(next)
    }
}

/// What a replace needs of a layer, on either axis: the manifest path that is its identity, and
/// the entities it stands for.
trait WindowLayer {
    fn identity(&self) -> Option<&str>;
    fn presence(&self) -> Bitmap;
}

impl WindowLayer for Layer {
    fn identity(&self) -> Option<&str> {
        self.values_rel.as_deref()
    }

    fn presence(&self) -> Bitmap {
        self.values.present()
    }
}

impl WindowLayer for TextLayer {
    fn identity(&self) -> Option<&str> {
        self.dict_rel.as_deref()
    }

    fn presence(&self) -> Bitmap {
        self.present.clone()
    }
}

/// Replace one window of a column's layers with the layer that carries their values, the one
/// walk both axes take.
///
/// A consumed layer this generation does not hold is a refusal rather than a no-op, and the
/// replacement stands for exactly the entities its inputs did or it does not enter. Nothing is
/// removed until the replacement is in hand and checked, so one that will not open leaves the
/// consumed layers standing rather than a column short of a window.
fn replace_window<L: WindowLayer>(
    layers: &mut Vec<L>,
    column: &str,
    consumed: &[String],
    open_replacement: impl FnOnce() -> Result<L, ComposeError>,
) -> Result<(), ComposeError> {
    let mut union = Bitmap::new();
    for rel in consumed {
        let Some(layer) = layers.iter().find(|l| l.identity() == Some(rel.as_str())) else {
            return Err(ComposeError::MissingLayer {
                column: column.to_string(),
                rel: rel.clone(),
            });
        };
        union |= layer.presence();
    }
    let replacement = open_replacement()?;
    let presence = replacement.presence();
    if union != presence {
        return Err(ComposeError::CoverageMismatch {
            column: column.to_string(),
            replacement: presence.cardinality(),
            consumed: consumed.len(),
            covered: union.cardinality(),
        });
    }
    layers.retain(|l| {
        l.identity()
            .is_none_or(|rel| !consumed.iter().any(|c| c == rel))
    });
    layers.push(replacement);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_support::*;
    use crate::filter::{Family, FilterOperand};
    use tessera_filter::RecordValue;

    // -------------------------------------------------------------------------------------
    // The layer pairing, and what crosses the boundary
    // -------------------------------------------------------------------------------------

    /// A keyword extent arriving without its dictionary is refused rather than composed.
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
            matches!(&err, ComposeError::KeywordWithoutDictionary { column } if column == "sub"),
            "{err:?}"
        );
    }


    /// And the other direction: a dictionary for a column the schema does not call a keyword.
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
            matches!(&err, ComposeError::DictionaryOnOtherFamily { column } if column == "sub"),
            "{err:?}"
        );
    }


    /// A coalesced keyword window installs with the dictionary its merge minted, and every entity
    /// still reads its own key.
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
        // 11 -> beta is ordinal 0, neither consumed layer's numbering.
        let window = |dict: Option<Arc<SortedDict>>| CoalescedWindow {
            consumed: consumed.clone(),
            replacement: opened(
                "sub",
                "coalesced/c-1/attrs/sub/values.arrow",
                partial(&[10, 11], &[1, 0]),
                dict,
            ),
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
            matches!(&err, ComposeError::KeywordWithoutDictionary { column } if column == "sub"),
            "{err:?}"
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
            matches!(&err, ComposeError::DictionaryOnOtherFamily { column } if column == "sub"),
            "{err:?}"
        );
    }


    /// A flush that lands between a coalesce's plan and its replace keeps its own dictionary: the
    /// replace names the consumed layers by path.
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
                &[opened(
                    "sub",
                    "extents/f3.arrow",
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
                    consumed: vec!["extents/f1.arrow".into(), "extents/f2.arrow".into()],
                    replacement: opened(
                        "sub",
                        "coalesced/c-1/attrs/sub/values.arrow",
                        partial(&[10, 11], &[1, 0]),
                        Some(dict(&["beta", "gamma"])),
                    ),
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
