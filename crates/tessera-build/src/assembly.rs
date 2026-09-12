//! One view's row space, assembled from the ordinal-order files through a Morton partition.
//!
//! **The stage that could not complete at rung 6 under any budget.** What it replaced collected a
//! 12 B record for every row of the view into one heap vector — 42 GB over 3.5×10⁹ rows — sorted
//! it there, and then built the row→entity, residual and `tessera_id` columns as three more
//! vectors beside it, a 70 GB peak against a residency model with no term for any of them. Before
//! that it scattered the view's geometry into two entity-order files nothing else reads, which is
//! the write-back-and-re-dirty pattern of the whole-corpus observations
//! (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md` §§1, 4).
//!
//! The shape here is the one rule of
//! `docs/evidence/memos/2026-09-12-bounded-assembly-design.md` §4.1: nothing sized by the row
//! count is anonymous, and no mapped file is written at a scattered index.
//!
//! 1. **Histogram.** One sequential walk over the view's ordinal geometry counts `morton >> 8`
//!    and fixes the Morton boundaries ([`MortonBoundaries`]). The same walk learns which pages of
//!    entity space the view occupies, which is what the permutation is laid out from.
//! 2. **Row partition.** The walk again, pushing a 12 B `(morton, residual, entity)` record per
//!    present ordinal into the Morton buckets. `priority` is not carried: it is `forward(entity)`'s
//!    prefix and is recomputed when a bucket is loaded.
//! 3. **Per bucket, in Morton order.** Load, recompute each record's `priority`, sort by
//!    `(morton, tessera_id)`, and emit: `morton.u32` and `row-entity.u32` are appended, the
//!    identity and the residual go into `columns.arrow`'s body at their own offsets, the occupancy
//!    counter is fed the codes, and `(entity, row)` is pushed to a second partition.
//! 4. **The permutation**, one entity-range bucket at a time.
//! 5. **The render tail.** Each entity bucket reads its render column sequentially and pushes
//!    `(row, value)` to a row partition; each row bucket writes its window into that column's
//!    buffer. Two 8 B-a-row partitions for a column of any width, the second growing as the first
//!    is consumed.
//!
//! **What is held.** One bucket and one window per partition in flight, the Morton histogram
//! (a constant 134 MB, the `morton >> 8` space), the page plan (one `bool` per 65,536 entities),
//! and each render column's presence bitmap. Nothing sized by the row count.

use std::io::Write;
use std::path::{Path, PathBuf};

use tessera_spatial::split32;
use tessera_spatial::tiler::ScalarType;
use tessera_store::columns::{ColumnsFile, ColumnsPlan};
use tessera_store::write::{PagePlan, PermutationWriter};
use tessera_types::{EntityId, IdentityKey};

use crate::column::EntityColumn;
use crate::error::{BuildError, Result};
use crate::spill::{self, Partition};
use crate::Occupancy;

/// The most bins the Morton histogram has: the design's `morton >> 8`, the code's top 24 bits, so
/// a bin is the sum of up to 256 adjacent cells. As `u64` counts that is 134 MB — a constant of
/// the code type, not of the corpus, and the reason the counting pass can be a plain array rather
/// than a map.
///
/// **A smaller corpus gets fewer bins** ([`MortonHistogram::for_rows`]): 134 MB to count 4,000
/// rows is a pre-flight refusal on a fixture, and a coarser bin decides nothing the refinement
/// does not then decide properly.
const MORTON_BINS_MAX: u64 = 1 << 24;

/// A row bucket's boundaries are multiples of this, so that a `bool` render column's bit-packed
/// window starts on a byte boundary and no bucket has to read-modify-write a neighbour's byte.
const ROW_ALIGN: u64 = 64;

/// Rows of `columns.arrow` filled per positional write. Bounds the transient the emission holds
/// beside the bucket it is walking; nothing about the file depends on it.
const COLUMN_CHUNK_ROWS: usize = 1 << 16;

/// One row of the view while its bucket is being ordered — `morton` and the `tessera_id` prefix
/// are the sort key, `entity` recomputes the full identity on a prefix tie (contracts §2.6), and
/// `residual` is carried from the partition so that no later pass reads geometry at a scattered
/// index.
///
/// **This lives for one bucket, not for the corpus.** The vector is at most the partition's
/// target — `n / 128` rows — where the record it replaced was one per row of the view.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct RowRec {
    pub(crate) morton: u32,
    pub(crate) entity: u32,
    pub(crate) residual: u32,
    pub(crate) priority: u16,
    pub(crate) _pad: u16,
}

