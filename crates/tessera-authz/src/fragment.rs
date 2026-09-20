//! Mask fragment build (the authorise path) and the directory-backed frozen fragment cache.
//!
//! A mask fragment is the authorisation decision: [`build_fragment`] unions the postings of
//! every term a viewer's credential satisfies into one bitmap. This module is parametric over
//! [`PostingsReader`], holds no lifecycle state, and never handles a `RowId`: entity space only.
//!
//! [`FragmentCache`] persists the result as a CRoaring `Frozen`-format bitmap in an engine-local
//! cache directory, never in the bundle, so a repeated grant set reuses the on-disk fragment
//! across process restarts. The cache key is wider than the set of granted terms; see
//! [`FragmentCache::new`]. Entries are content-addressed with a stored SHA-256 digest verified on
//! every reopen, before the unsafe `Frozen` view is constructed; writes are fsynced before the
//! rename that makes them visible; the directory and its files are owner-only on unix.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use croaring::{Bitmap, BitmapView, Frozen};
use sha2::{Digest, Sha256};

use tessera_types::TermId;

use crate::postings::{invalid_data, union_postings, PostingRef, PostingsReader};
use tessera_cache::{CacheWeight, SingleFlightCache, SingleFlightError};
use crate::tier::DeltaTier;

/// The in-memory tier's operator gauges, re-exported so [`FragmentCache::stats`]'s return type
/// is nameable at this path. A server-plane caller uses `tessera_engine::FragmentCacheStats`,
/// which re-exports this one: `tessera-server` may not depend on `tessera-authz`.
pub use tessera_cache::CacheStats;

/// Union the postings of every term in `terms` into one bitmap: this is the authorisation
/// decision. `terms` and the returned bitmap are entity-space, never row-space.
pub fn build_fragment(terms: &[TermId], postings: &PostingsReader) -> io::Result<Bitmap> {
    build_fragment_with_deltas(terms, postings, &[])
}

/// [`build_fragment`] over the base postings and every live delta tier: the union, per satisfied
/// term, of the base posting and that term's posting in each tier that carries it.
///
/// The union is over `terms`, never over a tier's whole term set: a tier holds the postings of
/// every term its flushed items carried, including terms this session was never granted, and
/// unioning a tier wholesale would hand a viewer entities it was never granted.
pub fn build_fragment_with_deltas(
    terms: &[TermId],
    postings: &PostingsReader,
    deltas: &[Arc<DeltaTier>],
) -> io::Result<Bitmap> {
    let mut sources = Vec::new();
    collect_postings(&mut sources, terms, Some(postings), deltas)?;

    let mut fragment = union_postings(sources);
    fragment.run_optimize();

    Ok(fragment)
}

/// Append each of `terms`' postings, the base's where `base` is given, then every tier's, to
/// `sources`. Never appends a tier's whole term set, only the terms given.
fn collect_postings<'a>(
    sources: &mut Vec<PostingRef<'a>>,
    terms: &[TermId],
    base: Option<&'a PostingsReader>,
    deltas: &'a [Arc<DeltaTier>],
) -> io::Result<()> {
    for term in terms.iter().copied() {
        if let Some(base) = base {
            sources.extend(base.posting(term)?);
        }
        for tier in deltas {
            sources.extend(tier.posting(term)?);
        }
    }
    Ok(())
}

/// The entities `fragment` holds that the `kept` terms' base postings do not cover, as a
/// superset that is still inside the fragment.
///
/// A session whose row projection is built from term images unions the images of the terms in
/// `kept` and walks whatever those images cannot have covered. The images are projections of
/// base postings alone, so what is left is the `unkept` terms in full plus every kept term's
/// delta postings, which no image carries: a superset of what is strictly missing, which is
/// enough, since projecting a superset inside the fragment adds no row the fragment does not
/// grant.
///
/// The intersection with `fragment` is required: `deltas` is the live generation's tier list and
/// can be newer than the tiers the fragment was unioned from, so without it the result could
/// carry an entity outside the fragment, serving a row the principal was never granted.
pub fn residual_fragment(
    unkept: &[TermId],
    kept: &[TermId],
    postings: &PostingsReader,
    deltas: &[Arc<DeltaTier>],
    fragment: &Bitmap,
) -> io::Result<Bitmap> {
    let mut sources = Vec::new();
    collect_postings(&mut sources, unkept, Some(postings), deltas)?;
    collect_postings(&mut sources, kept, None, deltas)?;

    let mut residual = union_postings(sources);
    residual.and_inplace(fragment);
    residual.run_optimize();
    Ok(residual)
}

