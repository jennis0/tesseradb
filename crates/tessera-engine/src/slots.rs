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
//! Slots are worked out a level at a time, when a request names a palette size for that level; a
//! tiered level's parents' levels first.
//!
//! **Centres come from the level's figures where the layer declares a centroid or a box** and the
//! level is served from its column alone: each cluster's centre is the centroid of its visible
//! members those figures carry, the same entry the map fills when it draws the level. Every route
//! that names a palette size reads the level as the map does, filling that entry where no route
//! has; a request without a palette size reads nothing for slots.
//!
//! **Elsewhere centres come from a sample.** The items sampled are the visible items whose
//! `tessera_id` is below the cut `⌊S · 2⁶⁴ / N⌋`, `S` being [`SAMPLE`] and `N` the visible count, or
//! every visible item where `N <= S`. They are read from the narrowest identity band holding the cut
//! ([`tessera_store::bands`]), or from the segment's columns where no band does, with a column
//! level's labels read from its band-order copy where it has one. The cut moves in proportion to
//! `N`, with no step, so a change in the visible count moves the centres only of the clusters
//! holding an item between the old cut and the new. A cluster's centre is the mean position of its
//! sampled members. A cluster with none takes its nearest centred ancestor's centre, moved by at
//! most [`JITTER`] grid units drawn from its own `tessera_id`, and one with no such ancestor has no
//! centre and no neighbours. So for clusters far smaller than the sample resolves, which neighbours
//! they are told apart from is approximate.

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
//! items, ties going to the lower `tessera_id`, among all its children in the viewer's tree (on a
//! tiered layer, at whatever level they sit); a child that is heir to several parents inherits
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
//! Where every slot is claimed by a neighbour, it takes the slot its farthest neighbour claims.
//!
//! # What a change reaches
//!
//! A slot depends on the cluster's own `tessera_id`, its parents' slots and which parent it is heir
//! to, and on each neighbour's `tessera_id`, which parent the neighbour is heir to, the neighbour's
//! parents' slots, and the slot of a neighbour coloured at an earlier depth. It never depends on the
//! slot a neighbour takes at the same depth. So a change reaches:
//!
//! - the clusters whose neighbours it changes at a depth: those beside a cluster that appeared,
//!   went or moved;
//! - where it changes which child is a parent's heir, the old and the new heir, and their
//!   neighbours;
//! - at each depth below, the children of every cluster whose slot changed, every cluster drawn
//!   beside a cluster whose slot changed, and their neighbours.
//!
//! The reach therefore widens by about a ring of neighbours for each depth below the change, and
//! an heir that changes near the root recolours its subtree. Two neighbours that both give up their
//! claim can still take one slot; [`SlotStats`] counts every pair of clusters drawn beside each
//! other at any depth that hold one slot.

use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, PoisonError};

use croaring::Bitmap;
use rustc_hash::{FxHashMap, FxHashSet};
use tessera_lifecycle::membership::Attachment;
use tessera_types::layer::{HierarchyKind, RegisteredLayer};
use tessera_store::bands::BandLabels;
use tessera_types::{EntityId, MortonCode};

use crate::artifacts::ArtifactVerdict;
use crate::compose::{EffectiveMask, WholeMask};
use crate::error::{EngineError, Result};
use crate::layer_read::ReadLevel;
use crate::viewport::ServedView;
use crate::Engine;

/// The palette sizes a request may name.
pub const PALETTE_SIZES: RangeInclusive<u8> = 2..=32;

/// About how many visible items the centres are taken from, where a level's centres come from a
/// sample, or every visible item where there are fewer.
pub const SAMPLE: u64 = 1 << 19;

/// The farthest a cluster centred on an ancestor is placed from the ancestor's centre, in the
/// 32-bit grid's units: one cell at zoom 16.
const JITTER: f64 = 65_536.0;

/// No slot: a position that holds no served cluster.
const NO_SLOT: u8 = u8::MAX;

/// How many steps of a walk are taken between two readings of the cancellation token.
const CHUNK: u32 = 1024;

/// How long a request waits for another's build of the same slots.
const BUILD_WAIT_MS: u64 = 600_000;

/// The bound on slots held, which are nine bytes an artifact.
const SLOTS_BYTES: u64 = 256 << 20;

/// `size` as a palette size, or the refusal naming the sizes there are.
pub fn check_palette_size(size: u32) -> Result<u8> {
    u8::try_from(size)
        .ok()
        .filter(|size| PALETTE_SIZES.contains(size))
        .ok_or(EngineError::PaletteRefused(size))
}

/// Whether a level's centres are the centroids its figures carry: the layer declares a centroid
/// or a box and the level is served from its column alone, so a route that draws it fills them.
fn centred_by_figures(layer: &RegisteredLayer, read: &ReadLevel) -> bool {
    crate::figures::Geometry::declared(&layer.declaration) != crate::figures::Geometry::None
        && read.rows.column().is_some()
        && !read.rows.membership().rows_held()
}

/// One level's slots for one viewer and one palette size, and the centre each cluster was placed
/// at, by ordinal.
pub(crate) struct LevelSlots {
    slots: Vec<u8>,
    /// `NaN` where the cluster has no centre or is not served.
    centres: Vec<[f32; 2]>,
    pub(crate) stats: SlotStats,
}

