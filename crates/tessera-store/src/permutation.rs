//! `Permutation`: the **only** legal EntityId→RowId path in the codebase (invariant I4).
//! Backed by `permutation.bin` (R4): `"TSPM"` ‖ `u16 version` ‖ `u16 reserved` ‖ `u64 bound` ‖
//! `bound` little-endian `u32` slots, sentinel `0xFFFF_FFFF` for an entity never assigned a row
//! in this segment.

use std::fs::File;
use std::path::{Path, PathBuf};

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
    /// **Parallel (task 7), executor-agnostic.** This crate owns no `rayon::ThreadPool` of its
    /// own — `par_chunks`/`par_sort_unstable` below run on whatever pool the caller has
    /// `install`ed (the engine wraps its cold-session build in `self.pool.install(..)`, D-D), or
    /// on rayon's own global pool if nobody has. Chunk boundaries never change the result — each
    /// chunk's matches are independent of every other chunk's, and are simply concatenated and
    /// then sorted — only throughput does, so this stays correct under any thread count,
    /// including 1 (see the `--ignored` gate test and the correctness tests in
    /// `tests/permutation_project_parallel.rs`, both of which run this at several thread counts).
    ///
    /// **Transient memory (brief's constraint):** `entities` and `rows` are both live at once, so
    /// this call's peak transient memory is roughly double `mask.cardinality()` `u32`s, on top of
    /// the mmap-backed `slots()` array (never copied — `slots` is only ever borrowed, read
    /// concurrently by every chunk).
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
                chunk
                    .iter()
                    .filter_map(|&entity| slots.get(entity as usize).copied())
                    .filter(|&slot| slot != ROW_ABSENT)
                    .collect()
            })
            .collect();
        let mut rows: Vec<u32> = per_chunk.concat();

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
