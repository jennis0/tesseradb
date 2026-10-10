//! A grouping by the artifacts of one level of a layer.
//!
//! Which artifacts a table can list is what the artifacts frame serves this viewer: the layer's
//! reach, then each artifact's verdict over the viewer's authorised set, with its label, its
//! existence criterion and, for an attached artifact, its target's verdict, and content that reads
//! back. On a treed layer it is the cut the map draws ([`Cut`]), taken over the artifacts with a
//! visible member in the cut's tiles; where the layer draws ancestors beside their descendants,
//! only those with nothing drawn beneath them. The filtered set never decides it. An artifact's count is its visible members in the set.
//! The rest are the set's items in a served artifact and in no listed one, and none the items in
//! no served artifact, so an item held only by a withheld artifact counts in none. Artifacts can
//! overlap, so a table's rows can add to more than the set's size.

use croaring::Bitmap;
use rustc_hash::{FxHashMap, FxHashSet};
use mosaica_types::layer::{HierarchyKind, RegisteredLayer, ServingLayout};
use mosaica_types::{EntityId, MosaicaId};

use super::set::Cx;
use super::table::{Groups, Key};
use super::{AggregateRefused, AggregateTimings, Cut, Pick};
use crate::cells::{pass, CellSet, LabelTable, RowGroups};
use crate::engine::Engine;
use crate::error::{EngineError, Result};
use crate::layer_read::{check_level, LayerRefusal, ReadLevel};
use crate::records::RecordsRefused;
use crate::session::Session;
use crate::viewport::DependencyContext;
use crate::Generation;

/// How many ordinals the verdict walk takes between two readings of the cancellation token.
const CHUNK: u32 = 1024;

/// One level of a layer as one request counts it.
pub(super) struct Layer {
    name: String,
    /// The layer's own entity when the read began: a layer registered since under the name is
    /// another layer, and serves nothing to this read.
    entity: EntityId,
    level: u32,
    pick: Pick<MosaicaId>,
    cut: Option<Cut>,
    palette: Option<u8>,
}

/// A level's served artifacts under one page, and the forms their members are read from.
pub(super) struct Served {
    read: Option<ReadLevel>,
    /// Served ordinals, ascending.
    ordinals: Vec<u32>,
    /// Listed ordinals, in table order.
    listed: Vec<u32>,
    /// On a level counted artifact by artifact, each group's items as a set of their own: in the
    /// set, and in the reference. `None` on a level read through its row-addressed column.
    parts: Option<(Vec<Bitmap>, Option<Vec<Bitmap>>)>,
}

