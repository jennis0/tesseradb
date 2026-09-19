//! The bundle read protocol (contracts §2.3), the zero-copy `columns.arrow` / `morton.u32`
//! loader, and the tile lookup — `tile_ranges` for one tile, `tile_ranges_all` for the whole
//! tile set of a viewport in one sweep.
//!
//! `tessera-store` never depends on `tessera-authz`, and this module has its own Arrow IPC
//! reader — `columns.arrow`'s schema (fixed-width primitive columns) differs from
//! `tessera-authz::postings`'s single `LargeBinary` column, so the zero-copy technique (mmap →
//! `arrow::buffer::Buffer::from_custom_allocation` → decode without copying) is reused, not the
//! code.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{
    Array, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, Int8Array,
    StringArray, TimestampMicrosecondArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
};
use arrow::buffer::Buffer;
use arrow::datatypes::{DataType, SchemaRef, TimeUnit};
use arrow::ipc::convert::fb_to_schema;
use arrow::ipc::reader::{read_footer_length, FileDecoder};
use arrow::ipc::{root_as_footer, root_as_message, Block, MetadataVersion};
use arrow::record_batch::RecordBatch;
use memmap2::Mmap;
use sha2::{Digest, Sha256};

use tessera_spatial::Tile;
use tessera_types::{IdentityKey, BUNDLE_FORMAT};

use crate::error::{read_to_vec, Result, StoreError};
use crate::manifest::{CurrentPointer, FileDigest, Honourability, Manifest, SegmentsManifest};
use crate::permutation::{Permutation, RowSpace, SegmentExtent};
use crate::render_presence::{render_presence_path, RenderPresence, RENDER_PRESENCE_DIR};

/// One loaded (partition, view) pair: the row space addressing its rows, and every segment
/// in that view — a build writes exactly one (contracts §2.1's "one segment per
/// (partition, view) at build"); the field is a `Vec` because the on-disk shape, and the
/// engine's tile-lookup interface with it, already generalises to streamed segments.
///
/// `row_space` is the built `permutation.bin` plus whatever extents flush has appended — see
/// [`RowSpace`]. A bundle straight out of `tessera build` carries no extents, so it behaves
/// exactly as the bare permutation did.
///
/// **`Clone`, and the segments are behind `Arc`, because a generation is constructed
/// incrementally** (§1.2): a flush publishes a bundle sharing every mapped file with its
/// predecessor plus one new segment. `SegmentData` holds mmaps and an Arrow batch and is not
/// `Clone`; an `Arc` per segment is what makes "share, do not re-open" expressible, and re-opening
/// would re-pay `Permutation::load`'s O(bound) `validate_rows` per flush.
#[derive(Debug, Clone)]
pub struct ViewData {
    pub row_space: RowSpace,
    pub segments: Vec<Arc<SegmentData>>,
    /// **Which incarnation of the key these files belong to** (decision 0115). Read off the
    /// segments this data was built from, or off the manifest for a view that has none yet.
    ///
    /// [`Bundle::with_views`] is what it is for: a dropped key may be created again, and the
    /// recreated view is declared under the same id, so "is this view still declared" no longer
    /// tells the predecessor's row space from the successor's. This does.
    pub incarnation: tessera_types::view::ViewIncarnation,
    /// This view's term images, mapped, or `None` where the side-manifest names none and where
    /// the file it names would not open ([`crate::term_images`]).
    ///
    /// `None` costs time and changes no answer: a session whose terms have no image walks its
    /// permutation instead, and arrives at the same rows.
    pub term_images: Option<Arc<crate::term_images::TermImages>>,
}

/// One loaded segment: its row count, its Morton codes (row order, ascending), and a zero-copy
/// view into its `columns.arrow`.
#[derive(Debug)]
pub struct SegmentData {
    pub seg_id: String,
    pub row_count: u32,
    pub morton: MortonSlice,
    /// Where each occupied leaf Morton cell's rows begin — [`CutIndex`], the run-length index of
    /// `morton`, which selection evaluates per cell instead of per row.
    pub cuts: CutIndex,
    pub columns: ColumnsRef,
}

impl SegmentData {
    /// Opens a segment's three files from its directory. The error names the one that failed.
    pub fn load(
        dir: &Path,
        seg_id: &str,
        row_count: u32,
    ) -> std::result::Result<Self, SegmentLoadError> {
        Ok(SegmentData {
            seg_id: seg_id.to_string(),
            row_count,
            morton: MortonSlice::load(&dir.join("morton.u32")).map_err(|source| {
                SegmentLoadError {
                    file: "morton",
                    source,
                }
            })?,
            cuts: CutIndex::load(&dir.join(CutIndex::FILE), row_count).map_err(|source| {
                SegmentLoadError {
                    file: "cuts",
                    source,
                }
            })?,
            columns: ColumnsRef::load(&dir.join("columns.arrow")).map_err(|source| {
                SegmentLoadError {
                    file: "columns",
                    source,
                }
            })?,
        })
    }
}

/// Which of a segment's files would not open, and why.
#[derive(Debug)]
pub struct SegmentLoadError {
    pub file: &'static str,
    pub source: StoreError,
}

impl std::fmt::Display for SegmentLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.file, self.source)
    }
}

/// One loaded partition: its verified side-manifest, the `n` that manifest was found at, the
/// highest `n` present in the partition directory, and every view it names.
///
/// `Clone` for [`Bundle::with_segment`]'s sake — see [`ViewData`]. Cloning one copies a manifest
/// and two small maps of `Arc`s; it opens no file.
#[derive(Debug, Clone)]
pub struct PartitionData {
    pub manifest: SegmentsManifest,
    /// The `n` of the `SEGMENTS-<n>.json` actually served — taken from the **filename**, which is
    /// the only place it is written. The manifest carried a `segments_version` field defined as
    /// `= n`; it is gone, because the name collided with the *geometry* version, a different
    /// counter that must not move when a manifest is written for deny state alone (see
    /// `SegmentsManifest`'s doc).
    pub segments_n: u64,
    /// The highest `SEGMENTS-<n>.json` present for this partition at open, whether or not it
    /// was the one served.
    ///
    /// **Carried as data because a log line cannot be gated on.** A step-down is a *success*
    /// return: the partition opens and serves, and the only trace of it is the `warn!` in
    /// [`load_verifying_segments_manifest`]. Contracts §2.3 pairs step-down with a `readyz`
    /// freshness gate — "`readyz` fails if the newest verifying `n` is older than the
    /// deployment's configured lag bound; unbounded step-down would let a badly synced replica
    /// serve long-deleted items as live" — and that gate cannot be built from a log. This field
    /// and [`Self::segments_n`] are the data that gate is built from. They are also the
    /// only way a test can assert that a step-down did, or did not, happen.
    ///
    /// **⊘ Specified, not implemented.** The freshness gate itself does not exist: step-down is
    /// built, its time bound is not, so a stepped-down partition serves its older manifest
    /// indefinitely. Contracts §2.3 is marked the same way.
    pub highest_candidate_n: u64,
    pub views: HashMap<String, ViewData>,
}

impl PartitionData {
    /// `true` if the served manifest is not the newest one on disk — i.e. the candidate walk
    /// stepped down past at least one manifest carrying state this reader cannot honour.
    ///
    /// Every stepped-past candidate was examined and *refused*; see
    /// [`load_verifying_segments_manifest`] for why that enumeration is what makes the
    /// step-down safe.
    pub fn stepped_down(&self) -> bool {
        self.highest_candidate_n > self.segments_n
    }
}

/// An open, digest-verified bundle: the top-level manifest plus every partition's loaded data.
#[derive(Debug)]
pub struct Bundle {
    pub manifest: Manifest,
    pub partitions: HashMap<String, PartitionData>,
}

/// A side-manifest together with the `n` it is published at.
///
/// **One value because they must never be supplied separately.** `n` lives in the filename and
/// nothing inside the manifest carries it (see [`SegmentsManifest`]'s doc), so a caller holding
/// the two apart can install a manifest under a number that does not describe it — and the number
/// is what every later reader uses to decide which candidate it walked to.
pub struct PublishedManifest {
    pub manifest: SegmentsManifest,
    pub n: u64,
}

impl Bundle {
    /// This bundle plus one segment in `(partition, view)`: a **new** `Bundle` sharing every
    /// mapped file with this one.
    ///
    /// **Required, not an optimisation.** `open_bundle` maps every file afresh and
    /// `Permutation::load` re-pays an O(bound) `validate_rows`, so re-opening the bundle per flush
    /// would cost more than the flush it followed — and §1.4's claim that the marginal cost of a
    /// pin drain entry is roughly one flush segment rests on consecutive generations sharing their
    /// base geometry rather than being whole distinct bundles.
    ///
    /// Only the new segment's files were opened, by the caller; nothing here re-reads anything.
    /// `manifest` is the `SEGMENTS-<n+1>.json` that named them, and it is carried whole because
    /// contracts §2.3 makes a side-manifest complete current state for its partition rather than a
    /// diff.
    ///
    /// **No verification happens here, and that is the commit point's doing rather than an
    /// omission.** The files were made durable and the side-manifest written before this is ever
    /// called; digest verification is what `open_bundle` does for files it did not write. A
    /// generation constructed here is verified in the ordinary way at the next restart.
    pub fn with_segment(
        &self,
        partition: &str,
        view: &str,
        segment: SegmentData,
        extent: SegmentExtent,
        published: PublishedManifest,
    ) -> Result<Arc<Bundle>> {
        self.substituting(partition, view, published, |view_data| {
            let row_space = view_data.row_space.with_extent(extent).ok_or_else(|| {
                StoreError::MalformedBundle {
                    detail: format!(
                        "flush segment '{}' does not continue view '{view}'s row space",
                        segment.seg_id
                    ),
                }
            })?;
            let mut segments = view_data.segments.clone();
            segments.push(Arc::new(segment));
            Ok(ViewData {
                row_space,
                segments,
                // **The view's own, unchanged**: a flush and a merge write into the incarnation
                // that is live, and neither crosses a drop — a dropped view has no row space to
                // extend and no segments to collapse.
                incarnation: view_data.incarnation,
                // **The base's own, unchanged.** A flush and a merge write no images: an image is
                // the base permutation applied to a term's base posting, and neither publication
                // touches either. The rows they add are walked (ruling B, the term-images memo).
                term_images: view_data.term_images.clone(),
            })
        })
    }

    /// This bundle with the adjacent run `consumed` replaced by one merged segment.
    ///
    /// `Err` if any consumed `seg_id` is absent, which is how a merge planned against a generation
    /// that has since been superseded is **discarded rather than published**. ABA-safe because
    /// `seg_id`s are never reused, across merges or prefixes (contracts §2.1), so an absent one is
    /// proof the inputs are gone — never a pointer comparison. An `Err`, not a panic: a discarded
    /// merge is an expected outcome of the rebase, not a bug.
    pub fn with_merged(
        &self,
        partition: &str,
        view: &str,
        consumed: &[String],
        segment: SegmentData,
        extent: SegmentExtent,
        published: PublishedManifest,
    ) -> Result<Arc<Bundle>> {
        self.substituting(partition, view, published, |view_data| {
            let row_space = view_data
                .row_space
                .collapsing(consumed, extent)
                .ok_or_else(|| StoreError::MalformedBundle {
                    detail: format!(
                        "merge inputs {consumed:?} are not a present, adjacent run of view \
                         '{view}', or the merged segment does not preserve their rows"
                    ),
                })?;
            let mut segments: Vec<Arc<SegmentData>> = view_data
                .segments
                .iter()
                .filter(|s| !consumed.contains(&s.seg_id))
                .cloned()
                .collect();
            segments.push(Arc::new(segment));
            Ok(ViewData {
                row_space,
                segments,
                // **The view's own, unchanged**: a flush and a merge write into the incarnation
                // that is live, and neither crosses a drop — a dropped view has no row space to
                // extend and no segments to collapse.
                incarnation: view_data.incarnation,
                // **The base's own, unchanged.** A flush and a merge write no images: an image is
                // the base permutation applied to a term's base posting, and neither publication
                // touches either. The rows they add are walked (ruling B, the term-images memo).
                term_images: view_data.term_images.clone(),
            })
        })
    }

    /// This bundle with one partition's side-manifest replaced and **nothing else touched** — no
    /// segment added, no extent collapsed, no row space rebuilt.
    ///
    /// The entity-space coalesce publication's whole bundle edit (`tessera_engine::coalesce`): it
    /// rewrites `deltas`, `external_id_runs`, `locator_extents`, `dict_extents` and `files`, every
    /// one of which addresses entity space. A caller that needed row space to move would be using
    /// one of the two above, and the type is what keeps the two apart.
    pub fn with_manifest(
        &self,
        partition: &str,
        published: PublishedManifest,
    ) -> Result<Arc<Bundle>> {
        let view = self
            .partitions
            .get(partition)
            .and_then(|p| p.views.keys().next().cloned())
            .ok_or_else(|| StoreError::MalformedBundle {
                detail: format!("no partition '{partition}' with a view in this bundle"),
            })?;
        self.substituting(partition, &view, published, |view_data| {
            Ok(ViewData {
                row_space: view_data.row_space.clone(),
                segments: view_data.segments.clone(),
                incarnation: view_data.incarnation,
                term_images: view_data.term_images.clone(),
            })
        })
    }

