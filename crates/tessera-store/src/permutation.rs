//! `Permutation`: the **only** legal EntityId→RowId path in the codebase (invariant I4).
//! Backed by `permutation.bin` (R4): `"TSPM"` ‖ `u16 version` ‖ `u16 reserved` ‖ `u64 bound` ‖
//! `bound` little-endian `u32` slots, sentinel `0xFFFF_FFFF` for an entity never assigned a row
//! in this segment.
//!
//! ## Row space is that file plus an ordered extent list
//!
//! A build writes one segment per (partition, slice) and one `permutation.bin` covering it. A
//! flush appends a segment beside it, and [`RowSpace`] is what makes the pair addressable as one
//! row space: the base file below the build bound, an ordered list of [`SegmentExtent`]s above it.
//!
//! **The dispatch lives here rather than in the engine, and that is I4 rather than tidiness.**
//! The claim this module makes about itself — that it is the only legal EntityId→RowId path — is
//! falsified by an engine that learns to select an extent and index a segment. Every caller still
//! sees `row_of` and `project`; which segment answered is this module's business.
//!
//! Two bounds hold by construction and are checked at the one place an extent enters
//! ([`RowSpace::with_extent`]): total rows per slice stay under 2³², and the extent list is
//! bounded by the live segment count, which the merge policy bounds.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;
use rayon::prelude::*;

use tessera_types::{EntityId, RowId, ROW_ABSENT};

use crate::error::{Result, StoreError};

const PERMUTATION_MAGIC: &[u8; 4] = b"TSPM";
const PERMUTATION_VERSION: u16 = 1;
const HEADER_LEN: usize = 4 + 2 + 2 + 8; // magic, version, reserved, bound

/// A memory-mapped `permutation.bin`. `row_of` and `project` are the only ways to cross from
/// entity space to row space anywhere in the codebase (I4) — no other module may open this
/// file or otherwise derive a row ID from an entity ID.
#[derive(Debug)]
pub struct Permutation {
    mmap: Mmap,
    bound: u64,
    bound_usize: usize,
    path: PathBuf,
}