impl Layer {
    /// The level `layer` and `level` name, where this viewer may read it in `view`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn of(
        engine: &Engine,
        session: &Session,
        generation: &Generation,
        view: &str,
        layer: &str,
        level: Option<u32>,
        pick: &Pick<MosaicaId>,
        cut: Option<Cut>,
        palette: Option<u8>,
    ) -> Result<Layer> {
        let refused = |refusal| {
            EngineError::RecordsRefused(match refusal {
                LayerRefusal::Unknown => RecordsRefused::UnknownLayer(layer.to_string()),
                LayerRefusal::OneLevel(_) => RecordsRefused::OneLevel(layer.to_string()),
                LayerRefusal::NoSuchLevel { held } => RecordsRefused::NoSuchLevel {
                    layer: layer.to_string(),
                    held,
                },
            })
        };
        let registered = engine
            .readable_layer(session, generation, layer, view)
            .map_err(refused)?;
        check_level(&registered, level).map_err(refused)?;
        let levelled = matches!(
            registered.declaration.hierarchy.kind,
            HierarchyKind::Stacked | HierarchyKind::Tiered
        );
        if levelled && level.is_none() {
            return Err(EngineError::AggregateRefused(
                AggregateRefused::LevelRequired(layer.to_string()),
            ));
        }
        let treed = matches!(
            registered.declaration.hierarchy.kind,
            HierarchyKind::Nested | HierarchyKind::Dag
        );
        match (cut, pick) {
            (Some(_), _) if !treed => {
                return Err(EngineError::AggregateRefused(
                    AggregateRefused::CutOnUntreed(layer.to_string()),
                ))
            }
            (None, Pick::Top(_)) if treed => {
                return Err(EngineError::AggregateRefused(
                    AggregateRefused::CutRequired(layer.to_string()),
                ))
            }
            _ => {}
        }
        if let Some(cut) = cut {
            engine.tile_extent(generation, view, cut.zoom, cut.bbox, None)?;
        }
        Ok(Layer {
            name: layer.to_string(),
            entity: registered.entity,
            level: level.unwrap_or(0),
            pick: pick.clone(),
            cut,
            palette,
        })
    }

    /// The layer's own entity, which a cursor over this grouping is bound to.
    pub(super) fn entity(&self) -> u64 {
        self.entity.raw()
    }

    /// The table's groups under `cx`: the listed artifacts, `chosen` where the table has begun,
    /// and each group's counts in the set and the reference.
    ///
    /// An attached artifact's members are its target's, as `/v1/artifacts` counts them. A level
    /// with a row-addressed column and no attachments is counted by walking the set's rows through
    /// the column; any other is counted artifact by artifact, each served artifact's members in the
    /// set read once and kept as the groups' own sets.
    pub(super) fn groups(
        &self,
        cx: &Cx<'_>,
        chosen: Option<&[u64]>,
        timings: &mut AggregateTimings,
    ) -> Result<(Groups, Served)> {
        let counting = std::time::Instant::now();
        let engine = cx.engine;
        let served_view = &cx.open.served;
        let mask = &cx.open.mask;
        let generation = cx.generation;
        let shard = generation.bundle.manifest.identity.shard_id;
        let registered = engine
            .readable_layer(
                served_view.session,
                generation,
                &self.name,
                served_view.name,
            )
            .ok()
            .filter(|layer| layer.entity == self.entity)
            .filter(|layer| (self.level as usize) < layer.runs.len());
        let reachable = engine.reachable_layers(served_view.session);
        let context = DependencyContext::new(served_view, mask, &reachable);
        let dependency_served = engine.dependency_gate(&context);
        let mut ordinals: Vec<u32> = Vec::new();
        // Each served ordinal's rank, which its content is read under.
        let mut ranks: FxHashMap<u32, Option<u32>> = FxHashMap::default();
        let mut targets = Targets::default();
        let read = match &registered {
            None => None,
            Some(layer) => {
                // With a cut, the level's figures are the entry the map's treed frame reads.
                let read =
                    engine.read_level(served_view, mask, layer, self.level, self.cut.is_some())?;
                let readable = |ordinal: u32, entity: EntityId, rank: Option<u32>| {
                    read.content(engine, generation, layer, ordinal, entity, rank)
                        .is_some()
                };
                match &self.cut {
                    Some(cut) => {
                        let drawn = engine.drawn_cut(
                            served_view,
                            mask,
                            cut.zoom,
                            cut.bbox,
                            cut.budget,
                            &self.name,
                            &dependency_served,
                            cx.cancel,
                        )?;
                        if let Some(drawn) = drawn {
                            let listed = match layer.declaration.hierarchy.prune_children {
                                true => drawn.served.clone(),
                                false => crate::cut::frontier_of(&drawn.lineage, &drawn.served),
                            };
                            for (ordinal, entity, _, rank) in drawn.passing_in(&listed) {
                                if readable(ordinal, entity, rank) {
                                    ordinals.push(ordinal);
                                    ranks.insert(ordinal, rank);
                                }
                            }
                        }
                    }
                    None => {
                        let view = read.view(engine, served_view, mask, layer, &dependency_served);
                        let runs = &layer.runs[self.level as usize];
                        for ordinal in 0..read.rows.len() as u32 {
                            if ordinal % CHUNK == 0 {
                                cx.check_cancelled()?;
                            }
                            let Some(entity) =
                                runs.entity_of(u64::from(ordinal)).map(EntityId::new)
                            else {
                                continue;
                            };
                            let crate::artifacts::ArtifactVerdict::Serve { rank, .. } =
                                view.verdict(entity, ordinal)
                            else {
                                continue;
                            };
                            if readable(ordinal, entity, rank) {
                                ordinals.push(ordinal);
                                ranks.insert(ordinal, rank);
                            }
                        }
                    }
                }
                targets = Targets::of(cx, &read, &ordinals, &dependency_served)?;
                context.finish()?;
                Some(read)
            }
        };
        let entity_at = |ordinal: u32| -> Option<u64> {
            registered.as_ref()?.runs[self.level as usize].entity_of(u64::from(ordinal))
        };
        let mosaica_id = |ordinal: u32| -> Option<u64> {
            engine
                .identity_key
                .forward(shard, EntityId::new(entity_at(ordinal)?))
                .ok()
                .map(|id| id.raw())
        };
        let attached = registered
            .as_ref()
            .is_some_and(|layer| !layer.declaration.depends_on.is_empty());
        let row_major = !attached && read.as_ref().is_some_and(|r| r.rows.column().is_some());
        // Over the whole visible set, each artifact's count is its figure, already held.
        let figures = read
            .as_ref()
            .filter(|_| row_major && cx.sets.set.is_whole())
            .and_then(|read| read.counts.as_deref());
        // And where the column is a label column, a row is in at most one artifact, so the groups'
        // sizes are sums of those figures.
        let summed = figures.is_some()
            && read
                .as_ref()
                .and_then(|read| read.rows.column())
                .is_some_and(|column| column.layout() == ServingLayout::RowMajorLabel);
        let set_rows = cx.sets.set.rows(cx);
        let reference_rows = cx.sets.reference.as_ref().map(|r| r.rows(cx));
        let (counts, members) = match &read {
            None => (
                Vec::new(),
                Some((Vec::new(), reference_rows.map(|_| Vec::new()))),
            ),
            Some(read) if row_major => {
                // A narrower set is counted by scanning its rows.
                let counts = match figures {
                    Some(figures) => ordinals.iter().map(|&o| figures.get(o)).collect(),
                    None => {
                        let histogram = read
                            .filtered_counts(engine, mask, set_rows)
                            .expect("a level with a column counts through it");
                        ordinals
                            .iter()
                            .map(|&o| u64::from(histogram.get(o as usize).copied().unwrap_or(0)))
                            .collect()
                    }
                };
                (counts, None)
            }
            Some(read) => {
                let of = |rows: &Bitmap| -> Vec<Bitmap> {
                    engine.pool.install(|| {
                        use rayon::prelude::*;
                        ordinals
                            .par_iter()
                            .map(|&o| members(read, &targets, mask, o, rows))
                            .collect()
                    })
                };
                let in_set = of(set_rows);
                let counts = in_set.iter().map(Bitmap::cardinality).collect();
                (counts, Some((in_set, reference_rows.map(of))))
            }
        };
        // A resumed table's artifacts are carried by entity: an ordinal that holds another entity
        // now, or none, stands for nothing.
        let ordinal_of = |entity: u64| -> u32 {
            let Some((name, level, ordinal)) =
                engine.write.live().locate_artifact(EntityId::new(entity))
            else {
                return u32::MAX;
            };
            if name == self.name && level == self.level && entity_at(ordinal) == Some(entity) {
                ordinal
            } else {
                u32::MAX
            }
        };
        let listed: Vec<u32> = match chosen {
            Some(chosen) => chosen.iter().map(|&entity| ordinal_of(entity)).collect(),
            None => match &self.pick {
                Pick::Top(n) => {
                    let mut ranked: Vec<(u64, u64, u32)> = ordinals
                        .iter()
                        .zip(&counts)
                        .filter(|&(_, &count)| count > 0)
                        .filter_map(|(&o, &count)| Some((count, mosaica_id(o)?, o)))
                        .collect();
                    ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
                    ranked
                        .into_iter()
                        .take(*n as usize)
                        .map(|(_, _, o)| o)
                        .collect()
                }
                Pick::Named(ids) => {
                    let mut listed: Vec<u32> = Vec::with_capacity(ids.len());
                    for &id in ids {
                        let (id_shard, entity) = engine.identity_key.invert(id);
                        if id_shard != shard {
                            continue;
                        }
                        let Some((name, level, ordinal)) =
                            engine.write.live().locate_artifact(entity)
                        else {
                            continue;
                        };
                        // The registry can be ahead of the page's generation: the ordinal must
                        // hold this entity in the runs the page reads.
                        if name == self.name
                            && level == self.level
                            && entity_at(ordinal) == Some(entity.raw())
                            && ordinals.binary_search(&ordinal).is_ok()
                            && !listed.contains(&ordinal)
                        {
                            listed.push(ordinal);
                        }
                    }
                    listed
                }
            },
        };
        let mut served = Served {
            read,
            ordinals,
            listed,
            parts: None,
        };
        let sizes: Vec<(u64, u64)> = match members {
            Some((in_set, in_reference)) => {
                let set_parts = served.split(&in_set, set_rows);
                let reference_parts = match (in_reference, reference_rows) {
                    (Some(members), Some(rows)) => Some(served.split(&members, rows)),
                    _ => None,
                };
                let sizes = set_parts
                    .iter()
                    .enumerate()
                    .map(|(g, part)| {
                        let reference = reference_parts.as_ref().map_or(0, |r| r[g].cardinality());
                        (part.cardinality(), reference)
                    })
                    .collect();
                served.parts = Some((set_parts, reference_parts));
                sizes
            }
            None => {
                let in_set = match summed {
                    true => served.summed_sizes(&counts, cx.sets.set.size()),
                    false => served.pass_sizes(cx, cx.sets.set.cells(cx))?,
                };
                let in_reference = match &cx.sets.reference {
                    Some(reference) if summed && reference.is_whole() => Some(in_set.clone()),
                    Some(reference) => Some(served.pass_sizes(cx, reference.cells(cx))?),
                    None => None,
                };
                in_set
                    .iter()
                    .enumerate()
                    .map(|(g, &s)| (s, in_reference.as_ref().map_or(0, |r| r[g])))
                    .collect()
            }
        };
        // Each listed artifact's name, as `/v1/artifacts/browse` names it to this viewer.
        let titles = match (&served.read, &registered) {
            (Some(read), Some(layer)) => {
                let attached = crate::browse::AttachedLevels::default();
                let namer = crate::browse::Namer::new(
                    engine,
                    served_view,
                    mask,
                    layer,
                    &reachable,
                    &context,
                    &dependency_served,
                    &attached,
                );
                served
                    .listed
                    .iter()
                    .map(|&o| {
                        let rank = *ranks.get(&o)?;
                        let entity = EntityId::new(entity_at(o)?);
                        let own = read
                            .content(engine, generation, layer, o, entity, rank)
                            .and_then(|content| content.name());
                        namer.name(own, self.level, o)
                    })
                    .collect()
            }
            _ => vec![None; served.listed.len()],
        };
        // Each listed artifact's slot, over this viewer's whole tree and not the cut or the set.
        let slots = match (self.palette, &registered) {
            (Some(palette), Some(layer)) => {
                let slots = engine.cluster_slots(
                    served_view,
                    mask,
                    layer,
                    self.level,
                    palette,
                    &dependency_served,
                )?;
                Some(
                    served
                        .listed
                        .iter()
                        .map(|&o| slots.as_ref().and_then(|slots| slots.get(o)))
                        .collect(),
                )
            }
            (Some(_), None) => Some(vec![None; served.listed.len()]),
            (None, _) => None,
        };
        context.finish()?;
        let named = matches!(self.pick, Pick::Named(_));
        let chosen: Vec<u64> = match chosen {
            Some(chosen) => chosen.to_vec(),
            None => served
                .listed
                .iter()
                .map(|&o| entity_at(o).expect("a listed artifact is held"))
                .collect(),
        };
        let keys = chosen
            .iter()
            .map(|&entity| {
                let id = engine.identity_key.forward(shard, EntityId::new(entity));
                Key::Id(id.map_or(0, |id| id.raw()))
            })
            .collect();
        timings.count_ns += counting.elapsed().as_nanos() as u64;
        Ok((
            Groups {
                chosen,
                sizes,
                always: served
                    .listed
                    .iter()
                    .map(|&o| named && served.serves(o))
                    .collect(),
                keys,
                titles: Some(titles),
                slots,
                distinct: counts.iter().filter(|&&n| n > 0).count() as u64,
                sample: None,
            },
            served,
        ))
    }
}

