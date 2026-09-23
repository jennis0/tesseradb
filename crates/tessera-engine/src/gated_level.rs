//! What `POST /v1/artifacts/browse` and `POST /v1/artifacts` read a layer by: the layer resolved
//! for one viewer, one level of it gated and counted under one composed mask, and a filter's rows
//! over the whole view.
//!
//! Both routes walk a layer's artifacts off the viewport, so both need the filter answered over
//! every row of the view: a leaf over a render-only column is answered by the viewport only inside
//! its tiles, and these routes have none, so the row route's predicate is run over the whole view.

use std::sync::Arc;

use croaring::Bitmap;
use tessera_lifecycle::membership::Attachment;
use tessera_types::layer::{HierarchyKind, RegisteredLayer};
use tessera_types::EntityId;

use crate::artifact_content::LevelContent;
use crate::artifacts::{ArtifactRows, ArtifactView};
use crate::cancel::CancelToken;
use crate::compose::{EffectiveMask, MaskedSet, WholeMask};
use crate::error::Result;
use crate::filter::{FilterExpr, RoutedFilter};
use crate::histogram::MaskedCounts;
use crate::region::RegionVerdict;
use crate::session::Session;
use crate::viewport::ServedView;
use crate::{Engine, Generation};

/// Why a layer or a level named by a request cannot be read. Each names deployment schema the
/// caller reads off `/v1/meta`, so refusing discloses nothing.
pub(crate) enum LayerRefusal {
    /// Not a layer `/v1/meta` publishes to this viewer in this view, or one suppressed or deleted:
    /// one answer for every reason.
    Unknown,
    /// `level` on a kind with one level.
    OneLevel(HierarchyKind),
    /// A level past the ones the layer holds.
    NoSuchLevel { held: usize },
}

impl Engine {
    /// The layer `name` as this viewer may read it in `view`.
    pub(crate) fn readable_layer(
        &self,
        session: &Session,
        generation: &Generation,
        name: &str,
        view: &str,
    ) -> std::result::Result<RegisteredLayer, LayerRefusal> {
        if !self.reachable_layers(session).contains(name) {
            return Err(LayerRefusal::Unknown);
        }
        let layer = self
            .write
            .live()
            .registered_layer(name)
            .ok_or(LayerRefusal::Unknown)?;
        if !layer.declaration.views.iter().any(|s| s == view)
            || generation.overlay.is_deleted(layer.entity)
            || generation.overlay.is_suppressed(layer.entity)
        {
            return Err(LayerRefusal::Unknown);
        }
        Ok(layer)
    }

    /// A filter's rows over the whole view, before the mask, and the coarsest verdict its region
    /// leaves reached. A leaf with an entity-space route is projected whole; one over a
    /// render-only column is scanned over every row of the view.
    pub(crate) fn whole_view_filter_rows(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        expr: &FilterExpr,
        cancel: &Option<CancelToken>,
    ) -> Result<(Bitmap, Option<RegionVerdict>)> {
        let total_rows = served.data.row_space.total_rows();
        self.route_filters(served, mask, cancel, |route| {
            // A column with both routes takes the entity route: there are no tile ranges here to
            // make the row route cheaper, so only a render-only column reaches the scan.
            Ok(match route(expr, false)? {
                RoutedFilter::Entity(entities) => (served.data.row_space.project(&entities), None),
                RoutedFilter::Row(tree) => {
                    let region = tree.region_verdict();
                    let whole = std::iter::once(0..u32::try_from(total_rows).unwrap_or(u32::MAX))
                        .collect::<Vec<_>>();
                    let rows = self
                        .evaluate_row_route(&tree, served, &whole, total_rows, false)?
                        .rows()
                        .clone();
                    (rows, region)
                }
            })
        })
    }

