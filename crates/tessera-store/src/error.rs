//! `tessera-store`'s error type. Every failure mode in the read protocol (digest mismatch,
//! unsupported `bundle_format`, no verifying `SEGMENTS-<n>.json`, malformed Arrow, malformed
//! `permutation.bin`) is a variant here — the read protocol is fail-closed (shared-context
//! constraint 3): any of these turns bundle open into a hard error, never a partial `Bundle`.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// `tessera-store`'s result alias.
pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug)]
pub enum StoreError {
    /// An I/O error reading a bundle file, with the path that failed.
    Io { path: PathBuf, source: io::Error },
    /// A file's bytes didn't parse as the JSON shape expected of it.
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// `CURRENT.manifest_digest` didn't match the SHA-256 of the fetched `MANIFEST.json`.
    ManifestDigestMismatch { expected: String, actual: String },
    /// `MANIFEST.json`'s `bundle_format` is newer than this reader supports.
    UnsupportedBundleFormat { found: u32, max_supported: u32 },
    /// A file named in a manifest's `files` map failed size or SHA-256 verification.
    FileVerificationFailed { path: PathBuf, reason: String },
    /// No `SEGMENTS-<n>.json` for a partition verified, at any `n` — the bundle is unusable
    /// for that partition (and therefore, fail-closed, unusable at all).
    ///
    /// `highest_candidate_error` carries the reason the **highest** `n` failed — the newest
    /// manifest the partition published, and the one an operator must actually fix. The loop
    /// walks highest-first and keeps the *first* reason rather than overwriting per iteration:
    /// overwriting leaves the oldest candidate's reason, which is the least actionable one on
    /// offer (an ancient manifest failing is expected once a newer one exists; the newest one
    /// failing is the incident).
    ///
    /// One reason, not a per-candidate `Vec`: a partition directory accumulates
    /// `SEGMENTS-<n>.json` files without bound, and the older candidates almost always failed
    /// for the same reason or for one that no longer matters — an unbounded list in an
    /// operator's face buries the line that names the fix.
    NoVerifyingSegmentsManifest {
        partition: String,
        highest_candidate_error: Option<String>,
    },
    /// A `SEGMENTS-<n>.json` carries state this reader does not implement, and the state is of
    /// the kind that may not be stepped past: a non-empty `tombstones` or `deny` (contracts
    /// §2.3's publication rule). **The partition is unready, not merely stale.**
    ///
    /// A manifest carries either field *because a deny was accepted*. Falling back to an older
    /// manifest would therefore re-expose every entity suppressed or deleted since that older
    /// one was written, indefinitely and silently — which is exactly the state §2.3 forbids a
    /// syncing replica to reconstruct. Between hard-down and serving suppressed items forever,
    /// this corpus chooses hard-down (SA §9: "a worker that cannot verify its partition marks
    /// itself unready rather than serving partial data").
    ///
    /// **The operator response is to move the reader, not to rebuild.** The shape that produces
    /// this error is a writer ahead of its reader — a manifest published by a build that
    /// implements suppression against a replica that does not — so "run another build" usually
    /// cannot fix it: the next build publishes the same fields. The remedy is to upgrade this
    /// replica to a reader that honours them, or to roll the writer back to one that does not
    /// publish them. That is why the variant names the fields rather than reporting a bare
    /// boolean: the field names *are* the missing capability.
    ///
    /// `fields` is `&'static str` by construction: these are field *names* from
    /// [`crate::manifest::SegmentsManifest`], never entity IDs, so nothing item-shaped can
    /// reach a log through this variant (SA §9).
    UnhonourableManifest {
        partition: String,
        n: u64,
        fields: Vec<&'static str>,
    },
    /// A `SEGMENTS-<n>.json` carrying deny-disposition state (contracts §2.3) failed file
    /// verification. **Unready, never stepped past** — which is the whole of the distinction from
    /// the ordinary verification failure that continues the candidate walk.
    ///
    /// The classification guard cannot catch this one: once `deny` and `tombstones` are honoured
    /// a deny-carrying manifest is `Honourable`, so it reaches `verify_files` like any other, and
    /// a `continue` there would step down to an older manifest that re-exposes every entity
    /// denied since it was written — the fail-open decision 0018 promoted into contract, in
    /// exactly the damaged-newest case this reader names as most likely.
    ///
    /// `detail` is the underlying verification failure, so an operator still learns which file
    /// went wrong; the posture is not negotiable regardless of which one it was.
    UnverifiedDenyManifest {
        partition: String,
        n: u64,
        fields: Vec<&'static str>,
        detail: String,
    },
    /// A manifest referenced a partition/view/segment directory structure that doesn't exist
    /// or doesn't match the expected `columns.arrow` / `morton.u32` / `permutation.bin` shape.
    MalformedBundle { detail: String },
    /// A file the loader is about to open has no corresponding entry in either the chosen
    /// `SEGMENTS-<n>.json`'s `files` map or `MANIFEST.json`'s — i.e. its bytes were never
    /// digest-verified. Reading it anyway would defeat the entire read protocol (a manifest
    /// with an empty or partial `files` map would otherwise verify vacuously). Fail closed
    /// rather than open a file the manifest never vouched for.
    UnverifiedFile { path: PathBuf },
    /// [`crate::reclaim::reclaim_prefix`] was asked to delete the prefix `CURRENT` still names.
    ///
    /// **Its own variant because this is the only operation in the system that deletes bundle
    /// data**, and a wrong argument to it is unrecoverable. A caller — or a test — that has to
    /// match on free text to tell "refused, and nothing was deleted" from any other malformed-input
    /// error is one string edit away from not noticing when the refusal stops firing.
    ReclaimRefused { prefix: String, current: String },
    /// A manifest-derived path component (partition `phash`, view/segment id, or a `files`
    /// map key) was rejected before ever being joined onto a filesystem path — empty, `.`,
    /// `..`, absolute, or containing a path separator where a single opaque component was
    /// expected. Bundle contents are trusted for shape but never for path escape.
    UnsafePath { what: String, value: String },
    /// Writing `terms/pairs.parquet` failed in the Parquet encoder — see [`crate::pairs`]. Its
    /// own variant rather than `Io` or `MalformedBundle`: an encoder failure is neither a
    /// filesystem fault nor a claim about a bundle already on disc, and the two producers (a build
    /// and compaction's pass 2) both need to report it as what it is.
    Parquet { path: PathBuf, detail: String },
    /// `columns.arrow` failed Arrow IPC / schema validation (wrong column count, name, type,
    /// more than one record batch, compressed buffers, or misaligned buffers).
    InvalidColumns { path: PathBuf, detail: String },
    /// `permutation.bin` failed header/length/content validation.
    InvalidPermutation { path: PathBuf, detail: String },
    /// `MANIFEST.json`'s `identity` object named an unknown construction or a round count this
    /// reader doesn't implement — a bundle written by a different `tessera_id` construction
    /// must not be silently read by this one (contracts §2.6 r6).
    InvalidIdentity { detail: String },
    /// The external-ID sidecar (contracts §2.4 r6, §0.3 deviation 9) failed closed: a missing
    /// digest entry, a digest mismatch, a schema mismatch, an out-of-order extent, a shuffled
    /// extent list, or a missing/corrupt/out-of-range locator. Every one of these is fail-closed
    /// on purpose (see `crate::sidecar`'s module doc) — a `None` here would read as "no such
    /// external id" and could turn a WAL-resident suppression into a silent no-op.
    InvalidSidecar { path: PathBuf, detail: String },
    /// A directory entry in a partition directory is spelled like a `SEGMENTS-<n>.json` but is
    /// not the canonical name of any `n` (contracts §2.1): a leading zero, an empty or
    /// non-numeric part, a sign, whitespace, or a value past `u64`.
    ///
    /// **Refusing the name is what keeps a padded manifest from being silently stepped past.**
    /// `n` is unpadded decimal, so a reader that parses `SEGMENTS-01.json` to `n = 1` and then
    /// reconstructs `SEGMENTS-1.json` to read from discovers a manifest and reads a different or
    /// absent file — an I/O failure the candidate walk records and steps past, carrying the
    /// reader past a manifest that may hold a `deny`. §2.1: "Parsing leniently and reconstructing
    /// canonically is the combination that hides it."
    ///
    /// **Its own variant so a caller can tell this from an absent manifest**, which is exactly
    /// the confusion the refusal exists to end: [`StoreError::NoVerifyingSegmentsManifest`] says
    /// nothing verified, and folding a mis-named file into it would report a manifest that is
    /// present and unread as one that is not there. Nothing in this repository writes such a
    /// name; a file carrying one arrived from outside the writer, and the operator response is to
    /// rename it to its canonical spelling or remove it.
    NonCanonicalManifestName { partition: String, name: String },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
            StoreError::Json { path, source } => {
                write!(f, "invalid JSON at {}: {source}", path.display())
            }
            StoreError::ManifestDigestMismatch { expected, actual } => write!(
                f,
                "MANIFEST.json digest mismatch: CURRENT said {expected}, computed {actual}"
            ),
            StoreError::UnsupportedBundleFormat {
                found,
                max_supported,
            } => write!(
                f,
                "bundle_format {found} is newer than this reader supports (max {max_supported})"
            ),
            StoreError::FileVerificationFailed { path, reason } => {
                write!(
                    f,
                    "file verification failed for {}: {reason}",
                    path.display()
                )
            }
            StoreError::NoVerifyingSegmentsManifest {
                partition,
                highest_candidate_error,
            } => match highest_candidate_error {
                Some(reason) => write!(
                    f,
                    "no verifying SEGMENTS-<n>.json found for partition '{partition}' \
                     (highest candidate failed: {reason})"
                ),
                None => write!(
                    f,
                    "no verifying SEGMENTS-<n>.json found for partition '{partition}' \
                     (no SEGMENTS-<n>.json present)"
                ),
            },
            StoreError::UnhonourableManifest {
                partition,
                n,
                fields,
            } => write!(
                f,
                "SEGMENTS-{n}.json for partition '{partition}' carries state this reader does \
                 not honour ({}); the writer is ahead of this reader — upgrade the reader, or \
                 roll back the writer that published these fields. The partition is unready \
                 until then, deliberately: stepping down past this would undo an accepted deny",
                fields.join(", ")
            ),
            StoreError::UnverifiedDenyManifest {
                partition,
                n,
                fields,
                detail,
            } => write!(
                f,
                "SEGMENTS-{n}.json for partition '{partition}' carries deny-disposition state \
                 ({}) and its files did not verify: {detail}. The partition is unready. It is \
                 NOT stepped past to an older manifest, deliberately: that would re-expose every \
                 entity denied since the older one was written. Repair or re-sync the files this \
                 manifest names",
                fields.join(", ")
            ),
            StoreError::MalformedBundle { detail } => write!(f, "malformed bundle: {detail}"),
            StoreError::UnverifiedFile { path } => write!(
                f,
                "refusing to open {} — not covered by any verified `files` entry",
                path.display()
            ),
            StoreError::ReclaimRefused { prefix, current } => write!(
                f,
                "refusing to reclaim prefix '{prefix}': CURRENT still names '{current}' as live, \
                 and reclamation is the one operation here that deletes bundle data"
            ),
            StoreError::NonCanonicalManifestName { partition, name } => write!(
                f,
                "refusing '{name}' in partition '{partition}': SEGMENTS-<n>.json's n is unpadded \
                 decimal (contracts §2.1), so this is not the canonical name of any n. It is \
                 refused rather than parsed: parsing it and reading the canonical name back \
                 would step silently past a manifest that may carry a deny. Rename it to its \
                 canonical spelling or remove it"
            ),
            StoreError::UnsafePath { what, value } => {
                write!(f, "unsafe path in manifest ({what}): '{value}'")
            }
            StoreError::Parquet { path, detail } => {
                write!(f, "writing {}: {detail}", path.display())
            }
            StoreError::InvalidColumns { path, detail } => {
                write!(f, "invalid columns.arrow at {}: {detail}", path.display())
            }
            StoreError::InvalidPermutation { path, detail } => {
                write!(f, "invalid permutation.bin at {}: {detail}", path.display())
            }
            StoreError::InvalidIdentity { detail } => {
                write!(f, "invalid identity descriptor: {detail}")
            }
            StoreError::InvalidSidecar { path, detail } => {
                write!(
                    f,
                    "invalid external-ID sidecar at {}: {detail}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io { source, .. } => Some(source),
            StoreError::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Read `path` fully into a `Vec<u8>`, wrapping any I/O error with the path (fail-closed
/// bundle reads need to say *which* file was unreadable, not just "some I/O error").
pub(crate) fn read_to_vec(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}