impl LevelSlots {
    /// The slot of the cluster at `ordinal`, where this viewer is served it.
    pub(crate) fn get(&self, ordinal: u32) -> Option<u8> {
        let slot = *self.slots.get(ordinal as usize)?;
        (slot != NO_SLOT).then_some(slot)
    }

    fn centre(&self, ordinal: u32) -> Option<[f64; 2]> {
        let [x, y] = *self.centres.get(ordinal as usize)?;
        (!x.is_nan()).then_some([f64::from(x), f64::from(y)])
    }

    fn weight(&self) -> u64 {
        self.slots.len() as u64 * 9 + 64
    }
}

impl tessera_cache::CacheWeight for LevelSlots {
    fn cache_weight_bytes(&self) -> u64 {
        self.weight()
    }
}

/// What one build of a level's slots did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SlotStats {
    /// Clusters given a slot.
    pub clusters: u64,
    /// Pairs of clusters drawn beside each other at one depth or more.
    pub edges: u64,
    /// Those pairs whose two clusters hold one slot.
    pub clashes: u64,
    /// Clusters centred on the centroid their level's figures carry.
    pub from_figures: u64,
    /// Visible items in the sample, where the level's centres come from one.
    pub sampled_items: u64,
    /// Clusters with no member in the sample, centred beside an ancestor.
    pub beside_ancestor: u64,
    /// Clusters with no centre, which have no neighbours.
    pub without_centre: u64,
    /// The build's wall time, in microseconds.
    pub build_us: u64,
}

impl SlotStats {
    /// Two levels' figures together.
    pub fn and(self, other: SlotStats) -> SlotStats {
        SlotStats {
            clusters: self.clusters + other.clusters,
            edges: self.edges + other.edges,
            clashes: self.clashes + other.clashes,
            from_figures: self.from_figures + other.from_figures,
            sampled_items: self.sampled_items.max(other.sampled_items),
            beside_ancestor: self.beside_ancestor + other.beside_ancestor,
            without_centre: self.without_centre + other.without_centre,
            build_us: self.build_us + other.build_us,
        }
    }
}

/// What a viewer's slots are a function of where a change can withhold something: a deletion, a
/// suppression or an unsuppression, an edit, a fold, the session, the layer registry, the layer's
/// edges and the layers it depends on. A change here is answered with slots built over it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WithheldKey {
    token_id: u64,
    view: String,
    layer: String,
    entity: u64,
    level: u32,
    palette: u8,
    registry_version: u64,
    /// Each level's lineage version.
    lineages: Vec<u64>,
    /// Each layer it depends on, with each of its levels' record and lineage versions.
    dependencies: Vec<(String, Vec<(u64, u64)>)>,
    deny_epoch: u64,
    edit_epoch: u64,
    fold_epoch: u64,
    fragment_identity: [u8; 32],
}

/// What a viewer's slots are a function of where a change only adds: a flush, an ingest, a growth
/// of the layer's memberships, a publication. A change here alone is answered with the slots held
/// and rebuilt after the response.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AddedKey {
    /// Each level's record version.
    levels: Vec<u64>,
    segments_version: u64,
    projection_segments_version: u64,
    overlay_version: u64,
    fragment_watermark: u64,
}

/// A rebuild a request left for after its response.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    view: String,
    layer: String,
    level: u32,
    palette: u8,
}

/// The slots held: the newest built for each [`WithheldKey`], what it was built at, and the
/// rebuilds wanted, by session.
pub(crate) struct SlotsCache {
    builds: tessera_cache::SingleFlightCache<(WithheldKey, AddedKey), LevelSlots>,
    newest: Mutex<FxHashMap<WithheldKey, (AddedKey, Arc<LevelSlots>)>>,
    wanted: Mutex<FxHashMap<u64, Vec<Wanted>>>,
}

impl SlotsCache {
    pub(crate) fn new() -> Self {
        let builds = tessera_cache::SingleFlightCache::new(SLOTS_BYTES);
        builds.set_wait_budget_ms(BUILD_WAIT_MS);
        SlotsCache {
            builds,
            newest: Mutex::default(),
            wanted: Mutex::default(),
        }
    }

    /// Hold `slots` as the newest for `withheld`, letting older entries go while the held
    /// slots weigh more than [`SLOTS_BYTES`].
    fn hold(&self, withheld: WithheldKey, added: AddedKey, slots: Arc<LevelSlots>) {
        let mut newest = self.newest.lock().unwrap_or_else(PoisonError::into_inner);
        newest.insert(withheld.clone(), (added, slots));
        let mut weight: u64 = newest.values().map(|(_, s)| s.weight()).sum();
        while weight > SLOTS_BYTES {
            let Some(other) = newest.keys().find(|k| **k != withheld).cloned() else {
                break;
            };
            if let Some((_, gone)) = newest.remove(&other) {
                weight -= gone.weight();
            }
        }
    }
}

/// Whether the request `cancel` belongs to has gone.
fn gone(cancel: &Option<crate::CancelToken>) -> bool {
    cancel.as_ref().is_some_and(|c| c.is_cancelled())
}

