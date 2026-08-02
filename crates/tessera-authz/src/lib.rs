pub mod dict;
pub mod fragment;
pub mod postings;
mod single_flight;
pub mod tier;

pub use dict::{Dict, DictStreamWriter, DictWriter};
pub use fragment::{
    build_fragment, build_fragment_with_deltas, FragmentCache, FragmentCacheError, FrozenFragment,
};
pub use postings::{
    encode_posting, write_posting_records, write_postings, PostingRef, PostingsReader,
    PostingsSpool,
};
pub use tier::{write_delta_tier, DeltaTier};

/// The on-disk fragment-cache entry format this build reads and writes.
///
/// **Bumped when the cache *key* changes shape, not only when the entry bytes do.** Version 2 is
/// the watermark joining the key (§9): every version-1 entry is unreachable under it — a leak
/// rather than a fail-open, since new code can never read one — and nothing on that path deletes
/// anything, so the version is what lets `FragmentCache::sweep_orphans` tell an orphan from a live
/// entry.
pub const FRAGMENT_FORMAT: u32 = 2;