    /// This bundle carrying a different **top-level manifest**, with the per-view map brought
    /// into step with the views it declares (`views.md` §3.2).
    ///
    /// The one edit a view create and a view drop make to a live bundle, and the only site that
    /// touches `Bundle::manifest` at all: a created view gains an **empty** row space, because it
    /// owns nothing on disc until its first flush and a view absent from this map is read as
    /// *unknown view* by the viewport and as *the mask and the bundle disagree* by the deny mask;
    /// a dropped view leaves it, which is what makes a request naming it the same 404 as one that
    /// never existed. **No file is read, written or removed**: the dropped view's files are
    /// garbage the fold reclaims, and until then they are simply unreachable.
    ///
    /// Side-manifests are untouched — the roster's durable half is published by the ordinary
    /// deny-state path at the next tick, on the mechanism `layer_tombstones` already uses.
    pub fn with_views(&self, manifest: Manifest) -> Arc<Bundle> {
        let mut partitions = self.partitions.clone();
        for partition in partitions.values_mut() {
            // **Declared *and* at the live incarnation** (decision 0115). A dropped key may be
            // created again, and the recreated view carries the same id — so declaration alone
            // stopped being the question the moment the burn was withdrawn. A view whose data was
            // built over a dead incarnation is dropped here exactly as an undeclared one is, and
            // the seeding below puts an **empty** view back in its place: the predecessor's files
            // stay on disc, reachable by nothing, until the fold reclaims them.
            partition
                .views
                .retain(|id, data| manifest.is_live_incarnation(id, data.incarnation));
            for view in &manifest.views {
                partition
                    .views
                    .entry(view.id.clone())
                    .or_insert_with(|| ViewData {
                        row_space: RowSpace::new(
                            Arc::new(
                                crate::permutation::Permutation::empty()
                                    .expect("an anonymous mapping of one page"),
                            ),
                            0,
                        ),
                        segments: Vec::new(),
                        incarnation: view.incarnation,
                        // A view created while the service runs owns no row space until its first
                        // flush, so there is nothing to have projected.
                        term_images: None,
                    });
            }
        }
        Arc::new(Bundle {
            manifest,
            partitions,
        })
    }

    /// The shared half of [`Self::with_segment`], [`Self::with_merged`] and
    /// [`Self::with_manifest`]: clone the partition and view maps — `Arc`s and a manifest, no
    /// file IO — and replace the one view.
    fn substituting(
        &self,
        partition: &str,
        view: &str,
        published: PublishedManifest,
        replace: impl FnOnce(&ViewData) -> Result<ViewData>,
    ) -> Result<Arc<Bundle>> {
        let existing =
            self.partitions
                .get(partition)
                .ok_or_else(|| StoreError::MalformedBundle {
                    detail: format!("no partition '{partition}' in this bundle"),
                })?;
        let view_data = existing
            .views
            .get(view)
            .ok_or_else(|| StoreError::MalformedBundle {
                detail: format!("no view '{view}' in partition '{partition}'"),
            })?;
        let next_view = replace(view_data)?;

        let mut partitions = self.partitions.clone();
        let entry = partitions
            .get_mut(partition)
            .expect("looked up immediately above");
        entry.views.insert(view.to_string(), next_view);
        // The served `n` and the highest candidate move together: this generation *is* the newest
        // manifest, so it is not stepped down, whatever the one it was built from was.
        //
        // **Passed in, not read off the manifest.** `n` lives in the filename (see
        // `SegmentsManifest`'s doc for why the field that duplicated it is gone), so the only
        // honest source for it here is the writer that allocated it.
        entry.segments_n = published.n;
        entry.highest_candidate_n = published.n;
        entry.manifest = published.manifest;

        Ok(Arc::new(Bundle {
            manifest: self.manifest.clone(),
            partitions,
        }))
    }
}

/// Open `root` (a bundle directory containing `CURRENT`) following the read protocol
/// (contracts §2.3): `CURRENT` → digest-checked `MANIFEST.json` (refusing a `bundle_format` other
/// than this reader's own) → per partition, the highest `SEGMENTS-<n>.json` whose listed files (and
/// `MANIFEST.json`'s) all verify by size and SHA-256, stepping down on failure. Any failure at
/// any stage is a typed error — fail-closed, per the invariant this method exists to uphold:
/// serving must never treat a partially-verified bundle as ready.
pub fn open_bundle(root: &Path) -> Result<Bundle> {
    let current_path = root.join("CURRENT");
    let current_bytes = read_to_vec(&current_path)?;
    let current: CurrentPointer =
        serde_json::from_slice(&current_bytes).map_err(|source| StoreError::Json {
            path: current_path.clone(),
            source,
        })?;

    let manifest_path = root.join(&current.prefix).join("MANIFEST.json");
    let manifest_bytes = read_to_vec(&manifest_path)?;

    let actual_digest = hex_sha256(&manifest_bytes);
    if actual_digest != current.manifest_digest {
        return Err(StoreError::ManifestDigestMismatch {
            expected: current.manifest_digest,
            actual: actual_digest,
        });
    }

    // The **bytes just digested**, not a second read: `MANIFEST.json` is immutable by contract
    // (§2.1 — `CURRENT` is the only mutable file), but building the bundle from a re-read would
    // make the digest a claim about one read and the bundle a product of another, on the one file
    // whose digest *is* the bundle identity.
    open_prefix(root, &current.prefix, manifest_bytes, Verification::Digests)
}

/// How much of a prefix an open re-checks before mapping it — see [`open_written_prefix`] for the
/// one caller that may answer anything but [`Verification::Digests`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verification {
    /// The read protocol in full: every file named by either `files` map is read and hashed, and
    /// `permutation.bin`'s row bound is re-validated. What every bundle arriving from storage
    /// gets, because a bundle whose bytes were not checked is a bundle whose authorisation data
    /// was not checked.
    Digests,
    /// The bytes were produced and digested by *this process*, moments ago — see
    /// [`open_written_prefix`].
    JustWritten,
}

/// Open a prefix **this process just wrote**, skipping the digest sweep and the permutation's row
/// validation. The fourth bundle constructor, and the one a compaction's publication needs.
///
/// # Why this exists rather than a second [`open_bundle`] call
///
/// A fold writes a whole new prefix and must then serve from it *in this process* — decision D1
/// (compaction §13): the alternative was publish-then-restart, whose price is the measured 40–53 s
/// dictionary lookup rebuild and every session re-established. [`Bundle`]'s three incremental
/// constructors ([`Bundle::with_segment`], [`Bundle::with_merged`], [`Bundle::with_manifest`]) all
/// work *within* one prefix, so none of them can express a prefix change; [`open_bundle`] can, and
/// re-reads and re-hashes every byte both `files` maps name — tens of gigabytes the fold has just
/// finished writing and hashing — and re-pays `Permutation::validate_rows` over the whole entity
/// space on top.
///
/// # What is skipped, and what is emphatically not
///
/// Skipped: the two `verify_files` sweeps, and `Permutation::validate_rows`. **Both are checks on
/// bytes that arrived from storage**, and the premise here is that they did not: the fold hashed
/// each file as it wrote it (compaction §3, pass 5), and the digests in the manifest this call
/// parses are the ones it computed from the bytes it had in hand. Re-reading them proves nothing
/// that the write did not already prove, and costs the whole bundle in IO.
///
/// **What `validate_rows` also produces is taken from the writer instead of dropped.** Besides
/// checking the file, that scan records that the mapping is onto `[0, row_count)`, which is what
/// lets a whole-domain grant's projection be the row range rather than a walk of every page
/// (`Permutation::project`). Skipping the scan and recording nothing would leave every session on
/// this prefix walking until the next restart — a cost a compaction imposes on the deployment for
/// as long as the process lives. So this open calls `Permutation::declare_dense_rows` with the
/// segment's own `row_count`. That is the writer stating what it wrote, which rests on the same
/// premise as everything else in this section rather than on a new one, and the `row_count` it
/// states is cross-checked against `morton.u32` and `columns.arrow` by the check the next paragraph
/// lists among what is kept.
///
/// Kept, all of it: the `bundle_format` check, `identity.validate()`, every path-component
/// sanitisation, the `ensure_verified` membership check (a file the loader reads must appear in a
/// `files` map — cheap, and it catches a manifest that names a file it does not digest), the
/// `row_count` agreement between the manifest and `morton.u32`/`columns.arrow`, and every extent's
/// `rebuild`/`with_extent` contiguity check. These are checks on the manifest's *self-consistency*
/// and on the writer's own correctness, not on the medium, so the premise above does not cover them
/// and they stay unconditional.
///
/// # The caller's obligation, stated because nothing here can check it
///
/// **The files under `prefix` must have been written by this process since it started, and must
/// not have been read back from anywhere else.** A caller that pointed this at a prefix it did not
/// write would map unverified bytes as authorisation data. There is exactly one such caller — the
/// fold's publication — and `open_bundle` is what everything else uses, including every restart.
pub fn open_written_prefix(root: &Path, prefix: &str) -> Result<Bundle> {
    let manifest_path = root.join(prefix).join("MANIFEST.json");
    let manifest_bytes = read_to_vec(&manifest_path)?;
    open_prefix(root, prefix, manifest_bytes, Verification::JustWritten)
}