/// What the attached artifacts among a level's served ones hang from: each target level read
/// under the page's mask, and the attached ordinals whose target this viewer is served, its slot
/// still holding the entity the edge names and its content readable, as `/v1/artifacts` requires
/// before it counts a target's members.
#[derive(Default)]
struct Targets {
    levels: FxHashMap<(String, u32), ReadLevel>,
    counted: FxHashSet<u32>,
}

impl Targets {
    fn of(
        cx: &Cx<'_>,
        read: &ReadLevel,
        ordinals: &[u32],
        dependency_served: &dyn Fn(&mosaica_lifecycle::membership::Attachment) -> bool,
    ) -> Result<Targets> {
        let (engine, served, mask) = (cx.engine, &cx.open.served, &cx.open.mask);
        let mut targets = Targets::default();
        let mut layers: FxHashMap<String, Option<RegisteredLayer>> = FxHashMap::default();
        for &ordinal in ordinals {
            let Some(attachment) = read.rows.attachment(ordinal) else {
                continue;
            };
            let Some(layer) = layers
                .entry(attachment.layer.clone())
                .or_insert_with(|| engine.write.live().registered_layer(&attachment.layer))
            else {
                continue;
            };
            let Some(runs) = layer.runs.get(attachment.level as usize) else {
                continue;
            };
            if runs.entity_of(u64::from(attachment.ordinal)) != Some(attachment.entity.raw()) {
                continue;
            }
            let key = (attachment.layer.clone(), attachment.level);
            let level = match targets.levels.entry(key) {
                std::collections::hash_map::Entry::Occupied(held) => held.into_mut(),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(engine.read_level(served, mask, layer, attachment.level, false)?)
                }
            };
            let view = level.view(engine, served, mask, layer, dependency_served);
            let crate::artifacts::ArtifactVerdict::Serve { rank, .. } =
                view.verdict(attachment.entity, attachment.ordinal)
            else {
                continue;
            };
            let content = level.content(
                engine,
                cx.generation,
                layer,
                attachment.ordinal,
                attachment.entity,
                rank,
            );
            if content.is_some() {
                targets.counted.insert(ordinal);
            }
        }
        Ok(targets)
    }
}

