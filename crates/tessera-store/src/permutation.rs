//! `Permutation`: the **only** legal EntityId→RowId path in the codebase (invariant I4).
//! Backed by `permutation.bin` (R4): `"TSPM"` ‖ `u16 version` ‖ `u16 reserved` ‖ `u64 bound` ‖
//! `bound` little-endian `u32` slots, sentinel `0xFFFF_FFFF` for an entity never assigned a row
//! in this segment.

use std::fs::File;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

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

        let expected_len = (bound as usize)
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
            path: path.to_path_buf(),
        })
    }

    /// The number of entity-ID slots this permutation covers, `[0, bound)`.
    pub fn bound(&self) -> u64 {
        self.bound
    }

    fn slots(&self) -> &[u32] {
        let bytes = &self.mmap[HEADER_LEN..];
        debug_assert_eq!(bytes.len(), self.bound as usize * 4);
        // SAFETY: `bytes` starts at a fixed offset (HEADER_LEN = 16) into a page-aligned mmap
        // base, and 16 is a multiple of 4, so `bytes.as_ptr()` is 4-byte aligned regardless of
        // file content — no adversarial input can misalign this cast. Length is exactly
        // `bound * 4` bytes, checked once at `load`.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u32, self.bound as usize) }
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
    pub fn project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        let slots = self.slots();
        let mut rows: Vec<u32> = Vec::with_capacity(mask.cardinality() as usize);
        for entity in mask.iter() {
            if let Some(&slot) = slots.get(entity as usize) {
                if slot != ROW_ABSENT {
                    rows.push(slot);
                }
            }
        }
        rows.sort_unstable();
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