fn open_prefix(
    root: &Path,
    prefix: &str,
    manifest_bytes: Vec<u8>,
    verification: Verification,
) -> Result<Bundle> {
    let prefix_dir = root.join(prefix);
    let manifest_path = prefix_dir.join("MANIFEST.json");

    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|source| StoreError::Json {
            path: manifest_path.clone(),
            source,
        })?;

    // **Exactly the current format, not at most.** A ceiling alone would open a bundle written at
    // an earlier number, and the numbers exist because an earlier bundle's bytes decode as
    // something else under this reader — format 4's artifact record reads its one-byte parent tag
    // as the low byte of a parent count. No bundle predates the current format (decision 0048),
    // so the refusal costs a rebuild and nothing more.
    if manifest.bundle_format != BUNDLE_FORMAT {
        return Err(StoreError::UnsupportedBundleFormat {
            found: manifest.bundle_format,
            supported: BUNDLE_FORMAT,
        });
    }

    // A bundle written by a different `tessera_id` construction (or round count) must not be
    // silently read by this one (contracts §2.6 r6) — fail closed before any segment is opened.
    manifest.identity.validate()?;

    // The roster against the views it names, and the views against the roster (`views.md` §3.2).
    // Before any segment is opened, for the reason above it: a view a client can see and cannot
    // address is a bundle to refuse, not one to serve part of.
    manifest
        .validate_groups()
        .map_err(|detail| StoreError::MalformedBundle { detail })?;

    // The MANIFEST-level `files` set (dictionary extents and anything else it names) is
    // verified once, up front — it isn't partition-specific, and the reader protocol requires
    // every one of these entries to verify regardless of which SEGMENTS-<n>.json a partition
    // settles on.
    if verification == Verification::Digests {
        verify_files(&prefix_dir, &manifest.files)?;
    }

    // Row space above the build bound is rebuilt from each flush segment's own `tessera_id`
    // column — see [`SegmentExtent::rebuild`] for why nothing is stored for it and what that
    // costs. Parsed once here rather than per segment: `validate()` above has already refused a
    // manifest whose identity configuration this reader cannot honour.
    let identity_key = IdentityKey::from_hex(&manifest.identity.key).map_err(|source| {
        StoreError::InvalidIdentity {
            detail: format!("MANIFEST.json's identity key is unusable: {source}"),
        }
    })?;

    let mut partitions = HashMap::with_capacity(manifest.partitions.len());
    for partition_desc in &manifest.partitions {
        sanitize_component("partition phash", &partition_desc.phash)?;
        let partition_dir = prefix_dir.join("partitions").join(&partition_desc.phash);
        let selected = load_verifying_segments_manifest(&prefix_dir, &partition_dir, verification)?;
        let segments_manifest = selected.manifest;

        // **One incarnation per view, and it is the newest** (decision 0115). A drop leaves its
        // segments in the live side-manifest until a fold reclaims them, so after a key has been
        // created again this list can name two incarnations of one view id. Their row spaces are
        // unrelated — each is dense from its own base — so composing them would be nonsense
        // before it was a disclosure. Incarnations are minted monotonically, so the newest is the
        // live one; the rest are the fold's to reclaim and are not opened. `with_views` then
        // checks even that one against the roster and blanks the view if it disagrees, which is
        // where the *fail-closed* half lives: this pass has the manifest and not yet the log.
        let mut newest: HashMap<&str, tessera_types::view::ViewIncarnation> = HashMap::new();
        for seg_desc in &segments_manifest.segments {
            let seen = newest.entry(seg_desc.view.as_str()).or_default();
            *seen = (*seen).max(seg_desc.incarnation);
        }
        let mut views: HashMap<String, ViewData> = HashMap::new();
        for seg_desc in &segments_manifest.segments {
            if newest.get(seg_desc.view.as_str()) != Some(&seg_desc.incarnation) {
                continue;
            }
            // **Per component the path derivation will lay down**, not on the joined id: a
            // group's view is `group:key` and `:` is exactly the character the two-component path
            // exists for (`views.md` §3.2).
            for component in crate::view_path_components(&seg_desc.view) {
                sanitize_component("view id", component)?;
            }
            sanitize_component("segment id", &seg_desc.seg_id)?;

            let view_dir = crate::view_path(&partition_dir, &seg_desc.view);
            let is_new_view = !views.contains_key(&seg_desc.view);
            let perm_path = view_dir.join("permutation.bin");
            let perm_rel = format!(
                "partitions/{}/{}/permutation.bin",
                partition_desc.phash,
                crate::view_rel(&seg_desc.view)
            );
            // **A view need not have a base at all** (`views.md` §3.2). Every view a build or a
            // fold wrote has one, and its first segment is the build segment `permutation.bin`
            // addresses; a view *created while the service runs* owns no row space until its
            // first flush, and every segment it ever takes is an extent over an empty base. The
            // manifest is what says which — a view whose permutation no manifest names has none,
            // and reading that as a missing file would refuse the bundle for a view that is
            // simply new.
            let has_base = segments_manifest.files.contains_key(&perm_rel)
                || manifest.files.contains_key(&perm_rel);
            let is_base_segment = is_new_view && has_base;
            let view_entry = match views.get_mut(&seg_desc.view) {
                Some(entry) => entry,
                None => {
                    let permutation = if has_base {
                        ensure_verified(
                            &perm_rel,
                            &segments_manifest,
                            &manifest.files,
                            &perm_path,
                        )?;
                        Permutation::load(&perm_path)?
                    } else {
                        Permutation::empty()?
                    };

                    // `row-entity.u32` beside it, the other direction
                    // (`crate::row_entity`). **Optional, and its absence is not a refusal**: it is
                    // an optimisation for the filtered viewport's per-tile route, and a view
                    // without one still answers every query through the projecting route. Where it
                    // *is* named, it is verified like any other file — an unverifiable table is a
                    // refusal, because a wrong row→entity mapping would put another entity's
                    // filter verdict on a row.
                    let row_entity_rel = format!(
                        "partitions/{}/{}/{}",
                        partition_desc.phash,
                        crate::view_rel(&seg_desc.view),
                        crate::row_entity::ROW_ENTITY_FILE
                    );
                    let row_entity = if segments_manifest.files.contains_key(&row_entity_rel)
                        || manifest.files.contains_key(&row_entity_rel)
                    {
                        let path = view_dir.join(crate::row_entity::ROW_ENTITY_FILE);
                        ensure_verified(
                            &row_entity_rel,
                            &segments_manifest,
                            &manifest.files,
                            &path,
                        )?;
                        Some(std::sync::Arc::new(crate::row_entity::RowToEntity::load(
                            &path,
                        )?))
                    } else {
                        None
                    };

                    // The first segment named for a view is its build segment: `permutation.bin`
                    // addresses that one's row space, and every later segment arrives as an
                    // extent above it.
                    // A base-less view's rows all belong to extents, so its base owns none:
                    // `base_rows` is this segment's count only where the permutation is what
                    // addresses it.
                    let mut row_space = RowSpace::new(
                        std::sync::Arc::new(permutation),
                        if has_base { seg_desc.row_count } else { 0 },
                    );
                    if let Some(table) = row_entity {
                        row_space = row_space.with_row_entity(table);
                    }
                    views.insert(
                        seg_desc.view.clone(),
                        ViewData {
                            row_space,
                            segments: Vec::new(),
                            incarnation: seg_desc.incarnation,
                            // Attached below, once the base segment's row count and the
                            // permutation's bound have been checked: both are stamped into the
                            // file and the open compares them.
                            term_images: None,
                        },
                    );
                    views.get_mut(&seg_desc.view).expect("just inserted")
                }
            };

            let seg_dir = view_dir.join("segments").join(&seg_desc.seg_id);
            let morton_path = seg_dir.join("morton.u32");
            let cuts_path = seg_dir.join(CutIndex::FILE);
            let columns_path = seg_dir.join("columns.arrow");
            let morton_rel = format!(
                "partitions/{}/{}/segments/{}/morton.u32",
                partition_desc.phash,
                crate::view_rel(&seg_desc.view),
                seg_desc.seg_id
            );
            let cuts_rel = format!(
                "partitions/{}/{}/segments/{}/{}",
                partition_desc.phash,
                crate::view_rel(&seg_desc.view),
                seg_desc.seg_id,
                CutIndex::FILE
            );
            let columns_rel = format!(
                "partitions/{}/{}/segments/{}/columns.arrow",
                partition_desc.phash,
                crate::view_rel(&seg_desc.view),
                seg_desc.seg_id
            );
            ensure_verified(
                &morton_rel,
                &segments_manifest,
                &manifest.files,
                &morton_path,
            )?;
            ensure_verified(&cuts_rel, &segments_manifest, &manifest.files, &cuts_path)?;
            ensure_verified(
                &columns_rel,
                &segments_manifest,
                &manifest.files,
                &columns_path,
            )?;
            // `ColumnsRef::load` reads every presence bitmap beside the column, so each one has to
            // pass the same membership rule the two files above do. Driven off the directory rather
            // than off the manifest because it is what the loader will *read* that must have been
            // verified — a bitmap dropped in beside a segment, naming rows no digest covers, is
            // exactly what this half of the check exists to refuse.
            let presence_dir = seg_dir.join(RENDER_PRESENCE_DIR);
            if presence_dir.is_dir() {
                let entries =
                    std::fs::read_dir(&presence_dir).map_err(|source| StoreError::Io {
                        path: presence_dir.clone(),
                        source,
                    })?;
                for entry in entries {
                    let entry = entry.map_err(|source| StoreError::Io {
                        path: presence_dir.clone(),
                        source,
                    })?;
                    let rel = format!(
                        "partitions/{}/{}/segments/{}/{RENDER_PRESENCE_DIR}/{}",
                        partition_desc.phash,
                        crate::view_rel(&seg_desc.view),
                        seg_desc.seg_id,
                        entry.file_name().to_string_lossy()
                    );
                    ensure_verified(&rel, &segments_manifest, &manifest.files, &entry.path())?;
                }
            }

            let segment = SegmentData::load(&seg_dir, &seg_desc.seg_id, seg_desc.row_count)
                .map_err(|e| e.source)?;
            let (morton, columns) = (&segment.morton, &segment.columns);

            if morton.len() as u32 != seg_desc.row_count
                || columns.row_count() != seg_desc.row_count
            {
                return Err(StoreError::MalformedBundle {
                    detail: format!(
                        "segment '{}' (view '{}'): manifest row_count {} doesn't match \
                         morton.u32 ({} codes) or columns.arrow ({} rows)",
                        seg_desc.seg_id,
                        seg_desc.view,
                        seg_desc.row_count,
                        morton.len(),
                        columns.row_count()
                    ),
                });
            }

            // `permutation.bin` addresses this view's single build segment (R4); validate its
            // row bound against that segment's `row_count` the first time we see it (I11/I4 —
            // a corrupt permutation must never hand out a `RowId` that indexes `columns.arrow`
            // out of range). Only meaningful once, against the one segment a Phase-1 view has.
            if is_base_segment {
                match verification {
                    Verification::Digests => view_entry
                        .row_space
                        .base()
                        .validate_rows(seg_desc.row_count)?,
                    // The scan is skipped here and its *result* is taken from the writer — see
                    // [`open_written_prefix`]'s "what is skipped" section for why that rests on
                    // the same premise rather than a weaker one. Without it a compaction's
                    // in-process open would leave every session on the new prefix walking
                    // `permutation.bin` for a whole-domain grant until the process restarted.
                    //
                    // The complement route rests on this same count: it subtracts the rows of the
                    // entities outside a grant from `[0, row_count)`, which is the whole row space
                    // only if the count is right. A wrong declaration would make both routes wrong
                    // together, which is why the premise is that these are the bytes this process
                    // wrote and nothing weaker.
                    Verification::JustWritten => view_entry
                        .row_space
                        .base()
                        .declare_dense_rows(seg_desc.row_count),
                }

                // **The view's term images, here and not earlier**: the file's stamp names the
                // base segment's row count and the permutation's bound, and the bound is only
                // established once the permutation above has been validated or declared.
                //
                // A digest failure is not this path. The sweep covers this file as it covers
                // every other file the manifest lists and refuses the whole bundle before the
                // first view is opened; under [`Verification::JustWritten`] the bytes are the
                // writer's own.
                view_entry.term_images = open_term_images(
                    &prefix_dir,
                    prefix,
                    &segments_manifest,
                    &manifest.files,
                    seg_desc,
                    view_entry.row_space.base().bound(),
                )?;
            }

            // Every segment after the first is one a flush appended or a merge collapsed, and it
            // owns row space above the base. Its entity→row mapping is rebuilt here from its own
            // `tessera_id` column — nothing on disk carries it, deliberately; see
            // [`SegmentExtent::rebuild`]. `with_extent` then re-checks contiguity and
            // well-formedness, so a manifest listing segments out of entity order, or one whose
            // `row_count` disagrees with what the extent actually owns, fails closed here rather
            // than serving rows under the wrong entity.
            if !is_base_segment {
                let row_base = u32::try_from(view_entry.row_space.total_rows()).map_err(|_| {
                    StoreError::MalformedBundle {
                        detail: format!(
                            "view '{}' already holds {} rows, so segment '{}' cannot begin \
                             inside a u32 row space",
                            seg_desc.view,
                            view_entry.row_space.total_rows(),
                            seg_desc.seg_id
                        ),
                    }
                })?;
                let extent = SegmentExtent::rebuild(
                    &seg_desc.seg_id,
                    seg_desc.entity_lo,
                    seg_desc.entity_hi,
                    row_base,
                    columns.tessera_id(),
                    &identity_key,
                    manifest.identity.shard_id,
                )?;
                view_entry.row_space =
                    view_entry.row_space.with_extent(extent).ok_or_else(|| {
                        StoreError::MalformedBundle {
                            detail: format!(
                                "segment '{}' does not continue view '{}'s row space",
                                seg_desc.seg_id, seg_desc.view
                            ),
                        }
                    })?;
            }

            view_entry.segments.push(Arc::new(segment));
        }

        // **Every declared view is a view, with or without rows** (`views.md` §3.2). The map
        // above is built from the segments, because that is where a row space comes from; a view
        // that has taken no flush yet has no segment and would otherwise be absent from it — and
        // absent is read as *unknown view* by the viewport (404) and as *the deny mask and the
        // bundle disagree* by the mask derivation (500). A view created while the service runs is
        // in exactly that state between its create and its first flush, and it must answer
        // **empty** in both places, so it is seeded here.
        for view in &manifest.views {
            views.entry(view.id.clone()).or_insert_with(|| ViewData {
                row_space: RowSpace::new(
                    std::sync::Arc::new(
                        Permutation::empty().expect("an anonymous mapping of one page"),
                    ),
                    0,
                ),
                segments: Vec::new(),
                incarnation: view.incarnation,
                term_images: None,
            });
        }

        partitions.insert(
            partition_desc.phash.clone(),
            PartitionData {
                manifest: segments_manifest,
                segments_n: selected.n,
                highest_candidate_n: selected.highest_candidate_n,
                views,
            },
        );
    }

    Ok(Bundle {
        manifest,
        partitions,
    })
}

/// Reject a single opaque path component (`phash`, view id, seg id) that could otherwise
/// escape the bundle root once joined: empty, `.`, `..`, containing a path separator, or
/// absolute. Manifest JSON is trusted for shape (it was digest-verified before we get here)
/// but never for path safety — a digest only proves the bytes weren't tampered with, not that
/// the *values inside* are safe to join onto a filesystem path.
fn sanitize_component(what: &str, value: &str) -> Result<()> {
    let is_safe = !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !Path::new(value).is_absolute();
    if is_safe {
        Ok(())
    } else {
        Err(StoreError::UnsafePath {
            what: what.to_string(),
            value: value.to_string(),
        })
    }
}

/// Before opening `full_path`, confirm its prefix-relative form `rel` is a verified entry in
/// either the chosen `SEGMENTS-<n>.json`'s `files` map or `MANIFEST.json`'s. `verify_files` only
/// checked the entries a manifest *does* list — a manifest with an empty or partial `files` map
/// verifies vacuously, and without this check the loader would go on to mmap files no digest
/// ever covered. This is the second half of that check: every file the loader is about to
/// *read* must have appeared in the set that was actually verified.
fn ensure_verified(
    rel: &str,
    segments_manifest: &SegmentsManifest,
    manifest_files: &BTreeMap<String, FileDigest>,
    full_path: &Path,
) -> Result<()> {
    if segments_manifest.files.contains_key(rel) || manifest_files.contains_key(rel) {
        Ok(())
    } else {
        Err(StoreError::UnverifiedFile {
            path: full_path.to_path_buf(),
        })
    }
}