impl Permutation {
    /// Open and validate `path`: magic, version, and that the file is exactly
    /// `HEADER_LEN + bound * 4` bytes (a truncated or padded file is a corrupt bundle, not a
    /// partial one to silently accept).
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: the mapped file is treated as read-only for the lifetime of this struct;
        // nothing in this process writes to it concurrently. A backing file that another
        // process truncates while mapped is an operational hazard shared with every other
        // mmap-based reader in this codebase (`tessera-authz`'s postings reader), not one
        // introduced here.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        if mmap.len() < HEADER_LEN {
            return Err(StoreError::InvalidPermutation {
                path: path.to_path_buf(),
                detail: format!(
                    "file is {} bytes, shorter than the {HEADER_LEN}-byte header",
                    mmap.len()
                ),
            });
        }
        if &mmap[0..4] != PERMUTATION_MAGIC {
            return Err(StoreError::InvalidPermutation {
                path: path.to_path_buf(),
                detail: "bad magic (expected 'TSPM')".to_string(),
            });
        }
        let version = u16::from_le_bytes(mmap[4..6].try_into().expect("2-byte slice"));
        if version != PERMUTATION_VERSION {
            return Err(StoreError::InvalidPermutation {
                path: path.to_path_buf(),
                detail: format!("unsupported version {version} (expected {PERMUTATION_VERSION})"),
            });
        }
        let bound = u64::from_le_bytes(mmap[8..16].try_into().expect("8-byte slice"));
        // Checked, not `as usize`: on a 32-bit target (or an adversarial 64-bit `bound` value)
        // a truncating cast would silently shrink `bound` instead of failing closed.
        let bound_usize = usize::try_from(bound).map_err(|_| StoreError::InvalidPermutation {
            path: path.to_path_buf(),
            detail: format!("bound {bound} does not fit in usize on this platform"),
        })?;

        let expected_len = bound_usize
            .checked_mul(4)
            .and_then(|slots_len| slots_len.checked_add(HEADER_LEN))
            .ok_or_else(|| StoreError::InvalidPermutation {
                path: path.to_path_buf(),
                detail: format!("bound {bound} overflows the expected file length"),
            })?;
        if mmap.len() != expected_len {
            return Err(StoreError::InvalidPermutation {
                path: path.to_path_buf(),
                detail: format!(
                    "file is {} bytes, expected {expected_len} for bound {bound}",
                    mmap.len()
                ),
            });
        }

        Ok(Permutation {
            mmap,
            bound,
            bound_usize,
            path: path.to_path_buf(),
        })
    }

    /// The number of entity-ID slots this permutation covers, `[0, bound)`.
    pub fn bound(&self) -> u64 {
        self.bound
    }

    fn slots(&self) -> &[u32] {
        let bytes = &self.mmap[HEADER_LEN..];
        debug_assert_eq!(bytes.len(), self.bound_usize * 4);
        // SAFETY: `bytes` starts at a fixed offset (HEADER_LEN = 16) into a page-aligned mmap
        // base, and 16 is a multiple of 4, so `bytes.as_ptr()` is 4-byte aligned regardless of
        // file content — no adversarial input can misalign this cast. Length is exactly
        // `bound * 4` bytes, checked once at `load`.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u32, self.bound_usize) }
    }

    /// Validate that every non-sentinel slot addresses a row within `row_count`, and that no
    /// two entities claim the same row (a permutation is a bijection onto `[0, row_count)`,
    /// not merely a function into it). Called once per slice at bundle open, against the row
    /// count of the single build segment this permutation addresses (R4) — **not** on any
    /// per-viewport path (this is an `O(bound)` scan, same cost class as [`Self::project`]).
    /// A corrupt or hand-edited `permutation.bin` that points rows out of range, or that
    /// aliases two entities onto one row, must fail bundle open rather than let `row_of` or
    /// `project` later hand out a `RowId` that indexes `columns.arrow` out of bounds (I4/I11).
    pub fn validate_rows(&self, row_count: u32) -> Result<()> {
        let row_count_usize = row_count as usize;
        let mut seen = vec![false; row_count_usize];
        for (entity, &slot) in self.slots().iter().enumerate() {
            if slot == ROW_ABSENT {
                continue;
            }
            if slot >= row_count {
                return Err(StoreError::InvalidPermutation {
                    path: self.path.clone(),
                    detail: format!(
                        "entity {entity} maps to row {slot}, out of bound for row_count \
                         {row_count}"
                    ),
                });
            }
            let idx = slot as usize;
            if seen[idx] {
                return Err(StoreError::InvalidPermutation {
                    path: self.path.clone(),
                    detail: format!(
                        "row {slot} is claimed by more than one entity (not a bijection)"
                    ),
                });
            }
            seen[idx] = true;
        }
        Ok(())
    }

    /// Row ID currently occupied by `e` in this segment, or `None` if `e` is out of bound or
    /// holds the row-absent sentinel (never allocated a row here — including entities that
    /// exist but live in a different segment, or don't exist at all).
    pub fn row_of(&self, e: EntityId) -> Option<RowId> {
        let raw = e.raw();
        if raw >= self.bound {
            return None;
        }
        let slot = self.slots()[raw as usize];
        if slot == ROW_ABSENT {
            None
        } else {
            Some(RowId::new(slot))
        }
    }

    /// Project an entity-space bitmap into row space: for every entity ID set in `mask`
    /// (ascending order, as `croaring::Bitmap` iterates), look up its row via this
    /// permutation, skip entities with no row here (out of bound or sentinel), and return the
    /// resulting row IDs as a bitmap.
    ///
    /// **Cost note (shared-context constraint 8):** this touches every set bit in `mask` and
    /// then sorts the results — at 10⁹ rows this costs *seconds*, not the microseconds a
    /// viewport query budgets for. Never call this on the per-viewport path; the engine caches
    /// the result per `(token, slice, pin)` and reuses it across viewports within a session.
    ///
    /// **Parallel, and executor-agnostic.** This crate owns no `rayon::ThreadPool` of its
    /// own — `par_chunks`/`par_sort_unstable` below run on whatever pool the caller has
    /// `install`ed (the engine wraps its cold-session build in `self.pool.install(..)`, D-D), or
    /// on rayon's own global pool if nobody has. Chunk boundaries never change the result — each
    /// chunk's matches are independent of every other chunk's, and are simply concatenated and
    /// then sorted — only throughput does, so this stays correct under any thread count,
    /// including 1 (see the `--ignored` gate test and the correctness tests in
    /// `tests/permutation_project_parallel.rs`, both of which run this at several thread counts).
    ///
    /// **Transient memory (brief's constraint; fix round 1 correction).** Two bindings are live
    /// at any one instant, never three: `entities` is dropped explicitly the moment the chunked
    /// map that reads it has finished (before `rows` exists at all), and `per_chunk` is consumed
    /// — not copied — into `rows`, so its chunks free themselves one at a time as `rows` fills
    /// rather than sitting alongside a fully-built `rows`. The peak instant is therefore either
    /// "`entities` (mask.cardinality() `u32`s) + the just-finished `per_chunk` (<= the same
    /// size)" or "the just-finished `per_chunk` + `rows`'s reserved-but-empty capacity (exactly
    /// that size, computed below)" — both are one cardinality's worth of `u32`s each, so peak
    /// transient is roughly **double** `mask.cardinality()` `u32`s, not triple. This is on top of
    /// the mmap-backed `slots()` array, which is never copied — only ever borrowed, read
    /// concurrently by every chunk.
    ///
    /// Output is a sorted set of *unique* row IDs — unique because `self` is a permutation (a
    /// bijection), so no two entities can ever map to the same row, whichever chunk found them —
    /// and the sort makes the result order-independent of chunk scheduling, so the output bytes
    /// cannot change with the thread count.
    pub fn project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        let slots = self.slots();
        let entities: Vec<u32> = mask.to_vec();

        // Aim for a handful of chunks per worker so a run of mostly-sentinel entities in one
        // chunk doesn't leave a worker idle while the others are still busy — the same
        // over-subscription reasoning as `tessera-engine::viewport`'s `TILE_PAR_MIN_LEN`, just
        // computed from the ambient pool's size rather than a fixed constant, since this crate
        // does not know (and must not assume) how large that pool is.
        let threads = rayon::current_num_threads().max(1);
        let chunk_len = (entities.len() / (threads * 8)).max(1);

        let per_chunk: Vec<Vec<u32>> = entities
            .par_chunks(chunk_len)
            .map(|chunk| {
                // `chunk.len()` is an exact upper bound on this chunk's hits (every filtered
                // element survives at most once), so this capacity hint means the chunk's local
                // `Vec` never reallocates as it fills — no realloc churn on top of the peak this
                // doc note already accounts for.
                let mut local = Vec::with_capacity(chunk.len());
                local.extend(
                    chunk
                        .iter()
                        .filter_map(|&entity| slots.get(entity as usize).copied())
                        .filter(|&slot| slot != ROW_ABSENT),
                );
                local
            })
            .collect();
        // `entities` is dead from here on — dropped explicitly rather than left to fall out of
        // scope at the end of the function, so its allocation is freed before `rows` is even
        // reserved below (fix round 1: this used to overlap with both `per_chunk` and `rows` at
        // once, a 3x peak rather than the documented 2x).
        drop(entities);

        let total_rows: usize = per_chunk.iter().map(Vec::len).sum();
        let mut rows: Vec<u32> = Vec::with_capacity(total_rows);
        // `per_chunk.into_iter()` yields owned `Vec<u32>`s one at a time; `flatten` drains and
        // drops each one as `extend` exhausts it, so `per_chunk`'s chunks free themselves
        // progressively as `rows` fills, rather than the whole of `per_chunk` staying alive
        // alongside a fully-built `rows` (which is what `per_chunk.concat()` did before this
        // fix — the other half of the 3x-not-2x peak).
        rows.extend(per_chunk.into_iter().flatten());

        rows.par_sort_unstable();
        // croaring 2.x has no dedicated "construct from sorted slice" entry point; `of` /
        // `add_many` (`roaring_bitmap_add_many`) is CRoaring's bulk-add path and is what the
        // design's "build from sorted output" guidance (§10.4) maps onto in this binding.
        // Row IDs are unique (a permutation), so `rows` has no duplicates to fold away.
        croaring::Bitmap::of(&rows)
    }

    /// The path this permutation was loaded from (for diagnostics only).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One flush or merge segment's slice of row space: the entity range it covers, and where in
