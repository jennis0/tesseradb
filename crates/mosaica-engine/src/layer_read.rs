//! What every artifact route reads a layer by: the layer resolved for one viewer, one level of it
//! in every shard of the view, counted under the composed mask in each shard the viewer sees
//! anything in, and a filter's rows over the whole view in each shard.
//!
//! [`Engine::whole_view_filter_rows`] serves the two routes that walk a layer's artifacts off the
//! viewport, `POST /v1/artifacts/browse` and `POST /v1/artifacts`: a leaf over a render-only
//! column is answered by the viewport only inside its tiles, and these routes have none, so the row
//! route's predicate is run over the whole view.

use std::sync::Arc;

use croaring::Bitmap;
use mosaica_lifecycle::membership::Attachment;
use mosaica_types::layer::{HierarchyKind, RegisteredLayer};
use mosaica_types::EntityId;

use crate::artifact_content::LevelContent;
use crate::artifacts::{ArtifactRows, ArtifactSide, ArtifactView, LabelGate};
use crate::cancel::CancelToken;
use crate::compose::{EffectiveMask, ShardedMask, WholeMask};
use crate::derived::RowLocator;
use crate::error::Result;
use crate::filter::{FilterExpr, RoutedFilter};
use crate::figures::{Geometry, LevelFigures};
use crate::region::RegionVerdict;
use crate::session::Session;
use crate::viewport::{ResolvedLeaves, ServedView};
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

    /// A filter's rows over the whole view in each side, indexed like `served.sides`, before the
    /// mask, and the coarsest verdict its region leaves reached. A side outside
    /// [`ShardedMask::filtered_leaves`] holds no row. A leaf with an entity-space route is
    /// projected whole; one over a render-only column is scanned over every row of the view.
    pub(crate) fn whole_view_filter_rows(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        expr: &FilterExpr,
        cancel: &Option<CancelToken>,
    ) -> Result<(Vec<Bitmap>, Option<RegionVerdict>)> {
        let resolved = ResolvedLeaves::new(mask);
        let mut rows = vec![Bitmap::new(); served.sides.len()];
        let mut region = None;
        for (place, side, _) in served.filtered_sides(mask) {
            let total_rows = side.data.row_space.total_rows();
            let (side_rows, verdict) =
                self.route_filters(served, side, &resolved, cancel, |route| {
                    // A column with both routes takes the entity route: there are no tile ranges
                    // here to make the row route cheaper, so only a render-only column reaches
                    // the scan.
                    Ok(match route(expr, false)? {
                        RoutedFilter::Entity(entities) => {
                            (side.data.row_space.project(&entities), None)
                        }
                        RoutedFilter::Row(tree) => {
                            let region = tree.region_verdict();
                            let whole = std::iter::once(
                                0..u32::try_from(total_rows).unwrap_or(u32::MAX),
                            )
                            .collect::<Vec<_>>();
                            let rows = self
                                .evaluate_row_route(&tree, side, &whole, total_rows, false)?
                                .rows()
                                .clone();
                            (rows, region)
                        }
                    })
                })?;
            rows[place] = side_rows;
            region = RegionVerdict::coarsest(region, verdict);
        }
        Ok((rows, region))
    }

    /// `level` of `layer` in every side of `served`'s view: each side's row form, built from that
    /// shard's slice of the level's memberships, every one under one look at the store, and the
    /// version of the level's lineage. Nothing is counted.
    pub(crate) fn level_forms(
        &self,
        served: &ServedView<'_>,
        layer: &RegisteredLayer,
        level: u32,
    ) -> LevelForms {
        let generation = served.generation;
        let vocabulary = crate::viewport::predicate_vocabulary(generation, &layer.declaration);
        let code_of_key = |key: &str| match vocabulary {
            Some(vocabulary) => vocabulary.code_of(key),
            None => key.parse::<u32>().ok(),
        };
        let (built, lineage) = self.write.live().with_artifacts(|written| {
            let built: Vec<(Arc<ArtifactRows>, u64)> = served
                .sides
                .iter()
                .map(|side| {
                    self.form_in(
                        generation,
                        side.shard,
                        served.name,
                        side.data,
                        &side.segments,
                        layer,
                        level,
                        &code_of_key,
                        written,
                    )
                })
                .collect();
            let lineage = lineage_at(
                written,
                &layer.declaration.name,
                level,
                served.sides[0].shard.id(),
                built[0].1,
            );
            (built, lineage)
        });
        let (rows, versions): (Vec<_>, Vec<_>) = built.into_iter().unzip();
        LevelForms {
            shards: served.sides.iter().map(|side| side.shard.id()).collect(),
            read: vec![false; rows.len()],
            rows,
            versions,
            lineage,
            counted: None,
        }
    }

    /// `level` of `layer` in `shard`'s rows for `view`: its row form, built from the shard's slice,
    /// and the version of the level the form is of, which counts are filed under. That version lags
    /// the store's between an accepted write and the tick that publishes it. `written` is the store
    /// the live state lends.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn form_in(
        &self,
        generation: &crate::Generation,
        shard: &crate::Shard,
        view: &str,
        view_data: &mosaica_store::read::ViewData,
        segments: &[(&mosaica_store::read::SegmentData, u32)],
        layer: &RegisteredLayer,
        level: u32,
        code_of_key: &dyn Fn(&str) -> Option<u32>,
        written: &mosaica_lifecycle::membership::ArtifactStore,
    ) -> (Arc<ArtifactRows>, u64) {
        let declaration = &layer.declaration;
        let predicate = crate::viewport::predicate_source(
            declaration,
            shard,
            view,
            view_data,
            segments,
            code_of_key,
            &self.shapes,
            written,
            level,
        );
        self.artifact_projections.get_or_build(
            &generation.prefix,
            shard.id(),
            view,
            view_data.incarnation,
            &declaration.name,
            level,
            crate::write::slice_of(written, shard.id()),
            written,
            &view_data.row_space,
            Some(&shard.partition_source()),
            layer.layout_of(level),
            predicate.as_ref(),
            shard.segments_version,
            declaration,
        )
    }

    /// Count `forms` under `mask`: the level's figures over every side whose form has a row
    /// column ([`Engine::level_figures`]), and which sides the request reads
    /// ([`ShardedMask::read_leaf`]). Decided by the level's layout, never the request: a form
    /// with no row column counts one artifact at a time and has no figures. `geometry` also
    /// accumulates the visible members' positions on a level served from its column alone.
    pub(crate) fn count_level(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        space: mosaica_types::LayerSpace,
        level: u32,
        forms: &mut LevelForms,
        geometry: Geometry,
    ) -> Result<()> {
        forms.counted = self
            .level_figures(
                served,
                mask,
                space,
                level,
                &forms.rows,
                &forms.versions,
                geometry,
            )?
            .map(Arc::new);
        forms.read = forms
            .shards
            .iter()
            .map(|shard| mask.read_leaf(*shard).is_some())
            .collect();
        Ok(())
    }

    /// [`Self::level_forms`], counted ([`Self::count_level`]).
    pub(crate) fn counted_level(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        layer: &RegisteredLayer,
        level: u32,
        geometry: Geometry,
    ) -> Result<LevelForms> {
        let mut forms = self.level_forms(served, layer, level);
        self.count_level(served, mask, layer.space, level, &mut forms, geometry)?;
        Ok(forms)
    }

    /// One level of `layer` under `mask`: its row forms, their masked counts and its contents.
    /// `geometry` also accumulates the visible members' positions on a level served from its
    /// column alone, for a centroid or box read off the counts.
    pub(crate) fn read_level(
        &self,
        served: &ServedView<'_>,
        mask: &ShardedMask,
        layer: &RegisteredLayer,
        level: u32,
        geometry: bool,
    ) -> Result<ReadLevel> {
        let generation = served.generation;
        let forms = self.counted_level(
            served,
            mask,
            layer,
            level,
            match geometry {
                true => Geometry::declared(&layer.declaration),
                false => Geometry::None,
            },
        )?;
        let contents = layer.runs.get(level as usize).and_then(|runs| {
            self.level_content(generation, layer, level, forms.level_version(), runs)
        });
        Ok(ReadLevel {
            level,
            forms,
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

/// The lineage the records of `shard`'s form of `level` hold, the form being of `form_version`:
/// the level's lineage version in `written`, and the form's version where the store's slice has
/// moved past it.
pub(crate) fn lineage_at(
    written: &mosaica_lifecycle::membership::ArtifactStore,
    layer: &str,
    level: u32,
    shard: mosaica_types::ShardId,
    form_version: u64,
) -> crate::cut::LineageAt {
    let current = crate::write::slice_of(written, shard).level_version(layer, level);
    crate::cut::LineageAt {
        version: written.lineage_version(layer, level),
        behind: (form_version != current).then_some(form_version),
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

/// Per ordinal, the sum of each side's count in `sides`: a filtered histogram over every shard, in
/// `u64`, past what one shard's `u32` holds.
fn summed_counts(sides: &[Vec<u32>]) -> Vec<u64> {
    let mut out = vec![0u64; sides.iter().map(Vec::len).max().unwrap_or(0)];
    for side in sides {
        for (sum, count) in out.iter_mut().zip(side) {
            *sum += u64::from(*count);
        }
    }
    out
}

/// One level of a layer in every side of a request's view: each side's row form, and the masked
/// counts the request took in the sides it reads.
pub(crate) struct LevelForms {
    /// Each side's shard, in the order of the view's sides.
    shards: Vec<mosaica_types::ShardId>,
    /// Each side's form of the level.
    pub(crate) rows: Vec<Arc<ArtifactRows>>,
    /// The version of the level each side's form is of.
    versions: Vec<u64>,
    /// The lineage the first side's records hold ([`lineage_at`]).
    pub(crate) lineage: crate::cut::LineageAt,
    /// Whether the request reads each side ([`Engine::count_level`]).
    read: Vec<bool>,
    /// The level's figures over every side whose form has a column; `None` where none has, and
    /// until [`Engine::count_level`].
    counted: Option<Arc<LevelFigures>>,
}

impl LevelForms {
    /// `rows`, the forms of one level in shards 0, 1, … of a view, uncounted.
    #[cfg(test)]
    pub(crate) fn synthetic(rows: Vec<ArtifactRows>) -> LevelForms {
        let shards: Vec<_> = (0..rows.len() as u32)
            .map(|raw| mosaica_types::ShardId::new(raw).expect("a shard id"))
            .collect();
        LevelForms {
            read: vec![true; rows.len()],
            versions: vec![0; rows.len()],
            rows: rows.into_iter().map(Arc::new).collect(),
            shards,
            lineage: crate::cut::LineageAt::current(0),
            counted: None,
        }
    }

    /// These forms with `figures` as their figures over every side, for tests of what reads them.
    #[cfg(test)]
    pub(crate) fn with_figures(mut self, figures: LevelFigures) -> LevelForms {
        self.counted = Some(Arc::new(figures));
        self
    }

    /// The level's figures over every side the request reads, where each of them has a column: a
    /// shard's form is built from its own slice and takes its own layout, so a figure over the
    /// sides with a column alone would leave out the rows of a side the request reads without one.
    pub(crate) fn figures(&self) -> Option<&LevelFigures> {
        let every = self
            .rows
            .iter()
            .zip(&self.read)
            .all(|(rows, read)| !read || rows.column().is_some());
        self.counted.as_deref().filter(|_| every)
    }

    /// Every artifact with a member the viewer sees in the side at each place of `covering`, where
    /// that is every side the request reads and the figures cover each: what candidacy over a
    /// viewport covering the whole mask in every side reads off the figures rather than a scan.
    pub(crate) fn populated_over(&self, covering: &[usize]) -> Option<Bitmap> {
        let figures = self.figures()?;
        let read: Vec<usize> = (0..self.read.len()).filter(|&at| self.read[at]).collect();
        let lengths = read.iter().all(|&at| {
            self.rows[at]
                .column()
                .is_some_and(|column| column.len() == figures.len())
        });
        (lengths && read == covering).then(|| figures.populated())
    }

    /// The first side's form, which answers for what every side's form shares: the level's length,
    /// slots and records. Not for its layout, which is each side's own.
    pub(crate) fn first(&self) -> &Arc<ArtifactRows> {
        &self.rows[0]
    }

    /// The sum of the versions of the level every side's form is of, which the level's contents
    /// and derived geometry are keyed by: it moves whenever any side's does.
    pub(crate) fn level_version(&self) -> u64 {
        self.versions.iter().sum()
    }

    /// The version of the level the form of the side at `place` is of.
    pub(crate) fn version(&self, place: usize) -> u64 {
        self.versions[place]
    }

    /// The one verdict over this level for `served`'s viewer under `mask`, labels tested by
    /// `labels` and an attached artifact's target answered by `dependency_served`. `one` settles
    /// containment an artifact at a time, for a caller that asks about one.
    pub(crate) fn view<'a>(
        &'a self,
        served: &'a ServedView<'a>,
        mask: &'a ShardedMask,
        layer: &'a RegisteredLayer,
        labels: LabelGate<'a>,
        dependency_served: &'a dyn Fn(&Attachment) -> bool,
        one: bool,
    ) -> ArtifactView<'a, EffectiveMask> {
        let satisfied = served.session.satisfied();
        ArtifactView::of(served, layer, labels, dependency_served, |place, side| {
            let rows: &ArtifactRows = &self.rows[place];
            ArtifactSide {
                rows,
                mask: mask.read_leaf(side.shard.id()),
                denied: side.denied,
                containment: rows.partition().map(|partition| match one {
                    true => partition.answer_for_one(satisfied),
                    false => partition.answers(satisfied),
                }),
                counted: rows.column().is_some() && self.counted.is_some(),
            }
        })
        .counted_by(self.counted.clone())
    }

    /// Each side the request reads, its column and its visible rows in `filter_rows`, indexed like
    /// `served.sides`: what [`ReadLevel::filtered_counts`] walks. `None` unless every side the
    /// request reads has a column.
    fn filtered_inputs<'a>(
        &'a self,
        mask: &ShardedMask,
        filter_rows: &[impl std::borrow::Borrow<Bitmap>],
    ) -> Option<Vec<(&'a crate::row_column::RowColumn, Bitmap)>> {
        let mut sides = Vec::new();
        for ((rows, shard), filter_rows) in self.rows.iter().zip(&self.shards).zip(filter_rows) {
            let Some(leaf) = mask.read_leaf(*shard) else {
                continue;
            };
            sides.push((rows.column()?, leaf.visible_all().and(filter_rows.borrow())));
        }
        Some(sides)
    }

    /// Whether every side's form is served from its column alone, holding no per-artifact rows.
    pub(crate) fn column_only(&self) -> bool {
        self.rows
            .iter()
            .all(|rows| rows.column().is_some() && !rows.membership().rows_held())
    }

    /// The members of `ordinal` the viewer sees in each side the request reads, each beside the
    /// locator of that side's segments in `locators`, which follows the view's sides.
    pub(crate) fn visible<'l>(
        &self,
        ordinal: u32,
        mask: &ShardedMask,
        locators: &'l [RowLocator<'l>],
    ) -> Vec<(&'l RowLocator<'l>, Bitmap)> {
        self.rows
            .iter()
            .zip(&self.shards)
            .zip(locators)
            .filter_map(|((rows, shard), locator)| {
                let leaf = mask.read_leaf(*shard)?;
                Some((locator, rows.visible_rows(ordinal, leaf)))
            })
            .collect()
    }
}

