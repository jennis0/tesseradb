//! Mask fragment build (the authorise path) and the directory-backed frozen fragment cache
//! (Task 6, Reference Sheet R4, R1).
//!
//! A mask fragment *is* the authorisation decision (I2): [`build_fragment`] unions the postings
//! of every term a viewer's credential satisfies into one bitmap, entirely from
//! `M_auth`-eligible inputs — no other quantity is derived and then gated. This module is
//! parametric over [`PostingsReader`]: it holds no lifecycle state, and `RowId` never appears
//! here (SA §3's crate-dependency rule; entity space only).
//!
//! [`FragmentCache`] persists the result as a CRoaring `Frozen`-format bitmap in an engine-local
//! cache directory (never in the bundle — Reference Sheet R1), so that a repeated grant set
//! reuses the on-disk fragment across process restarts instead of re-unioning postings. The
//! cache key is deliberately wider than "the set of granted terms": see [`FragmentCache::new`].
//! Because a parseable-but-wrong fragment would be a silent disclosure (not merely a crash), the
//! cache does not rely on "the directory is engine-private" as its only line of defence: entries
//! are content-addressed with a stored SHA-256 digest verified on every reopen (before the
//! unsafe `Frozen` view is ever constructed), writes are `fsync`ed before the rename that makes
//! them visible, and the directory and its files are created with owner-only permissions on
//! unix.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use croaring::{Bitmap, BitmapView, Frozen};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use tessera_types::TermId;

use crate::postings::{PostingRef, PostingsReader};
use crate::single_flight::{SingleFlightCache, SingleFlightError};

/// Union the postings of every term in `terms` into one bitmap: this *is* the authorisation
/// decision (I2). Partitions the granted postings into Roaring views (unioned in bulk via
/// [`Bitmap::fast_or`] — croaring 2.7.0's binding for `roaring_bitmap_or_many`, the bulk-union
/// entry point the brief calls `Bitmap::or_many`) and small arrays (decoded, concatenated,
/// sorted, and folded in with `add_many`), then `run_optimize`s the result.
///
/// Parametric: takes `postings` as an argument and holds no lifecycle state of its own. `RowId`
/// must not appear anywhere in this crate — `terms` and the returned bitmap are both
/// entity-space, never row-space.
pub fn build_fragment(terms: &[TermId], postings: &PostingsReader) -> io::Result<Bitmap> {
    let mut views: Vec<BitmapView<'_>> = Vec::new();
    let mut small: Vec<u32> = Vec::new();

    for &term in terms {
        match postings.posting(term)? {
            PostingRef::Roaring(view) => views.push(view),
            PostingRef::Array(bytes) => {
                // `PostingsReader::open` (Task 5) validates every tag-0 payload's length is a
                // multiple of 4 once, at open time — this is not re-checked per lookup, so a
                // violation here would mean that validation was bypassed, not that this call site
                // needs its own fail-closed handling.
                debug_assert!(
                    bytes.len() % 4 == 0,
                    "tag-0 posting payload length must be a multiple of 4 (validated at \
                     PostingsReader::open)"
                );
                for chunk in bytes.chunks_exact(4) {
                    small.push(u32::from_le_bytes(chunk.try_into().unwrap()));
                }
            }
        }
    }

    let refs: Vec<&Bitmap> = views.iter().map(|view| &**view).collect();
    let mut fragment = if refs.is_empty() {
        Bitmap::new()
    } else {
        Bitmap::fast_or(&refs)
    };

    small.sort_unstable();
    fragment.add_many(&small);
    fragment.run_optimize();

    Ok(fragment)
}

