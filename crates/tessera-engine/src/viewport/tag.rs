//! The per-point cluster tag of a layer with no lineage (flat, stacked and tiered), read from each
//! selected level's per-row labels and this viewer's verdict on each label, with no walk of the
//! layer's artifacts.
//!
//! The answer is the one [`crate::membership_column`] resolves against the walk's served set. On
//! a layer with no lineage the cut serves every artifact that passes, so an artifact holding a
//! served point is served exactly when it passes its verdict and its content can be read. A point
//! is therefore tagged with the finest selected level at which a label of its row passes, and the
//! lowest `tessera_id` where several labels at that level pass.
//!
//! A row's labels are read from the band-order copy where the point came from a band entry of the
//! segment the copy follows, from the level's row column where it is served from one, and from
//! its label column otherwise. A row neither covers (a row above the base on a level served
//! artifact-major, or a level whose label column is not current) is placed by intersecting the
//! level's memberships with those rows alone.
//!
//! A layer whose artifacts may be dropped by a reconciliation of the whole response keeps the
//! walk: one with a lineage, where the cut and the budget decide what is served, and one depending
//! on a layer the same request names, whose artifact goes when its target is not served.

use rustc_hash::FxHashMap;
use tessera_store::bands::BandLabels;
use tessera_store::membership::{LabelColumnPack, ROW_COLUMN_HOLE};
use tessera_types::layer::RegisteredLayer;

use super::*;
use crate::artifacts::{ArtifactRows, ArtifactView};
use crate::row_column::RowColumn;
use artifacts::level_is_selected;

/// How one requested layer's points are tagged.
pub(super) enum Tagging {
    /// From the labels of its selected levels.
    Labels(Box<RegisteredLayer>),
    /// From the served set of the walk.
    Walk,
    /// Not at all: the layer is not drawn on this view, or is gone, deleted or suppressed, and
    /// the walk serves nothing from it either.
    None,
}

/// One served point, as its tag is read: its row, the segment holding it and its entry in that
/// segment's bands where the band route served it.
pub(super) struct TaggedPoint {
    pub(super) row: u32,
    segment: usize,
    entry: Option<u32>,
}

/// Every point the emit pass will gather, in its order.
pub(super) fn tagged_points(served: &ServedView<'_>, swept: &[TileSweepOut<'_>]) -> Vec<TaggedPoint> {
    let mut points = Vec::with_capacity(swept.iter().map(|ts| ts.rows.len()).sum());
    for ts in swept {
        for (i, &row) in ts.rows.iter().enumerate() {
            let (segment, _) = segment_holding(&served.segments, row)
                .expect("a selected row lies in the view's row space");
            points.push(TaggedPoint {
                row,
                segment,
                entry: ts.entries.as_ref().map(|entries| entries[i]),
            });
        }
    }
    points
}

/// Where one level's labels are read from for this request.
struct LevelLabels {
    rows: RowLabels,
    /// The band-order copy, and the position in the view's segment list of the segment it follows.
    copy: Option<(usize, Arc<BandLabels>)>,
}

enum RowLabels {
    /// The row column the level is served from, its tail and amendments included.
    Column,
    /// The label column of a level served artifact-major, over the base rows.
    Pack(Arc<LabelColumnPack>),
    /// None: every row is placed from the memberships.
    Memberships,
}

impl LevelLabels {
    /// Push `point`'s labels onto `out`. `false` where this source does not cover its row.
    fn read(&self, column: Option<&RowColumn>, point: &TaggedPoint, out: &mut Vec<u32>) -> bool {
        if let (Some(e), Some((segment, copy))) = (point.entry, &self.copy) {
            if point.segment == *segment {
                let label = copy.label(e as usize);
                if label != ROW_COLUMN_HOLE {
                    out.push(label);
                }
                return true;
            }
        }
        match &self.rows {
            RowLabels::Column => {
                if let Some(column) = column {
                    column.for_each_label(point.row, |label| out.push(label));
                }
                true
            }
            RowLabels::Pack(pack) if point.row < pack.rows() => {
                let label = pack.label(point.row as usize);
                if label != ROW_COLUMN_HOLE {
                    out.push(label);
                }
                true
            }
            RowLabels::Pack(_) | RowLabels::Memberships => false,
        }
    }
}

