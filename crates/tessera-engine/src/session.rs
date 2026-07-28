//! Engine construction and session authorisation (task-11 brief).
//!
//! [`Engine::open`] runs the bundle read protocol, replays the WAL, seeds the I9 allocator, and
//! assembles the first [`Generation`]. [`Engine::authorise`] turns a credential into a
//! [`Session`]: the plugin's granted descriptors are resolved against the bundle dictionary
//! (an unknown descriptor is simply unsatisfied, never an error — the dictionary is the
//! authority on which descriptors exist), and the resulting term set is unioned into a mask
//! fragment via [`FragmentCache`] — this union *is* the authorisation decision (I2).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use arrow::array::{Array, BinaryArray, UInt64Array};
use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use sha2::{Digest, Sha256};

use tessera_authz::{Dict, FragmentCache, FrozenFragment, PostingsReader};
use tessera_lifecycle::alloc::{high_water_from, Allocator};
use tessera_lifecycle::overlay::replay;
use tessera_lifecycle::wal::{Wal, WalError};
use tessera_lifecycle::OverlayError;
use tessera_plugin::{Plugin, PluginError};
use tessera_store::manifest::CurrentPointer;
use tessera_store::read::open_bundle;
use tessera_store::StoreError;
use tessera_types::{EntityId, TermId};

use crate::{Generation, GenerationHandle};

/// Engine-wide configuration (SA §7's `[disclosure]`/`[serve]` sections, the subset this task
/// needs). Task 13 owns parsing `tessera.toml`; this is just the shape [`Engine::open`] consumes.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// How long a freshly minted session token remains valid, in seconds.
    pub token_max_lifetime_secs: u64,
    /// The hard cap on a viewport request's `k` (Reference Sheet R1: default 30, cap `max_k`
    /// 200). [`Engine::viewport`] clamps to this defensively even though Task 13's server is
    /// expected to enforce it at the HTTP boundary too.
    pub max_k: usize,
}

/// One authorised viewer session: the credential's granted term set and the mask fragment it
/// unions to (I2 — the fragment *is* the authorisation decision, computed once here and reused,
/// never recomputed per viewport).
///
/// `handles` (a per-session entity-ID → wire-`Handle` table) is deliberately absent: that state
/// is owned by `tessera-wire` (Task 12), which must not gain a dependency on this crate's
/// `EntityId` (I10) any more than this crate should depend on `tessera-wire` — see this module's
/// doc note in the task report. `tessera-server` (Task 13) holds a session's handle table
/// alongside, not inside, this struct.
pub struct Session {
    /// Bearer token: 32 random bytes, hex-encoded.
    pub token: String,
    /// A process-local identity for this session, distinct from `token` — used as (part of) the
    /// row-projection cache key (`(token_id, slice, segments_version)`, shared-context
    /// constraint 8) so the cache never has to hash or compare the full token string.
    pub token_id: u64,
    /// The credential's granted terms, resolved to bundle-relative `TermId`s. An unknown
    /// descriptor (no dictionary entry) is simply absent here — never an error.
    pub satisfied: FxHashSet<TermId>,
    /// The materialised mask fragment (I2): the union of every satisfied term's postings.
    pub fragment: Arc<FrozenFragment>,
    /// Unix timestamp (seconds) after which this session is no longer valid.
    pub expires_at: u64,
}