impl RowRec {
    /// `(morton, tessera_id)` ascending, with no further tiebreak (contracts §2.6 r6) —
    /// `priority` is compared first as a cheap, physically contiguous prefix, and the full
    /// identity is recomputed from `entity` only on a prefix tie. `forward` is a pure function, so
    /// this is exact: the tie path costs eight `splitmix64` rounds, not an approximation of the
    /// order.
    ///
    /// Comparing `priority` first and refining on a tie is **identical** to comparing the full
    /// `tessera_id` at every row — it is not merely "usually agrees" — because `priority` is
    /// defined as `tessera_id`'s leading 16 bits (`TesseraId::priority`), so two rows can only
    /// disagree in `priority` if they already disagree in `tessera_id`.
    /// `row_rec_comparator_agrees_with_a_full_tessera_id_sort_over_engineered_ties` checks this
    /// against a naive full-`tessera_id` sort over a batch engineered to contain prefix ties.
    pub(crate) fn cmp(&self, other: &Self, key: &IdentityKey, shard: u32) -> std::cmp::Ordering {
        self.morton.cmp(&other.morton).then_with(|| {
            self.priority.cmp(&other.priority).then_with(|| {
                // Unreachable in practice (the allocator cap makes `forward` infallible for any
                // entity a build ever assigns), but `expect` rather than `unwrap_or` — a
                // silently wrong tiebreak here is a silently wrong row order, and that must be
                // loud if it is ever reached.
                let a = key
                    .forward(shard, EntityId::new(self.entity as u64))
                    .expect("entity ids are capped below u32::MAX by the allocator (I-1)");
                let b = key
                    .forward(shard, EntityId::new(other.entity as u64))
                    .expect("entity ids are capped below u32::MAX by the allocator (I-1)");
                a.cmp(&b)
            })
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The Morton boundaries
// ---------------------------------------------------------------------------------------------

/// Where one Morton bucket ends and the next begins.
///
/// **A composite key, because a Morton code is not uniform and a hot cell is not rare.** The first
/// pass counts `morton >> 8` and takes the smallest prefixes whose counts fit the target; a bin
/// over the target on its own is counted again over its 256 full codes; and a single code still
/// over the target is split by the `priority` prefix of the identity, which is sound because the
/// row order is `(morton, tessera_id)` and `priority` is that identity's leading 16 bits. So a
/// boundary is a `(morton, priority)` pair, with `priority` zero everywhere no code needed
/// splitting — which is every bucket of every corpus measured so far.
#[derive(Debug)]
pub(crate) struct MortonBoundaries {
    /// The first `(morton, priority)` of each bucket, ascending, starting at `(0, 0)`.
    firsts: Vec<(u32, u16)>,
    /// The codes a `priority` split reaches into, ascending. A record whose code is not one of
    /// these routes on `(morton, 0)` and never pays for `forward`.
    split_codes: Vec<u32>,
    /// The largest bucket the boundaries admit, for the stage's line.
    largest: u64,
}

impl MortonBoundaries {
    pub(crate) fn buckets(&self) -> usize {
        self.firsts.len()
    }

    /// The bucket a row belongs to. `priority_of` is called only where the row's code is one a
    /// split reaches into, so the ordinary row costs a binary search and nothing else.
    pub(crate) fn route(&self, morton: u32, priority_of: impl FnOnce() -> u16) -> usize {
        let priority = if self.split_codes.binary_search(&morton).is_ok() {
            priority_of()
        } else {
            0
        };
        self.firsts
            .partition_point(|&first| first <= (morton, priority))
            .max(1)
            - 1
    }

    /// The largest bucket the boundaries admit, for the plan to print.
    pub(crate) fn largest(&self) -> u64 {
        self.largest
    }
}

/// What one counting pass over the view's geometry yields. Separate from the boundaries so the
/// refinement passes can be driven by the caller, which is what holds the geometry.
pub(crate) struct MortonHistogram {
    /// How far a code is shifted to reach its bin. Eight at rung scale, more on a corpus with
    /// fewer rows than bins.
    shift: u32,
    bins: Vec<u64>,
    rows: u64,
}

impl MortonHistogram {
    /// Bins enough that the count is not itself the thing deciding the boundaries — a few per row,
    /// capped at the design's `morton >> 8` and floored at 256 so the array is never a special
    /// case.
    ///
    /// **The bin width decides nothing.** A bin over the target is counted again over the full
    /// codes inside it, so a coarse bin costs a refinement pass and not a worse boundary; what the
    /// width buys is how often that second pass runs.
    pub(crate) fn for_rows(rows: u64) -> MortonHistogram {
        let bins = rows
            .saturating_mul(4)
            .next_power_of_two()
            .clamp(256, MORTON_BINS_MAX);
        MortonHistogram {
            shift: 32 - bins.trailing_zeros(),
            bins: vec![0u64; bins as usize],
            rows: 0,
        }
    }

    /// What [`Self::for_rows`] will allocate, for the residency model to charge the same number.
    pub(crate) fn bytes_for_rows(rows: u64) -> u64 {
        rows.saturating_mul(4)
            .next_power_of_two()
            .clamp(256, MORTON_BINS_MAX)
            * 8
    }

    pub(crate) fn count(&mut self, morton: u32) {
        self.bins[(morton >> self.shift) as usize] += 1;
        self.rows += 1;
    }

    /// The first code of bin `bin`.
    fn first_code(&self, bin: usize) -> u32 {
        (bin as u32) << self.shift
    }

    pub(crate) fn rows(&self) -> u64 {
        self.rows
    }

    /// The bins that carry more rows than one bucket should, ascending. Empty for every corpus
    /// whose hottest 256-cell neighbourhood holds fewer than `n / 128` points, which is every
    /// corpus measured.
    pub(crate) fn hot_bins(&self, target: u64) -> Vec<u32> {
        self.bins
            .iter()
            .enumerate()
            .filter(|(_, &count)| count > target)
            .map(|(bin, _)| bin as u32)
            .collect()
    }

    /// The target a bucket is sized against: `rows / 128`, never zero.
    pub(crate) fn target(&self) -> u64 {
        (self.rows / spill::PARTITION_BUCKETS as u64).max(1)
    }
}

/// The counts a refinement pass returns: `code → rows` for the hot bins' full codes, and
/// `(code, priority) → rows` for the hot codes beneath them.
#[derive(Default)]
pub(crate) struct MortonRefinement {
    /// One entry per hot bin, in the order [`MortonHistogram::hot_bins`] gave them: the full codes
    /// inside it with their counts, ascending. A map rather than an array over the bin's span,
    /// because a bin is 256 codes wide only at the corpus scale where the histogram has all its
    /// bins — below that it is wider, and what fills it is at most the rows it counted.
    pub(crate) codes: Vec<std::collections::BTreeMap<u32, u64>>,
    /// One entry per hot code: the code, and 65,536 counts, one per `priority`.
    pub(crate) priorities: Vec<(u32, Vec<u64>)>,
}

/// Fix the boundaries from the histogram and whatever refinement the caller ran for it.
///
/// The three levels are offered to [`spill::boundaries_from_histogram`] as one ascending stream of
/// `(key, count)`: a bin that is not hot, the full codes of one that is, and the `priority` values
/// of a code that is still hot beneath that. One greedy fill decides every boundary.
///
/// Refuses where a single `(morton, priority)` pair carries more rows than a bucket should: that
/// is 33.6×10⁶ points at one cell sharing sixteen bits of a blinded identity, which no corpus has,
/// and there is no finer key to split it by — the row order says nothing about how two rows with
/// the same code and the same identity prefix are placed relative to a third.
pub(crate) fn morton_boundaries(
    histogram: &MortonHistogram,
    refinement: &MortonRefinement,
) -> Result<MortonBoundaries> {
    let target = histogram.target();
    let hot = histogram.hot_bins(target);
    let mut split_codes: Vec<u32> = Vec::new();
    let mut refused: Option<BuildError> = None;
    let mut keys: Vec<((u32, u16), u64)> = Vec::new();
    let mut hot_at = 0usize;
    for (bin, &count) in histogram.bins.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let bin_first = histogram.first_code(bin);
        if hot_at >= hot.len() || hot[hot_at] != bin as u32 {
            keys.push(((bin_first, 0), count));
            continue;
        }
        let codes = &refinement.codes[hot_at];
        hot_at += 1;
        for (&code, &code_count) in codes.iter() {
            if code_count == 0 {
                continue;
            }
            if code_count <= target {
                keys.push(((code, 0), code_count));
                continue;
            }
            split_codes.push(code);
            let Some((_, counts)) = refinement
                .priorities
                .iter()
                .find(|(hot_code, _)| *hot_code == code)
            else {
                refused.get_or_insert_with(|| {
                    BuildError::Invalid(format!(
                        "the segment assembly's Morton boundaries: cell {code} carries \
                         {code_count} rows against a bucket target of {target} and was not \
                         counted by priority"
                    ))
                });
                continue;
            };
            for (priority, &priority_count) in counts.iter().enumerate() {
                if priority_count == 0 {
                    continue;
                }
                if priority_count > target {
                    refused.get_or_insert_with(|| {
                        BuildError::Invalid(format!(
                            "the segment assembly's Morton boundaries: {priority_count} rows \
                             share cell {code} and identity prefix {priority}, against a bucket \
                             target of {target} over {} rows. There is no finer key to split them \
                             by — the row order `(morton, tessera_id)` places two such rows only \
                             by their full identities, which are not a range",
                            histogram.rows
                        ))
                    });
                    continue;
                }
                keys.push(((code, priority as u16), priority_count));
            }
        }
    }
    if let Some(refused) = refused {
        return Err(refused);
    }
    let (firsts, largest) = spill::boundaries_from_histogram((0, 0), keys, target);
    Ok(MortonBoundaries {
        firsts,
        split_codes,
        largest,
    })
}

// ---------------------------------------------------------------------------------------------
// The assembly
// ---------------------------------------------------------------------------------------------

/// One render column, as the assembly reads it: the name and type that name its slot in
/// `columns.arrow`, and the entity-order values it is gathered from.
pub(crate) struct RenderColumn<'a> {
    pub(crate) name: String,
    pub(crate) ty: ScalarType,
    pub(crate) values: &'a EntityColumn,
}

/// The view's geometry in **ordinal** order — what steps 1 and 2 read, and nothing after them.
///
/// Separate from [`Assembly`] so the caller can unlink these two files the moment the row
/// partition is finished, which is 28 GB back at rung 6 and the reason the stage's transient does
/// not rise by the partition's whole size (design §4.1).
pub(crate) struct RowSource<'a> {
    pub(crate) view_dir: &'a Path,
    /// Names this view's partition files apart from the next view's, which share the one spill
    /// directory.
    pub(crate) view: &'a str,
    pub(crate) tmp: &'a Path,
    pub(crate) x: &'a [u32],
    pub(crate) y: &'a [u32],
    /// Which ordinals this view holds a row for.
    pub(crate) present: &'a [u64],
    pub(crate) entity_of_ordinal: &'a [u32],
    /// Entity space's bound — the permutation's, not the view's population.
    pub(crate) n: u64,
    pub(crate) identity_key: &'a IdentityKey,
    pub(crate) shard_id: u32,
    /// What the view's points file said its row count is — the histogram is sized against it, and
    /// the walk's own count is checked against it by the caller.
    pub(crate) rows_hint: u64,
}

/// What the segment write is given, once the geometry has gone.
pub(crate) struct Assembly<'a> {
    pub(crate) view_dir: &'a Path,
    pub(crate) view: &'a str,
    pub(crate) segment_dir: &'a Path,
    pub(crate) tmp: &'a Path,
    pub(crate) n: u64,
    pub(crate) identity_key: &'a IdentityKey,
    pub(crate) shard_id: u32,
    pub(crate) render: Vec<RenderColumn<'a>>,
}

