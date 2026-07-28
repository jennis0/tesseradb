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
        }
    }
}

impl std::error::Error for BuildError {}

impl From<tessera_store::error::StoreError> for BuildError {
    fn from(e: tessera_store::error::StoreError) -> Self {
        BuildError::Store(e)
    }
}

impl From<tessera_plugin::PluginError> for BuildError {
    fn from(e: tessera_plugin::PluginError) -> Self {
        BuildError::Plugin(e.to_string())
    }
}
