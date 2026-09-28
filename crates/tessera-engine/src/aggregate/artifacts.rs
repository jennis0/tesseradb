//! A grouping by the artifacts of one level of a layer.
//!
//! Which artifacts a table can list is what the artifacts frame serves this viewer: the layer's
//! reach, then each artifact's verdict over the viewer's authorised set, with its label, its
//! existence criterion and, for an attached artifact, its target's verdict, and content that reads
//! back. The filtered set never decides it. An artifact's count is its visible members in the set.
//! The rest are the set's items in a served artifact and in no listed one, and none the items in
//! no served artifact, so an item held only by a withheld artifact counts in none. Artifacts can
//! overlap, so a table's rows can add to more than the set's size.

use croaring::Bitmap;
use tessera_types::layer::HierarchyKind;
use tessera_types::{EntityId, TesseraId};

use super::set::Cx;
use super::table::{Groups, Key};
use super::{AggregateRefused, AggregateTimings, Pick};
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
    pick: Pick<TesseraId>,
}

/// A level's served artifacts under one page, and the forms their members are read from.
pub(super) struct Served {
    read: Option<ReadLevel>,
    /// Served ordinals, ascending.
    ordinals: Vec<u32>,
    /// Listed ordinals, in table order.
    listed: Vec<u32>,
}

impl Layer {
    /// The level `layer` and `level` name, where this viewer may read it in `view`.
    pub(super) fn of(
        engine: &Engine,
        session: &Session,
        generation: &Generation,
        view: &str,
        layer: &str,
        level: Option<u32>,
        pick: &Pick<TesseraId>,
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
        Ok(Layer {
            name: layer.to_string(),
            entity: registered.entity,
            level: level.unwrap_or(0),
            pick: pick.clone(),
        })
    }

