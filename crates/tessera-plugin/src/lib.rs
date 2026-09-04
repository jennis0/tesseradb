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
///
/// **One string, both hashes.** The data and auth sides of `builtin:passthrough` are the same
/// implementation, so a change to either moves both — `:2` names the revision that added
/// [`Plugin::terms_of_labels`], which is a data-side change the auth side never saw. Moving the
/// auth hash too is the conservative direction: fragment caches recompute and tokens re-mint,
/// where the alternative — pinning the auth hash while the data rule moves — would let a cached
/// fragment outlive the labelling that produced it. The hash is recorded in `MANIFEST.json`
/// precisely so a bundle cannot be served by a plugin that would label its items differently.
const PASSTHROUGH_IDENTITY: &str = "builtin:passthrough:2";

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

    /// Map an item's terms, already separated by the caller, to its authorisation descriptors
    /// (the *data* side, list form).
    ///
    /// This is the entry point the build uses: the caller's source column already holds one
    /// string per term, so there is nothing to parse and nothing a separator could split wrongly.
    /// [`Plugin::terms_of_label`] remains the wire path — an ingest request carries one opaque
    /// `access` byte string, which only the plugin can decompose.
    ///
    /// Required, deliberately without a default. A default would be a labelling rule a plugin
    /// author never wrote, silently inherited; a plugin that folds or rewrites terms must say so
    /// here in its own words or not compile.
    ///
    /// Deterministic, on the same terms as [`Plugin::terms_of_label`].
    fn terms_of_labels(&self, labels: &[Descriptor]) -> Result<Vec<Descriptor>, PluginError>;

    /// Map a credential's `auth_data` bytes to the descriptors it authorises (the *auth* side).
    fn terms_of_auth(&self, auth_data: &[u8]) -> Result<AuthTerms, PluginError>;

    /// Map descriptors to the strings a viewer is shown for them — the *presentation* side
    /// (design §6.1, decision 0114).
    ///
    /// The one caller is the item drill-down's `labels` array, and what it hands in is already
    /// the intersection of the item's own terms with the asking session's **satisfied** set: a
    /// descriptor reaches this method only if the credential that opened the session presented it.
    /// So a plugin cannot widen a disclosure here however it implements this — the set is decided
    /// before the call, and this decides only how each member is spelled.
    ///
    /// Positional: one string per descriptor, in the order given. A plugin that returned a
    /// different count would leave the caller unable to say which label it had failed to present,
    /// so the count is checked and a mismatch is fail-closed.
    ///
    /// Required, deliberately without a default, on [`Plugin::terms_of_labels`]' argument: a
    /// default would be a presentation rule a plugin author never wrote. For a plugin whose
    /// descriptors *are* display strings, the identity is the correct implementation and saying so
    /// takes one line.
    fn present_terms(&self, descriptors: &[Descriptor]) -> Result<Vec<String>, PluginError>;

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
/// * `access` (the wire path) is a UTF-8 comma-separated descriptor list — split on `,`, trim,
///   drop empties.
/// * a term *list* (the build path) is taken verbatim, one descriptor per element.
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

    /// The identity — and identity is a *rule*, not the absence of one.
    ///
    /// Each label's bytes become exactly one descriptor, verbatim and in the order given: no
    /// splitting, no trimming, no deduplication, no dropping. Every one of those is load-bearing
    /// downstream. The build's dictionary pass derives a term's descriptor from its term id
    /// alone, so it needs one descriptor per source term and the two sequences aligned
    /// positionally; a rule that dropped or reordered elements would leave every posting naming
    /// a different term than the item was labelled with, which is a disclosure rather than a
    /// cosmetic difference.
    ///
    /// An empty element is refused. An empty descriptor is not a grant, and silently dropping it
    /// would shorten the descriptor list below the item's term count — the fail-open direction,
    /// and the one place identity could quietly stop being one-to-one. An empty *list* is fine:
    /// an item may legitimately carry zero terms.
    fn terms_of_labels(&self, labels: &[Descriptor]) -> Result<Vec<Descriptor>, PluginError> {
        for (i, label) in labels.iter().enumerate() {
            if label.is_empty() {
                return Err(PluginError::Malformed(format!(
                    "term {i} of {} is empty; an empty descriptor is not a grant and dropping it \
                     would leave the item with fewer terms than it was given",
                    labels.len()
                )));
            }
        }
        Ok(labels.to_vec())
    }

    /// **The identity, and it is this plugin's real answer rather than a fallback.** A
    /// passthrough descriptor *is* the caller's own label string — `terms_of_label` splits the
    /// wire's `access` bytes into them and `terms_of_labels` takes a build's term column verbatim
    /// — so the string a viewer should be shown for a descriptor is the descriptor. There is no
    /// mapping to look up and none to omit.
    ///
    /// Non-UTF-8 is refused rather than lossily converted. `terms_of_label` already requires the
    /// wire's bytes to be UTF-8, so a descriptor that is not is one a build's term column
    /// supplied, and replacing its bytes with substitution characters would show a viewer a label
    /// no principal holds.
    fn present_terms(&self, descriptors: &[Descriptor]) -> Result<Vec<String>, PluginError> {
        descriptors
            .iter()
            .map(|d| {
                String::from_utf8(d.clone()).map_err(|e| {
                    PluginError::Malformed(format!(
                        "a descriptor is not valid UTF-8 and this plugin presents descriptors \
                         verbatim: {e}"
                    ))
                })
            })
            .collect()
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
    fn terms_of_labels_is_the_identity_one_descriptor_per_label() {
        let p = Passthrough::new();
        let labels = vec![
            b" spaced ".to_vec(),
            b"cs,LG".to_vec(),
            b"cs.LG".to_vec(),
            b"cs.LG".to_vec(),
        ];
        // Verbatim, in order, one each: no trim, no split on the comma, no dedup.
        assert_eq!(p.terms_of_labels(&labels).unwrap(), labels);
    }

    #[test]
    fn terms_of_labels_accepts_the_empty_list_and_refuses_an_empty_term() {
        let p = Passthrough::new();
        assert!(p.terms_of_labels(&[]).unwrap().is_empty());
        assert!(p.terms_of_labels(&[b"cs.LG".to_vec(), Vec::new()]).is_err());
    }

    /// The presentation seam is the identity for this plugin: the caller's own strings, verbatim
    /// and in order, because a passthrough descriptor is the label string.
    #[test]
    fn present_terms_serves_the_callers_own_strings_verbatim() {
        let p = Passthrough::new();
        let descriptors = vec![b"cs.LG".to_vec(), b" spaced ".to_vec(), b"1207".to_vec()];
        assert_eq!(
            p.present_terms(&descriptors).unwrap(),
            vec!["cs.LG".to_string(), " spaced ".to_string(), "1207".to_string()]
        );
        assert!(p.present_terms(&[]).unwrap().is_empty());
        // Fail-closed rather than lossy: a substitution character is a label nobody holds.
        assert!(p.present_terms(&[vec![0xff, 0xfe]]).is_err());
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
            hex_lower(&Sha256::digest(b"builtin:passthrough:2"))
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