/// The members of the artifact at `ordinal` among `rows`, which lie inside the mask: its target's
/// where it hangs from one, and none where that target is not counted.
fn members(
    read: &ReadLevel,
    targets: &Targets,
    mask: &crate::compose::EffectiveMask,
    ordinal: u32,
    rows: &Bitmap,
) -> Bitmap {
    let (level, ordinal) = match read.rows.attachment(ordinal) {
        None => (read, ordinal),
        Some(attachment) => {
            let key = (attachment.layer.clone(), attachment.level);
            match targets.levels.get(&key) {
                Some(target) if targets.counted.contains(&ordinal) => (target, attachment.ordinal),
                _ => return Bitmap::new(),
            }
        }
    };
    match level.rows.get(ordinal) {
        Some(members) => members.and(rows),
        None => level.rows.visible_rows(ordinal, mask).and(rows),
    }
}

impl Served {
    /// Whether this viewer is served the artifact at `ordinal`.
    fn serves(&self, ordinal: u32) -> bool {
        self.ordinals.binary_search(&ordinal).is_ok()
    }

    /// The labels table a pass over a level read through its column reads; `None` on a level
    /// counted artifact by artifact.
    pub(super) fn label_table(&self) -> Option<LabelTable> {
        if self.parts.is_some() {
            return None;
        }
        let read = self.read.as_ref()?;
        let listed: Vec<u32> = self
            .listed
            .iter()
            .map(|&o| if self.serves(o) { o } else { u32::MAX })
            .collect();
        Some(LabelTable::new(
            read.rows.len(),
            self.ordinals.iter().copied(),
            &listed,
        ))
    }

