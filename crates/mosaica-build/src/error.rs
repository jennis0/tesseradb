//! Build errors. Every failure is typed and fatal — a batch build that half-succeeds would
//! leave a bundle whose `CURRENT` points at an incomplete prefix, so there is no partial
//! success path here (fail closed, shared-context constraint 3).

use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, BuildError>;

#[derive(Debug)]
pub enum BuildError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parquet {
        path: PathBuf,
        detail: String,
    },
    Arrow {
        path: PathBuf,
        detail: String,
    },
    Schema {
        path: PathBuf,
        detail: String,
    },
    /// A refusal from `schema.toml` or a vocabulary file bound to it — a *declaration* the design
    /// forbids, rather than a malformed file. Carries no path because the message names the
    /// attribute and the rule, which is what an operator acts on; most of these are refusals
    /// whose whole content is the reason (see [`crate::schema`]'s module doc).
    Declaration(String),
    /// The input violates an invariant the bundle format depends on.
    Invalid(String),
    Store(mosaica_store::error::StoreError),
    /// A `mosaica_id` derivation failed — in this build, always
    /// [`mosaica_types::IdentityError::EntityOutOfRange`], which the allocator cap (I-1) makes
    /// unreachable in practice. Never a truncation: see `IdentityKey::forward`'s doc comment.
    Identity(mosaica_types::IdentityError),
}

impl BuildError {
    pub fn io(path: &Path, source: std::io::Error) -> Self {
        BuildError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
    pub fn parquet(path: &Path, e: parquet::errors::ParquetError) -> Self {
        BuildError::Parquet {
            path: path.to_path_buf(),
            detail: e.to_string(),
        }
    }
    pub fn arrow(path: &Path, e: arrow::error::ArrowError) -> Self {
        BuildError::Arrow {
            path: path.to_path_buf(),
            detail: e.to_string(),
        }
    }
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            BuildError::Parquet { path, detail } => {
                write!(f, "{}: parquet error: {detail}", path.display())
            }
            BuildError::Arrow { path, detail } => {
                write!(f, "{}: arrow error: {detail}", path.display())
            }
            BuildError::Schema { path, detail } => {
                write!(f, "{}: unusable schema: {detail}", path.display())
            }
            BuildError::Declaration(detail) => write!(f, "schema: {detail}"),
            BuildError::Invalid(detail) => write!(f, "invalid input: {detail}"),
            BuildError::Store(e) => write!(f, "store error: {e}"),
            BuildError::Identity(e) => write!(f, "identity error: {e}"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<mosaica_store::error::StoreError> for BuildError {
    fn from(e: mosaica_store::error::StoreError) -> Self {
        BuildError::Store(e)
    }
}

impl From<mosaica_types::IdentityError> for BuildError {
    fn from(e: mosaica_types::IdentityError) -> Self {
        BuildError::Identity(e)
    }
}

