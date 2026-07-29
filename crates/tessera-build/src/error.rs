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
    Plugin(String),
    /// The input violates an invariant the bundle format depends on.
    Invalid(String),
    Store(tessera_store::error::StoreError),
    /// A `tessera_id` derivation failed — in this build, always
    /// [`tessera_types::IdentityError::EntityOutOfRange`], which the allocator cap (I-1) makes
    /// unreachable in practice. Never a truncation: see `IdentityKey::forward`'s doc comment.
    Identity(tessera_types::IdentityError),
    /// Contracts §1 r6: an external ID longer than 64 bytes is a typed error at build, never a
    /// truncation — a truncated key is a *different* key, and two callers' keys sharing a
    /// 64-byte prefix would collide into one entity.
    ExternalIdTooLong {
        len: usize,
    },
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
            BuildError::Plugin(detail) => write!(f, "plugin error: {detail}"),
            BuildError::Invalid(detail) => write!(f, "invalid input: {detail}"),
            BuildError::Store(e) => write!(f, "store error: {e}"),
            BuildError::Identity(e) => write!(f, "identity error: {e}"),
            BuildError::ExternalIdTooLong { len } => write!(
                f,
                "external id is {len} bytes, exceeding the 64-byte cap (contracts §1); refused \
                 rather than truncated, since a truncated key is a different key"
            ),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<tessera_store::error::StoreError> for BuildError {
    fn from(e: tessera_store::error::StoreError) -> Self {
        BuildError::Store(e)
    }
}

impl From<tessera_types::IdentityError> for BuildError {
    fn from(e: tessera_types::IdentityError) -> Self {
        BuildError::Identity(e)
    }
}

impl From<tessera_plugin::PluginError> for BuildError {
    fn from(e: tessera_plugin::PluginError) -> Self {
        BuildError::Plugin(e.to_string())
    }
}
