//! Cluster colours: a palette slot for every cluster a viewer is served, chosen so that clusters
//! drawn beside each other differ, worked out once over the viewer's whole visible tree.
//!
//! A slot is a number below the palette size `N` the request names (2 to 32); the client maps it to
//! a colour. It is fixed for a viewer: the same at any zoom, box, budget or filter, because nothing
//! a request draws enters it.
//!
//! # What it is computed from
//!
//! Only what this viewer is served: the clusters whose verdict passes over the viewer's visible
//! set and that hold an item the viewer sees (one that holds none draws nothing and has no slot),
//! the viewer's tree over those clusters (a withheld cluster occupies no place in it, as in
//! [`crate::cut`]), each cluster's visible count, its `tessera_id`, and a centre over its visible
//! members. A cluster the viewer is not served is no input, so a viewer's slots are those of a
//! corpus without it.
//!
//! **Centres come from a sample.** The items sampled are the visible items whose `tessera_id` is
//! below `2^(64 - j)`, `j` the largest whole number with `N_visible / 2^j >= SAMPLE`, read from
//! identity band `j` ([`tessera_store::bands`]) where there is one and from the segment's columns
//! where `j` is below the first band. The cut depends on the visible count and [`SAMPLE`] alone. A
//! cluster's centre is the mean position of its sampled members. A cluster with none takes its
//! nearest sampled ancestor's centre, moved by an offset of at most [`JITTER`] grid units drawn
//! from its own `tessera_id`, so that siblings without a sample do not sit on one point. One with
//! no sampled ancestor has no centre.
//!
//! # Depths and neighbours
//!
//! A cluster is coloured once, at the first depth it is drawn:
//!
//! - **nested and dag:** each depth of the viewer's tree, where the clusters drawn are the cut at
//!   that depth with ancestors pruned ([`crate::cut::depths`]) and the clusters coloured are the
//!   ones at that rung;
//! - **tiered:** each level, coarsest first, every cluster of the level drawn and coloured;
//! - **stacked:** each level alone; **flat:** the one level.
//!
//! Two clusters are neighbours at a depth where the edge between their centres is in the Delaunay
//! triangulation of the centres drawn there. Clusters sharing a centre are first set on a ring about
//! it, a grid unit apart in order of `tessera_id`; a cluster with no centre has no neighbours.
//!
//! # The rule
//!
//! Every cluster has an order of the `N` slots, a permutation drawn from its `tessera_id` and `N`,
//! and a rank drawn from its `tessera_id`. A parent's **heir** is its child with the most visible
//! items, ties going to the lower `tessera_id`; a child that is heir to several parents inherits
//! from the one with the most visible items, ties likewise.
//!
//! A cluster's **claim** is the slot it asks for first:
//!
//! - a cluster coloured at an earlier depth claims its slot, and outranks every other claim;
//! - an heir claims its parent's slot, and outranks a cluster that is not an heir;
//! - any other cluster claims the first slot of its order that none of its parents holds.
//!
//! Among claims of one class the higher rank wins. A cluster takes the first slot of its list (its
//! claim, then its order without its parents' slots, then its parents' slots) that no neighbour
//! claims, except that it keeps its own claim against a neighbour of lower rank claiming the same.
//! Where every slot is claimed by a neighbour, it takes the slot its farthest neighbour claims,
//! which is a clash.
//!
//! So a slot depends on the cluster's own `tessera_id`, its parents' slots and whether it is an
//! heir, and on its neighbours' `tessera_id`s and what they inherit, but never on a neighbour's
//! slot at the same depth. A change to the corpus moves the slots of the clusters whose neighbours
//! or parents it moved, and of their descendants, and no others. Two neighbours that both give up
//! their claim can still take the same free slot; [`SlotStats`] counts every drawn edge whose ends
//! share a slot.

use std::ops::RangeInclusive;
use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;
use tessera_lifecycle::membership::Attachment;
use tessera_types::layer::{HierarchyKind, RegisteredLayer};
use tessera_types::{EntityId, MortonCode};

use crate::artifacts::ArtifactVerdict;
use crate::compose::{EffectiveMask, WholeMask};
use crate::error::{EngineError, Result};
use crate::layer_read::ReadLevel;
use crate::viewport::ServedView;
use crate::Engine;

/// The palette sizes a request may name.
pub const PALETTE_SIZES: RangeInclusive<u8> = 2..=32;

/// About how many visible items the centres are taken from: at least this many and fewer than
/// twice this, or every visible item where there are fewer than twice this.
pub const SAMPLE: u64 = 1 << 18;

/// The farthest a cluster with no sampled member is placed from its ancestor's centre, in the
/// 32-bit grid's units: one cell at zoom 16.
const JITTER: f64 = 65_536.0;

/// No slot: a position that holds no served cluster.
const NO_SLOT: u8 = u8::MAX;

/// How many ordinals the verdict walk takes between two readings of the cancellation token.
const CHUNK: u32 = 1024;

