use croaring::Bitmap;

use tessera_store::manifest::{AttrExtent, RecordExtent};

use crate::Generation;

/// One live segment of one view. `dir` is prefix-relative, as the manifest's `files` map uses it.
pub(crate) struct PlannedSegment {
    pub(crate) seg_id: String,
    pub(crate) dir: String,
}

/// One view's half of a fold plan.
pub(crate) struct FoldViewPlan {
    pub(crate) view: String,
    /// The incarnation of `view` this plan folds, stamped into the new base segment.
    pub(crate) incarnation: tessera_types::view::ViewIncarnation,
    /// Every live segment of this view at the snapshot: the base plus every extent. Pass 1 merges
    /// them all in `(morton, tessera_id)` order; input order does not matter.
    pub(crate) segments: Vec<PlannedSegment>,
    /// The bound for this view's new `permutation.bin`: one past the highest entity the
    /// snapshot's row space covers, not the live entity allocator high-water.
    pub(crate) permutation_bound: u64,
    /// The rows this view holds at the snapshot, base and extents together. An upper bound on the
    /// new base's row count, and what [`memory_estimate`] charges pass 2b's image against.
    pub(crate) rows: u64,
}

/// One fold's immutable plan: the files it consumes, and the tombstone set. Pure and holds
/// nothing open, so it survives churn on the executor while the fold runs, but not a publication
/// that consumed one of its files; the rebase check at publication catches that.
pub(crate) struct FoldPlan {
    pub(crate) partition: String,
    pub(crate) views: Vec<FoldViewPlan>,
    /// Every live delta tier, prefix-relative, in the live manifest's order; all consumed.
    pub(crate) tiers: Vec<String>,
    /// Every live external-id run, prefix-relative, oldest first; all consumed by pass 3.
    pub(crate) runs: Vec<String>,
    /// Every live locator extent's path, consumed with the runs they index.
    pub(crate) locator_extents: Vec<String>,
    /// Every attribute extent the partition's side-manifest named at the snapshot. All consumed;
    /// the folded state is a function of the schema, not of deletion history.
    pub(crate) attr_extents: Vec<AttrExtent>,
    /// Every record-blob extent the side-manifest named at the snapshot, on the same
    /// all-or-nothing basis as [`FoldPlan::attr_extents`].
    pub(crate) record_extents: Vec<RecordExtent>,
    /// The entity→term transpose's extents at the snapshot, folded into the new base by pass 4c.
    pub(crate) entity_terms_extents: Vec<tessera_store::manifest::EntityTermsExtent>,
    /// Every text extent the side-manifest named at the snapshot, on the same all-or-nothing basis.
    pub(crate) text_extents: Vec<tessera_store::manifest::TextExtent>,
    /// The plan's tombstone set. Handed to passes 1–3 whole; never [`executed`].
    pub(crate) tombstones: Bitmap,
    /// One past the highest entity with a row anywhere in this partition at the snapshot. Not the
    /// live high-water, which would make the base locator answer "no external id" for a
    /// post-snapshot entity that has one.
    pub(crate) entity_bound: u64,
    /// The dictionary length the term sweep emits records for.
    pub(crate) dict_len: u32,
    pub(crate) small_term_threshold: u32,
    /// The prefix this plan was taken against. A publication into a different one is discarded.
    pub(crate) prefix: String,
}

/// Why a tick planned no fold. Each is a distinct operator-facing condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoFold {
    /// The WAL is poisoned: a fold would write overlay dispositions no durable record backs.
    WalPoisoned,
    /// The in-memory overlay has diverged from the durable WAL.
    OverlayDiverged,
    /// A partition is serving a stepped-down side-manifest: folding it would remove the
    /// stepped-past segment rather than shadow it.
    SteppedDown,
    /// No partition, or a partition with no view holding a segment. Nothing to fold.
    NothingToFold,
    /// The estimated peak memory is above what the host has available. Both figures in bytes.
    InsufficientMemory { need: u64, available: u64 },
    /// The estimated output is above the free space on the device. Both figures in bytes.
    InsufficientDisc { need: u64, free: u64 },
}

impl NoFold {
    /// Every gate, in the order [`NoFold::index`] numbers them, as fixed literals: these names
    /// reach `/control/status` and must never carry corpus-derived text.
    pub(crate) const GATES: [&'static str; 6] = [
        "wal_poisoned",
        "overlay_diverged",
        "stepped_down",
        "nothing_to_fold",
        "insufficient_memory",
        "insufficient_disc",
    ];

    /// This gate's position in [`NoFold::GATES`] and its per-gate counter. Exhaustive, so a new
    /// variant is a compile error here.
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