    /// Where a pass over a level read through its column reads each row's groups, by `table`.
    pub(super) fn row_groups<'s>(&'s self, table: &'s LabelTable) -> RowGroups<'s> {
        let column = self
            .read
            .as_ref()
            .and_then(|read| read.rows.column())
            .expect("a level read through its column has one");
        RowGroups::Labels { column, table }
    }

    /// Each group's items in the set and in the reference, on a level counted artifact by
    /// artifact.
    pub(super) fn parts(&self) -> Option<&(Vec<Bitmap>, Option<Vec<Bitmap>>)> {
        self.parts.as_ref()
    }

    /// Each group's items among `cells`, by one pass through the level's column: the listed
    /// artifacts, the rest, and none.
    fn pass_sizes(&self, cx: &Cx<'_>, cells: CellSet<'_>) -> Result<Vec<u64>> {
        cx.check_cancelled()?;
        let mut sizes = vec![0u64; self.listed.len() + 2];
        let Some(table) = self.label_table() else {
            return Ok(sizes);
        };
        let groups = self.row_groups(&table);
        let counted = cx
            .engine
            .pool
            .install(|| pass(cells, cx.segments(), 0, &groups));
        for entry in counted {
            sizes[entry.group as usize] += entry.count;
        }
        Ok(sizes)
    }