/// Compute the canonical cache key: SHA-256 over
/// `bundle_identity ‖ auth_plugin_hash ‖ sorted term_id u32 LEs` (deduplicated). Term IDs are
/// bundle-relative ordinals, so a persistent cache directory reused across bundle rebuilds — or
/// across an auth plugin upgrade — would otherwise serve a frozen fragment naming a *different*
/// entity set: a disclosure bug, not a perf bug.
fn canonical_key(
    bundle_identity: &[u8; 32],
    auth_plugin_hash: &[u8; 32],
    terms: &[TermId],
) -> [u8; 32] {
    let mut sorted: Vec<u32> = terms.iter().copied().map(TermId::raw).collect();
    sorted.sort_unstable();
    sorted.dedup();

    let mut hasher = Sha256::new();
    hasher.update(bundle_identity);
    hasher.update(auth_plugin_hash);
    for term in &sorted {
        hasher.update(term.to_le_bytes());
    }
    hasher.finalize().into()
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Process-unique counter for [`tmp_sibling`] — see its doc for why the suffix must be unique
/// per *call*, not just per target path.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Append a unique `.<pid>.<counter>.tmp` suffix to `path`'s full file name — used for
/// write-then-rename. The suffix must be unique per call (not just derived from `path`): two
/// concurrent `get_or_build` calls that race to build the *same* canonical key (same process,
/// racing threads sharing this `FragmentCache`, or two separate processes sharing the cache
/// directory) must not both write through one shared tmp path, where an unsynchronised write from
/// each could interleave and leave a corrupt file behind before either rename lands. With a
/// unique tmp path per attempt, both writes complete independently and the final `rename`
/// (POSIX-atomic) simply lets the later one win — both wrote byte-identical content, since the
/// frozen bytes are a deterministic function of the same bitmap.
fn tmp_sibling(path: &Path) -> PathBuf {
    let unique = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.{unique}.tmp", std::process::id()));
    PathBuf::from(name)
}

/// Create `dir` (and any missing ancestors) with owner-only permissions on unix (`0700`); on
/// other platforms this is `create_dir_all` with whatever the platform default is — mask
/// fragments are name the viewer's exact visible set, so per-principal cache entries should never
/// be group/world-readable where the platform lets us say so.
#[cfg(unix)]
fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Create (truncating) a file at `path` with owner-only permissions on unix (`0600`); see
/// [`create_private_dir_all`].
#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// A frozen, memory-mapped fragment reopened from the cache directory. `view()` borrows straight
/// from the mapping — no copy, no per-lookup deserialisation cost beyond CRoaring's own
/// pointer-fixup.
pub struct FrozenFragment {
    mmap: memmap2::Mmap,
    /// The generation's SEGMENTS watermark at build time, as passed to
    /// [`FragmentCache::get_or_build`]. Persisted alongside the frozen bytes (see
    /// [`FragmentCache`]'s module doc for the sidecar layout) so a reopened fragment restores it
    /// without the caller having to remember it out of band.
    pub watermark: u64,
}

impl FrozenFragment {
    /// View the frozen bitmap. Safe because every `FrozenFragment` in existence was constructed
    /// by [`open`](Self::open), which — before ever returning `Ok` — checked:
    /// - the mapped length matches the sidecar-recorded exact frozen size, *and*
    /// - the mapped bytes' SHA-256 digest matches the sidecar-recorded digest (computed from the
    ///   frozen bytes at build time). This is the load-bearing check: bytes of the right length
    ///   but wrong or corrupted content — e.g. right-length garbage left by a torn write after
    ///   power loss, on a filesystem where the rename lands before the data is durable — would
    ///   pass a length-only check and then be undefined behaviour (or a disclosure: a
    ///   parseable-but-wrong fragment) once handed to `Frozen`'s unchecked deserialiser. The
    ///   digest closes that gap.
    ///
    /// The mapping's base address is also page-aligned (mmap always returns page-aligned bases),
    /// satisfying `Frozen::REQUIRED_ALIGNMENT` (32).
    pub fn view(&self) -> BitmapView<'_> {
        // SAFETY: see the discharge above — `open()` is the only constructor of `FrozenFragment`
        // and it verifies length and digest before returning `Ok`.
        unsafe { BitmapView::deserialize::<Frozen>(&self.mmap[..]) }
    }

    /// Open an existing `(frag_path, meta_path)` pair, verifying the sidecar-recorded length and
    /// SHA-256 digest against the mapped file before ever calling the unsafe `Frozen` view
    /// deserialiser. Fails closed (`InvalidData`) on any mismatch, truncation, or malformed
    /// sidecar — a corrupt or tampered cache entry must never reach
    /// `BitmapView::deserialize::<Frozen>`, whose safety contract we could not otherwise
    /// discharge for bytes we did not just produce ourselves.
    fn open(frag_path: &Path, meta_path: &Path) -> io::Result<Self> {
        let meta = std::fs::read(meta_path)?;
        if meta.len() != META_LEN {
            return Err(invalid_data(format!(
                "fragment cache: {} has length {} (expected {META_LEN}: watermark u64 LE \
                 ‖ frozen_len u64 LE ‖ sha256(frozen_bytes))",
                meta_path.display(),
                meta.len()
            )));
        }
        let watermark = u64::from_le_bytes(meta[0..8].try_into().unwrap());
        let expected_len = u64::from_le_bytes(meta[8..16].try_into().unwrap());
        let expected_digest: [u8; 32] = meta[16..48].try_into().unwrap();

        let file = File::open(frag_path)?;
        // SAFETY: the mapping is read-only for the duration of this function and dropped (or
        // handed back inside `FrozenFragment`, still read-only) before anything else in this
        // process opens the same path for writing — `FragmentCache` never mutates a `.frag` file
        // in place, only write-then-rename under a fresh temp name (same discharge as
        // `PostingsReader::open`).
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;

        if mmap.len() as u64 != expected_len {
            return Err(invalid_data(format!(
                "fragment cache: {} has length {} but the sidecar records {expected_len} \
                 — refusing to view a mismatched frozen buffer",
                frag_path.display(),
                mmap.len()
            )));
        }

        let actual_digest: [u8; 32] = Sha256::digest(&mmap[..]).into();
        if actual_digest != expected_digest {
            return Err(invalid_data(format!(
                "fragment cache: {} does not match its sidecar-recorded SHA-256 digest — \
                 refusing to view a corrupted or tampered frozen buffer",
                frag_path.display()
            )));
        }

        Ok(FrozenFragment { mmap, watermark })
    }

    /// Serialise `bitmap` in `Frozen` format and persist it (plus its watermark/length/digest
    /// sidecar) under `(frag_path, meta_path)` via write-then-rename, then reopen it as a
    /// `FrozenFragment` (exercising the same validated-open path a cache hit would use).
    fn build_and_persist(
        frag_path: &Path,
        meta_path: &Path,
        bitmap: &Bitmap,
        watermark: u64,
    ) -> io::Result<Self> {
        // `serialize_into_vec` inserts whatever front padding CRoaring's `Frozen` format needs to
        // hand back a 32-byte-aligned *in-memory* slice; the slice's own bytes are the exact
        // frozen buffer with no such padding, so writing exactly those bytes at file offset 0 —
        // where the (page-aligned) mmap base will later satisfy the same 32-byte alignment
        // requirement — reproduces the identical, exact-length buffer on reopen.
        let mut scratch = Vec::new();
        let frozen_bytes = bitmap.serialize_into_vec::<Frozen>(&mut scratch);
        let frozen_len = frozen_bytes.len() as u64;
        let digest: [u8; 32] = Sha256::digest(&*frozen_bytes).into();

        // Frag before meta, and both fsynced before their rename: on crash recovery, a `.meta`
        // file existing implies its `.frag` sibling is already fully durable — `open()` treats a
        // frag-without-meta (or a length/digest mismatch) as a plain cache miss, never a false
        // hit, so writing meta second is what makes "meta present" a trustworthy signal that the
        // pair is complete and intact.
        let tmp_frag = tmp_sibling(frag_path);
        {
            let mut f = create_private_file(&tmp_frag)?;
            f.write_all(frozen_bytes)?;
            f.sync_data()?;
        }
        std::fs::rename(&tmp_frag, frag_path)?;

        let mut meta = Vec::with_capacity(META_LEN);
        meta.extend_from_slice(&watermark.to_le_bytes());
        meta.extend_from_slice(&frozen_len.to_le_bytes());
        meta.extend_from_slice(&digest);
        let tmp_meta = tmp_sibling(meta_path);
        {
            let mut f = create_private_file(&tmp_meta)?;
            f.write_all(&meta)?;
            f.sync_data()?;
        }
        std::fs::rename(&tmp_meta, meta_path)?;

        // Fsync the containing directory so both renames' directory-entry updates are durable,
        // not just the file contents — otherwise a power loss right after the renames could
        // leave the entries themselves unrecorded even though the file bytes hit disk.
        if let Some(parent) = frag_path.parent() {
            if let Ok(dir_file) = File::open(parent) {
                let _ = dir_file.sync_all();
            }
        }

        Self::open(frag_path, meta_path)
    }
}