/// Map one view's term images, or answer `None` where it has none to map.
///
/// `None` is the answer for a manifest that names no file for this view and for a file that fails
/// any of [`crate::term_images::TermImages::open`]'s checks. A refused file costs the time the
/// images would have saved and changes no answer: the session walks its permutation instead, and
/// the rows it arrives at are the same ones.
///
/// Two refusals are the bundle's rather than the file's, and both refuse the bundle. A side-manifest
/// naming two files for one incarnation of one view describes a state no publication produces, and
/// there is no rule for choosing between them. A file no `files` map digests is one the loader
/// would map without its bytes having been covered, which is the check every derived file passes.
fn open_term_images(
    prefix_dir: &Path,
    prefix: &str,
    segments_manifest: &SegmentsManifest,
    manifest_files: &BTreeMap<String, FileDigest>,
    seg_desc: &crate::manifest::SegmentDescriptor,
    bound: u64,
) -> Result<Option<Arc<crate::term_images::TermImages>>> {
    let mut named = segments_manifest
        .term_image_extents
        .iter()
        .filter(|entry| entry.view == seg_desc.view && entry.incarnation == seg_desc.incarnation);
    let Some(entry) = named.next() else {
        return Ok(None);
    };
    if named.next().is_some() {
        return Err(StoreError::MalformedBundle {
            detail: format!(
                "view '{}' is named by two term-image extents at incarnation {}",
                seg_desc.view, seg_desc.incarnation
            ),
        });
    }

    let path = safe_join(prefix_dir, &entry.path)?;
    ensure_verified(&entry.path, segments_manifest, manifest_files, &path)?;

    if u64::from(entry.keep_rows_per_container) != crate::term_images::KEEP_ROWS_PER_CONTAINER {
        tracing::warn!(
            view = %seg_desc.view,
            found = entry.keep_rows_per_container,
            expected = crate::term_images::KEEP_ROWS_PER_CONTAINER,
            "term images were derived under another keep rule and are dropped; the view's \
             sessions walk their permutation"
        );
        return Ok(None);
    }

    let expected = crate::term_images::TermImageStamp {
        prefix: prefix.to_string(),
        view: seg_desc.view.clone(),
        base_seg_id: seg_desc.seg_id.clone(),
        incarnation: seg_desc.incarnation,
        base_rows: seg_desc.row_count,
        bound,
    };
    match crate::term_images::TermImages::open(&path, &expected, entry.dict_len) {
        Ok(images) => Ok(Some(Arc::new(images))),
        Err(refusal) => {
            tracing::warn!(
                view = %seg_desc.view,
                %refusal,
                "term images would not open and are dropped; the view's sessions walk their \
                 permutation"
            );
            Ok(None)
        }
    }
}

/// Find the highest-numbered `SEGMENTS-<n>.json` under `partition_dir` whose own `files` all
/// verify by size and SHA-256 **and** whose state this reader can honour, stepping down through
/// lower `n` on failure. Errors (fail-closed) if none qualifies.
///
/// # The honourable-state check, and why it does not step down uniformly
///
/// `SegmentsManifest` parses `deltas`, `tombstones` and `deny`, and nothing in this crate or
/// above it acts on any of them ([`SegmentsManifest::unhonourable_state`]). That is harmless
/// only while nothing *writes* them. The moment something does, opening such a manifest serves
/// rows a tombstone deletes and rows a `deny` suppresses — an accepted deny silently undone,
/// which is the whole of what contracts §2.3's publication rule exists to prevent.
///
/// Refusing every unhonourable manifest by stepping down to an older one is the obvious
/// implementation and it is the same fail-open wearing a fallback's clothes: the older manifest
/// predates the deny, so serving it re-exposes the suppressed item indefinitely, with no
/// operator signal, because the freshness gate §2.3 pairs with step-down does not exist yet.
/// So the two dispositions separate ([`Honourability`],
/// [`crate::manifest::DENY_DISPOSITION_STATE`]):
///
/// - **`deltas` alone** ([`Honourability::Steppable`]) → step down. Items are *missing*, never
///   re-exposed; the availability argument for a mid-sync replica holds here and only here.
/// - **`tombstones` or `deny`** ([`Honourability::Unready`]) → the partition is unready
///   ([`StoreError::UnhonourableManifest`]). SA §9: "a worker that cannot verify its partition
///   marks itself unready rather than serving partial data."
///
/// The classification is [`SegmentsManifest::honourability`]'s, not this loop's, and
/// deliberately so: the shape §2.3 makes ordinary is a manifest carrying `deny` **and**
/// `deltas` (a side-manifest is "full current state, not a diff", so every publication while a
/// suppression is live carries both), and an `any`/`all` slip over that pair silently converts
/// the refusal below into a step-down. Nothing here re-derives the posture from a field list.
///
/// **The check runs before `verify_files`, deliberately.** A `SEGMENTS-<n>.json` carries no
/// digest of its own — only the files it names are verified — so its `deny` list is exactly as
/// trustworthy whether or not those files check out, and the mid-sync replica that has the new
/// manifest but not yet its data files is the *most* likely way to meet a deny-carrying manifest
/// whose files fail. Verifying first would step that case down and re-expose the item. It is
/// also the cheaper order: the refusal costs no I/O where `verify_files` hashes every named
/// file. "Verify the bytes before interpreting them" is the right instinct and is what every
/// other arm of this loop does — it is wrong *here*, for that reason, and
/// `a_deny_carrying_manifest_whose_files_are_missing_is_still_refused` is the test that says so.
///
/// # Why stepping down is safe, and the one thing that would make it unsafe
///
/// **The step-down's safety is a property of this loop, not of `deltas`.** It rests on every
/// intervening candidate being *examined and refused*: between the manifest served and the
/// highest one present, each `n` is read, classified, and — if it carries a deny disposition —
/// turned into a hard error before any lower candidate is considered. "Items go missing, never
/// re-exposed" follows from that enumeration, not from anything intrinsic to a delta tier.
/// **Do not narrow the candidate list.** Trying only the top few candidates (an obvious-looking
/// optimisation on [`list_segments_manifests`], which stats a whole directory) would let a
/// deny-carrying manifest go unexamined and be stepped past unseen, which is this guard's
/// fail-open reintroduced from the other end. If that list ever needs bounding, it must be
/// bounded by *refusing* what it could not examine, never by ignoring it.
///
/// **Known residuals, out of scope here.** A deny-carrying manifest is still stepped past when
/// this loop cannot tell that it carries one — two ways, both of them the same shape:
///
/// - it does not **parse** (`serde_json` error below),
/// - it cannot be **read** (I/O error below — a permission or media fault).
///
/// Neither is closable here, and neither should be closed by guessing: an ordinary torn write
/// must not become a hard partition failure, and a manifest whose bytes are unavailable tells
/// the reader nothing about what it carried. **The bound on both is time, and that bound does
/// not exist yet** — the `readyz` freshness gate (contracts §2.3) is unbuilt, so a replica in this
/// state serves the older manifest indefinitely.
///
/// A third residual — a manifest present under a **non-canonical name** — is closed:
/// [`list_segments_manifests`] refuses such a name rather than parsing it (contracts §2.1), so a
/// padded `SEGMENTS-01.json` is a typed error and never a step-past. It differed from the other
/// two in being decidable without reading anything.
///
/// **The deny writer has shipped and the gate has not**, which an earlier note here said must
/// never happen. The rule was stated wider than the condition it protected: both residuals
/// require a *replica* — a reader seeded from a manifest it did not write — and this deployment
/// has one node, which replays its own WAL over the seed. The gate bounds how stale a synced
/// replica's view may be, and there is nothing to sync. It ships with replication (owner ruling,
/// 2026-08-03).
// `SEGMENTS-<n>.json`'s own `files` map, like `MANIFEST.json`'s, is keyed by paths relative to
// the bundle *prefix* directory (R1: "manifest paths prefix-relative"), not to the partition
// directory the side-manifest itself lives in — so verification is against `prefix_dir`, even
// though the side-manifest file is found by walking `partition_dir`.
fn load_verifying_segments_manifest(
    prefix_dir: &Path,
    partition_dir: &Path,
    verification: Verification,
) -> Result<SelectedManifest> {
    // Bundle-root-relative rather than the bare phash: this process swaps bundles at runtime, so
    // a log line or an error naming only `default` cannot say *which* bundle's `default` it
    // means. Short enough to stay a decent structured field, and free of the absolute prefix.
    // Built before the listing because the listing can itself refuse (a non-canonical name).
    let partition_label = match (prefix_dir.file_name(), partition_dir.file_name()) {
        (Some(prefix), Some(phash)) => format!(
            "{}/partitions/{}",
            prefix.to_string_lossy(),
            phash.to_string_lossy()
        ),
        _ => partition_dir.display().to_string(),
    };

    let mut candidates = list_segments_manifests(partition_dir, &partition_label)?;
    // Highest n first.
    candidates.sort_unstable_by(|a, b| b.cmp(a));
    let highest_candidate_n = candidates.first().copied();

    // The **first** failure recorded, not the last: the loop walks highest-first, so the first
    // is the newest manifest's — the one an operator must fix. Overwriting per iteration leaves
    // the oldest candidate's reason instead, which is the least actionable one on offer.
    let mut highest_candidate_error: Option<String> = None;

    for n in candidates {
        let path = partition_dir.join(format!("SEGMENTS-{n}.json"));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                highest_candidate_error.get_or_insert_with(|| format!("{}: {e}", path.display()));
                continue;
            }
        };
        let segments_manifest = match serde_json::from_slice::<SegmentsManifest>(&bytes) {
            Ok(m) => m,
            Err(e) => {
                highest_candidate_error
                    .get_or_insert_with(|| format!("{}: invalid JSON: {e}", path.display()));
                continue;
            }
        };
        // One classification, made where the fields are defined (`SegmentsManifest`), never
        // re-derived here — see this function's doc and [`Honourability`].
        //
        // `fields` is field *names* and `n` is a counter: no entity ID reaches either arm's
        // error or the `warn!` (SA §9), which is why `Honourability` carries `&'static str`.
        match segments_manifest.honourability() {
            Honourability::Honourable => {}
            Honourability::Unready { fields } => {
                return Err(StoreError::UnhonourableManifest {
                    partition: partition_label,
                    n,
                    fields,
                });
            }
            Honourability::Steppable { fields } => {
                tracing::warn!(
                    partition = %partition_label,
                    n,
                    fields = ?fields,
                    "stepping down past a SEGMENTS manifest carrying state this reader cannot \
                     honour; the items it adds stay missing until a build that honours it runs"
                );
                let reason = StoreError::UnhonourableManifest {
                    partition: partition_label.clone(),
                    n,
                    fields,
                };
                highest_candidate_error.get_or_insert_with(|| reason.to_string());
                continue;
            }
        }

        let verified = match verification {
            Verification::Digests => verify_files(prefix_dir, &segments_manifest.files),
            // The step-down walk still runs: a just-written prefix carries exactly the one
            // manifest its publication wrote, so there is nothing to step past, and leaving the
            // walk in place keeps one code path rather than two.
            Verification::JustWritten => Ok(()),
        };
        match verified {
            Ok(()) => {
                return Ok(SelectedManifest {
                    manifest: segments_manifest,
                    n,
                    highest_candidate_n: highest_candidate_n
                        .expect("a candidate was selected, so the candidate list is non-empty"),
                })
            }
            Err(e) => {
                // **A candidate carrying deny-disposition state is never stepped past, whether
                // the objection came from classification or from verification.** The
                // classification arm above no longer catches this one: `deny` and `tombstones`
                // are honoured, so such a manifest is `Honourable` and arrives here like any
                // other. A `continue` would then serve an older manifest that re-exposes every
                // entity denied since it was written — the fail-open decision 0018 promoted into
                // contract, reached in exactly the damaged-newest case this reader calls most
                // likely.
                let fields = segments_manifest.deny_disposition_state();
                if !fields.is_empty() {
                    return Err(StoreError::UnverifiedDenyManifest {
                        partition: partition_label,
                        n,
                        fields,
                        detail: e.to_string(),
                    });
                }
                highest_candidate_error.get_or_insert_with(|| e.to_string());
                continue;
            }
        }
    }

    Err(StoreError::NoVerifyingSegmentsManifest {
        partition: partition_label,
        highest_candidate_error,
    })
}

/// What the candidate walk settled on for one partition: the manifest served, the `n` it was
/// found at, and the highest `n` present — see [`PartitionData::highest_candidate_n`] for why
/// the last of these is returned rather than left in the `warn!`.
struct SelectedManifest {
    manifest: SegmentsManifest,
    n: u64,
    highest_candidate_n: u64,
}

