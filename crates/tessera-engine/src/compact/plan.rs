use croaring::Bitmap;

use tessera_store::manifest::{AttrExtent, RecordExtent};

use crate::Generation;

/// One live segment of one view. `dir` is prefix-relative, as the manifest's `files` map uses it.
pub(crate) struct PlannedSegment {
    pub(crate) seg_id: String,
    pub(crate) dir: String,
}

pub(crate) struct FoldViewPlan {
    pub(crate) view: String,
    pub(crate) incarnation: tessera_types::view::ViewIncarnation,
    /// The base and every extent at the snapshot, in any order.
    pub(crate) segments: Vec<PlannedSegment>,
    /// One past the highest entity the snapshot's row space covers, which can be below the live
    /// entity allocator's high-water.
    pub(crate) permutation_bound: u64,
    pub(crate) rows: u64,
}

/// A fold's plan. It consumes every file it names and holds none open, so it survives churn but
/// not a publication that consumed one of its files; the check at publication catches that.
pub(crate) struct FoldPlan {
    pub(crate) partition: String,
    pub(crate) views: Vec<FoldViewPlan>,
    /// Prefix-relative, in the live manifest's order.
    pub(crate) tiers: Vec<String>,
    /// Prefix-relative, oldest first.
    pub(crate) runs: Vec<String>,
    pub(crate) locator_extents: Vec<String>,
    pub(crate) attr_extents: Vec<AttrExtent>,
    pub(crate) record_extents: Vec<RecordExtent>,
    pub(crate) entity_terms_extents: Vec<tessera_store::manifest::EntityTermsExtent>,
    pub(crate) text_extents: Vec<tessera_store::manifest::TextExtent>,
    pub(crate) tombstones: Bitmap,
    /// One past the highest entity with a row in this partition at the snapshot.
    pub(crate) entity_bound: u64,
    /// One past the highest entity whose external-id binding a run records at the snapshot: where
    /// the base run and locator end. A join can give an entity a row before its own row's flush
    /// records the binding, so this can sit below [`Self::entity_bound`]. Publication checks that
    /// every later locator extent begins at or above it.
    pub(crate) binding_bound: u64,
    pub(crate) dict_len: u32,
    pub(crate) small_term_threshold: u32,
    /// A publication into a prefix other than this one is discarded.
    pub(crate) prefix: String,
}

/// Why a tick planned no fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoFold {
    /// A fold would write overlay dispositions that no durable record backs.
    WalPoisoned,
    /// The in-memory overlay no longer matches the durable WAL.
    OverlayDiverged,
    /// A fold would remove the segment a stepped-down side-manifest steps past.
    SteppedDown,
    /// No partition, or no view holding a segment.
    NothingToFold,
    /// Both figures in bytes.
    InsufficientMemory { need: u64, available: u64 },
    /// Both figures in bytes.
    InsufficientDisc { need: u64, free: u64 },
}

impl NoFold {
    /// In [`NoFold::index`] order. Fixed literals, because they reach `/control/status` and must
    /// never carry text derived from the corpus.
    pub(crate) const GATES: [&'static str; 6] = [
        "wal_poisoned",
        "overlay_diverged",
        "stepped_down",
        "nothing_to_fold",
        "insufficient_memory",
        "insufficient_disc",
    ];

    pub(crate) fn index(self) -> usize {
        match self {
            NoFold::WalPoisoned => 0,
            NoFold::OverlayDiverged => 1,
            NoFold::SteppedDown => 2,
            NoFold::NothingToFold => 3,
            NoFold::InsufficientMemory { .. } => 4,
            NoFold::InsufficientDisc { .. } => 5,
        }
    }

    /// The gate's name, then the estimate and the host's figure where the gate has them.
    pub(crate) fn gauge(self) -> (&'static str, Option<u64>, Option<u64>) {
        let (need, had) = match self {
            NoFold::WalPoisoned
            | NoFold::OverlayDiverged
            | NoFold::SteppedDown
            | NoFold::NothingToFold => (None, None),
            NoFold::InsufficientMemory { need, available } => (Some(need), Some(available)),
            NoFold::InsufficientDisc { need, free } => (Some(need), Some(free)),
        };
        (NoFold::GATES[self.index()], need, had)
    }
}

/// Host figures for the two pre-flight refusals, measured by the caller so [`plan_fold`] stays
/// pure. `None` means unknown: that pre-flight is skipped instead of refusing on a guess.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FoldResources {
    pub(crate) available_memory: Option<u64>,
    pub(crate) free_disc: Option<u64>,
    /// Roaring containers across all artifact memberships held. The manifest does not record it.
    pub(crate) membership_containers: u64,
}

