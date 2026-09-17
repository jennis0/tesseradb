//! Mask fragment build (the authorise path) and the directory-backed frozen fragment cache.
//!
//! A mask fragment *is* the authorisation decision (I2): [`build_fragment`] unions the postings
//! of every term a viewer's credential satisfies into one bitmap, entirely from
//! `M_auth`-eligible inputs — no other quantity is derived and then gated. This module is
//! parametric over [`PostingsReader`]: it holds no lifecycle state, and `RowId` never appears
//! here (SA §3's crate-dependency rule; entity space only).
//!
//! [`FragmentCache`] persists the result as a CRoaring `Frozen`-format bitmap in an engine-local
//! cache directory — never in the bundle, whose contents are fixed by contracts §2.1 — so that a
//! repeated grant set
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
use crate::single_flight::{CacheWeight, SingleFlightCache, SingleFlightError};
use crate::tier::DeltaTier;

/// The in-memory tier's operator gauges, re-exported here so that [`FragmentCache::stats`]'s
/// return type is **nameable** by a caller outside this crate.
///
/// `crate::single_flight` is a private module, so `tessera_authz::CacheStats` is not a public path
/// at all: a caller could invoke `stats()` and infer the type, but could not write it in a
/// signature, a struct field or a `use`. So `Engine::fragment_cache().stats() ->
/// tessera_authz::CacheStats` does not compile, and its obvious repair — adding a `tessera-authz`
/// dependency to `tessera-server` — is a layering violation `scripts/check-layers.sh` refuses
/// (`deny tessera-server tessera-authz`).
///
/// The path a server-plane caller should use is `tessera_engine::FragmentCacheStats`, which
/// re-exports this one.
pub use crate::single_flight::CacheStats;

/// Union the postings of every term in `terms` into one bitmap: this *is* the authorisation
/// decision (I2). Partitions the granted postings into Roaring views (unioned in bulk via
/// [`Bitmap::fast_or`] — croaring 2.7.0's binding for `roaring_bitmap_or_many`, the bulk-union
/// entry point) and small arrays (decoded, concatenated,
/// sorted, and folded in with `add_many`), then `run_optimize`s the result.
///
/// Parametric: takes `postings` as an argument and holds no lifecycle state of its own. `RowId`
/// must not appear anywhere in this crate — `terms` and the returned bitmap are both
/// entity-space, never row-space.
pub fn build_fragment(terms: &[TermId], postings: &PostingsReader) -> io::Result<Bitmap> {
    build_fragment_with_deltas(terms, postings, &[])
}