/// List the `n` values of every `SEGMENTS-<n>.json` present in `partition_dir` (unordered,
/// unverified — candidates only).
///
/// **A candidate whose name is not the canonical spelling of its `n` is refused, not parsed**
/// (contracts §2.1, [`StoreError::NonCanonicalManifestName`]). `n` is unpadded decimal, and the
/// caller reconstructs `SEGMENTS-{n}.json` to read from — so parsing `SEGMENTS-01.json` to
/// `n = 1` discovers a manifest and then reads a different or absent file, which the candidate
/// walk records as an I/O failure and steps past. That step-past carries the reader past a
/// manifest that may hold a `deny`, and is indistinguishable from the file not being there at
/// all. §2.1: "Parsing leniently and reconstructing canonically is the combination that hides
/// it." The refusal is what makes the two distinguishable.
///
/// The test is canonical-spelling equality — the parsed number, re-rendered, must equal what was
/// read — which admits `0` and `11` and refuses a leading zero, an empty or non-numeric part, a
/// sign, whitespace, and anything past `u64`. It is deliberately confined to the
/// `SEGMENTS-<…>.json` family: the `SEGMENTS-<n>.json.tmp` orphan a crashed manifest write
/// leaves behind ([`crate::manifest_write`]) does not end in `.json`, is not a candidate, and
/// must not become a partition failure.
fn list_segments_manifests(partition_dir: &Path, partition_label: &str) -> Result<Vec<u64>> {
    let entries = match std::fs::read_dir(partition_dir) {
        Ok(entries) => entries,
        // No such directory at all is not itself a hard read error here: the caller reports a
        // typed "no verifying manifest" error either way, with a clearer message than a raw
        // ENOENT would give.
        Err(_) => return Ok(Vec::new()),
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| StoreError::Io {
            path: partition_dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name
            .strip_prefix("SEGMENTS-")
            .and_then(|r| r.strip_suffix(".json"))
        {
            // Canonical-spelling equality, before anything reads the file: `rest.parse()`
            // alone accepts `007` and yields `7`, and the caller then opens `SEGMENTS-7.json`.
            match rest.parse::<u64>() {
                Ok(n) if n.to_string() == rest => found.push(n),
                _ => {
                    return Err(StoreError::NonCanonicalManifestName {
                        partition: partition_label.to_string(),
                        name: name.into_owned(),
                    })
                }
            }
        }
    }
    Ok(found)
}

/// The highest `n` any `SEGMENTS-<n>.json` under `bundle_root` is named with — over every prefix
/// directory and every partition directory beneath them — or `None` where the tree holds none.
///
/// **The floor a writer's next `n` must clear** (write-path §1.2). `n` is allocated once and never
/// reused, and the only complete record of which numbers are taken is the set of filenames present:
/// a manifest names the files of its own publication, not the side-manifests of any other, and a
/// prefix an in-flight compaction is building is named by nothing at all until it flips `CURRENT`.
/// An allocator seeded from what a manifest names is therefore seeded below files that exist, and
/// the first publication at such an `n` is refused by
/// [`StoreError::SideManifestExists`](crate::StoreError::SideManifestExists).
///
/// Symlinked prefix and partition directories are followed. A link whose target is absent is
/// skipped, a target that cannot be stat'd for any other reason is an error: see
/// [`sub_directories`].
///
/// **A non-canonical name raises the floor rather than refusing here.** The reader refuses one
/// ([`list_segments_manifests`], contracts §2.1) because it must not read a manifest under a name
/// it cannot reconstruct; this asks only which numbers may be taken, and a padded `SEGMENTS-01.json`
/// says `1` may be. Refusing would stop a node writing over a file it can already read past.
///
/// A directory that cannot be listed is an error rather than an omission: an allocator that cannot
/// see the files present cannot say a number is free.
pub fn highest_side_manifest_n(bundle_root: &Path) -> Result<Option<u64>> {
    let mut highest: Option<u64> = None;
    for prefix in sub_directories(bundle_root)? {
        for partition in sub_directories(&prefix.join("partitions"))? {
            for entry in read_dir_if_present(&partition)? {
                let entry = entry.map_err(|source| StoreError::Io {
                    path: partition.clone(),
                    source,
                })?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(rest) = name
                    .strip_prefix("SEGMENTS-")
                    .and_then(|r| r.strip_suffix(".json"))
                else {
                    continue;
                };
                if let Ok(n) = rest.parse::<u64>() {
                    highest = Some(highest.map_or(n, |h: u64| h.max(n)));
                }
            }
        }
    }
    Ok(highest)
}

/// The directories directly under `dir`, empty where `dir` does not exist.
///
/// `metadata` rather than the entry's own `file_type`, so a symlinked prefix or partition directory
/// is walked: the entry's type says "symlink" where the target is the directory the numbers live
/// in, and an allocator that skipped it would allocate over files that are there.
///
/// A target that is not there — a broken link, or an entry removed between the listing and the
/// stat — is skipped, because neither holds a number. Any other stat failure is an error, on the
/// rule this whole scan follows: an allocator that cannot see what is present cannot say a number
/// is free.
fn sub_directories(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in read_dir_if_present(dir)? {
        let entry = entry.map_err(|source| StoreError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        // `NotFound` is the broken link and the entry removed between the listing and the stat,
        // neither of which holds numbers. Every other failure is reported: an entry that may be a
        // directory full of side-manifests, unread, is the same fail-open as a directory that
        // could not be listed.
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => found.push(path),
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(StoreError::Io { path, source }),
        }
    }
    Ok(found)
}

/// `read_dir`, with an absent directory reading as empty and every other failure an error.
fn read_dir_if_present(dir: &Path) -> Result<Vec<std::io::Result<std::fs::DirEntry>>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries.collect()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(StoreError::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// **The one path-escape rule in this crate**, shared with [`crate::reclaim`] rather than copied:
/// a second implementation of what counts as a safe manifest path is a second thing to get right,
/// on the boundary where getting it wrong walks outside the bundle root.
///
/// Join a manifest-supplied, forward-slash `files`-map key onto `base` one component at a
/// time, rejecting anything that could escape `base`: a leading `/` (absolute), a backslash
/// (not R1's convention and a Windows path-separator ambiguity), or any `.`/`..`/empty
/// component. `Path::join` on an absolute-looking argument silently *replaces* the base
/// instead of erroring, and a naive `.replace('/', separator)` would happily turn
/// `"../../etc/passwd"` into a working traversal — this walks the split path so no single
/// string ever reaches `PathBuf::join` unchecked.
pub(crate) fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
        return Err(StoreError::UnsafePath {
            what: "files map path".to_string(),
            value: rel.to_string(),
        });
    }
    let mut path = base.to_path_buf();
    for component in rel.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(StoreError::UnsafePath {
                what: "files map path component".to_string(),
                value: rel.to_string(),
            });
        }
        path.push(component);
    }
    Ok(path)
}

/// Verify every entry of `files` (path relative to `base`, forward slashes per R1) by exact
/// size and SHA-256 hex digest. Any missing, mis-sized or mismatched file is a hard error.
///
/// **One exemption, and only one:** the external-ID sidecar's extents and locator
/// ([`is_sidecar_deferred`]) are skipped here, per contracts §0.3 deviation 9 — they are still
/// named, still digested in the manifest, and still fully verified by `crate::sidecar` at first
/// touch. See that predicate's doc for why open-time verification would defeat the deviation.
///
/// **TOCTOU note:** this reads each file's bytes once, here, to check size+digest; the loader
/// (`Permutation::load`, `MortonSlice::load`, `ColumnsRef::load`) then separately mmaps the
/// same path. These two accesses are not atomic. That gap is accepted, not overlooked: every
/// file a manifest names is contractually immutable once published (contracts §2.1 — "every
/// other file is immutable; the prefix grows only by whole new files named in a newer
/// side-manifest"), so a well-behaved bundle publisher never mutates a file after naming it in
/// a digest-verified manifest. A concurrent adversarial rewrite between these two reads is the
/// same class of hazard as any other mmap-of-a-file-another-process-can-touch situation in this
/// codebase (see `tessera-authz`'s postings reader) — it is an operational/deployment concern
/// (read-only bundle storage, no writer with access to a serving replica's files), not one this
/// module's checks can close from inside a single process.
/// How much of a file is held in memory at once while hashing it (see `verify_files`). Matches
/// `tessera-build`'s constant of the same name; the two crates share no dependency to share it
/// through.
const DIGEST_CHUNK_BYTES: usize = 1 << 20;

/// `true` if `rel` names a file belonging to the external-ID sidecar — an
/// `external-ids-<k>.arrow` extent or the `ext-locator.u32` locator, both under an `entities/`
/// directory (contracts §2.4 r6 fixes both names).
///
/// **Contracts §0.3 deviation 9**: these paths are *"exempt from the §2.3 reader protocol's
/// readiness gate… Nothing is mapped, scanned or verified at open"*. [`verify_files`] therefore
/// skips them — and that exemption is the whole point of the sidecar's per-extent laziness: at
/// 10⁹ items the family runs to ~18.9 GB, and digesting it at open would reimpose exactly the
/// sequential read (and page-cache churn) the deviation exists to remove, for a structure no
/// viewport request ever touches.
///
/// **Their digests stay in the manifest, and verification is deferred, not dropped.**
/// `ExternalIdSidecar` verifies the extent's (or the locator's) SHA-256 against the manifest
/// entry at first touch, plus sortedness and declared length, before any answer comes out of it
/// — see `crate::sidecar`. A file skipped here is a file no request path has read yet; the first
/// read of it is fully checked.
fn is_sidecar_deferred(rel: &str) -> bool {
    let Some((dir, file)) = rel.rsplit_once('/') else {
        return false;
    };
    if dir != "entities" && !dir.ends_with("/entities") {
        return false;
    }
    file == "ext-locator.u32" || (file.starts_with("external-ids-") && file.ends_with(".arrow"))
}

/// Verify one named file's size and digest. The per-file half of [`verify_files`], split out so
/// the sweep can run them concurrently.
fn verify_one(base: &Path, rel_path: &str, digest: &FileDigest) -> Result<()> {
    let path = safe_join(base, rel_path)?;
    // The path is still validated (above) even when its bytes are not read here, so an
    // unsafe `files`-map key cannot hide behind the sidecar's deferral.
    if is_sidecar_deferred(rel_path) {
        return Ok(());
    }
    // Read in fixed-size chunks, never whole: at 10^9 items `columns.arrow` alone is over 20 GB,
    // and slurping every file to hash it would make opening a bundle cost more memory than
    // serving it. One buffer per file rather than one per sweep, which is what lets the files run
    // concurrently; at `DIGEST_CHUNK_BYTES` the transient is the buffer times the pool's width.
    let mut buffer = vec![0u8; DIGEST_CHUNK_BYTES];
    let mut file = File::open(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    if size != digest.size {
        return Err(StoreError::FileVerificationFailed {
            path,
            reason: format!(
                "size mismatch: manifest says {}, file is {size} bytes",
                digest.size
            ),
        });
    }
    let actual = hex_digest(hasher.finalize().as_slice());
    if actual != digest.sha256 {
        return Err(StoreError::FileVerificationFailed {
            path,
            reason: format!(
                "SHA-256 mismatch: manifest says {}, computed {actual}",
                digest.sha256
            ),
        });
    }
    Ok(())
}

/// Hash every file the manifest names, in full, before the bundle is served.
///
/// The verification is unconditional — a bundle whose bytes were not checked is a bundle whose
/// authorisation data was not checked (fail closed) — with one exemption, the external-ID sidecar
/// ([`is_sidecar_deferred`]).
///
/// **The sweep is parallel because it is I/O-bound, not hash-bound, and the two have different
/// remedies.** SHA-256 runs at ~2.3 GB/s on one core with the hardware extensions this CPU has, but
/// a *serial* read-and-hash delivered only ~310–390 MB/s — so the serial sweep was leaving the
/// device's queue depth idle, not the CPU. Measured over 4.46 GB in eight files: **11.4–15.0 s
/// serial against 1.74–1.89 s across eight workers, 6.5–8×**, with the parallel rate landing at
/// ~2.4–2.6 GB/s, which is the device rather than the hash. (Measured with `sha256sum` and
/// `xargs -P`, so it sizes the effect rather than this code path exactly.) At 10⁹ with sixteen
/// declared filter columns `attrs/` alone is ~64 GB, where that ratio is minutes against seconds.
///
/// **Deferring the value columns to first touch was considered and declined** (owner ruling,
/// 2026-08-10). It would have paid off only for columns nobody filters on — every declared column
/// is opened at generation build, so "first touch" has to mean *first scan* to buy anything, and
/// that puts a multi-second hash of a 4 GB column on a request path budgeted at 0.5–1 s. The
/// sidecar's deferral works because its extents are small; a value column is the largest artefact
/// in the bundle. Parallelism takes the wall clock without touching the fail-closed rule, which is
/// why `attrs/` is **not** in `is_sidecar_deferred` and contracts §2.4 owes no amendment for it.
///
/// **The error is deterministic and does not depend on which worker lost.** Results are collected
/// and the failure reported is the first in the manifest's own (sorted) order, so a bundle with two
/// corrupt files refuses with the same message on every run.
fn verify_files(base: &Path, files: &BTreeMap<String, FileDigest>) -> Result<()> {
    use rayon::prelude::*;

    files
        .par_iter()
        .map(|(rel_path, digest)| verify_one(base, rel_path, digest))
        .collect::<Vec<_>>()
        .into_iter()
        .find(|outcome| outcome.is_err())
        .unwrap_or(Ok(()))
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes).as_slice())
}