    /// One level of `layer` under `mask`: its row form, its masked counts, its contents and,
    /// where `filter_rows` is given and the level is served from its column, the filtered count
    /// of every artifact in one walk. `geometry` also accumulates the visible members' positions
    /// on a level served from its column alone, for a centroid or box read off the counts.
    pub(crate) fn gated_level(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        layer: &RegisteredLayer,
        level: u32,
        filter_rows: Option<&Bitmap>,
        geometry: bool,
    ) -> GatedLevel {
        let generation = served.generation;
        let name = layer.declaration.name.as_str();
        let vocabulary = crate::viewport::predicate_vocabulary(generation, &layer.declaration);
        let code_of_key = |key: &str| match vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let source = generation.partition_source();
        let (rows, level_version) = self.write.live().with_artifacts(|store| {
            let predicate = crate::viewport::predicate_source(
                &layer.declaration,
                generation,
                served.name,
                served.data,
                &served.segments,
                &code_of_key,
                &self.shapes,
                store,
                level,
            );
            // The version the form is of, not the store's: the counts below are filed under it
            // and are the same entry a viewport reads.
            self.artifact_projections.get_or_build(
                &generation.prefix,
                served.name,
                name,
                level,
                store,
                &served.data.row_space,
                Some(&source),
                layer.layout_of(level),
                predicate.as_ref(),
                generation.segments_version,
                crate::artifacts::serves_column_only(&layer.declaration),
            )
        });
        let counts = self.masked_counts(
            &served.mask_identity,
            served.name,
            name,
            level,
            level_version,
            &rows,
            mask,
            (geometry && crate::artifacts::derives_accumulated_geometry(&layer.declaration))
                .then_some(&served.segments[..]),
        );
        // A label column has no per-artifact membership to intersect, so the filtered counts are
        // one walk of `visible ∩ filter`, the shape the masked counts take.
        let filtered = match (rows.column(), filter_rows) {
            (Some(column), Some(filter_rows)) => {
                let visible = mask.visible_all().and(filter_rows);
                Some(self.pool.install(|| column.histogram_over(&visible)))
            }
            _ => None,
        };
        let contents = match layer.runs.get(level as usize) {
            Some(runs) if !layer.declaration.content.supplied.is_empty() => {
                Some(self.level_contents.get_or_build(
                    name,
                    level,
                    level_version,
                    generation.segments_version,
                    || LevelContent::build(generation.filter_columns.records(), runs),
                ))
            }
            _ => None,
        };
        GatedLevel {
            level,
            rows,
            counts,
            filtered,
            contents,
        }
    }
}

/// Whether `layer` holds the `level` a request named: a kind with one level holds none to name.
pub(crate) fn check_level(
    layer: &RegisteredLayer,
    level: Option<u32>,
) -> std::result::Result<(), LayerRefusal> {
    let Some(level) = level else {
        return Ok(());
    };
    let kind = layer.declaration.hierarchy.kind;
    if !matches!(kind, HierarchyKind::Stacked | HierarchyKind::Tiered) {
        return Err(LayerRefusal::OneLevel(kind));
    }
    if level as usize >= layer.runs.len() {
        return Err(LayerRefusal::NoSuchLevel {
            held: layer.runs.len(),
        });
    }
    Ok(())
}

/// One level of one layer, resolved for one viewer under one composed mask.
pub(crate) struct GatedLevel {
    pub(crate) level: u32,
    pub(crate) rows: Arc<ArtifactRows>,
    pub(crate) counts: Option<Arc<MaskedCounts>>,
    /// Each artifact's visible members matching the filter, on a level served from its column.
    filtered: Option<Vec<u32>>,
    contents: Option<Arc<LevelContent>>,
}

impl GatedLevel {
    /// The one verdict over this level for `session`, with `dependency_served` answering for an
    /// attached artifact's target.
    pub(crate) fn view<'a>(
        &'a self,
        engine: &Engine,
        served: &'a ServedView<'a>,
        mask: &'a EffectiveMask,
        layer: &'a RegisteredLayer,
        dependency_served: &'a dyn Fn(&Attachment) -> bool,
    ) -> ArtifactView<'a, EffectiveMask> {
        ArtifactView {
            declaration: &layer.declaration,
            overlay: &served.generation.overlay,
            labels: engine.label_gate(served.session, &layer.declaration),
            layer_reachable: true,
            rows: &self.rows,
            mask,
            dependency_served,
            containment: self
                .rows
                .partition()
                .map(|p| p.answers(served.session.satisfied())),
            denied: served.denied,
            counts: self.counts.clone(),
        }
    }

    /// The content the verdict's `rank` chose, whole, or `None` where it cannot be read back,
    /// which withholds the artifact.
    pub(crate) fn content(
        &self,
        engine: &Engine,
        generation: &Generation,
        layer: &RegisteredLayer,
        ordinal: u32,
        entity: EntityId,
        rank: Option<u32>,
    ) -> Option<Vec<String>> {
        engine.supplied_content(
            generation,
            &layer.declaration.name,
            self.level,
            ordinal,
            entity,
            layer.declaration.content.supplied.len(),
            rank,
            true,
            self.contents.as_deref(),
        )
    }

    /// How many of the artifact's visible members are in `filter_rows`.
    pub(crate) fn matched_count(
        &self,
        ordinal: u32,
        mask: &EffectiveMask,
        filter_rows: &Bitmap,
    ) -> u64 {
        match (&self.filtered, self.rows.get(ordinal)) {
            (Some(histogram), _) => histogram.get(ordinal as usize).copied().unwrap_or(0) as u64,
            (None, Some(members)) => mask.visible_rows(members).and_cardinality(filter_rows),
            (None, None) => 0,
        }
    }
}
