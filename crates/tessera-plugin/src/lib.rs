//! The plugin surface (contracts §4, the plugin ABI).
//!
//! This crate is the *trait* — mirroring the wasm ABI's four entry points — plus one native
//! implementation, [`Passthrough`] (`builtin:passthrough`).
//!
//! **⊘ Specified, not implemented.** There is no wasmtime host: nothing here loads a guest
//! module, so the only plugin a deployment can run is the built-in one below. The trait exists
//! regardless, so that the build pipeline, the session plane and the conformance oracle all
//! speak to one surface when a host does arrive.
//!
//! Two obligations from the design carry into any implementation of [`Plugin`]:
//!
//! * **Determinism.** The same input bytes must always produce the same descriptors, in the
//!   same order, on every host. Term interning and therefore the whole entity-ID assignment
//!   (I9, permanent) depend on it.
//! * **Fail closed.** A credential that cannot be parsed yields an error, never an empty or
//!   partial term list that would be mistaken for "authorised for nothing in particular".
//!   (A *validly parsed* zero-term credential is a different thing: it legitimately mints a
//!   zero-visibility token, per contracts §4.3.)

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// An opaque authorisation descriptor: the plugin's own bytes, interned into a `TermId` by the
/// dictionary. Tessera never interprets a descriptor's contents.
pub type Descriptor = Vec<u8>;

/// The identity string hashed to produce `builtin:passthrough`'s plugin hashes.
const PASSTHROUGH_IDENTITY: &str = "builtin:passthrough:1";

/// What `terms_of_auth` returns: the credential's descriptors and its optional expiry.
///
/// `not_after` is a Unix timestamp in seconds; `None` means the plugin declares no expiry of
/// its own (the session plane still applies its configured token lifetime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthTerms {
    pub terms: Vec<Descriptor>,
    pub not_after: Option<i64>,
}

/// The plugin's sizing declarations. These are *declarations*, not limits: exceeding one
/// warns and is recorded, and never causes an item or a term to be silently excluded — a
/// dropped term is a disclosure risk (I2/I3), so the fail-open behaviour is forbidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredBounds {
    pub max_distinct_terms: u64,
    pub max_terms_per_item: u32,
    pub max_terms_per_token: u32,
}

/// A plugin failure. Callers must treat this as fail-closed (design §4, I1).
#[derive(Debug)]
pub enum PluginError {
    /// The input bytes were not the shape this plugin accepts.
    Malformed(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Malformed(detail) => write!(f, "malformed plugin input: {detail}"),
        }
    }
}

impl std::error::Error for PluginError {}

/// The plugin surface, mirroring the ABI's entry points (contracts §4).
pub trait Plugin: Send + Sync {
    /// Map an item's `access` bytes to its authorisation descriptors (the *data* side).
    ///
    /// Deterministic: identical bytes in, identical descriptors out, every time.
    fn terms_of_label(&self, access: &[u8]) -> Result<Vec<Descriptor>, PluginError>;

    /// Map a credential's `auth_data` bytes to the descriptors it authorises (the *auth* side).
    fn terms_of_auth(&self, auth_data: &[u8]) -> Result<AuthTerms, PluginError>;

    /// The plugin's sizing declarations.
    fn declared_bounds(&self) -> DeclaredBounds;

    /// Hex SHA-256 identifying the data-side implementation; recorded in `MANIFEST.json` so a
    /// bundle can never be served by a plugin that would label its items differently.
    fn data_plugin_hash(&self) -> String;

    /// Hex SHA-256 identifying the auth-side implementation.
    fn auth_plugin_hash(&self) -> String;
}

/// `builtin:passthrough`: the identity plugin, and the only one this build can run. The
/// conformance oracle implements the same mapping.
///
/// * `access` is a UTF-8 comma-separated descriptor list — split on `,`, trim, drop empties.
/// * `auth_data` is JSON `{"terms": ["<descriptor>", …]}`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Passthrough;

/// `auth_data`'s wire shape for [`Passthrough`].
#[derive(Deserialize)]
struct PassthroughAuthData {
    terms: Vec<String>,
}