/// `watermark: u64 LE (8) ‖ frozen_len: u64 LE (8) ‖ sha256(frozen_bytes) (32)`.
const META_LEN: usize = 48;

/// Directory-backed frozen fragment store.
///
/// Cache key: SHA-256 over `bundle_identity ‖ auth_plugin_hash ‖ sorted term_id u32 LEs`
/// (deduplicated) — see [`canonical_key`]'s doc for why the key must be wider than the granted
/// term set. `bundle_identity` is the generation's MANIFEST digest and `auth_plugin_hash` is the
/// active auth plugin's hash (design §2.3 requires the plugin version in the key; SA §3 adds the
/// bundle identity: term IDs are bundle-relative ordinals, so reusing a cache dir across a bundle
/// rebuild — or an auth plugin change — with a stale key would otherwise serve a frozen fragment
/// naming a *different* entity set).
///
/// On-disk layout (flat, under `dir`, one pair per canonical key, hex-encoded; `dir` and every
/// file in it are created with owner-only permissions on unix — see
/// [`create_private_dir_all`]/[`create_private_file`]):
/// - `<hex key>.frag` — the exact `Frozen`-format bitmap bytes, written at file offset 0 (the
///   mmap base is page-aligned, satisfying `Frozen::REQUIRED_ALIGNMENT = 32` on reopen).
/// - `<hex key>.meta` — a [`META_LEN`]-byte sidecar: `watermark: u64 LE ‖ frozen_len: u64 LE ‖
///   sha256(frozen_bytes)`. `watermark` restores the generation's SEGMENTS watermark at build
///   time across process restarts; `frozen_len` and the digest let [`FrozenFragment::open`]
///   verify the mapped file's length *and content* before ever calling the unsafe `Frozen` view
///   deserialiser — fail-closed on a truncated, corrupted, or tampered cache entry, not just a
///   short one (a length-only check would pass right-length garbage, e.g. from a torn write after
///   power loss).
///
/// Both files are written via write-then-rename (`<name>.<pid>.<n>.tmp` → `<name>`), each
/// `fsync`ed before its rename and the containing directory `fsync`ed after, so a crash mid-write
/// or immediately after never leaves a partial or not-yet-durable file visible at the looked-up
/// name.
///
/// An in-memory `FxHashMap<auth_data_hash, canonical_key>` gives repeat sessions presenting the
/// same credential a fast path that skips re-sorting and re-hashing the granted term list; it is
/// pure memoisation of [`canonical_key`]'s computation; it is not itself a source of authorisation
/// decisions and holds nothing that must survive a restart (the on-disk `.frag`/`.meta` pair is
/// the durable cache; this map is not). Because the fast path skips recomputation, it trusts that
/// **`auth_data_hash` determines `satisfied`** — see [`get_or_build`](Self::get_or_build)'s doc.
///
/// **D-G slot-state single-flight (lifecycle §3.3).** A second map, [`Self::slots`], is keyed by
/// the CANONICAL key (never `auth_data_hash` — see [`get_or_build`](Self::get_or_build)'s doc for
/// why the fast-path key would be an I2 hazard here) and holds each key's build state: `Building`
/// while a build is in flight, `Ready(Arc<FrozenFragment>)` once it lands. `Ready` doubles as the
/// in-memory cache — a warm `get_or_build` call returns straight from this map without any file
/// IO (no mmap, no SHA-256 verify), which is the fix for the other half of this cache's defect
/// (every warm authorise previously re-mmapped and re-verified the frozen file on every hit). A
/// concurrent arrival on a key already `Building` does not wait for it (D-G's non-blocking-waiters
/// rule); it gets `FragmentCacheError::Building` immediately. A failed build never publishes
/// `Ready` and never leaves `Building` behind — see [`crate::single_flight`]'s module doc.
pub struct FragmentCache {
    dir: PathBuf,
    bundle_identity: [u8; 32],
    auth_plugin_hash: [u8; 32],
    key_memo: Mutex<FxHashMap<[u8; 32], [u8; 32]>>,
    slots: SingleFlightCache<[u8; 32], FrozenFragment>,
    rebuilds: AtomicU64,
}

