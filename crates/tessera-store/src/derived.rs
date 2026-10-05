//! **The derived artifact structures, written by whoever produces a prefix** — the tile-index
//! extent column, the row-major column, the containment partition, and the pick that says which of
//! them a level gets.
//!
//! # Why this is here and not in the engine
//!
//! It was in the engine, and only the fold could reach it. `tessera build` writes a prefix too, and
//! a build that cannot write these files leaves every one of them to be derived on the first
//! request that names the level — which is the 111-second truncated response
//! `probes/2026-08-22-artifact-serving-e2e/README.md` §8 reproduces, and the 10–11× the same
//! campaign measures between a fresh bundle and its first fold.
//!
//! The rule this module follows is `tessera-build`'s own manifest comment about the filter
//! artefact: **the format's owner owns both halves, so the writer and the reader cannot drift.**
//! [`crate::membership`] owns the bytes of all three structures — [`pack_tile_index`],
//! [`pack_label_column`]/[`pack_list_column`] and [`pack_containment`] — and this owns the one pass
//! that produces their inputs and the one naming rule that files them. The engine's `TileIndex`,
//! `RowColumn` and `ContainmentPartition` are the *reading* halves and stay where they are, each
//! now composed by calling in here rather than by a second walk of its own.
//!
//! **Beside the formats rather than inside them.** This is the one part of the prefix's artifact
//! machinery that reads bitmaps, and it lives in its own file so [`crate::membership`] can keep
//! saying what is true of it — that the extent format knows nothing about the payload. The
//! boundary this module keeps is the other one: it never sees an `ArtifactRecord`, because every
//! entry point takes a walk the caller closes over its own store with.
//!
//! # What a caller supplies, and why it is a callback
//!
//! A level's memberships live in `tessera-lifecycle`, which this crate does not depend on and must
//! not: the Roaring form is that crate's and the addressing is [`crate::membership`]'s. So every
//! entry point here takes a [`LevelWalk`] — *call me with each ordinal and its projected rows* —
//! which the caller closes over its own store with. Nothing in this file knows what an
//! `ArtifactRecord` is.
//!
//! **A walk is called more than once** and has to be re-runnable: a list column is an offset table
//! sized by one pass and filled by a second, and the shape observation walks separately from the
//! projection. That is why it is a `Fn` and not an iterator.
//!
//! # Two costs this deliberately does not fold together
//!
//! Observing a level's shape and projecting its extents are the same `project_base` per artifact,
//! and a caller that wants both pays it twice. They are separate because they are wanted at
//! different times — the shape decides the layout *before* anything is written, and only the
//! layout says which of the two files is owed — and because the observation holds one membership at
//! a time where a fused pass would hold the level's.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;

use croaring::Bitmap;

use tessera_spatial::shape::{
    contexts_at, read_wkb, read_wkt, CanonReport, DecodeError, PolyCtx, Rect, Shape, ShapeF64,
    Space,
};
use tessera_spatial::{unsplit32, Bounds, Projection, Tile};
use tessera_types::layer::{LayerDeclaration, MembershipSource, ServingLayout, ShapeKind};
use tessera_types::MortonCode;

use crate::read::{tile_ranges_all, SegmentData};

use crate::manifest::{DerivedExtent, DerivedForm};
use crate::membership::{
    pack_containment, pack_shape_rows, pack_tile_index, ShapeRowsPack, TILE_INDEX_EMPTY,
    TILE_INDEX_HOLE,
};

/// One level's artifacts, visited in ascending ordinal with their **base** row projections.
///
/// A hole yields no visit at all, exactly as a level's own iteration does; an artifact whose
/// membership projects to nothing yields an empty bitmap, which is a different fact and is told
/// apart everywhere below.
pub type LevelWalk<'a> = &'a dyn Fn(&mut dyn FnMut(u32, &Bitmap));

/// One level's membership as **`(row, ordinal)` pairs**, in any order — the form
/// [`project_row_column`] immediately flattens its bitmaps into, exposed so a caller that already
/// holds the membership row-addressed can feed it without materialising a bitmap per artifact
/// first. May be called more than once, on [`LevelWalk`]'s terms.
pub type PairWalk<'a> = &'a dyn Fn(&mut dyn FnMut(u32, u32));