/// How long a request waits for another's build of the same slots.
const BUILD_WAIT_MS: u64 = 600_000;

/// The bound on slots held, which are a byte an artifact.
const SLOTS_BYTES: u64 = 256 << 20;

/// `size` as a palette size, or the refusal naming the sizes there are.
pub fn check_palette_size(size: u32) -> Result<u8> {
    u8::try_from(size)
        .ok()
        .filter(|size| PALETTE_SIZES.contains(size))
        .ok_or(EngineError::PaletteRefused(size))
}

/// One layer's slots for one viewer and one palette size: a byte per ordinal of each level.
pub(crate) struct LayerSlots {
    levels: Vec<Vec<u8>>,
    pub(crate) stats: SlotStats,
}

impl LayerSlots {
    /// The slot of the cluster at `ordinal` of `level`, where this viewer is served it.
    pub(crate) fn get(&self, level: u32, ordinal: u32) -> Option<u8> {
        let slot = *self.levels.get(level as usize)?.get(ordinal as usize)?;
        (slot != NO_SLOT).then_some(slot)
    }
}

impl tessera_cache::CacheWeight for LayerSlots {
    fn cache_weight_bytes(&self) -> u64 {
        self.levels.iter().map(|level| level.len() as u64).sum::<u64>() + 64
    }
}

/// What one build of a layer's slots did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SlotStats {
    /// Clusters given a slot.
    pub clusters: u64,
    /// Neighbouring pairs, over every depth, at least one of which was coloured there.
    pub edges: u64,
    /// Those pairs whose two clusters hold one slot.
    pub clashes: u64,
    /// Visible items the centres were taken from.
    pub sampled_items: u64,
    /// Clusters with no sampled member, placed beside an ancestor's centre.
    pub unsampled: u64,
    /// Clusters with no centre, which have no neighbours.
    pub without_centre: u64,
    /// The build's wall time, in microseconds.
    pub build_us: u64,
}

/// What a viewer's slots are a function of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SlotsKey {
    token_id: u64,
    view: String,
    layer: String,
    entity: u64,
    /// Each level's record and lineage versions.
    levels: Vec<(u64, u64)>,
    segments_version: u64,
    projection_segments_version: u64,
    overlay_version: u64,
    fragment_identity: [u8; 32],
    fragment_watermark: u64,
    palette: u8,
}

/// The slots held, one entry per viewer, layer and palette size, built once for concurrent asks.
pub(crate) struct SlotsCache(tessera_cache::SingleFlightCache<SlotsKey, LayerSlots>);

impl SlotsCache {
    pub(crate) fn new() -> Self {
        let cache = tessera_cache::SingleFlightCache::new(SLOTS_BYTES);
        cache.set_wait_budget_ms(BUILD_WAIT_MS);
        SlotsCache(cache)
    }
}

impl Engine {
    /// This viewer's slots over `layer` for a palette of `palette` colours — see the module doc.
    /// `mask` is the viewer's composed mask; a filter it carries changes nothing here.
    pub(crate) fn cluster_slots(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        layer: &RegisteredLayer,
        palette: u8,
        dependency_served: &dyn Fn(&Attachment) -> bool,
    ) -> Result<Arc<LayerSlots>> {
        check_palette_size(u32::from(palette))?;
        let name = layer.declaration.name.as_str();
        let levels: Vec<(u64, u64)> = self.write.live().with_artifacts(|store| {
            (0..layer.runs.len() as u32)
                .map(|level| {
                    (
                        store.level_version(name, level),
                        store.lineage_version(name, level),
                    )
                })
                .collect()
        });
        let id = served.mask_identity;
        let key = SlotsKey {
            token_id: id.token_id,
            view: served.name.to_string(),
            layer: name.to_string(),
            entity: layer.entity.raw(),
            levels: levels.clone(),
            segments_version: id.segments_version,
            projection_segments_version: id.projection_segments_version,
            overlay_version: id.overlay_version,
            fragment_identity: id.fragment_identity,
            fragment_watermark: id.fragment_watermark,
            palette,
        };
        let cancel = served.cancel.clone().unwrap_or_default();
        self.slots
            .0
            .get_or_try_build_waiting(key, &cancel, || {
                self.build_slots(served, mask, layer, palette, &levels, dependency_served)
            })
            .map_err(crate::figures::waited)
    }