/// Engine-level failures. Every variant here is fail-closed (Global Constraint 3): none of them
/// hand back a partial or best-effort result.
#[derive(Debug)]
pub enum EngineError {
    Store(StoreError),
    Wal(WalError),
    Overlay(OverlayError),
    Plugin(PluginError),
    Io(io::Error),
    /// A presented pin's `(prefix, segments_version)` does not match the live generation (I11) —
    /// maps to HTTP 410 at the server boundary.
    PinExpired,
    /// A viewport request named a slice this bundle doesn't have.
    UnknownSlice(String),
    /// A slice with more than one segment. `tile_ranges` returns **segment-local** row indices
    /// (Reference Sheet R1), while the slice's mask is built from one `Permutation` addressing
    /// exactly one segment's row space (Phase 1's build always produces exactly one segment per
    /// (partition, slice) — R4). Summing `count_range`/`iter_range` over a second segment's
    /// ranges through that same row space would silently mis-count or mis-index rows belonging to
    /// a different segment; there is no segment-row offset table to fold them together correctly
    /// yet, so this fails closed rather than produce a wrong (not even necessarily *obviously*
    /// wrong) answer.
    MultiSegmentSlice(String),
    /// A bundle-level file (`CURRENT`, a plugin hash) was not the shape this engine expects.
    Malformed(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Store(e) => write!(f, "store error: {e}"),
            EngineError::Wal(e) => write!(f, "wal error: {e}"),
            EngineError::Overlay(e) => write!(f, "overlay error: {e}"),
            EngineError::Plugin(e) => write!(f, "plugin error: {e}"),
            EngineError::Io(e) => write!(f, "io error: {e}"),
            EngineError::PinExpired => write!(f, "pin expired"),
            EngineError::UnknownSlice(slice) => write!(f, "unknown slice '{slice}'"),
            EngineError::MultiSegmentSlice(slice) => write!(
                f,
                "slice '{slice}' has more than one segment, which this engine's row-space \
                 handling does not yet support (see EngineError::MultiSegmentSlice's doc)"
            ),
            EngineError::Malformed(detail) => write!(f, "malformed: {detail}"),
        }
    }
}

impl std::error::Error for EngineError {}

pub type Result<T> = std::result::Result<T, EngineError>;

/// The request-serving engine: one immutable [`Generation`] behind an atomically-swappable
/// pointer, plus the process-lifetime state that doesn't change on an overlay/bundle swap (the
/// dictionary, the postings reader, the fragment cache).
pub struct Engine {
    pub(crate) generation: GenerationHandle,
    pub(crate) plugin: Arc<dyn Plugin>,
    pub(crate) dict: Arc<Dict>,
    pub(crate) postings: Arc<PostingsReader>,
    pub(crate) fragment_cache: Arc<FragmentCache>,
    /// Cached row-space projections, keyed `(token_id, slice, segments_version)` — never
    /// recomputed on the per-viewport path (shared-context constraint 8; see
    /// `crate::compose::RowProjection`'s doc for the cost this avoids).
    #[allow(clippy::type_complexity)]
    pub(crate) row_projection_cache:
        Mutex<FxHashMap<(u64, String, u64), Arc<crate::compose::RowProjection>>>,
    pub(crate) config: EngineConfig,
    next_token_id: AtomicU64,
    /// The write-ahead log handle, kept open for future ingest/change acceptance (Task 13); not
    /// exercised by this task's authorise/viewport paths.
    wal: Mutex<Wal>,
    /// The I9 allocator, seeded at open (`max(manifest high-water, WAL high-water)`); not
    /// exercised by this task's authorise/viewport paths, but seeding it here — rather than
    /// leaving it to whichever task first needs it — is what the brief asks `Engine::open` to do.
    allocator: Mutex<Allocator>,
}