    /// The gauge form: a stable name for the condition, and the two figures it carries. `None`
    /// for the four conditions with no figures; for the other two, `need` is the estimate and
    /// `had` is what the host answered.
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

/// What the fold's two pre-flight refusals compare against. Measured by the caller, so
/// [`plan_fold`] stays pure: free space and available memory are syscalls against the host, not
/// properties of the generation. `None` means unknowable, and the corresponding pre-flight simply
/// does not run rather than refusing a fold on a guess.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FoldResources {
    pub(crate) available_memory: Option<u64>,
    pub(crate) free_disc: Option<u64>,
    /// Roaring containers across every artifact membership this node holds. Charged per container
    /// ([`ARTIFACT_BYTES_PER_CONTAINER`]); a planner cannot derive it from the manifest alone.
    pub(crate) membership_containers: u64,
}

/// The multiplier on [`memory_estimate`]'s computable terms, covering the one term that needs a
/// postings scan to compute exactly. Assumed, not measured directly.
const FOLD_MEMORY_SAFETY_FACTOR: u64 = 2;

/// Workers the fold's term-image derivation runs across. One, because [`execute`] runs on one
/// dedicated thread and occupying request-serving workers would put maintenance work on the
/// request path. The build passes `rayon::current_num_threads()` instead.
pub(super) const TERM_IMAGE_THREADS: usize = 1;

/// The publication number the fold's term-image files are named after. Zero, because a fold's
/// side-manifest number is allocated at publication, hours after this pass writes the file, and a
/// number taken earlier would collide with a flush published during the flight.
pub(super) const TERM_IMAGE_MANIFEST_N: u64 = 0;

/// The fold's peak un-reclaimable memory in bytes: the anonymous memory plus the dirty pages of
/// the arrays the fold writes through a mapping. Most other resident memory is page cache the
/// kernel can drop under pressure.
///
/// | term | basis |
/// |---|---|
/// | 4 B × permutation bound | `permutation.bin`, written through a mapping |
/// | 4 B × entity bound | `ext-locator.u32`, same |
/// | 8 B × dictionary length | `PostingsSpool`'s offsets buffer |
/// | 90 B × membership containers | the artifact pass's row forms, held while it rebuilds them |
/// | threads × (posting + image + frozen + scratch) | pass 2b's window and its projection |
///
/// The permutation and image terms take the maximum across views, since pass 1 and pass 2b each
/// fold one view at a time.
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

/// The widest a Roaring container can be once built, in bytes: a bitset over its 65 536 values.
const BYTES_PER_BITSET_CONTAINER: u64 = 8 * 1024;

/// Values one Roaring container covers, rows or entities.
const VALUES_PER_CONTAINER: u64 = 1 << 16;

/// [`memory_estimate`]'s pass 2b term: per worker, the widest posting it can hold, the widest
/// image built from one, the buffer that image is serialised into, and the scratch it projects
/// through. Zero where the pass does not run: a view with no row, or a dictionary with no term.
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

/// What one Roaring container costs resident, in bytes: the artifact pass's whole price model.
/// Measured at 78.5–94.0 B per container across a range of artifact counts and membership shapes;
/// the cost is per container rather than per artifact or per member, so 90 covers both.
const ARTIFACT_BYTES_PER_CONTAINER: u64 = 90;

/// The free space a fold needs, as a percentage of the bytes its inputs' manifests name. The
/// fold's own output is at most the live bytes, and carried-forward files are hard links that
/// cost no bytes. The 50% margin covers flushes still publishing into the old prefix, its WAL,
/// and the fragment cache. Assumed rather than measured against a deployment's ingest rate.
const FOLD_DISC_PERCENT: u64 = 150;

/// The free bytes a fold needs before it starts, from the bytes its inputs' manifests name.
pub(crate) fn disc_estimate(live_bytes: u64) -> u64 {
    live_bytes
        .saturating_mul(FOLD_DISC_PERCENT)
        .saturating_div(100)
}