/// The sum of `terms`' delta-posting cardinalities across every live tier, in entities. A sum
/// rather than the cardinality of a union, so an entity carried by two tiers is counted twice,
/// which biases the residual walk's costed route choice toward the walk.
pub fn delta_entities(terms: &[TermId], deltas: &[Arc<DeltaTier>]) -> io::Result<u64> {
    let mut entities = 0u64;
    for term in terms.iter().copied() {
        for tier in deltas {
            if let Some(posting) = tier.posting(term)? {
                entities += posting.cardinality();
            }
        }
    }
    Ok(entities)
}

/// The canonical cache key: SHA-256 over `bundle_identity ‖ auth_plugin_hash ‖ watermark ‖
/// sorted, deduplicated term_id u32 LEs`. Term ids are bundle-relative ordinals, so a persistent
/// cache directory reused across bundle rebuilds, or across an auth plugin upgrade, would
/// otherwise serve a frozen fragment naming a different entity set.
fn canonical_key(
    bundle_identity: &[u8; 32],
    auth_plugin_hash: &[u8; 32],
    terms: &[TermId],
    watermark: u64,
) -> [u8; 32] {
    let mut sorted: Vec<u32> = terms.iter().copied().map(TermId::raw).collect();
    sorted.sort_unstable();
    sorted.dedup();

    let mut hasher = Sha256::new();
    hasher.update(bundle_identity);
    hasher.update(auth_plugin_hash);
    hasher.update(watermark.to_le_bytes());
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

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Append a unique `.<pid>.<counter>.tmp` suffix to `path`'s full file name, for
/// write-then-rename. Unique per call, not per target path: two concurrent builds racing to the
/// same canonical key must not write through one shared tmp path, where an unsynchronised write
/// from each could interleave and leave a corrupt file behind before either rename lands.
fn tmp_sibling(path: &Path) -> PathBuf {
    let unique = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.{unique}.tmp", std::process::id()));
    PathBuf::from(name)
}

/// Create `dir` and any missing ancestors with owner-only permissions on unix (`0700`); on other
/// platforms this is `create_dir_all` with the platform default. A mask fragment names the
/// viewer's exact visible set, so a per-principal cache entry must not be readable by other users
/// where the platform lets us say so.
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

/// A frozen, memory-mapped fragment reopened from the cache directory. `view()` borrows straight
/// from the mapping: no copy, no per-lookup deserialisation cost beyond CRoaring's own
/// pointer-fixup.
pub struct FrozenFragment {
    mmap: memmap2::Mmap,
    /// The generation's watermark at build time, as passed to [`FragmentCache::get_or_build`].
    /// Persisted alongside the frozen bytes so a reopened fragment restores it.
    pub watermark: u64,
    /// The bundle identity of the [`FragmentCache`] that produced this fragment: the digest of
    /// the prefix whose postings it was unioned from. Not persisted: [`canonical_key`] already
    /// hashes the identity, so this field just records which one produced the file found.
    ///
    /// A compaction rewrites the term index and publishes a new prefix. A holder that kept a
    /// fragment across a fold, such as a `Session`, would go on composing against a mask that
    /// still contains every folded-away entity, re-exposing what the fold retired. The
    /// comparison against this field is made where the fragment is composed.
    pub identity: [u8; 32],
}

impl FrozenFragment {
    /// View the frozen bitmap. Safe because every `FrozenFragment` was constructed by
    /// [`open`](Self::open), which checks the mapped length and the mapped bytes' SHA-256 digest
    /// against the sidecar record, closing the gap a length-only check would leave for
    /// right-length garbage from a torn write. The mapping's base address is page-aligned,
    /// satisfying `Frozen::REQUIRED_ALIGNMENT`.
    pub fn view(&self) -> BitmapView<'_> {
        // SAFETY: `open()` is the only constructor of `FrozenFragment` and it verifies length
        // and digest before returning `Ok`.
        unsafe { BitmapView::deserialize::<Frozen>(&self.mmap[..]) }
    }

    /// Open an existing `(frag_path, meta_path)` pair, verifying the sidecar-recorded length and
    /// SHA-256 digest against the mapped file before ever calling the unsafe `Frozen` view
    /// deserialiser. Fails closed (`InvalidData`) on any mismatch, truncation, or malformed
    /// sidecar: a corrupt or tampered cache entry must never reach
    /// `BitmapView::deserialize::<Frozen>`.
    fn open(frag_path: &Path, meta_path: &Path, identity: [u8; 32]) -> io::Result<Self> {
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
        // SAFETY: read-only here and inside `FrozenFragment`. `FragmentCache` never mutates a
        // `.frag` file in place, only write-then-rename under a fresh temp name.
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

        Ok(FrozenFragment {
            mmap,
            watermark,
            identity,
        })
    }

    /// Serialise `bitmap` in `Frozen` format and persist it, plus its watermark/length/digest
    /// sidecar, under `(frag_path, meta_path)` via write-then-rename, then reopen it as a
    /// `FrozenFragment` through the same validated-open path a cache hit would use.
    fn build_and_persist(
        frag_path: &Path,
        meta_path: &Path,
        bitmap: &Bitmap,
        watermark: u64,
        identity: [u8; 32],
    ) -> io::Result<Self> {
        // The slice's own bytes are the exact frozen buffer with no front padding, so writing
        // them at file offset 0 reproduces the identical buffer on reopen.
        let mut scratch = Vec::new();
        let frozen_bytes = bitmap.serialize_into_vec::<Frozen>(&mut scratch);
        let frozen_len = frozen_bytes.len() as u64;
        let digest: [u8; 32] = Sha256::digest(&*frozen_bytes).into();

        // Frag before meta, both fsynced before their rename: a present meta file is then a
        // trustworthy signal that the pair is complete.
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

        // Fsync the containing directory so both renames' directory-entry updates are durable.
        if let Some(parent) = frag_path.parent() {
            if let Ok(dir_file) = File::open(parent) {
                let _ = dir_file.sync_all();
            }
        }

        Self::open(frag_path, meta_path, identity)
    }
}