/// Each label's tag at one level, decided once a request: indexed by ordinal where the level is
/// small enough, and in a map otherwise.
enum Verdicts {
    Dense(Vec<Option<Option<u64>>>),
    Sparse(FxHashMap<u32, Option<u64>>),
}

impl Verdicts {
    /// The most ordinals a level is indexed over: 16 MiB a request at most.
    const DENSE_MAX: usize = 1 << 20;

    fn over(ordinals: usize) -> Self {
        if ordinals <= Self::DENSE_MAX {
            Verdicts::Dense(vec![None; ordinals])
        } else {
            Verdicts::Sparse(FxHashMap::default())
        }
    }

    fn get_or(&mut self, ordinal: u32, decide: impl FnOnce() -> Option<u64>) -> Option<u64> {
        match self {
            Verdicts::Dense(held) => match held.get_mut(ordinal as usize) {
                Some(Some(tag)) => *tag,
                Some(slot) => *slot.insert(decide()),
                // An ordinal past the level's end names no artifact.
                None => None,
            },
            Verdicts::Sparse(held) => *held.entry(ordinal).or_insert_with(decide),
        }
    }
}

impl Engine {
    /// How each of `names` is tagged, in the same order.
    pub(super) fn taggings(&self, served: &ServedView<'_>, names: &[String]) -> Vec<Tagging> {
        names
            .iter()
            .map(|name| {
                let Some(registered) = self.write.live().registered_layer(name) else {
                    return Tagging::None;
                };
                let declaration = &registered.declaration;
                if !declaration.views.iter().any(|v| v == served.name)
                    || served.generation.overlay.is_deleted(registered.entity)
                    || served.generation.overlay.is_suppressed(registered.entity)
                {
                    return Tagging::None;
                }
                if !self.switches.tags_from_labels.load(Ordering::Relaxed)
                    || artifacts::lineage_kind(declaration.hierarchy.kind).is_some()
                    || declaration.depends_on.iter().any(|target| names.contains(target))
                {
                    return Tagging::Walk;
                }
                Tagging::Labels(Box::new(registered))
            })
            .collect()
    }

