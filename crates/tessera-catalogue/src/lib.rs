//! The identity catalogue: local principals and their credentials, groups, the terms and
//! permissions granted to each, and the OIDC providers whose claims map to terms.
//!
//! The catalogue is a SQLite database in a directory the caller names, independent of any
//! bundle, so principals and grants carry across a rebuild. The whole of it is held in memory
//! and no read touches SQLite. A change commits to SQLite first and then updates memory; a change
//! that fails to commit leaves both as they were.
//!
//! Every change returns an [`Affected`]: the principals, API keys and providers whose sessions it
//! may have changed the terms or permissions of. The caller ends those sessions.
//!
//! This crate sees names and terms only. It depends on nothing that can see a row id or an
//! entity id.

mod apikey;
mod catalogue;
mod names;
mod password;
mod permission;
mod provider;
mod store;

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use catalogue::{
    ApiKeyInfo, Authenticated, Catalogue, Grantee, GroupInfo, IssuedKey, PrincipalInfo,
    PrincipalKind, ProviderInfo, Resolution,
};
pub use names::PUBLIC;
pub use permission::{Permission, PermissionSet};
pub use provider::{ClaimMapping, ClaimRule, Provider, RuleTarget};

/// Seconds since the Unix epoch.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// How a catalogue is opened.
#[derive(Clone)]
pub struct Options {
    /// Failed password attempts for one principal within `failed_attempt_window` after which
    /// further attempts are refused until the oldest of them leaves the window.
    pub failed_attempt_limit: u32,
    pub failed_attempt_window: Duration,
    /// The time, for API key expiry and the failed-attempt window.
    pub clock: Clock,
    /// Providers declared in the deployment's configuration. They are listed with the stored
    /// ones and cannot be changed or removed through the catalogue.
    pub config_providers: Vec<Provider>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            failed_attempt_limit: 10,
            failed_attempt_window: Duration::from_secs(15 * 60),
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
            }),
            config_providers: Vec::new(),
        }
    }
}

/// What a change may have altered the terms or permissions of. Every session of each principal,
/// every session authorised with each API key, and every session authorised through each
/// provider is to be ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Affected {
    pub principals: BTreeSet<String>,
    /// API key prefixes.
    pub api_keys: BTreeSet<String>,
    pub providers: BTreeSet<String>,
}

impl Affected {
    pub fn is_empty(&self) -> bool {
        self.principals.is_empty() && self.api_keys.is_empty() && self.providers.is_empty()
    }
}

/// Why a catalogue operation was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input breaks a rule of what may be stored.
    Invalid(String),
    NotFound {
        what: &'static str,
        name: String,
    },
    Exists {
        what: &'static str,
        name: String,
    },
    /// The provider is declared in the configuration and cannot be changed here.
    ReadOnly {
        provider: String,
    },
    /// A provider is declared twice, in the configuration and the catalogue or twice in the
    /// configuration.
    DeclaredTwice {
        provider: String,
    },
    /// The database was written by a different version of the catalogue.
    Version {
        found: i64,
        supported: i64,
    },
    /// SQLite or the file system failed. Nothing was changed.
    Storage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(why) => f.write_str(why),
            Error::NotFound { what, name } => {
                write!(f, "there is no {what} named `{name}`; create it first")
            }
            Error::Exists { what, name } => {
                write!(
                    f,
                    "a {what} named `{name}` already exists; choose another name"
                )
            }
            Error::ReadOnly { provider } => write!(
                f,
                "provider `{provider}` is declared in the configuration file; change it there \
                 and restart"
            ),
            Error::DeclaredTwice { provider } => write!(
                f,
                "provider `{provider}` is declared more than once; remove it from the \
                 configuration file or from the catalogue"
            ),
            Error::Version { found, supported } => write!(
                f,
                "the catalogue has schema version {found} and this build reads version \
                 {supported}; open it with the build that wrote it or recreate it"
            ),
            Error::Storage(why) => write!(f, "the catalogue could not be read or written: {why}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Storage(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Storage(e.to_string())
    }
}

/// Why a credential was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// Unknown principal or key, wrong secret, disabled principal, expired key, or no password.
    Refused,
    /// Too many failed password attempts. The next is taken at `retry_at`, in seconds since the
    /// Unix epoch.
    Throttled { retry_at: u64 },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Refused => f.write_str("the credential was not accepted"),
            AuthError::Throttled { retry_at } => write!(
                f,
                "too many failed attempts; try again at {retry_at} seconds after the epoch"
            ),
        }
    }
}

impl std::error::Error for AuthError {}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use crate::{Catalogue, Options};

    /// A catalogue directory and a clock the test moves.
    pub struct Fixture {
        pub dir: tempfile::TempDir,
        now: Arc<AtomicU64>,
    }

    impl Fixture {
        pub fn new() -> Fixture {
            Fixture {
                dir: tempfile::tempdir().unwrap(),
                now: Arc::new(AtomicU64::new(1_000_000)),
            }
        }

        pub fn options(&self) -> Options {
            let now = Arc::clone(&self.now);
            Options {
                clock: Arc::new(move || now.load(Ordering::SeqCst)),
                ..Options::default()
            }
        }

        pub fn open(&self) -> Catalogue {
            Catalogue::open(self.dir.path(), self.options()).unwrap()
        }

        pub fn now(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }

        pub fn advance(&self, secs: u64) {
            self.now.fetch_add(secs, Ordering::SeqCst);
        }

        /// A second connection to the catalogue's file, as another process would have.
        pub fn raw(&self) -> rusqlite::Connection {
            rusqlite::Connection::open(self.dir.path().join(crate::store::FILE_NAME)).unwrap()
        }
    }
}