/// `watermark: u64 LE (8) ‖ frozen_len: u64 LE (8) ‖ sha256(frozen_bytes) (32)`.
const META_LEN: usize = 48;

impl CacheWeight for FrozenFragment {
    /// The mapped file's length. Bounds address space, not resident memory: a fragment is
    /// resident only in the pages actually touched, and evicting one frees nothing while any
    /// live `Session` still holds its own `Arc<FrozenFragment>`.
    fn cache_weight_bytes(&self) -> u64 {
        self.mmap.len() as u64
    }
}

/// Frozen fragments, held in memory and persisted under a directory.
///
/// An entry is named by its canonical key: SHA-256 over the bundle identity, the auth plugin's
/// hash, the watermark and the sorted, deduplicated granted terms. Term ids are ordinals of one
/// bundle, so a key narrower than that would serve one bundle's entity set under another's.
///
/// On disk an entry is `<hex key>.frag`, the `Frozen` bitmap bytes, and `<hex key>.meta`, which
/// holds `watermark: u64 LE ‖ frozen_len: u64 LE ‖ sha256(frozen_bytes)`. Each file is written to
/// a temporary sibling, synced and renamed, and [`FrozenFragment::open`] checks the length and
/// the digest before it views the bytes, so a truncated, corrupt or altered entry is a miss.
///
/// In memory, a slot per canonical key is either building or ready. A ready hit does no file IO.
/// An arrival on a key that is building does not wait; it gets [`FragmentCacheError::Building`].
pub struct FragmentCache {
    dir: PathBuf,
    bundle_identity: [u8; 32],
    auth_plugin_hash: [u8; 32],
    slots: SingleFlightCache<[u8; 32], FrozenFragment>,
    rebuilds: AtomicU64,
}