impl Engine {
    /// Open the bundle at `bundle_root`, replay the WAL at `wal_path`, seed the I9 allocator, and
    /// build the first [`Generation`]. `cache_dir` is the engine-local (never in-bundle) fragment
    /// cache directory (Reference Sheet R1).
    pub fn open(
        bundle_root: &Path,
        cache_dir: &Path,
        wal_path: &Path,
        plugin: impl Plugin + 'static,
        config: EngineConfig,
    ) -> Result<Engine> {
        let bundle = open_bundle(bundle_root).map_err(EngineError::Store)?;

        let current = read_current(bundle_root)?;
        let prefix = current.prefix.clone();
        let bundle_identity = hex_decode_32(&current.manifest_digest).ok_or_else(|| {
            EngineError::Malformed(format!(
                "CURRENT manifest_digest '{}' is not 64 hex characters",
                current.manifest_digest
            ))
        })?;

        let prefix_dir = bundle_root.join(&prefix);

        // Phase 1 has exactly one partition (no compartments — scope constraint 11); take
        // whichever one is present rather than hard-coding its phash.
        let (phash, partition) = bundle
            .partitions
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v))
            .ok_or_else(|| EngineError::Malformed("bundle has no partitions".to_string()))?;

        let segments_version = partition.manifest.segments_version;
        let watermark = partition.manifest.watermark;

        let dict_paths: Vec<PathBuf> = partition
            .manifest
            .dict_extents
            .iter()
            .map(|e| prefix_dir.join(&e.path))
            .collect();
        let dict = Arc::new(Dict::load(&dict_paths).map_err(EngineError::Io)?);

        let postings_path = prefix_dir
            .join("partitions")
            .join(&phash)
            .join("terms")
            .join("postings.arrow");
        // Mmap-backed: the engine holds this reader for the process lifetime, so paying the
        // mmap setup cost once at open (rather than reading the whole file into memory) is the
        // right trade — see `PostingsReader::open`'s doc.
        let postings =
            Arc::new(PostingsReader::open(&postings_path, true).map_err(EngineError::Io)?);

        let external_id_paths: Vec<PathBuf> = partition
            .manifest
            .external_id_extents
            .iter()
            .map(|p| prefix_dir.join(p))
            .collect();
        let external_index = ExternalIdIndex::load(&external_id_paths).map_err(EngineError::Io)?;

        let (wal, records) = Wal::open(wal_path).map_err(EngineError::Wal)?;
        let high_water = bundle
            .manifest
            .entity_id_high_water
            .max(high_water_from(&records));
        let allocator = Allocator::new(high_water);

        let (overlay, buffer) = replay(&records, &dict, |external_id| {
            external_index.resolve(external_id)
        })
        .map_err(EngineError::Overlay)?;

        let plugin: Arc<dyn Plugin> = Arc::new(plugin);
        let auth_plugin_hash = hex_decode_32(&plugin.auth_plugin_hash()).ok_or_else(|| {
            EngineError::Malformed("plugin auth_plugin_hash is not 64 hex characters".to_string())
        })?;

        let fragment_cache = Arc::new(FragmentCache::new(
            cache_dir,
            bundle_identity,
            auth_plugin_hash,
        ));

        let generation = Generation {
            prefix,
            segments_version,
            watermark,
            bundle: Arc::new(bundle),
            overlay_version: 0,
            overlay: Arc::new(overlay),
            buffer: Arc::new(buffer),
        };

        Ok(Engine {
            generation: ArcSwap::new(Arc::new(generation)),
            plugin,
            dict,
            postings,
            fragment_cache,
            row_projection_cache: Mutex::new(FxHashMap::default()),
            config,
            next_token_id: AtomicU64::new(0),
            wal: Mutex::new(wal),
            allocator: Mutex::new(allocator),
        })
    }

    /// Authorise a credential: `plugin.terms_of_auth` → dictionary lookup (unknown descriptors
    /// simply drop out, never an error) → `FragmentCache::get_or_build`. A zero-term credential
    /// (or one whose every descriptor is unknown) is a valid, zero-visibility session (R5) — not
    /// an error.
    pub fn authorise(&self, auth_data: &[u8]) -> Result<Session> {
        let auth_terms = self
            .plugin
            .terms_of_auth(auth_data)
            .map_err(EngineError::Plugin)?;

        let satisfied: FxHashSet<TermId> = auth_terms
            .terms
            .iter()
            .filter_map(|descriptor| self.dict.lookup(descriptor))
            .collect();

        let mut satisfied_sorted: Vec<TermId> = satisfied.iter().copied().collect();
        satisfied_sorted.sort_unstable();

        // The cache's caller obligation (`FragmentCache::get_or_build`'s doc): this hash must be
        // a function of the exact `auth_data` that produced `satisfied` above, which it is.
        let auth_data_hash: [u8; 32] = Sha256::digest(auth_data).into();

        let generation = self.generation.load();
        let fragment = self
            .fragment_cache
            .get_or_build(
                &satisfied_sorted,
                auth_data_hash,
                &self.postings,
                generation.watermark,
            )
            .map_err(EngineError::Io)?;

        let mut token_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut token_bytes);
        let token = hex_encode(&token_bytes);

        let token_id = self.next_token_id.fetch_add(1, Ordering::Relaxed);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_secs();
        let expires_at = now + self.config.token_max_lifetime_secs;

        Ok(Session {
            token,
            token_id,
            satisfied,
            fragment,
            expires_at,
        })
    }

    /// The I9 allocator's current high-water mark — exposed for tests/diagnostics confirming
    /// `Engine::open`'s seeding rule (`max(manifest high-water, WAL high-water)`); not otherwise
    /// used by this task's request paths.
    pub fn allocator_high_water(&self) -> u64 {
        self.allocator.lock().unwrap().high_water()
    }

    /// Access to the open WAL handle, for future ingest/change acceptance (Task 13); not used by
    /// this task's request paths.
    pub fn wal(&self) -> &Mutex<Wal> {
        &self.wal
    }
}