fn hex_digest(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A memory-mapped, zero-copy view of `morton.u32`: raw sorted little-endian `u32` codes, no
/// header (R4).
#[derive(Debug)]
pub struct MortonSlice {
    mmap: Mmap,
}

impl MortonSlice {
    /// The mapped file's size in bytes — the operand of `tessera-server`'s merge-size relation
    /// (§4's relation 2), which needs a segment's on-disk size and has no other way to ask for it.
    pub fn byte_len(&self) -> u64 {
        self.mmap.len() as u64
    }

    /// `madvise(MADV_SEQUENTIAL)` on this mapping — compaction §6.1's mitigation, decision 0052.
    ///
    /// **Called by the streaming passes and never by the request path**, which is the whole of what
    /// makes it safe: `madvise` applies to the *mapping*, so advising one a viewport also holds
    /// would disable its random-access read-ahead for the life of that mapping. A fold and a merge
    /// each `load` their own, through [`crate::segment_cursor::SegmentCursor`]; a viewport's is a
    /// different mapping of the same file and is untouched.
    ///
    /// What it buys is reclaim order. Without it the fold's pages are the most recently touched in
    /// the whole machine and therefore look hottest, so the kernel evicts a viewport's genuinely hot
    /// tiles to make room for bytes nothing will read again — a measured 2.03× on a concurrent
    /// viewport (P3). This states what is true: streamed once, freeable after.
    ///
    /// **A hint, so a refusal is not a failure.** `madvise` failing leaves a correct mapping that
    /// is merely no gentler than before, and turning that into a failed segment open would trade
    /// the whole operation for an optimisation.
    ///
    /// ⊘ **Unmeasured.** P3 must be re-run with this applied, over a sweep long enough to displace
    /// a real fraction of the bundle; `MADV_COLD` behind the cursor is the escalation if it proves
    /// insufficient (compaction §6.1).
    pub fn advise_sequential(&self) {
        if streaming_advice_enabled() {
            let _ = self.mmap.advise(memmap2::Advice::Sequential);
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: read-only for this struct's lifetime; see `Permutation::load`'s note on the
        // shared operational hazard of a concurrently-truncated backing file.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if mmap.len() % 4 != 0 {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "{}: length {} is not a multiple of 4",
                    path.display(),
                    mmap.len()
                ),
            });
        }
        let view = MortonSlice { mmap };
        // `tile_ranges`'s binary search is only sound over an ascending array (contracts
        // §2.5/§2.6: "Morton order"); a hand-corrupted or wrongly-built `morton.u32` that isn't
        // sorted would make `partition_point` silently return a wrong (not merely imprecise)
        // range instead of erroring — checked once here, fail-closed, rather than trusted.
        if !view.u32().windows(2).all(|w| w[0] <= w[1]) {
            return Err(StoreError::MalformedBundle {
                detail: format!("{}: codes are not sorted ascending", path.display()),
            });
        }
        Ok(view)
    }

    /// The number of codes (rows) in this segment.
    pub fn len(&self) -> usize {
        self.mmap.len() / 4
    }

    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// The codes, in row order (ascending; no further tiebreak beyond `tessera_id` at write
    /// time — contracts §2.6 r6).
    pub fn u32(&self) -> &[u32] {
        // SAFETY: length is a checked multiple of 4 (validated at `load`); the mmap base is
        // page-aligned (>= 4-byte aligned) by construction, so this cast is always valid — no
        // per-open re-check needed the way `permutation.bin`'s offset-16 slice needed one,
        // since here the slice starts at offset 0.
        unsafe { std::slice::from_raw_parts(self.mmap.as_ptr() as *const u32, self.len()) }
    }
}

/// A memory-mapped, zero-copy view of `cuts.u32`: where each occupied leaf Morton cell's rows
/// begin, ascending, raw little-endian `u32`, no header (contracts §2.6).
///
/// # What it is for
///
/// Row order is `(morton, tessera_id)`, so the rows of one leaf cell are contiguous **and their
/// identities ascend within it**. That second half is what selection needs and what nothing on
/// disk previously said: given a cell's row range, the identities below a threshold are a prefix
/// of it, and the smallest identities of a tile are a merge of its cells' prefixes. Selection
/// reads a bounded number of rows per cell instead of every visible row of the tile
/// ([`crate::read`] has no opinion on that; see `tessera_engine::select`).
///
/// Cell *i* covers rows `starts[i] .. starts[i + 1]`, the last ending at the segment's
/// `row_count`. `starts[0]` is 0 in a segment with rows. The array is therefore the run-length
/// index of `morton.u32` and holds no code: the code is `morton[starts[i]]`, and storing it again
/// would be a second copy that could disagree with the column it describes.
///
/// # What holds the premise, and what a broken one would cost
///
/// **Identities ascending within a cell is by construction, and it is not checked at open.** It is
/// the row order itself, so every producer gets it from the sort it already does:
/// [`crate::write::SegmentWriter::append`] debug-asserts the arriving key against the last, and
/// the build's bounded assembly sorts each bucket by the same comparator. `tessera verify --deep`
/// checks it in full, over both columns, along with the boundaries falling where the code changes.
/// What this type checks at `load` is only what one column can answer: strictly ascending, opening
/// at row 0, ending inside the segment. A whole-column identity pass at every open would cost the
/// identity column on the startup path, which is the thing the route exists to stop reading.
///
/// **A violated premise gives a wrong subset of the visible rows, never a row outside the mask.**
/// Every row selection returns is one the composed mask admitted, because membership is only ever
/// asked of the mask; what the ordering decides is where the walk stops. So the failure is an
/// under-counted `C_θ` and a served set that may not be the *m* smallest — items missing from a
/// map, of the principal's own items. It is not a disclosure. That is why the check is a verifier's
/// and not an open's: the cost of being wrong is a wrong map, which an operator can be told about,
/// rather than a leak, which they could not be.
///
/// # Size
///
/// One `u32` per **occupied cell**, not per row. Both GBIF corpora, counted from their Morton
/// columns (measured, 2026-09-14): 3,508,005 cells over 25,846,007 rows is 14.0 MB against 103 MB
/// of `morton.u32` and 207 MB of identity column, and 41,899,178 cells over 3,495,729,729 rows is
/// 167.6 MB against 14.0 GB and 28.0 GB. A corpus whose cells are large pays less per row, not
/// more; the ceiling is 4 B/row, reached only where every row has a cell to itself.
#[derive(Debug)]
pub struct CutIndex {
    mmap: Mmap,
}

impl CutIndex {
    /// The file a segment's cut index lives in, beside `morton.u32`.
    pub const FILE: &'static str = "cuts.u32";

    /// Map and validate `cuts.u32` against the segment's row count.
    ///
    /// **Validated here rather than trusted**, for the reason [`MortonSlice::load`] gives: the
    /// selection binary-searches this array, so a non-ascending or out-of-range entry produces a
    /// wrong row range rather than an error. What the check cannot see from here is whether the
    /// boundaries fall where the Morton code actually changes — that needs both columns, and
    /// `tessera verify --deep` is where it is made.
    pub fn load(path: &Path, row_count: u32) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: read-only for this struct's lifetime; see `Permutation::load`'s note on the
        // shared operational hazard of a concurrently-truncated backing file.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if mmap.len() % 4 != 0 {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "{}: length {} is not a multiple of 4",
                    path.display(),
                    mmap.len()
                ),
            });
        }
        let view = CutIndex { mmap };
        let starts = view.starts();
        let malformed = |detail: String| StoreError::MalformedBundle {
            detail: format!("{}: {detail}", path.display()),
        };
        match starts.first() {
            None => {
                if row_count != 0 {
                    return Err(malformed(format!(
                        "no cells for a segment of {row_count} rows"
                    )));
                }
            }
            Some(&first) => {
                if first != 0 {
                    return Err(malformed(format!("the first cell begins at row {first}")));
                }
                if row_count == 0 {
                    return Err(malformed("cells in a segment with no rows".to_string()));
                }
            }
        }
        if !starts.windows(2).all(|w| w[0] < w[1]) {
            return Err(malformed(
                "cell starts are not strictly ascending".to_string(),
            ));
        }
        if starts.last().is_some_and(|&last| last >= row_count) {
            return Err(malformed(format!(
                "the last cell begins at row {} in a segment of {row_count} rows",
                starts.last().copied().unwrap_or_default()
            )));
        }
        Ok(view)
    }

    /// The number of occupied cells.
    pub fn len(&self) -> usize {
        self.mmap.len() / 4
    }

    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// This mapping's size in bytes — see [`MortonSlice::byte_len`], which this joins in the
    /// merge-size relation's operand: the three files are mapped together and a segment's size is
    /// all of them.
    pub fn byte_len(&self) -> u64 {
        self.mmap.len() as u64
    }

    /// The row at which each occupied cell begins, ascending.
    pub fn starts(&self) -> &[u32] {
        // SAFETY: as [`MortonSlice::u32`] — a checked multiple of 4 from a page-aligned base.
        unsafe { std::slice::from_raw_parts(self.mmap.as_ptr() as *const u32, self.len()) }
    }

    /// The cell starts at or after `from` and before `until`.
    ///
    /// A tile's row range is cell-aligned — a tile is a code prefix, so it holds whole leaf
    /// cells — which makes the first returned start equal to `from` for any range this crate
    /// hands out. Callers clamp anyway rather than rely on it, so that a sub-range of a tile is
    /// still answered correctly.
    pub fn starts_within(&self, from: u32, until: u32) -> &[u32] {
        let starts = self.starts();
        let lo = starts.partition_point(|&s| s < from);
        let hi = starts.partition_point(|&s| s < until);
        &starts[lo..hi.max(lo)]
    }
}