/// row space its rows live.
///
/// `rows` is **dense over `[entity_lo, entity_hi]`** and holds each entity's row *relative to*
/// `row_base`, or [`ROW_ABSENT`] for an entity the segment never got a row for — a
/// deleted-at-flush entity, whose ID stays burned (I9) while no row is created for it. Dense
/// rather than sparse because the range is contiguous by construction: entity IDs are issued
/// monotonically from the high-water, so a flush segment covers a contiguous ascending range
/// (§2.1), and one `u32` per entity is smaller than any keyed form over the same span.
///
/// It is not a `permutation.bin`. That file's length is the *bundle's* whole entity space, which
/// is the wrong shape for a segment covering a few thousand ids at the top of it; an extent's rows
/// live in the side-manifest's file set instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentExtent {
    pub entity_lo: u64,
    /// Inclusive.
    pub entity_hi: u64,
    pub seg_id: String,
    pub row_base: u32,
    /// `rows[e - entity_lo]`, relative to `row_base`; [`ROW_ABSENT`] where the entity has no row.
    pub rows: Vec<u32>,
}

impl SegmentExtent {
    /// Recover a flush or merge segment's extent from the segment itself, at open.
    ///
    /// **Why this exists rather than a file.** §2.1 describes an extent as four scalars —
    /// `{entity_lo, entity_hi, seg_id, row_base}` — which is only a mapping if the segment's rows
    /// are in entity order. The same section requires every segment to be Morton-sorted, and that
    /// sort *is* the tile index, so the mapping is an arbitrary permutation of the entity range and
    /// four scalars cannot express it. Something has to carry it across a restart.
    ///
    /// The alternative was to write `rows` beside `morton.u32` (4 bytes × entities in the segment,
    /// one more file, one more manifest field, a contracts §2.1 change). This construction stores
    /// **nothing**: `columns.arrow` already carries `tessera_id` at the row, the identity is a
    /// bijection over 2⁶⁴ ([`tessera_types::IdentityKey`]), and `MANIFEST.json` already carries the
    /// key — so the mapping is derivable from artefacts that must exist anyway. Ruled on
    /// 2026-08-02 in favour of adding no artefact.
    ///
    /// **The invariant it spends, stated so it is not spent again silently.** Row space above the
    /// build bound is now recoverable *only* while the identity permutation is invertible at open.
    /// A future construction that made `tessera_id` one-way — a keyed hash, a key held outside the
    /// bundle, a per-session blinding — would silently strand every flushed entity's row, exactly
    /// the failure this replaces. I10 is unaffected: nothing here leaves the engine, and the
    /// inversion is the same one `Engine::item` already performs per request.
    ///
    /// **Cost.** One Feistel inversion per row of each flush segment, at every open, bounded by
    /// what merge leaves unmerged rather than by the corpus. Deliberately not parallelised: it runs
    /// once at open, inside a loop that is already mapping and digest-verifying files.
    pub fn rebuild(
        seg_id: &str,
        entity_lo: u64,
        entity_hi: u64,
        row_base: u32,
        tessera_ids: &[u64],
        key: &tessera_types::IdentityKey,
        shard_id: u32,
    ) -> Result<Self> {
        let malformed = |detail: String| StoreError::MalformedBundle { detail };
        let span = entity_hi
            .checked_sub(entity_lo)
            .and_then(|d| d.checked_add(1))
            .and_then(|d| usize::try_from(d).ok())
            .ok_or_else(|| {
                malformed(format!(
                    "segment '{seg_id}': entity span {entity_lo}..={entity_hi} is inverted or too \
                     wide to address"
                ))
            })?;

        let mut rows = vec![ROW_ABSENT; span];
        for (local, &raw) in tessera_ids.iter().enumerate() {
            let (shard, entity) = key.invert(tessera_types::TesseraId::new(raw));
            // A wrong shard means this segment was written under a different identity
            // configuration than the manifest declares — corruption, not a row to skip. Serving
            // past it would put a row under an entity id that names a different item.
            if shard != shard_id {
                return Err(malformed(format!(
                    "segment '{seg_id}': row {local}'s tessera_id inverts to shard {shard}, but \
                     the manifest declares shard {shard_id}"
                )));
            }
            let raw_entity = entity.raw();
            if raw_entity < entity_lo || raw_entity > entity_hi {
                return Err(malformed(format!(
                    "segment '{seg_id}': row {local} belongs to entity {raw_entity}, outside the \
                     descriptor's range {entity_lo}..={entity_hi}"
                )));
            }
            let slot = &mut rows[(raw_entity - entity_lo) as usize];
            if *slot != ROW_ABSENT {
                return Err(malformed(format!(
                    "segment '{seg_id}': entity {raw_entity} is claimed by rows {} and {local} — \
                     the identity is a bijection, so two rows cannot invert to one entity",
                    *slot
                )));
            }
            *slot = local as u32;
        }

        let extent = SegmentExtent {
            entity_lo,
            entity_hi,
            seg_id: seg_id.to_string(),
            row_base,
            rows,
        };
        // The same check `with_extent` applies to an extent arriving from a flush. Applied here
        // too rather than left to the caller, so a rebuild that produced a malformed extent says
        // which segment it was reading rather than surfacing as "does not continue row space".
        if !extent.is_well_formed() {
            return Err(malformed(format!(
                "segment '{seg_id}': the extent rebuilt from its tessera_id column is not a \
                 bijection onto its own rows"
            )));
        }
        Ok(extent)
    }

