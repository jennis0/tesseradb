//! Engine construction and session authorisation (task-11 brief).
//!
//! [`Engine::open`] runs the bundle read protocol, replays the WAL, seeds the I9 allocator, and
//! assembles the first [`Generation`]. [`Engine::authorise`] turns a credential into a
//! [`Session`]: the plugin's granted descriptors are resolved against the bundle dictionary
//! (an unknown descriptor is simply unsatisfied, never an error — the dictionary is the
//! authority on which descriptors exist), and the resulting term set is unioned into a mask
//! fragment via [`FragmentCache`] — this union *is* the authorisation decision (I2).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use sha2::{Digest, Sha256};

use tessera_authz::{Dict, FragmentCache, FrozenFragment, PostingsReader};
use tessera_lifecycle::alloc::{high_water_from, Allocator};
use tessera_lifecycle::buffer::DescriptorResolver;
use tessera_lifecycle::overlay::replay;
use tessera_lifecycle::wal::{ChangeOp, Wal, WalError, WalRecord, WalRow};
use tessera_lifecycle::{alloc::PendingItem, assign_sorted, Overlay, OverlayError};
use tessera_plugin::{Descriptor, Plugin, PluginError};
use tessera_store::manifest::CurrentPointer;
use tessera_store::read::open_bundle;
use tessera_store::StoreError;
use tessera_types::{EntityId, IdentityError, IdentityKey, TermId, TesseraId};

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
    ///
    /// **This is the MACHINE ceiling** — GPU, transport, handle table — and it is the number the
    /// drawn-mark budget spec's probes calibrate. It is deliberately *not* the same knob as
    /// [`Self::k_max_marks`]: conflating them would mean that raising this on transport evidence
    /// silently dissolved §7.2's cap clause and the per-tile work bound with it.
    pub max_k: usize,
    /// §7.2's floor clause, `k_min`: the minimum marks a non-empty tile draws, whatever the
    /// threshold says. **This is the I7 guarantee** — it is what stops the sparsest principals'
    /// maps going blank — and it may not be removed as an optimisation. Provisional value 2 (density
    /// memo §4), pending that memo's §0 visual experiments.
    ///
    /// **Must be at least 1.** At 0 the floor clause is switched off and a tile whose visible items
    /// all sit above θ serves nothing — I7 gone, silently. `tessera-server`'s config loader refuses
    /// to start on `k_min = 0` (`ConfigError::FloorClauseDisabled`) rather than clamping, so no
    /// `tessera.toml` can reach that state; an embedder constructing this struct directly is on its
    /// own honour, which is why the constraint is stated here rather than only in the loader.
    pub k_min: usize,
    /// §7.2's cap clause, `K_max`: the most marks any one tile draws.
    ///
    /// **This is the OVERPLOT ceiling**, not the machine ceiling — density memo §4 sizes it at 128
    /// from ink coverage at ~80x80 px per tile, explicitly "overplot-bound, not machine-bound". A
    /// client may request `k <= k_max_marks`; the effective cap is the smaller. Provisional,
    /// pending the memo's §0 visual experiments.
    pub k_max_marks: usize,
    /// θ's anchor target: the number of marks the *mean occupied tile* should draw at any depth.
    /// θ_0 is derived as `theta_target_marks * 2^64 / V_total` and progresses `x4` per depth, which
    /// is what makes the per-tile expectation depth-stable. Provisional value 16 (density memo §4).
    ///
    /// Raising this above a session's total visible count saturates θ, which turns selection into
    /// "serve every visible row up to the cap" — the configuration tests use when they mean to
    /// assert masking rather than density.
    pub theta_target_marks: u64,
    /// The largest `underlay_offset` a request may ask for (§3.3 sub-cell counts). The sub-cell
    /// depth is `zoom + offset`, clamped to 16.
    pub max_underlay_offset: u8,
    /// The hard ceiling on sub-cells in one response. `tiles_for_bbox` is itself uncapped, and the
    /// underlay multiplies its output by `4^offset`, so without this a single request can ask for
    /// ~77k `count_range` calls and blow the 10 ms p99 latency gate.
    pub max_underlay_cells: usize,
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
    Overlay(OverlayError<StoreError>),
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
    /// A `/v1/viewport` request asked for a §3.3 underlay this engine will not serve.
    ///
    /// **Rejected, never clamped** — and that is one rule for all three bounds (config offset, the
    /// depth-16 grid limit, and the total cell budget), deliberately. A Morton prefix carries no
    /// depth of its own, so a silently-reduced offset would hand the client cells it cannot
    /// interpret; rejecting means the depth is always `zoom + offset` from the caller's own request.
    UnderlayRefused(String),
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
            EngineError::UnderlayRefused(detail) => write!(f, "underlay refused: {detail}"),
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
    /// External ids established by the bundle's own extent, at open — immutable for the process
    /// lifetime (Task 11).
    external_index: ExternalIdIndex,
    /// External ids established live (bundle replay's `IngestBatch` rows, plus every
    /// subsequently-accepted `/control/ingest` batch) — consulted before falling back to
    /// `external_index`, so a `/control/changes` naming an item ingested only seconds ago (not
    /// yet in any bundle) still resolves (Task 13).
    established: Mutex<FxHashMap<Vec<u8>, EntityId>>,
    /// The inverse of `established` — `entity -> external_id` — for the drill-down direction
    /// (Important I-9). Written by the same two writers as `established` (`Engine::open`'s
    /// replay and `Engine::accept_ingest`), in the same critical section each time, so the two
    /// maps can never disagree about the same item (task-9 brief).
    established_inverse: Mutex<FxHashMap<EntityId, Vec<u8>>>,
    /// The `tessera_id` blinding permutation's per-deployment key (contracts §2.6 r6, design
    /// memo `docs/design-memos/2026-07-30-tessera-id-construction.md`) — parsed once at open from
    /// MANIFEST's `identity.key` and held for the process lifetime. Never leaves the server (I10).
    /// `IdentityKey`'s `Debug` is redacted and it has no hex accessor, so *this* field cannot be
    /// logged; the plaintext hex carried beside it in MANIFEST is redacted at its own carriers
    /// (`IdentityDescriptor`'s and `BuildArgs`' hand-written `Debug` impls print a fingerprint) —
    /// stated precisely because "the key is never logged" is a property of every carrier, not of
    /// this type alone. `pub(crate)`: `viewport.rs`'s
    /// `Engine::item` inverts a caller-supplied `tessera_id` with it directly.
    pub(crate) identity_key: IdentityKey,
    /// The descriptor resolver's extension state (dictionary-miss descriptors interned in
    /// replay/accept order), detached from replay's borrow of `dict` and resumed on every live
    /// resolution — see `DescriptorResolver::resume`'s doc (Task 13).
    resolver_state: Mutex<(FxHashMap<Vec<u8>, TermId>, u32)>,
    /// `/control/ingest` idempotency index: accepted batch id -> `(body hash, entity ids)` it was
    /// accepted with (Task 13). The entity ids ride along so a byte-identical replay can answer
    /// with the same `tessera_id`s per row (contracts §3.4 r6) without needing to re-resolve them
    /// from `external_id` — which a null-external-id row has none of.
    #[allow(clippy::type_complexity)]
    accepted_batches: Mutex<FxHashMap<String, ([u8; 32], Vec<EntityId>)>>,
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

        // The sidecar is lazy for real (Task 8): nothing here is opened, mapped or verified —
        // `ExternalIdSidecar::deferred_from_manifest` only reads already-parsed JSON manifest
        // data (paths and digests), never the filesystem. No extent descriptor, digest, ordinal
        // or file path is handed to this crate — the constructor takes the manifests and the
        // prefix directory and keeps everything else behind its own API (Ruling B).
        let external_index =
            ExternalIdIndex::open(&bundle.manifest, &partition.manifest, &prefix_dir)
                .map_err(EngineError::Store)?;

        // Contracts §2.6 r6: the deployment's `tessera_id` key, parsed once here and held for
        // the process lifetime. `IdentityKey::from_hex` also rejects a degenerate key — a bundle
        // this engine would otherwise open is refused rather than silently blinding identities
        // with a collapsed round schedule.
        let identity_key = IdentityKey::from_hex(&bundle.manifest.identity.key)
            .map_err(|e| EngineError::Malformed(format!("MANIFEST identity.key: {e}")))?;

        let (wal, records) = Wal::open(wal_path).map_err(EngineError::Wal)?;

        let high_water = bundle
            .manifest
            .entity_id_high_water
            .max(high_water_from(&records));
        // `try_new`, not `new`: the seed comes from durable state this process did not write in
        // this run (MANIFEST's `entity_id_high_water`, or a replayed WAL row/lease), so a
        // corrupt or hand-edited value at or above `u32::MAX` must be refused **here**, before
        // any ingest, rather than surfacing later as an opaque exhaustion error on whichever
        // request happened to allocate first. This is the check `Allocator::try_new`'s own doc
        // says "belongs at open" — open is this function.
        let allocator = Allocator::try_new(high_water).map_err(|e| {
            EngineError::Malformed(format!(
                "entity-ID allocator seed from durable state (MANIFEST high-water {}, WAL \
                 high-water {}): {e}",
                bundle.manifest.entity_id_high_water,
                high_water_from(&records),
            ))
        })?;

        // **C3 closed (review round 4, Critical)**: `resolve_from_bundle` propagates a real
        // sidecar failure through `replay` as `Err`, rather than the closure panicking on it —
        // `ExternalIdIndex::resolve` below is fallible end to end.
        let (overlay, buffer, established, resolver) = replay(&records, &dict, |external_id| {
            external_index.resolve(external_id)
        })
        .map_err(EngineError::Overlay)?;

        // `established_inverse` — the drill-down direction (Important I-9) — is the exact
        // inverse of `established`, built once here from the same replay pass; the two are kept
        // in sync from this point on by `Engine::accept_ingest`'s single critical section.
        let established_inverse: FxHashMap<EntityId, Vec<u8>> = established
            .iter()
            .map(|(ext, ent)| (*ent, ext.clone()))
            .collect();
        // Detach the resolver's extension state from `dict`'s borrow immediately (Task 13): the
        // live serving path resumes exactly this state on every future descriptor resolution, so
        // novel-descriptor extension ids keep counting down from wherever replay left off, rather
        // than restarting and colliding with ids already handed out earlier in this process's
        // lifetime (see `DescriptorResolver::resume`'s doc).
        let resolver_state = resolver.into_state();

        // The idempotency index for `/control/ingest` (Task 13): every previously-accepted batch
        // id, mapped to the body hash it was accepted with plus the entity ids that batch's rows
        // were assigned, so a retried request with the same id and body is recognised as a no-op
        // 200 rather than re-applied, and can still answer with the same `tessera_id`s (contracts
        // §3.4 r6) even for a row that carried no external id to re-resolve from.
        let mut accepted_batches: FxHashMap<String, ([u8; 32], Vec<EntityId>)> =
            FxHashMap::default();
        for record in &records {
            if let WalRecord::IngestBatch {
                batch_id,
                body_hash,
                rows,
            } = record
            {
                let entity_ids = rows.iter().map(|row| row.entity_id).collect();
                accepted_batches.insert(batch_id.clone(), (*body_hash, entity_ids));
            }
        }

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
            external_index,
            established: Mutex::new(established),
            established_inverse: Mutex::new(established_inverse),
            identity_key,
            resolver_state: Mutex::new(resolver_state),
            accepted_batches: Mutex::new(accepted_batches),
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

    /// The number of cached row-space projections currently held — exposed for tests confirming
    /// `Engine::item`'s entity-space visibility test never constructs one (Critical C-5: this
    /// must stay `0` across drill-down calls, warm or cold, unlike `Engine::viewport`'s path,
    /// which populates this cache deliberately).
    pub fn row_projection_cache_len(&self) -> usize {
        self.row_projection_cache.lock().unwrap().len()
    }

    /// Whether the external-id sidecar has opened any extent (or its locator) yet — exposed for
    /// tests confirming `Engine::open` never touches it (Task 8's per-extent laziness
    /// guarantee).
    pub fn external_id_sidecar_is_open(&self) -> bool {
        self.external_index.0.is_open()
    }

    /// The plugin this engine was opened with — Task 13's `/control/ingest` handler calls
    /// `terms_of_label` through this to turn an item's `access` bytes into descriptors.
    pub fn plugin(&self) -> &Arc<dyn Plugin> {
        &self.plugin
    }

    /// The plugin's declared sizing bounds (R6) — Task 13's ingest handler consults these to
    /// decide `over_bound`, never to exclude an item (bounds warn, never exclude — design §6.2
    /// r16).
    pub fn declared_bounds(&self) -> tessera_plugin::DeclaredBounds {
        self.plugin.declared_bounds()
    }

    /// Resolve raw term descriptors to `TermId`s: a dictionary hit resolves to its durable,
    /// bundle-relative id; a miss is interned into the process-lifetime extension state, resumed
    /// from wherever WAL replay (or the previous call to this method) left off — see
    /// `DescriptorResolver::resume`'s doc for why restarting that sequence per call would be
    /// fail-open.
    ///
    /// **Durability-ordering exemption (review finding, Important 3):** ideally every call to
    /// this method happens only after the record that will carry its descriptors is durably WAL
    ///-appended and fsynced — otherwise an extension id can be minted in-process for a batch
    /// whose append then fails, leaving the live resolver's state one step ahead of what a
    /// restart-replay would ever reconstruct from the WAL alone. `Engine::accept_change` honours
    /// that ordering (it resolves only after its `Change` record's append/fsync succeeds).
    /// `/control/ingest` is a deliberate, structural exception: signature-sorted entity-id
    /// assignment (I9/§11.1, `allocate_sorted`) needs each item's resolved terms to compute its
    /// sort key *before* the item's `WalRow` (which carries the assigned id) can even be framed
    /// for append — so this call cannot be deferred past the durability boundary for ingest
    /// without abandoning signature-sorted assignment itself. This is judged safe in practice
    /// (not merely convenient) because an extension id is, by construction, unsatisfiable by any
    /// session's `satisfied` set (`tessera_lifecycle::buffer`'s module doc) — a live/replay
    /// mismatch in exactly *which* extension id a novel descriptor got renumbers internal
    /// bookkeeping only, never a visibility outcome.
    pub fn resolve_terms(&self, descriptors: &[Descriptor]) -> Vec<TermId> {
        let mut state = self.resolver_state.lock().unwrap();
        let (extension, next_extension_id) = std::mem::take(&mut *state);
        let mut resolver = DescriptorResolver::resume(&self.dict, extension, next_extension_id);
        let ids = descriptors.iter().map(|d| resolver.resolve(d)).collect();
        *state = resolver.into_state();
        ids
    }

    /// Resolve an external id to its `EntityId`, checking every item established live (bundle
    /// replay's own `IngestBatch` rows, plus every `/control/ingest` batch accepted since) before
    /// falling back to the bundle's own `entities/external-ids-0.arrow` extent.
    ///
    /// **Fallible** (closes review round 4's Critical C3): a real sidecar failure — digest
    /// mismatch, out-of-order extent, corrupt locator — now propagates as `Err` rather than the
    /// previous `ExternalIdIndex::resolve` panicking on it. A `/control/changes` request naming
    /// an external id backed by a corrupt sidecar gets a `500`, never a silent "unknown" *or* a
    /// panicked worker.
    pub fn resolve_external_id(
        &self,
        external_id: &[u8],
    ) -> std::result::Result<Option<EntityId>, StoreError> {
        if let Some(&entity) = self.established.lock().unwrap().get(external_id) {
            return Ok(Some(entity));
        }
        self.external_index.resolve(external_id)
    }

    /// Batch form of [`Self::resolve_external_id`] for `/control/ingest`'s duplicate check
    /// (contracts §3.1 r6): live map first for the *whole* batch (Important I-8 — `established`
    /// holds every id ingested since the build, which the sidecar cannot see at all, and is
    /// exactly where a retried client batch's duplicate lives), then one batched, sorted sidecar
    /// call for whatever residual keys the live map didn't resolve — each bundle extent is opened
    /// at most once regardless of batch size, never once per row.
    ///
    /// Returns one `Option<EntityId>` per input, in the caller's given order.
    pub fn resolve_external_ids(
        &self,
        external_ids: &[Vec<u8>],
    ) -> std::result::Result<Vec<Option<EntityId>>, StoreError> {
        let established = self.established.lock().unwrap();
        let mut results: Vec<Option<EntityId>> = external_ids
            .iter()
            .map(|id| established.get(id.as_slice()).copied())
            .collect();
        drop(established);

        let residual_positions: Vec<usize> = results
            .iter()
            .enumerate()
            .filter_map(|(i, r)| if r.is_none() { Some(i) } else { None })
            .collect();
        if residual_positions.is_empty() {
            return Ok(results);
        }
        let residual_keys: Vec<Vec<u8>> = residual_positions
            .iter()
            .map(|&i| external_ids[i].clone())
            .collect();
        let residual_results = self.external_index.resolve_many(&residual_keys)?;
        for (pos, resolved) in residual_positions.into_iter().zip(residual_results) {
            results[pos] = resolved;
        }
        Ok(results)
    }

    /// `entity -> external_id` for drill-down (`/v1/items`). Ordering mirrors
    /// `resolve_external_id`'s live-map-first rule, running the other way: post-build ingest has
    /// no locator slot and no extent entry, so the live map (`established_inverse`) is consulted
    /// first — Important I-9. `Ok(None)` means "this item genuinely has no caller external id", a
    /// legitimate state since `external_id` is optional on ingest; it must never mean "I could
    /// not find out". A `/v1/items` entity that is below the live high-water, past this bundle's
    /// locator, and unknown to the live map is an inconsistency, not an absent external id, and
    /// fails closed as `Err(StoreError::InvalidSidecar)` — see
    /// `ExternalIdSidecar::external_id_of_checked`'s doc.
    pub fn external_id_of(
        &self,
        entity: EntityId,
    ) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        if let Some(external_id) = self.established_inverse.lock().unwrap().get(&entity) {
            return Ok(Some(external_id.clone()));
        }
        self.external_index
            .external_id_of_checked(entity, self.allocator_high_water())
    }

    /// Allocate entity ids for a freshly-parsed ingest batch, in signature-sorted order (I9/§11.1)
    /// — must be called after every item's `terms` field is populated (via
    /// [`Engine::resolve_terms`]) and before the batch's `WalRow`s are framed for WAL append.
    ///
    /// **Propagates `AllocError`** rather than silently discarding it (a pre-existing
    /// `unused_must_use` gap this task closes incidentally, to keep `cargo clippy -D warnings`
    /// green): the allocator's ceiling is a real, reachable failure (I9's u32 cap), and an
    /// ingest batch left with unassigned or partially-assigned ids would frame `WalRow`s the WAL
    /// must never see.
    pub fn allocate_sorted(
        &self,
        items: &mut [PendingItem],
    ) -> std::result::Result<(), tessera_lifecycle::alloc::AllocError> {
        let mut alloc = self.allocator.lock().unwrap();
        assign_sorted(items, &mut alloc)
    }

    /// The body hash and per-row entity ids a batch id was previously accepted with, if any — the
    /// idempotency check for `/control/ingest`'s replay rule (R5): equal hash -> 200 no-op
    /// (returning the same `tessera_id`s, via the entity ids here); different hash -> 409.
    pub fn accepted_batch(&self, batch_id: &str) -> Option<([u8; 32], Vec<EntityId>)> {
        self.accepted_batches.lock().unwrap().get(batch_id).cloned()
    }

    /// Record a batch id as accepted. Must only be called after the batch's `IngestBatch` record
    /// has been WAL-appended and fsynced (the ack contract) — this index is purely an in-memory
    /// accelerant for the idempotency check above, not itself a durability boundary.
    pub fn record_accepted_batch(
        &self,
        batch_id: String,
        body_hash: [u8; 32],
        entity_ids: Vec<EntityId>,
    ) {
        self.accepted_batches
            .lock()
            .unwrap()
            .insert(batch_id, (body_hash, entity_ids));
    }

    /// Compute the wire `tessera_id` for `entity` under this deployment's current shard id and
    /// identity key (contracts §2.6/§3.4 r6). `/control/ingest`'s 200 response returns each
    /// accepted row's `tessera_id` this way rather than its raw `EntityId` (I10: entity ids never
    /// cross the trust boundary).
    ///
    /// **Fallible, not `.unwrap()`-able**: `IdentityKey::forward` refuses an entity at or above
    /// `u32::MAX` (Important I-1). The I9 allocator's ceiling makes that unreachable in practice
    /// for any entity this method is ever called with, but this stays a typed error rather than a
    /// panic — an internal invariant violation must fail closed (500), never crash the request
    /// thread or silently truncate.
    pub fn tessera_id_of(&self, entity: EntityId) -> std::result::Result<TesseraId, IdentityError> {
        let generation = self.generation.load_full();
        self.identity_key
            .forward(generation.bundle.manifest.identity.shard_id, entity)
    }

    /// Accept an ingest batch atomically: WAL append -> fsync -> apply (buffer clone + insert) ->
    /// generation swap, all while holding `self.wal`'s lock (**review finding, Critical 1**: the
    /// previous split — append/fsync under the caller's own WAL lock, then a *separate*,
    /// unlocked `apply_ingest`/`apply_change` call — let two concurrent acceptances race on
    /// `ArcSwap::load_full`/`store`: both load the same pre-swap generation, both clone it, and
    /// whichever `store`s last silently discards the other's already-fsynced, already-acked
    /// change with no error. Holding the WAL mutex across the *entire* append-through-swap
    /// sequence, for both this method and [`Engine::accept_change`], serialises every generation
    /// swap through one lock: the second of two concurrent acceptances cannot even begin its
    /// `load_full()` until the first has finished its `store()`, so it always builds its new
    /// generation on top of the first's effect rather than racing it.
    ///
    /// `rows` carry raw descriptor bytes (never `TermId`s — see `WalRow`'s doc); `terms` is each
    /// row's already-resolved term set, in the same order (resolved by the caller via
    /// [`Engine::resolve_terms`] before this call — see that method's doc for why ingest,
    /// specifically, cannot defer resolution past this call the way [`Engine::accept_change`]
    /// does).
    ///
    /// On success, also records `batch_id`/`body_hash` as accepted (the idempotency index) before
    /// releasing the lock, so a concurrent replay of the same batch id can never observe a window
    /// where the generation has swapped but the idempotency index hasn't caught up yet.
    ///
    /// Returns each accepted row's `EntityId`, in the same order as `rows` — never the caller's
    /// raw entity ids to keep (I10 stays server-side), but the caller (`/control/ingest`) needs
    /// them for exactly as long as it takes to turn each into a `tessera_id` (via
    /// [`Engine::tessera_id_of`]) for the 200 response (contracts §3.4 r6). Also recorded, keyed
    /// by `batch_id`, so a byte-identical replay of an already-acked batch can answer with the
    /// same `tessera_id`s without re-deriving them from `external_id` — which would not work at
    /// all for a row that has none.
    pub fn accept_ingest(
        &self,
        rows: Vec<WalRow>,
        terms: Vec<Vec<TermId>>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> std::result::Result<Vec<EntityId>, WalError> {
        debug_assert_eq!(rows.len(), terms.len());
        let record = WalRecord::IngestBatch {
            batch_id: batch_id.clone(),
            body_hash,
            rows: rows.clone(),
        };

        let mut wal = self.wal.lock().unwrap();
        wal.append(&record)?;
        wal.fsync()?;

        let generation = self.generation.load_full();
        let mut buffer = (*generation.buffer).clone();
        let mut established = self.established.lock().unwrap();
        // `established` and `established_inverse` are updated together, in this one critical
        // section, so a `/control/changes` lookup and a `/v1/items` drill-down can never
        // disagree about the same item (task-9 brief, Important I-9).
        let mut established_inverse = self.established_inverse.lock().unwrap();
        for (row, row_terms) in rows.iter().zip(&terms) {
            // Contracts §3.4 r6: no external id means no sidecar entry and nothing to establish
            // here either -- the item is addressable only by its `tessera_id`. `None` must never
            // collide with `None`, so this simply skips the insert rather than inserting under a
            // shared "empty" key.
            if let Some(external_id) = &row.external_id {
                established.insert(external_id.clone(), row.entity_id);
                established_inverse.insert(row.entity_id, external_id.clone());
            }
            buffer.insert_row_with_terms(row, row_terms.clone());
        }
        drop(established);
        drop(established_inverse);

        let next = Generation {
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            overlay_version: generation.overlay_version + 1,
            overlay: Arc::clone(&generation.overlay),
            buffer: Arc::new(buffer),
        };
        self.generation.store(Arc::new(next));

        let entity_ids: Vec<EntityId> = rows.iter().map(|row| row.entity_id).collect();
        self.accepted_batches
            .lock()
            .unwrap()
            .insert(batch_id, (body_hash, entity_ids.clone()));

        drop(wal);
        Ok(entity_ids)
    }

    /// Accept one `/control/changes` disposition change atomically: WAL append -> fsync -> apply
    /// (overlay clone + `Overlay::apply`) -> generation swap, all while holding `self.wal`'s lock
    /// — see [`Engine::accept_ingest`]'s doc for why (Critical 1) and this crate's `ChangeOp`
    /// doc for the three retirement rules this composes with.
    ///
    /// **Deny-op append failure** (lifecycle §4): if the append/fsync genuinely fails and `op` is
    /// `Delete`/`Suppress`, the change is still applied (the item hidden immediately) before this
    /// returns `Err` — never a refusal that leaves a deny unapplied. For any other op, a failed
    /// append/fsync applies nothing.
    ///
    /// **Durability-ordering fix (review finding, Important 3):** `raw_descriptors` (present only
    /// for `Predicate`) are resolved to `TermId`s via [`Engine::resolve_terms`] *inside* this
    /// method, only after the append/fsync has already succeeded — never before. Unlike ingest
    /// (see `resolve_terms`'s doc for why that path is a structural exception), a change's
    /// resolved terms are needed only for the subsequent `Overlay::apply` call, not for anything
    /// that must be decided before the record can be framed, so there is no reason to mint an
    /// extension id for a record that might never become durable. `Delete`/`Suppress`/
    /// `Unsuppress` never carry descriptors, so the deny-op append-failure path never resolves
    /// anything either.
    pub fn accept_change(
        &self,
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        raw_descriptors: Option<Vec<Vec<u8>>>,
    ) -> std::result::Result<(), WalError> {
        let record = WalRecord::Change {
            external_id,
            op,
            descriptors: raw_descriptors.clone(),
        };

        let mut wal = self.wal.lock().unwrap();
        let append_result = wal.append(&record).and_then(|()| wal.fsync());

        let result = match append_result {
            Ok(_) => {
                let terms = raw_descriptors.as_ref().map(|ds| self.resolve_terms(ds));
                self.apply_change_locked(entity, op, terms);
                Ok(())
            }
            Err(e) => {
                if matches!(op, ChangeOp::Delete | ChangeOp::Suppress) {
                    self.apply_change_locked(entity, op, None);
                }
                Err(e)
            }
        };

        drop(wal);
        result
    }

    /// The overlay-clone-and-swap step shared by both of [`Engine::accept_change`]'s outcomes.
    /// Private: called only while `self.wal`'s lock is held (see [`Engine::accept_ingest`]'s doc
    /// for why every generation swap must be serialised through that one lock). Pins are never
    /// invalidated by this (I11: a pin fixes `(prefix, segments_version)` only, and this bumps
    /// `overlay_version`, not `segments_version`) — lifecycle §2.3's rule that a suppression
    /// applies to a pinned request the moment it is accepted, without expiring the pin.
    fn apply_change_locked(&self, entity: EntityId, op: ChangeOp, terms: Option<Vec<TermId>>) {
        let generation = self.generation.load_full();
        let mut overlay: Overlay = (*generation.overlay).clone();
        overlay.apply(entity, op, terms);

        let next = Generation {
            prefix: generation.prefix.clone(),
            segments_version: generation.segments_version,
            watermark: generation.watermark,
            bundle: Arc::clone(&generation.bundle),
            overlay_version: generation.overlay_version + 1,
            overlay: Arc::new(overlay),
            buffer: Arc::clone(&generation.buffer),
        };
        self.generation.store(Arc::new(next));
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

/// Resolve an external id to an [`EntityId`] via `tessera_store::ExternalIdSidecar` — a thin
/// newtype-free wrapper so callers in this crate keep using `EntityId` rather than the bare
/// `tessera_types`-free type the store crate returns, and so no `ExtentDesc`, digest, ordinal or
/// file path from the sidecar's own bookkeeping is ever named in this crate (Ruling B).
///
/// The sidecar is per-extent lazy (Task 8): nothing is opened, mapped or digested until the
/// first resolution, and then only the one extent the key falls in — never the whole family.
/// This is the `resolve_from_bundle` seam `tessera_lifecycle::overlay::replay` uses,
/// authorisation-bearing because `/control/changes` denies whichever entity it resolves to — a
/// wrong resolution denies the wrong entity and leaves the intended target visible.
struct ExternalIdIndex(tessera_store::ExternalIdSidecar);

impl ExternalIdIndex {
    fn open(
        bundle_manifest: &tessera_store::manifest::Manifest,
        partition_manifest: &tessera_store::manifest::SegmentsManifest,
        prefix_dir: &Path,
    ) -> std::result::Result<Self, StoreError> {
        tessera_store::ExternalIdSidecar::deferred_from_manifest(
            bundle_manifest,
            partition_manifest,
            prefix_dir,
        )
        .map(ExternalIdIndex)
    }

    /// **Fallible, closing review round 4's Critical C3.** `resolve_from_bundle`'s closure
    /// signature in `tessera_lifecycle::overlay::replay` now takes a generic error parameter
    /// rather than a fixed `Option` — `tessera-lifecycle` does not depend on `tessera-store`, so
    /// the closure cannot name `StoreError` itself, but it can return any `Result<_, E>` and let
    /// the caller's `E` be inferred as `StoreError` here. A corrupt extent, a digest mismatch or
    /// a shuffled extent list now propagates as `Err(StoreError::InvalidSidecar)` through
    /// `replay`/`Engine::open`/`Engine::resolve_external_id`, rather than the previous panic —
    /// still fail-closed in effect, but no longer a panic in an async handler or at open.
    fn resolve(&self, external_id: &[u8]) -> std::result::Result<Option<EntityId>, StoreError> {
        self.0.resolve(external_id)
    }

    /// Batched form of [`Self::resolve`] — one sorted pass over `external_ids`, each extent
    /// opened at most once, rather than one open per row (`/control/ingest`'s duplicate check,
    /// contracts §3.1 r6).
    fn resolve_many(
        &self,
        external_ids: &[Vec<u8>],
    ) -> std::result::Result<Vec<Option<EntityId>>, StoreError> {
        self.0.resolve_many(external_ids)
    }

    /// `entity -> external_id`, distinguishing "genuinely has none" from "an inconsistency" — see
    /// `tessera_store::ExternalIdSidecar::external_id_of_checked`'s doc.
    fn external_id_of_checked(
        &self,
        entity: EntityId,
        high_water: u64,
    ) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        self.0.external_id_of_checked(entity, high_water)
    }
}