    /// The table's groups under `cx`: the listed artifacts, `chosen` where the table has begun,
    /// and each group's counts in the set and the reference.
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
            .readable_layer(served_view.session, generation, &self.name, served_view.name)
            .ok()
            .filter(|layer| layer.entity == self.entity)
            .filter(|layer| (self.level as usize) < layer.runs.len());
        let mut ordinals: Vec<u32> = Vec::new();
        let read = match &registered {
            None => None,
            Some(layer) => {
                let read = engine.read_level(served_view, mask, layer, self.level, false);
                let reachable = engine.reachable_layers(served_view.session);
                let context = DependencyContext::new(served_view, mask, &reachable);
                let dependency_served = engine.dependency_gate(&context);
                let view = read.view(engine, served_view, mask, layer, &dependency_served);
                let runs = &layer.runs[self.level as usize];
                for ordinal in 0..read.rows.len() as u32 {
                    if ordinal % CHUNK == 0 {
                        cx.check_cancelled()?;
                    }
                    let Some(entity) = runs.entity_of(u64::from(ordinal)).map(EntityId::new)
                    else {
                        continue;
                    };
                    let crate::artifacts::ArtifactVerdict::Serve { rank, .. } =
                        view.verdict(entity, ordinal)
                    else {
                        continue;
                    };
                    if read
                        .content(engine, generation, layer, ordinal, entity, rank)
                        .is_some()
                    {
                        ordinals.push(ordinal);
                    }
                }
                Some(read)
            }
        };
        let tessera_id = |ordinal: u32| -> Option<u64> {
            let layer = registered.as_ref()?;
            let entity = layer.runs[self.level as usize].entity_of(u64::from(ordinal))?;
            engine
                .identity_key
                .forward(shard, EntityId::new(entity))
                .ok()
                .map(|id| id.raw())
        };
        let set_rows = cx.sets.set.rows(cx);
        let reference_rows = cx.sets.reference.as_ref().map(|r| r.rows(cx));
        let each = |rows: &Bitmap| -> Vec<u64> {
            let Some(read) = &read else {
                return Vec::new();
            };
            match read.filtered_counts(engine, mask, rows) {
                Some(histogram) => ordinals
                    .iter()
                    .map(|&o| u64::from(histogram.get(o as usize).copied().unwrap_or(0)))
                    .collect(),
                None => ordinals
                    .iter()
                    .map(|&o| read.matched_count(o, mask, rows))
                    .collect(),
            }
        };
        let counts = each(set_rows);
        let listed: Vec<u32> = match chosen {
            Some(chosen) => chosen.iter().map(|&g| g as u32).collect(),
            None => match &self.pick {
                Pick::Top(n) => {
                    let mut ranked: Vec<(u64, u64, u32)> = ordinals
                        .iter()
                        .zip(&counts)
                        .filter(|&(_, &count)| count > 0)
                        .filter_map(|(&o, &count)| Some((count, tessera_id(o)?, o)))
                        .collect();
                    ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
                    ranked.into_iter().take(*n as usize).map(|(_, _, o)| o).collect()
                }
                Pick::Named(ids) => {
                    let mut listed: Vec<u32> = Vec::with_capacity(ids.len());
                    for &id in ids {
                        let (id_shard, entity) = engine.identity_key.invert(id);
                        if id_shard != shard {
                            continue;
                        }
                        let Some((name, level, ordinal)) = engine.write.live().locate_artifact(entity)
                        else {
                            continue;
                        };
                        if name == self.name
                            && level == self.level
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
        let served = Served {
            read,
            ordinals,
            listed,
        };
        let set_sizes = served.sizes(cx, cx.sets.set.cells(cx), set_rows)?;
        let reference_sizes = match (&cx.sets.reference, reference_rows) {
            (Some(reference), Some(rows)) => Some(served.sizes(cx, reference.cells(cx), rows)?),
            _ => None,
        };
        let sizes = set_sizes
            .iter()
            .enumerate()
            .map(|(g, &s)| (s, reference_sizes.as_ref().map_or(0, |r| r[g])))
            .collect();
        let named = matches!(self.pick, Pick::Named(_));
        let keys = served
            .listed
            .iter()
            .map(|&o| Key::Id(tessera_id(o).unwrap_or_default()))
            .collect();
        timings.count_ns += counting.elapsed().as_nanos() as u64;
        Ok((
            Groups {
                chosen: served
                    .listed
                    .iter()
                    .map(|&o| u64::from(self.level) << 32 | u64::from(o))
                    .collect(),
                sizes,
                always: served.listed.iter().map(|&o| named && served.serves(o)).collect(),
                keys,
                titles: None,
                distinct: counts.iter().filter(|&&n| n > 0).count() as u64,
            },
            served,
        ))
    }
}

impl Served {
    /// Whether this viewer is served the artifact at `ordinal`.
    fn serves(&self, ordinal: u32) -> bool {
        self.ordinals.binary_search(&ordinal).is_ok()
    }

    /// The listed ordinals, each one no longer served standing for nothing.
    fn listed_served(&self) -> Vec<u32> {
        self.listed
            .iter()
            .map(|&o| if self.serves(o) { o } else { u32::MAX })
            .collect()
    }

    /// Whether the level has a row-addressed column, which one pass over the set reads for every
    /// artifact at once.
    fn row_major(&self) -> bool {
        self.read
            .as_ref()
            .is_some_and(|read| read.rows.column().is_some())
    }

    /// The labels table a pass over a row-major level reads.
    pub(super) fn label_table(&self) -> Option<LabelTable> {
        let read = self.read.as_ref()?;
        self.row_major().then(|| {
            LabelTable::new(
                read.rows.len(),
                self.ordinals.iter().copied(),
                &self.listed_served(),
            )
        })
    }

    /// Where a pass over a row-major level reads each row's groups, by `table`.
    pub(super) fn row_groups<'s>(&'s self, table: &'s LabelTable) -> RowGroups<'s> {
        let column = self
            .read
            .as_ref()
            .and_then(|read| read.rows.column())
            .expect("a row-major level has its column");
        RowGroups::Labels { column, table }
    }

    /// Each group's items among `rows`: the listed artifacts, the rest, and none.
    fn sizes(&self, cx: &Cx<'_>, cells: CellSet<'_>, rows: &Bitmap) -> Result<Vec<u64>> {
        cx.check_cancelled()?;
        if let Some(table) = self.label_table() {
            let groups = self.row_groups(&table);
            let counted = cx
                .engine
                .pool
                .install(|| pass(cells, cx.segments(), 0, &groups));
            let mut sizes = vec![0u64; self.listed.len() + 2];
            for entry in counted {
                sizes[entry.group as usize] += entry.count;
            }
            return Ok(sizes);
        }
        let parts = self.group_rows(cx, rows);
        Ok(parts.iter().map(Bitmap::cardinality).collect())
    }

    /// Each group's items among `rows` as a set of their own, on a level with per-artifact rows:
    /// each listed artifact's visible members, the rest, and none.
    pub(super) fn group_rows(&self, cx: &Cx<'_>, rows: &Bitmap) -> Vec<Bitmap> {
        let mask = &cx.open.mask;
        // `rows` is inside the mask, so an artifact's members among them are visible members.
        let visible = |ordinal: u32| -> Bitmap {
            match &self.read {
                Some(read) => match read.rows.get(ordinal) {
                    Some(members) => members.and(rows),
                    None => read.rows.visible_rows(ordinal, mask).and(rows),
                },
                None => Bitmap::new(),
            }
        };
        let listed: Vec<Bitmap> = self
            .listed
            .iter()
            .map(|&o| if self.serves(o) { visible(o) } else { Bitmap::new() })
            .collect();
        let served: Vec<Bitmap> = cx.engine.pool.install(|| {
            use rayon::prelude::*;
            self.ordinals.par_iter().map(|&o| visible(o)).collect()
        });
        let union = |parts: &[Bitmap]| {
            let refs: Vec<&Bitmap> = parts.iter().collect();
            Bitmap::fast_or(&refs)
        };
        let in_served = union(&served);
        let in_listed = union(&listed);
        let rest = in_served.andnot(&in_listed);
        let none = rows.andnot(&in_served);
        listed.into_iter().chain([rest, none]).collect()
    }
}
