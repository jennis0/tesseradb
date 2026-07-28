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

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use croaring::{Bitmap, BitmapView, Frozen};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use tessera_types::TermId;

use crate::postings::{PostingRef, PostingsReader};

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
                for chunk in bytes.chunks_exact(4) {
                    // `chunk` is exactly 4 bytes by construction of `chunks_exact(4)`.
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
/// directory) must not both write through one shared tmp path, where an unsynchronised
/// `std::fs::write` from each could interleave and leave a corrupt file behind before either
/// rename lands. With a unique tmp path per attempt, both writes complete independently and the
/// final `rename` (POSIX-atomic) simply lets the later one win — both wrote byte-identical
/// content, since the frozen bytes are a deterministic function of the same bitmap.
fn tmp_sibling(path: &Path) -> PathBuf {
    let unique = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.{unique}.tmp", std::process::id()));
    PathBuf::from(name)
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
    /// View the frozen bitmap. Safe because:
    /// - the mapped bytes are exactly what [`FragmentCache`] wrote via
    ///   `Bitmap::serialize_into_vec::<Frozen>` (never touched by anything else — the cache
    ///   directory is engine-private), and
    /// - the mapping's length was checked against the sidecar-recorded exact frozen size when
    ///   this `FrozenFragment` was opened, and
    /// - the mapping's base address is page-aligned (mmap always returns page-aligned bases),
    ///   which satisfies `Frozen::REQUIRED_ALIGNMENT` (32).
    pub fn view(&self) -> BitmapView<'_> {
        // SAFETY: see the discharge above; `open`/`build_and_persist` are the only writers/
        // openers of these bytes and both uphold Frozen's `deserialize_view` contract.
        unsafe { BitmapView::deserialize::<Frozen>(&self.mmap[..]) }
    }

    /// Open an existing `(frag_path, meta_path)` pair, validating the sidecar-recorded length
    /// against the mapped file before ever calling the unsafe `Frozen` view deserialiser. Fails
    /// closed (`InvalidData`) on any mismatch, truncation, or malformed sidecar — a corrupt cache
    /// entry must never reach `BitmapView::deserialize::<Frozen>`, whose safety contract we could
    /// not otherwise discharge.
    fn open(frag_path: &Path, meta_path: &Path) -> io::Result<Self> {
        let meta = std::fs::read(meta_path)?;
        if meta.len() != 16 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "fragment cache: {} has length {} (expected 16: watermark u64 LE \
                     ‖ frozen_len u64 LE)",
                    meta_path.display(),
                    meta.len()
                ),
            ));
        }
        let watermark = u64::from_le_bytes(meta[0..8].try_into().unwrap());
        let expected_len = u64::from_le_bytes(meta[8..16].try_into().unwrap());

        let file = File::open(frag_path)?;
        // SAFETY: the cache directory is engine-private and not concurrently truncated/resized
        // by anything outside this process during the mapping's lifetime, matching memmap2's
        // usual caveat for file-backed mappings (same discharge as `PostingsReader::open`).
        let mmap = unsafe { memmap2::Mmap::map(&file) }?;

        if mmap.len() as u64 != expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "fragment cache: {} has length {} but the sidecar records {expected_len} \
                     — refusing to view a mismatched frozen buffer",
                    frag_path.display(),
                    mmap.len()
                ),
            ));
        }

        Ok(FrozenFragment { mmap, watermark })
    }

    /// Serialise `bitmap` in `Frozen` format and persist it (plus its watermark sidecar) under
    /// `(frag_path, meta_path)` via write-then-rename, then reopen it as a `FrozenFragment`
    /// (exercising the same validated-open path a cache hit would use).
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

        let tmp_frag = tmp_sibling(frag_path);
        std::fs::write(&tmp_frag, &*frozen_bytes)?;
        std::fs::rename(&tmp_frag, frag_path)?;

        let mut meta = Vec::with_capacity(16);
        meta.extend_from_slice(&watermark.to_le_bytes());
        meta.extend_from_slice(&frozen_len.to_le_bytes());
        let tmp_meta = tmp_sibling(meta_path);
        std::fs::write(&tmp_meta, &meta)?;
        std::fs::rename(&tmp_meta, meta_path)?;

        Self::open(frag_path, meta_path)
    }
}

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
/// On-disk layout (flat, under `dir`, one pair per canonical key, hex-encoded):
/// - `<hex key>.frag` — the exact `Frozen`-format bitmap bytes, written at file offset 0 (the
///   mmap base is page-aligned, satisfying `Frozen::REQUIRED_ALIGNMENT = 32` on reopen).
/// - `<hex key>.meta` — a 16-byte sidecar: `watermark: u64 LE ‖ frozen_len: u64 LE`. `watermark`
///   restores the generation's SEGMENTS watermark at build time across process restarts;
///   `frozen_len` lets `FrozenFragment::open` validate the mapped length before ever calling the
///   unsafe `Frozen` view deserialiser (fail-closed on a truncated or corrupt cache entry).
///
/// Both files are written via write-then-rename (`<name>.tmp` → `<name>`), so a crash mid-write
/// never leaves a partial file visible at the looked-up name.
///
/// An in-memory `FxHashMap<auth_data_hash, canonical_key>` gives repeat sessions presenting the
/// same credential a fast path that skips re-sorting and re-hashing the granted term list; it is
/// pure memoisation of [`canonical_key`]'s computation; it is not itself a source of authorisation
/// decisions and holds nothing that must survive a restart (the on-disk `.frag`/`.meta` pair is
/// the durable cache; this map is not).
pub struct FragmentCache {
    dir: PathBuf,
    bundle_identity: [u8; 32],
    auth_plugin_hash: [u8; 32],
    key_memo: Mutex<FxHashMap<[u8; 32], [u8; 32]>>,
    rebuilds: AtomicU64,
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
            rebuilds: AtomicU64::new(0),
        }
    }

    /// Number of times [`get_or_build`](Self::get_or_build) has actually called
    /// [`build_fragment`] (cache miss, on this `FragmentCache` instance) rather than reusing an
    /// existing frozen fragment. Exposed for cache-behaviour tests and operational metrics; not
    /// itself part of the authorisation decision.
    pub fn rebuild_count(&self) -> u64 {
        self.rebuilds.load(Ordering::Relaxed)
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
    /// `auth_data_hash` identifies the *credential* (not the term set) for the in-memory
    /// canonical-key fast path — repeat calls with the same `auth_data_hash` skip re-sorting and
    /// re-hashing `satisfied`. `postings` supplies the union inputs on a cache miss.
    /// `watermark` is the caller-supplied SEGMENTS watermark to persist alongside a freshly built
    /// fragment; it is ignored on a cache hit (the hit's own persisted watermark, from when it
    /// was built, is what's returned — Task 10's composition uses the fragment's own watermark).
    pub fn get_or_build(
        &self,
        satisfied: &[TermId],
        auth_data_hash: [u8; 32],
        postings: &PostingsReader,
        watermark: u64,
    ) -> io::Result<Arc<FrozenFragment>> {
        let key = {
            let cached = self.key_memo.lock().unwrap().get(&auth_data_hash).copied();
            match cached {
                Some(key) => key,
                None => {
                    let key =
                        canonical_key(&self.bundle_identity, &self.auth_plugin_hash, satisfied);
                    self.key_memo.lock().unwrap().insert(auth_data_hash, key);
                    key
                }
            }
        };

        let frag_path = self.frag_path(&key);
        let meta_path = self.meta_path(&key);

        if frag_path.exists() && meta_path.exists() {
            if let Ok(frozen) = FrozenFragment::open(&frag_path, &meta_path) {
                return Ok(Arc::new(frozen));
            }
            // Fall through: a malformed on-disk entry is rebuilt, not fatal — the store is a
            // cache, not the source of truth, and rebuilding is always safe (I2: `build_fragment`
            // recomputes from `M_auth`-eligible postings either way).
        }

        std::fs::create_dir_all(&self.dir)?;
        let bitmap = build_fragment(satisfied, postings)?;
        self.rebuilds.fetch_add(1, Ordering::Relaxed);
        let frozen = FrozenFragment::build_and_persist(&frag_path, &meta_path, &bitmap, watermark)?;
        Ok(Arc::new(frozen))
    }
}