/// What it produced.
pub(crate) struct Assembled {
    pub(crate) rows_in_view: u32,
    pub(crate) occupancy: Occupancy,
    pub(crate) morton_path: PathBuf,
    pub(crate) row_entity_path: PathBuf,
    pub(crate) columns_path: PathBuf,
    pub(crate) permutation_path: PathBuf,
    pub(crate) presence_paths: Vec<PathBuf>,
    /// The largest Morton bucket the boundaries admitted, for the stage's line.
    pub(crate) largest_bucket: u64,
}

/// Bit `i` of a packed bitset.
fn bit_get(bits: &[u64], i: usize) -> bool {
    bits[i / 64] & (1u64 << (i % 64)) != 0
}

/// Walk the view's present ordinals, handing each one its Morton code, residual and entity.
fn walk(job: &RowSource<'_>, mut each: impl FnMut(u32, u32, u32)) {
    for (ordinal, &entity) in job.entity_of_ordinal.iter().enumerate() {
        if !bit_get(job.present, ordinal) {
            continue;
        }
        let (code, residual) = split32(job.x[ordinal], job.y[ordinal]);
        each(code.raw(), residual, entity);
    }
}

/// Steps 1 and 2: the histogram, the boundaries, the page plan, and the row partition.
///
/// Returns before the sort so the caller can close the tiler's stage timing where it always did.
pub(crate) fn partition_rows(
    job: &RowSource<'_>,
) -> Result<(PartitionedRows, MortonBoundaries, PagePlan)> {
    let mut histogram = MortonHistogram::for_rows(job.rows_hint);
    let mut plan = PagePlan::new(job.n).map_err(|e| BuildError::io(job.view_dir, e))?;
    // The page plan rides the same walk: the permutation is laid out from which pages of entity
    // space the view occupies, and that is known here (design §4.1 step 4).
    let mut planned: Result<()> = Ok(());
    walk(job, |code, _, entity| {
        histogram.count(code);
        if planned.is_ok() {
            planned = plan
                .insert(EntityId::new(entity as u64))
                .map_err(|e| BuildError::io(job.view_dir, e));
        }
    });
    planned?;

    let target = histogram.target();
    let hot = histogram.hot_bins(target);
    let mut refinement = MortonRefinement::default();
    if !hot.is_empty() {
        refinement.codes = vec![std::collections::BTreeMap::new(); hot.len()];
        let shift = histogram.shift;
        walk(job, |code, _, _| {
            if let Ok(at) = hot.binary_search(&(code >> shift)) {
                *refinement.codes[at].entry(code).or_insert(0) += 1;
            }
        });
        let hot_codes: Vec<u32> = refinement
            .codes
            .iter()
            .flat_map(|codes| {
                codes
                    .iter()
                    .filter(|(_, &count)| count > target)
                    .map(|(&code, _)| code)
                    .collect::<Vec<_>>()
            })
            .collect();
        if !hot_codes.is_empty() {
            let mut counts: Vec<(u32, Vec<u64>)> = hot_codes
                .iter()
                .map(|&code| (code, vec![0u64; 1 << 16]))
                .collect();
            let key = job.identity_key;
            let shard = job.shard_id;
            walk(job, |code, _, entity| {
                if let Ok(at) = hot_codes.binary_search(&code) {
                    let priority = priority_of(key, shard, entity);
                    counts[at].1[priority as usize] += 1;
                }
            });
            refinement.priorities = counts;
        }
    }
    let boundaries = morton_boundaries(&histogram, &refinement)?;

    let mut partition = Partition::create_routed(
        job.tmp,
        &format!("assembly-{}-rows", job.view),
        boundaries.buckets(),
        ROW_RECORD_BYTES,
        histogram.rows(),
    )?;
    let key = job.identity_key;
    let shard = job.shard_id;
    let mut pushed: Result<()> = Ok(());
    walk(job, |code, residual, entity| {
        if pushed.is_err() {
            return;
        }
        let bucket = boundaries.route(code, || priority_of(key, shard, entity));
        let mut record = [0u8; ROW_RECORD_BYTES];
        record[0..4].copy_from_slice(&code.to_le_bytes());
        record[4..8].copy_from_slice(&residual.to_le_bytes());
        record[8..12].copy_from_slice(&entity.to_le_bytes());
        pushed = partition.push_to(bucket, &record);
    });
    pushed?;
    Ok((
        PartitionedRows {
            store: partition.finish()?,
            rows: histogram.rows(),
        },
        boundaries,
        plan,
    ))
}