/// [`build_fragment`] over the base postings **and every live delta tier**.
///
/// Flush publishes one sparse tier per segment — only the terms present in its flushed set — so a
/// build is the union, per satisfied term, of the base posting and that term's posting in each
/// tier that carries it. A tier that does not carry the term contributes nothing at zero cost.
///
/// **The union is over `terms`, never over a tier's whole term set** (I2): a tier holds the
/// postings of every term its flushed items carried, including terms this session was never
/// granted, and unioning a tier wholesale would hand a viewer entities outside `M_auth`.
///
/// Over zero tiers this is byte-for-byte what a base-only build produces, which is what made it
/// landable before any flush existed.
pub fn build_fragment_with_deltas(
    terms: &[TermId],
    postings: &PostingsReader,
    deltas: &[Arc<DeltaTier>],
) -> io::Result<Bitmap> {
    let mut views: Vec<BitmapView<'_>> = Vec::new();
    let mut small: Vec<u32> = Vec::new();

    // The base and the tiers are read by one loop over one `PostingRef` shape: the two files
    // differ in how a term is *found* (an ordinal index against a binary search) and not in what
    // a posting is, and the union does not care which file an entity came from — only that the
    // term is satisfied.
    for term in terms.iter().copied() {
        let base = postings.posting(term)?;
        for posting in base.into_iter().chain(
            deltas
                .iter()
                .map(|tier| tier.posting(term))
                .collect::<io::Result<Vec<_>>>()?
                .into_iter()
                .flatten(),
        ) {
            match posting {
                PostingRef::Roaring(view) => views.push(view),
                PostingRef::Array(bytes) => {
                    // `PostingsReader::open` validates every tag-0 payload's length is a
                    // multiple of 4 once, at open time — this is not re-checked per lookup, so a
                    // violation here would mean that validation was bypassed, not that this call
                    // site needs its own fail-closed handling.
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

/// **S for the split route** (`architecture.md` §6.3): the
/// entities `fragment` holds that the `kept` terms' base postings do not cover, as a superset that
/// is still inside the fragment.
///
/// A session whose row projection is built from term images unions the images of the terms in
/// `kept` and walks whatever those images cannot have covered. The images are projections of base
/// postings alone, so what is left is the `unkept` terms in full plus every kept term's *delta*
/// postings, which no image carries. That is a superset of what is strictly missing, which is all
/// the union needs: the projection of a superset inside the fragment adds no row the fragment does
/// not grant.
///
/// **The intersection with `fragment` is not an optimisation.** `deltas` is the live generation's
/// tier list and can be newer than the tiers the fragment was unioned from, so without it the
/// result could carry an entity outside the fragment, and projecting that entity would serve a row
/// the principal was never granted (I2). Intersecting makes `S ⊆ F` hold for any tier list the
/// caller passes, rather than for the one the fragment was unioned from alone.
///
/// `RowId` does not appear here: both arguments and the result are entity-space, and the caller
/// projects.
pub fn residual_fragment(
    unkept: &[TermId],
    kept: &[TermId],
    postings: &PostingsReader,
    deltas: &[Arc<DeltaTier>],
    fragment: &Bitmap,
) -> io::Result<Bitmap> {
    let mut residual = build_fragment_with_deltas(unkept, postings, deltas)?;
    or_delta_postings(&mut residual, kept, deltas)?;
    residual.and_inplace(fragment);
    residual.run_optimize();
    Ok(residual)
}

/// The sum of `terms`' delta-posting cardinalities across every live tier: the route chooser's
/// residual overcount, in entities.
///
/// It is a sum rather than the cardinality of a union, so an entity carried by two tiers is
/// counted twice. The chooser prices the residual walk with it, and an overcount biases the choice
/// toward the walk, which is the route whose cost is measured over the widest set of principals.
pub fn delta_entities(terms: &[TermId], deltas: &[Arc<DeltaTier>]) -> io::Result<u64> {
    let mut entities = 0u64;
    for term in terms.iter().copied() {
        for tier in deltas {
            match tier.posting(term)? {
                Some(PostingRef::Roaring(view)) => entities += view.cardinality(),
                Some(PostingRef::Array(bytes)) => entities += (bytes.len() / 4) as u64,
                None => {}
            }
        }
    }
    Ok(entities)
}

/// Union `terms`' postings **in the delta tiers only** into `into`, leaving the base unread.
///
/// The union is over `terms` and never over a tier's whole term set, for
/// [`build_fragment_with_deltas`]' reason: a tier carries the postings of every term its flushed
/// items held, including terms this session was never granted.
fn or_delta_postings(
    into: &mut Bitmap,
    terms: &[TermId],
    deltas: &[Arc<DeltaTier>],
) -> io::Result<()> {
    let mut small: Vec<u32> = Vec::new();
    for term in terms.iter().copied() {
        for tier in deltas {
            match tier.posting(term)? {
                Some(PostingRef::Roaring(view)) => into.or_inplace(&view),
                Some(PostingRef::Array(bytes)) => {
                    debug_assert!(
                        bytes.len() % 4 == 0,
                        "tag-0 posting payload length must be a multiple of 4 (validated at \
                         DeltaTier::open)"
                    );
                    for chunk in bytes.chunks_exact(4) {
                        small.push(u32::from_le_bytes(chunk.try_into().unwrap()));
                    }
                }
                None => {}
            }
        }
    }
    small.sort_unstable();
    into.add_many(&small);
    Ok(())
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
    /// The bundle identity of the [`FragmentCache`] that produced this fragment — the MANIFEST
    /// digest of the prefix whose postings it was unioned from.
    ///
    /// **Not persisted, and it does not need to be**: [`canonical_key`] already hashes the
    /// identity, so a `.frag` file can only ever be *found* under a key carrying the identity it
    /// was built under. This field records which one that was, so a holder outside the cache can
    /// ask.
    ///
    /// **What it is for.** A compaction rewrites the term index and publishes a new prefix, so
    /// the postings a pre-fold fragment was unioned from no longer describe the live bundle — and
    /// a fold advances no watermark, so the watermark test cannot see it. A holder that kept a
    /// fragment across a fold (a `Session`, which holds one for its whole lifetime) would go on
    /// composing against a mask that still contains every folded-away entity, which is Rule F's
    /// retirement re-exposing exactly what it retired (write-path §5.4). The comparison is made
    /// where the fragment is *composed*, not arranged for by swapping the cache: a cache swap
    /// does not reach a fragment somebody already holds by `Arc`.
    pub identity: [u8; 32],
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

        Ok(FrozenFragment {
            mmap,
            watermark,
            identity,
        })
    }

    /// Serialise `bitmap` in `Frozen` format and persist it (plus its watermark/length/digest
    /// sidecar) under `(frag_path, meta_path)` via write-then-rename, then reopen it as a
    /// `FrozenFragment` (exercising the same validated-open path a cache hit would use).
    fn build_and_persist(
        frag_path: &Path,
        meta_path: &Path,
        bitmap: &Bitmap,
        watermark: u64,
        identity: [u8; 32],
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

        Self::open(frag_path, meta_path, identity)
    }
}

/// `watermark: u64 LE (8) ‖ frozen_len: u64 LE (8) ‖ sha256(frozen_bytes) (32)`.
const META_LEN: usize = 48;

impl CacheWeight for FrozenFragment {
    /// The mapped file's length — exact, free, and it *is* the frozen buffer's own length (see
    /// [`FrozenFragment::open`], which refuses any mapping whose length disagrees with the
    /// sidecar).
    ///
    /// **This bounds address space, not resident memory, and the difference is bigger here than
    /// for the row-projection cache.** These are file mappings, so a fragment is resident only in
    /// the pages actually touched, and — the part that matters operationally — **evicting a
    /// fragment frees nothing while any live `Session` still holds it.** `Engine::authorise` hands
    /// each session an `Arc<FrozenFragment>` that it keeps for `token_max_lifetime_secs`, so N
    /// sessions sharing one grant set keep that mapping alive through any number of evictions of
    /// the cache's own reference. The bound therefore governs *this map*; it is not a ceiling on
    /// the process's mapped fragments, and an eviction of a hot fragment costs the next authorise a
    /// re-open and a SHA-256 while freeing nothing at all.
    fn cache_weight_bytes(&self) -> u64 {
        self.mmap.len() as u64
    }
}

/// The most `auth_data_hash → canonical_key` memoisations kept before the map is cleared.
///
/// **This bound closes an unbounded, attacker-driven allocation that the byte bound does not
/// reach.** `key_memo` is keyed by `SHA-256(auth_data)`, so its growth is driven by the number of
/// distinct *credentials* presented, not by the number of distinct grant sets. A caller holding the
/// session credential can POST `/session/authorise` with random `auth_data` whose descriptors are
/// all unknown to the dictionary: `Engine::authorise` drops unknown descriptors silently, so
/// `satisfied` is empty, the canonical key is identical every time, [`Self::slots`] takes a `Ready`
/// hit and builds nothing — while this map grows by a fresh 64-byte entry plus overhead on every
/// call, for ever. No fragment build, no disk IO, and nothing in the byte accounting moves.
///
/// **Clearing the whole map rather than evicting one entry is deliberate and cheap.** This map is
/// *pure memoisation* of [`canonical_key`] (see this type's doc): discarding it costs one re-derive
/// — a sort, a dedup and a SHA-256 over the granted term list — and never a wrong answer. An LRU
/// here would be a second recency structure to keep in step for no correctness gain.
///
/// 4096 is sized as "comfortably more distinct credentials than a single-node deployment presents
/// between clears" — assumed, not measured; at ~80 B per entry it caps this map at ~330 KB.
const KEY_MEMO_MAX_ENTRIES: usize = 4096;

/// What a canonical key is memoised against: the credential, the **generation stamp its terms were
/// resolved against**, and the watermark the fragment covers. All three, because each of them alone
/// changes what the same credential's fragment contains over time — see
/// [`FragmentCache::get_or_build`].
type KeyMemoKey = ([u8; 32], u64, u64);

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
    key_memo: Mutex<FxHashMap<KeyMemoKey, [u8; 32]>>,
    slots: SingleFlightCache<[u8; 32], FrozenFragment>,
    rebuilds: AtomicU64,
}

/// [`FragmentCache::get_or_build`]'s failure modes. Neither variant is ever cached (I13a
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
    /// path inside the bundle itself, whose contents are fixed by contracts §2.1.
    /// `bundle_identity` is the generation's MANIFEST digest; `auth_plugin_hash` is the active auth
    /// plugin's hash. Does not touch the filesystem; `get_or_build` creates `dir` (and any missing
    /// ancestors) on first write.
    ///
    /// **The in-memory tier's byte bound is deliberately not a constructor argument.** It arrives
    /// through [`Self::set_memory_bound`], which `tessera-server` calls at startup after validating
    /// it — the same shape `Engine::start_write_executor` uses for `ingest_queue_bound`, and for
    /// the same reason: the bound is a validated deployment setting, and the constructor's many
    /// test, bench and embedder call sites have no opinion on it.
    ///
    /// A cache built this way is therefore **unbounded**. That is correct for tests, benches and
    /// embedders; it is not correct for a server, and `tessera_server::prepare` is what makes sure
    /// a server never gets one.
    pub fn new(dir: &Path, bundle_identity: [u8; 32], auth_plugin_hash: [u8; 32]) -> Self {
        FragmentCache {
            dir: dir.to_path_buf(),
            bundle_identity,
            auth_plugin_hash,
            key_memo: Mutex::new(FxHashMap::default()),
            slots: SingleFlightCache::new(u64::MAX),
            rebuilds: AtomicU64::new(0),
        }
    }

    /// A cache over the same directory and the same auth plugin, under a **new bundle identity**
    /// — what a compaction's publication installs, and the only way this identity ever changes.
    ///
    /// **Rotation is a fresh cache, never a mutation of this one, because both maps are keyed
    /// under the old identity.** `slots` is keyed by the canonical key, which hashes the identity;
    /// `key_memo` maps a credential to a canonical key it computed under the identity. Storing a
    /// new identity in place would leave every entry in both maps reachable by a key no live
    /// lookup can produce for `slots`, and — the fail-open — reachable by exactly the key a live
    /// lookup *does* produce for `key_memo`, which returns the memoised canonical key without
    /// re-deriving it. A post-fold authorise would then be handed the pre-fold fragment: every
    /// folded-away entity back in the mask, with no error. Starting empty makes that unexpressible
    /// rather than forbidden.
    ///
    /// The persisted `.frag`/`.meta` pairs are left alone and become unreachable for the same
    /// reason — their names are the old identity's keys, and nothing will ever compute one again.
    /// Sweeping them is reclamation's (compaction §8), not this call's.
    ///
    /// **The byte bound is carried across**, because it is a validated deployment setting that
    /// arrives once at startup ([`Self::set_memory_bound`]) and nothing would re-apply it. A
    /// rotation that silently unbounded the cache would undo the startup refusal
    /// `tessera_server::prepare` exists to enforce.
    pub fn rotate(&self, bundle_identity: [u8; 32]) -> Self {
        FragmentCache {
            dir: self.dir.clone(),
            bundle_identity,
            auth_plugin_hash: self.auth_plugin_hash,
            key_memo: Mutex::new(FxHashMap::default()),
            slots: SingleFlightCache::new(self.slots.stats().bound_bytes),
            rebuilds: AtomicU64::new(0),
        }
    }

    /// Every persisted entry present now — the set a rotation supersedes.
    ///
    /// # Why the sweep is "everything", and why it is two calls rather than one
    ///
    /// Compaction §8 asks a fold to sweep the persisted cache "of entries under superseded
    /// identities", and that set is **not selectable by name**: an entry is `<canonical key>.frag`,
    /// the key is a SHA-256 over the bundle identity among other things, and a hash does not
    /// invert. Nothing beside the file carries the identity either — the `.meta` sidecar holds a
    /// watermark, a length and a digest.
    ///
    /// It does not need to be selectable. **At the instant the identity rotates, every existing
    /// entry is under the superseded one**, so "everything present now" *is* the set §8 names,
    /// exactly rather than approximately. That is what makes this correct without the format change
    /// the alternative would need (decision 0055).
    ///
    /// **List before the swap, delete after it**, which is why this is separate from
    /// [`Self::sweep`]. Deleting before the swap discards a cache that is still the live one if the
    /// publication then fails; deleting after it, by re-listing, would race a request that
    /// authorised in between and wrote a *new* entry under the *new* identity. A listing taken
    /// before the swap names only superseded entries and can never name a later one, so the two
    /// hazards close together.
    ///
    /// A directory that cannot be read yields an empty list rather than an error: the sweep is
    /// reclamation of derived data, and a fold must not fail because a cache directory was
    /// unreadable.
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

    /// Delete the entries [`Self::superseded_entries`] named. Returns how many were removed.
    ///
    /// **An associated function, not a method**, because by the time it runs the cache it belongs
    /// to has been replaced: the live one is the rotated cache, and sweeping through *it* would
    /// read as sweeping its own entries. The paths are the whole of what this needs.
    ///
    /// **Unlinking an entry a live request still holds is safe.** A `FrozenFragment` is a mapping,
    /// and on POSIX a mapping outlives the directory entry; a `Session` or an in-memory slot
    /// holding one keeps reading the same bytes. What the unlink removes is the name, which nothing
    /// will compute again.
    ///
    /// Failures are counted out rather than propagated, for [`Self::superseded_entries`]' reason.
    pub fn sweep(entries: &[PathBuf]) -> usize {
        entries
            .iter()
            .filter(|path| std::fs::remove_file(path).is_ok())
            .count()
    }

    /// The bundle identity every key in this cache is computed under — the MANIFEST digest of the
    /// prefix whose postings its fragments were unioned from. Compared against
    /// [`FrozenFragment::identity`] wherever a fragment a caller already holds is composed.
    pub fn bundle_identity(&self) -> [u8; 32] {
        self.bundle_identity
    }

    /// Bound the **in-memory** tier at `bytes`. The digest-verified `.frag` sidecar tier is
    /// untouched by it — see [`Self::evict`].
    pub fn set_memory_bound(&self, bytes: u64) {
        self.slots.set_bound_bytes(bytes);
    }

    /// The canonical cache key for `satisfied` under this cache's bundle and plugin identity — the
    /// only way to name an entry from outside, and therefore what [`Self::evict`] takes.
    ///
    /// Public because evicting a *named* entry is impossible without it, and the key is otherwise
    /// computed only inside [`Self::get_or_build`]. It is a pure function of its inputs and reveals
    /// nothing a caller did not supply: the term set is the caller's own.
    pub fn canonical_key_for(&self, satisfied: &[TermId], watermark: u64) -> [u8; 32] {
        canonical_key(
            &self.bundle_identity,
            &self.auth_plugin_hash,
            satisfied,
            watermark,
        )
    }

    /// Drop one entry from the **in-memory** tier. Returns whether anything was there.
    ///
    /// **The `.frag`/`.meta` sidecar pair is deliberately left on disk.** It is digest-verified on
    /// every reopen ([`FrozenFragment::open`]), so an in-memory eviction costs the next caller a
    /// re-open plus SHA-256 over the frozen bytes — ~60–80 ms **modelled** at the 125 MB operating
    /// point — and never correctness. A caller that wants a genuinely cold rebuild (no mmap, no
    /// sidecar) must delete the pair itself; this method is not that, and a caller that offers the
    /// choice should say which of the two it means.
    ///
    /// Also note what eviction does *not* free: any live `Session` holding this fragment keeps its
    /// mapping alive regardless — see [`FrozenFragment`]'s [`CacheWeight`] impl.
    pub fn evict(&self, key: &[u8; 32]) -> bool {
        self.slots.evict(key)
    }

    /// The operator gauges for the in-memory tier — see [`CacheStats`]. Lock-free.
    ///
    /// These reach an operator as `/control/status`'s `fragment_cache` block, via
    /// `tessera_engine::Engine::fragment_cache_stats`.
    pub fn stats(&self) -> CacheStats {
        self.slots.stats()
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
    /// fail-closed tests confirming a failed build leaves no wedge (I13a), analogous to
    /// `tessera_engine::Engine::row_projection_cache_len`.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Entries currently memoised in `key_memo` — the observable that makes
    /// [`KEY_MEMO_MAX_ENTRIES`] a tested bound rather than a stated one.
    ///
    /// It exists because the round-1 review found that deleting the `memo.clear()` was caught by
    /// nothing **and could not have been**: there was no accessor, so no test could be written
    /// against the bound on one of the two attacker-driven allocation paths this task closes.
    ///
    /// `cfg(test)` rather than `pub`: this is a memoisation detail with no operator meaning — its
    /// size says how many distinct *credentials* have been presented since the last clear, not
    /// anything about the cache's memory or hit rate — and the surface an operator needs is
    /// [`Self::stats`]. Widening the public API to test an internal bound is the trade
    /// `crate::single_flight::SingleFlightCache::is_locked_now` refuses for the same reason.
    #[cfg(test)]
    fn key_memo_len(&self) -> usize {
        self.key_memo.lock().unwrap().len()
    }

    /// Entries live flat in `dir`, named by their canonical key.
    ///
    /// **No format version and no orphan sweep** (owner ruling, 2026-08-02; write-path §4.6). Both existed
    /// because the key changed shape when the watermark joined it, leaving every pre-upgrade entry
    /// unreachable — a leak rather than a fail-open, since new code can never read one — with
    /// nothing on this path deleting anything. Pre-alpha there are no pre-upgrade entries anywhere,
    /// so the machinery migrated from a state that has never existed. A cache an upgraded binary
    /// cannot read is reclaimed by deleting the cache directory; it is a derived artefact and
    /// rebuilds itself.
    fn frag_path(&self, key: &[u8; 32]) -> PathBuf {
        self.dir.join(format!("{}.frag", hex_encode(key)))
    }

    fn meta_path(&self, key: &[u8; 32]) -> PathBuf {
        self.dir.join(format!("{}.meta", hex_encode(key)))
    }

    /// The `.frag` path an entry for `satisfied` at `watermark` occupies. Exposed so that "two
    /// watermarks are two entries" is assertable on the paths themselves rather than inferred from
    /// two reads.
    pub fn path_of(&self, satisfied: &[TermId], watermark: u64) -> PathBuf {
        self.frag_path(&canonical_key(
            &self.bundle_identity,
            &self.auth_plugin_hash,
            satisfied,
            watermark,
        ))
    }

    /// Return the frozen fragment for `satisfied` (the terms a viewer's credential grants),
    /// building and persisting it if this is the first time this exact `(bundle_identity,
    /// auth_plugin_hash, satisfied)` combination has been seen — by *any* process sharing this
    /// cache directory, not just this one.
    ///
    /// **Caller obligation:** `auth_data_hash` must identify the *credential* whose evaluation
    /// produced `satisfied` — i.e. it must be a (collision-resistant) function of the same
    /// `auth_data` that the auth plugin evaluated to obtain `satisfied`; and `resolved_at` must be
    /// the generation stamp that resolution ran against. Together they must never arrive paired
    /// with two different term sets.
    ///
    /// **`resolved_at` is in the memo key because the same credential legitimately resolves to
    /// different term sets over time.** A flush promotes a novel descriptor to a durable ordinal
    /// (§3.2), so a credential naming it resolves to *more* terms after that flush than before —
    /// and `auth_data_hash` alone would then map to the older, smaller set, silently defeating the
    /// promotion and, if a dictionary could ever renumber, returning a fragment for the wrong grant
    /// set outright.
    ///
    /// **It is the generation's own stamp rather than the dictionary's length, and the difference
    /// is what the obligation rests on** (#112, 2026-08-14). Length was the natural proxy and is a
    /// faithful one only while three separate things hold: that a dictionary grows by appending
    /// within a prefix, that nothing but a fold removes or renumbers a term, and that a fold
    /// rotates this cache and so empties this map. The third does all the work — compaction sweeps
    /// terms, so a fold *can* leave a dictionary of a length it held before meaning something
    /// different — and it is exactly the fact a later change would break by keeping the cache warm
    /// across a rotation, which is an obvious thing to want. A monotone generation stamp needs none
    /// of them: the engine refuses any publication that does not strictly increase it, so a
    /// dictionary that changes at all changes this, and the dictionary may then be rebuilt however
    /// compaction likes. `dict_len` also carried an obligation on compaction to keep it monotone;
    /// that obligation is discharged rather than inherited.
    ///
    /// The in-memory canonical-key fast path trusts this: on a memo hit it returns the
    /// previously-computed canonical key *without* re-deriving it from `satisfied`, so a caller
    /// that violates the obligation would silently get back a fragment built for a *different*
    /// grant set — an I2 disclosure if that other set happens to be a superset. Debug builds catch
    /// a violation via a `debug_assert_eq!` against a freshly recomputed key; release builds do not
    /// re-check on the fast path (that would defeat its purpose), so this obligation is
    /// load-bearing in release too.
    ///
    /// `postings` and `deltas` supply the union inputs on a cache miss.
    ///
    /// **`watermark` is part of the key, because it is what identifies a tier set** (§9). Two
    /// builds over the same grant and different live tiers must not collide: they differ by the
    /// entities the newer tiers carry, and under one key which of them a session gets would be
    /// decided by whoever wrote last — a disclosure, not merely staleness. It is also what makes
    /// `tmp_sibling`'s "both writers wrote byte-identical content" argument hold again, which is
    /// the thing that makes a concurrent write-then-rename safe here.
    ///
    /// **A merge needs nothing of its own**, and that is why the watermark suffices rather than
    /// merely helping: a merge coalesces tiers as a content-preserving re-encode (§5.2), so the
    /// fragment it would produce is identical and reusing the pre-merge entry is correct. Only a
    /// flush changes what a build returns, and a flush moves the watermark.
    ///
    /// For the compaction author: a persisted fragment surviving a restart at a **pre-flush** stamp
    /// would falsify lifecycle §3.2's "the cache restarts cold" premise, which is what scopes the
    /// future retirement floor worker-locally. With the watermark in the key a pre-flush fragment
    /// is never found by a post-flush lookup, and the premise holds.
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
    /// build (`Err` or panic) leaves the key absent rather than wedged or cached (I13a).
    pub fn get_or_build(
        &self,
        satisfied: &[TermId],
        auth_data_hash: [u8; 32],
        resolved_at: u64,
        postings: &PostingsReader,
        deltas: &[Arc<DeltaTier>],
        watermark: u64,
    ) -> Result<Arc<FrozenFragment>, FragmentCacheError> {
        let memo_key = (auth_data_hash, resolved_at, watermark);
        let key = {
            let cached = self.key_memo.lock().unwrap().get(&memo_key).copied();
            match cached {
                Some(key) => {
                    debug_assert_eq!(
                        key,
                        canonical_key(
                            &self.bundle_identity,
                            &self.auth_plugin_hash,
                            satisfied,
                            watermark
                        ),
                        "get_or_build: auth_data_hash {auth_data_hash:02x?} at generation stamp \
                         {resolved_at} was previously associated with a different term set than \
                         `satisfied` now hashes to — callers must derive auth_data_hash from the \
                         same auth_data that produced `satisfied`, and resolved_at from the \
                         generation it resolved against (see this method's doc: a violation \
                         silently returns a fragment for the wrong grant set, an I2 disclosure \
                         risk)"
                    );
                    key
                }
                None => {
                    let key = canonical_key(
                        &self.bundle_identity,
                        &self.auth_plugin_hash,
                        satisfied,
                        watermark,
                    );
                    let mut memo = self.key_memo.lock().unwrap();
                    // Bounded by clearing rather than by evicting: this map is pure memoisation, so
                    // discarding it costs a re-derive and never an answer. See
                    // `KEY_MEMO_MAX_ENTRIES` for the unbounded-growth path this closes — it is
                    // driven by distinct *credentials*, which the byte bound below does not see at
                    // all, because a credential granting nothing still produces a `Ready` hit.
                    if memo.len() >= KEY_MEMO_MAX_ENTRIES {
                        memo.clear();
                    }
                    memo.insert(memo_key, key);
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

    /// A corpus for the two cases below: a base entity list per term, and the tiers.
    struct Corpus {
        base: Vec<Vec<u32>>,
        tiers: Vec<Vec<TierEntry>>,
    }

    /// A reproducible random corpus: per-term base entity lists, and two sparse tiers each
    /// carrying a subset of the terms with entities drawn from a higher range, as a flush's tier
    /// does.
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

    /// Write `base` and `tiers` through the real writers and open them through the real readers,
    /// so what the assertions below compare is what a bundle holds.
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

    /// The expected fragment, assembled from the **source lists** rather than from the readers:
    /// the pointwise union, over `terms`, of each term's base entities and its entities in every
    /// tier that carries it.
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

    /// **The union shape the split route's exactness rests on** (handover memo §3.1).
    ///
    /// A fragment must be the pointwise union, over the terms a session satisfies, of each term's
    /// base posting and its posting in every live tier. The split route unions images of base
    /// postings and walks only the residual, which is sound exactly because each satisfied term's
    /// base posting lies wholly inside the fragment. A plugin or a future composition that
    /// combined terms any other way — an intersection, a precedence, a term that removes entities
    /// — would leave the route serving a set the walk does not, and this is the test that would
    /// say so.
    #[test]
    fn a_fragment_is_the_pointwise_union_of_its_terms_base_and_delta_postings() {
        let temp = tempfile::TempDir::new().unwrap();
        for seed in 0..8u64 {
            let dir = temp.path().join(format!("seed-{seed}"));
            std::fs::create_dir_all(&dir).unwrap();
            let Corpus { base, tiers } = random_corpus(seed);
            let (reader, opened) = readers(&dir, &base, &tiers);

            // A grant of roughly half the terms, so the union is over a subset and a term outside
            // it contributing would show.
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

    /// The residual is inside the fragment and covers everything the kept terms' base postings do
    /// not — the two bounds the split route's union needs, over the same random corpora.
    ///
    /// The upper bound is checked against a fragment built from **fewer tiers** than the residual
    /// is given, which is the arrangement that makes the intersection necessary: a live generation
    /// can hold a tier the session's fragment was never unioned from.
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

            // The fragment sees only the first tier; the residual is given both.
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

    /// **The `key_memo` bound, closed against its attacker.** `key_memo` is keyed by
    /// `SHA-256(auth_data)`, so a caller holding the session credential grows it by one entry per
    /// call with random `auth_data` whose descriptors the dictionary does not know: `satisfied` is
    /// empty, the canonical key is identical every time, [`FragmentCache::slots`] takes a `Ready`
    /// hit, **nothing in the byte accounting moves**, and the map grows for ever.
    ///
    /// This is one of the two allocation paths the byte bound does not reach, and deleting the
    /// `memo.clear()` was caught by nothing before this test existed (round-1 review, MX3).
    ///
    /// The assertion is on the bound, not on the clear's exact schedule: what must hold is that the
    /// map never exceeds [`KEY_MEMO_MAX_ENTRIES`] however many distinct credentials are presented.
    /// Asserting "it is exactly 1 after the (n+1)th call" would pin the *policy* (clear-all rather
    /// than evict-one), which this type's doc deliberately leaves free to change.
    #[test]
    fn key_memo_is_bounded_however_many_distinct_credentials_arrive() {
        let temp = tempfile::TempDir::new().unwrap();
        let postings_path = temp.path().join("postings.arrow");
        crate::postings::write_postings(&postings_path, &[vec![1u32, 2, 3]], 32).unwrap();
        let reader = PostingsReader::open(&postings_path, false).unwrap();

        let cache = FragmentCache::new(&temp.path().join("frag"), [7u8; 32], [9u8; 32]);

        // Every call presents a *distinct* credential digest and an empty grant set — the exact
        // shape the doc describes: one canonical key, one build, unbounded distinct hashes.
        let calls = KEY_MEMO_MAX_ENTRIES + KEY_MEMO_MAX_ENTRIES / 2;
        let mut high_water = 0usize;
        for n in 0..calls {
            let mut auth_data_hash = [0u8; 32];
            auth_data_hash[..8].copy_from_slice(&(n as u64).to_le_bytes());
            cache
                .get_or_build(&[], auth_data_hash, 0, &reader, &[], 0)
                .expect("an empty grant set builds once and hits thereafter");
            high_water = high_water.max(cache.key_memo_len());
            assert!(
                cache.key_memo_len() <= KEY_MEMO_MAX_ENTRIES,
                "key_memo exceeded its bound after {} calls: {} > {KEY_MEMO_MAX_ENTRIES}",
                n + 1,
                cache.key_memo_len()
            );
        }

        assert_eq!(
            cache.rebuild_count(),
            1,
            "the attack costs the server no fragment builds at all — which is why the byte bound \
             never sees it"
        );
        assert!(
            high_water > KEY_MEMO_MAX_ENTRIES / 2,
            "the test must actually have driven the map up to its bound, not merely stayed small"
        );
        assert!(
            cache.key_memo_len() < calls,
            "the map must have been cleared at least once: {} entries after {calls} distinct \
             credentials",
            cache.key_memo_len()
        );
    }
}