    /// One layer's tag for every point, aligned to `points`.
    pub(super) fn tag_layer(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        req: &ViewportRequest<'_>,
        dependency_served: &dyn Fn(&tessera_lifecycle::membership::Attachment) -> bool,
        registered: &RegisteredLayer,
        points: &[TaggedPoint],
    ) -> Vec<Option<u64>> {
        let generation = served.generation;
        let declaration = &registered.declaration;
        let name = &declaration.name;
        let vocabulary = predicate_vocabulary(generation, declaration);
        let source = generation.partition_source();
        let shard = generation.bundle.manifest.identity.shard_id;
        let base = served.segments.first().map(|&(segment, _)| segment);

        let mut tags: Vec<Option<u64>> = vec![None; points.len()];
        let mut open: Vec<usize> = (0..points.len()).collect();
        let levels: Vec<u32> = (0..registered.runs.len() as u32)
            .filter(|&level| level_is_selected(req.levels, &declaration.levels, level, req.zoom))
            .collect();
        // The finest level first: a point tagged there is not asked about a coarser one.
        for &level in levels.iter().rev() {
            if open.is_empty() {
                break;
            }
            let runs = &registered.runs[level as usize];
            let ((rows, level_version), _) =
                self.level_form(served, registered, vocabulary, &source, level);
            let rows: &ArtifactRows = &rows;
            let labels = LevelLabels {
                rows: if rows.column().is_some() {
                    RowLabels::Column
                } else {
                    match self.artifact_projections.level_labels(
                        &generation.prefix,
                        served.name,
                        name,
                        level,
                        level_version,
                    ) {
                        Some(pack) => RowLabels::Pack(pack),
                        None => RowLabels::Memberships,
                    }
                },
                copy: base.and_then(|base| {
                    self.artifact_projections
                        .band_labels(
                            &generation.prefix,
                            served.name,
                            name,
                            level,
                            level_version,
                            base,
                        )
                        .map(|copy| (0, copy))
                }),
            };

            // The verdict as the walk reaches it, asked once per distinct label. The masked
            // counts are read only where the criterion needs more than the member in hand.
            let needs_counts = !matches!(
                declaration.require_member_visibility,
                None | Some(tessera_types::layer::ExistenceCriterion::Count(0 | 1))
            );
            let counts = needs_counts
                .then(|| {
                    self.masked_counts(
                        &served.mask_identity,
                        served.name,
                        name,
                        level,
                        level_version,
                        rows,
                        mask,
                        crate::artifacts::derives_accumulated_geometry(declaration)
                            .then_some(&served.segments[..]),
                    )
                })
                .flatten();
            let view = ArtifactView {
                declaration,
                overlay: &generation.overlay,
                labels: self.label_gate(served.session, declaration),
                layer_reachable: true,
                rows,
                mask,
                dependency_served,
                containment: rows
                    .partition()
                    .map(|p| p.answers(served.session.satisfied())),
                denied: served.denied,
                counts,
            };
            let mut contents: Option<Arc<crate::artifact_content::LevelContent>> = None;
            let mut verdicts = Verdicts::over(rows.len());
            let mut tessera_id = |ordinal: u32| -> Option<u64> {
                verdicts.get_or(ordinal, || {
                    let entity = runs.entity_of(ordinal as u64).map(EntityId::new)?;
                    let rank = view.serves_visible_member(entity, ordinal).ok()?;
                    // The walk withholds an artifact whose content cannot be read back.
                    let table = (rank.is_some() && !declaration.content.supplied.is_empty())
                        .then(|| {
                            contents
                                .get_or_insert_with(|| {
                                    self.level_contents.get_or_build(
                                        name,
                                        level,
                                        level_version,
                                        generation.segments_version,
                                        || {
                                            crate::artifact_content::LevelContent::build(
                                                generation.filter_columns.records(),
                                                runs,
                                            )
                                        },
                                    )
                                })
                                .clone()
                        });
                    self.supplied_content(
                        generation,
                        declaration,
                        level,
                        ordinal,
                        entity,
                        rank,
                        false,
                        table.as_deref(),
                    )?;
                    self.identity_key
                        .forward(shard, entity)
                        .ok()
                        .map(|id| id.raw())
                })
            };
            let mut lowest = |labels: &[u32]| -> Option<u64> {
                labels.iter().filter_map(|&ordinal| tessera_id(ordinal)).min()
            };

            let mut still_open = Vec::new();
            let mut unread = Vec::new();
            let mut held = Vec::new();
            for &i in &open {
                held.clear();
                if !labels.read(rows.column(), &points[i], &mut held) {
                    unread.push(i);
                    continue;
                }
                match lowest(&held) {
                    Some(id) => tags[i] = Some(id),
                    None => still_open.push(i),
                }
            }
            if !unread.is_empty() {
                let mut at: FxHashMap<u32, usize> = FxHashMap::default();
                let mut set = croaring::Bitmap::new();
                for &i in &unread {
                    at.insert(points[i].row, i);
                    set.add(points[i].row);
                }
                let mut found: FxHashMap<usize, Vec<u32>> = FxHashMap::default();
                for ordinal in rows.index().candidates(&set).iter() {
                    let Some(members) = rows.get(ordinal) else {
                        continue;
                    };
                    for row in members.and(&set).iter() {
                        found.entry(at[&row]).or_default().push(ordinal);
                    }
                }
                for i in unread {
                    match found.get(&i).and_then(|labels| lowest(labels)) {
                        Some(id) => tags[i] = Some(id),
                        None => still_open.push(i),
                    }
                }
            }
            open = still_open;
        }
        tags
    }
}