/// [`FragmentCache::get_or_build`]'s failure modes. Neither variant is ever cached.
#[derive(Debug)]
pub enum FragmentCacheError {
    /// Another caller is already building this exact canonical key. This call did not wait for
    /// it; retry shortly.
    Building,
    /// The build itself failed. The failing canonical key was removed before this was returned,
    /// so a failure does not permanently wedge a credential.
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
    /// `dir` is the engine's local cache directory for this bundle/auth-plugin pair, never a
    /// path inside the bundle itself. Does not touch the filesystem; `get_or_build` creates
    /// `dir` and any missing ancestors on first write.
    ///
    /// The in-memory tier's byte bound is not a constructor argument; it arrives through
    /// [`Self::set_memory_bound`]. A cache built this way is unbounded, which is correct for
    /// tests and benches; `tessera_server::prepare` makes sure a server never gets one.
    pub fn new(dir: &Path, bundle_identity: [u8; 32], auth_plugin_hash: [u8; 32]) -> Self {
        FragmentCache {
            dir: dir.to_path_buf(),
            bundle_identity,
            auth_plugin_hash,
            slots: SingleFlightCache::new(u64::MAX),
            rebuilds: AtomicU64::new(0),
        }
    }

    /// An empty cache over the same directory and auth plugin under a new bundle identity, which
    /// is what a compaction's publication installs. Every slot is keyed under the old identity,
    /// so none carries over; the persisted pairs become unreachable and are left to
    /// [`Self::sweep`]. The byte bound carries over.
    pub fn rotate(&self, bundle_identity: [u8; 32]) -> Self {
        FragmentCache {
            dir: self.dir.clone(),
            bundle_identity,
            auth_plugin_hash: self.auth_plugin_hash,
            slots: SingleFlightCache::new(self.slots.stats().bound_bytes),
            rebuilds: AtomicU64::new(0),
        }
    }

