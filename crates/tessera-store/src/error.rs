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
    NoVerifyingSegmentsManifest { partition: String },
    /// A manifest referenced a partition/slice/segment directory structure that doesn't exist
    /// or doesn't match the expected `columns.arrow` / `morton.u64` / `permutation.bin` shape.
    MalformedBundle { detail: String },
    /// `columns.arrow` failed Arrow IPC / schema validation (wrong column count, name, type,
    /// more than one record batch, compressed buffers, or misaligned buffers).
    InvalidColumns { path: PathBuf, detail: String },
    /// `permutation.bin` failed header/length validation.
    InvalidPermutation { path: PathBuf, detail: String },
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
            StoreError::NoVerifyingSegmentsManifest { partition } => write!(
                f,
                "no verifying SEGMENTS-<n>.json found for partition '{partition}'"
            ),
            StoreError::MalformedBundle { detail } => write!(f, "malformed bundle: {detail}"),
            StoreError::InvalidColumns { path, detail } => {
                write!(f, "invalid columns.arrow at {}: {detail}", path.display())
            }
            StoreError::InvalidPermutation { path, detail } => {
                write!(f, "invalid permutation.bin at {}: {detail}", path.display())
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