    /// How many rows this extent actually owns — the non-absent slots, not the entity span.
    pub fn row_count(&self) -> u32 {
        self.rows.iter().filter(|&&r| r != ROW_ABSENT).count() as u32
    }

    /// Whether this extent is internally well-formed: its `rows` cover its entity span exactly,
    /// and the non-absent slots are a bijection onto `[0, row_count)`.
    ///
    /// The same property [`Permutation::validate_rows`] enforces for the base, and for the same
    /// reason: a slot outside the range, or two entities aliased onto one row, would let `row_of`
    /// hand out a `RowId` that indexes the segment's `columns.arrow` out of bounds (I4/I11).
    fn is_well_formed(&self) -> bool {
        if self.entity_hi < self.entity_lo {
            return false;
        }
        let Some(span) = self
            .entity_hi
            .checked_sub(self.entity_lo)
            .and_then(|d| d.checked_add(1))
            .and_then(|d| usize::try_from(d).ok())
        else {
            return false;
        };
        if self.rows.len() != span {
            return false;
        }
        let count = self.row_count();
        let mut seen = vec![false; count as usize];
        for &row in &self.rows {
            if row == ROW_ABSENT {
                continue;
            }
            if row >= count || seen[row as usize] {
                return false;
            }
            seen[row as usize] = true;
        }
        true
    }

