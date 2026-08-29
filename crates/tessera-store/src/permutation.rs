//! `Permutation`: the **only** legal EntityId→RowId path in the codebase (invariant I4).
//! Backed by `permutation.bin` (R4): `"TSPM"` ‖ `u16 version` ‖ `u16 reserved` ‖ `u64 bound` ‖
//! `bound` little-endian `u32` slots, sentinel `0xFFFF_FFFF` for an entity never assigned a row
//! in this segment.
//!
//! ## Row space is that file plus an ordered extent list
//!
//! A build writes one segment per (partition, view) and one `permutation.bin` covering it. A
//! flush appends a segment beside it, and [`RowSpace`] is what makes the pair addressable as one
//! row space: the base file below the build bound, an ordered list of [`SegmentExtent`]s above it.
//!
//! **The dispatch lives here rather than in the engine, and that is I4 rather than tidiness.**
//! The claim this module makes about itself — that it is the only legal EntityId→RowId path — is
//! falsified by an engine that learns to select an extent and index a segment. Every caller still
//! sees `row_of` and `project`; which segment answered is this module's business.
//!
//! Two bounds hold by construction and are checked at the one place an extent enters
//! ([`RowSpace::with_extent`]): total rows per view stay under 2³², and the extent list is
//! bounded by the live segment count, which the merge policy bounds.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

use tessera_roaring::{Sink, WORDS};
use tessera_types::{EntityId, RowId, ROW_ABSENT};

use crate::error::{Result, StoreError};

const PERMUTATION_MAGIC: &[u8; 4] = b"TSPM";
const PERMUTATION_VERSION: u16 = 1;
const HEADER_LEN: usize = 4 + 2 + 2 + 8; // magic, version, reserved, bound

/// Rows per bucket in [`Permutation::project`], as a power of two — the one free parameter in that
/// pass, and the two constraints that fix it pull in opposite directions.
///
/// A bucket owns a contiguous range of row space. Too *wide* and the bit array it stamps falls out
/// of L2, making every row's stamp a last-level miss; too *narrow* and there are so many live write
/// cursors in the first pass that the append side misses instead. 2²² rows is a 512 KB bit array,
/// which is L2-resident on the machines this targets, and leaves 239 buckets at 10⁹ — whose cursors
/// and write tails together stay inside L1.
///
/// It is also a whole number of Roaring containers (64 of them), which is not a coincidence to be
/// preserved by luck: the emit step hands each container's words straight to [`Sink`], so a bucket
/// boundary that fell inside a container would split a payload across two of them.
const BUCKET_SHIFT: u32 = 22;

/// The buffers [`Permutation::project_with`] reuses between calls.
///
/// A 512 KB stamp and one `Vec` per bucket. They hold nothing between calls — `project_with`
/// clears them on entry — so a `Default` one and a reused one give byte-identical results; what
/// reuse saves is the allocation and the zeroing, which at one projection per artifact is the
/// dominant cost of the artifact pass rather than a rounding error.
#[derive(Default)]
pub struct ProjectScratch {
    buckets: Vec<Vec<u32>>,
    stamp: Vec<u64>,
}