/// Covers the one term that needs a postings scan to compute exactly. Assumed, not measured.
const FOLD_MEMORY_SAFETY_FACTOR: u64 = 2;

/// One: the fold runs on one dedicated thread and must not occupy request-serving workers.
pub(super) const TERM_IMAGE_THREADS: usize = 1;

/// Zero: the fold's side-manifest number is only allocated at publication, and any number taken
/// now could collide with a flush published while the fold runs.
pub(super) const TERM_IMAGE_MANIFEST_N: u64 = 0;

/// The fold's peak un-reclaimable memory in bytes: anonymous memory plus the dirty pages of arrays
/// written through a mapping.
///
/// | term | basis |
/// |---|---|
/// | 4 B × permutation bound | `permutation.bin`, written through a mapping |
/// | 4 B × entity bound | `ext-locator.u32`, same |
/// | 8 B × dictionary length | `PostingsSpool`'s offsets buffer |
/// | 90 B × membership containers | the artifact pass's row forms |
/// | threads × (posting + image + frozen + scratch) | pass 2b's window and its projection |
///
/// The permutation and image terms take the widest view: passes 1 and 2b go one view at a time.
pub(crate) fn memory_estimate(
    permutation_bound: u64,
    entity_bound: u64,
    dict_len: u64,
    membership_containers: u64,
    base_rows: u64,
) -> u64 {
    let terms = 4u64
        .saturating_mul(permutation_bound)
        .saturating_add(4u64.saturating_mul(entity_bound))
        .saturating_add(8u64.saturating_mul(dict_len))
        .saturating_add(ARTIFACT_BYTES_PER_CONTAINER.saturating_mul(membership_containers))
        .saturating_add(term_image_estimate(dict_len, permutation_bound, base_rows));
    terms.saturating_mul(FOLD_MEMORY_SAFETY_FACTOR)
}

const BYTES_PER_BITSET_CONTAINER: u64 = 8 * 1024;

const VALUES_PER_CONTAINER: u64 = 1 << 16;

/// Pass 2b's term, per worker: the widest posting, the widest image, the buffer the image is
/// serialised into, and the projection scratch. Zero where the pass does not run.
fn term_image_estimate(dict_len: u64, permutation_bound: u64, base_rows: u64) -> u64 {
    if dict_len == 0 || base_rows == 0 {
        return 0;
    }
    let widest = |values: u64| {
        values
            .div_ceil(VALUES_PER_CONTAINER)
            .saturating_mul(BYTES_PER_BITSET_CONTAINER)
    };
    let image = widest(base_rows);
    let scratch =
        tessera_store::permutation::project_scratch_bound(permutation_bound, base_rows).total();
    let held = widest(permutation_bound)
        .saturating_add(image)
        .saturating_add(image)
        .saturating_add(scratch);
    (TERM_IMAGE_THREADS as u64).saturating_mul(held)
}

/// Resident bytes per Roaring container in the artifact pass. Measured at 78.5 to 94.0 B across
/// artifact counts and membership shapes.
const ARTIFACT_BYTES_PER_CONTAINER: u64 = 90;

/// Free space a fold needs, as a percentage of the live bytes. Carried files are hard links and
/// cost nothing. The 50% margin is assumed: it covers flushes still publishing into the old
/// prefix, its WAL and the fragment cache.
const FOLD_DISC_PERCENT: u64 = 150;

pub(crate) fn disc_estimate(live_bytes: u64) -> u64 {
    live_bytes
        .saturating_mul(FOLD_DISC_PERCENT)
        .saturating_div(100)
}