/// The 12 B record the row partition carries: `(morton, residual, entity)`. `priority` is not
/// carried — it is `forward(entity)`'s prefix, eight `splitmix64` rounds when the bucket is
/// loaded, against 2 B a row on disk for the whole view.
const ROW_RECORD_BYTES: usize = 12;

/// The `(entity, row)` record of step 4's partition, and the `(row, …)` key of step 5's.
const PAIR_RECORD_BYTES: usize = 8;

pub(crate) struct PartitionedRows {
    store: spill::PartitionStore,
    rows: u64,
}

impl PartitionedRows {
    pub(crate) fn rows(&self) -> u64 {
        self.rows
    }
}

fn priority_of(key: &IdentityKey, shard: u32, entity: u32) -> u16 {
    key.forward(shard, EntityId::new(entity as u64))
        .expect("entity ids are capped below u32::MAX by the allocator (I-1)")
        .priority()
}

/// Steps 3 to 6: the buckets in Morton order, the permutation, the render tail and the framing
/// around them.
pub(crate) fn write_segment(
    job: &Assembly<'_>,
    mut rows: PartitionedRows,
    boundaries: &MortonBoundaries,
    plan: PagePlan,
) -> Result<Assembled> {
    let rows_in_view = u32::try_from(rows.rows).map_err(|_| {
        BuildError::Invalid(format!(
            "the view holds {} rows, which does not fit the u32 a row id is",
            rows.rows
        ))
    })?;

    let morton_path = job.segment_dir.join("morton.u32");
    let row_entity_path = job.view_dir.join(tessera_store::ROW_ENTITY_FILE);
    let columns_path = job.segment_dir.join("columns.arrow");
    let permutation_path = job.view_dir.join("permutation.bin");

    let declared: Vec<(String, ScalarType)> = job
        .render
        .iter()
        .map(|column| (column.name.clone(), column.ty))
        .collect();
    let columns = ColumnsFile::create(
        &columns_path,
        ColumnsPlan::new(&declared, rows_in_view as usize)
            .map_err(|e| BuildError::io(&columns_path, e))?,
    )
    .map_err(|e| BuildError::io(&columns_path, e))?;

    let mut morton_out = std::io::BufWriter::new(
        std::fs::File::create(&morton_path).map_err(|e| BuildError::io(&morton_path, e))?,
    );
    let mut row_entity_out = std::io::BufWriter::new(
        std::fs::File::create(&row_entity_path)
            .map_err(|e| BuildError::io(&row_entity_path, e))?,
    );
    let mut pairs = Partition::create(
        job.tmp,
        &format!("assembly-{}-pairs", job.view),
        spill::boundaries_uniform(job.n),
        PAIR_RECORD_BYTES,
        rows_in_view as u64,
    )?;
    let mut occupancy = crate::OccupancyRun::default();

    let mut row: u64 = 0;
    for bucket in 0..boundaries.buckets() {
        let bytes = rows.store.load(bucket)?;
        rows.store.delete(bucket)?;
        let mut loaded: Vec<RowRec> = bytes
            .chunks_exact(ROW_RECORD_BYTES)
            .map(|record| {
                let read = |at: usize| {
                    u32::from_le_bytes(record[at..at + 4].try_into().expect("four bytes"))
                };
                let entity = read(8);
                RowRec {
                    morton: read(0),
                    entity,
                    residual: read(4),
                    priority: priority_of(job.identity_key, job.shard_id, entity),
                    _pad: 0,
                }
            })
            .collect();
        drop(bytes);
        loaded.sort_unstable_by(|a, b| a.cmp(b, job.identity_key, job.shard_id));

        for chunk in loaded.chunks(COLUMN_CHUNK_ROWS) {
            let mut codes: Vec<u8> = Vec::with_capacity(chunk.len() * 4);
            let mut entities: Vec<u8> = Vec::with_capacity(chunk.len() * 4);
            let mut identities: Vec<u8> = Vec::with_capacity(chunk.len() * 8);
            let mut residuals: Vec<u8> = Vec::with_capacity(chunk.len() * 4);
            for record in chunk {
                occupancy.push(record.morton);
                codes.extend_from_slice(&record.morton.to_le_bytes());
                entities.extend_from_slice(&record.entity.to_le_bytes());
                let identity = job
                    .identity_key
                    .forward(job.shard_id, EntityId::new(record.entity as u64))
                    .map_err(BuildError::Identity)?;
                identities.extend_from_slice(&identity.raw().to_le_bytes());
                residuals.extend_from_slice(&record.residual.to_le_bytes());
            }
            morton_out
                .write_all(&codes)
                .map_err(|e| BuildError::io(&morton_path, e))?;
            row_entity_out
                .write_all(&entities)
                .map_err(|e| BuildError::io(&row_entity_path, e))?;
            columns
                .put(0, row * 8, &identities)
                .map_err(|e| BuildError::io(&columns_path, e))?;
            columns
                .put(1, row * 4, &residuals)
                .map_err(|e| BuildError::io(&columns_path, e))?;
            for (at, record) in chunk.iter().enumerate() {
                let mut pair = [0u8; PAIR_RECORD_BYTES];
                pair[0..4].copy_from_slice(&record.entity.to_le_bytes());
                pair[4..8].copy_from_slice(&((row + at as u64) as u32).to_le_bytes());
                pairs.push(&pair)?;
            }
            row += chunk.len() as u64;
        }
    }
    morton_out
        .flush()
        .map_err(|e| BuildError::io(&morton_path, e))?;
    row_entity_out
        .flush()
        .map_err(|e| BuildError::io(&row_entity_path, e))?;
    drop(morton_out);
    drop(row_entity_out);
    debug_assert_eq!(row, rows_in_view as u64);
    let pairs = pairs.finish()?;

    // ---- 4. the permutation, one entity-range bucket at a time ------------------------------
    {
        let mut writer =
            PermutationWriter::create_planned(&permutation_path, &plan)
                .map_err(|e| BuildError::io(&permutation_path, e))?;
        for bucket in 0..spill::boundaries_uniform(job.n).len() {
            for pair in pairs.load(bucket)?.chunks_exact(PAIR_RECORD_BYTES) {
                let entity = u32::from_le_bytes(pair[0..4].try_into().expect("four bytes"));
                let at = u32::from_le_bytes(pair[4..8].try_into().expect("four bytes"));
                writer
                    .set(EntityId::new(entity as u64), at)
                    .map_err(|e| BuildError::io(&permutation_path, e))?;
            }
        }
        writer
            .finish()
            .map_err(|e| BuildError::io(&permutation_path, e))?;
    }

    // ---- 5. the render tail ------------------------------------------------------------------
    let presence_paths = render_tail(job, &columns, &pairs, rows_in_view)?;

    columns
        .finish()
        .map_err(|e| BuildError::io(&columns_path, e))?;
    Ok(Assembled {
        rows_in_view,
        occupancy: occupancy.finish(),
        morton_path,
        row_entity_path,
        columns_path,
        permutation_path,
        presence_paths,
        largest_bucket: boundaries.largest(),
    })
}