/// [`FragmentCache::get_or_build`]'s failure modes. Neither variant is ever cached (I13
/// fail-closed): a `Building` observation means some other caller owns the in-flight build, and
/// an `Io` failure means the canonical key is left absent so the very next call retries from
/// scratch.
#[derive(Debug)]
pub enum FragmentCacheError {
    /// D-G: another caller is already building this exact canonical key right now (lifecycle
    /// §3.3's single-flight rule). This call did not wait for it — retry shortly. `Engine::
    /// authorise` maps this to `EngineError::FragmentBuilding`, which the server maps to a
    /// fail-closed 500 today and HTTP 429 once a later task wires that mapping.
    Building,
    /// The build itself failed (postings read, directory creation, or the write-then-rename
    /// persist step). The failing canonical key was removed before this was returned, never
    /// cached — a cached `Err` would be a permanent fail-closed wedge for that credential.
    Io(io::Error),
}

impl std::fmt::Display for FragmentCacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FragmentCacheError::Building => write!(
                f,
                "fragment build already in progress for this credential's canonical key; retry \
                 shortly"
            ),
            FragmentCacheError::Io(e) => write!(f, "fragment cache: {e}"),
        }
    }
}

impl std::error::Error for FragmentCacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FragmentCacheError::Building => None,
            FragmentCacheError::Io(e) => Some(e),
        }
    }
}