impl Passthrough {
    pub fn new() -> Self {
        Passthrough
    }
}

impl Plugin for Passthrough {
    fn terms_of_label(&self, access: &[u8]) -> Result<Vec<Descriptor>, PluginError> {
        let text = std::str::from_utf8(access)
            .map_err(|e| PluginError::Malformed(format!("access is not valid UTF-8: {e}")))?;
        Ok(text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_vec())
            .collect())
    }

    fn terms_of_auth(&self, auth_data: &[u8]) -> Result<AuthTerms, PluginError> {
        let parsed: PassthroughAuthData = serde_json::from_slice(auth_data).map_err(|e| {
            PluginError::Malformed(format!("auth_data is not the expected JSON: {e}"))
        })?;
        Ok(AuthTerms {
            terms: parsed.terms.into_iter().map(String::into_bytes).collect(),
            not_after: None,
        })
    }

    fn declared_bounds(&self) -> DeclaredBounds {
        DeclaredBounds {
            max_distinct_terms: 200_000_000,
            max_terms_per_item: 4_096,
            max_terms_per_token: 100_000,
        }
    }

    fn data_plugin_hash(&self) -> String {
        passthrough_hash()
    }

    fn auth_plugin_hash(&self) -> String {
        passthrough_hash()
    }
}

fn passthrough_hash() -> String {
    hex_lower(&Sha256::digest(PASSTHROUGH_IDENTITY.as_bytes()))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_of_label_splits_trims_and_drops_empties() {
        let p = Passthrough::new();
        assert_eq!(
            p.terms_of_label(b" 1207 ,, 9,cs.LG,").unwrap(),
            vec![b"1207".to_vec(), b"9".to_vec(), b"cs.LG".to_vec()]
        );
    }

    #[test]
    fn empty_label_yields_no_terms() {
        let p = Passthrough::new();
        assert!(p.terms_of_label(b"").unwrap().is_empty());
        assert!(p.terms_of_label(b" , , ").unwrap().is_empty());
    }

    #[test]
    fn terms_of_label_rejects_non_utf8() {
        let p = Passthrough::new();
        assert!(p.terms_of_label(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn terms_of_auth_parses_json_terms() {
        let p = Passthrough::new();
        let got = p.terms_of_auth(br#"{"terms": ["1207", "9"]}"#).unwrap();
        assert_eq!(
            got,
            AuthTerms {
                terms: vec![b"1207".to_vec(), b"9".to_vec()],
                not_after: None,
            }
        );
    }

    #[test]
    fn zero_term_credential_is_accepted_not_an_error() {
        // A valid zero-term credential mints a zero-visibility token — deliberate
        // (contracts §4.3), and is not the fail-closed error path above.
        let p = Passthrough::new();
        assert!(p
            .terms_of_auth(br#"{"terms": []}"#)
            .unwrap()
            .terms
            .is_empty());
    }

    #[test]
    fn terms_of_auth_rejects_malformed_json_fail_closed() {
        let p = Passthrough::new();
        assert!(p.terms_of_auth(b"not json").is_err());
        assert!(p.terms_of_auth(br#"{"nope": 1}"#).is_err());
    }

    #[test]
    fn plugin_hashes_are_sha256_of_the_identity_string() {
        let p = Passthrough::new();
        // Both sides are the same implementation, so both hashes are the same value.
        assert_eq!(p.data_plugin_hash(), p.auth_plugin_hash());
        assert_eq!(p.data_plugin_hash().len(), 64);
        assert!(p
            .data_plugin_hash()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Pin the value: changing it silently would let a bundle be served by a differently
        // behaving plugin.
        assert_eq!(
            p.data_plugin_hash(),
            hex_lower(&Sha256::digest(b"builtin:passthrough:1"))
        );
    }

    #[test]
    fn declared_bounds_match_r6() {
        let b = Passthrough::new().declared_bounds();
        assert_eq!(b.max_distinct_terms, 200_000_000);
        assert_eq!(b.max_terms_per_item, 4_096);
        assert_eq!(b.max_terms_per_token, 100_000);
    }
}