    /// This extent's contribution to a projection: the rows of every entity of `mask` that falls
    /// inside it. Nothing else in `mask` can be answered here, so nothing else is looked at.
    ///
    /// **It seeks to `entity_lo` rather than skipping up to it, and the difference is the whole
    /// cost of a flush's patch.** An extent's entities are the newest in the partition, so they
    /// sit at the top of the mask; a walk from the mask's start pays O(mask cardinality) to reach
    /// them — *measured* 79 ms per extent against a 25 M-entity grant at 10⁸
    /// (`probes/2026-08-04-refresh-ladder/`), which is the patch's cost being a function of the
    /// grant's width rather than of the flush's size. `reset_at_or_after` costs O(containers
    /// skipped), which is the cost model this index is designed against
    /// (`CLAUDE.md`: bitmap operations cost O(containers touched), not O(cardinality)).
    fn project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        let mut rows: Vec<u32> = Vec::new();
        // `entity_hi` is inclusive and the range end is exclusive; both ends are already inside
        // `u32` because a `mask` is entity-space and entity ids are capped at `u32::MAX` (I9).
        let lo = u32::try_from(self.entity_lo).unwrap_or(u32::MAX);
        let hi = u32::try_from(self.entity_hi).unwrap_or(u32::MAX);
        let mut iter = mask.iter();
        iter.reset_at_or_after(lo);
        for entity in iter {
            if entity > hi {
                break;
            }
            let slot = self.rows[(entity - lo) as usize];
            if slot != ROW_ABSENT {
                rows.push(self.row_base + slot);
            }
        }
        croaring::Bitmap::of(&rows)
    }

    fn row_of(&self, entity: u64) -> Option<RowId> {
        if entity < self.entity_lo || entity > self.entity_hi {
            return None;
        }
        let slot = self.rows[(entity - self.entity_lo) as usize];
        (slot != ROW_ABSENT).then(|| RowId::new(self.row_base + slot))
    }
}

