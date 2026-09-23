use std::path::PathBuf;

use tokio::sync::Semaphore;

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Toml(toml::de::Error),
    NoDeploymentConfig {
        from: PathBuf,
    },
    IdentityKeyInline,
    UnknownIdentityKey(String),
    IdentityNotATable,
    MissingDisclosureSection,
    MissingDisclosureKey(&'static str),
    MissingCredential(&'static str),
    CredentialFileUnreadable {
        which: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    BadAddr {
        key: &'static str,
        value: String,
    },
    UnsupportedPlugin(String),
    /// `tower_http`'s `AllowOrigin::list` panics on a wildcard.
    CorsWildcard {
        key: &'static str,
    },
    CompactionWindowNotATime(String),
    NotANumberOrOff {
        key: &'static str,
        value: String,
    },
    /// `Semaphore::new` panics past `Semaphore::MAX_PERMITS`.
    AdmissionTooLarge {
        key: &'static str,
    },
    /// A bound that must admit at least one of what it counts.
    Zero {
        key: &'static str,
    },
    /// A page is one frame, whose length is 32 bits.
    PageBytesTooLarge {
        value: usize,
    },
    /// No page could start inside the response's byte budget.
    ResponseBelowPage {
        response_bytes: usize,
        page_bytes: usize,
    },
    /// The stream deadline would cut a response before its own time budget ends it.
    ResponseTimeNotBelowDeadline {
        response_ms: u64,
        deadline_ms: u64,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(
                f,
                "cannot read the deployment file ({e}); name a readable tessera.toml"
            ),
            ConfigError::Toml(e) => write!(
                f,
                "tessera.toml does not parse; correct what this names: {e}"
            ),
            ConfigError::NoDeploymentConfig { from } => write!(
                f,
                "no tessera.toml found searching upward from {}; create one beside the corpus \
                 declaration, or name one with --deployment <path>, for example:\n\n\
                 \x20   [bundle]\n\
                 \x20   path  = \"bundles/corpus\"\n\
                 \x20   cache = \".tessera/cache\"\n\
                 \x20   wal   = \".tessera/wal.log\"\n\n\
                 \x20   [build]\n\
                 \x20   schema = \"schema.toml\"\n\n\
                 \x20   [identity]\n\
                 \x20   env = \"TESSERA_IDENTITY_KEY\"\n\n\
                 \x20   [plugin]\n\
                 \x20   module = \"builtin:passthrough\"\n\n\
                 \x20   [disclosure]\n\
                 \x20   token_max_lifetime = 3600\n\n\
                 \x20   [serve]\n\
                 \x20   viewer  = \"127.0.0.1:37585\"\n\
                 \x20   session = \"127.0.0.1:49303\"\n\
                 \x20   control = \"127.0.0.1:45721\"",
                from.display()
            ),
            ConfigError::IdentityKeyInline => write!(
                f,
                "[identity] carries `key`, but the identity key never appears in this file; write \
                 `env = \"TESSERA_IDENTITY_KEY\"` naming the variable that holds it, or pass the \
                 key with --identity-file"
            ),
            ConfigError::UnknownIdentityKey(key) => write!(
                f,
                "[identity] has no key `{key}`; it takes `env`, the name of the variable that \
                 holds the identity key"
            ),
            ConfigError::IdentityNotATable => write!(
                f,
                "`identity` is not a table; write `[identity]` with `env = \"TESSERA_IDENTITY_KEY\"` \
                 under it"
            ),
            ConfigError::MissingDisclosureSection => write!(
                f,
                "tessera.toml has no [disclosure] section; add `[disclosure]` with \
                 `token_max_lifetime = 3600` (seconds) under it"
            ),
            ConfigError::MissingDisclosureKey(key) => write!(
                f,
                "[disclosure] has no `{key}`; write `{key} = 3600` (seconds) under it"
            ),
            ConfigError::MissingCredential(which) => write!(
                f,
                "there is no {which} credential; set `{which}_credential_file` or \
                 `{which}_credential_env` under [serve] and put the secret in that file or variable"
            ),
            ConfigError::CredentialFileUnreadable {
                which,
                path,
                source,
            } => write!(
                f,
                "cannot read the {which} credential file {} ({source}); name a readable file, \
                 relative to tessera.toml's directory or absolute",
                path.display()
            ),
            ConfigError::BadAddr { key, value } => write!(
                f,
                "serve.{key} = \"{value}\" is not a listen address; write an address and port such \
                 as \"127.0.0.1:8080\" (the control plane also takes \"unix:<path>\")"
            ),
            ConfigError::UnsupportedPlugin(module) => write!(
                f,
                "plugin.module = \"{module}\" is not available in this build; write \
                 `module = \"builtin:passthrough\"`"
            ),
            ConfigError::CorsWildcard { key } => write!(
                f,
                "serve.{key} contains \"*\", which a CORS origin list cannot hold; list each \
                 origin, such as \"https://app.example\", or remove the key"
            ),
            ConfigError::CompactionWindowNotATime(value) => write!(
                f,
                "ingest.compaction_window_start = \"{value}\" is not a time of day; write UTC \
                 \"HH:MM\" (24-hour) or \"off\""
            ),
            ConfigError::NotANumberOrOff { key, value } => write!(
                f,
                "{key} = \"{value}\" is neither a number nor \"off\"; write a number or \"off\""
            ),
            ConfigError::AdmissionTooLarge { key } => write!(
                f,
                "{key} is above {}, the most permits an admission gate can hold; write a smaller \
                 number",
                Semaphore::MAX_PERMITS
            ),
            ConfigError::Zero { key } => {
                write!(f, "{key} is 0; write a number of at least 1")
            }
            ConfigError::PageBytesTooLarge { value } => write!(
                f,
                "serve.max_page_bytes = {value} is above {}, the largest page one frame can \
                 carry; write a smaller number",
                crate::defaults::MAX_PAGE_BYTES_CEILING
            ),
            ConfigError::ResponseBelowPage {
                response_bytes,
                page_bytes,
            } => write!(
                f,
                "serve.bulk_response_bytes = {response_bytes} is below serve.max_page_bytes = \
                 {page_bytes}, so no page could start; write at least {page_bytes}"
            ),
            ConfigError::ResponseTimeNotBelowDeadline {
                response_ms,
                deadline_ms,
            } => write!(
                f,
                "serve.bulk_response_ms = {response_ms} is not below serve.stream_deadline_ms = \
                 {deadline_ms}, so the deadline would cut a bulk read before its own budget \
                 ends it; write less than {deadline_ms}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Toml(e)
    }
}