/// The row-range boundaries the render tail's second partition uses: the uniform ones, rounded
/// down to a multiple of 64 so that a `bool` column's bit-packed window starts on a byte boundary.
fn row_boundaries(rows: u32) -> Vec<u32> {
    spill::boundaries_uniform(rows as u64)
        .into_iter()
        .map(|first| (first as u64 / ROW_ALIGN * ROW_ALIGN) as u32)
        .collect::<std::collections::BTreeSet<u32>>()
        .into_iter()
        .collect()
}

/// Step 5, per render column: the entity buckets read the column sequentially and push
/// `(row, value)`; the row buckets write their window into the column's buffer.
fn render_tail(
    job: &Assembly<'_>,
    columns: &ColumnsFile,
    pairs: &spill::PartitionStore,
    rows_in_view: u32,
) -> Result<Vec<PathBuf>> {
    let entity_buckets = spill::boundaries_uniform(job.n).len();
    let boundaries = row_boundaries(rows_in_view);
    let mut paths = Vec::new();
    for (index, column) in job.render.iter().enumerate() {
        let slot = index + 2;
        let width = columns.width(slot);
        let value_bytes = width.unwrap_or(1);
        let record_width = 4 + value_bytes;
        let mut lane = Partition::create(
            job.tmp,
            &format!("assembly-{}-lane-{index}", job.view),
            boundaries.clone(),
            record_width,
            rows_in_view as u64,
        )?;
        for bucket in 0..entity_buckets {
            let mut bytes = pairs.load(bucket)?;
            // Sorted by entity so the column is read forward: the pairs arrived in row order
            // within a bucket, which is Morton order, and the column is entity-major.
            let pairs_in_bucket: &mut [[u8; PAIR_RECORD_BYTES]] = bytemuck_pairs(&mut bytes);
            pairs_in_bucket.sort_unstable_by_key(|pair| {
                u32::from_le_bytes(pair[0..4].try_into().expect("four bytes"))
            });
            let mut record = vec![0u8; record_width];
            for pair in pairs_in_bucket.iter() {
                let entity = u32::from_le_bytes(pair[0..4].try_into().expect("four bytes"));
                let Some(value) = column.values.raw_at(entity as usize) else {
                    // An absent entity pushes nothing: the row keeps the zero the reserved file
                    // reads as, which *is* the render placeholder (decision 0064), and the
                    // presence bitmap below says so.
                    continue;
                };
                record[0..4].copy_from_slice(&pair[4..8]);
                record[4..4 + value_bytes].copy_from_slice(&value[..value_bytes]);
                lane.push(&record)?;
            }
        }
        let mut lane = lane.finish()?;
        let mut present = croaring::Bitmap::new();
        let mut any_absent = false;
        for bucket in 0..boundaries.len() {
            let lo = boundaries[bucket] as u64;
            let hi = boundaries
                .get(bucket + 1)
                .map(|&first| first as u64)
                .unwrap_or(rows_in_view as u64)
                .min(rows_in_view as u64);
            let bytes = lane.load(bucket)?;
            lane.delete(bucket)?;
            if hi <= lo {
                continue;
            }
            let span = (hi - lo) as usize;
            let mut window = vec![0u8; span * value_bytes];
            let mut filled = vec![false; span];
            for record in bytes.chunks_exact(record_width) {
                let at = u32::from_le_bytes(record[0..4].try_into().expect("four bytes")) as usize
                    - lo as usize;
                window[at * value_bytes..(at + 1) * value_bytes]
                    .copy_from_slice(&record[4..4 + value_bytes]);
                filled[at] = true;
            }
            for (at, &is_filled) in filled.iter().enumerate() {
                if is_filled {
                    present.add((lo as usize + at) as u32);
                } else {
                    any_absent = true;
                }
            }
            match width {
                Some(width) => columns
                    .put(slot, lo * width as u64, &window)
                    .map_err(|e| BuildError::io(&job.segment_dir.join("columns.arrow"), e))?,
                None => {
                    // A `bool` column's buffer is one bit a row, least significant first — the
                    // bucket's first row is a multiple of 64, so the packed run starts on a byte.
                    let mut packed = vec![0u8; span.div_ceil(8)];
                    for (at, byte) in window.iter().enumerate() {
                        if *byte != 0 {
                            packed[at / 8] |= 1u8 << (at % 8);
                        }
                    }
                    columns
                        .put(slot, lo / 8, &packed)
                        .map_err(|e| BuildError::io(&job.segment_dir.join("columns.arrow"), e))?
                }
            }
        }
        if any_absent {
            if let Some(path) = tessera_store::flush::write_render_presence(
                job.segment_dir,
                &column.name,
                present,
                rows_in_view,
            )
            .map_err(|e| BuildError::Invalid(format!("attribute '{}': {e}", column.name)))?
            {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

/// A bucket's bytes as fixed-width pair records, so they can be sorted in place.
fn bytemuck_pairs(bytes: &mut [u8]) -> &mut [[u8; PAIR_RECORD_BYTES]] {
    let count = bytes.len() / PAIR_RECORD_BYTES;
    // SAFETY: `[u8; 8]` has the alignment and size of eight `u8`, and the slice covers exactly
    // `count` of them.
    unsafe {
        std::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut [u8; PAIR_RECORD_BYTES], count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn histogram_of(rows: &[(u32, u16)]) -> MortonHistogram {
        let mut histogram = MortonHistogram::for_rows(rows.len() as u64);
        for &(code, _) in rows {
            histogram.count(code);
        }
        histogram
    }

    fn code_counts(counts: &[(u32, u64)]) -> std::collections::BTreeMap<u32, u64> {
        counts.iter().copied().collect()
    }

    #[test]
    fn a_flat_histogram_closes_a_bucket_when_the_next_key_would_pass_the_target() {
        let counts = [(0u32, 3u64), (1, 3), (2, 3), (3, 3)];
        let (boundaries, largest) = spill::boundaries_from_histogram(0, counts, 6);
        assert_eq!(boundaries, vec![0, 2]);
        assert_eq!(largest, 6);
        // Every bucket holds at most the target, and consecutive buckets exceed it — the bound the
        // writer buffers are charged against.
        let (boundaries, largest) = spill::boundaries_from_histogram(0, counts, 2);
        assert_eq!(boundaries, vec![0, 1, 2, 3]);
        assert_eq!(largest, 3, "a key over the target keeps its own bucket and stays over it");
    }

    #[test]
    fn a_bin_over_the_target_is_refined_to_its_full_codes() {
        // 1,280 rows: ten in each of 128 cells of one bin, so the bin alone is over the target of
        // 1280 / 128 = 10 and the bin boundary would give one bucket 128 times the target.
        let rows: Vec<(u32, u16)> = (0..128u32)
            .flat_map(|cell| std::iter::repeat_n((cell, 0u16), 10))
            .collect();
        let histogram = histogram_of(&rows);
        assert_eq!(histogram.target(), 10);
        assert_eq!(histogram.hot_bins(10), vec![0]);

        let refinement = MortonRefinement {
            codes: vec![code_counts(
                &(0..128u32).map(|cell| (cell, 10u64)).collect::<Vec<_>>(),
            )],
            ..MortonRefinement::default()
        };
        let boundaries = morton_boundaries(&histogram, &refinement).expect("boundaries");
        assert_eq!(boundaries.buckets(), 128, "one bucket a cell");
        assert_eq!(boundaries.largest(), 10);
        assert_eq!(boundaries.route(0, || 0), 0);
        assert_eq!(boundaries.route(127, || 0), 127);
    }

    #[test]
    fn a_code_over_the_target_is_split_by_priority() {
        // Every row at one cell: the code cannot be split by any Morton boundary, so the split is
        // by the identity prefix the row order already ranks by.
        let histogram = histogram_of(&vec![(7u32, 0u16); 256]);
        assert_eq!(histogram.target(), 2);
        assert_eq!(histogram.hot_bins(2), vec![0]);
        let codes = code_counts(&[(7, 256)]);
        let mut priorities = vec![0u64; 1 << 16];
        for (at, slot) in priorities.iter_mut().enumerate().take(128) {
            *slot = if at == 0 { 129 } else { 1 };
        }
        // 129 rows share priority 0, which is over the target — and that is the refusal.
        let refinement = MortonRefinement {
            codes: vec![codes.clone()],
            priorities: vec![(7, priorities.clone())],
        };
        let refused = morton_boundaries(&histogram, &refinement).expect_err("over the target");
        assert!(
            format!("{refused}").contains("no finer key"),
            "{refused}"
        );

        // Spread over the prefix, the split lands: two rows a bucket.
        let mut priorities = vec![0u64; 1 << 16];
        for slot in priorities.iter_mut().take(256) {
            *slot = 1;
        }
        let refinement = MortonRefinement {
            codes: vec![codes.clone()],
            priorities: vec![(7, priorities)],
        };
        let boundaries = morton_boundaries(&histogram, &refinement).expect("boundaries");
        assert_eq!(boundaries.buckets(), 128);
        assert_eq!(boundaries.largest(), 2);
        // A row at the split code pays for its priority; one at any other code does not.
        assert_eq!(boundaries.route(7, || 0), 0);
        assert_eq!(boundaries.route(7, || 255), 127);
        assert_eq!(
            boundaries.route(8, || panic!("a code no split reaches must not ask for a priority")),
            127
        );
    }

    /// The tie path: comparing the `priority` prefix first and refining on a tie by recomputing
    /// the full `tessera_id` from `entity` must produce **exactly** the same row order as sorting
    /// by the full `tessera_id` directly — not merely "usually agrees". Fixed `morton` across
    /// every row so the fixture ties on the first comparator field too.
    #[test]
    fn row_rec_comparator_agrees_with_a_full_tessera_id_sort_over_engineered_ties() {
        let key = IdentityKey::from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
        let shard = 0u32;
        let morton = 42u32;

        let rows: Vec<RowRec> = (0..4000u32)
            .map(|entity| {
                let tessera_id = key.forward(shard, EntityId::new(entity as u64)).unwrap();
                RowRec {
                    morton,
                    entity,
                    residual: 0,
                    priority: tessera_id.priority(),
                    _pad: 0,
                }
            })
            .collect();

        let mut priorities: Vec<u16> = rows.iter().map(|r| r.priority).collect();
        priorities.sort_unstable();
        assert!(
            priorities.windows(2).any(|w| w[0] == w[1]),
            "fixture must contain at least one priority-prefix tie"
        );

        let mut via_comparator = rows.clone();
        via_comparator.sort_by(|a, b| a.cmp(b, &key, shard));

        let mut naive = rows;
        naive.sort_by_key(|r| {
            key.forward(shard, EntityId::new(r.entity as u64))
                .unwrap()
                .raw()
        });

        assert_eq!(
            via_comparator
                .iter()
                .map(|r| r.entity)
                .collect::<Vec<u32>>(),
            naive.iter().map(|r| r.entity).collect::<Vec<u32>>(),
            "the prefix-then-recompute comparator must agree with a full tessera_id sort"
        );
    }
}