/// One slice's whole entity→row mapping: the built base permutation, plus the extents flush has
/// appended and merge has collapsed since.
///
/// **Constructed incrementally, never rebuilt.** [`Self::with_extent`] and [`Self::collapsing`]
/// return a new value sharing the base by `Arc`, because re-opening it would re-pay
/// `Permutation::load`'s `O(bound)` `validate_rows` — more than the flush that prompted it.
///
/// The extent list is ordered, ascending and disjoint, and each extent begins exactly where row
/// space currently ends. That is what makes `total_rows` a running sum rather than a scan, and it
/// is what a merge preserves: a merge emits exactly as many rows as it consumed, so no later
/// extent's `row_base` ever moves.
#[derive(Debug, Clone)]
pub struct RowSpace {
    base: Arc<Permutation>,
    /// The build segment's row count — the base owns `[0, base_rows)`. Not derivable from the
    /// permutation, whose `bound` is an entity-space width and may exceed its row count.
    base_rows: u32,
    /// Ordered, ascending, disjoint.
    extents: Vec<SegmentExtent>,
    /// `base_rows` plus every extent's `row_count`, maintained rather than recomputed.
    total_rows: u64,
}

impl RowSpace {
    pub fn new(base: Arc<Permutation>, base_rows: u32) -> Self {
        RowSpace {
            base,
            base_rows,
            extents: Vec::new(),
            total_rows: base_rows as u64,
        }
    }

    /// This row space plus one more segment, sharing the base.
    ///
    /// `None` if `extent` is malformed, does not begin strictly above the last extent's
    /// `entity_hi`, does not begin at or above the base's bound, or does not continue row space
    /// exactly (`row_base == total_rows()`). Every one of those is corruption rather than a state
    /// to tolerate: a gap or an overlap makes some other segment's rows unreachable or aliased.
    pub fn with_extent(&self, extent: SegmentExtent) -> Option<Self> {
        if !extent.is_well_formed() {
            return None;
        }
        let entity_floor = match self.extents.last() {
            Some(last) => last.entity_hi + 1,
            None => self.base.bound(),
        };
        if extent.entity_lo < entity_floor {
            return None;
        }
        if u64::from(extent.row_base) != self.total_rows {
            return None;
        }
        let total_rows = self.total_rows + u64::from(extent.row_count());
        // Row ids are `u32` (bundle_format 1), so a slice that would cross 2^32 rows must fail
        // here rather than at the first `row_base + slot` that wraps.
        if total_rows > u64::from(u32::MAX) {
            return None;
        }
        let mut extents = self.extents.clone();
        extents.push(extent);
        Some(RowSpace {
            base: Arc::clone(&self.base),
            base_rows: self.base_rows,
            extents,
            total_rows,
        })
    }

