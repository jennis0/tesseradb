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
    /// for that partition (and therefore, fail-closed, unusable at all). `last_error` carries
    /// the reason the highest (most recently tried) candidate failed, so a caller isn't left
    /// with only "nothing verified" when there's a more specific, actionable cause.
    NoVerifyingSegmentsManifest {
        partition: String,
        last_error: Option<String>,
    },
    /// A manifest referenced a partition/slice/segment directory structure that doesn't exist
    /// or doesn't match the expected `columns.arrow` / `morton.u32` / `permutation.bin` shape.
    MalformedBundle { detail: String },
    /// A file the loader is about to open has no corresponding entry in either the chosen
    /// `SEGMENTS-<n>.json`'s `files` map or `MANIFEST.json`'s — i.e. its bytes were never
    /// digest-verified. Reading it anyway would defeat the entire read protocol (a manifest
    /// with an empty or partial `files` map would otherwise verify vacuously). Fail closed
    /// rather than open a file the manifest never vouched for.
    UnverifiedFile { path: PathBuf },
    /// A manifest-derived path component (partition `phash`, slice/segment id, or a `files`
    /// map key) was rejected before ever being joined onto a filesystem path — empty, `.`,
    /// `..`, absolute, or containing a path separator where a single opaque component was
    /// expected. Bundle contents are trusted for shape but never for path escape.
    UnsafePath { what: String, value: String },
    /// `columns.arrow` failed Arrow IPC / schema validation (wrong column count, name, type,
    /// more than one record batch, compressed buffers, or misaligned buffers).
    InvalidColumns { path: PathBuf, detail: String },
    /// `permutation.bin` failed header/length/content validation.
    InvalidPermutation { path: PathBuf, detail: String },
    /// `MANIFEST.json`'s `identity` object named an unknown construction or a round count this
    /// reader doesn't implement — a bundle written by a different `tessera_id` construction
    /// must not be silently read by this one (contracts §2.6 r6).
    InvalidIdentity { detail: String },
    /// An `external-ids-<n>.arrow` extent failed Arrow IPC / schema validation, or failed the
    /// within-extent ascending-order check (R4).
    InvalidExternalIds { path: PathBuf, detail: String },
    /// TEMPORARY (Task 2, `skip-id-index` measurement feature — removed in Task 8): the
    /// external-ID index was never loaded (`ExternalIdIndex::disabled`) and every resolution
    /// fails closed with this variant rather than silently returning `None`, which would read as
    /// "no such external id" and turn a WAL-resident suppression into a no-op. Never returned
    /// outside a `skip-id-index` build.
    IdIndexDisabled,
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
                last_error,
            } => match last_error {
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
            StoreError::MalformedBundle { detail } => write!(f, "malformed bundle: {detail}"),
            StoreError::UnverifiedFile { path } => write!(
                f,
                "refusing to open {} — not covered by any verified `files` entry",
                path.display()
            ),
            StoreError::UnsafePath { what, value } => {
                write!(f, "unsafe path in manifest ({what}): '{value}'")
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
            StoreError::InvalidExternalIds { path, detail } => {
                write!(
                    f,
                    "invalid external-ids extent at {}: {detail}",
                    path.display()
                )
            }
            StoreError::IdIndexDisabled => write!(
                f,
                "external-ID index disabled (skip-id-index measurement build) — refusing to \
                 resolve rather than returning a fail-open None"
            ),
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