/// One level of one layer, resolved for one viewer under one composed mask.
pub(crate) struct ReadLevel {
    pub(crate) level: u32,
    /// The level in every side of the view, counted.
    pub(crate) forms: LevelForms,
    /// Each artifact's visible members matching the filter over every side, on a level served from
    /// its column; set from [`ReadLevel::filtered_counts`].
    pub(crate) filtered: Option<Arc<Vec<u64>>>,
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
        mask: &'a ShardedMask,
        layer: &'a RegisteredLayer,
        dependency_served: &'a dyn Fn(&Attachment) -> bool,
    ) -> ArtifactView<'a, EffectiveMask> {
        let labels = engine.label_gate(served.session, &layer.declaration);
        self.forms
            .view(served, mask, layer, labels, dependency_served, false)
    }

    /// The content the verdict's `position` names, with any authored shape taken out of it, or
    /// `None` where it cannot be read back, which withholds the artifact.
    pub(crate) fn content(
        &self,
        engine: &Engine,
        layer: &RegisteredLayer,
        ordinal: u32,
        entity: EntityId,
        position: Option<u32>,
    ) -> Option<crate::viewport::Supplied> {
        engine.supplied_content(
            self.space_content.as_deref(),
            &layer.declaration,
            self.level,
            ordinal,
            entity,
            position,
            true,
            self.contents.as_deref(),
        )
    }

    /// Every artifact's visible members in `filter_rows` (indexed like `served.sides`) over every
    /// side: in each side the request reads, one walk of `visible ∩ filter` through the side's
    /// column, which needs no per-artifact membership, summed. `None` unless every side the request
    /// reads has a column.
    pub(crate) fn filtered_counts(
        &self,
        engine: &Engine,
        mask: &ShardedMask,
        filter_rows: &[impl std::borrow::Borrow<Bitmap>],
    ) -> Option<Arc<Vec<u64>>> {
        let sides: Vec<Vec<u32>> = self
            .forms
            .filtered_inputs(mask, filter_rows)?
            .into_iter()
            .map(|(column, visible)| engine.pool.install(|| column.histogram_over(&visible)))
            .collect();
        Some(Arc::new(summed_counts(&sides)))
    }

    /// How many of the artifact's visible members are in `filter_rows` over every side, from
    /// [`ReadLevel::filtered`] where the level is served from its column.
    pub(crate) fn matched_count(
        &self,
        ordinal: u32,
        mask: &ShardedMask,
        filter_rows: &[Bitmap],
    ) -> u64 {
        if let Some(histogram) = &self.filtered {
            return histogram.get(ordinal as usize).copied().unwrap_or(0);
        }
        self.forms
            .rows
            .iter()
            .zip(&self.forms.shards)
            .zip(filter_rows)
            .filter_map(|((rows, shard), filter_rows)| {
                let leaf = mask.read_leaf(*shard)?;
                // Either half of the side's form: a side served from its column alone holds no
                // per-artifact rows.
                Some(rows.visible_rows(ordinal, leaf).and_cardinality(filter_rows))
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaica_types::ShardId;

    /// A viewer seeing `leaves[s]` in shard `s`.
    fn mask(leaves: &[&[u32]]) -> ShardedMask {
        ShardedMask::new(leaves.iter().enumerate().map(|(raw, rows)| {
            (
                ShardId::new(raw as u32).unwrap(),
                EffectiveMask::of_rows(Bitmap::of(rows)),
            )
        }))
    }

    /// The figures over the sides with a column are the level's only where every side the
    /// request reads has one: a side read without one would drop out of every sum. A side the
    /// request leaves out adds nothing.
    #[test]
    fn a_read_shard_without_a_column_withholds_the_figures() {
        let forms = |read: [bool; 2]| {
            let mut forms = LevelForms::synthetic(vec![
                ArtifactRows::synthetic_column_only(&[Some(&[0, 1, 2]), Some(&[3, 4])], 5),
                ArtifactRows::synthetic(&[Some(&[0, 1]), Some(&[2, 3])], None),
            ])
            .with_figures(LevelFigures::of_counts(&[vec![1, 2]]));
            forms.read = read.to_vec();
            forms
        };
        assert!(forms([true, true]).figures().is_none());
        assert!(forms([false, true]).figures().is_none());
        let left_out = forms([true, false]);
        let figures = left_out.figures().expect("the side without a column is not read");
        assert_eq!((figures.get(0), figures.get(1)), (1, 2));
        assert!(LevelForms::synthetic(Vec::new()).figures().is_none());
    }

    /// A level whose first shard's form is served from its column alone and whose second holds
    /// its rows: the filtered counts are not taken from the column shards alone, and each
    /// artifact's matched count reads both shards' members, whichever half of its form holds them.
    #[test]
    fn filtered_counts_read_every_shard_or_none() {
        let column_only =
            ArtifactRows::synthetic_column_only(&[Some(&[0, 1, 2]), Some(&[3, 4])], 5);
        let held = ArtifactRows::synthetic(&[Some(&[0, 1]), Some(&[2, 3])], None);
        assert!(column_only.column().is_some() && !column_only.membership().rows_held());
        let read = ReadLevel {
            level: 0,
            forms: LevelForms::synthetic(vec![column_only, held]),
            filtered: None,
            contents: None,
            space_content: None,
        };
        let mask = mask(&[&[0, 2, 3, 4], &[1, 2, 3]]);
        let filter = [Bitmap::of(&[2, 3]), Bitmap::of(&[1, 3])];
        assert!(read.forms.filtered_inputs(&mask, &filter).is_none());
        assert!(!read.forms.column_only());
        // Ordinal 0: row 2 of the first shard and row 1 of the second; ordinal 1: row 3 of each.
        assert_eq!(read.matched_count(0, &mask, &filter), 2);
        assert_eq!(read.matched_count(1, &mask, &filter), 2);

        let both = LevelForms::synthetic(vec![
            ArtifactRows::synthetic_column_only(&[Some(&[0, 1, 2]), Some(&[3, 4])], 5),
            ArtifactRows::synthetic_column_only(&[Some(&[0, 1]), Some(&[2, 3])], 4),
        ]);
        assert!(both.column_only());
        let inputs = both.filtered_inputs(&mask, &filter).expect("every shard has a column");
        let histograms: Vec<Vec<u32>> = inputs
            .iter()
            .map(|(column, visible)| column.histogram_over(visible))
            .collect();
        assert_eq!(summed_counts(&histograms), vec![2, 2]);
    }

    /// The shards' filtered counts are added past what a `u32` holds.
    #[test]
    fn filtered_counts_are_widened_before_they_are_added() {
        assert_eq!(
            summed_counts(&[vec![u32::MAX, 1], vec![u32::MAX]]),
            vec![2 * u64::from(u32::MAX), 1]
        );
    }
}