    /// This row space with the adjacent run `seg_ids` replaced by the single extent `merged`.
    ///
    /// `None` if any input `seg_id` is absent from the current generation — which is how a merge
    /// planned against a superseded generation is discarded rather than published. It is ABA-safe
    /// because `seg_id`s are never reused, across merges or prefixes (contracts §2.1), so a
    /// present `seg_id` is the same segment the merge consumed.
    ///
    /// Also `None` unless the inputs form a *contiguous run* and `merged` covers exactly their
    /// entity range at exactly their `row_base` and row count. A merge is row-count preserving, so
    /// anything else would move a later extent's rows.
    pub fn collapsing(&self, seg_ids: &[String], merged: SegmentExtent) -> Option<Self> {
        if seg_ids.is_empty() || !merged.is_well_formed() {
            return None;
        }
        let start = self
            .extents
            .iter()
            .position(|e| e.seg_id == seg_ids[0])
            .filter(|&i| i + seg_ids.len() <= self.extents.len())?;
        let run = &self.extents[start..start + seg_ids.len()];
        if run.iter().zip(seg_ids).any(|(e, id)| &e.seg_id != id) {
            return None;
        }
        let consumed_rows: u32 = run.iter().map(SegmentExtent::row_count).sum();
        if merged.entity_lo != run[0].entity_lo
            || merged.entity_hi != run[run.len() - 1].entity_hi
            || merged.row_base != run[0].row_base
            || merged.row_count() != consumed_rows
        {
            return None;
        }
        let mut extents = Vec::with_capacity(self.extents.len() - seg_ids.len() + 1);
        extents.extend_from_slice(&self.extents[..start]);
        extents.push(merged);
        extents.extend_from_slice(&self.extents[start + seg_ids.len()..]);
        Some(RowSpace {
            base: Arc::clone(&self.base),
            base_rows: self.base_rows,
            extents,
            // Row-count preserving, by the check above.
            total_rows: self.total_rows,
        })
    }

    /// How many rows the base permutation covers — the boundary below which no flush and no merge
    /// moves a row, which is what makes an extents-only re-projection exact
    /// (`tessera_engine::compose::RowProjection::rebase_extents`).
    pub fn base_rows(&self) -> u32 {
        self.base_rows
    }

    /// Row ID currently occupied by `e`, or `None` if it has none — the base lookup below the
    /// build bound, otherwise a binary search over the extent list, `O(log k)`.
    pub fn row_of(&self, e: EntityId) -> Option<RowId> {
        if e.raw() < self.base.bound() {
            return self.base.row_of(e);
        }
        let raw = e.raw();
        let i = self
            .extents
            .partition_point(|extent| extent.entity_lo <= raw)
            .checked_sub(1)?;
        self.extents[i].row_of(raw)
    }

    /// Project an entity-space bitmap into this slice's row space: the base projection unioned
    /// with each extent's own. The parts are disjoint — an extent's rows lie at or above
    /// `row_base`, which is where every earlier part ended — so the union is exact rather than
    /// merely a superset, and that is what lets a flush patch a cached projection instead of
    /// rebuilding it.
    ///
    /// Inherits [`Permutation::project`]'s cost note in full: never call this on a per-viewport
    /// path.
    pub fn project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        let mut rows = self.base.project(mask);
        rows.or_inplace(&self.project_extents_from(mask, 0));
        rows
    }

    /// The rows contributed by the extents at or after `from` — the only part a flush recomputes.
    pub fn project_extents_from(&self, mask: &croaring::Bitmap, from: usize) -> croaring::Bitmap {
        let mut rows = croaring::Bitmap::new();
        for extent in &self.extents[from.min(self.extents.len())..] {
            rows.or_inplace(&extent.project(mask));
        }
        rows
    }

    pub fn extent_count(&self) -> usize {
        self.extents.len()
    }

    pub fn extents(&self) -> &[SegmentExtent] {
        &self.extents
    }

    /// Every row this slice holds, across the base and every extent.
    pub fn total_rows(&self) -> u64 {
        self.total_rows
    }

    /// The base permutation, for the callers that legitimately need the built segment alone —
    /// `tessera build`'s own verification, and the sharing assertion incremental generation
    /// construction rests on.
    pub fn base(&self) -> &Arc<Permutation> {
        &self.base
    }
}