/// One term's postings, handed to a visitor — the shape [`SignatureIndex::build`] walks the
/// postings through. See [`PostingSlice`] for why it is a callback and not a return.
pub type PostingWalk<'a> = &'a dyn Fn(u32, &mut dyn FnMut(PostingSlice<'_>)) -> std::io::Result<()>;

/// One level's generating sets, visited in ascending ordinal — rank order within each artifact.
///
/// Separate from [`LevelWalk`] because it is a different projection of the same records: a
/// membership is what a viewport asks about, and a generating set is what containment does.
pub type ContentWalk<'a> = &'a dyn Fn(&mut dyn FnMut(u32, &[&Bitmap]));

// ---------------------------------------------------------------------------------------------
// The node ladder
//
// Moved here from the engine's `tile_index` because the *fraction of a level that no node holds*
// is now what the layout pick reads, and the pick has to be computable at a build that never
// constructs the hierarchy. One definition, two readers.
// ---------------------------------------------------------------------------------------------

/// The finest row range a node addresses: **ten bits, a thousand rows.**
///
/// ⊘ **The probe's constant, measured at no other value** (`design/artifact-serving-at-scale.md`
/// §4.4's closing note, carried into `2026-08-21-artifact-layout-selection.md` §9's constraint 6).
/// The floor trades settling granularity against the index's own size: nodes are bounded at
/// `rows / 2¹⁰`, which is the 4.2 MB §3 measures at 10⁸ rows and the 25.8 MB at 10⁹.
pub const FINEST_SHIFT: u32 = 10;

/// Bits per level: **four, a sixteen-way fan-out.**
///
/// ⊘ The probe's other constant, and measured at no other value either. Morton over two dimensions
/// makes a quadtree level two bits, so four bits is *two* quad levels per index level — half the
/// depth at the cost of a coarser alignment for the settle test.
pub const LEVEL_STEP: u32 = 4;

/// The index's levels for a row space, **coarse to fine**.
///
/// The first entry is the largest shift with `row_count >> shift > 0`, so there are at most
/// `2^LEVEL_STEP` roots however large the corpus is; the last is [`FINEST_SHIFT`].
pub fn tile_index_shifts(row_count: u32) -> Vec<u32> {
    let mut shifts = Vec::new();
    let mut shift = 32;
    while shift > FINEST_SHIFT {
        shift -= LEVEL_STEP;
        if (row_count as u64) >> shift > 0 || shift <= FINEST_SHIFT {
            shifts.push(shift.max(FINEST_SHIFT));
        }
    }
    if shifts.is_empty() {
        shifts.push(FINEST_SHIFT);
    }
    shifts
}

/// **Whether an extent is too wide for every node there is** — the `everywhere` test, answered
/// from one shift.
///
/// The hierarchy places an artifact at the *finest* level whose block holds both ends of its span,
/// so an artifact is in `everywhere` exactly when the **coarsest** level fails that test: a finer
/// level has smaller blocks and cannot succeed where a coarser one did not. That collapses the
/// whole placement to one comparison per artifact, which is what makes the fraction computable in
/// a pass that never builds the tree.
pub fn is_everywhere(lo: u32, hi: u32, coarsest_shift: u32) -> bool {
    (lo >> coarsest_shift) != (hi >> coarsest_shift)
}

/// The shift the `everywhere` test is taken at, for a row space of `row_count` rows.
pub fn coarsest_shift(row_count: u32) -> u32 {
    tile_index_shifts(row_count)[0]
}

// ---------------------------------------------------------------------------------------------
// The shape a level is observed to have, and the pick that reads it
// ---------------------------------------------------------------------------------------------

/// ⊘ **Provisional: the fraction of a level's artifacts too wide for any index node at or above
/// which the level is served row-major.**
///
/// **This axis replaced blocks per artifact on 2026-08-23**, on the campaign's own evidence
/// (`docs/evidence/memos/2026-08-22-artifact-scale-campaign.md`, "The bracket re-run"). Four
/// controlled points with every other fixture statistic held exactly — 0.960 members per row, 32
/// distinct containment expressions, a 2.0 MB tile index — moved blocks per artifact through 6, 8,
/// 10 and 12 and the whole-map cell **fell**, 104.1 → 42.8 → 44.3 → 36.2 ms, with the worst cell
/// falling monotonically 122.3 → 81.7 → 67.4 → 68.1. The one quantity that tracked the cost was the
/// `everywhere` set, rising **1.6% → 2.3% → 2.9% → 3.6%** as it fell. A threshold in blocks per
/// artifact therefore flips *away from a layout that is improving*, which is what the old constant
/// of 10.0 did; blocks per artifact conflates container count with spread, and eight containers
/// clumped settle where eight corner-to-corner do not.
///
/// So this is **0.25**, and every part of that number is an argument rather than a measurement:
///
/// - It is seven times the top of the measured-improving band, so all four bracket points — and
///   the whole region between them — stay artifact-major. A threshold inside 1.6–3.6% would flip
///   in territory where the layout being flipped away from is measurably getting better.
/// - It is far below where the layers that *win* by flipping sit. Every artifact of a scattered
///   layer lands in `everywhere` at every size the campaign measured (§5, and the engine's
///   `tile_index` module doc says the same), so the two 10⁷ enumerated layers whose fold-time flip
///   bought **11× and 10.5×** are at or near 1.0 on this axis and flip on any threshold below it.
/// - Everything between 3.6% and that band is unmeasured, and the direction of the mistake decides
///   where in it to sit. Picking artifact-major where row-major would have been cheaper costs
///   latency on a route measured, built and correct at every size reached; picking row-major where
///   the level does not suit it costs a whole-map scan of `viewport ∩ M_auth` on exactly the
///   request an artifact-major level answers without scanning anything. A quarter is a *quarter of
///   the level* unplaceable — a level that scattered is not one the node walk is doing work for.
///
/// ⊘ **Provisional pending a sweep along this axis.** The campaign's bracket varied blocks per
/// artifact and read the fraction off it; nothing has yet varied the fraction through 0.1–0.9 and
/// measured the crossover, which is what would replace this argument with a number.
pub const ROW_MAJOR_EVERYWHERE_FRACTION: f64 = 0.25;

/// ⊘ **Provisional: the artifact count below which the pick stays artifact-major whatever the
/// spread.**
///
/// A thousand. Below it the whole-map cell is milliseconds on either route — the measured cells run
/// in single-digit milliseconds at 10³ artifacts — so a flip buys nothing measurable and costs a
/// column, a manifest entry and a per-session histogram. Above it the row-major scan's `O(visible
/// rows)` starts to be paid against a per-candidate cost that is climbing with the population.
///
/// It is a **tiebreak and not a bound**: a level under it that is *pinned* row-major is served
/// row-major, because a pin is an operator saying they know something the observations do not.
pub const ROW_MAJOR_MIN_ARTIFACTS: u64 = 1_000;

/// What one level looks like, as a build or a fold observes it — **after** any retirements the
/// caller has already executed, which is exactly the case the fold's re-evaluation exists for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelShape {
    /// How many live artifacts the level holds. Holes are not counted: a retired slot is not an
    /// artifact, and counting it would keep a level that has been emptied looking populous.
    pub artifacts: u64,
    /// **Reported, and no longer the trigger** (decision 0092's (c)). The mean number of Roaring
    /// containers a membership touches — the measured cost model is that bitmap operations cost
    /// O(containers touched) rather than O(cardinality), so this says how much *work* a membership
    /// is. What it does not say is how far that work is spread, which is what the walk pays for;
    /// see [`Self::everywhere_fraction`] and [`ROW_MAJOR_EVERYWHERE_FRACTION`].
    pub blocks_per_artifact: f64,
    /// **The trigger.** The fraction of live artifacts whose extent is too wide for every node of
    /// the tile index — the set the walk cannot place and returns on every request whatever the
    /// viewport. Zero for a level with no artifacts.
    pub everywhere_fraction: f64,
    /// Whether the memberships are disjoint — which decides the **label/list** split and is not a
    /// choice. Observed rather than declared: single-valuedness is a property of the data.
    pub partitions: bool,
}

impl LevelShape {
    /// The shape of a level with nothing in it — what a registration sees, since a level with no
    /// artifacts has no spread to observe.
    pub fn empty() -> Self {
        LevelShape {
            artifacts: 0,
            blocks_per_artifact: 0.0,
            everywhere_fraction: 0.0,
            partitions: true,
        }
    }
}

/// Observe one level's shape in one pass, holding **one membership at a time**.
///
/// Every figure comes from the same walk: the container count is the cost model's own number, the
/// extent is `minimum`/`maximum` over the projection, and the running union is what says whether
/// the memberships partition — dropped the moment an overlap is found, so a level that plainly
/// overlaps pays one intersection rather than a second copy of itself.
pub fn observe_shape(row_count: u32, each: LevelWalk<'_>) -> LevelShape {
    let shift = coarsest_shift(row_count);
    let mut artifacts = 0u64;
    let mut blocks = 0u64;
    let mut everywhere = 0u64;
    let mut partitions = true;
    let mut claimed = Bitmap::new();
    each(&mut |_, rows| {
        if rows.is_empty() {
            return;
        }
        artifacts += 1;
        blocks += rows.statistics().n_containers as u64;
        if let (Some(lo), Some(hi)) = (rows.minimum(), rows.maximum()) {
            if is_everywhere(lo, hi, shift) {
                everywhere += 1;
            }
        }
        if partitions {
            if claimed.intersect(rows) {
                partitions = false;
                claimed = Bitmap::new();
            } else {
                claimed.or_inplace(rows);
            }
        }
    });
    if artifacts == 0 {
        return LevelShape::empty();
    }
    LevelShape {
        artifacts,
        blocks_per_artifact: blocks as f64 / artifacts as f64,
        everywhere_fraction: everywhere as f64 / artifacts as f64,
        partitions,
    }
}

/// **The one rule.** `pin` is the layer's declared override, which is read and never re-derived.
///
/// `source` decides representability before anything else: a shape has no per-row source, and
/// inverting its ranges into a column would materialise the membership the ranges exist to avoid.
/// **A pin never flips**, at the build or at any fold after it. That is the whole point of pinning:
/// a layer whose measured shape says one thing and whose operator knows another — a level about to
/// be grown, a benchmark, a bug being cornered. A nightly fold that silently reverted it would make
/// the key a suggestion.
///
/// **A pinned `column` on a level that does not partition is not corrected here.** Whether the
/// memberships are disjoint is checked where the column is built, and a double claim declines the
/// column and leaves the level artifact-major with a loud trace — so the fallback is one decision at
/// one place rather than a rule this function and the builder would each have to hold.
pub fn choose(declaration: &LayerDeclaration, shape: LevelShape) -> ServingLayout {
    // **An attribute's form follows from its membership and is never re-derived.** A single-valued
    // attribute's members *are* the column, one label per row, so there is no second form to be
    // chosen between — which is why `LayerDeclaration::validate` refuses a pin on it and why the
    // fold's re-evaluation reaches here and leaves it alone.
    //
    // **A spatial level is picked exactly as an enumerated one is.** Its membership is resolved
    // into a per-row source at every segment's publication (`polygon-membership.md` §6.3), so the
    // observation the pick reads is over those resolved rows and the three forms are all
    // available to it: `rows` for a level of few, large shapes; `column` where the shapes
    // partition the corpus, which a boundary level almost always does and which is the form that
    // scales; `list` where they overlap.
    if matches!(declaration.membership, MembershipSource::Attribute(_)) {
        return ServingLayout::RowMajorLabel;
    }
    if let Some(pinned) = declaration.layout {
        return pinned;
    }
    let row_major = shape.artifacts >= ROW_MAJOR_MIN_ARTIFACTS
        && shape.everywhere_fraction >= ROW_MAJOR_EVERYWHERE_FRACTION;
    if !row_major {
        return ServingLayout::ArtifactMajor;
    }
    // **The label/list split follows from the membership, never from the numbers.** A level whose
    // memberships are disjoint has exactly one label per row; one whose memberships overlap needs a
    // list, and pays the larger constant for it.
    if shape.partitions {
        ServingLayout::RowMajorLabel
    } else {
        ServingLayout::RowMajorList
    }
}

// ---------------------------------------------------------------------------------------------
// Byte production
// ---------------------------------------------------------------------------------------------

/// One level's extent column, framed — [`crate::membership::pack_tile_index`]'s input produced in
/// one walk.
///
/// A skipped ordinal is a **hole**, exactly as the row form's `resize_with(|| None)` makes it; a
/// live artifact whose membership projects to nothing is [`TILE_INDEX_EMPTY`], which is a different
/// fact and is told apart by every reader.
///
/// **`ordinals` is the level's own length, holes included, and it is not the walk's business.** A
/// level ending in a retired slot yields no visit for it, so a column sized by the last *visit*
/// would be short — and a reader compares an offered column's length against the level's before
/// adopting it, so a short one is silently dropped and derived again. The count is what the caller
/// knows and the walk does not.
pub fn project_tile_index(ordinals: u32, row_count: u32, each: LevelWalk<'_>) -> Vec<u8> {
    let mut spans: Vec<(u32, u32)> = vec![TILE_INDEX_HOLE; ordinals as usize];
    each(&mut |ordinal, rows| {
        let idx = ordinal as usize;
        if spans.len() <= idx {
            spans.resize(idx + 1, TILE_INDEX_HOLE);
        }
        spans[idx] = match (rows.minimum(), rows.maximum()) {
            (Some(lo), Some(hi)) => (lo, hi),
            _ => TILE_INDEX_EMPTY,
        };
    });
    pack_tile_index(row_count, &spans)
}

/// One level's row-major column, **written front to back into a file under `scratch`** — or `None`
/// where the level cannot take the form asked for.
///
/// # What it holds while it runs
///
/// Nothing sized by the row count. The composition used to fill a row-sized lane — a `u32` an
/// ordinal for the label form, a `u32` an offset for the list form — at a scattered row index, and
/// then pack it; at rung 6 that lane is 14 GB a level, and the engine's fold paid the same price at
/// every fold. Every `(row, ordinal)` pair now goes to a [`crate::partition::Partition`] by row
/// range, and the buckets are replayed in row order into the file the column becomes
/// (`docs/evidence/memos/2026-09-12-bounded-assembly-design.md` §4.6, owner ruling: one
/// implementation, disk-backed on both the build and the fold). What stands is one bucket, its
/// sort, and for the list form one window of counts over the bucket's row range.
///
/// **The window is bounded by the `u32` row key over 128 buckets; the bucket is not.** A record is
/// a `(row, ordinal)` pair and a row carries as many of them as it has ordinals, so a bucket holds
/// one record per member entry in its row range: a level whose rows carry one ordinal each fills it
/// in proportion to the row space, and a list-form level of many ordinals a row fills it in
/// proportion to the level's entries. The pre-flight charges it at the larger of the two
/// (`crates/tessera-build/src/residency.rs`).
///
/// **Each bucket is sorted by `(row, ordinal)` before it is replayed, and the list form's bytes
/// depend on it.** A row's list is written in the order the ordinals reach it, and both walks that
/// feed this — the build's artifact pass and the store's own level iteration — hand ordinals
/// ascending, so today's bytes are a row's ordinals ascending. The sort is what makes that true of
/// the replay too, whatever order the pairs were pushed in.
///
/// `None` on [`ServingLayout::RowMajorLabel`] means the memberships do **not** partition: a row was
/// claimed twice, which is the refusal a declaration could not make because single-valuedness is a
/// property of the data. The caller serves the level artifact-major and says so.
///
/// `None` on [`ServingLayout::RowMajorList`] means the level's member entries outrun the `u32` the
/// format's offsets are, which no level of any corpus built so far reaches and a layer of tens of
/// entries a row over 3.5×10⁹ rows would. The caller's answer is the same one.
///
/// `None` on an artifact-major or spatial layout is the caller asking for a file no writer
/// produces.
///
/// The returned path is the caller's to move into place or to read back and delete; nothing here
/// keeps it.
pub fn project_row_column(
    ordinals: u32,
    row_count: u32,
    layout: ServingLayout,
    scratch: &Path,
    each: LevelWalk<'_>,
) -> crate::Result<Option<std::path::PathBuf>> {
    // **One composition, two feeds.** The pairs are what the partition route takes; a walk that
    // hands whole bitmaps is flattened into them here rather than composed a second way (decision
    // 0139: one implementation across a type family).
    let pairs = |visit: &mut dyn FnMut(u32, u32)| {
        each(&mut |ordinal, rows| {
            for row in rows.iter() {
                visit(row, ordinal);
            }
        });
    };
    project_row_column_pairs(ordinals, row_count, layout, scratch, &pairs)
}

/// [`project_row_column`] fed by `(row, ordinal)` pairs rather than by a bitmap an artifact — see
/// [`PairWalk`]. Its one caller outside this crate is the engine recomposing a level's column from
/// the column it already holds, which is row-addressed and has no per-artifact bitmap to offer.
pub fn project_row_column_pairs(
    ordinals: u32,
    row_count: u32,
    layout: ServingLayout,
    scratch: &Path,
    each: PairWalk<'_>,
) -> crate::Result<Option<std::path::PathBuf>> {
    if matches!(layout, ServingLayout::ArtifactMajor) {
        return Ok(None);
    }
    std::fs::create_dir_all(scratch).map_err(|source| crate::StoreError::Io {
        path: scratch.to_path_buf(),
        source,
    })?;
    let name = scratch_name();

    // The pairs, pushed once. A record is `(row, ordinal)`, the row first because a partition
    // routes on a record's first four bytes.
    let buckets = crate::partition::boundaries_uniform(u64::from(row_count));
    let mut partition = crate::partition::Partition::create(
        scratch,
        &name,
        buckets.clone(),
        ROW_ORDINAL_RECORD,
        u64::from(row_count),
    )?;
    let mut entries: u64 = 0;
    let mut pushed: crate::Result<()> = Ok(());
    each(&mut |row, ordinal| {
        if pushed.is_err() {
            return;
        }
        // A row past the column is a member the projection placed above this view's base row
        // space, which `project_base` does not produce. Guarded rather than trusted: the
        // alternative is a panic on a shape nothing here controls.
        if row >= row_count {
            return;
        }
        let mut record = [0u8; ROW_ORDINAL_RECORD];
        record[..4].copy_from_slice(&row.to_le_bytes());
        record[4..].copy_from_slice(&ordinal.to_le_bytes());
        if let Err(error) = partition.push(&record) {
            pushed = Err(error);
            return;
        }
        entries += 1;
    });
    pushed?;
    let mut store = partition.finish()?;
    // Bucket `k`'s row range, clipped to the column: the boundaries ascend from zero and the last
    // bucket runs to the end of the key type, so clipped they tile `[0, row_count)` in order.
    let range = |k: usize| -> (u32, u32) {
        let lo = buckets[k].min(row_count);
        let hi = buckets
            .get(k + 1)
            .map(|first| (*first).min(row_count))
            .unwrap_or(row_count)
            .max(lo);
        (lo, hi)
    };

    let path = scratch.join(format!("{name}.column"));
    // **The file goes back unless the composition finished.** Every `?` below leaves a truncated
    // column in the scratch directory otherwise, and the caller's answer to a failure is to derive
    // the column on first use, so nothing would ever come back for it. Written as a closure so that
    // one cleanup covers every exit, which is the shape [`crate::partition::Partition`]'s own
    // `Drop` has.
    let mut compose = || -> crate::Result<bool> {
        match layout {
            ServingLayout::ArtifactMajor => unreachable!("returned above"),
            ServingLayout::RowMajorLabel => {
                let mut file =
                    crate::membership::LabelColumnFile::create(&path, ordinals, row_count)?;
                let mut partitions = true;
                for k in 0..buckets.len() {
                    let sorted = sorted_pairs(&store, k)?;
                    store.delete(k)?;
                    for pair in sorted {
                        if !file.put((pair >> 32) as u32, pair as u32)? {
                            partitions = false;
                            break;
                        }
                    }
                    if !partitions {
                        break;
                    }
                }
                if !partitions {
                    return Ok(false);
                }
                file.finish()?;
            }
            ServingLayout::RowMajorList => {
                // **The offsets, then the values**, which is the order the format lays them in
                // and so the order they are written in. The offsets are a running count over the
                // rows, and a bucket holds the rows of one contiguous range, so one pass over the
                // buckets in order produces the whole array; the values then need a second pass
                // over the same buckets. A bucket is read twice and sorted once — the alternative,
                // a second scratch file of values to concatenate afterwards, copies the whole
                // column instead.
                //
                // **A level with more entries than a `u32` counts has no list form.** The format's
                // offsets are `u32`, so the total is one of them; a layer of 47 entries a row over
                // 3.5×10⁹ rows is past it, and truncating the count would write a column whose
                // offsets wrap. The caller serves the level artifact-major, which is the same
                // answer the double-claim path gives and is unchanged in what any viewer is told.
                let Some(entries) = list_form_entries(entries) else {
                    return Ok(false);
                };
                let mut file =
                    crate::membership::ListColumnFile::create(&path, ordinals, row_count, entries)?;
                let mut at: u64 = 0;
                file.offset(at as u32)?;
                for k in 0..buckets.len() {
                    let (lo, hi) = range(k);
                    let mut counts = vec![0u32; (hi - lo) as usize];
                    for record in store.load(k)?.as_chunks::<ROW_ORDINAL_RECORD>().0 {
                        let row = u32::from_le_bytes(record[..4].try_into().expect("four bytes"));
                        counts[(row - lo) as usize] += 1;
                    }
                    for count in counts {
                        at += u64::from(count);
                        file.offset(at as u32)?;
                    }
                }
                for k in 0..buckets.len() {
                    let sorted = sorted_pairs(&store, k)?;
                    store.delete(k)?;
                    for pair in sorted {
                        file.value(pair as u32)?;
                    }
                }
                file.finish()?;
            }
        }
        Ok(true)
    };
    match compose() {
        Ok(true) => Ok(Some(path)),
        Ok(false) => {
            let _ = std::fs::remove_file(&path);
            Ok(None)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            Err(error)
        }
    }
}

/// The most row ranges one artifact's covering holds.
pub const COVERING_RANGES: usize = 32;

/// A row-major column's labels, read by [`project_row_members`].
#[derive(Clone, Copy)]
pub enum ColumnLabels<'a> {
    Label(&'a crate::membership::LabelColumnPack),
    List(&'a crate::membership::ListColumnPack),
}

impl ColumnLabels<'_> {
    fn ordinals(&self) -> u32 {
        match self {
            ColumnLabels::Label(pack) => pack.ordinals(),
            ColumnLabels::List(pack) => pack.ordinals(),
        }
    }

    fn rows(&self) -> u32 {
        match self {
            ColumnLabels::Label(pack) => pack.rows(),
            ColumnLabels::List(pack) => pack.rows(),
        }
    }

    /// Every ordinal of the column, once for each row carrying it.
    fn for_each_ordinal(&self, mut visit: impl FnMut(u32)) {
        let rows = self.rows() as usize;
        match self {
            ColumnLabels::Label(pack) => {
                pack.for_each_row_label(0, rows, |_, ordinal| visit(ordinal))
            }
            ColumnLabels::List(pack) => pack.for_each_value(0, rows, visit),
        }
    }

    /// Every `(row, ordinal)` of the column, in row order.
    fn for_each(&self, visit: impl FnMut(u32, u32)) {
        let rows = self.rows() as usize;
        match self {
            ColumnLabels::Label(pack) => pack.for_each_row_label(0, rows, visit),
            ColumnLabels::List(pack) => pack.for_each_row_value(0, rows, visit),
        }
    }
}

/// **One level's members by artifact and their coverings, written from its column's labels into a
/// file under `scratch`** ([`crate::row_members`] for the format). The build, the fold and the
/// engine's own compositions all call this on the column they have just written, so the two forms
/// are always the column's transpose and never a second reading of the memberships.
///
/// Each artifact's covering is its row span split at its `COVERING_RANGES − 1` widest gaps between
/// members, the wider gap first and the lower one on a tie: of the coverings with that many ranges,
/// the one holding the fewest rows that are not members.
///
/// # What it holds while it runs
///
/// The pairs go to a disk partition by ordinal and each bucket is read back in turn, as the
/// column's own composition does by row. A bucket holds at most
/// [`crate::partition::PARTITION_BUCKET_RECORDS`] entries, or one artifact alone where that
/// artifact has more. What stands is the bucket's rows, 4 B an entry, or for an artifact alone in
/// its bucket only its bitmap as it grows. The coverings, 8 B a range, are held until the file is
/// finished.
pub fn project_row_members(
    labels: ColumnLabels<'_>,
    scratch: &Path,
) -> crate::Result<std::path::PathBuf> {
    std::fs::create_dir_all(scratch).map_err(|source| crate::StoreError::Io {
        path: scratch.to_path_buf(),
        source,
    })?;
    let name = scratch_name();
    let path = scratch.join(format!("{name}.members"));
    let written = write_row_members(labels, scratch, &name, &path);
    if written.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    written.map(|()| path)
}

fn write_row_members(
    labels: ColumnLabels<'_>,
    scratch: &Path,
    name: &str,
    path: &Path,
) -> crate::Result<()> {
    let ordinals = labels.ordinals();
    let mut counts = vec![0u64; ordinals as usize];
    labels.for_each_ordinal(|ordinal| {
        if let Some(count) = counts.get_mut(ordinal as usize) {
            *count += 1;
        }
    });
    let mut file = crate::row_members::RowMembersFile::create(
        path,
        ordinals,
        labels.rows(),
        COVERING_RANGES as u16,
    )?;
    let entries: u64 = counts.iter().sum();
    if entries == 0 {
        return file.finish();
    }
    // A bucket closes before the ordinal that would take it past the target, so one holds a run
    // of ordinals near the target, or a single ordinal over it.
    // Each ordinal's bucket is looked up rather than searched for, since every pair takes one.
    let target = crate::partition::PARTITION_BUCKET_RECORDS;
    let mut boundaries = vec![0u32];
    let mut bucket_of = vec![0u32; ordinals as usize];
    let mut held = 0u64;
    for (ordinal, &count) in counts.iter().enumerate() {
        if held > 0 && held + count > target {
            boundaries.push(ordinal as u32);
            held = 0;
        }
        held += count;
        bucket_of[ordinal] = (boundaries.len() - 1) as u32;
    }
    let mut partition = crate::partition::Partition::create_routed(
        scratch,
        name,
        boundaries.len(),
        ROW_ORDINAL_RECORD,
        entries,
    )?;
    let mut pushed: crate::Result<()> = Ok(());
    labels.for_each(|row, ordinal| {
        if pushed.is_err() || ordinal >= ordinals {
            return;
        }
        let mut record = [0u8; ROW_ORDINAL_RECORD];
        record[..4].copy_from_slice(&ordinal.to_le_bytes());
        record[4..].copy_from_slice(&row.to_le_bytes());
        if let Err(error) = partition.push_to(bucket_of[ordinal as usize] as usize, &record) {
            pushed = Err(error);
        }
    });
    drop(bucket_of);
    pushed?;
    let mut store = partition.finish()?;
    let record = |bytes: &[u8]| {
        (
            u32::from_le_bytes(bytes[..4].try_into().expect("four bytes")),
            u32::from_le_bytes(bytes[4..].try_into().expect("four bytes")),
        )
    };
    for k in 0..boundaries.len() {
        let lo = boundaries[k];
        let hi = boundaries.get(k + 1).copied().unwrap_or(ordinals);
        if hi - lo == 1 {
            // One artifact alone, its rows arriving ascending in the order they were pushed.
            let mut builder = MemberBuilder::new();
            store.read_each(k, |bytes| {
                builder.push(record(bytes).1);
                Ok(())
            })?;
            let (members, covering) = builder.finish();
            file.push(lo, &members, &covering)?;
        } else {
            // A run of ordinals: their rows counting-sorted by ordinal, which keeps each one's rows
            // in the ascending order they were pushed in.
            let mut starts: Vec<usize> = Vec::with_capacity((hi - lo) as usize + 1);
            let mut total = 0usize;
            for ordinal in lo..hi {
                starts.push(total);
                total += counts[ordinal as usize] as usize;
            }
            starts.push(total);
            let mut cursor = starts.clone();
            let mut rows = vec![0u32; total];
            store.read_each(k, |bytes| {
                let (ordinal, row) = record(bytes);
                let at = &mut cursor[(ordinal - lo) as usize];
                rows[*at] = row;
                *at += 1;
                Ok(())
            })?;
            for ordinal in lo..hi {
                let at = (ordinal - lo) as usize;
                let mine = &rows[starts[at]..starts[at + 1]];
                if mine.is_empty() {
                    continue;
                }
                let mut builder = MemberBuilder::new();
                for &row in mine {
                    builder.push(row);
                }
                let (members, covering) = builder.finish();
                file.push(ordinal, &members, &covering)?;
            }
        }
        store.delete(k)?;
    }
    file.finish()
}

/// One artifact's bitmap and covering, built from its rows as they arrive ascending: each Roaring
/// container handed over whole, and the widest gaps kept as they pass.
struct MemberBuilder {
    sink: tessera_roaring::Sink,
    key: u32,
    run: Vec<u32>,
    first: Option<u32>,
    last: u32,
    /// The widest gaps so far as `(Reverse(length), start)`, so the heap's top is the one to drop:
    /// the narrowest, and of equals the highest.
    gaps: std::collections::BinaryHeap<(std::cmp::Reverse<u32>, u32)>,
}

impl MemberBuilder {
    fn new() -> Self {
        MemberBuilder {
            sink: tessera_roaring::Sink::new(),
            key: 0,
            run: Vec::new(),
            first: None,
            last: 0,
            gaps: std::collections::BinaryHeap::new(),
        }
    }

    fn push(&mut self, row: u32) {
        match self.first {
            None => self.first = Some(row),
            Some(_) if row <= self.last => return,
            Some(_) => {
                let length = row - self.last - 1;
                if length > 0 {
                    let gap = (std::cmp::Reverse(length), self.last + 1);
                    if self.gaps.len() < COVERING_RANGES - 1 {
                        self.gaps.push(gap);
                    } else if self.gaps.peek().is_some_and(|worst| gap < *worst) {
                        self.gaps.pop();
                        self.gaps.push(gap);
                    }
                }
            }
        }
        self.last = row;
        if row >> 16 != self.key && !self.run.is_empty() {
            self.flush_run();
        }
        self.key = row >> 16;
        self.run.push(row);
    }

    fn flush_run(&mut self) {
        let key = u16::try_from(self.key).expect("a row below 2³² has a block key below 2¹⁶");
        self.sink.push_members(key, &self.run);
        self.run.clear();
    }

    fn finish(mut self) -> (Bitmap, Vec<(u32, u32)>) {
        if !self.run.is_empty() {
            self.flush_run();
        }
        let mut members = self.sink.finish();
        members.run_optimize();
        let Some(first) = self.first else {
            return (members, Vec::new());
        };
        let mut gaps: Vec<(u32, u32)> = self
            .gaps
            .into_iter()
            .map(|(std::cmp::Reverse(length), start)| (start, length))
            .collect();
        gaps.sort_unstable();
        let mut covering = Vec::with_capacity(gaps.len() + 1);
        let mut lo = first;
        for (start, length) in gaps {
            covering.push((lo, start - 1));
            lo = start + length;
        }
        covering.push((lo, self.last));
        (members, covering)
    }
}

/// Write the member file for the column staged at `column`, laid out as `layout`, into `scratch`.
/// What the build and the fold call beside each row-major column they compose.
pub fn stage_row_members(
    column: &Path,
    layout: ServingLayout,
    scratch: &Path,
) -> crate::Result<std::path::PathBuf> {
    match layout {
        ServingLayout::RowMajorLabel => {
            let pack = crate::membership::LabelColumnPack::open(column)?;
            project_row_members(ColumnLabels::Label(&pack), scratch)
        }
        _ => {
            let pack = crate::membership::ListColumnPack::open(column)?;
            project_row_members(ColumnLabels::List(&pack), scratch)
        }
    }
}

/// What a level's column is composed as and filed under, given the layout it is served in and
/// whether its observed memberships partition the rows ([`LevelShape::partitions`]), or `None`
/// where it has no column.
///
/// A row-major level's column is the one it is served from. A level served artifact-major gets a
/// label column where its memberships partition the rows, so that every such level has one label
/// per row whatever form serves it: the band-order copies are taken from it
/// ([`stage_band_labels`]), and the level's artifact-major form is unchanged. One whose memberships
/// overlap has none, and is refused here from the shape the layout pick has already observed rather
/// than after its pairs are spilled and sorted. The build and the fold both ask here, so a level
/// gets the same files from either.
pub fn level_column(
    layout: ServingLayout,
    partitions: bool,
) -> Option<(ServingLayout, DerivedForm)> {
    match layout {
        ServingLayout::ArtifactMajor if !partitions => None,
        ServingLayout::ArtifactMajor => {
            Some((ServingLayout::RowMajorLabel, DerivedForm::LevelLabels))
        }
        layout => Some((layout, DerivedForm::RowColumn { layout })),
    }
}

/// Copy the label column just composed at `column` into the entry order of `bands`, the bands of
/// the segment whose rows the column labels, and return the staged copy in `scratch`.
///
/// The copy is taken here, where the column is written, and nowhere else; the column and its copy
/// are therefore always written by one publication at one level version.
pub fn stage_band_labels(
    column: &Path,
    bands: &crate::bands::Bands,
    scratch: &Path,
) -> crate::Result<std::path::PathBuf> {
    let labels = crate::membership::LabelColumnPack::open(column)?;
    let path = scratch.join(format!("{}.band-labels", scratch_name()));
    if let Err(error) = crate::bands::write_band_labels(&path, bands, &labels) {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

/// A `(row, ordinal)` record: the row first, because a partition routes on a record's first four
/// bytes.
const ROW_ORDINAL_RECORD: usize = 8;

/// A level's member entries as the list form's header holds them, or `None` where they outrun it.
///
/// The format's offsets are `u32` and the total is one of them, so a level of more entries than
/// that has no list form at all — the composition declines it and the level is served
/// artifact-major. Separate from its one caller so the boundary can be tested: the corpus that
/// reaches it is 4.3×10⁹ member entries in one level, which no fixture can push.
fn list_form_entries(entries: u64) -> Option<u32> {
    u32::try_from(entries).ok()
}

/// Bucket `k`'s pairs as `row << 32 | ordinal`, ascending — the replay order every form depends on.
///
/// The bucket's bytes are dropped before the sort, so what stands is 8 bytes an entry in the
/// bucket's row range and not sixteen.
fn sorted_pairs(store: &crate::partition::PartitionStore, k: usize) -> crate::Result<Vec<u64>> {
    let bytes = store.load(k)?;
    let mut pairs: Vec<u64> = bytes
        .as_chunks::<ROW_ORDINAL_RECORD>()
        .0
        .iter()
        .map(|record| {
            let row = u32::from_le_bytes(record[..4].try_into().expect("four bytes"));
            let ordinal = u32::from_le_bytes(record[4..].try_into().expect("four bytes"));
            (u64::from(row) << 32) | u64::from(ordinal)
        })
        .collect();
    drop(bytes);
    pairs.sort_unstable();
    Ok(pairs)
}

/// A name no two compositions share. A fold composes levels in parallel and a build runs beside
/// whatever else holds the directory, so the partition's buckets and the column they become carry
/// this process's id and a counter that never repeats within it.
fn scratch_name() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}{}-{:08}",
        ROW_COLUMN_SCRATCH_PREFIX,
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// The name every file this composition leaves in a scratch directory begins with — the partition's
/// buckets and the column itself alike.
///
/// **A crashed fold leaves them behind**, and an engine sweeps them at open
/// ([`crate::derived::sweep_row_column_scratch`]). The prefix is what makes that sweep safe: the
/// engine's cache directory holds the fragment cache and the suggestion indexes too, and a sweep by
/// anything looser would take one of those with it.
pub const ROW_COLUMN_SCRATCH_PREFIX: &str = "row-column-scratch-";

/// Remove whatever a previous process's composition left in `scratch`.
///
/// Every file [`project_row_column`] creates is named from [`ROW_COLUMN_SCRATCH_PREFIX`], and a
/// composition that finishes removes its own; what remains at an open is the leavings of a run that
/// died part-way. Best effort and never a refusal: a scratch file that will not be removed costs
/// disk, and refusing to open a node over it would trade a service for a tidy directory.
pub fn sweep_row_column_scratch(scratch: &Path) {
    let Ok(entries) = std::fs::read_dir(scratch) else {
        return;
    };
    // **What this process wrote is not swept.** A name carries the writing process's id
    // ([`scratch_name`]), and a second open over one cache directory — two nodes on a box, or one
    // process opening twice — would otherwise delete the buckets of a composition running beside
    // it. Only a pid that is not ours can be a run that died.
    let ours = format!("{ROW_COLUMN_SCRATCH_PREFIX}{}-", std::process::id());
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(ROW_COLUMN_SCRATCH_PREFIX) && !name.starts_with(&ours) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The containment partition
// ---------------------------------------------------------------------------------------------

/// One term's postings, as this module needs to read them.
///
/// **An adapter, not a second format.** `tessera-authz` owns `postings.arrow` and this crate does
/// not depend on it, so a caller hands each posting over in whichever of the two shapes it is
/// stored in and the walk below is written once. The decode that produces the shape stays where the
/// format lives.
pub enum PostingSlice<'a> {
    /// Ascending little-endian `u32` entity ids.
    Array(&'a [u8]),
    /// A Roaring bitmap of entity ids.
    Roaring(&'a Bitmap),
}

/// Every entity's term signature, for the entities one level's generating sets name.
///
/// **Built by one pass over the postings, not one probe per entity.** The inverse direction —
/// entity to terms — is not stored anywhere, so the only route is to walk each term's posting and
/// intersect it with the entities wanted. Done per entity that would be `O(terms)` each; done once
/// for the whole level it is `O(terms)` in total, which is why this is a level-scale object and
/// not a lookup.
pub struct SignatureIndex {
    /// `entity → its term ids, ascending`. Absent means *no term reaches this entity*, which is a
    /// real and fail-closed answer rather than a missing one.
    by_entity: HashMap<u32, Vec<u32>>,
}

impl SignatureIndex {
    /// Walk `term_count` postings, keeping only the entities in `wanted`.
    ///
    /// `posting` is called once per term and hands its posting to the visitor, or does not call the
    /// visitor at all where the file carries no record for that term — which is an ordinary answer.
    pub fn build(
        wanted: &Bitmap,
        term_count: u32,
        posting: PostingWalk<'_>,
    ) -> std::io::Result<Self> {
        let mut by_entity: HashMap<u32, Vec<u32>> = HashMap::default();
        if wanted.is_empty() {
            return Ok(SignatureIndex { by_entity });
        }
        for term in 0..term_count {
            posting(term, &mut |slice| match slice {
                // Ascending `u32` little-endian, so the walk is the decode. Tested against
                // `wanted` one at a time because the array form is the *small* terms, and
                // materialising a bitmap to intersect would cost more than the probe.
                PostingSlice::Array(bytes) => {
                    for chunk in bytes.as_chunks::<4>().0 {
                        let entity = u32::from_le_bytes(*chunk);
                        if wanted.contains(entity) {
                            by_entity.entry(entity).or_default().push(term);
                        }
                    }
                }
                // The intersection first: a large term's posting may be the whole corpus, and
                // `and` is O(containers touched) against a generating-set union that is not.
                PostingSlice::Roaring(view) => {
                    for entity in view.and(wanted).iter() {
                        by_entity.entry(entity).or_default().push(term);
                    }
                }
            })?;
        }
        // Terms are walked in ascending ordinal, so every signature is already ascending and
        // duplicate-free — a term's posting names an entity at most once. Asserted rather than
        // sorted: re-sorting would hide a postings file that had stopped being a set.
        debug_assert!(by_entity
            .values()
            .all(|sig| sig.windows(2).all(|w| w[0] < w[1])));
        Ok(SignatureIndex { by_entity })
    }

    fn signature(&self, entity: u32) -> &[u32] {
        self.by_entity
            .get(&entity)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// The interning state, alive only while a level is being composed.
///
/// **It builds exactly the arrays the durable form holds**, so composing and opening a file are
/// the same structure reached two ways rather than two encodings that have to be kept in step.
#[derive(Default)]
struct Interner {
    /// Every expression's canonical encoding, concatenated: `nclauses, (len, terms…)*`.
    words: Vec<u32>,
    /// `at[e]..at[e + 1]` is expression `e`'s words, with the trailing sentinel — so the last
    /// expression needs no special case, the case a reader gets wrong.
    at: Vec<u32>,
    seen: HashMap<Vec<u32>, u32>,
}

impl Interner {
    fn intern(&mut self, canonical: Vec<u32>) -> u32 {
        if let Some(id) = self.seen.get(&canonical) {
            return *id;
        }
        if self.at.is_empty() {
            self.at.push(0);
        }
        let id = (self.at.len() - 1) as u32;
        self.words.extend_from_slice(&canonical);
        self.at.push(self.words.len() as u32);
        self.seen.insert(canonical, id);
        id
    }

    fn finish(self) -> (Vec<u32>, Vec<u32>) {
        let Interner { words, mut at, .. } = self;
        if at.is_empty() {
            at.push(0);
        }
        (words, at)
    }
}

/// One expression identifier per `(artifact, rank)`, with the width the durable form will use.
///
/// **`u16` with a checked promotion to `u32`, never a byte and never a truncation**
/// (`2026-08-21-artifact-layout-selection.md` §9, constraint 4). A truncated identifier does not
/// fail — it names a *different* expression, which is a containment verdict for another artifact's
/// generating set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct IdColumn {
    ids: Vec<u32>,
    wide: bool,
}

impl IdColumn {
    fn push(&mut self, id: u32) {
        self.wide |= u16::try_from(id).is_err();
        self.ids.push(id);
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    /// Bytes per identifier, for the durable form's header and for a residency line.
    fn width(&self) -> u8 {
        if self.wide {
            4
        } else {
            2
        }
    }
}

/// The partition under construction: the interned table and the per-`(artifact, rank)` column.
///
/// **Public because it is the only constructor**, and there must be exactly one: the interning key
/// and the stored expression are the same bytes, so a second assembly route is a way for them to
/// drift. `compose_containment` below drives it from a level's generating sets; the engine's own
/// clause-named test builder drives it from clauses directly, and both reach the same encoder.
#[derive(Default)]
pub struct ContainmentBuilder {
    interner: Interner,
    at: Vec<u32>,
    ids: IdColumn,
}

impl ContainmentBuilder {
    pub fn new() -> Self {
        ContainmentBuilder {
            at: vec![0],
            ..Default::default()
        }
    }

    /// One artifact's ranks, in order, each already canonically encoded by
    /// [`encode_expression`].
    ///
    /// A level is dense over its ordinals and the walk is in ordinal order, but a hole between two
    /// artifacts yields no record at all — so the offsets are carried forward to the ordinal being
    /// written rather than pushed once per record.
    pub fn push(&mut self, ordinal: u32, ranks: impl IntoIterator<Item = Vec<u32>>) {
        let idx = ordinal as usize;
        while self.at.len() <= idx {
            self.at.push(self.ids.len() as u32);
        }
        for expression in ranks {
            let id = self.interner.intern(expression);
            self.ids.push(id);
        }
        self.at.push(self.ids.len() as u32);
    }

    /// Frame what has been pushed. The caller reads it back through
    /// [`crate::membership::ContainmentPack`], which is what makes composing and opening one reader.
    pub fn finish(self) -> Vec<u8> {
        let ContainmentBuilder { interner, at, ids } = self;
        let (words, expr_at) = interner.finish();
        pack_containment(ids.width(), &at, &ids.ids, &expr_at, &words)
    }
}

/// Compose one `(layer, level)`'s containment partition, framed.
///
/// **Two walks of the level under one borrow of the caller's store**, which is the one-snapshot
/// rule (`2026-08-21-artifact-layout-selection.md` §9, constraint 1): the first collects the
/// entities the generating sets name so the signature pass knows what to look for, the second
/// composes. A growth landing between them would leave the expression describing a set the
/// membership no longer has, so both come from the same borrow — which is the caller's to hold, and
/// is why `contents` is a re-runnable walk.
pub fn compose_containment(contents: ContentWalk<'_>, signatures: &SignatureIndex) -> Vec<u8> {
    let mut builder = ContainmentBuilder::new();
    contents(&mut |ordinal, generating| {
        let ranks: Vec<Vec<u32>> = generating
            .iter()
            .map(|set| canonicalise(set, signatures))
            .collect();
        builder.push(ordinal, ranks);
    });
    builder.finish()
}

/// The entities one level's generating sets name, as [`SignatureIndex::build`] wants them.
pub fn generating_entities(contents: ContentWalk<'_>) -> Bitmap {
    let mut wanted = Bitmap::new();
    contents(&mut |_, generating| {
        for set in generating {
            wanted.or_inplace(set);
        }
    });
    wanted
}

/// One generating set's containment expression, canonically encoded.
///
/// **The conservative label join, written down**: a token satisfies the expression when, for every
/// member of the generating set, it holds at least one of that member's terms. So the encoding is
/// one clause per member — its whole signature, ascending — and the clauses are sorted and
/// deduplicated so two generating sets with the same signature multiset intern to the same id.
///
/// A member no term reaches contributes an **empty** clause, which nothing satisfies: an artifact
/// whose generating set includes an entity outside every term is contained by nobody, which is the
/// fail-closed direction on the one test I3 exists to make conservative.
fn canonicalise(generated_from: &Bitmap, signatures: &SignatureIndex) -> Vec<u32> {
    encode_expression(
        generated_from
            .iter()
            .map(|entity| signatures.signature(entity))
            .collect(),
    )
}

/// The canonical encoding of a clause list — sorted and deduplicated, then framed as
/// `nclauses, (len, terms…)*`.
///
/// One function so the interning key and the stored expression cannot be produced by two different
/// rules.
pub fn encode_expression(mut clauses: Vec<&[u32]>) -> Vec<u32> {
    clauses.sort_unstable();
    clauses.dedup();
    let mut words = Vec::with_capacity(1 + clauses.len() * 2);
    words.push(clauses.len() as u32);
    for clause in clauses {
        words.push(clause.len() as u32);
        words.extend_from_slice(clause);
    }
    words
}

// ---------------------------------------------------------------------------------------------
// Filing what was produced
//
// One naming rule, one durability sequence, one manifest-entry shape — shared so that a build and
// a fold cannot file the same structure two ways. Every failure is a dropped entry rather than a
// refusal: these files are derived, and a level without one composes it on first use, which is
// what every request did before they existed.
// ---------------------------------------------------------------------------------------------

/// One derived file waiting to be filed: its coordinates and its bytes.
pub struct Filed {
    pub view: Option<String>,
    pub incarnation: Option<tessera_types::view::ViewIncarnation>,
    pub layer: String,
    pub level: u32,
    pub level_version: u64,
    pub form: DerivedForm,
    pub bytes: FiledBytes,
}

/// Where a filed structure's bytes are: in hand, or already written in a scratch directory.
///
/// **The row column is staged and every other kind is in hand**, and the asymmetry is the point:
/// a column is composed front to back into the file it becomes
/// ([`project_row_column`]), so there is no `Vec<u8>` to hand over — at rung 6 there could not be.
/// The other kinds are bounded by the artifact count rather than by the row count and are built in
/// memory as they always were.
pub enum FiledBytes {
    /// The bytes, to be written where the kind's naming rule puts them.
    InHand(Vec<u8>),
    /// A file already written, to be moved there. Renamed where the scratch and the bundle share a
    /// filesystem and copied where they do not, which is a deployment's choice of cache directory
    /// and not this module's to refuse. The staged file is gone either way once the move succeeds.
    Staged(std::path::PathBuf),
}

impl FiledBytes {
    /// Put the bytes at `dest`, durably — the one place the two forms differ.
    fn place(&self, dest: &Path) -> crate::Result<()> {
        match self {
            FiledBytes::InHand(bytes) => crate::write_and_fsync(dest, bytes),
            FiledBytes::Staged(staged) => {
                if std::fs::rename(staged, dest).is_err() {
                    std::fs::copy(staged, dest).map_err(|source| crate::StoreError::Io {
                        path: dest.to_path_buf(),
                        source,
                    })?;
                    let _ = std::fs::remove_file(staged);
                }
                let file = std::fs::File::open(dest).map_err(|source| crate::StoreError::Io {
                    path: dest.to_path_buf(),
                    source,
                })?;
                file.sync_all().map_err(|source| crate::StoreError::Io {
                    path: dest.to_path_buf(),
                    source,
                })
            }
        }
    }
}

impl Drop for Filed {
    /// **A staged file that was never placed goes back.** A publication drops the items it could
    /// not file — a directory that would not be created, a kind whose fsync failed — and a column
    /// left in a scratch directory by such a drop is disk nothing will ever claim.
    fn drop(&mut self) {
        if let FiledBytes::Staged(path) = &self.bytes {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Create `prefix_dir/partitions/<partition>/<kind>`, or say why not.
fn derived_dir(prefix_dir: &Path, partition: &str, kind: &str) -> Option<std::path::PathBuf> {
    let dir = prefix_dir.join("partitions").join(partition).join(kind);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(source) => {
            tracing::warn!(path = %dir.display(), %source, "a derived-structure directory would not be created");
            None
        }
    }
}

/// The running index each kind's files are named from, carried across every call one publication
/// makes.
///
/// A build files each kind **once per view**. Two calls numbering from zero name the same files,
/// so the second view's bytes replace the first's while both views' manifest entries stand, each
/// of them naming whichever view was written last. One counter per kind, advanced by the number of
/// items every call is given, keeps the index unique across those calls and puts no caller-shaped
/// string in a path. [`derived_name`] states why a view id may not be one.
///
/// A fold files each kind once, with every view's structures in the one vector, so its numbering
/// is the same either way.
///
/// The counters are per kind because the kind is already in the name. One shared counter would
/// leave gaps in each kind's numbering and say nothing more.
#[derive(Debug, Default)]
pub struct DerivedIndex {
    containment: usize,
    tile_index: usize,
    row_column: usize,
    row_members: usize,
    shape_rows: usize,
    shape_held: usize,
    labels: usize,
    band_labels: usize,
    term_images: usize,
}

/// Where one derived file is written and what the manifest calls it.
pub struct DerivedFile {
    /// The file to write.
    pub path: std::path::PathBuf,
    /// The directory holding it, to fsync once the file is durable.
    pub dir: std::path::PathBuf,
    /// The file's prefix-relative name, for the manifest entry and for `files`.
    pub rel: String,
}

/// Make the directory one view's term images go in and name the file
/// (`crate::term_images` for what it holds).
///
/// The counter is advanced before the caller writes anything, on [`file_all`]'s rule: an index
/// handed out again after a failure would name a later call's file after one this call's manifest
/// entry already names. A build files one per view and a fold one per view, so the counter runs
/// across the calls of a single publication.
pub fn term_image_file(
    prefix_dir: &Path,
    partition: &str,
    n: u64,
    index: &mut DerivedIndex,
) -> crate::Result<DerivedFile> {
    const KIND: &str = "term-images";
    let position = index.term_images;
    index.term_images += 1;
    let dir = prefix_dir.join("partitions").join(partition).join(KIND);
    std::fs::create_dir_all(&dir).map_err(|source| crate::StoreError::Io {
        path: dir.clone(),
        source,
    })?;
    let name = derived_name(KIND, n, position, "timg");
    Ok(DerivedFile {
        path: dir.join(&name),
        dir,
        rel: format!("partitions/{partition}/{KIND}/{name}"),
    })
}

/// The naming rule every derived file follows: a layer name and a view id are caller-shaped and
/// never reach a filename; the publication that introduced the file does, with the position
/// [`DerivedIndex`] hands out beside it.
fn derived_name(kind: &str, n: u64, index: usize, extension: &str) -> String {
    format!("{kind}-{n:06}-{index:03}.{extension}")
}

/// Write one kind's files and return the manifest entries that name them.
///
/// The directory entry itself has to be durable, or a crash leaves a manifest naming a file whose
/// name was never written — the rule every other publication follows. A directory that will not
/// fsync drops the whole kind.
/// Writes each item under its form's directory and returns the manifest entries for the ones that
/// landed. A file that will not be written is a dropped entry: the level derives it on first use.
pub fn file_derived(
    prefix_dir: &Path,
    partition: &str,
    n: u64,
    index: &mut DerivedIndex,
    items: Vec<Filed>,
) -> Vec<DerivedExtent> {
    let mut entries = Vec::with_capacity(items.len());
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    for item in &items {
        let kind = item.form.dir();
        let (next, extension) = match &item.form {
            DerivedForm::Containment => (&mut index.containment, "tscp"),
            DerivedForm::TileIndex => (&mut index.tile_index, "tsti"),
            DerivedForm::RowColumn {
                layout: ServingLayout::RowMajorList,
            } => (&mut index.row_column, "tsll"),
            DerivedForm::RowColumn { .. } => (&mut index.row_column, "tslb"),
            DerivedForm::RowMembers => (&mut index.row_members, "tsrm"),
            DerivedForm::ShapeRows { .. } => (&mut index.shape_rows, "tssr"),
            DerivedForm::ShapeHeld => (&mut index.shape_held, "tssh"),
            DerivedForm::LevelLabels => (&mut index.labels, "tslb"),
            DerivedForm::BandLabels { .. } => (&mut index.band_labels, "tsbl"),
        };
        // Every item spends an index, written or not, so a later call never reuses a name this
        // call's manifest entry may already carry.
        let name = derived_name(kind, n, *next, extension);
        *next += 1;
        let Some(dir) = derived_dir(prefix_dir, partition, kind) else {
            continue;
        };
        if let Err(error) = item.bytes.place(&dir.join(&name)) {
            tracing::warn!(
                layer = %item.layer,
                level = item.level,
                view = ?item.view,
                kind,
                %error,
                "a derived artifact structure would not be written; that level derives it on first use"
            );
            continue;
        }
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
        entries.push(DerivedExtent {
            path: format!("partitions/{partition}/{kind}/{name}"),
            layer: item.layer.clone(),
            level: item.level,
            level_version: item.level_version,
            view: item.view.clone(),
            incarnation: item.incarnation,
            form: item.form.clone(),
        });
    }
    for dir in &dirs {
        if let Err(error) = crate::fsync_dir(dir) {
            tracing::warn!(%error, dir = %dir.display(), "a derived-structure directory would not be fsynced; its files are dropped");
            let rel = dir.file_name().and_then(|k| k.to_str()).unwrap_or_default();
            entries.retain(|e| e.form.dir() != rel);
        }
    }
    entries
}

const SHAPE_HELD_MAGIC: &[u8; 4] = b"TSSH";
const SHAPE_HELD_VERSION: u16 = 1;
const SHAPE_HELD_HOLE: u32 = u32::MAX;

/// FNV-1a over a shape's canonical bytes — the per-entry guard that a persisted decomposition is
/// used only for the shape it was descended from. Not a security digest: a decomposition is a
/// function of the shape and discloses nothing, and what this guards against is a stale file.
pub fn canonical_digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Every artifact's decomposition of one `(view, layer, level)`, framed (`ShapeHeldExtent`).
///
/// ```text
/// TSSH | u16 version | u16 reserved (0) | u32 ordinals | u64 level_version
///      | per ordinal: u32 canonical_len — u32::MAX a hole (no shape) | u64 canonical digest
///        | u8 has_bounds | [4 × u32 bounds] | u32 n_interior | u32 n_boundary
///        | n_interior × (u64 prefix, u8 depth) | n_boundary × (u32 cell, u8 parity)
/// ```
///
/// `canonical` is each ordinal's canonical bytes beside its held form, so the entry carries what
/// the reader compares against.
pub fn shape_held_bytes(
    level_version: u64,
    shapes: &[(Option<&[u8]>, Option<&HeldShape>)],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SHAPE_HELD_MAGIC);
    out.extend_from_slice(&SHAPE_HELD_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(shapes.len() as u32).to_le_bytes());
    out.extend_from_slice(&level_version.to_le_bytes());
    for (canonical, held) in shapes {
        let (Some(canonical), Some(held)) = (canonical, held) else {
            out.extend_from_slice(&SHAPE_HELD_HOLE.to_le_bytes());
            continue;
        };
        out.extend_from_slice(&(canonical.len() as u32).to_le_bytes());
        out.extend_from_slice(&canonical_digest(canonical).to_le_bytes());
        match held.bounds {
            Some(b) => {
                out.push(1);
                for v in [b.min_x, b.min_y, b.max_x, b.max_y] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            None => out.push(0),
        }
        out.extend_from_slice(&(held.interior.len() as u32).to_le_bytes());
        out.extend_from_slice(&(held.boundary.len() as u32).to_le_bytes());
        for tile in &held.interior {
            out.extend_from_slice(&tile.prefix.to_le_bytes());
            out.push(tile.depth);
        }
        for (code, parity) in &held.boundary {
            out.extend_from_slice(&code.raw().to_le_bytes());
            out.push(u8::from(*parity));
        }
    }
    out
}

/// One artifact's persisted decomposition, read back — see [`read_shape_held`].
pub struct HeldEntry {
    pub canonical_len: u32,
    pub digest: u64,
    pub bounds: Option<tessera_spatial::shape::Bbox>,
    pub interior: Vec<Tile>,
    pub boundary: Vec<(MortonCode, bool)>,
}

impl HeldEntry {
    /// Whether this entry was descended from exactly these canonical bytes.
    pub fn is_of(&self, canonical: &[u8]) -> bool {
        self.canonical_len as usize == canonical.len() && self.digest == canonical_digest(canonical)
    }

    /// The held form over a shape decoded from bytes this entry [`is_of`](Self::is_of).
    pub fn into_held(self, shape: Shape) -> HeldShape {
        HeldShape {
            shape,
            interior: self.interior,
            boundary: self.boundary,
            bounds: self.bounds,
        }
    }
}

/// Read a persisted decomposition file — or refuse, on [`read_shape_rows`]'s rule: the level
/// version must equal the caller's, every length must frame exactly, and nothing is read short.
pub fn read_shape_held(path: &Path, level_version: u64) -> crate::Result<Vec<Option<HeldEntry>>> {
    let raw = std::fs::read(path).map_err(|source| crate::StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let refuse = |detail: String| crate::StoreError::MalformedBundle {
        detail: format!("shape held {}: {detail}", path.display()),
    };
    let mut at = 0usize;
    let mut take = |n: usize| -> crate::Result<&[u8]> {
        if raw.len() < at + n {
            return Err(refuse(format!("truncated at byte {at}")));
        }
        let s = &raw[at..at + n];
        at += n;
        Ok(s)
    };
    let u32_of = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    let u64_of = |s: &[u8]| u64::from_le_bytes(s.try_into().expect("eight bytes"));
    if take(4)? != SHAPE_HELD_MAGIC {
        return Err(refuse("magic is not TSSH".into()));
    }
    let version = u16::from_le_bytes(take(2)?.try_into().expect("two bytes"));
    if version != SHAPE_HELD_VERSION {
        return Err(refuse(format!(
            "version {version}, expected {SHAPE_HELD_VERSION}"
        )));
    }
    if take(2)? != [0, 0] {
        return Err(refuse("reserved is not 0".into()));
    }
    let ordinals = u32_of(take(4)?);
    let written_at = u64_of(take(8)?);
    if written_at != level_version {
        return Err(refuse(format!(
            "written at level version {written_at}, and the level is at {level_version}"
        )));
    }
    let mut out: Vec<Option<HeldEntry>> = Vec::with_capacity(ordinals as usize);
    for ordinal in 0..ordinals {
        let canonical_len = u32_of(take(4)?);
        if canonical_len == SHAPE_HELD_HOLE {
            out.push(None);
            continue;
        }
        let digest = u64_of(take(8)?);
        let bounds = match take(1)?[0] {
            0 => None,
            1 => {
                let b = take(16)?;
                Some(tessera_spatial::shape::Bbox {
                    min_x: u32_of(&b[0..4]),
                    min_y: u32_of(&b[4..8]),
                    max_x: u32_of(&b[8..12]),
                    max_y: u32_of(&b[12..16]),
                })
            }
            other => return Err(refuse(format!("ordinal {ordinal}: bounds flag {other}"))),
        };
        let n_interior = u32_of(take(4)?) as usize;
        let n_boundary = u32_of(take(4)?) as usize;
        let mut interior = Vec::with_capacity(n_interior);
        for _ in 0..n_interior {
            let t = take(9)?;
            let depth = t[8];
            if depth > 16 {
                return Err(refuse(format!(
                    "ordinal {ordinal}: a tile at depth {depth}"
                )));
            }
            interior.push(Tile {
                prefix: u64_of(&t[0..8]),
                depth,
            });
        }
        let mut boundary = Vec::with_capacity(n_boundary);
        for _ in 0..n_boundary {
            let c = take(5)?;
            boundary.push((MortonCode::new(u32_of(&c[0..4])), c[4] != 0));
        }
        out.push(Some(HeldEntry {
            canonical_len,
            digest,
            bounds,
            interior,
            boundary,
        }));
    }
    if at != raw.len() {
        return Err(refuse(format!(
            "{} bytes, but the entries end at {at} — trailing bytes the packer did not write",
            raw.len()
        )));
    }
    Ok(out)
}

/// One segment's resolved rows as the row form's bytes — [`pack_shape_rows`]'s input produced
/// from the bitmaps [`resolve_segment`] returns, one portable serialisation per live ordinal.
pub fn shape_rows_bytes(
    level_version: u64,
    seg_id: &str,
    row_count: u32,
    rows: &[Option<Bitmap>],
) -> Vec<u8> {
    let entries: Vec<Option<Vec<u8>>> = rows
        .iter()
        .map(|rows| {
            rows.as_ref().map(|rows| {
                if rows.is_empty() {
                    Vec::new()
                } else {
                    rows.serialize::<croaring::Portable>()
                }
            })
        })
        .collect();
    pack_shape_rows(level_version, seg_id, row_count, &entries)
}

/// Read a persisted row form back as the piece it was written from — or refuse.
///
/// **Refused, never adapted** (I11): the file's own key must equal the manifest entry's and the
/// entry's must equal what the caller is resolving for — the level version, the segment id and
/// the segment's row count — and every bitmap must lie below the row count. A file that fails any
/// of these is not this segment's membership under this level's shapes, and the caller resolves
/// the segment again from the geometry.
pub fn read_shape_rows(
    path: &Path,
    level_version: u64,
    seg_id: &str,
    row_count: u32,
) -> crate::Result<Vec<Option<Bitmap>>> {
    let pack = ShapeRowsPack::open(path)?;
    let refuse = |detail: String| crate::StoreError::MalformedBundle {
        detail: format!("shape rows {}: {detail}", path.display()),
    };
    if pack.level_version() != level_version {
        return Err(refuse(format!(
            "written at level version {}, and the level is at {level_version}",
            pack.level_version()
        )));
    }
    if pack.seg_id() != seg_id {
        return Err(refuse(format!(
            "written for segment {}, asked for segment {seg_id}",
            pack.seg_id()
        )));
    }
    if pack.row_count() != row_count {
        return Err(refuse(format!(
            "written over {} rows, and the segment has {row_count}",
            pack.row_count()
        )));
    }
    let mut out: Vec<Option<Bitmap>> = Vec::with_capacity(pack.ordinals() as usize);
    for ordinal in 0..pack.ordinals() {
        let entry = match pack.entry(ordinal) {
            None => {
                out.push(None);
                continue;
            }
            Some([]) => {
                out.push(Some(Bitmap::new()));
                continue;
            }
            Some(bytes) => bytes,
        };
        let rows = Bitmap::try_deserialize::<croaring::Portable>(entry)
            .ok_or_else(|| refuse(format!("ordinal {ordinal}'s bitmap would not deserialise")))?;
        if rows.maximum().is_some_and(|max| max >= row_count) {
            return Err(refuse(format!(
                "ordinal {ordinal} names a row at or past the segment's {row_count}"
            )));
        }
        out.push(Some(rows));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Shapes — what a spatial level holds, and how a segment's rows are resolved against it
// (`polygon-membership.md` §6.3, §6.6).
//
// Here rather than in the engine for the reason the rest of this file gives: the build resolves
// its one segment through the same code the flush and the fold resolve theirs, so the two entry
// points cannot disagree about who is inside a shape (decision 0091). This crate already owns the
// segment's Morton column and `tile_ranges_all`, which is the whole of what the resolution reads
// beside the shape itself.
// ---------------------------------------------------------------------------------------------

/// A shape as one artifact row declares it, before canonicalisation.
///
/// The kind's own fields (`polygon-membership.md` §6.1): a box, a circle or an ellipse is its
/// parameters; a polygon is WKB in a table's `geometry` column or WKT inline.
#[derive(Debug, Clone, PartialEq)]
pub enum ShapeInput {
    Bbox([f64; 4]),
    Circle([f64; 3]),
    Ellipse([f64; 5]),
    Wkb(Vec<u8>),
    Wkt(String),
}

impl ShapeInput {
    /// The kind this input spells, for the check against the layer's declaration.
    pub fn kind(&self) -> ShapeKind {
        match self {
            ShapeInput::Bbox(_) => ShapeKind::Bbox,
            ShapeInput::Circle(_) => ShapeKind::Circle,
            ShapeInput::Ellipse(_) => ShapeKind::Ellipse,
            ShapeInput::Wkb(_) | ShapeInput::Wkt(_) => ShapeKind::Polygon,
        }
    }
}

/// The space a shape's coordinates are written in (`polygon-membership.md` §4.3).
///
/// A property of the submission, never of the layer. `view` is the space the points are stored in
/// — the quantisation frame, whatever produced it. `wgs84` says the coordinates are longitude and
/// latitude and asks the view to project them **with the same function it projects points
/// through**, which [`ShapeSpace::resolve`] is: it takes the view's own declared projection and
/// nothing else, so a shape placed by a function the corpus was not placed by is not a thing a
/// caller can write (R12).
///
/// **A view with `projection = "none"` still refuses `wgs84`**: it has one space, `view` is the
/// right word for it, and there is nothing to convert a degree from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShapeSpace {
    #[default]
    View,
    Wgs84,
}

impl ShapeSpace {
    pub fn parse(word: &str) -> Result<Self, ShapeRefusal> {
        match word {
            "view" => Ok(ShapeSpace::View),
            "wgs84" => Ok(ShapeSpace::Wgs84),
            other => Err(ShapeRefusal(format!(
                "`space = \"{other}\"` is not a space; the values are \"view\" and \"wgs84\""
            ))),
        }
    }

    /// The space with the view's own transform in it — what canonicalisation takes.
    ///
    /// Refuses here rather than at [`ShapeSpace::parse`] because the word is a fact about the
    /// submission and the projection is a fact about the view: which of the two makes the pair
    /// impossible is what the caller needs told.
    pub fn resolve(self, projection: Projection) -> Result<Space, ShapeRefusal> {
        match self {
            ShapeSpace::View => Ok(Space::View),
            ShapeSpace::Wgs84 if projection == Projection::None => Err(ShapeRefusal(
                "`space = \"wgs84\"` is refused on a view whose `projection` is `none`: such a \
                 view has one space and nothing to convert a degree from, so a longitude and a \
                 latitude would be quantised as though they were frame coordinates. Write the \
                 shape in the view's own coordinates with `space = \"view\"`, or declare a \
                 projection on the view (`projections.md` §5.3)"
                    .to_string(),
            )),
            ShapeSpace::Wgs84 => Ok(Space::Wgs84(projection)),
        }
    }
}

/// Why a shape was refused at publication — the caller's own arithmetic, each with a one-line fix
/// (`polygon-membership.md` §6.5): a coordinate that is not one, an inverted box, a non-positive
/// radius or axis, a polygon over the vertex cap, or a kind that is not the layer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeRefusal(pub String);

impl std::fmt::Display for ShapeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ShapeRefusal {}

/// What canonicalising one shape for one view produced, beside the report: the held form's size,
/// which is what §6.5's report needs and nobody can estimate from a vertex count.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShapeStats {
    pub interior_tiles: u64,
    pub boundary_cells: u64,
    pub parts: u64,
    pub rings: u64,
}

/// One shape canonicalised for every view of its layer.
#[derive(Debug, Clone)]
pub struct CanonicalShapes {
    /// `(view, canonical bytes)` — the `ArtifactShapes` a record carries, before the lifecycle
    /// crate wraps them.
    pub by_view: Vec<(String, Vec<u8>)>,
    /// Per view, what canonicalisation did and what the held form costs.
    pub reports: Vec<(String, CanonReport, ShapeStats)>,
    /// The grid-unit bounds per view, for the parent-escape report.
    pub bounds: Vec<(String, Option<tessera_spatial::shape::Bbox>)>,
}

/// Read an **authored content's** text as the shape its declared kind names
/// (`polygon-membership.md` §6.1): a `polygon` is WKT, a `circle` is `cx, cy, r` and an
/// `ellipse` is `cx, cy, a, b, angle` — the numbers of the row fields a membership shape takes,
/// comma- or space-separated, in the submission's space. The value then takes exactly the route a
/// membership shape takes: [`shape_input`], [`canonical_shapes`], the same report and the same
/// vertex cap. A `bbox` is not an authored kind — a supplied box is the `extent` content that
/// already exists.
pub fn authored_shape_input(kind: ShapeKind, text: &str) -> Result<ShapeInput, ShapeRefusal> {
    let numbers = |want: usize, field: &str| -> Result<Vec<f64>, ShapeRefusal> {
        let values: Result<Vec<f64>, _> = text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<f64>())
            .collect();
        let values = values.map_err(|e| {
            ShapeRefusal(format!(
                "the `{field}` content {text:?} is not {want} numbers: {e}"
            ))
        })?;
        if values.len() != want {
            return Err(ShapeRefusal(format!(
                "the `{field}` content has {} value(s); it is exactly {want}",
                values.len()
            )));
        }
        Ok(values)
    };
    Ok(match kind {
        ShapeKind::Polygon => ShapeInput::Wkt(text.to_string()),
        ShapeKind::Circle => {
            let v = numbers(3, "circle")?;
            ShapeInput::Circle([v[0], v[1], v[2]])
        }
        ShapeKind::Ellipse => {
            let v = numbers(5, "ellipse")?;
            ShapeInput::Ellipse([v[0], v[1], v[2], v[3], v[4]])
        }
        ShapeKind::Bbox => {
            return Err(ShapeRefusal(
                "a `bbox` is not an authored content kind; a supplied box is an `extent`"
                    .to_string(),
            ))
        }
    })
}

/// Read a row's shape into the caller's form, checking it against the layer's declared kind.
pub fn shape_input(kind: ShapeKind, input: ShapeInput) -> Result<ShapeF64, ShapeRefusal> {
    if input.kind() != kind {
        return Err(ShapeRefusal(format!(
            "the row carries a {} and the layer's `shape.kind` is \"{}\"; a row's geometry is in \
             its layer's kind's fields and no other",
            input.kind().as_str(),
            kind.as_str()
        )));
    }
    Ok(match input {
        ShapeInput::Bbox([min_x, min_y, max_x, max_y]) => ShapeF64::Bbox {
            min_x,
            min_y,
            max_x,
            max_y,
        },
        ShapeInput::Circle([cx, cy, r]) => ShapeF64::Circle { cx, cy, r },
        ShapeInput::Ellipse([cx, cy, a, b, angle_degrees]) => ShapeF64::Ellipse {
            cx,
            cy,
            a,
            b,
            angle_degrees,
        },
        ShapeInput::Wkb(bytes) => ShapeF64::Polygon(
            read_wkb(&bytes).map_err(|e| ShapeRefusal(format!("the WKB would not read: {e}")))?,
        ),
        ShapeInput::Wkt(text) => ShapeF64::Polygon(
            read_wkt(&text).map_err(|e| ShapeRefusal(format!("the WKT would not read: {e}")))?,
        ),
    })
}

/// One view a shape layer is drawn in, with **the frame that view's own points are quantised in**
/// (decision 0040): the transform that placed them and the extent they were quantised against.
///
/// A layer's views need share neither, and what makes them comparable is that each shape is put
/// through *this* view's pair and no other — the same function that placed the rows it is about to
/// select (`polygon-membership.md` §4.3, R12).
#[derive(Debug, Clone, PartialEq)]
pub struct ViewFrame {
    /// The view id — a plain view's name, or a group's view as `group:key`.
    pub view: String,
    pub projection: Projection,
    pub extent: Bounds,
}

impl ViewFrame {
    pub fn new(view: impl Into<String>, projection: Projection, extent: Bounds) -> Self {
        ViewFrame {
            view: view.into(),
            projection,
            extent,
        }
    }
}

/// The two spans [decision 0111](../../../docs/decisions/0111-a-shape-spans-projected-views-through-wgs84.md)
/// refuses, checked once for a whole layer.
///
/// **A layer's views are all projected or all `none`.** `wgs84` means nothing in an embedding, so
/// no geometry spans the two kinds of space and the mix is refused whatever a row declares — which
/// is why this arm ignores `space` and can be called at the layer's declaration, before any
/// geometry is read.
///
/// **A `view`-space shape spans only identical frames.** Its coordinates are one specific frame's,
/// so over views differing in projection or extent it names different places in each; `wgs84` is
/// the spelling that spans, and this arm therefore depends on what the submission declared. A
/// layer scoped to a group is exempt in fact rather than by rule — a group's views share a frame
/// by construction, so the frames compare equal.
///
/// The caller prefixes the layer's name: every publication route already wraps a
/// [`ShapeRefusal`] in `layer '<name>': …`.
pub fn check_shape_span(views: &[ViewFrame], space: ShapeSpace) -> Result<(), ShapeRefusal> {
    let named = |select: &dyn Fn(&ViewFrame) -> bool| -> String {
        views
            .iter()
            .filter(|v| select(v))
            .map(|v| format!("'{}'", v.view))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let projected = |v: &ViewFrame| v.projection != Projection::None;
    if views.iter().any(projected) && views.iter().any(|v| !projected(v)) {
        return Err(ShapeRefusal(format!(
            "its views are a mix of projected and unprojected row spaces — {} declare a \
             projection and {} declare `projection = \"none\"`. A `wgs84` coordinate means \
             nothing in an embedding, so no geometry spans the two kinds of space (decision 0111); \
             draw the layer on one kind or the other",
            named(&projected),
            named(&|v| !projected(v))
        )));
    }
    if space == ShapeSpace::View {
        if let Some(first) = views.first() {
            if let Some(other) = views
                .iter()
                .find(|v| v.projection != first.projection || v.extent != first.extent)
            {
                return Err(ShapeRefusal(format!(
                    "`space = \"view\"` geometry is written in one view's frame, and views '{}' \
                     and '{}' do not share one — {:?} against {:?}. Declare the geometry \
                     `space = \"wgs84\"`, which is the spelling that spans frames (decision \
                     0111), or draw the layer on views of one group, whose frames are identical by \
                     construction",
                    first.view, other.view, first.extent, other.extent
                )));
            }
        }
    }
    Ok(())
}

/// Canonicalise one shape for every named view against the frame the points are quantised in
/// (`polygon-membership.md` §4.4), and decompose it so the report can say what it will cost to
/// hold.
///
/// **Reported, never refused**, for everything the [`CanonReport`] carries — clipped, outside,
/// rings dropped, degrees-looking — on the rule that bounds warn and never exclude. What refuses is
/// a [`tessera_spatial::shape::CanonError`] (a coordinate that is not one, and a `wgs84` one
/// outside ±180 × ±90 with it) and a polygon over `max_vertices`, naming the count and the cap
/// (ruling (e)).
///
/// **`space` and `projection` are taken together and here**, at the one place every publication
/// route passes through: a `wgs84` shape is densified and put through the view's own transform
/// before it is quantised (`polygon-membership.md` §4.3, R10), and a caller cannot canonicalise
/// one without naming the function that placed the points.
///
/// **Per view, in that view's own frame** ([decision 0111](../../../docs/decisions/0111-a-shape-spans-projected-views-through-wgs84.md)):
/// each [`ViewFrame`] carries the projection that placed its view's points and the extent they are
/// quantised against, and the shape goes through that pair once per view. A layer whose views share
/// a frame — every view of a group, by construction — therefore produces identical bytes under each
/// name and pays only the repeated canonicalisation; a layer spanning frames produces a genuinely
/// different canonical form per view, which is the semantics `polygon-membership.md` §4.3 states.
///
/// Two spans are refused rather than resolved, both by [`check_shape_span`]: a layer mixing a
/// `projection = "none"` view with a projected one, and a `view`-space shape over views whose
/// frames are not identical. The first is a property of the layer and is refused at its
/// declaration too; the second is a property of the submission and can only be known here.
pub fn canonical_shapes(
    shape: &ShapeF64,
    views: &[ViewFrame],
    space: ShapeSpace,
    max_vertices: u64,
) -> Result<CanonicalShapes, ShapeRefusal> {
    check_shape_span(views, space)?;
    let mut out = CanonicalShapes {
        by_view: Vec::with_capacity(views.len()),
        reports: Vec::with_capacity(views.len()),
        bounds: Vec::with_capacity(views.len()),
    };
    for frame in views {
        let view = frame.view.as_str();
        let space = space.resolve(frame.projection)?;
        let (canonical, report) = shape
            .canonical(space, &frame.extent)
            .map_err(|e| ShapeRefusal(e.to_string()))?;
        let vertices = canonical.vertex_count();
        if vertices > max_vertices {
            return Err(ShapeRefusal(format!(
                "the polygon has {vertices} vertices after canonicalisation and this deployment's \
                 `max_shape_vertices` is {max_vertices}; simplify it (`ST_Simplify`) or raise the \
                 cap"
            )));
        }
        let held = HeldShape::new(canonical);
        let (parts, rings) = held.shape.parts_and_rings();
        out.reports.push((
            view.to_string(),
            report,
            ShapeStats {
                interior_tiles: held.interior.len() as u64,
                boundary_cells: held.boundary.len() as u64,
                parts,
                rings,
            },
        ));
        out.bounds.push((view.to_string(), held.bounds));
        out.by_view.push((view.to_string(), held.shape.encode()));
    }
    Ok(out)
}

/// One artifact's shape as a level holds it for the artifact's life (`polygon-membership.md`
/// §6.3): the canonical shape and its decomposition — interior tiles whole, boundary cells as
/// **code and corner parity only**. The per-cell edge lists are the structure that dominated memory
/// at world scale, so they are not held; [`resolve_segment`] re-derives them for the cells a segment
/// puts rows in, once per `(cell, segment)`, through [`contexts_at`].
#[derive(Debug, Clone)]
pub struct HeldShape {
    pub shape: Shape,
    /// Tiles wholly inside, at whatever depth the descent found them.
    pub interior: Vec<Tile>,
    /// Depth-16 cells the boundary crosses, **ascending by code**, each with the parity of its
    /// lower corner.
    pub boundary: Vec<(MortonCode, bool)>,
    /// The grid-unit box enclosing the shape; `None` for a polygon canonicalised to nothing.
    pub bounds: Option<tessera_spatial::shape::Bbox>,
}

impl HeldShape {
    /// Decompose a canonical shape. Never budgeted: a published shape always reaches the grid.
    pub fn new(shape: Shape) -> Self {
        let decomposition = shape.decompose(None);
        debug_assert!(!decomposition.is_cover());
        let mut boundary: Vec<(MortonCode, bool)> = decomposition
            .boundary
            .iter()
            .map(|cell| (cell.cell, cell.ctx.parity))
            .collect();
        boundary.sort_unstable_by_key(|(code, _)| code.raw());
        HeldShape {
            bounds: shape.bounds(),
            interior: decomposition.interior,
            boundary,
            shape,
        }
    }

    /// The held form of a record's stored bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        Shape::decode(bytes).map(HeldShape::new)
    }

    pub fn stats(&self) -> ShapeStats {
        let (parts, rings) = self.shape.parts_and_rings();
        ShapeStats {
            interior_tiles: self.interior.len() as u64,
            boundary_cells: self.boundary.len() as u64,
            parts,
            rings,
        }
    }

    /// Bytes held beyond the shape's own: sixteen per interior tile, five per boundary cell.
    pub fn held_bytes(&self) -> u64 {
        16 * self.interior.len() as u64 + 5 * self.boundary.len() as u64
    }
}

/// The depth of the coarse index a level keeps over its shapes' bounds.
///
/// **Eight**: a depth-8 tile is 1/256 of the extent on each axis — some 150 km across on a
/// world view — so the 378 countries of a world-scale boundary set each meet tens of tiles and
/// the 10⁶ localities one, while the table has 65,536 keys at most and ~10⁶ entries. Deeper
/// would put a country in thousands of tiles for no gain, since the segment side of the lookup
/// is coarsened to the same depth.
pub const SHAPE_INDEX_DEPTH: u32 = 8;

/// Per level, the geometry-derived index from coarse tiles to the artifacts whose bounds meet
/// them (`polygon-membership.md` §6.3). Principal-independent and segment-independent; built with
/// the held shapes and read at every resolution to skip the artifacts a segment cannot touch.
#[derive(Debug, Clone, Default)]
pub struct ShapeIndex {
    by_tile: HashMap<u32, Vec<u32>>,
}

impl ShapeIndex {
    pub fn build(shapes: &[Option<HeldShape>]) -> Self {
        let mut by_tile: HashMap<u32, Vec<u32>> = HashMap::new();
        for (ordinal, held) in shapes.iter().enumerate() {
            let Some(bounds) = held.as_ref().and_then(|h| h.bounds) else {
                continue;
            };
            for tile in coarse_tiles(bounds) {
                by_tile.entry(tile).or_default().push(ordinal as u32);
            }
        }
        ShapeIndex { by_tile }
    }

    /// The ordinals whose bounds meet any of `tiles` (depth-[`SHAPE_INDEX_DEPTH`] prefixes),
    /// ascending and distinct.
    pub fn candidates(&self, tiles: impl IntoIterator<Item = u32>) -> Vec<u32> {
        let mut out: Vec<u32> = tiles
            .into_iter()
            .filter_map(|t| self.by_tile.get(&t))
            .flat_map(|v| v.iter().copied())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn entries(&self) -> u64 {
        self.by_tile.values().map(|v| v.len() as u64).sum()
    }

    /// The depth-[`SHAPE_INDEX_DEPTH`] tiles a segment's rows occupy, from its sorted codes.
    pub fn tiles_of(segment: &SegmentData) -> Vec<u32> {
        let shift = 32 - 2 * SHAPE_INDEX_DEPTH;
        let mut out: Vec<u32> = Vec::new();
        for &code in segment.morton.u32() {
            let tile = code >> shift;
            if out.last() != Some(&tile) {
                out.push(tile);
            }
        }
        out
    }
}

/// The depth-[`SHAPE_INDEX_DEPTH`] tiles a grid-unit box meets.
fn coarse_tiles(b: tessera_spatial::shape::Bbox) -> impl Iterator<Item = u32> {
    let shift = 32 - SHAPE_INDEX_DEPTH;
    let (x0, x1) = (b.min_x >> shift, b.max_x >> shift);
    let (y0, y1) = (b.min_y >> shift, b.max_y >> shift);
    (y0..=y1).flat_map(move |ty| {
        (x0..=x1)
            .map(move |tx| tessera_spatial::interleave_bits(tx, ty, SHAPE_INDEX_DEPTH as u8) as u32)
    })
}

/// What resolving one segment produced: one segment-local row set per ordinal, and the cost.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSegment {
    /// Parallel to the shapes handed in; `None` where there was no shape.
    pub rows: Vec<Option<Bitmap>>,
    /// Rows tested one by one in boundary cells — the per-point work the design prices.
    pub rows_tested: u64,
    /// Rows admitted whole from interior tiles.
    pub rows_interior: u64,
    /// Artifacts the coarse index let the segment skip.
    pub artifacts_skipped: u64,
    /// Artifacts holding a shape and **no row of this segment** — skipped by the index or tested
    /// and found to contain nothing. Reported beside the artifact count so the two counts a build
    /// prints (artifacts with a shape, artifacts with rows) are both stated and their difference
    /// is a number rather than a discrepancy.
    pub artifacts_empty: u64,
}

/// Resolve every row of `segment` against a level's held shapes (`polygon-membership.md` §6.3).
///
/// Interior tiles are whole row ranges through [`tile_ranges_all`] — the same call a viewport
/// resolves its own tiles through, which is what makes a membership and a viewport that overlap on
/// the map overlap here. The rows in boundary cells are tested one by one against the shape over
/// the exact position `unsplit32(cell, residual)` recovers — four bytes read per row — with the
/// cell's edge list derived once per `(cell, segment)`. Row ids are **segment-local**; the caller
/// applies the row base.
///
/// Deterministic and principal-free: a function of the shapes and the segment alone, which is why
/// its output may be held as the membership rather than treated as a served quantity (§10).
pub fn resolve_segment(
    segment: &SegmentData,
    shapes: &[Option<HeldShape>],
    index: &ShapeIndex,
) -> ResolvedSegment {
    let mut out = ResolvedSegment {
        rows: vec![None; shapes.len()],
        ..Default::default()
    };
    if segment.row_count == 0 {
        for (ordinal, held) in shapes.iter().enumerate() {
            if held.is_some() {
                out.rows[ordinal] = Some(Bitmap::new());
            }
        }
        return out;
    }
    let candidates = index.candidates(ShapeIndex::tiles_of(segment));
    let residual = segment.columns.residual();
    let mut next = candidates.iter().copied().peekable();
    for (ordinal, held) in shapes.iter().enumerate() {
        let Some(held) = held else {
            continue;
        };
        let is_candidate = next.peek() == Some(&(ordinal as u32));
        if is_candidate {
            next.next();
        } else {
            out.artifacts_skipped += 1;
            out.rows[ordinal] = Some(Bitmap::new());
            continue;
        }
        let mut rows = Bitmap::new();
        for span in tile_ranges_all(segment, &held.interior) {
            if !span.is_empty() {
                out.rows_interior += u64::from(span.end - span.start);
                rows.add_range(span);
            }
        }
        let cells: Vec<Tile> = held
            .boundary
            .iter()
            .map(|(code, _)| Tile {
                prefix: u64::from(code.raw()),
                depth: 16,
            })
            .collect();
        let touched: Vec<(usize, Range<u32>)> = tile_ranges_all(segment, &cells)
            .into_iter()
            .enumerate()
            .filter(|(_, span)| !span.is_empty())
            .collect();
        if !touched.is_empty() {
            let prepared = held.shape.prepared();
            let codes: Vec<MortonCode> = touched.iter().map(|(i, _)| held.boundary[*i].0).collect();
            // The edges of each touched cell, re-derived once for this segment; a closed form
            // carries none and tests by its own predicate.
            let contexts: Vec<PolyCtx> = match prepared.region() {
                Some(region) => contexts_at(region, &codes),
                None => vec![PolyCtx::default(); codes.len()],
            };
            for ((i, span), ctx) in touched.iter().zip(&contexts) {
                let (code, parity) = held.boundary[*i];
                debug_assert!(prepared.region().is_none() || ctx.parity == parity);
                let rect = Rect::of_cell(code);
                for row in span.clone() {
                    let position = unsplit32(code, residual[row as usize]);
                    out.rows_tested += 1;
                    if prepared.contains_in_cell(position, rect, ctx) {
                        rows.add(row);
                    }
                }
            }
        }
        rows.run_optimize();
        out.rows[ordinal] = Some(rows);
    }
    out.artifacts_empty = out
        .rows
        .iter()
        .filter(|rows| rows.as_ref().is_some_and(Bitmap::is_empty))
        .count() as u64;
    out
}

#[cfg(test)]
mod derived_tests {
    use super::*;

    #[allow(clippy::type_complexity)]
    fn walk_of(sets: &[Vec<u32>]) -> impl Fn(&mut dyn FnMut(u32, &Bitmap)) + '_ {
        move |visit: &mut dyn FnMut(u32, &Bitmap)| {
            for (ordinal, set) in sets.iter().enumerate() {
                let bitmap: Bitmap = set.iter().copied().collect();
                visit(ordinal as u32, &bitmap);
            }
        }
    }

    /// **The level set is corpus-relative and the floor is not**, and the coarsest shift is the one
    /// the `everywhere` test is taken at.
    #[test]
    fn the_hierarchy_has_a_level_for_every_zoom_and_sixteen_roots_at_most() {
        assert_eq!(tile_index_shifts(0), vec![10]);
        assert_eq!(tile_index_shifts(500), vec![10]);
        assert_eq!(tile_index_shifts(100_000_000), vec![24, 20, 16, 12, 10]);
        assert_eq!(
            tile_index_shifts(1_000_000_000),
            vec![28, 24, 20, 16, 12, 10]
        );
        for rows in [1_000u32, 1_000_000, 100_000_000, u32::MAX] {
            let top = coarsest_shift(rows);
            assert!(
                (rows as u64) >> top < (1 << LEVEL_STEP),
                "{rows} rows would give more than a fan-out of roots"
            );
        }
    }

    /// The trigger's own arithmetic: a level whose artifacts are clumped has no `everywhere` set at
    /// all, and one whose artifacts span the map is entirely `everywhere`.
    #[test]
    fn the_everywhere_fraction_separates_a_clumped_level_from_a_spread_one() {
        let clumped: Vec<Vec<u32>> = (0..100u32).map(|i| vec![i * 10, i * 10 + 5]).collect();
        let shape = observe_shape(1_000_000, &walk_of(&clumped));
        assert_eq!(shape.artifacts, 100);
        assert_eq!(shape.everywhere_fraction, 0.0);

        let spread: Vec<Vec<u32>> = (0..100u32).map(|i| vec![i, 999_999 - i]).collect();
        let shape = observe_shape(1_000_000, &walk_of(&spread));
        assert_eq!(shape.artifacts, 100);
        assert_eq!(shape.everywhere_fraction, 1.0);
    }

    /// Holes and empty projections are not artifacts, and neither counts towards any figure.
    #[test]
    fn an_empty_projection_is_not_an_artifact() {
        let sets = vec![vec![1u32, 2], vec![], vec![7]];
        let shape = observe_shape(4_096, &walk_of(&sets));
        assert_eq!(shape.artifacts, 2);
        assert!(shape.partitions);
        assert_eq!(observe_shape(4_096, &walk_of(&[])), LevelShape::empty());
    }

    /// The persisted row form round-trips holes, empties and rows, and refuses every key that is
    /// not the one it was written under — the I11 half of persisting a resolution.
    #[test]
    fn a_shape_row_form_round_trips_and_refuses_another_key() {
        let rows: Vec<Option<Bitmap>> = vec![
            Some([1u32, 2, 3, 900].into_iter().collect()),
            None,
            Some(Bitmap::new()),
            Some((10..500u32).collect()),
        ];
        let bytes = shape_rows_bytes(7, "seg-0", 1_000, &rows);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("piece.tssr");
        std::fs::write(&path, &bytes).unwrap();

        let back = read_shape_rows(&path, 7, "seg-0", 1_000).expect("the form reads back");
        assert_eq!(back.len(), 4);
        assert_eq!(
            back[0].as_ref().map(|b| b.to_vec()),
            Some(vec![1, 2, 3, 900])
        );
        assert!(
            back[1].is_none(),
            "a hole is a hole, not an empty membership"
        );
        assert_eq!(back[2].as_ref().map(Bitmap::cardinality), Some(0));
        assert_eq!(back[3].as_ref().map(Bitmap::cardinality), Some(490));

        assert!(
            read_shape_rows(&path, 8, "seg-0", 1_000).is_err(),
            "another level version"
        );
        assert!(
            read_shape_rows(&path, 7, "seg-1", 1_000).is_err(),
            "another segment"
        );
        assert!(
            read_shape_rows(&path, 7, "seg-0", 2_000).is_err(),
            "another row count"
        );

        // A row at or past the segment's count is a form written over other rows.
        let wide = shape_rows_bytes(7, "seg-0", 100, &rows);
        std::fs::write(&path, &wide).unwrap();
        assert!(read_shape_rows(&path, 7, "seg-0", 100).is_err());

        // Truncated, and with trailing bytes: refused rather than read short or long.
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        assert!(read_shape_rows(&path, 7, "seg-0", 1_000).is_err());
        let mut long = bytes.clone();
        long.push(0);
        std::fs::write(&path, &long).unwrap();
        assert!(read_shape_rows(&path, 7, "seg-0", 1_000).is_err());
    }

    /// A persisted decomposition reads back as the one the descent produced, and is refused under
    /// another level version, for other canonical bytes, and when torn.
    #[test]
    fn a_persisted_decomposition_round_trips_and_refuses_another_shape() {
        let extent = Bounds {
            x_min: 0.0,
            x_max: 1000.0,
            y_min: 0.0,
            y_max: 1000.0,
        };
        let polygon = ShapeF64::Polygon(
            read_wkt("POLYGON ((100 100, 900 150, 850 900, 120 800, 100 100))").unwrap(),
        );
        let canonical = polygon.canonical(Space::View, &extent).unwrap().0;
        let bytes = canonical.encode();
        let held = HeldShape::new(canonical);
        assert!(!held.interior.is_empty() && !held.boundary.is_empty());
        let file = shape_held_bytes(3, &[(Some(&bytes), Some(&held)), (None, None)]);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("held.tssh");
        std::fs::write(&path, &file).unwrap();

        let back = read_shape_held(&path, 3).expect("reads back");
        assert_eq!(back.len(), 2);
        assert!(back[1].is_none());
        let entry = back.into_iter().next().unwrap().unwrap();
        assert!(entry.is_of(&bytes));
        assert!(
            !entry.is_of(&bytes[..bytes.len() - 1]),
            "other bytes are another shape"
        );
        assert_eq!(entry.interior, held.interior);
        assert_eq!(entry.boundary, held.boundary);
        assert_eq!(entry.bounds, held.bounds);

        assert!(read_shape_held(&path, 4).is_err(), "another level version");
        std::fs::write(&path, &file[..file.len() - 2]).unwrap();
        assert!(read_shape_held(&path, 3).is_err(), "torn");
    }

    /// Disjointness is observed, not declared.
    #[test]
    fn overlapping_memberships_do_not_partition() {
        assert!(!observe_shape(4_096, &walk_of(&[vec![1, 2], vec![2, 3]])).partitions);
        assert!(observe_shape(4_096, &walk_of(&[vec![1, 2], vec![3, 4]])).partitions);
    }

    /// **A view with no rows composes an empty column of either form, and leaves no scratch.** The
    /// degenerate end of the partition's arithmetic: the boundaries collapse to one bucket, no pair
    /// is pushed because every row is past the column, and the file is a header.
    #[test]
    fn a_view_of_no_rows_composes_an_empty_column_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
            let path =
                project_row_column(2, 0, layout, dir.path(), &walk_of(&[vec![0, 1], vec![2]]))
                    .expect("a column over no rows")
                    .expect("a column, not a refusal");
            match layout {
                ServingLayout::RowMajorLabel => {
                    let pack =
                        crate::membership::LabelColumnPack::open(&path).expect("the column frames");
                    assert_eq!(pack.rows(), 0);
                }
                _ => {
                    let pack =
                        crate::membership::ListColumnPack::open(&path).expect("the column frames");
                    assert_eq!(pack.rows(), 0);
                }
            }
            std::fs::remove_file(&path).expect("the column is the caller's to remove");
        }
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .expect("the scratch directory")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            left.is_empty(),
            "the buckets went back with the store: {left:?}"
        );
    }

    /// **A level whose entries outrun the list form's `u32` has no list form.** The header states
    /// the total as a `u32` and the offsets index it, so the entries above 2³² − 1 would be written
    /// at wrapped offsets and read back as another row's. The composition declines and the level is
    /// served artifact-major, which is the same answer a level whose memberships do not partition
    /// gets and is unchanged in what any viewer is told.
    ///
    /// The boundary and not the path: pushing 2³² records through the partition is not a fixture.
    #[test]
    fn a_level_of_more_entries_than_the_list_forms_offsets_count_has_no_list_form() {
        assert_eq!(super::list_form_entries(0), Some(0));
        assert_eq!(
            super::list_form_entries(u64::from(u32::MAX)),
            Some(u32::MAX)
        );
        assert_eq!(super::list_form_entries(u64::from(u32::MAX) + 1), None);
        // A layer of 47 entries a row over the rung-6 corpus, which is the case that reaches it.
        assert_eq!(super::list_form_entries(47 * 3_495_729_729), None);
    }

    /// The covering a test expects, written the long way: every gap between consecutive members,
    /// the `n − 1` widest kept (the lower on a tie), and the span cut at them.
    pub(crate) fn widest_gap_covering(rows: &[u32], n: usize) -> Vec<(u32, u32)> {
        let (Some(&first), Some(&last)) = (rows.first(), rows.last()) else {
            return Vec::new();
        };
        let mut gaps: Vec<(u32, u32)> = rows
            .windows(2)
            .filter(|w| w[1] > w[0] + 1)
            .map(|w| (w[1] - w[0] - 1, w[0] + 1))
            .collect();
        gaps.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        gaps.truncate(n - 1);
        gaps.sort_by_key(|g| g.1);
        let mut out = Vec::new();
        let mut lo = first;
        for (length, start) in gaps {
            out.push((lo, start - 1));
            lo = start + length;
        }
        out.push((lo, last));
        out
    }

    /// Every artifact's members and covering, against the column's labels transposed by hand.
    fn assert_members_match(pack: &crate::row_members::RowMembersPack, sets: &[Vec<u32>]) {
        assert_eq!(pack.ordinals() as usize, sets.len());
        for (ordinal, set) in sets.iter().enumerate() {
            let ordinal = ordinal as u32;
            let mut rows = set.clone();
            rows.sort_unstable();
            rows.dedup();
            let held: Vec<u32> = pack
                .members(ordinal)
                .map(|view| view.iter().collect())
                .unwrap_or_default();
            assert_eq!(held, rows, "ordinal {ordinal}'s members");
            assert_eq!(pack.member_count(ordinal) as usize, rows.len());
            assert_eq!(
                pack.covering(ordinal).collect::<Vec<_>>(),
                widest_gap_covering(&rows, COVERING_RANGES),
                "ordinal {ordinal}'s covering"
            );
        }
    }

    /// **Both column forms give the same members and coverings as the labels transposed by hand**:
    /// a level of one artifact (alone in its bucket), artifacts with more gaps than a covering has
    /// ranges, a hole, an empty artifact, and rows past one Roaring container.
    #[test]
    fn the_members_and_coverings_are_the_columns_labels_transposed() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let rows = 300_000u32;
        let scattered: Vec<u32> = (0..200u32).map(|i| i * i * 7 % rows).collect();
        let mut scattered_sorted = scattered.clone();
        scattered_sorted.sort_unstable();
        scattered_sorted.dedup();
        let dense: Vec<u32> = (0..rows)
            .filter(|r| r % 3 == 1 && !scattered_sorted.contains(r))
            .collect();
        let cases: Vec<Vec<Vec<u32>>> = vec![
            vec![(10..70_000).step_by(5).collect()],
            vec![
                scattered_sorted.clone(),
                Vec::new(),
                dense.clone(),
                vec![rows - 1],
            ],
        ];
        for sets in &cases {
            for layout in [ServingLayout::RowMajorLabel, ServingLayout::RowMajorList] {
                let column =
                    project_row_column(sets.len() as u32, rows, layout, dir.path(), &walk_of(sets))
                        .expect("a column")
                        .expect("the sets partition");
                let members = stage_row_members(&column, layout, dir.path()).expect("the members");
                let pack = crate::row_members::RowMembersPack::open(&members).expect("they open");
                assert_members_match(&pack, sets);
                assert_eq!(pack.rows(), rows);
                std::fs::remove_file(&column).unwrap();
                std::fs::remove_file(&members).unwrap();
            }
        }
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert!(
            left.is_empty(),
            "the writer leaves nothing behind: {left:?}"
        );
    }

    /// A list column's row may carry several artifacts, and each of them has the row.
    #[test]
    fn an_overlapping_level_gives_each_artifact_its_shared_rows() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let sets = vec![vec![0, 1, 2, 90], vec![1, 2, 3], vec![]];
        let column = project_row_column(
            3,
            100,
            ServingLayout::RowMajorList,
            dir.path(),
            &walk_of(&sets),
        )
        .unwrap()
        .unwrap();
        let members = stage_row_members(&column, ServingLayout::RowMajorList, dir.path()).unwrap();
        let pack = crate::row_members::RowMembersPack::open(&members).unwrap();
        assert_members_match(&pack, &sets);
    }
}