impl Engine {
    /// This viewer's slots over `level` of `layer` for a palette of `palette` colours: see the
    /// module doc. `mask` is the viewer's composed mask; a filter it carries changes nothing here.
    /// The level is read as the map reads it, so the figures filled are the map's entry, whichever
    /// route asks. `None` for a level the layer does not hold.
    ///
    /// Where only additions separate the slots held from the corpus now, the slots held are
    /// answered and a rebuild is left for [`Self::refresh_cluster_slots`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cluster_slots(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        layer: &RegisteredLayer,
        level: u32,
        palette: u8,
        dependency_served: &dyn Fn(&Attachment) -> bool,
    ) -> Result<Option<Arc<LevelSlots>>> {
        check_palette_size(u32::from(palette))?;
        if level as usize >= layer.runs.len() {
            return Ok(None);
        }
        let (withheld, added) = self.slot_keys(served, layer, level, palette);
        let held = self
            .slots
            .newest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&withheld)
            .cloned();
        if let Some((at, slots)) = held {
            if at != added {
                let wanted = Wanted {
                    view: served.name.to_string(),
                    layer: layer.declaration.name.clone(),
                    level,
                    palette,
                };
                let mut all = self.slots.wanted.lock().unwrap_or_else(PoisonError::into_inner);
                let list = all.entry(withheld.token_id).or_default();
                if !list.contains(&wanted) {
                    list.push(wanted);
                }
            }
            return Ok(Some(slots));
        }
        self.build_and_hold(
            served,
            mask,
            layer,
            level,
            palette,
            dependency_served,
            withheld,
            added,
        )
    }

    /// Build this viewer's slots over `level` and hold them under `withheld` and `added`: `None`
    /// where a tiered level above it has none.
    #[allow(clippy::too_many_arguments)]
    fn build_and_hold(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        layer: &RegisteredLayer,
        level: u32,
        palette: u8,
        dependency_served: &dyn Fn(&Attachment) -> bool,
        withheld: WithheldKey,
        added: AddedKey,
    ) -> Result<Option<Arc<LevelSlots>>> {
        // A tiered level's parents sit at the levels above it, which are coloured first.
        let mut above: Vec<Arc<LevelSlots>> = Vec::new();
        if layer.declaration.hierarchy.kind == HierarchyKind::Tiered {
            for coarser in 0..level {
                match self.cluster_slots(
                    served,
                    mask,
                    layer,
                    coarser,
                    palette,
                    dependency_served,
                )? {
                    Some(slots) => above.push(slots),
                    None => return Ok(None),
                }
            }
        }
        let cancel = served.cancel.clone().unwrap_or_default();
        let slots = self
            .slots
            .builds
            .get_or_try_build_waiting((withheld.clone(), added.clone()), &cancel, || {
                self.build_level(
                    served,
                    mask,
                    layer,
                    level,
                    palette,
                    dependency_served,
                    &above,
                )
            })
            .map_err(crate::figures::waited)?;
        self.slots.hold(withheld, added, Arc::clone(&slots));
        Ok(Some(slots))
    }

    /// Both halves of the key this viewer's slots over `level` of `layer` are held under.
    fn slot_keys(
        &self,
        served: &ServedView<'_>,
        layer: &RegisteredLayer,
        level: u32,
        palette: u8,
    ) -> (WithheldKey, AddedKey) {
        let versions = |name: &str, levels: usize| -> Vec<(u64, u64)> {
            self.write.live().with_artifacts(|store| {
                (0..levels as u32)
                    .map(|level| {
                        (
                            store.level_version(name, level),
                            store.lineage_version(name, level),
                        )
                    })
                    .collect()
            })
        };
        let name = layer.declaration.name.as_str();
        let own = versions(name, layer.runs.len());
        let dependencies = layer
            .declaration
            .depends_on
            .iter()
            .map(|target| {
                let levels = self
                    .write
                    .live()
                    .registered_layer(target)
                    .map_or(0, |target| target.runs.len());
                (target.clone(), versions(target, levels))
            })
            .collect();
        let id = served.mask_identity;
        let generation = served.generation;
        (
            WithheldKey {
                token_id: id.token_id,
                view: served.name.to_string(),
                layer: name.to_string(),
                entity: layer.entity.raw(),
                level,
                palette,
                registry_version: self.write.live().registry_version(),
                lineages: own.iter().map(|&(_, lineage)| lineage).collect(),
                dependencies,
                deny_epoch: generation.deny_epoch,
                edit_epoch: generation.edit_epoch,
                fold_epoch: generation.fold_epoch,
                fragment_identity: id.fragment_identity,
            },
            AddedKey {
                levels: own.iter().map(|&(record, _)| record).collect(),
                segments_version: id.segments_version,
                projection_segments_version: id.projection_segments_version,
                overlay_version: id.overlay_version,
                fragment_watermark: id.fragment_watermark,
            },
        )
    }

    /// Whether a request of `session` left a rebuild of its slots for after its response.
    pub fn cluster_slots_stale(&self, session: &crate::Session) -> bool {
        self.slots
            .wanted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&session.token_id())
    }

    /// Rebuild the slots `session`'s requests found held over a corpus that has since only grown,
    /// over the corpus now. A route calls this once its response is sent.
    pub fn refresh_cluster_slots(&self, session: &crate::Session) -> Result<()> {
        let wanted = self
            .slots
            .wanted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&session.token_id())
            .unwrap_or_default();
        for wanted in wanted {
            self.with_slot_view(session, &wanted.view, &wanted.layer, |served, mask, layer, gate| {
                let (withheld, added) = self.slot_keys(served, layer, wanted.level, wanted.palette);
                self.build_and_hold(
                    served,
                    mask,
                    layer,
                    wanted.level,
                    wanted.palette,
                    gate,
                    withheld,
                    added,
                )
                .map(|_| ())
            })?;
        }
        Ok(())
    }

    /// Run `f` over `layer` in `view` as `session` reads it, with the dependency hook the
    /// viewport uses. A layer the session no longer reads is nothing to do.
    fn with_slot_view<T: Default>(
        &self,
        session: &crate::Session,
        view: &str,
        layer: &str,
        f: impl FnOnce(
            &ServedView<'_>,
            &EffectiveMask,
            &RegisteredLayer,
            &dyn Fn(&Attachment) -> bool,
        ) -> Result<T>,
    ) -> Result<T> {
        let generation = self.generation.load_full();
        let open = self.open_view(
            session,
            &generation,
            view,
            &None,
            &mut crate::timing::Probe::new(),
        )?;
        let Ok(registered) = self.readable_layer(session, &generation, layer, view) else {
            return Ok(T::default());
        };
        let reachable = self.reachable_layers(session);
        let context = crate::viewport::DependencyContext::new(&open.served, &open.mask, &reachable);
        let dependency_served = self.dependency_gate(&context);
        let out = f(&open.served, &open.mask, &registered, &dependency_served)?;
        context.finish()?;
        Ok(out)
    }

    /// What this viewer's slots over every level of `layer` in `view` were built from and took,
    /// building them where they are not held. For `tessera-bench`'s `slot_cost`; not part of the
    /// engine's API.
    #[doc(hidden)]
    pub fn cluster_slot_stats(
        &self,
        session: &crate::Session,
        view: &str,
        layer: &str,
        palette: u8,
    ) -> Result<SlotStats> {
        self.with_slot_view(session, view, layer, |served, mask, registered, gate| {
            let mut stats = SlotStats::default();
            for level in 0..registered.runs.len() as u32 {
                if let Some(slots) =
                    self.cluster_slots(served, mask, registered, level, palette, gate)?
                {
                    stats = stats.and(slots.stats);
                }
            }
            Ok(stats)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn build_level(
        &self,
        served: &ServedView<'_>,
        mask: &EffectiveMask,
        layer: &RegisteredLayer,
        level: u32,
        palette: u8,
        dependency_served: &dyn Fn(&Attachment) -> bool,
        above: &[Arc<LevelSlots>],
    ) -> Result<LevelSlots> {
        let started = std::time::Instant::now();
        let generation = served.generation;
        let shard = generation.bundle.manifest.identity.shard_id;
        let kind = layer.declaration.hierarchy.kind;
        let treed = matches!(kind, HierarchyKind::Nested | HierarchyKind::Dag);
        let stop = || gone(&served.cancel);

        // The clusters this viewer is served, as the artifacts frame serves them: the verdict over
        // the visible set, and content that reads back. A tiered level is read with every level of
        // its layer, so that each parent's heir is chosen among all its children in the viewer's
        // tree, at whatever level they sit; the levels above carry the slots and centres already
        // settled for them.
        let tiered = kind == HierarchyKind::Tiered;
        let levels: Vec<u32> = match tiered {
            true => (0..layer.runs.len() as u32).collect(),
            false => vec![level],
        };
        let mut reads: Vec<ReadLevel> = Vec::new();
        let mut index: Vec<Vec<u32>> = Vec::new();
        let mut clusters: Vec<Cluster> = Vec::new();
        let mut preset: Vec<u8> = Vec::new();
        let mut first = 0;
        let mut ordinals: Vec<u32> = Vec::new();
        for &at_level in &levels {
            let read = self.read_level(served, mask, layer, at_level, true)?;
            let runs = &layer.runs[at_level as usize];
            let view = read.view(self, served, mask, layer, dependency_served);
            let settled = above.get(at_level as usize).filter(|_| at_level < level);
            if at_level == level {
                first = clusters.len();
            }
            let mut at = vec![u32::MAX; read.rows.len()];
            for ordinal in 0..read.rows.len() as u32 {
                if ordinal.is_multiple_of(CHUNK) && stop() {
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
                    centre: settled.and_then(|slots| slots.centre(ordinal)),
                });
                preset.push(settled.and_then(|slots| slots.get(ordinal)).unwrap_or(NO_SLOT));
                if at_level == level {
                    ordinals.push(ordinal);
                }
            }
            drop(view);
            reads.push(read);
            index.push(at);
        }
        let position = |of: u32| if tiered { of as usize } else { 0 };
        let read = &reads[position(level)];
        let here = &index[position(level)];

        // The viewer's tree: each cluster's nearest served ancestors on every path.
        if treed || tiered {
            for (l, at) in index.iter().enumerate() {
                for (ordinal, &c) in at.iter().enumerate() {
                    if c != u32::MAX {
                        clusters[c as usize].parents =
                            nearest_served(&reads, &index, l as u32, ordinal as u32, treed);
                    }
                }
            }
        }

        let mine: Vec<u32> = (first as u32..(first + ordinals.len()) as u32).collect();
        let passes: Vec<Pass> = if treed {
            let lineage_version = self
                .write
                .live()
                .with_artifacts(|store| store.lineage_version(&layer.declaration.name, level));
            let lineage = self.level_lineage(layer, level, lineage_version, &read.rows);
            let of = |served: Vec<u32>| -> Vec<u32> {
                served
                    .into_iter()
                    .map(|o| here[o as usize])
                    .collect()
            };
            crate::cut::depths(&lineage, &ordinals)
                .into_iter()
                .map(|depth| Pass {
                    drawn: of(depth.served),
                    entering: of(depth.entering),
                })
                .collect()
        } else {
            vec![Pass {
                drawn: mine.clone(),
                entering: mine.clone(),
            }]
        };

        let mut stats = SlotStats::default();
        let figures = read
            .counts
            .as_ref()
            .filter(|figures| centred_by_figures(layer, read) && figures.has_geometry());
        let own: Vec<Option<[f64; 2]>> = match figures {
            Some(figures) => {
                let own: Vec<Option<[f64; 2]>> =
                    ordinals.iter().map(|&o| figures.centroid(o)).collect();
                stats.from_figures = own.iter().filter(|c| c.is_some()).count() as u64;
                own
            }
            None => {
                let visible = mask.visible_all();
                let size = self
                    .switches
                    .slot_sample
                    .load(std::sync::atomic::Ordering::Relaxed);
                let cut = sample_cut(visible.cardinality(), size);
                let sample = sample(served, visible, cut, &stop)?;
                stats.sampled_items = sample.len() as u64;
                let copy = served.segments.first().and_then(|&(first, _)| {
                    read.rows.column()?;
                    self.artifact_projections.band_labels(
                        &generation.prefix,
                        served.name,
                        &layer.declaration.name,
                        level,
                        read.level_version,
                        first,
                    )
                });
                sample_centres(read, here, first, &sample, copy, &stop)?
            }
        };
        for (i, centre) in own.iter().enumerate() {
            clusters[first + i].centre = *centre;
        }
        let centred: Vec<Option<[f64; 2]>> = clusters.iter().map(|c| c.centre).collect();
        for c in first..first + ordinals.len() {
            if centred[c].is_some() {
                continue;
            }
            let placed = beside_ancestor(&clusters, &centred, c as u32);
            match placed {
                Some(_) => stats.beside_ancestor += 1,
                None => stats.without_centre += 1,
            }
            clusters[c].centre = placed;
        }

        let (slots, assigned) =
            assign(&clusters, &passes, palette, &preset, &stop).ok_or(EngineError::Cancelled)?;
        stats.clusters = assigned.clusters;
        stats.edges = assigned.edges;
        stats.clashes = assigned.clashes;
        let mut out = vec![NO_SLOT; here.len()];
        let mut centres = vec![[f32::NAN; 2]; out.len()];
        for (i, &ordinal) in ordinals.iter().enumerate() {
            out[ordinal as usize] = slots[first + i];
            if let Some([x, y]) = clusters[first + i].centre {
                centres[ordinal as usize] = [x as f32, y as f32];
            }
        }
        stats.build_us = started.elapsed().as_micros() as u64;
        tracing::debug!(
            layer = %layer.declaration.name,
            level,
            palette,
            clusters = stats.clusters,
            edges = stats.edges,
            clashes = stats.clashes,
            without_centre = stats.without_centre,
            build_us = stats.build_us,
            "cluster slots built"
        );
        Ok(LevelSlots {
            slots: out,
            centres,
            stats,
        })
    }
}

/// Each of a level's served clusters' centre over its members in `sample`, in the order of the
/// clusters, which begin at index `first`; `None` for one with no member there. On a level served
/// from its column the labels are read from the band-order `copy` where the sample came from the
/// first segment's bands, and from the column otherwise.
fn sample_centres(
    read: &ReadLevel,
    at: &[u32],
    first: usize,
    sample: &[Sampled],
    copy: Option<Arc<BandLabels>>,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<Option<[f64; 2]>>> {
    let count = at.iter().filter(|&&c| c != u32::MAX).count();
    let mut sums: Vec<([f64; 2], u32)> = vec![([0.0; 2], 0); count];
    let cluster = |ordinal: u32| {
        at.get(ordinal as usize)
            .copied()
            .filter(|&c| c != u32::MAX)
            .map(|c| c as usize - first)
    };
    match read.rows.column() {
        Some(column) => {
            for (step, item) in sample.iter().enumerate() {
                if (step as u32).is_multiple_of(CHUNK) && stop() {
                    return Err(EngineError::Cancelled);
                }
                match (item.entry, &copy) {
                    (Some(e), Some(copy)) => {
                        if let Some(c) = cluster(copy.label(e)) {
                            add(&mut sums[c], item.position);
                        }
                    }
                    _ => column.for_each_label(item.row, |ordinal| {
                        if let Some(c) = cluster(ordinal) {
                            add(&mut sums[c], item.position);
                        }
                    }),
                }
            }
        }
        None => {
            let rows = Bitmap::of(&sample.iter().map(|s| s.row).collect::<Vec<_>>());
            for (ordinal, _) in at.iter().enumerate().filter(|(_, &c)| c != u32::MAX) {
                if (ordinal as u32).is_multiple_of(CHUNK) && stop() {
                    return Err(EngineError::Cancelled);
                }
                if read.rows.attachment(ordinal as u32).is_some() {
                    continue;
                }
                let (Some(members), Some(c)) =
                    (read.rows.get(ordinal as u32), cluster(ordinal as u32))
                else {
                    continue;
                };
                for row in members.and(&rows).iter() {
                    if let Ok(i) = sample.binary_search_by_key(&row, |s| s.row) {
                        add(&mut sums[c], sample[i].position);
                    }
                }
            }
        }
    }
    Ok(sums
        .iter()
        .map(|&(sum, n)| (n > 0).then(|| [sum[0] / f64::from(n), sum[1] / f64::from(n)]))
        .collect())
}

/// Add `position` to a running sum of positions and their number.
fn add(sum: &mut ([f64; 2], u32), position: [f64; 2]) {
    sum.0[0] += position[0];
    sum.0[1] += position[1];
    sum.1 += 1;
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

/// The centre of the nearest ancestor of `c` with a centre of its own, searched breadth first
/// with parents in ascending order, moved by an offset drawn from `c`'s seed.
fn beside_ancestor(clusters: &[Cluster], own: &[Option<[f64; 2]>], c: u32) -> Option<[f64; 2]> {
    let mut queue: std::collections::VecDeque<u32> =
        clusters[c as usize].parents.iter().copied().collect();
    let mut seen: FxHashSet<u32> = FxHashSet::default();
    while let Some(at) = queue.pop_front() {
        if !seen.insert(at) {
            continue;
        }
        if let Some(centre) = own[at as usize] {
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

/// The `tessera_id` below which a visible item is in the sample, out of `visible` items for a
/// sample of about `size`: every item where there are no more than `size`.
fn sample_cut(visible: u64, size: u64) -> u64 {
    if visible <= size {
        return u64::MAX;
    }
    ((u128::from(size) << 64) / u128::from(visible)) as u64
}

/// The position a stored `(cell code, residual)` names, in the 32-bit grid's units.
fn position(code: u32, residual: u32) -> [f64; 2] {
    let (x, y) = tessera_spatial::unsplit32(MortonCode::new(code), residual);
    [f64::from(x), f64::from(y)]
}

/// One item of the sample: its view row, its position, and its entry in the first segment's bands
/// where it was read from them.
#[derive(Clone, Copy)]
struct Sampled {
    row: u32,
    position: [f64; 2],
    entry: Option<usize>,
}

/// The visible items whose `tessera_id` is below `cut`, ascending by row: read from the narrowest
/// band holding the cut, or from each segment's columns where no band does.
fn sample(
    served: &ServedView<'_>,
    visible: &Bitmap,
    cut: u64,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<Sampled>> {
    let band = tessera_store::bands::band_below(cut);
    let mut out: Vec<Sampled> = Vec::new();
    for (s, &(segment, row_base)) in served.segments.iter().enumerate() {
        if stop() {
            return Err(EngineError::Cancelled);
        }
        if let Some(band) = band {
            let bands = &segment.bands;
            let (rows, ids) = (bands.rows(), bands.ids());
            let (codes, residuals) = (bands.codes(), bands.residuals());
            for e in bands.band(band) {
                let row = row_base + rows[e];
                if ids[e] < cut && visible.contains(row) {
                    out.push(Sampled {
                        row,
                        position: position(codes[e], residuals[e]),
                        entry: (s == 0).then_some(e),
                    });
                }
            }
            continue;
        }
        let (ids, morton, residual) = (
            segment.columns.tessera_id(),
            segment.morton.u32(),
            segment.columns.residual(),
        );
        let mut rows = visible.iter();
        rows.reset_at_or_after(row_base);
        for row in rows.take_while(|&row| row < row_base + segment.row_count) {
            let local = (row - row_base) as usize;
            if cut == u64::MAX || ids[local] < cut {
                out.push(Sampled {
                    row,
                    position: position(morton[local], residual[local]),
                    entry: None,
                });
            }
        }
    }
    Ok(out)
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

/// Every cluster's slot under the rule in the module doc, by index, and what was counted, the
/// clusters `preset` gives a slot keeping it. [`NO_SLOT`] for a cluster no pass colours. `None`
/// once `stop` answers true.
pub(crate) fn assign(
    clusters: &[Cluster],
    passes: &[Pass],
    palette: u8,
    preset: &[u8],
    stop: &dyn Fn() -> bool,
) -> Option<(Vec<u8>, Assigned)> {
    let heirs = heirs(clusters);
    let mut slots = match preset.is_empty() {
        true => vec![NO_SLOT; clusters.len()],
        false => preset.to_vec(),
    };
    let mut assigned = Assigned::default();
    // Every pair drawn beside each other at some depth, smaller index first.
    let mut pairs: FxHashSet<(u32, u32)> = FxHashSet::default();
    // Each drawn cluster's position among the points of the pass, `u32::MAX` where it has none.
    let mut local = vec![u32::MAX; clusters.len()];
    for pass in passes {
        if stop() {
            return None;
        }
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
        for (c, list) in &lists {
            let class = u8::from(inherited(&slots, heirs[*c as usize]).is_some());
            let i = local[*c as usize];
            if i != u32::MAX {
                claims[i as usize].slot = list[0];
                claims[i as usize].class = class;
            }
        }
        let mut chosen: Vec<(u32, u8)> = Vec::with_capacity(lists.len());
        for (step, (c, list)) in lists.iter().enumerate() {
            if (step as u32).is_multiple_of(CHUNK) && stop() {
                return None;
            }
            let i = local[*c as usize];
            if i == u32::MAX {
                chosen.push((*c, list[0]));
                continue;
            }
            let around = near.of(i);
            let mine = claims[i as usize];
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
            chosen.push((*c, slot));
        }
        for (c, slot) in chosen {
            slots[c as usize] = slot;
            assigned.clusters += 1;
        }
        for i in 0..placed.len() as u32 {
            for &j in near.of(i).iter().filter(|&&j| j > i) {
                let (a, b) = (placed[i as usize], placed[j as usize]);
                pairs.insert((a.min(b), a.max(b)));
            }
        }
        for &c in &placed {
            local[c as usize] = u32::MAX;
        }
    }
    assigned.edges = pairs.len() as u64;
    assigned.clashes = pairs
        .iter()
        .filter(|&&(a, b)| slots[a as usize] == slots[b as usize])
        .count() as u64;
    Some((slots, assigned))
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
    fn tree(seed: u64, depth: u32, fan: u64) -> (Vec<Cluster>, Vec<Vec<u32>>) {
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
        (clusters, levels)
    }

    /// The passes a nested layer colours `levels` in, leaving out the clusters `gone` names.
    fn passes(clusters: &[Cluster], levels: &[Vec<u32>], gone: &[u32]) -> Vec<Pass> {
        let has_child: FxHashSet<u32> = clusters
            .iter()
            .enumerate()
            .filter(|(c, _)| !gone.contains(&(*c as u32)))
            .flat_map(|(_, c)| c.parents.clone())
            .collect();
        let mut passes: Vec<Pass> = Vec::new();
        let mut leaves: Vec<u32> = Vec::new();
        for level in levels {
            let level: Vec<u32> = level.iter().copied().filter(|c| !gone.contains(c)).collect();
            let mut drawn = leaves.clone();
            drawn.extend_from_slice(&level);
            passes.push(Pass {
                drawn,
                entering: level.clone(),
            });
            leaves.extend(level.iter().copied().filter(|c| !has_child.contains(c)));
        }
        passes
    }

    /// The corpus without `gone`: the clusters left as they were, `gone` no longer anyone's child.
    fn without(clusters: &[Cluster], gone: u32) -> Vec<Cluster> {
        let mut out = clusters.to_vec();
        out[gone as usize].parents.clear();
        out[gone as usize].centre = None;
        out
    }

    fn colour(clusters: &[Cluster], passes: &[Pass], palette: u8) -> (Vec<u8>, Assigned) {
        assign(clusters, passes, palette, &[], &|| false).expect("never stopped")
    }

    /// Each pass's neighbours as `assign` takes them, by cluster.
    fn graphs(clusters: &[Cluster], passes: &[Pass]) -> Vec<FxHashMap<u32, Vec<u32>>> {
        passes
            .iter()
            .map(|pass| {
                let placed: Vec<u32> = pass
                    .drawn
                    .iter()
                    .copied()
                    .filter(|&c| clusters[c as usize].centre.is_some())
                    .collect();
                let mut points: Vec<[f64; 2]> = placed
                    .iter()
                    .map(|&c| clusters[c as usize].centre.expect("placed"))
                    .collect();
                let seeds: Vec<u64> = placed.iter().map(|&c| clusters[c as usize].seed).collect();
                spread_coincident(&mut points, &seeds);
                let near = neighbours(&points);
                placed
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| {
                        let around = near.of(i as u32).iter().map(|&j| placed[j as usize]);
                        (c, around.collect())
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn every_pair_drawn_beside_each_other_is_counted_and_few_share_a_slot() {
        for palette in [8u8, 10, 20, 22] {
            let (mut edges, mut clashes) = (0u64, 0u64);
            for seed in 0..40 {
                let (clusters, levels) = tree(seed, 4, 6);
                let passes = passes(&clusters, &levels, &[]);
                let (slots, assigned) = colour(&clusters, &passes, palette);
                assert!(slots.iter().all(|&s| s < palette), "every cluster is coloured");
                let pairs: FxHashSet<(u32, u32)> = graphs(&clusters, &passes)
                    .iter()
                    .flat_map(|graph| {
                        graph.iter().flat_map(|(&a, near)| {
                            near.iter().map(move |&b| (a.min(b), a.max(b)))
                        })
                    })
                    .collect();
                let shared = pairs
                    .iter()
                    .filter(|&&(a, b)| slots[a as usize] == slots[b as usize])
                    .count() as u64;
                assert_eq!((assigned.edges, assigned.clashes), (pairs.len() as u64, shared));
                edges += assigned.edges;
                clashes += assigned.clashes;
            }
            let rate = clashes as f64 / edges as f64;
            assert!(rate < 0.05, "{clashes} of {edges} pairs clash at N = {palette}");
        }
    }

    #[test]
    fn an_heir_keeps_its_parents_slot_where_no_neighbour_holds_or_inherits_it() {
        for seed in 0..40 {
            let (clusters, levels) = tree(seed, 4, 5);
            let passes = passes(&clusters, &levels, &[]);
            let (slots, _) = colour(&clusters, &passes, 10);
            let heirs = heirs(&clusters);
            let mut kept = 0;
            for (pass, graph) in passes.iter().zip(graphs(&clusters, &passes)) {
                for &c in &pass.entering {
                    let Some(parent) = heirs[c as usize] else {
                        continue;
                    };
                    let slot = slots[parent as usize];
                    let contested = graph[&c].iter().any(|&n| {
                        let entering = pass.entering.contains(&n);
                        let inherits = heirs[n as usize].is_some_and(|q| slots[q as usize] == slot);
                        (entering && inherits) || (!entering && slots[n as usize] == slot)
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

    /// **A withdrawn leaf moves only the slots it reaches**: its neighbours where it was drawn,
    /// the clusters drawn there beside new neighbours (a parent left with no child among them),
    /// and, where it was its parent's heir, the new heir and that heir's neighbours. Over many
    /// trees and leaves, with every heir chosen again over the corpus without it.
    #[test]
    fn a_withdrawn_leaf_moves_only_its_neighbours_and_the_new_heirs() {
        let mut moved_beyond_neighbours = 0;
        for seed in 0..40 {
            let (clusters, levels) = tree(seed, 4, 6);
            let full = passes(&clusters, &levels, &[]);
            let (before, _) = colour(&clusters, &full, 10);
            let old_heirs = heirs(&clusters);
            let deepest = levels.last().expect("a level");
            for &gone in deepest.iter().step_by(7).take(12) {
                let fewer_clusters = without(&clusters, gone);
                let fewer = passes(&fewer_clusters, &levels, &[gone]);
                let (after, _) = colour(&fewer_clusters, &fewer, 10);
                let new_heirs = heirs(&fewer_clusters);
                let last_before = &graphs(&clusters, &full)[levels.len() - 1];
                let last_after = &graphs(&fewer_clusters, &fewer)[levels.len() - 1];
                let mut reach: FxHashSet<u32> = last_before[&gone].iter().copied().collect();
                // A parent left with no child is drawn at the last depth, beside new neighbours.
                for (&c, near) in last_after {
                    if last_before.get(&c) != Some(near) {
                        reach.insert(c);
                        reach.extend(near.iter().copied());
                    }
                }
                for c in 0..clusters.len() as u32 {
                    if c != gone && old_heirs[c as usize] != new_heirs[c as usize] {
                        reach.insert(c);
                        reach.extend(last_before.get(&c).into_iter().flatten().copied());
                        reach.extend(last_after.get(&c).into_iter().flatten().copied());
                    }
                }
                for c in 0..clusters.len() as u32 {
                    if c == gone || before[c as usize] == after[c as usize] {
                        continue;
                    }
                    assert!(reach.contains(&c), "seed {seed}: {c} moved when {gone} went");
                    moved_beyond_neighbours += u32::from(!last_before[&gone].contains(&c));
                }
            }
        }
        assert!(moved_beyond_neighbours > 0, "a new heir and its neighbours do move");
    }

    /// **A slot depends only on the inputs the module doc lists**: withdrawing a cluster at any
    /// depth moves only clusters one of whose inputs it changed, depth by depth.
    #[test]
    fn a_withdrawal_at_any_depth_moves_only_clusters_whose_inputs_moved() {
        for seed in 0..30 {
            let (clusters, levels) = tree(seed, 4, 4);
            let full = passes(&clusters, &levels, &[]);
            let (before, _) = colour(&clusters, &full, 10);
            let old_heirs = heirs(&clusters);
            let old_graphs = graphs(&clusters, &full);
            for depth in 1..levels.len() - 1 {
                // A leaf at this depth, where there is one; otherwise the depth's first cluster.
                let gone = levels[depth][0];
                let fewer_clusters = without(&clusters, gone);
                let descendants: Vec<u32> = (0..clusters.len() as u32)
                    .filter(|&c| {
                        let mut at = c;
                        while let Some(&p) = clusters[at as usize].parents.first() {
                            if p == gone {
                                return true;
                            }
                            at = p;
                        }
                        false
                    })
                    .collect();
                let mut gone_all = descendants.clone();
                gone_all.push(gone);
                let fewer = passes(&fewer_clusters, &levels, &gone_all);
                let (after, _) = colour(&fewer_clusters, &fewer, 10);
                let new_heirs = heirs(&fewer_clusters);
                let new_graphs = graphs(&fewer_clusters, &fewer);
                for (d, pass) in fewer.iter().enumerate() {
                    let claim_moved = |n: u32| {
                        gone_all.contains(&n)
                            || old_heirs[n as usize] != new_heirs[n as usize]
                            || clusters[n as usize]
                                .parents
                                .iter()
                                .any(|&p| before[p as usize] != after[p as usize])
                            || (!pass.entering.contains(&n)
                                && before[n as usize] != after[n as usize])
                    };
                    for &c in &pass.entering {
                        if before[c as usize] == after[c as usize] {
                            continue;
                        }
                        let old_near = &old_graphs[d][&c];
                        let new_near = &new_graphs[d][&c];
                        let inputs_moved = claim_moved(c)
                            || old_near != new_near
                            || old_near.iter().any(|&n| claim_moved(n));
                        assert!(inputs_moved, "seed {seed}: {c} moved with its inputs unmoved");
                    }
                }
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

    /// **The sample's cut has no step**: a small change in the visible count changes which items
    /// are sampled by about as much, never by half of them, at every count.
    #[test]
    fn a_small_change_in_the_visible_count_moves_few_items_in_or_out_of_the_sample() {
        let mut state = 11u64;
        let ids: Vec<u64> = (0..200_000).map(|_| splitmix(&mut state)).collect();
        let sampled = |visible: u64| {
            let cut = sample_cut(visible, 4_000);
            ids.iter().filter(|&&id| id < cut).count() as i64
        };
        for visible in [8_000u64, 15_999, 16_000, 16_001, 31_999, 32_000, 64_000, 1 << 20] {
            let (a, b) = (sampled(visible), sampled(visible + visible / 100));
            assert!((a - b).abs() <= a / 50 + 10, "{visible}: {a} then {b}");
        }
        assert_eq!(sample_cut(3_000, 4_000), u64::MAX, "few enough are all sampled");
    }
}