impl From<io::Error> for FragmentCacheError {
    fn from(e: io::Error) -> Self {
        FragmentCacheError::Io(e)
    }
}

impl FragmentCache {
    /// `dir` is the engine's local cache directory for this bundle/auth-plugin pair — never a
    /// path inside the bundle itself (Reference Sheet R1). `bundle_identity` is the generation's
    /// MANIFEST digest; `auth_plugin_hash` is the active auth plugin's hash. Does not touch the
    /// filesystem; `get_or_build` creates `dir` (and any missing ancestors) on first write.
    pub fn new(dir: &Path, bundle_identity: [u8; 32], auth_plugin_hash: [u8; 32]) -> Self {
        FragmentCache {
            dir: dir.to_path_buf(),
            bundle_identity,
            auth_plugin_hash,
            key_memo: Mutex::new(FxHashMap::default()),
            slots: SingleFlightCache::new(),
            rebuilds: AtomicU64::new(0),
        }
    }

    /// Number of times [`get_or_build`](Self::get_or_build) has actually called
    /// [`build_fragment`] (cache miss, on this `FragmentCache` instance) rather than reusing an
    /// existing frozen fragment. Exposed for cache-behaviour tests and operational metrics; not
    /// itself part of the authorisation decision. D-G: increments exactly once per single-flight
    /// build — a losing arrival that retries into a `Ready` hit never increments this, whether
    /// that hit came from this process's in-memory cache or another process's on-disk one.
    pub fn rebuild_count(&self) -> u64 {
        self.rebuilds.load(Ordering::Relaxed)
    }