/// One declared-scalar column's typed, zero-copy value slice.
#[derive(Debug)]
pub enum ScalarSlice<'a> {
    /// Bit-packed by Arrow, so this is the array rather than a `&[bool]` — the one fixed-width
    /// member that is not a flat slice of itself, for the same reason `Utf8` is not.
    Bool(&'a BooleanArray),
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
    U64(&'a [u64]),
    I8(&'a [i8]),
    I16(&'a [i16]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    F64(&'a [f64]),
    /// Microseconds since the Unix epoch — an `i64` slice whose *unit* the declaration fixes.
    TimestampUs(&'a [i64]),
    /// Variable-length; `StringArray` itself is a zero-copy view over the mapped buffers, so
    /// this is still zero-copy even though it isn't a flat `&[&str]`.
    Utf8(&'a StringArray),
}

impl ScalarSlice<'_> {
    /// The stored type's name, for a diagnostic that has to say what it found. Deliberately the
    /// same spelling `ScalarType::arrow_type_name` uses, so a mismatch message names the two sides
    /// in one vocabulary rather than making a reader translate between them.
    pub fn type_name(&self) -> &'static str {
        match self {
            ScalarSlice::Bool(_) => "bool",
            ScalarSlice::U8(_) => "u8",
            ScalarSlice::U16(_) => "u16",
            ScalarSlice::U32(_) => "u32",
            ScalarSlice::U64(_) => "u64",
            ScalarSlice::I8(_) => "i8",
            ScalarSlice::I16(_) => "i16",
            ScalarSlice::I32(_) => "i32",
            ScalarSlice::I64(_) => "i64",
            ScalarSlice::F32(_) => "f32",
            ScalarSlice::F64(_) => "f64",
            ScalarSlice::TimestampUs(_) => "timestamp_us",
            ScalarSlice::Utf8(_) => "utf8",
        }
    }
}

/// A zero-copy, mmap-backed view of `columns.arrow`. Validated once at [`ColumnsRef::load`]:
/// exactly one record batch, uncompressed, 8-byte-aligned buffers (via
/// [`FileDecoder::with_require_alignment`]), and the two fixed columns present with the
/// expected names and types (contracts §2.6; the `priority` column is cut — decision 0046 —
/// and a pre-cut bundle carrying it is a typed error here, never a silently ignored column). Every accessor below borrows directly from the
/// underlying `RecordBatch`'s buffers — no per-call copy.
#[derive(Debug)]
pub struct ColumnsRef {
    batch: RecordBatch,
    scalar_index: HashMap<String, usize>,
    /// One entry per scalar column whose segment holds a presence bitmap (decision 0064). A column
    /// with no entry is every-row-present, and [`ColumnsRef::presence`] hands back
    /// `all_present` for it rather than an `Option`, so no caller branches on which it got.
    presence: HashMap<String, RenderPresence>,
    /// The value every column without a file resolves to. Held rather than constructed per call
    /// because the accessor lends a reference.
    all_present: RenderPresence,
    /// The mapping `batch`'s buffers point into, kept only so [`ColumnsRef::advise_sequential`] has
    /// something to advise: the `Arc` is already captured as each `Buffer`'s allocation, and there
    /// is no way back to it from a `RecordBatch`. A second reference count, no second mapping.
    mapping: Arc<Mmap>,
}

const FIXED_COLUMNS: [(&str, DataType); 2] = [
    ("tessera_id", DataType::UInt64),
    ("residual", DataType::UInt32),
];

impl ColumnsRef {
    /// This segment's `columns.arrow` size in bytes — see [`MortonSlice::byte_len`].
    pub fn byte_len(&self) -> u64 {
        self.batch.get_array_memory_size() as u64
    }

    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: identical justification to `tessera_authz::postings::PostingsReader::open`'s
        // mmap branch — `arc` outlives every `Buffer` built from it (captured as the buffer's
        // `Allocation`), the mapping is valid for `len` bytes for its whole lifetime, and
        // `memmap2::Mmap` never returns a null base pointer.
        let mapping = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let len = mapping.len();
        let arc: Arc<Mmap> = Arc::new(mapping);
        let ptr = NonNull::new(arc.as_ptr() as *mut u8)
            .expect("memmap2::Mmap never returns a null base pointer");
        // Cloned rather than moved: the buffer's allocation and `ColumnsRef::mapping` are two
        // references to one mapping, which is what lets the latter exist at all (a `RecordBatch`
        // offers no way back to the allocation its buffers hold).
        let allocation: Arc<Mmap> = Arc::clone(&arc);
        let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, allocation) };

        let batch = decode_single_batch(&buffer, path)?;
        validate_schema(&batch, path)?;

        let scalar_index: HashMap<String, usize> = batch
            .schema_ref()
            .fields()
            .iter()
            .enumerate()
            .skip(FIXED_COLUMNS.len())
            .map(|(idx, field)| (field.name().clone(), idx))
            .collect();

        // The bitmaps beside the column, one per scalar column that has one. Read here rather than
        // on demand so that a damaged one refuses at open, with every other malformation of this
        // segment, instead of at the first request that filters on it.
        let segment_dir = path.parent().unwrap_or(Path::new("."));
        let mut presence = HashMap::new();
        for name in scalar_index.keys() {
            let bitmap_path = render_presence_path(segment_dir, name);
            if bitmap_path.exists() {
                let loaded = RenderPresence::load(&bitmap_path)?;
                // A bit past this segment's last row is a bitmap belonging to some other segment —
                // the shape a merge or a carry-forward produces when it moves a file without
                // permuting it — and it fails open: rows this segment does hold read as present
                // because the bits describing them landed elsewhere. Refused here, where the row
                // count is known, rather than at the scan, which sees only a `contains`.
                let rows = batch.num_rows() as u32;
                if loaded
                    .bitmap()
                    .and_then(|b| b.maximum())
                    .is_some_and(|max| max >= rows)
                {
                    return Err(StoreError::MalformedBundle {
                        detail: format!(
                            "{}: presence bitmap names a row at or past this segment's {rows}",
                            bitmap_path.display()
                        ),
                    });
                }
                presence.insert(name.clone(), loaded);
            }
        }

        Ok(ColumnsRef {
            batch,
            scalar_index,
            presence,
            all_present: RenderPresence::all_present(),
            mapping: arc,
        })
    }

    /// `madvise(MADV_SEQUENTIAL)` on this mapping — see [`MortonSlice::advise_sequential`], which
    /// states why this is the streaming passes' call and not the request path's.
    pub fn advise_sequential(&self) {
        if streaming_advice_enabled() {
            let _ = self.mapping.advise(memmap2::Advice::Sequential);
        }
    }

    pub fn row_count(&self) -> u32 {
        self.batch.num_rows() as u32
    }

    /// The row→wire-identity direction (contracts §2.6, §0.3 deviations 2 and 6): the
    /// `tessera_id` shown to viewers, stored at the row it is shown from. No entity ID is
    /// stored here — after contracts r6 the gather cannot produce one, which is what makes
    /// I10 structural rather than a discipline at the serialisation chokepoint.
    pub fn tessera_id(&self) -> &[u64] {
        downcast::<UInt64Array>(&self.batch, 0).values()
    }

    /// The low half of each row's 64-bit interleaved position. The high half is the row's cell
    /// code in `morton.u32`, so a whole position is `(morton[i] as u64) << 32 | residual[i]` —
    /// see `tessera_spatial::split32`, which is what wrote it. No coordinate is stored: this is
    /// the position in the grid's own units, and turning it back into coordinates needs the
    /// extent `MANIFEST.json` declares.
    pub fn residual(&self) -> &[u32] {
        downcast::<UInt32Array>(&self.batch, 1).values()
    }

    /// A declared-scalar column by name, or `None` if `columns.arrow` has no such column.
    pub fn scalar(&self, name: &str) -> Option<ScalarSlice<'_>> {
        let idx = *self.scalar_index.get(name)?;
        let column = self.batch.column(idx);
        // One arm per stored type. Generated for the flat ones (`downcast().values()` differs only
        // in the array type), hand-written for the two that are not flat.
        macro_rules! flat {
            ($($dt:pat => ($variant:ident, $arr:ident)),* $(,)?) => {
                match column.data_type() {
                    $($dt => ScalarSlice::$variant(
                        column
                            .as_any()
                            .downcast_ref::<$arr>()
                            .expect("data_type checked")
                            .values(),
                    ),)*
                    DataType::Boolean => ScalarSlice::Bool(
                        column
                            .as_any()
                            .downcast_ref::<BooleanArray>()
                            .expect("data_type checked"),
                    ),
                    DataType::Utf8 => ScalarSlice::Utf8(
                        column
                            .as_any()
                            .downcast_ref::<StringArray>()
                            .expect("data_type checked"),
                    ),
                    other => panic!(
                        "columns.arrow: scalar '{name}' has an unsupported type {other:?} that \
                         should have been rejected at load"
                    ),
                }
            };
        }
        Some(flat! {
            DataType::UInt8 => (U8, UInt8Array),
            DataType::UInt16 => (U16, UInt16Array),
            DataType::UInt32 => (U32, UInt32Array),
            DataType::UInt64 => (U64, UInt64Array),
            DataType::Int8 => (I8, Int8Array),
            DataType::Int16 => (I16, Int16Array),
            DataType::Int32 => (I32, Int32Array),
            DataType::Int64 => (I64, Int64Array),
            DataType::Float32 => (F32, Float32Array),
            DataType::Float64 => (F64, Float64Array),
            DataType::Timestamp(TimeUnit::Microsecond, None) =>
                (TimestampUs, TimestampMicrosecondArray),
        })
    }

    /// Which of this segment's rows carry a value for `column` — decision 0064's bitmap, read from
    /// `presence/<column>.roaring` beside `columns.arrow`.
    ///
    /// **A column with no file is every row present**, and that is an answer rather than a
    /// missing artefact: the common column has no absences and costs no bytes, so this returns
    /// [`RenderPresence::all_present`] for it and a caller never learns which it got. An unknown
    /// column name resolves the same way, because a column that is not in the tail has no row
    /// whose value could be absent.
    ///
    /// A **category** never has a file: its vocabulary reserves code 0 out of the value space, so
    /// its absence is in the column itself (per-point-attributes §3.6). The row-space scan reads a
    /// category's absence from that code and a number's from here, and those are the only two
    /// rules.
    pub fn presence(&self, column: &str) -> &RenderPresence {
        self.presence.get(column).unwrap_or(&self.all_present)
    }
}

/// Downcast column `idx` of `batch` to `T` (one of the fixed-column array types), panicking on
/// mismatch — safe to assume because [`validate_schema`] already checked every fixed column's
/// type at `ColumnsRef::load`, before any `ColumnsRef` accessor is reachable.
fn downcast<T: Array + 'static>(batch: &RecordBatch, idx: usize) -> &T {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<T>()
        .expect("validated at ColumnsRef::load")
}

fn validate_schema(batch: &RecordBatch, path: &Path) -> Result<()> {
    let schema = batch.schema_ref();
    if schema.fields().len() < FIXED_COLUMNS.len() {
        return Err(StoreError::InvalidColumns {
            path: path.to_path_buf(),
            detail: format!(
                "expected at least {} columns, found {}",
                FIXED_COLUMNS.len(),
                schema.fields().len()
            ),
        });
    }
    for (idx, (name, ty)) in FIXED_COLUMNS.iter().enumerate() {
        let field = schema.field(idx);
        if field.name() != name || field.data_type() != ty {
            return Err(StoreError::InvalidColumns {
                path: path.to_path_buf(),
                detail: format!(
                    "column {idx}: expected '{name}' ({ty:?}), found '{}' ({:?})",
                    field.name(),
                    field.data_type()
                ),
            });
        }
        reject_nulls(batch, idx, field.name(), field.is_nullable(), path)?;
    }
    let fixed_names: std::collections::HashSet<&str> =
        FIXED_COLUMNS.iter().map(|(name, _)| *name).collect();
    for (idx, field) in schema.fields().iter().enumerate().skip(FIXED_COLUMNS.len()) {
        if fixed_names.contains(field.name().as_str()) {
            return Err(StoreError::InvalidColumns {
                path: path.to_path_buf(),
                detail: format!(
                    "declared scalar '{}' shadows a fixed column name",
                    field.name()
                ),
            });
        }
        // The accepted set is `write::arrow_type_of`'s range, and the two must move together:
        // a type this refuses is a segment the writer can produce and no reader can open.
        if !matches!(
            field.data_type(),
            DataType::Boolean
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::Float32
                | DataType::Float64
                | DataType::Timestamp(TimeUnit::Microsecond, None)
                | DataType::Utf8
        ) {
            return Err(StoreError::InvalidColumns {
                path: path.to_path_buf(),
                detail: format!(
                    "declared scalar '{}' has unsupported type {:?}",
                    field.name(),
                    field.data_type()
                ),
            });
        }
        reject_nulls(batch, idx, field.name(), field.is_nullable(), path)?;
    }
    Ok(())
}

/// Reject a column that either declares itself nullable in the schema, or (belt and braces)
/// actually carries a null in its data — every column in `columns.arrow` is contractually
/// non-nullable (R4's field table has no "nullable" column; accessors here hand back flat
/// `&[T]` slices with no validity bitmap, so a null would silently read as a garbage/zero
/// value rather than surface as an error anywhere else).
fn reject_nulls(
    batch: &RecordBatch,
    idx: usize,
    name: &str,
    is_nullable: bool,
    path: &Path,
) -> Result<()> {
    if is_nullable {
        return Err(StoreError::InvalidColumns {
            path: path.to_path_buf(),
            detail: format!("column '{name}' is declared nullable; all columns must be non-null"),
        });
    }
    if batch.column(idx).null_count() != 0 {
        return Err(StoreError::InvalidColumns {
            path: path.to_path_buf(),
            detail: format!(
                "column '{name}' contains {} null(s)",
                batch.column(idx).null_count()
            ),
        });
    }
    Ok(())
}

/// Decode the (single, uncompressed, 8-byte-aligned) record batch of an Arrow IPC FILE held in
/// `buffer`, zero-copy. Structurally the same footer/dictionary/block walk as
/// `tessera_authz::postings::decode_single_batch`, generalised to any single-record-batch
/// schema and hardened with two checks that file has no need of: `with_require_alignment(true)`
/// (fail closed on a misaligned buffer rather than silently reallocating) and an explicit
/// rejection of compressed batches (§ "no compression" in the task brief — decoding would
/// otherwise quietly succeed via an allocated, decompressed copy, defeating the zero-copy
/// contract without ever raising an error). Shared with [`crate::sidecar`], which reads the
/// same on-disk shape (uncompressed, alignment-checked, exactly one batch) for
/// `external-ids-<n>.arrow` extents — errors come back as `StoreError::InvalidColumns`
/// regardless of caller; the sidecar remaps them to `InvalidSidecar` at its call sites so the
/// message names the right file.
pub(crate) fn decode_single_batch(buffer: &Buffer, path: &Path) -> Result<RecordBatch> {
    const FOOTER_TRAILER_LEN: usize = 10; // 4-byte footer length + 6-byte "ARROW1" magic
    if buffer.len() < FOOTER_TRAILER_LEN {
        return Err(invalid_columns(path, "file too short to contain a footer"));
    }

    let trailer_start = buffer.len() - FOOTER_TRAILER_LEN;
    let trailer: [u8; FOOTER_TRAILER_LEN] = buffer[trailer_start..]
        .try_into()
        .expect("view length matches FOOTER_TRAILER_LEN");
    let footer_len = read_footer_length(trailer)
        .map_err(|e| invalid_columns(path, &format!("bad footer length: {e}")))?;
    if footer_len > trailer_start {
        return Err(invalid_columns(path, "footer length exceeds file size"));
    }

    let footer = root_as_footer(&buffer[trailer_start - footer_len..trailer_start])
        .map_err(|e| invalid_columns(path, &format!("invalid footer: {e}")))?;

    if footer.dictionaries().map(|d| d.len()).unwrap_or(0) != 0 {
        return Err(invalid_columns(
            path,
            "dictionary-encoded columns are not supported",
        ));
    }

    let schema_fb = footer
        .schema()
        .ok_or_else(|| invalid_columns(path, "footer has no schema"))?;
    let schema: SchemaRef = Arc::new(fb_to_schema(schema_fb));

    let version: MetadataVersion = footer.version();
    let decoder = FileDecoder::new(schema, version).with_require_alignment(true);

    let batches = footer
        .recordBatches()
        .ok_or_else(|| invalid_columns(path, "footer has no record batches"))?;
    if batches.len() != 1 {
        return Err(invalid_columns(
            path,
            &format!("expected exactly one record batch, found {}", batches.len()),
        ));
    }

    let block = batches.get(0);
    let (offset, body_len, meta_len) = checked_block_range(path, block, buffer.len())?;
    let data = buffer.slice_with_length(offset, body_len + meta_len);

    reject_if_compressed(path, &data, meta_len)?;

    decoder
        .read_record_batch(block, &data)
        .map_err(|e| invalid_columns(path, &format!("{e}")))?
        .ok_or_else(|| invalid_columns(path, "record batch block decoded to nothing"))
}

const CONTINUATION_MARKER: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// Parse the IPC `Message` metadata prefix of a record-batch block and reject it outright if
/// its body is compressed. This duplicates a small slice of what `FileDecoder::read_record_batch`
/// parses internally — there is no public API to ask "was this compressed?" after the fact,
/// and by the time the batch is decoded a compressed buffer has already been silently
/// reallocated into an owned, decompressed copy.
fn reject_if_compressed(path: &Path, data: &Buffer, meta_len: usize) -> Result<()> {
    if meta_len > data.len() {
        return Err(invalid_columns(
            path,
            "block metaDataLength exceeds block data",
        ));
    }
    let meta = &data[..meta_len];
    if meta.len() < 4 {
        return Err(invalid_columns(path, "message metadata too short"));
    }
    let stripped = if meta[..4] == CONTINUATION_MARKER {
        if meta.len() < 8 {
            return Err(invalid_columns(path, "message metadata too short"));
        }
        &meta[8..]
    } else {
        &meta[4..]
    };
    let message = root_as_message(stripped)
        .map_err(|e| invalid_columns(path, &format!("invalid message metadata: {e}")))?;
    let record_batch = message.header_as_record_batch().ok_or_else(|| {
        invalid_columns(
            path,
            &format!(
                "expected a RecordBatch message, found header type {:?}",
                message.header_type()
            ),
        )
    })?;
    if record_batch.compression().is_some() {
        return Err(invalid_columns(
            path,
            "compressed record batches are not supported (uncompressed buffers only, R4)",
        ));
    }
    Ok(())
}

