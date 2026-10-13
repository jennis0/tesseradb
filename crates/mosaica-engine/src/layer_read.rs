//! What `POST /v1/artifacts/browse` and `POST /v1/artifacts` read a layer by: the layer resolved
//! for one viewer, one level of it resolved and counted under one composed mask, and a filter's
//! rows over the whole view.
//!
//! Both routes walk a layer's artifacts off the viewport, so both need the filter answered over
//! every row of the view: a leaf over a render-only column is answered by the viewport only inside
//! its tiles, and these routes have none, so the row route's predicate is run over the whole view.

use std::sync::Arc;

use croaring::Bitmap;
use mosaica_lifecycle::membership::Attachment;
use mosaica_types::layer::{HierarchyKind, RegisteredLayer};
use mosaica_types::EntityId;

use crate::artifact_content::LevelContent;
use crate::artifacts::{ArtifactRows, ArtifactView};
use crate::cancel::CancelToken;
use crate::compose::{EffectiveMask, MaskedSet, WholeMask};
use crate::error::Result;
use crate::filter::{FilterExpr, RoutedFilter};
use crate::figures::Figures;
use crate::region::RegionVerdict;
use crate::session::Session;
use crate::viewport::ServedView;
use crate::Engine;

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
    /// The layer `name` as this viewer may read it in `view` of `generation`.
    pub(crate) fn readable_layer(
        &self,
        session: &Session,
        generation: &crate::Generation,
        name: &str,
        view: &str,
    ) -> std::result::Result<RegisteredLayer, LayerRefusal> {
        let layer = self.write.live().registered_layer(name);
        self.readable(session, generation, layer, view)
    }

    /// [`Self::readable_layer`] for the layer holding `space`: the one a read began on, if it
    /// still holds it.
    pub(crate) fn readable_layer_at(
        &self,
        session: &Session,
        generation: &crate::Generation,
        space: mosaica_types::LayerSpace,
        view: &str,
    ) -> std::result::Result<RegisteredLayer, LayerRefusal> {
        let layer = self.write.live().layer_holding(space);
        self.readable(session, generation, layer, view)
    }

    fn readable(
        &self,
        session: &Session,
        generation: &crate::Generation,
        layer: Option<RegisteredLayer>,
        view: &str,
    ) -> std::result::Result<RegisteredLayer, LayerRefusal> {
        let layer = layer.ok_or(LayerRefusal::Unknown)?;
        if !self.reachable_layers(session).contains(&layer.declaration.name)
            || !layer.declaration.draws_on(view)
            || generation.layer_denied(layer.space)
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
        let side = served.only_side();
        let total_rows = side.data.row_space.total_rows();
        self.route_filters(served, side, mask, cancel, |route| {
            // A column with both routes takes the entity route: there are no tile ranges here to
            // make the row route cheaper, so only a render-only column reaches the scan.
            Ok(match route(expr, false)? {
                RoutedFilter::Entity(entities) => {
                    (side.data.row_space.project(&entities), None)
                }
                RoutedFilter::Row(tree) => {
                    let region = tree.region_verdict();
                    let whole = std::iter::once(0..u32::try_from(total_rows).unwrap_or(u32::MAX))
                        .collect::<Vec<_>>();
                    let rows = self
                        .evaluate_row_route(&tree, side, &whole, total_rows, false)?
                        .rows()
                        .clone();
                    (rows, region)
                }
            })
        })
    }

    /// One level of `layer` under `mask`: its row form, its masked counts and its contents.
    /// `geometry` also accumulates the visible members' positions on a level served from its
    /// column alone, for a centroid or box read off the counts.
    pub(crate) fn read_level(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        layer: &RegisteredLayer,
        level: u32,
        geometry: bool,
    ) -> Result<ReadLevel> {
        let generation = served.generation;
        let name = layer.declaration.name.as_str();
        let vocabulary = crate::viewport::predicate_vocabulary(generation, &layer.declaration);
        let code_of_key = |key: &str| match vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let source = served.only_side().shard.partition_source();
        let (rows, level_version) = self.write.live().with_artifacts(|store| {
            let predicate = crate::viewport::predicate_source(
                &layer.declaration,
                served.only_side().shard,
                served.name,
                served.only_side().data,
                &served.only_side().segments,
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
                &served.only_side().data.row_space,
                Some(&source),
                layer.layout_of(level),
                predicate.as_ref(),
                generation.segments_version,
                &layer.declaration,
            )
        });
        let counts = self.figures(
            served,
            layer.space,
            level,
            level_version,
            &rows,
            mask,
            match geometry {
                true => crate::figures::Geometry::declared(&layer.declaration),
                false => crate::figures::Geometry::None,
            },
        )?;
        let contents = layer
            .runs
            .get(level as usize)
            .and_then(|runs| self.level_content(generation, layer, level, level_version, runs));
        Ok(ReadLevel {
            level,
            level_version,
            rows,
            counts,
            filtered: None,
            contents,
            space_content: generation.space_content(layer.space).cloned(),
        })
    }

    /// `layer`'s supplied contents at `level`, read once for the level rather than once per
    /// artifact: 408ms of a response whose points half is 1.3ms, decompressing one zstd block per
    /// artifact, against 6.7ms read together. `None` where the layer supplies none.
    pub(crate) fn level_content(
        &self,
        generation: &crate::Generation,
        layer: &RegisteredLayer,
        level: u32,
        level_version: u64,
        runs: &mosaica_types::layer::ReservedRuns,
    ) -> Option<Arc<LevelContent>> {
        if layer.declaration.content.supplied.is_empty() {
            return None;
        }
        Some(self.level_contents.get_or_build(
            &layer.declaration.name,
            level,
            level_version,
            generation.segments_version,
            || LevelContent::build(generation.space_content(layer.space).map(Arc::as_ref), runs),
        ))
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
pub(crate) struct ReadLevel {
    pub(crate) level: u32,
    /// The version of the level the row form is of.
    pub(crate) level_version: u64,
    pub(crate) rows: Arc<ArtifactRows>,
    pub(crate) counts: Option<Arc<Figures>>,
    /// Each artifact's visible members matching the filter, on a level served from its column;
    /// set from [`ReadLevel::filtered_counts`].
    pub(crate) filtered: Option<Arc<Vec<u32>>>,
    contents: Option<Arc<LevelContent>>,
    /// The layer space's supplied content on disc, for an artifact `contents` does not hold.
    space_content: Option<Arc<mosaica_filter::RecordStack>>,
}

impl ReadLevel {
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
        ArtifactView::of(
            served,
            layer,
            engine.label_gate(served.session, &layer.declaration),
            &self.rows,
            mask,
            dependency_served,
            self.rows.partition().map(|p| p.answers(served.session.satisfied())),
            self.counts.clone(),
        )
    }

    /// The content the verdict's `rank` chose, with any authored shape taken out of it, or
    /// `None` where it cannot be read back, which withholds the artifact.
    pub(crate) fn content(
        &self,
        engine: &Engine,
        layer: &RegisteredLayer,
        ordinal: u32,
        entity: EntityId,
        rank: Option<u32>,
    ) -> Option<crate::viewport::Supplied> {
        engine.supplied_content(
            self.space_content.as_deref(),
            &layer.declaration,
            self.level,
            ordinal,
            entity,
            rank,
            true,
            self.contents.as_deref(),
        )
    }

    /// Every artifact's visible members in `filter_rows`, in one walk of `visible ∩ filter`, on a
    /// level served from its column, which has no per-artifact membership to intersect; `None` on
    /// any other level.
    pub(crate) fn filtered_counts(
        &self,
        engine: &Engine,
        mask: &EffectiveMask,
        filter_rows: &Bitmap,
    ) -> Option<Arc<Vec<u32>>> {
        let column = self.rows.column()?;
        let visible = mask.visible_all().and(filter_rows);
        Some(Arc::new(engine.pool.install(|| column.histogram_over(&visible))))
    }

    /// How many of the artifact's visible members are in `filter_rows`, from
    /// [`ReadLevel::filtered`] where the level is served from its column.
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