    /// What this viewer's slots over `layer` in `view` were built from and took, building them
    /// where they are not held. For `tessera-bench`'s `slot_cost`; not part of the engine's API.
    #[doc(hidden)]
    pub fn cluster_slot_stats(
        &self,
        session: &crate::Session,
        view: &str,
        layer: &str,
        palette: u8,
    ) -> Result<SlotStats> {
        let generation = self.generation.load_full();
        let open = self.open_view(
            session,
            &generation,
            view,
            &None,
            &mut crate::timing::Probe::new(),
        )?;
        let registered = self
            .readable_layer(session, &generation, layer, view)
            .map_err(|_| {
                EngineError::RecordsRefused(crate::records::RecordsRefused::UnknownLayer(
                    layer.to_string(),
                ))
            })?;
        let reachable = self.reachable_layers(session);
        let context = crate::viewport::DependencyContext::new(&open.served, &open.mask, &reachable);
        let dependency_served = self.dependency_gate(&context);
        let slots = self.cluster_slots(
            &open.served,
            &open.mask,
            &registered,
            palette,
            &dependency_served,
        )?;
        context.finish()?;
        Ok(slots.stats)
    }

    fn build_slots(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        layer: &RegisteredLayer,
        palette: u8,
        versions: &[(u64, u64)],
        dependency_served: &dyn Fn(&Attachment) -> bool,
    ) -> Result<LayerSlots> {
        let started = std::time::Instant::now();
        let generation = served.generation;
        let shard = generation.bundle.manifest.identity.shard_id;
        let kind = layer.declaration.hierarchy.kind;

        // The clusters this viewer is served, as the artifacts frame serves them: the verdict over
        // the visible set, and content that reads back. Each level is read as the map's frames read
        // it, so its counts are the entry the map fills.
        let mut clusters: Vec<Cluster> = Vec::new();
        let mut located: Vec<(u32, u32)> = Vec::new();
        let mut index: Vec<Vec<u32>> = Vec::new();
        let mut reads: Vec<ReadLevel> = Vec::new();
        for (level, runs) in layer.runs.iter().enumerate() {
            let level = level as u32;
            let read = self.read_level(served, mask, layer, level, true)?;
            let view = read.view(self, served, mask, layer, dependency_served);
            let mut at = vec![u32::MAX; read.rows.len()];
            for ordinal in 0..read.rows.len() as u32 {
                if ordinal % CHUNK == 0 && served.cancel.as_ref().is_some_and(|c| c.is_cancelled())
                {
                    return Err(EngineError::Cancelled);
                }
                let Some(entity) = runs.entity_of(u64::from(ordinal)).map(EntityId::new) else {
                    continue;
                };
                let ArtifactVerdict::Serve { masked_count, rank } = view.verdict(entity, ordinal)
                else {
                    continue;
                };
                if masked_count == 0 {
                    continue;
                }
                if read
                    .content(self, generation, layer, ordinal, entity, rank)
                    .is_none()
                {
                    continue;
                }
                let Ok(id) = self.identity_key.forward(shard, entity) else {
                    continue;
                };
                at[ordinal as usize] = clusters.len() as u32;
                clusters.push(Cluster {
                    seed: id.raw(),
                    count: masked_count,
                    parents: Vec::new(),
                    centre: None,
                });
                located.push((level, ordinal));
            }
            index.push(at);
            reads.push(read);
        }

        // The viewer's tree: each cluster's nearest served ancestors on every path.
        let treed = matches!(kind, HierarchyKind::Nested | HierarchyKind::Dag);
        if treed || kind == HierarchyKind::Tiered {
            for (c, &(level, ordinal)) in located.iter().enumerate() {
                clusters[c].parents = nearest_served(&reads, &index, level, ordinal, treed);
            }
        }

        let passes: Vec<Pass> = if treed {
            let rows = &reads[0].rows;
            let lineage = self.level_lineage(layer, 0, versions[0].1, rows);
            let passing: Vec<u32> = located.iter().map(|&(_, ordinal)| ordinal).collect();
            let of = |ordinals: Vec<u32>| -> Vec<u32> {
                ordinals.into_iter().map(|o| index[0][o as usize]).collect()
            };
            crate::cut::depths(&lineage, &passing)
                .into_iter()
                .map(|depth| Pass {
                    drawn: of(depth.served),
                    entering: of(depth.entering),
                })
                .collect()
        } else {
            (0..reads.len() as u32)
                .map(|level| {
                    let all: Vec<u32> = (0..clusters.len() as u32)
                        .filter(|&c| located[c as usize].0 == level)
                        .collect();
                    Pass {
                        drawn: all.clone(),
                        entering: all,
                    }
                })
                .collect()
        };

        let sample = sample(served, mask.visible_all());
        let mut sums: Vec<([f64; 2], u64)> = vec![([0.0; 2], 0); clusters.len()];
        let rows_of_sample = Bitmap::of(&sample.iter().map(|&(row, _)| row).collect::<Vec<_>>());
        for (level, read) in reads.iter().enumerate() {
            let at = &index[level];
            let mut add = |c: u32, position: [f64; 2]| {
                let (sum, n) = &mut sums[c as usize];
                sum[0] += position[0];
                sum[1] += position[1];
                *n += 1;
            };
            match read.rows.column() {
                Some(column) => {
                    for &(row, position) in &sample {
                        column.for_each_label(row, |ordinal| {
                            if let Some(&c) = at.get(ordinal as usize).filter(|&&c| c != u32::MAX)
                            {
                                add(c, position);
                            }
                        });
                    }
                }
                None => {
                    for (ordinal, &c) in at.iter().enumerate() {
                        if c == u32::MAX || read.rows.attachment(ordinal as u32).is_some() {
                            continue;
                        }
                        let Some(members) = read.rows.get(ordinal as u32) else {
                            continue;
                        };
                        for row in members.and(&rows_of_sample).iter() {
                            if let Ok(i) = sample.binary_search_by_key(&row, |&(r, _)| r) {
                                add(c, sample[i].1);
                            }
                        }
                    }
                }
            }
        }
        let mut stats = SlotStats {
            sampled_items: sample.len() as u64,
            ..SlotStats::default()
        };
        let sampled: Vec<Option<[f64; 2]>> = sums
            .iter()
            .map(|&(sum, n)| (n > 0).then(|| [sum[0] / n as f64, sum[1] / n as f64]))
            .collect();
        for c in 0..clusters.len() {
            clusters[c].centre = match sampled[c] {
                Some(centre) => Some(centre),
                None => {
                    let placed = beside_ancestor(&clusters, &sampled, c as u32);
                    match placed {
                        Some(_) => stats.unsampled += 1,
                        None => stats.without_centre += 1,
                    }
                    placed
                }
            };
        }

        let (slots, assigned) = assign(&clusters, &passes, palette);
        stats.clusters = assigned.clusters;
        stats.edges = assigned.edges;
        stats.clashes = assigned.clashes;
        let mut levels: Vec<Vec<u8>> = index.iter().map(|at| vec![NO_SLOT; at.len()]).collect();
        for (c, &(level, ordinal)) in located.iter().enumerate() {
            levels[level as usize][ordinal as usize] = slots[c];
        }
        stats.build_us = started.elapsed().as_micros() as u64;
        tracing::debug!(
            layer = %layer.declaration.name,
            palette,
            clusters = stats.clusters,
            edges = stats.edges,
            clashes = stats.clashes,
            sampled_items = stats.sampled_items,
            build_us = stats.build_us,
            "cluster slots built"
        );
        Ok(LayerSlots { levels, stats })
    }
}