/// Validate a footer `Block`'s `(offset, bodyLength, metaDataLength)` against the file length,
/// returning them as checked `usize`s (see `tessera_authz::postings::checked_block_range` for
/// the identical rationale: `Block`'s fields are `i64` in the flatbuffer schema, and
/// `Buffer::slice_with_length` panics on out-of-bounds input rather than erroring).
fn checked_block_range(
    path: &Path,
    block: &Block,
    buffer_len: usize,
) -> Result<(usize, usize, usize)> {
    let offset = usize::try_from(block.offset())
        .map_err(|_| invalid_columns(path, "block offset is negative"))?;
    let body_len = usize::try_from(block.bodyLength())
        .map_err(|_| invalid_columns(path, "block bodyLength is negative"))?;
    let meta_len = usize::try_from(block.metaDataLength())
        .map_err(|_| invalid_columns(path, "block metaDataLength is negative"))?;
    let total = body_len
        .checked_add(meta_len)
        .ok_or_else(|| invalid_columns(path, "block length overflows"))?;
    let end = offset
        .checked_add(total)
        .ok_or_else(|| invalid_columns(path, "block offset + length overflows"))?;
    if end > buffer_len {
        return Err(invalid_columns(
            path,
            &format!("block range [{offset}, {end}) exceeds file length {buffer_len}"),
        ));
    }
    Ok((offset, body_len, meta_len))
}

fn invalid_columns(path: &Path, detail: &str) -> StoreError {
    StoreError::InvalidColumns {
        path: path.to_path_buf(),
        detail: detail.to_string(),
    }
}

/// The row range `tile` occupies within `seg`'s Morton order, found by binary search over
/// `seg.morton.u32()` (contracts §2.5). Callers must treat a tile as resolving to a **set** of
/// ranges — one per segment sharing the tile's view — even though a build writes exactly one
/// segment per view; the engine-level signature is `Vec<Range<u32>>` accordingly.
///
/// `Tile::code_range` returns `u64` bounds deliberately: at depth 0 the exclusive end is
/// `1 << 32`, which does not fit in `u32`. Each stored code is widened for the comparison
/// rather than the bounds being narrowed, which would overflow to an empty range there.
pub fn tile_ranges(seg: &SegmentData, tile: &Tile) -> Range<u32> {
    tile_ranges_within(seg, tile, 0..seg.morton.len() as u32)
}

/// [`tile_ranges`], but searching only `within` rather than the whole column.
///
/// For a tile **contained** in `within` this returns exactly what [`tile_ranges`] would, because the
/// Morton column is sorted: every row of a contained tile lies inside its container's contiguous
/// range, so restricting the search cannot exclude a matching row.
///
/// It exists for the §7.3 underlay, which resolves `4^offset` sub-cells per tile whose parent range
/// is already in hand. Searching the whole column for each is not merely wasteful but wasteful at a
/// bad ratio: at 10⁹ rows `morton.u32` is 4 GB, so each `partition_point` walks ~30 levels of a
/// mostly-cold mmap, and the underlay's ceiling of 8,192 sub-cells makes that 16,384 such walks per
/// request — a large fraction of a 10 ms budget spent re-deriving a bound the caller already knows.
/// Restricted to the parent, the search is over a few tens of kilobytes that the parent's own
/// `count_range` has already touched.
///
/// It is also the more honest construction: it *expresses* "sub-cells partition their parent"
/// rather than searching the whole column again and relying on that being true.
pub fn tile_ranges_within(seg: &SegmentData, tile: &Tile, within: Range<u32>) -> Range<u32> {
    let codes = seg.morton.u32();
    let lo_idx = within.start as usize;
    let hi_idx = (within.end as usize).min(codes.len());
    if hi_idx <= lo_idx {
        return within.start..within.start;
    }
    let window = &codes[lo_idx..hi_idx];
    let (lo, hi) = tile.code_range();
    let start = lo_idx + window.partition_point(|&c| (c as u64) < lo);
    let end = lo_idx + window.partition_point(|&c| (c as u64) < hi);
    start as u32..end as u32
}

/// `from + codes[from..].partition_point(|&c| (c as u64) < target)`, reached by doubling out
/// from `from` before binary searching the bracket that lands in — "galloping" (exponential)
/// search.
///
/// **The contract is relative and unconditional**: whatever `from` is, this returns the
/// partition point *of the tail beginning at `from`*. It never inspects `codes[..from]`, so it
/// is not a drop-in for a full-column search — it equals one exactly when `from` is at or below
/// the true partition point, which is the caller's obligation to establish. Stating it this way
/// is deliberate: the property test can then check it against `partition_point` over arbitrary
/// views and arbitrary `from`, with nothing assumed.
///
/// **Why galloping and not a binary search of the tail.** [`tile_ranges_all`] sweeps tiles in
/// ascending code order, so successive searches are a short hop apart — usually zero rows apart
/// in a sparse viewport, where most tiles are empty and share a partition point. A binary search
/// costs `log2(tail)` regardless; galloping costs `log2(distance actually travelled)`, which is
/// one or two comparisons in exactly that case. That difference is the measured win (see
/// [`tile_ranges_all`]).
fn gallop(codes: &[u32], from: usize, target: u64) -> usize {
    let tail = &codes[from.min(codes.len())..];
    // Double the probe until it reaches an element that is NOT below `target`, or runs off the
    // end. `lo` trails one step behind and is the last index proven to be entirely below
    // `target`, so the answer is bracketed in `[lo, hi)` when the loop exits.
    let mut lo = 0usize;
    let mut hi = 1usize;
    while hi <= tail.len() && (tail[hi - 1] as u64) < target {
        lo = hi;
        // Saturating so the doubling cannot wrap on a pathologically long column; a saturated
        // `hi` simply fails the loop condition and clamps to the length below.
        hi = hi.saturating_mul(2);
    }
    let hi = hi.min(tail.len());
    from.min(codes.len()) + lo + tail[lo..hi].partition_point(|&c| (c as u64) < target)
}

/// Resolve every tile in `tiles` against `seg` in one forward sweep, returning ranges
/// **positionally aligned with `tiles`** — `out[i]` is `tile_ranges(seg, &tiles[i])`, for every
/// `i`, with no exceptions and no reordering.
///
/// # Why this exists
///
/// Called once per tile, [`tile_ranges`] does two full-column binary searches. A viewport asks
/// for a few hundred tiles, so a sparse request spends most of its time binary searching
/// `morton.u32` several hundred times over — measured at 26–64% of a low-density request
/// (docs/evidence/memos/2026-07-30-f1-selection-overdraw.md), and *flat in density*, because the
/// cost is the searching, not the rows found. At 2.42M rows, zoom 8, 289 tiles, that is ~20 µs of
/// a 31 µs request.
///
/// This replaces `2 × tiles` independent `log2(rows)` searches with one monotone sweep of
/// [`gallop`]s, each costing `log2(distance from the previous tile)` — measured at 20.0 µs →
/// 4.5 µs on that request, and 21.8 µs → 4.6 µs on the same viewport over 25M rows.
///
/// Note what the win is *not*. The doubling was expected to pay mostly in avoided page faults on
/// a cold 4 GB column; at these fixture scales the column is resident and it pays in avoided
/// *probes* instead, which is why the ratio is roughly the probe-count ratio and not larger. The
/// page-fault saving is still there at 10^9 rows, and only makes the case stronger.
///
/// # Why it is correct
///
/// One lemma carries the whole thing: `partition_point(|c| c < t)` over an ascending column is
/// **non-decreasing in `t`** — a larger threshold can only admit more codes. `MortonSlice::load`
/// is what guarantees the column is ascending (it refuses to open one that is not), so the lemma
/// is not an assumption about the data.
///
/// [`gallop`]'s contract holds for any `from`, and equals the full-column search exactly when
/// `from` is at or below the answer. This sweep establishes that in both places it calls it:
///
/// - `start`: the floor begins at 0, and thereafter is the previous tile's `start`. Tiles are
///   visited in ascending `code_range().0`, so by the lemma their starts are ascending too.
/// - `end`: floored at this tile's own `start`, and `lo <= hi` for every tile's code range, so
///   by the lemma `start <= end`.
///
/// # The three traps
///
/// **Sort by the code-range low bound, never by `prefix`.** Prefixes are only comparable at
/// equal depth — prefix 1 at depth 1 covers a lower code range than prefix 3 at depth 16, but
/// compares greater. `tiles_for_bbox` happens to hand back one depth, but this function is
/// public and its correctness must not rest on its caller's habits. Ordering by `lo` assumes
/// nothing about the tiles at all: not equal depth, not disjointness, not uniqueness.
///
/// **The floor for the next tile is this tile's `start`, not its `end`.** `end` would be
/// tighter, and is sound only while no two tiles share a code range; a repeated tile would then
/// be searched from beyond its own start and silently come back empty. `start` is monotone
/// whatever the tile set contains, and the extra ground a gallop re-covers is one tile's worth
/// of rows — logarithmic, and not the cost this function exists to remove.
///
/// **`tiles_for_bbox`'s raster order is not Morton order**, and the response's tile order is
/// load-bearing: `ViewportOut.tiles` orders the wire payload's flat point concatenation and the
/// reference oracle's comparison. The sweep therefore reorders an index vector, never `tiles`,
/// and writes each result back at its caller-supplied index.
pub fn tile_ranges_all(seg: &SegmentData, tiles: &[Tile]) -> Vec<Range<u32>> {
    let mut out = vec![0u32..0u32; tiles.len()];
    if tiles.is_empty() {
        return out;
    }
    let codes = seg.morton.u32();

    let mut order: Vec<usize> = (0..tiles.len()).collect();
    order.sort_unstable_by_key(|&i| tiles[i].code_range().0);

    let mut floor = 0usize;
    for i in order {
        let (lo, hi) = tiles[i].code_range();
        let start = gallop(codes, floor, lo);
        let end = gallop(codes, start, hi);
        floor = start;
        out[i] = start as u32..end as u32;
    }
    out
}

/// Whether the streaming passes issue `madvise(MADV_SEQUENTIAL)` on the mappings they open —
/// **on by default**, and switchable only so a probe can measure what it is worth.
///
/// Compaction §6.1 rules the hint in (decision 0052) and §14 recorded its effect as *unmeasured*:
/// P3 measured the harm an unthrottled streaming read does to a concurrent viewport and could not
/// measure any mitigation, because it modelled a buffered reader and the fold's inputs are all
/// mappings. Answering "does the hint help" needs the same fold run twice, and a compile-time
/// constant cannot be run twice. Probe **P4** is what runs it.
///
/// A process-global rather than a parameter threaded to every cursor: the answer is the same for
/// every mapping in a process, the only writer is a probe before it starts, and a parameter would
/// put a measurement's switch in the signature of two hot constructors.
static STREAMING_ADVICE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn streaming_advice_enabled() -> bool {
    STREAMING_ADVICE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Turn [`STREAMING_ADVICE`] off or on. **Probe use only** — a deployment has no reason to want the
/// hint off, and this is not a configuration key.
pub fn set_streaming_advice_for_test(enabled: bool) {
    STREAMING_ADVICE.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
mod gallop_tests {
    use super::gallop;

    /// [`gallop`]'s whole contract, checked against the standard-library function it stands in
    /// for, over every `from` and every target of interest on a range of slice shapes — empty,
    /// singleton, strictly ascending, and heavily duplicated (equal codes are what a Morton
    /// column of co-located points actually looks like, and they are where an off-by-one in the
    /// bracket shows up).
    #[test]
    fn gallop_equals_the_tails_partition_point_for_every_start_and_target() {
        let columns: Vec<Vec<u32>> = vec![
            vec![],
            vec![5],
            (0..64u32).collect(),
            (0..64u32).map(|c| c * 7).collect(),
            (0..64u32).map(|c| c / 8).collect(), // eight-fold duplicates
            vec![0; 33],
            vec![u32::MAX; 9],
        ];
        for codes in &columns {
            for from in 0..=codes.len() + 2 {
                // Every stored value, its neighbours, and both extremes of the u64-widened
                // comparison space `Tile::code_range` produces (its depth-0 end is `1 << 32`).
                let mut targets: Vec<u64> = vec![0, 1, u64::from(u32::MAX), 1u64 << 32];
                for &c in codes {
                    targets.extend([
                        u64::from(c).saturating_sub(1),
                        u64::from(c),
                        u64::from(c) + 1,
                    ]);
                }
                for target in targets {
                    let clamped = from.min(codes.len());
                    let expected =
                        clamped + codes[clamped..].partition_point(|&c| u64::from(c) < target);
                    assert_eq!(
                        gallop(codes, from, target),
                        expected,
                        "codes {codes:?} from {from} target {target}"
                    );
                }
            }
        }
    }
}