/// Plans a fold of `generation`'s single partition. Pure: it reads only its arguments.
pub(crate) fn plan_fold(
    generation: &Generation,
    wal_poisoned: bool,
    overlay_diverged: bool,
    resources: FoldResources,
) -> Result<FoldPlan, NoFold> {
    if wal_poisoned {
        return Err(NoFold::WalPoisoned);
    }
    if overlay_diverged {
        return Err(NoFold::OverlayDiverged);
    }
    if generation
        .bundle
        .partitions
        .values()
        .any(|p| p.stepped_down())
    {
        return Err(NoFold::SteppedDown);
    }
    let (partition, partition_data) = generation
        .bundle
        .partitions
        .iter()
        .next()
        .ok_or(NoFold::NothingToFold)?;
    let manifest = &partition_data.manifest;

    let mut views: Vec<FoldViewPlan> = Vec::new();
    // Sorted, so the plan does not depend on a `HashMap`'s iteration order.
    let mut view_ids: Vec<&String> = partition_data.views.keys().collect();
    view_ids.sort_unstable();
    for view in view_ids {
        let view_data = &partition_data.views[view];
        if view_data.segments.is_empty() {
            continue;
        }
        let row_space = &view_data.row_space;
        let permutation_bound = row_space
            .extents()
            .last()
            .map_or(row_space.base().bound(), |extent| extent.entity_hi + 1);
        views.push(FoldViewPlan {
            view: view.clone(),
            incarnation: view_data.incarnation,
            segments: view_data
                .segments
                .iter()
                .map(|segment| PlannedSegment {
                    dir: format!(
                        "partitions/{partition}/{}/segments/{}",
                        tessera_store::view_rel(view),
                        segment.seg_id
                    ),
                    seg_id: segment.seg_id.clone(),
                })
                .collect(),
            permutation_bound,
            rows: row_space.total_rows(),
        });
    }
    if views.is_empty() {
        return Err(NoFold::NothingToFold);
    }

    let entity_bound = views
        .iter()
        .map(|view| view.permutation_bound)
        .max()
        .unwrap_or(0);

    let dict_len = generation.dict.len();
    if let Some(available) = resources.available_memory {
        // The widest view's permutation bound is the partition's entity bound.
        let need = memory_estimate(
            entity_bound,
            entity_bound,
            u64::from(dict_len),
            resources.membership_containers,
            views.iter().map(|view| view.rows).max().unwrap_or(0),
        );
        if need > available {
            return Err(NoFold::InsufficientMemory { need, available });
        }
    }
    if let Some(free) = resources.free_disc {
        // The bundle manifest names the build's files and the partition manifest the write path's.
        let live_bytes: u64 = generation
            .bundle
            .manifest
            .files
            .values()
            .map(|d| d.size)
            .chain(manifest.files.values().map(|d| d.size))
            .sum();
        let need = disc_estimate(live_bytes);
        if need > free {
            return Err(NoFold::InsufficientDisc { need, free });
        }
    }

    let tombstones = generation.overlay.deleted_set().clone();
    let binding_bound = manifest
        .locator_extents
        .iter()
        .map(|extent| extent.entity_hi + 1)
        .fold(generation.bundle.manifest.entity_id_high_water, u64::max);

    Ok(FoldPlan {
        partition: partition.clone(),
        views,
        tiers: manifest.deltas.clone(),
        runs: manifest.external_id_runs.clone(),
        locator_extents: manifest
            .locator_extents
            .iter()
            .map(|extent| extent.path.clone())
            .collect(),
        attr_extents: manifest.attr_extents.clone(),
        record_extents: manifest.record_extents.clone(),
        entity_terms_extents: manifest.entity_terms_extents.clone(),
        text_extents: manifest.text_extents.clone(),
        tombstones,
        entity_bound,
        binding_bound,
        dict_len,
        small_term_threshold: generation.bundle.manifest.small_term_threshold,
        prefix: generation.prefix.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A billion entities over 117 million terms: about 9.4 GB, doubled by the safety factor.
    #[test]
    fn the_memory_estimate_at_a_billion_entities_is_about_19_gb() {
        let need = memory_estimate(1_000_000_000, 1_000_000_000, 117_000_000, 0, 1_000_000_000);
        let gb = need as f64 / 1e9;
        assert!((17.0..=19.0).contains(&gb), "estimate is {gb:.1} GB");
    }

    /// Four bytes per entity for the permutation and four for the locator, doubled.
    #[test]
    fn the_estimate_charges_one_permutation_and_one_locator() {
        assert_eq!(
            memory_estimate(1_000, 1_000, 0, 0, 1_000),
            (4 * 1_000 + 4 * 1_000) * 2,
            "no terms, so no images"
        );
        // 8 B per dictionary ordinal, whatever the entity space.
        assert_eq!(memory_estimate(0, 0, 1_000, 0, 0), 8 * 1_000 * 2);
    }

    /// Pass 2b is charged one posting, one image, its buffer and one scratch, and only where it
    /// runs.
    #[test]
    fn the_estimate_charges_one_posting_one_image_one_buffer_and_one_scratch() {
        assert_eq!(
            memory_estimate(0, 0, 0, 0, 1_000_000),
            0,
            "no terms, so no images"
        );
        assert_eq!(
            memory_estimate(0, 0, 1, 0, 0),
            8 * 2,
            "no rows, so no images"
        );
        let scratch = |bound: u64, rows: u64| {
            tessera_store::permutation::project_scratch_bound(bound, rows).total()
        };
        // One container of rows: an 8 KiB image and a buffer of the same width.
        assert_eq!(
            memory_estimate(0, 0, 1, 0, VALUES_PER_CONTAINER),
            (8 + 2 * BYTES_PER_BITSET_CONTAINER + scratch(0, VALUES_PER_CONTAINER))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // One more row adds a container to the image and to the buffer.
        assert_eq!(
            memory_estimate(0, 0, 1, 0, VALUES_PER_CONTAINER + 1),
            (8 + 4 * BYTES_PER_BITSET_CONTAINER + scratch(0, VALUES_PER_CONTAINER + 1))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // A container of entity space adds its 4 B per entity and one posting container.
        assert_eq!(
            memory_estimate(VALUES_PER_CONTAINER, 0, 1, 0, VALUES_PER_CONTAINER),
            (4 * VALUES_PER_CONTAINER
                + 8
                + 3 * BYTES_PER_BITSET_CONTAINER
                + scratch(VALUES_PER_CONTAINER, VALUES_PER_CONTAINER))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // 3.5 billion rows: an image of about 437 MB.
        let held = term_image_estimate(1, 0, 3_500_000_000) - scratch(0, 3_500_000_000);
        let image = held / 2;
        assert_eq!(held, 2 * image, "image and buffer are one width");
        assert!(
            (430_000_000..=445_000_000).contains(&image),
            "widest image is {image} B"
        );
    }

    /// The artifact pass is free with no artifacts, and 40 million containers cost the 3.5 GB
    /// measured, before doubling.
    #[test]
    fn the_estimate_charges_the_artifact_pass_per_container() {
        assert_eq!(memory_estimate(0, 0, 0, 0, 0), 0, "no artifacts, no charge");
        let need = memory_estimate(0, 0, 0, 40_000_000, 0);
        let gb = need as f64 / 1e9 / FOLD_MEMORY_SAFETY_FACTOR as f64;
        assert!((3.2..=3.9).contains(&gb), "priced at {gb:.1} GB");
    }

    /// The disc estimate is 150% of the live bytes.
    #[test]
    fn the_disc_estimate_carries_a_margin_over_the_bytes_it_would_write() {
        let live = 47u64 << 30;
        let need = disc_estimate(live);
        assert!(need > live, "no margin");
        assert_eq!(need, live + live / 2, "150% of live bytes");
        // Saturates, so an absurd manifest refuses the fold instead of wrapping to a small need.
        assert_eq!(disc_estimate(u64::MAX), u64::MAX / 100);
    }

    /// Every refusal publishes its name, and the two with figures publish those too.
    #[test]
    fn every_refusal_carries_a_name_and_the_two_with_figures_keep_them() {
        assert_eq!(NoFold::WalPoisoned.gauge(), ("wal_poisoned", None, None));
        assert_eq!(
            NoFold::OverlayDiverged.gauge(),
            ("overlay_diverged", None, None)
        );
        assert_eq!(NoFold::SteppedDown.gauge(), ("stepped_down", None, None));
        assert_eq!(
            NoFold::NothingToFold.gauge(),
            ("nothing_to_fold", None, None)
        );
        assert_eq!(
            NoFold::InsufficientMemory {
                need: 9,
                available: 4
            }
            .gauge(),
            ("insufficient_memory", Some(9), Some(4)),
            "need, then available"
        );
        assert_eq!(
            NoFold::InsufficientDisc { need: 71, free: 12 }.gauge(),
            ("insufficient_disc", Some(71), Some(12)),
            "need, then free"
        );
    }

    /// Each gate has its own counter, keyed by `index`, so a standing refusal cannot hide the
    /// others. This catches an index with no entry in `GATES`.
    #[test]
    fn nofold_gates_are_named_and_numbered_once() {
        let arms = [
            NoFold::WalPoisoned,
            NoFold::OverlayDiverged,
            NoFold::SteppedDown,
            NoFold::NothingToFold,
            NoFold::InsufficientMemory {
                need: 9,
                available: 4,
            },
            NoFold::InsufficientDisc { need: 71, free: 12 },
        ];
        assert_eq!(arms.len(), NoFold::GATES.len(), "one counter per gate");
        for (expected, arm) in arms.iter().enumerate() {
            assert_eq!(arm.index(), expected, "numbered in GATES order");
            assert_eq!(
                arm.gauge().0,
                NoFold::GATES[expected],
                "published name matches the counter"
            );
        }
    }
}