    /// Canonical-key slots currently held (`Building` and `Ready` both counted) — exposed for
    /// fail-closed tests confirming a failed build leaves no wedge (I13), analogous to
    /// `tessera_engine::Engine::row_projection_cache_len`.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    fn frag_path(&self, key: &[u8; 32]) -> PathBuf {
        self.dir.join(format!("{}.frag", hex_encode(key)))
    }

    fn meta_path(&self, key: &[u8; 32]) -> PathBuf {
        self.dir.join(format!("{}.meta", hex_encode(key)))
    }

    /// Return the frozen fragment for `satisfied` (the terms a viewer's credential grants),
    /// building and persisting it if this is the first time this exact `(bundle_identity,
    /// auth_plugin_hash, satisfied)` combination has been seen — by *any* process sharing this
    /// cache directory, not just this one.
    ///
    /// **Caller obligation:** `auth_data_hash` must identify the *credential* whose evaluation
    /// produced `satisfied` — i.e. it must be a (collision-resistant) function of the same
    /// `auth_data` that the auth plugin evaluated to obtain `satisfied`, such that the same
    /// `auth_data_hash` never arrives paired with two different term sets. The in-memory
    /// canonical-key fast path trusts this: on a memo hit it returns the previously-computed
    /// canonical key *without* re-deriving it from `satisfied`, so a caller that violates the
    /// obligation would silently get back a fragment built for a *different* grant set — an I2
    /// disclosure if that other set happens to be a superset. Debug builds catch a violation via
    /// a `debug_assert_eq!` against a freshly recomputed key; release builds do not re-check on
    /// the fast path (that would defeat its purpose), so this obligation is load-bearing in
    /// release too.
    ///
    /// `postings` supplies the union inputs on a cache miss. `watermark` is the caller-supplied
    /// SEGMENTS watermark to persist alongside a freshly built fragment; it is ignored on a cache
    /// hit (the hit's own persisted watermark, from when it was built, is what's returned —
    /// Task 10's composition uses the fragment's own watermark).
    ///
    /// **D-G slot-state single-flight (lifecycle §3.3).** The single-flight map is keyed by the
    /// canonical key computed just below — never by `auth_data_hash` — so two different
    /// credentials that happen to satisfy the same term set correctly single-flight onto the same
    /// build, and (more importantly for I2) a fast-path `auth_data_hash` collision could never be
    /// mistaken for a build-in-flight signal on the wrong key. On a hit against `Ready`, this
    /// returns straight from memory: no file open, no mmap, no SHA-256 verify (the "warm authorise
    /// does no file IO" fix). On a miss, the closure below still tries the on-disk pair first (a
    /// **different** process, or an earlier run of this one before this map existed in memory, may
    /// already have persisted it) before falling back to [`build_fragment`]. A concurrent arrival
    /// on the same canonical key while a build is in flight gets `Err(FragmentCacheError::
    /// Building)` immediately — it does not wait (D-G's non-blocking-waiters rule) — and a failed
    /// build (`Err` or panic) leaves the key absent rather than wedged or cached (I13).
    pub fn get_or_build(
        &self,
        satisfied: &[TermId],
        auth_data_hash: [u8; 32],
        postings: &PostingsReader,
        watermark: u64,
    ) -> Result<Arc<FrozenFragment>, FragmentCacheError> {
        let key = {
            let cached = self.key_memo.lock().unwrap().get(&auth_data_hash).copied();
            match cached {
                Some(key) => {
                    debug_assert_eq!(
                        key,
                        canonical_key(&self.bundle_identity, &self.auth_plugin_hash, satisfied),
                        "get_or_build: auth_data_hash {auth_data_hash:02x?} was previously \
                         associated with a different term set than `satisfied` now hashes to — \
                         callers must derive auth_data_hash from the same auth_data that produced \
                         `satisfied` (see this method's doc: a violation silently returns a \
                         fragment for the wrong grant set, an I2 disclosure risk)"
                    );
                    key
                }
                None => {
                    let key =
                        canonical_key(&self.bundle_identity, &self.auth_plugin_hash, satisfied);
                    self.key_memo.lock().unwrap().insert(auth_data_hash, key);
                    key
                }
            }
        };

        self.slots
            .get_or_try_build(key, || {
                let frag_path = self.frag_path(&key);
                let meta_path = self.meta_path(&key);

                // No existence pre-check: `open()` itself fails closed on anything short of a
                // fully valid, digest-matching pair, so a missing file and a corrupt one are
                // indistinguishable "miss, rebuild" outcomes here — there is nothing a pre-check
                // would add. This only runs on a genuine slot-state miss (never on a `Ready`
                // hit), so it is the cold path: a first-ever build in this process, or a
                // fragment another process already persisted.
                if let Ok(frozen) = FrozenFragment::open(&frag_path, &meta_path) {
                    return Ok(frozen);
                }

                create_private_dir_all(&self.dir)?;
                let bitmap = build_fragment(satisfied, postings)?;
                self.rebuilds.fetch_add(1, Ordering::Relaxed);
                FrozenFragment::build_and_persist(&frag_path, &meta_path, &bitmap, watermark)
            })
            .map_err(|e| match e {
                SingleFlightError::Building => FragmentCacheError::Building,
                SingleFlightError::Build(io_err) => FragmentCacheError::Io(io_err),
            })
    }
}