fn read_current(root: &Path) -> Result<CurrentPointer> {
    let bytes = std::fs::read(root.join("CURRENT")).map_err(EngineError::Io)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| EngineError::Malformed(format!("CURRENT is not valid JSON: {e}")))
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A sorted `(external_id, entity_id)` index over the bundle's
/// `entities/external-ids-0.arrow` extent(s) (contracts §2.1) — the `resolve_from_bundle` seam
/// `tessera_lifecycle::overlay::replay` left open (Task 10's report flags this as the one thing
/// left to wire in). Built once at `Engine::open`, never on a per-request path: at Phase 1 scales
/// (up to 2.4M items validated, 10⁹ deferred to Task 16) a one-time linear read plus sort is
/// cheap relative to the WAL replay it feeds.
struct ExternalIdIndex {
    ids: Vec<Vec<u8>>,
    entities: Vec<u64>,
}

impl ExternalIdIndex {
    fn load(paths: &[PathBuf]) -> io::Result<Self> {
        let mut ids: Vec<Vec<u8>> = Vec::new();
        let mut entities: Vec<u64> = Vec::new();

        for path in paths {
            let file = File::open(path)?;
            let reader = arrow::ipc::reader::FileReader::try_new(file, None)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            for batch in reader {
                let batch =
                    batch.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                let ext_col = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "external-ids extent: column 0 is not Binary",
                        )
                    })?;
                let ent_col = batch
                    .column(1)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "external-ids extent: column 1 is not UInt64",
                        )
                    })?;
                for i in 0..batch.num_rows() {
                    ids.push(ext_col.value(i).to_vec());
                    entities.push(ent_col.value(i));
                }
            }
        }

        // Each extent is individually sorted by external-id bytes (R4); re-sorting once across
        // their concatenation makes the loader correct even for more than one extent, though
        // Phase 1 always has exactly one.
        let mut order: Vec<usize> = (0..ids.len()).collect();
        order.sort_by(|&a, &b| ids[a].cmp(&ids[b]));
        let sorted_ids: Vec<Vec<u8>> = order.iter().map(|&i| ids[i].clone()).collect();
        let sorted_entities: Vec<u64> = order.iter().map(|&i| entities[i]).collect();

        Ok(ExternalIdIndex {
            ids: sorted_ids,
            entities: sorted_entities,
        })
    }

    fn resolve(&self, external_id: &[u8]) -> Option<EntityId> {
        self.ids
            .binary_search_by(|probe| probe.as_slice().cmp(external_id))
            .ok()
            .map(|idx| EntityId::new(self.entities[idx]))
    }
}
