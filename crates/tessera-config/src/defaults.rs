//! What each key takes when the file does not name it.

/// The machine ceiling on a viewport's `k`: GPU, transport and handle table.
pub const DEFAULT_MAX_K: usize = 1_000;

/// The fewest marks a non-empty tile draws.
pub const DEFAULT_K_MIN: usize = 2;

/// The most marks any one tile draws: an overplot ceiling, below the machine one.
pub const DEFAULT_K_MAX_MARKS: usize = 500;

const _: () = assert!(DEFAULT_MAX_K >= DEFAULT_K_MAX_MARKS);

/// The marks the mean occupied tile should draw at any depth.
pub const DEFAULT_THETA_TARGET_MARKS: u64 = 16;

pub const DEFAULT_VISIBLE_WAIT_MAX_SECS: u64 = 30;

pub const DEFAULT_MAX_UNDERLAY_OFFSET: u8 = 4;

pub const DEFAULT_MAX_UNDERLAY_CELLS: usize = 8192;

/// At this many tiles one request's tile vector is at most 4 MB.
pub const DEFAULT_MAX_TILES_PER_REQUEST: usize = 262_144;

pub const DEFAULT_MAX_CATEGORY_VALUES: usize = 1_000;

pub const DEFAULT_MAX_SUGGESTIONS: usize = 20;

pub const DEFAULT_MAX_SUGGESTION_WALK: u64 = 100_000;

pub const DEFAULT_MAX_SUGGEST_SET_ENTITIES: u64 = 10_000_000;

pub const DEFAULT_MAX_BROWSE_ROWS: usize = 200;

pub const DEFAULT_MAX_REGION_VERTICES: u64 = 10_000;

pub const DEFAULT_REGION_CACHE_BYTES: u64 = 256 * 1024 * 1024;

pub const DEFAULT_ADMISSION_TIMEOUT_MS: u64 = 250;

pub const DEFAULT_STREAM_FLUSH_BYTES: usize = 1 << 20;

pub const DEFAULT_STREAM_WRITE_STALL_MS: u64 = 10_000;

pub const DEFAULT_STREAM_DEADLINE_MS: u64 = 60_000;

/// Rows per page of a bulk read.
pub const DEFAULT_MAX_PAGE_ROWS: u32 = 100_000;

/// Arrow bytes per page of a bulk read, before compression.
pub const DEFAULT_MAX_PAGE_BYTES: usize = 64 * 1024 * 1024;

/// The most `max_page_bytes` may be: a page is one frame, whose length is 32 bits, and a row
/// larger than the ceiling is sent alone past it.
pub const MAX_PAGE_BYTES_CEILING: usize = 1 << 31;

/// Bulk reads running at once. `0` refuses every bulk read with a 429.
pub const DEFAULT_BULK_ADMISSION: usize = 2;

/// Bytes one bulk-read response may carry.
pub const DEFAULT_BULK_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// Time one bulk-read response may run.
pub const DEFAULT_BULK_RESPONSE_MS: u64 = 30_000;

/// What one bulk read holds, in pages of `max_page_bytes`: about four while a page is built
/// (measured 4.05 with rows of very uneven size), and three encoded pages: two in the body's
/// channel and one being written by the HTTP layer.
pub const BULK_READ_PAGES_HELD: usize = 7;

/// `compute_admission`'s default per compute thread; small requests wait on scheduling, not CPU.
pub const COMPUTE_ADMISSION_MULTIPLIER: usize = 4;

/// Equal to [`DEFAULT_INGEST_MAX_BATCH_ROWS`], so one maximal batch is one maximal window.
pub const DEFAULT_COMMIT_WINDOW_MAX_ITEMS: usize = 10_000;

/// Below [`DEFAULT_INGEST_ADMISSION`] so the 429 can fire: an admitted handler holds one entry.
pub const DEFAULT_INGEST_QUEUE_BOUND: usize = 32;

pub const DEFAULT_INGEST_ADMISSION: usize = 64;

/// Blocking threads beyond both admission bounds, for the deny lane's fallback and tokio's own.
pub const BLOCKING_THREAD_RESERVE: usize = 32;

pub const DEFAULT_INGEST_MAX_BATCH_ROWS: usize = 10_000;

pub const DEFAULT_INGEST_MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;

pub const DEFAULT_PUBLISH_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

pub const DEFAULT_MAX_ARTIFACTS_PER_REQUEST: usize = 10_000;

/// Above what [`DEFAULT_PUBLISH_MAX_BODY_BYTES`] carries, so the byte cap is met first.
pub const DEFAULT_MAX_MEMBERS_PER_REQUEST: usize = 5_000_000;

pub const DEFAULT_MAX_EXCLUDED_PER_REQUEST: usize = 1_000_000;

/// The overlay depth that raises an alarm, and the default deletion count that dispatches a fold.
pub const DEFAULT_OVERLAY_SOFT_LIMIT: usize = 500_000;

pub const DEFAULT_FLUSH_MAX_ITEMS: usize = 4 * DEFAULT_COMMIT_WINDOW_MAX_ITEMS;

pub const DEFAULT_FLUSH_MAX_AGE_SECS: u64 = 90;

pub const DEFAULT_COMPACTION_MIN_INTERVAL_SECS: u64 = 86_400;

pub const DEFAULT_COMPACTION_WINDOW_START_SECS: u32 = 0;

pub const DEFAULT_COMPACTION_WINDOW_SECS: u32 = 4 * 3_600;

pub const DEFAULT_COMPACTION_WINDOW_MIN_SEGMENTS: usize = 8;

pub const DEFAULT_COMPACTION_MAX_SEGMENTS: u64 = 64;

pub const DEFAULT_COMPACTION_DEAD_ROWS_FRACTION: f64 = 0.2;

pub const DEFAULT_COMPACTION_DEAD_BYTES_RATIO: f64 = 1.0;

pub const DEFAULT_INGEST_BUFFER_MAX_ITEMS: usize = 1_000_000;

pub const DEFAULT_SEGMENT_FLOOR_BYTES: u64 = 16 * 1024 * 1024;

pub const DEFAULT_TIER_WIDTH: usize = 4;

pub const DEFAULT_COALESCE_WIDTH: usize = 8;

pub const DEFAULT_ROW_PROJECTION_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub const DEFAULT_MASKED_COUNT_CACHE_BYTES: u64 = 256 * 1024 * 1024;

pub const DEFAULT_FRAGMENT_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