    /// Every persisted entry present now, the set a rotation supersedes. An entry is not
    /// selectable by name, since the key is a SHA-256 and a hash does not invert, but at the
    /// instant the identity rotates every existing entry is under the superseded one, so
    /// everything present now is exactly the set to sweep.
    ///
    /// Separate from [`Self::sweep`] so callers list before the swap and delete after it:
    /// deleting before the swap would discard a cache still live if the publication then fails,
    /// and deleting after it by re-listing would race a request that authorised in between.
    ///
    /// A directory that cannot be read yields an empty list rather than an error.
    pub fn superseded_entries(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext == "frag" || ext == "meta")
            })
            .collect()
    }

    /// Delete the entries [`Self::superseded_entries`] named. Returns how many were removed. An
    /// associated function, not a method: by the time it runs, the cache it belongs to has been
    /// replaced by the rotated cache.
    ///
    /// Unlinking an entry a live request still holds is safe on POSIX: a mapping outlives the
    /// directory entry, so a `Session` or an in-memory slot holding one keeps reading the same
    /// bytes, and the unlink removes only the name. Failures are counted out rather than
    /// propagated.
    pub fn sweep(entries: &[PathBuf]) -> usize {
        entries
            .iter()
            .filter(|path| std::fs::remove_file(path).is_ok())
            .count()
    }

    /// The bundle identity every key in this cache is computed under. Compared against
    /// [`FrozenFragment::identity`] wherever a fragment a caller already holds is composed.
    pub fn bundle_identity(&self) -> [u8; 32] {
        self.bundle_identity
    }

    /// Bound the in-memory tier at `bytes`. The digest-verified `.frag` sidecar tier is untouched
    /// by it; see [`Self::evict`].
    pub fn set_memory_bound(&self, bytes: u64) {
        self.slots.set_bound_bytes(bytes);
    }

    /// The canonical cache key for `satisfied` under this cache's bundle and plugin identity, the
    /// only way to name an entry from outside and so what [`Self::evict`] takes.
    pub fn canonical_key_for(&self, satisfied: &[TermId], watermark: u64) -> [u8; 32] {
        canonical_key(
            &self.bundle_identity,
            &self.auth_plugin_hash,
            satisfied,
            watermark,
        )
    }

    /// Drop one entry from the in-memory tier. Returns whether anything was there. Frees only
    /// the in-memory slot: the `.frag`/`.meta` sidecar pair is left on disk, digest-verified on
    /// every reopen, so eviction costs the next caller a re-open plus a SHA-256, never
    /// correctness.
    pub fn evict(&self, key: &[u8; 32]) -> bool {
        self.slots.evict(key)
    }

    /// The operator gauges for the in-memory tier; see [`CacheStats`]. Lock-free.
    pub fn stats(&self) -> CacheStats {
        self.slots.stats()
    }

    /// Number of times [`get_or_build`](Self::get_or_build) has actually called
    /// [`build_fragment`] rather than reusing an existing frozen fragment. Increments exactly
    /// once per single-flight build.
    pub fn rebuild_count(&self) -> u64 {
        self.rebuilds.load(Ordering::Relaxed)
    }

    /// Canonical-key slots currently held, building and ready both counted.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Entries live flat in `dir`, named by their canonical key. A cache an upgraded binary
    /// cannot read is reclaimed by deleting the cache directory; it is a derived artefact and
    /// rebuilds itself.
    fn frag_path(&self, key: &[u8; 32]) -> PathBuf {
        self.dir.join(format!("{}.frag", hex_encode(key)))
    }

    fn meta_path(&self, key: &[u8; 32]) -> PathBuf {
        self.dir.join(format!("{}.meta", hex_encode(key)))
    }

    /// The `.frag` path an entry for `satisfied` at `watermark` occupies. Exposed so that two
    /// watermarks being two entries is assertable on the paths themselves.
    pub fn path_of(&self, satisfied: &[TermId], watermark: u64) -> PathBuf {
        self.frag_path(&canonical_key(
            &self.bundle_identity,
            &self.auth_plugin_hash,
            satisfied,
            watermark,
        ))
    }

    /// The frozen fragment for `satisfied`, the terms a credential grants, at `watermark`.
    ///
    /// A ready slot answers from memory. Otherwise the persisted pair is opened if it verifies,
    /// whichever process wrote it, and failing that the fragment is built from `postings` and
    /// `deltas` and persisted.
    ///
    /// The watermark is in the key because it names the set of live tiers: two builds over one
    /// grant and different tiers differ by the entities the newer tiers carry. A merge re-encodes
    /// tiers without changing their content and does not move the watermark, so its fragment is
    /// the one already cached.
    pub fn get_or_build(
        &self,
        satisfied: &[TermId],
        postings: &PostingsReader,
        deltas: &[Arc<DeltaTier>],
        watermark: u64,
    ) -> Result<Arc<FrozenFragment>, FragmentCacheError> {
        let key = self.canonical_key_for(satisfied, watermark);

        self.slots
            .get_or_try_build(key, || {
                let frag_path = self.frag_path(&key);
                let meta_path = self.meta_path(&key);

                // No existence pre-check: `open()` fails closed on anything short of a fully
                // valid, digest-matching pair, so a missing file and a corrupt one are both a
                // miss here.
                if let Ok(frozen) =
                    FrozenFragment::open(&frag_path, &meta_path, self.bundle_identity)
                {
                    return Ok(frozen);
                }

                create_private_dir_all(&self.dir)?;
                let bitmap = build_fragment_with_deltas(satisfied, postings, deltas)?;
                self.rebuilds.fetch_add(1, Ordering::Relaxed);
                FrozenFragment::build_and_persist(
                    &frag_path,
                    &meta_path,
                    &bitmap,
                    watermark,
                    self.bundle_identity,
                )
            })
            .map_err(|e| match e {
                SingleFlightError::Building => FragmentCacheError::Building,
                SingleFlightError::Build(io_err) => FragmentCacheError::Io(io_err),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One term's entity list in one delta tier, as the tier writer takes it.
    type TierEntry = (u32, Vec<u32>);

    struct Corpus {
        base: Vec<Vec<u32>>,
        tiers: Vec<Vec<TierEntry>>,
    }

    /// A reproducible random corpus: per-term base entity lists, and two sparse tiers each
    /// carrying a subset of the terms with entities drawn from a higher range.
    fn random_corpus(seed: u64) -> Corpus {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(seed);
        let terms = 24usize;
        let base: Vec<Vec<u32>> = (0..terms)
            .map(|_| {
                let mut entities: Vec<u32> = (0..2_000u32).filter(|_| rng.gen_bool(0.05)).collect();
                entities.dedup();
                entities
            })
            .collect();
        let mut tiers: Vec<Vec<(u32, Vec<u32>)>> = Vec::new();
        for tier in 0..2u32 {
            let lo = 2_000 + tier * 1_000;
            let mut entries: Vec<(u32, Vec<u32>)> = Vec::new();
            for term in 0..terms as u32 {
                if !rng.gen_bool(0.4) {
                    continue;
                }
                let entities: Vec<u32> = (lo..lo + 1_000).filter(|_| rng.gen_bool(0.05)).collect();
                entries.push((term, entities));
            }
            tiers.push(entries);
        }
        Corpus { base, tiers }
    }

    /// Write `base` and `tiers` through the real writers and open them through the real readers.
    fn readers(
        dir: &Path,
        base: &[Vec<u32>],
        tiers: &[Vec<TierEntry>],
    ) -> (PostingsReader, Vec<Arc<DeltaTier>>) {
        let postings_path = dir.join("postings.arrow");
        crate::postings::write_postings(&postings_path, base, 32).unwrap();
        let reader = PostingsReader::open(&postings_path, false).unwrap();
        let opened = tiers
            .iter()
            .enumerate()
            .map(|(n, entries)| {
                let path = dir.join(format!("tier-{n}.arrow"));
                crate::tier::write_delta_tier_at(&path, entries, 32).unwrap();
                Arc::new(DeltaTier::open(&path).unwrap())
            })
            .collect();
        (reader, opened)
    }

    /// The expected fragment, assembled from the source lists rather than from the readers.
    fn expected_union(terms: &[TermId], base: &[Vec<u32>], tiers: &[Vec<TierEntry>]) -> Bitmap {
        let mut expected = Bitmap::new();
        for term in terms.iter().copied() {
            let raw = term.raw();
            if let Some(entities) = base.get(raw as usize) {
                expected.add_many(entities);
            }
            for tier in tiers {
                for (carried, entities) in tier {
                    if *carried == raw {
                        expected.add_many(entities);
                    }
                }
            }
        }
        expected
    }

    /// A fragment must be the pointwise union, over the terms a session satisfies, of each
    /// term's base posting and its posting in every live tier.
    #[test]
    fn a_fragment_is_the_pointwise_union_of_its_terms_base_and_delta_postings() {
        let temp = tempfile::TempDir::new().unwrap();
        for seed in 0..8u64 {
            let dir = temp.path().join(format!("seed-{seed}"));
            std::fs::create_dir_all(&dir).unwrap();
            let Corpus { base, tiers } = random_corpus(seed);
            let (reader, opened) = readers(&dir, &base, &tiers);

            let terms: Vec<TermId> = (0..base.len() as u32)
                .filter(|t| t % 3 != 0)
                .map(TermId::new)
                .collect();
            let built = build_fragment_with_deltas(&terms, &reader, &opened).unwrap();
            let expected = expected_union(&terms, &base, &tiers);
            assert_eq!(
                built.to_vec(),
                expected.to_vec(),
                "seed {seed}: the fragment is not the pointwise union of its terms' postings"
            );
        }
    }

    /// The residual is inside the fragment and covers everything the kept terms' base postings
    /// do not. Checked against a fragment built from fewer tiers than the residual is given.
    #[test]
    fn the_residual_lies_inside_the_fragment_and_covers_what_the_kept_terms_do_not() {
        let temp = tempfile::TempDir::new().unwrap();
        for seed in 0..8u64 {
            let dir = temp.path().join(format!("seed-{seed}"));
            std::fs::create_dir_all(&dir).unwrap();
            let Corpus { base, tiers } = random_corpus(seed);
            let (reader, opened) = readers(&dir, &base, &tiers);

            let terms: Vec<TermId> = (0..base.len() as u32).map(TermId::new).collect();
            let (kept, unkept): (Vec<TermId>, Vec<TermId>) =
                terms.iter().partition(|t| t.raw() % 2 == 0);

            let fragment = build_fragment_with_deltas(&terms, &reader, &opened[..1]).unwrap();
            let residual = residual_fragment(&unkept, &kept, &reader, &opened, &fragment).unwrap();

            assert!(
                residual.and(&fragment) == residual,
                "seed {seed}: the residual reaches outside the fragment, which is an I2 \
                 disclosure once it is projected"
            );

            let mut covered = Bitmap::new();
            for term in kept.iter().copied() {
                if let Some(entities) = base.get(term.raw() as usize) {
                    covered.add_many(entities);
                }
            }
            let missing = fragment.andnot(&covered);
            assert!(
                missing.andnot(&residual).is_empty(),
                "seed {seed}: the residual misses entities no kept term's base posting covers, \
                 so the split route would serve fewer rows than the walk"
            );
        }
    }

}