/// The served clusters nearest above `(level, ordinal)` on every path, climbing through the ones
/// this viewer is not served, as cluster indices ascending. `within` keeps a treed layer's
/// in-level edges; otherwise a tiered layer's edges to coarser levels are followed.
fn nearest_served(
    reads: &[ReadLevel],
    index: &[Vec<u32>],
    level: u32,
    ordinal: u32,
    within: bool,
) -> Vec<u32> {
    let up = |level: u32, ordinal: u32| -> Vec<(u32, u32)> {
        reads[level as usize]
            .rows
            .parents(ordinal)
            .iter()
            .filter(|p| match within {
                true => p.level == level,
                false => p.level < level,
            })
            .filter(|p| (p.level as usize) < reads.len())
            .map(|p| (p.level, p.ordinal))
            .collect()
    };
    let mut found: Vec<u32> = Vec::new();
    let mut seen: FxHashSet<(u32, u32)> = FxHashSet::default();
    let mut climb = up(level, ordinal);
    while let Some(at) = climb.pop() {
        if !seen.insert(at) {
            continue;
        }
        match index[at.0 as usize].get(at.1 as usize) {
            Some(&c) if c != u32::MAX => found.push(c),
            _ => climb.extend(up(at.0, at.1)),
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// The centre of the nearest ancestor of `c` with a sampled centre, searched breadth first with
/// parents in ascending order, moved by an offset drawn from `c`'s seed.
fn beside_ancestor(clusters: &[Cluster], sampled: &[Option<[f64; 2]>], c: u32) -> Option<[f64; 2]> {
    let mut queue: std::collections::VecDeque<u32> =
        clusters[c as usize].parents.iter().copied().collect();
    let mut seen: FxHashSet<u32> = FxHashSet::default();
    while let Some(at) = queue.pop_front() {
        if !seen.insert(at) {
            continue;
        }
        if let Some(centre) = sampled[at as usize] {
            let mut state = clusters[c as usize].seed ^ 0x5851_F42D_4C95_7F2D;
            let radius = JITTER * unit(splitmix(&mut state)).sqrt();
            let angle = std::f64::consts::TAU * unit(splitmix(&mut state));
            return Some([
                centre[0] + radius * angle.cos(),
                centre[1] + radius * angle.sin(),
            ]);
        }
        queue.extend(clusters[at as usize].parents.iter().copied());
    }
    None
}

/// The visible items the centres are taken from, as `(view row, position)` ascending by row: see
/// the module doc.
fn sample(served: &ServedView<'_>, visible: &Bitmap) -> Vec<(u32, [f64; 2])> {
    let n = visible.cardinality();
    let band = match n / SAMPLE {
        0 => 0,
        ratio => 63 - ratio.leading_zeros(),
    };
    let position = |code: u32, residual: u32| {
        let (x, y) = tessera_spatial::unsplit32(MortonCode::new(code), residual);
        [f64::from(x), f64::from(y)]
    };
    let mut out: Vec<(u32, [f64; 2])> = Vec::new();
    for &(segment, row_base) in &served.segments {
        if band >= tessera_store::bands::FIRST_BAND {
            let bands = &segment.bands;
            let (rows, codes, residuals) = (bands.rows(), bands.codes(), bands.residuals());
            for e in bands.band(band) {
                let row = row_base + rows[e];
                if visible.contains(row) {
                    out.push((row, position(codes[e], residuals[e])));
                }
            }
            continue;
        }
        let below = match band {
            0 => u64::MAX,
            j => (1u64 << (64 - j)) - 1,
        };
        let (ids, morton, residual) = (
            segment.columns.tessera_id(),
            segment.morton.u32(),
            segment.columns.residual(),
        );
        let mut rows = visible.iter();
        rows.reset_at_or_after(row_base);
        for row in rows.take_while(|&row| row < row_base + segment.row_count) {
            let local = (row - row_base) as usize;
            if ids[local] <= below {
                out.push((row, position(morton[local], residual[local])));
            }
        }
    }
    out
}

/// A cluster as the rule reads it.
#[derive(Debug, Clone)]
pub(crate) struct Cluster {
    /// Its `tessera_id`.
    pub(crate) seed: u64,
    /// Its visible items.
    pub(crate) count: u64,
    /// Its parents in the viewer's tree, as indices of clusters.
    pub(crate) parents: Vec<u32>,
    pub(crate) centre: Option<[f64; 2]>,
}

/// One depth: the clusters drawn there, and those of them coloured there.
#[derive(Debug, Clone)]
pub(crate) struct Pass {
    pub(crate) drawn: Vec<u32>,
    pub(crate) entering: Vec<u32>,
}

/// What [`assign`] counted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Assigned {
    pub(crate) clusters: u64,
    pub(crate) edges: u64,
    pub(crate) clashes: u64,
}

/// What one cluster asks for at one depth, and how strongly.
#[derive(Debug, Clone, Copy)]
struct Claim {
    slot: u8,
    /// 2 for a cluster already coloured, 1 for an heir, 0 for any other.
    class: u8,
    rank: u64,
}

impl Claim {
    fn outranks(&self, other: &Claim) -> bool {
        (self.class, self.rank) > (other.class, other.rank)
    }
}

/// Every cluster's slot under the rule in the module doc, by index, and what was counted.
/// [`NO_SLOT`] for a cluster no pass colours.
pub(crate) fn assign(clusters: &[Cluster], passes: &[Pass], palette: u8) -> (Vec<u8>, Assigned) {
    let heirs = heirs(clusters);
    let mut slots = vec![NO_SLOT; clusters.len()];
    let mut assigned = Assigned::default();
    // Each drawn cluster's position among the points of the pass, `u32::MAX` where it has none.
    let mut local = vec![u32::MAX; clusters.len()];
    for pass in passes {
        let placed: Vec<u32> = pass
            .drawn
            .iter()
            .copied()
            .filter(|&c| clusters[c as usize].centre.is_some())
            .collect();
        for (i, &c) in placed.iter().enumerate() {
            local[c as usize] = i as u32;
        }
        let mut points: Vec<[f64; 2]> = placed
            .iter()
            .map(|&c| clusters[c as usize].centre.expect("placed"))
            .collect();
        let seeds: Vec<u64> = placed.iter().map(|&c| clusters[c as usize].seed).collect();
        spread_coincident(&mut points, &seeds);
        let near = neighbours(&points);
        // Each cluster still to colour's list of slots, its claim first.
        let lists: Vec<(u32, Vec<u8>)> = pass
            .entering
            .iter()
            .copied()
            .filter(|&c| slots[c as usize] == NO_SLOT)
            .map(|c| (c, preferences(clusters, &slots, heirs[c as usize], c, palette)))
            .collect();
        let mut claims: Vec<Claim> = placed
            .iter()
            .map(|&c| Claim {
                slot: slots[c as usize],
                class: 2,
                rank: splitmix(&mut clusters[c as usize].seed.clone()),
            })
            .collect();
        let mut entering = vec![false; placed.len()];
        for (c, list) in &lists {
            let class = u8::from(inherited(&slots, heirs[*c as usize]).is_some());
            let i = local[*c as usize];
            if i != u32::MAX {
                claims[i as usize].slot = list[0];
                claims[i as usize].class = class;
                entering[i as usize] = true;
            }
        }
        let chosen: Vec<(u32, u8)> = lists
            .iter()
            .map(|(c, list)| {
                let i = local[*c as usize];
                let around = match i {
                    u32::MAX => &[][..],
                    i => near.of(i),
                };
                let Some(mine) = (i != u32::MAX).then(|| claims[i as usize]) else {
                    return (*c, list[0]);
                };
                let free = list.iter().copied().find(|&slot| {
                    !around.iter().any(|&n| {
                        let other = &claims[n as usize];
                        other.slot == slot && (slot != mine.slot || other.outranks(&mine))
                    })
                });
                let slot = free.unwrap_or_else(|| {
                    let at = points[i as usize];
                    let far = |n: u32| {
                        let p = points[n as usize];
                        (p[0] - at[0]).powi(2) + (p[1] - at[1]).powi(2)
                    };
                    around
                        .iter()
                        .copied()
                        .max_by(|&a, &b| far(a).total_cmp(&far(b)))
                        .map_or(mine.slot, |n| claims[n as usize].slot)
                });
                (*c, slot)
            })
            .collect();
        for (c, slot) in chosen {
            slots[c as usize] = slot;
            assigned.clusters += 1;
        }
        for i in 0..placed.len() as u32 {
            for &j in near.of(i) {
                if i < j && (entering[i as usize] || entering[j as usize]) {
                    assigned.edges += 1;
                    let (a, b) = (placed[i as usize], placed[j as usize]);
                    assigned.clashes += u64::from(slots[a as usize] == slots[b as usize]);
                }
            }
        }
        for &c in &placed {
            local[c as usize] = u32::MAX;
        }
    }
    (slots, assigned)
}

/// Each cluster's parent it is heir to, where it is heir to one: of the parents whose child with
/// the most visible items it is, the one with the most visible items, ties going to the lower
/// `tessera_id` both times.
fn heirs(clusters: &[Cluster]) -> Vec<Option<u32>> {
    let weight = |c: u32| {
        let cluster = &clusters[c as usize];
        (cluster.count, std::cmp::Reverse(cluster.seed))
    };
    let mut heir_of: Vec<Option<u32>> = vec![None; clusters.len()];
    for (c, cluster) in clusters.iter().enumerate() {
        let c = c as u32;
        for &p in &cluster.parents {
            let held = &mut heir_of[p as usize];
            if held.is_none_or(|h| weight(c) > weight(h)) {
                *held = Some(c);
            }
        }
    }
    let mut heir: Vec<Option<u32>> = vec![None; clusters.len()];
    for (p, child) in heir_of.iter().enumerate() {
        let (p, Some(child)) = (p as u32, *child) else {
            continue;
        };
        let held = &mut heir[child as usize];
        if held.is_none_or(|q| weight(p) > weight(q)) {
            *held = Some(p);
        }
    }
    heir
}

/// The slot an heir inherits: its parent's, once that is coloured.
fn inherited(slots: &[u8], parent: Option<u32>) -> Option<u8> {
    parent
        .map(|p| slots[p as usize])
        .filter(|&slot| slot != NO_SLOT)
}

/// A cluster's slots in the order it takes them: what it inherits, then its own order without its
/// parents' slots, then its parents' slots.
fn preferences(
    clusters: &[Cluster],
    slots: &[u8],
    heir_to: Option<u32>,
    c: u32,
    palette: u8,
) -> Vec<u8> {
    let cluster = &clusters[c as usize];
    let held: Vec<u8> = cluster
        .parents
        .iter()
        .map(|&p| slots[p as usize])
        .filter(|&slot| slot != NO_SLOT)
        .collect();
    let order = order(cluster.seed, palette);
    let first = inherited(slots, heir_to);
    let mut list: Vec<u8> = Vec::with_capacity(palette as usize);
    list.extend(first);
    list.extend(
        order
            .iter()
            .copied()
            .filter(|slot| !held.contains(slot) && Some(*slot) != first),
    );
    list.extend(
        order
            .iter()
            .copied()
            .filter(|slot| held.contains(slot) && Some(*slot) != first),
    );
    list
}

/// A permutation of the `palette` slots drawn from `seed`.
fn order(seed: u64, palette: u8) -> Vec<u8> {
    let mut state = seed ^ u64::from(palette).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut permutation: Vec<u8> = (0..palette).collect();
    for i in (1..permutation.len()).rev() {
        let j = (splitmix(&mut state) % (i as u64 + 1)) as usize;
        (permutation[i], permutation[j]) = (permutation[j], permutation[i]);
    }
    permutation
}

/// The next value of a SplitMix64 sequence.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `bits` as a number in `[0, 1)`.
fn unit(bits: u64) -> f64 {
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// Move the points that share a position onto a ring about it, one grid unit across per point, in
/// order of their seeds, so that clusters on one spot are each beside a few of the others rather
/// than all of them.
fn spread_coincident(points: &mut [[f64; 2]], seeds: &[u64]) {
    let mut order: Vec<usize> = (0..points.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        let (p, q) = (points[a], points[b]);
        p[0].total_cmp(&q[0])
            .then(p[1].total_cmp(&q[1]))
            .then(seeds[a].cmp(&seeds[b]))
    });
    let mut start = 0;
    while start < order.len() {
        let at = points[order[start]];
        let end = start
            + order[start..]
                .iter()
                .take_while(|&&i| points[i] == at)
                .count();
        let k = end - start;
        if k > 1 {
            let radius = k as f64 / std::f64::consts::TAU;
            for (j, &i) in order[start..end].iter().enumerate() {
                let angle = std::f64::consts::TAU * j as f64 / k as f64;
                points[i] = [at[0] + radius * angle.cos(), at[1] + radius * angle.sin()];
            }
        }
        start = end;
    }
}

/// Each point's neighbours, as positions among the points, ascending.
struct Neighbours {
    at: Vec<u32>,
    list: Vec<u32>,
}

impl Neighbours {
    fn of(&self, i: u32) -> &[u32] {
        &self.list[self.at[i as usize] as usize..self.at[i as usize + 1] as usize]
    }
}

/// The neighbours of each of `points` in their Delaunay triangulation; on one line, each is beside
/// the next along it. Two points at one position are not told apart: [`spread_coincident`] is
/// what keeps them apart.
fn neighbours(points: &[[f64; 2]]) -> Neighbours {
    let vertices: Vec<delaunator::Point> = points
        .iter()
        .map(|p| delaunator::Point { x: p[0], y: p[1] })
        .collect();
    let mut edges: Vec<(u32, u32)> = Vec::new();
    let mut link = |a: usize, b: usize| {
        if a != b {
            edges.push((a as u32, b as u32));
            edges.push((b as u32, a as u32));
        }
    };
    match vertices.len() {
        0 | 1 => {}
        2 => link(0, 1),
        _ => {
            let triangulation = delaunator::triangulate(&vertices);
            if triangulation.triangles.is_empty() {
                for pair in triangulation.hull.windows(2) {
                    link(pair[0], pair[1]);
                }
            } else {
                for t in triangulation.triangles.chunks_exact(3) {
                    link(t[0], t[1]);
                    link(t[1], t[2]);
                    link(t[2], t[0]);
                }
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    let mut at = vec![0u32; points.len() + 1];
    for &(a, _) in &edges {
        at[a as usize + 1] += 1;
    }
    for i in 1..at.len() {
        at[i] += at[i - 1];
    }
    Neighbours {
        at,
        list: edges.into_iter().map(|(_, b)| b).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A random tree of `depth` levels below one root, each node with up to `fan` children placed
    /// near it, and the passes a nested layer colours it in: the nodes at each depth, drawn with
    /// the leaves of every shallower depth.
    fn tree(seed: u64, depth: u32, fan: u64) -> (Vec<Cluster>, Vec<Pass>) {
        let mut state = seed;
        let mut clusters = vec![Cluster {
            seed: splitmix(&mut state),
            count: 1 << 30,
            parents: Vec::new(),
            centre: Some([0.5, 0.5]),
        }];
        let mut levels: Vec<Vec<u32>> = vec![vec![0]];
        let mut spread = 0.5;
        for _ in 0..depth {
            let mut next = Vec::new();
            for &p in levels.last().expect("a level") {
                let children = 1 + splitmix(&mut state) % fan;
                let parent = clusters[p as usize].clone();
                for _ in 0..children {
                    let at = parent.centre.expect("placed");
                    let jitter = |s: &mut u64| (unit(splitmix(s)) - 0.5) * spread;
                    let centre = [at[0] + jitter(&mut state), at[1] + jitter(&mut state)];
                    next.push(clusters.len() as u32);
                    clusters.push(Cluster {
                        seed: splitmix(&mut state),
                        count: 1 + splitmix(&mut state) % parent.count.max(2),
                        parents: vec![p],
                        centre: Some(centre),
                    });
                }
            }
            spread /= fan as f64 / 2.0 + 1.0;
            levels.push(next);
        }
        let has_child: FxHashSet<u32> = clusters.iter().flat_map(|c| c.parents.clone()).collect();
        let mut passes: Vec<Pass> = Vec::new();
        let mut leaves: Vec<u32> = Vec::new();
        for level in &levels {
            let mut drawn = leaves.clone();
            drawn.extend_from_slice(level);
            passes.push(Pass {
                drawn,
                entering: level.clone(),
            });
            leaves.extend(level.iter().copied().filter(|c| !has_child.contains(c)));
        }
        (clusters, passes)
    }

    /// Every drawn pair of neighbours sharing a slot, over every pass.
    fn shared(clusters: &[Cluster], passes: &[Pass], slots: &[u8]) -> (u64, u64) {
        let (mut edges, mut clashes) = (0, 0);
        for pass in passes {
            let points: Vec<[f64; 2]> = pass
                .drawn
                .iter()
                .map(|&c| clusters[c as usize].centre.expect("placed"))
                .collect();
            let entering: FxHashSet<u32> = pass.entering.iter().copied().collect();
            let near = neighbours(&points);
            for i in 0..points.len() as u32 {
                for &j in near.of(i) {
                    let (a, b) = (pass.drawn[i as usize], pass.drawn[j as usize]);
                    if a < b && (entering.contains(&a) || entering.contains(&b)) {
                        edges += 1;
                        clashes += u64::from(slots[a as usize] == slots[b as usize]);
                    }
                }
            }
        }
        (edges, clashes)
    }

    #[test]
    fn neighbours_share_a_slot_only_where_counted_and_rarely() {
        for palette in [8u8, 10, 20, 22] {
            let (mut edges, mut clashes) = (0u64, 0u64);
            for seed in 0..40 {
                let (clusters, passes) = tree(seed, 4, 6);
                let (slots, assigned) = assign(&clusters, &passes, palette);
                assert!(slots.iter().all(|&s| s < palette), "every cluster is coloured");
                assert_eq!(
                    (assigned.edges, assigned.clashes),
                    shared(&clusters, &passes, &slots)
                );
                edges += assigned.edges;
                clashes += assigned.clashes;
            }
            let rate = clashes as f64 / edges as f64;
            assert!(rate < 0.05, "{clashes} of {edges} edges clash at N = {palette}");
        }
    }

    #[test]
    fn an_heir_keeps_its_parents_slot_where_no_neighbour_holds_or_inherits_it() {
        for seed in 0..40 {
            let (clusters, passes) = tree(seed, 4, 5);
            let (slots, _) = assign(&clusters, &passes, 10);
            let heirs = heirs(&clusters);
            let mut kept = 0;
            for pass in &passes {
                let points: Vec<[f64; 2]> = pass
                    .drawn
                    .iter()
                    .map(|&c| clusters[c as usize].centre.expect("placed"))
                    .collect();
                let near = neighbours(&points);
                for (i, &c) in pass.drawn.iter().enumerate() {
                    let Some(parent) = heirs[c as usize].filter(|_| pass.entering.contains(&c))
                    else {
                        continue;
                    };
                    let slot = slots[parent as usize];
                    let contested = near.of(i as u32).iter().any(|&j| {
                        let n = pass.drawn[j as usize];
                        let inherits = heirs[n as usize].is_some_and(|q| slots[q as usize] == slot)
                            && pass.entering.contains(&n);
                        inherits || (!pass.entering.contains(&n) && slots[n as usize] == slot)
                    });
                    if !contested {
                        assert_eq!(slots[c as usize], slot, "heir {c} of {parent}");
                        kept += 1;
                    }
                }
            }
            assert!(kept > 0);
        }
    }

    #[test]
    fn a_change_moves_only_the_slots_near_it() {
        let (clusters, passes) = tree(7, 4, 6);
        let (before, _) = assign(&clusters, &passes, 10);
        // Withdraw one leaf at the deepest depth: only its neighbours there can move.
        let last = passes.last().expect("a pass");
        let gone = last.entering[last.entering.len() / 2];
        let mut fewer = passes.clone();
        for pass in &mut fewer {
            pass.drawn.retain(|&c| c != gone);
            pass.entering.retain(|&c| c != gone);
        }
        let (after, _) = assign(&clusters, &fewer, 10);
        let points: Vec<[f64; 2]> = last
            .drawn
            .iter()
            .map(|&c| clusters[c as usize].centre.expect("placed"))
            .collect();
        let near = neighbours(&points);
        let at = last.drawn.iter().position(|&c| c == gone).expect("drawn");
        let close: FxHashSet<u32> = near
            .of(at as u32)
            .iter()
            .map(|&j| last.drawn[j as usize])
            .collect();
        for c in 0..clusters.len() as u32 {
            if c != gone && before[c as usize] != after[c as usize] {
                assert!(close.contains(&c), "cluster {c} moved, far from the change");
            }
        }
    }

    #[test]
    fn points_on_one_spot_are_set_apart_and_points_on_a_line_are_chained() {
        let mut points = [[5.0, 5.0], [5.0, 5.0], [5.0, 5.0], [9.0, 2.0]];
        spread_coincident(&mut points, &[3, 1, 2, 0]);
        for i in 0..3 {
            for j in 0..i {
                assert!(points[i] != points[j], "{points:?}");
            }
            let away = (points[i][0] - 5.0).hypot(points[i][1] - 5.0);
            assert!(away > 0.0 && away < 1.0, "{points:?}");
        }
        assert_eq!(points[3], [9.0, 2.0]);
        let near = neighbours(&points);
        for i in 0..3u32 {
            assert!(near.of(i).iter().any(|&j| j < 3), "{i} is beside one of its spot");
        }
        let line = neighbours(&[[0.0, 0.0], [2.0, 0.0], [1.0, 0.0]]);
        assert_eq!((line.of(0), line.of(1), line.of(2)), (&[2][..], &[2][..], &[0, 1][..]));
    }
}