    /// Each group's items in a set of `size` items where no item is in two artifacts, from each
    /// served artifact's count in the set (`counts`, in the order of the served ordinals): the
    /// listed artifacts, the rest, and none.
    fn summed_sizes(&self, counts: &[u64], size: u64) -> Vec<u64> {
        let count_of = |ordinal: u32| match self.ordinals.binary_search(&ordinal) {
            Ok(at) => counts[at],
            Err(_) => 0,
        };
        let listed: Vec<u64> = self.listed.iter().map(|&o| count_of(o)).collect();
        let served: u64 = counts.iter().sum();
        let rest = served - listed.iter().sum::<u64>();
        listed.into_iter().chain([rest, size - served]).collect()
    }

    /// The groups' own sets from each served artifact's members among `rows`, in the order of
    /// `ordinals`: each listed artifact's, the rest, and none.
    fn split(&self, members: &[Bitmap], rows: &Bitmap) -> Vec<Bitmap> {
        let listed: Vec<Bitmap> = self
            .listed
            .iter()
            .map(|o| match self.ordinals.binary_search(o) {
                Ok(i) => members[i].clone(),
                Err(_) => Bitmap::new(),
            })
            .collect();
        let union = |parts: &[Bitmap]| {
            let refs: Vec<&Bitmap> = parts.iter().collect();
            Bitmap::fast_or(&refs)
        };
        let in_served = union(members);
        let in_listed = union(&listed);
        let rest = in_served.andnot(&in_listed);
        let none = rows.andnot(&in_served);
        listed.into_iter().chain([rest, none]).collect()
    }
}