/// Entity IDs decoded from the mask at a time.
///
/// The point of decoding in bulk at all is that croaring's `read_many` is a memcpy per container
/// where stepping an iterator is a call per value: an alternative that range-splits the mask and
/// steps a cursor per range measured between 0.70× and 1.07× of this across scales — that is, never
/// reliably better, and it gives up the sequential walk of the slot array as well. The point of the
/// window being *small* is that it is reused: at 10⁹ a whole entity list is 1 GB written and
/// immediately re-read, and this is 32 KB that stays hot.
const DECODE_WINDOW: usize = 8192;

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
        let version = u16::from_le_bytes(mmap[4..6].try_into().expect("2-byte view"));
        if version != PERMUTATION_VERSION {
            return Err(StoreError::InvalidPermutation {
                path: path.to_path_buf(),
                detail: format!("unsupported version {version} (expected {PERMUTATION_VERSION})"),
            });
        }
        let bound = u64::from_le_bytes(mmap[8..16].try_into().expect("8-byte view"));
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
    /// not merely a function into it). Called once per view at bundle open, against the row
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
    /// **Cost (shared-context constraint 8):** this touches every set bit in `mask` and reads the
    /// slot array end to end, which is over a second at 10⁹ — **1 277 ms** single-threaded over a
    /// 25% grant (`probes/2026-08-14-project-decomposition/`). Never call it on the per-viewport
    /// path; the engine caches the result per `(token, view, pin)` and reuses it across viewports
    /// within a session.
    ///
    /// # One pass, and why the shape is not the obvious one
    ///
    /// §10.4 asks for "gather, radix sort, and bulk-construct from the sorted array". The gather
    /// and the sort are the same pass here, and the sort is not a sort.
    ///
    /// A row is written **once**, into a bucket, straight out of the slot lookup — the entity list
    /// never exists, and neither does a row array to be sorted and re-read. The earlier form wrote
    /// it four times over (decode to a `Vec`, gather, sort in place, read back through `add_many`)
    /// and spent 5 272 ms of its 8 267 ms in `par_sort_unstable` alone.
    ///
    /// **A bucket is a row range, not a container key**, and that is the load-bearing choice rather
    /// than an arbitrary one. Partitioning on the high 16 bits — which *is* the Roaring container
    /// key, so it looks like the natural unit — leaves 15 259 live write cursors at 10⁹, far more
    /// than the cache holds, and was **measured to get worse with scale**: 3.22× over the stages it
    /// replaces at 10⁸ but only 2.06× at 10⁹. [`BUCKET_SHIFT`] instead fixes the two things that
    /// decide the cost, and they pull in opposite directions: few enough buckets that the write
    /// cursors stay in L1, wide enough that the bit array each one stamps stays in L2.
    ///
    /// **Containers are emitted, not inserted.** The bit array a bucket stamps *is* 64 Roaring
    /// container payloads laid end to end, so [`tessera_roaring::Sink`] takes them as they are —
    /// a popcount and a memcpy each. Expanding them back to `u32` for `add_many` instead costs
    /// 1 077 ms more at 10⁹ (2 306 ms against 1 229 ms), which is the larger half of the primitive.
    ///
    /// **Serial, deliberately, and it is still faster in wall clock.** The form this replaces was
    /// rayon-parallel and reached **3 024 ms on twelve threads** at 10⁹; this reaches 1 277 ms on
    /// one. So the serial rewrite is not a trade of latency for efficiency — it wins both, by 2.4×
    /// on wall clock while leaving eleven cores to other sessions. That second part is the reason
    /// to care: a server answering concurrent sessions is already saturated, so a projection that
    /// spreads across cores buys throughput nothing and only removing work counts.
    ///
    /// The one-pass form has no parallel decomposition worth taking in any case — see
    /// [`DECODE_WINDOW`] for the range-split alternative and why bulk decode beats it.
    ///
    /// **Transient memory** is one `u32` per row of the result — roughly 1 GB at 10⁹ over a 25%
    /// grant, held once in the buckets, against the three simultaneous copies the previous form
    /// peaked at. The mmap-backed slot array is never copied, only read.
    pub fn project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        self.project_with(mask, &mut ProjectScratch::default())
    }

    /// [`Self::project`], reusing a caller's scratch buffers.
    ///
    /// **For a caller that projects many masks in a row**, which the artifact pass does — once per
    /// artifact, three times over per level. The scratch this reuses is a 512 KB stamp plus one
    /// `Vec` per bucket, and allocating it per call put an `mmap`/`munmap` pair and 128 minor
    /// faults on every projection: at 6×10⁵ artifacts that is ~10⁸ page faults and hundreds of
    /// gigabytes of zeroing, for buffers whose contents never outlive the call. The session path
    /// projects once and keeps [`Self::project`], which allocates as it always did.
    ///
    /// Behaviour is identical — the buffers are cleared rather than carried, exactly as a fresh
    /// allocation would leave them.
    pub fn project_with(
        &self,
        mask: &croaring::Bitmap,
        scratch: &mut ProjectScratch,
    ) -> croaring::Bitmap {
        let slots = self.slots();
        // Every row this permutation can yield is below `bound`: `validate_rows` establishes that
        // it is a bijection *onto* `[0, row_count)`, so each row is claimed by a distinct in-bound
        // entity and `row_count <= bound`. Sizing the buckets from `bound` therefore cannot
        // under-count them, whatever the mask contains.
        let nbuckets = (self.bound_usize >> BUCKET_SHIFT) + 1;
        // Rows land near-uniformly across row space — a build orders them by `(morton,
        // tessera_id)`, which is uncorrelated with entity-issue order — so the mean plus a quarter
        // absorbs the variation without a histogram pass to find it. **The slack is not a rounding
        // habit**: reserving the bare mean leaves about half the buckets to exceed it and double,
        // and a bucket at 10⁹ is megabytes, so that is a realloc and a memcpy of the whole thing on
        // half of them. Not separately measured — it was not separable from run-to-run variance at
        // this size — so it is here on the argument, not on a number.
        let expected = (mask.cardinality() as usize / nbuckets)
            .saturating_mul(5)
            .saturating_div(4)
            .saturating_add(64);
        let buckets = &mut scratch.buckets;
        for bucket in buckets.iter_mut() {
            bucket.clear();
        }
        buckets.resize_with(nbuckets, Vec::new);
        buckets.truncate(nbuckets);
        for bucket in buckets.iter_mut() {
            bucket.reserve(expected.saturating_sub(bucket.capacity()));
        }

        let mut window = [0u32; DECODE_WINDOW];
        let mut cursor = mask.cursor();
        loop {
            let decoded = cursor.read_many(&mut window);
            if decoded == 0 {
                break;
            }
            for &entity in &window[..decoded] {
                // `get` rather than an index: an entity at or above `bound` has no row *here* and
                // is skipped, which is the same answer `row_of` gives and is not an error — it is
                // ordinarily an entity living in a different segment.
                if let Some(&row) = slots.get(entity as usize) {
                    if row != ROW_ABSENT {
                        buckets[(row >> BUCKET_SHIFT) as usize].push(row);
                    }
                }
            }
        }

        let stamp = &mut scratch.stamp;
        stamp.clear();
        stamp.resize((1usize << BUCKET_SHIFT) / 64, 0);
        let mut sink = Sink::new();
        for (index, rows) in buckets.iter().enumerate() {
            if rows.is_empty() {
                continue;
            }
            let base = (index as u32) << BUCKET_SHIFT;
            // Which of the bucket's 64 containers hold anything. One `u64` covers them exactly,
            // which is what lets the emit below skip the empty ones without scanning their words.
            let mut occupied: u64 = 0;
            for &row in rows.iter() {
                let offset = row - base;
                stamp[(offset >> 6) as usize] |= 1u64 << (offset & 63);
                occupied |= 1u64 << (offset >> 16);
            }
            // Emitted and cleared in the same pass, container by container. Clearing *here* rather
            // than in a second loop is worth stating: the container's words are in cache because
            // the popcount just read them, and only the occupied ones are touched at all — so a
            // sparse bucket pays for what it used, while a dense one pays a sequential 8 KB wipe.
            // The alternative that looks frugal — re-walking each bucket's rows and zeroing the word
            // each sits in — is O(rows) rather than O(width), which sounds better and is 250 million
            // scattered writes at 10⁹ against 122 MB of sequential ones. Also not separately
            // measured, and stated as reasoning rather than as a result.
            for (container, words) in stamp.chunks_exact_mut(WORDS).enumerate() {
                if occupied & (1u64 << container) == 0 {
                    continue;
                }
                let cardinality: u32 = words.iter().map(|word| word.count_ones()).sum();
                let key = u16::try_from((base >> 16) + container as u32)
                    .expect("a row below 2^32 has a container key below 2^16");
                sink.push_block(
                    key,
                    cardinality,
                    (&*words).try_into().expect("a bucket is whole containers"),
                );
                words.fill(0);
            }
        }
        sink.finish()
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

/// One view's whole entity→row mapping: the built base permutation, plus the extents flush has
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
    /// `row-entity.u32` for the base, when the view published one — the row→entity direction, for
    /// the filtered viewport's per-tile route ([`crate::row_entity`]). `None` where a view has no
    /// table, in which case [`Self::entity_of`] answers `None` and the caller falls back to the
    /// projecting route rather than to a wrong answer.
    base_inverse: Option<Arc<crate::row_entity::RowToEntity>>,
    /// The same direction for the rows *above* the base, derived from the extents' own mappings on
    /// first use and thrown away whenever an extent is added. Materialised rather than searched
    /// because the caller is a per-row path; bounded by the flushed tail, which the merge ladder
    /// bounds in turn.
    extent_inverse: std::sync::OnceLock<Vec<u32>>,
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
            base_inverse: None,
            extent_inverse: std::sync::OnceLock::new(),
            base_rows,
            extents: Vec::new(),
            total_rows: base_rows as u64,
        }
    }

    /// This row space with the base's `row-entity.u32` attached.
    ///
    /// Additive rather than a constructor parameter: a row space is correct without it — every
    /// caller that only crosses entity→row is unaffected — and the table is an optimisation for
    /// the one path that crosses the other way.
    pub fn with_row_entity(mut self, table: Arc<crate::row_entity::RowToEntity>) -> Self {
        self.base_inverse = Some(table);
        self
    }

    /// Can this row space cross row→entity at all?
    ///
    /// A caller choosing between the per-tile and projecting routes asks this once, before
    /// committing to a route, rather than discovering row by row that [`Self::entity_of`] cannot
    /// answer. True when the base published a `row-entity.u32` — or when there is no base to
    /// invert, every row belonging to an extent, whose mapping is always recoverable.
    pub fn can_invert(&self) -> bool {
        self.base_inverse.is_some() || self.base_rows == 0
    }

    /// The entity occupying `row`, or `None` when this row space cannot answer — either `row` is
    /// out of range, or the view published no `row-entity.u32` and the base cannot be inverted
    /// without one.
    ///
    /// **`None` is "ask another way", not "no entity".** Every row has an entity by construction;
    /// a `None` here means the caller must fall back to the projecting route, and treating it as an
    /// absence would silently drop rows from a filtered viewport.
    pub fn entity_of(&self, row: RowId) -> Option<EntityId> {
        let raw = row.raw();
        if raw >= self.total_rows as u32 {
            return None;
        }
        if raw < self.base_rows {
            return self.base_inverse.as_ref()?.entity_of(row);
        }
        let table = self
            .extent_inverse
            .get_or_init(|| self.build_extent_inverse());
        table
            .get((raw - self.base_rows) as usize)
            .map(|&e| EntityId::new(e as u64))
    }

    /// Invert every extent's `rows` into one flat table covering `[base_rows, total_rows)`.
    ///
    /// Extents are disjoint and their `row_base`s continue row space exactly (`with_extent`
    /// enforces both), so the flat table is dense and each extent writes only its own span.
    fn build_extent_inverse(&self) -> Vec<u32> {
        let span = (self.total_rows - self.base_rows as u64) as usize;
        let mut out = vec![0u32; span];
        for extent in &self.extents {
            for (offset, &row) in extent.rows.iter().enumerate() {
                if row == ROW_ABSENT {
                    continue;
                }
                let absolute = extent.row_base as u64 + row as u64;
                let at = (absolute - self.base_rows as u64) as usize;
                out[at] = (extent.entity_lo + offset as u64) as u32;
            }
        }
        out
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
        // Row ids are `u32` (bundle_format 1), so a view that would cross 2^32 rows must fail
        // here rather than at the first `row_base + slot` that wraps.
        if total_rows > u64::from(u32::MAX) {
            return None;
        }
        let mut extents = self.extents.clone();
        extents.push(extent);
        Some(RowSpace {
            base: Arc::clone(&self.base),
            // The base table survives — the base's rows are exactly what an extent does not touch.
            // The derived tail does not: it covers the extents, and this call changes them.
            base_inverse: self.base_inverse.clone(),
            extent_inverse: std::sync::OnceLock::new(),
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
            base_inverse: self.base_inverse.clone(),
            // Row-count preserving, but not extent-preserving: a merge replaces a run of extents
            // with one, so the tail this derived is stale even though its length is not.
            extent_inverse: std::sync::OnceLock::new(),
            base_rows: self.base_rows,
            extents,
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

    /// Project an entity-space bitmap into this view's row space: the base projection unioned
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

    /// Project into the **base** rows alone, ignoring every extent above them.
    ///
    /// **The artifact row forms' projection, and the asymmetry is deliberate**
    /// (`annotation-write-cycle.md` §4.1). A session's mask must see every row a viewer may see, so
    /// it takes [`Self::project`]; a *shared, deployment-wide* membership form must not have to be
    /// rebuilt every time a flush appends, because at 10⁷ artifacts that rebuild is tens of seconds
    /// and it lands on whichever request arrives next. Restricting the form to base rows is what
    /// makes it survive a flush (an append moves no bit it holds) and a merge (only extent rows
    /// renumber, and it references none) — so it is rebuilt only by the fold, which is the one
    /// operation that renumbers the base.
    ///
    /// The price is that a member ingested since the last fold contributes nothing to its
    /// artifact's masked count. That is fail-closed — the count **understates**, exactly as a
    /// buffered point is invisible until its flush — and typically zero for a clustering, whose
    /// members predate the layer that names them.
    pub fn project_base(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        self.base.project(mask)
    }

    /// [`Self::project_base`], reusing a caller's scratch — see [`Permutation::project_with`].
    pub fn project_base_with(
        &self,
        mask: &croaring::Bitmap,
        scratch: &mut ProjectScratch,
    ) -> croaring::Bitmap {
        self.base.project_with(mask, scratch)
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

    /// Every row this view holds, across the base and every extent.
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