/// Plan a fold of `generation`'s single partition. Pure: it reads the generation, the two
/// executor-health flags and the host figures its caller measured, and nothing else.
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

    // The partition-wide locator span. Publication checks that every post-snapshot locator extent
    // begins above this, rather than assuming it.
    let entity_bound = views
        .iter()
        .map(|view| view.permutation_bound)
        .max()
        .unwrap_or(0);

    // The two pre-flight refusals run last, once the plan's own quantities are known.
    let dict_len = generation.dict.len();
    if let Some(available) = resources.available_memory {
        // The widest view's permutation bound is the partition's entity bound.
        let need = memory_estimate(
            entity_bound,
            entity_bound,
            u64::from(dict_len),
            resources.membership_containers,
            // The widest view's rows: pass 2b derives one view at a time.
            views.iter().map(|view| view.rows).max().unwrap_or(0),
        );
        if need > available {
            return Err(NoFold::InsufficientMemory { need, available });
        }
    }
    if let Some(free) = resources.free_disc {
        // Both manifests are summed: the build's artefacts and the write path's are separate.
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
        dict_len,
        small_term_threshold: generation.bundle.manifest.small_term_threshold,
        prefix: generation.prefix.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The memory estimate is spec §3's table, and the figure it produces at 10⁹ is the one that
    /// document states.** §3 budgets ~9–10 GB at 10⁹ entities over 1.17×10⁸ terms; this asserts the
    /// estimate lands in that band, which is what makes the pre-flight a check against the design
    /// rather than against a number invented at the call site.
    ///
    /// Kills the mutation that drops either 4 B array — either one alone gives ~5.9 GB and the
    /// assertion fails low, which is r4's original error (`permutation.bin` omitted) reintroduced.
    #[test]
    fn the_memory_estimate_is_section_3s_budget_at_ten_to_the_nine() {
        let need = memory_estimate(1_000_000_000, 1_000_000_000, 117_000_000, 0, 1_000_000_000);
        let gb = need as f64 / 1e9;
        assert!(
            (17.0..=19.0).contains(&gb),
            "the estimate is {gb:.1} GB; spec §3 budgets ~9–10 GB and this carries \
             FOLD_MEMORY_SAFETY_FACTOR on top, so ~18.8 GB is the figure"
        );
    }

    /// The permutation term is the **largest** view's, not every view's — pass 1 folds one view
    /// at a time and drops each writer before the next, so a sum would refuse folds a host could
    /// comfortably run. Asserted through the estimate's own arithmetic, since that is where a
    /// reader would look for the rule.
    #[test]
    fn the_estimate_charges_one_permutation_and_one_locator() {
        // 4 B + 4 B per entity, doubled by the safety factor, and no dictionary term.
        assert_eq!(
            memory_estimate(1_000, 1_000, 0, 0, 1_000),
            (4 * 1_000 + 4 * 1_000) * 2,
            "a dictionary of no terms has no images either, whatever the rows"
        );
        // The dictionary term is 8 B per ordinal and independent of entity space.
        assert_eq!(memory_estimate(0, 0, 1_000, 0, 0), 8 * 1_000 * 2);
    }

    /// **Pass 2b is charged one posting, one image, the buffer that image is frozen into and one
    /// scratch, and only where it runs.** The posting and the image are ceilings of a bitset
    /// container per 65 536 entities and per 65 536 rows, and the frozen buffer is charged at the
    /// image's width, so the term moves with entity space and with the view's rows and not with
    /// the dictionary. The scratch is flat.
    ///
    /// Kills the mutation that charges the scratch to a fold with nothing to project, which would
    /// refuse folds on a small host for work the pass skips, and the one that drops either
    /// bitmap or the buffer.
    #[test]
    fn the_estimate_charges_one_posting_one_image_one_buffer_and_one_scratch() {
        assert_eq!(
            memory_estimate(0, 0, 0, 0, 1_000_000),
            0,
            "a dictionary with no term gets no images"
        );
        assert_eq!(
            memory_estimate(0, 0, 1, 0, 0),
            8 * 2,
            "a view with no row gets none either, so only the dictionary term is charged"
        );
        // The scratch is the store's bound over the same row space, not a figure restated here.
        let scratch = |bound: u64, rows: u64| {
            tessera_store::permutation::project_scratch_bound(bound, rows).total()
        };
        // One container of rows and no entity space: one 8 KiB image, the buffer it is frozen
        // into at the same width, and the scratch.
        assert_eq!(
            memory_estimate(0, 0, 1, 0, VALUES_PER_CONTAINER),
            (8 + 2 * BYTES_PER_BITSET_CONTAINER + scratch(0, VALUES_PER_CONTAINER))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // One row past it takes a second container, in the image and in the buffer alike, and
        // nothing else moves.
        assert_eq!(
            memory_estimate(0, 0, 1, 0, VALUES_PER_CONTAINER + 1),
            (8 + 4 * BYTES_PER_BITSET_CONTAINER + scratch(0, VALUES_PER_CONTAINER + 1))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // The posting is a function of the permutation bound, above the 4 B/entity the mapped
        // array costs.
        assert_eq!(
            memory_estimate(VALUES_PER_CONTAINER, 0, 1, 0, VALUES_PER_CONTAINER),
            (4 * VALUES_PER_CONTAINER
                + 8
                + 3 * BYTES_PER_BITSET_CONTAINER
                + scratch(VALUES_PER_CONTAINER, VALUES_PER_CONTAINER))
                * FOLD_MEMORY_SAFETY_FACTOR
        );
        // The memo's figure at rung 6: ~437 MB of image at 3.5×10⁹ rows, and the frozen buffer
        // beside it at the same width.
        let held = term_image_estimate(1, 0, 3_500_000_000) - scratch(0, 3_500_000_000);
        let image = held / 2;
        assert_eq!(
            held,
            2 * image,
            "the image and its buffer are one width each"
        );
        assert!(
            (430_000_000..=445_000_000).contains(&image),
            "the widest image at 3.5×10⁹ rows is {image} B, against the memo's ~437 MB"
        );
    }

    /// **The artifact pass is charged, and charged per container.** A deployment holding no
    /// artifacts pays nothing for it — the term is what tells a fold it cannot fit, so a term that
    /// fired on a corpus with no layers would refuse folds for a pass that does no work.
    ///
    /// The design point is the cross-check: 10⁷ artifacts of four runs each is 4×10⁷ containers,
    /// which the estimate must price at the +3.5 GB the pass was measured to hold — doubled here by
    /// the safety factor, as every other term is.
    #[test]
    fn the_estimate_charges_the_artifact_pass_per_container() {
        assert_eq!(
            memory_estimate(0, 0, 0, 0, 0),
            0,
            "a deployment with no artifacts is charged nothing for the pass"
        );
        let need = memory_estimate(0, 0, 0, 40_000_000, 0);
        let gb = need as f64 / 1e9 / FOLD_MEMORY_SAFETY_FACTOR as f64;
        assert!(
            (3.2..=3.9).contains(&gb),
            "the pass measured +3.5 GB for 4×10⁷ containers; this prices it at {gb:.1} GB"
        );
    }

    /// **The disc estimate is above the live bytes, not equal to them**, which is the margin spec
    /// §8 asks for. An estimate of exactly the output leaves a device that fills at the last
    /// carried-forward link, and the write path goes down behind it (write-path §1.3).
    ///
    /// Kills the mutation that makes `FOLD_DISC_PERCENT` 100.
    #[test]
    fn the_disc_estimate_carries_a_margin_over_the_bytes_it_would_write() {
        let live = 47u64 << 30;
        let need = disc_estimate(live);
        assert!(
            need > live,
            "an estimate of {need} for {live} live bytes has no margin at all"
        );
        assert_eq!(need, live + live / 2, "150% of live bytes");
        // Saturating rather than wrapping: an absurd manifest must refuse the fold, never wrap to
        // a small number and admit it.
        assert_eq!(disc_estimate(u64::MAX), u64::MAX / 100);
    }

    /// **Every refusal has a name on the operator plane, and the two that carry figures keep
    /// them.** A refused fold advances no counter the `compaction` block publishes, so this
    /// mapping is the whole of what an operator sees; an arm that lost its figures would leave
    /// `insufficient_disc` reading as a bare condition when it is the one refusal whose numbers
    /// say how far the device is from folding.
    ///
    /// The match in `gauge` is exhaustive, so a seventh variant does not compile until it is
    /// named here.
    #[test]
    fn every_refusal_carries_a_name_and_the_two_with_figures_keep_them() {
        assert_eq!(
            NoFold::WalPoisoned.gauge(),
            ("wal_poisoned", None, None),
            "a condition with no figures publishes its name and two nulls"
        );
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
            "need first, then what the host answered"
        );
        assert_eq!(
            NoFold::InsufficientDisc { need: 71, free: 12 }.gauge(),
            ("insufficient_disc", Some(71), Some(12)),
            "the gap between these two is what the device has to gain before a fold will start"
        );
    }

    /// **One counter per gate, so a standing refusal cannot bury the others.** `due()` re-fires
    /// every tick once the interval floor has passed, and a refusal stamps no
    /// `last_fold_start_unix`, so a gate that stands increments on every tick while anything
    /// refusing between two of them replaces it in `last_refusal`. The per-gate counters are what
    /// an operator reads instead, and they are keyed by `index`.
    ///
    /// Both `index` and `gauge` are exhaustive, so a seventh variant does not compile until it is
    /// named. What neither catches is a variant given an index the array has no entry for, which
    /// is what this covers.
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
        assert_eq!(
            arms.len(),
            NoFold::GATES.len(),
            "every gate has a counter and every counter has a gate"
        );
        for (expected, arm) in arms.iter().enumerate() {
            assert_eq!(
                arm.index(),
                expected,
                "the arms are numbered in the order GATES names them"
            );
            assert_eq!(
                arm.gauge().0,
                NoFold::GATES[expected],
                "the name a refusal publishes is the one its counter is keyed by"
            );
        }
    }
}
