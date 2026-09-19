//! The plugin surface: the trait that maps a deployment's labels and credentials to authorisation
//! descriptors, and one native implementation, [`Passthrough`] (`builtin:passthrough`).
//!
//! Not built yet: a host that loads a guest module. `builtin:passthrough` is the only plugin a
//! deployment can run. The build, the session path and the conformance oracle already call the
//! trait, so a host adds an implementation and changes no caller.
//!
//! Every implementation of [`Plugin`] is deterministic: the same input bytes give the same
//! descriptors in the same order on every host, because term interning and entity-id assignment
//! follow from them. It also fails closed: a credential that cannot be parsed is an error and not
//! a short term list. A credential that parses to zero terms is valid and sees nothing.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// An authorisation descriptor: the plugin's own bytes, which the dictionary interns into a
/// `TermId`. Tessera does not interpret them.
pub type Descriptor = Vec<u8>;

/// The string hashed to give both of `builtin:passthrough`'s plugin hashes. The data and auth
/// sides are one implementation, so a change to either rule moves both: fragment caches recompute
/// and tokens are minted again, where a pinned auth hash would let a cached fragment outlive the
/// labelling that produced it.
const PASSTHROUGH_IDENTITY: &str = "builtin:passthrough:2";

/// What `terms_of_auth` returns: the credential's descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthTerms {
    pub terms: Vec<Descriptor>,
}

/// The sizes a plugin declares. Exceeding one is counted and reported. It does not exclude an
/// item or drop a term, because a dropped term widens what the item's viewers see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredBounds {
    pub max_distinct_terms: u64,
    pub max_terms_per_item: u32,
}

/// A plugin failure. The caller refuses the request.
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

/// The plugin surface. No method has a default: a default would be a labelling or presentation
/// rule the plugin's author did not write.
pub trait Plugin: Send + Sync {
    /// The descriptors a list of labels names, one label per element (the data side).
    ///
    /// Every reader of labels calls this: a points file's term column at the build, the `access`
    /// column at `/control/ingest`, and a view's `visibility` when it is declared, changed or
    /// authorised against. A label is one element, so its bytes are not parsed or split.
    fn terms_of_labels(&self, labels: &[Descriptor]) -> Result<Vec<Descriptor>, PluginError>;

    /// The descriptors a credential's `auth_data` authorises (the auth side).
    fn terms_of_auth(&self, auth_data: &[u8]) -> Result<AuthTerms, PluginError>;

    /// The strings a viewer is shown for `descriptors`, one per descriptor in the order given.
    ///
    /// The caller is the item card's `labels` array. It passes the item's terms intersected with
    /// the session's satisfied set, so this method decides how a label is spelled and cannot widen
    /// which labels are shown. The caller refuses an answer of a different length.
    fn present_terms(&self, descriptors: &[Descriptor]) -> Result<Vec<String>, PluginError>;

    fn declared_bounds(&self) -> DeclaredBounds;

    /// Lowercase hex SHA-256 of the data-side implementation. `MANIFEST.json` records it, and a
    /// bundle is not served by a plugin that would label its items differently.
    fn data_plugin_hash(&self) -> String;

    /// Lowercase hex SHA-256 of the auth-side implementation. It keys the fragment cache.
    fn auth_plugin_hash(&self) -> String;
}

/// `builtin:passthrough`: a label is its own descriptor, and `auth_data` is JSON
/// `{"terms": ["<descriptor>", …]}`. The conformance oracle implements the same mapping.
#[derive(Debug, Clone, Copy, Default)]
pub struct Passthrough;

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
    /// Each label becomes one descriptor, verbatim and in order: no splitting, trimming,
    /// deduplication or dropping. The build's dictionary pass pairs source terms with descriptors
    /// by position, so a dropped or reordered element would post an item under a term it was not
    /// labelled with.
    ///
    /// An empty label is refused: it grants nothing, and dropping it would break that pairing. An
    /// empty list is accepted, because an item may carry no terms.
    fn terms_of_labels(&self, labels: &[Descriptor]) -> Result<Vec<Descriptor>, PluginError> {
        if let Some(i) = labels.iter().position(|label| label.is_empty()) {
            return Err(PluginError::Malformed(format!(
                "label {i} of {} is empty; give every label at least one byte",
                labels.len()
            )));
        }
        Ok(labels.to_vec())
    }

    /// A passthrough descriptor is the label string, so it is shown as itself. A descriptor that
    /// is not UTF-8 is refused: a lossy conversion would show a label no principal holds.
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
        })
    }

    fn declared_bounds(&self) -> DeclaredBounds {
        DeclaredBounds {
            max_distinct_terms: 200_000_000,
            max_terms_per_item: 4_096,
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
    format!("{:x}", Sha256::digest(PASSTHROUGH_IDENTITY.as_bytes()))
}

/// The label every principal holds. A gate of `public` alone is no gate.
pub const PUBLIC: &str = "public";
/// Means "take the container's gate" where a layer declares visibility; it is not a label.
pub const INHERITED: &str = "inherited";

/// Check the labels a view or group declares as its gate, and answer the gate as stored: `None`
/// for a public view. A principal passes a gate by holding any one of its terms, so a gate that
/// names no terms would shut everyone out and is refused.
pub fn check_gate(
    plugin: &dyn Plugin,
    declared: Option<&[String]>,
) -> Result<Option<Vec<String>>, String> {
    let Some(labels) = declared else {
        return Ok(None);
    };
    if labels == [PUBLIC] {
        return Ok(None);
    }
    if labels.is_empty() {
        return Err("`visibility = []` names no labels; write `public` or list them".to_string());
    }
    for label in labels {
        if label.trim().is_empty() {
            return Err("`visibility` has an empty label".to_string());
        }
        if label == PUBLIC {
            return Err("`visibility` lists `public` beside other labels; write `public` alone \
                        or leave it out"
                .to_string());
        }
        if label == INHERITED {
            return Err("`inherited` is not a label a view's `visibility` can take".to_string());
        }
    }
    let descriptors: Vec<Descriptor> = labels.iter().map(|l| l.as_bytes().to_vec()).collect();
    let terms = plugin
        .terms_of_labels(&descriptors)
        .map_err(|e| format!("`visibility = {labels:?}`: the plugin cannot read the labels ({e})"))?;
    if terms.is_empty() {
        return Err(format!("`visibility = {labels:?}` names no terms"));
    }
    Ok(Some(labels.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn terms_of_labels_is_the_identity_one_descriptor_per_label() {
        let p = Passthrough::new();
        // No trim, no split on the comma, no dedup.
        let given = vec![
            b" spaced ".to_vec(),
            b"cs,LG".to_vec(),
            b"cs.LG".to_vec(),
            b"cs.LG".to_vec(),
        ];
        assert_eq!(p.terms_of_labels(&given).unwrap(), given);
        assert!(p.terms_of_labels(&[]).unwrap().is_empty());
        assert!(p.terms_of_labels(&[b"cs.LG".to_vec(), Vec::new()]).is_err());
    }

    #[test]
    fn present_terms_serves_the_callers_own_strings_verbatim() {
        let p = Passthrough::new();
        let descriptors = vec![b"cs.LG".to_vec(), b" spaced ".to_vec(), b"1207".to_vec()];
        assert_eq!(
            p.present_terms(&descriptors).unwrap(),
            vec!["cs.LG", " spaced ", "1207"]
        );
        assert!(p.present_terms(&[]).unwrap().is_empty());
        assert!(p.present_terms(&[vec![0xff, 0xfe]]).is_err());
    }

    #[test]
    fn terms_of_auth_reads_json_terms_and_refuses_anything_else() {
        let p = Passthrough::new();
        assert_eq!(
            p.terms_of_auth(br#"{"terms": ["1207", "9"]}"#).unwrap(),
            AuthTerms {
                terms: vec![b"1207".to_vec(), b"9".to_vec()],
            }
        );
        // A credential of no terms is valid and sees nothing; it is not the refusal below.
        assert!(p.terms_of_auth(br#"{"terms": []}"#).unwrap().terms.is_empty());
        assert!(p.terms_of_auth(b"not json").is_err());
        assert!(p.terms_of_auth(br#"{"nope": 1}"#).is_err());
    }

    /// The engine decodes the auth hash to 32 bytes and compares the data hash with the manifest's.
    #[test]
    fn plugin_hashes_are_lowercase_hex_sha256() {
        let p = Passthrough::new();
        assert_eq!(p.data_plugin_hash(), p.auth_plugin_hash());
        assert_eq!(p.data_plugin_hash().len(), 64);
        assert!(p
            .data_plugin_hash()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn a_gate_is_stored_as_its_labels_and_public_as_none() {
        let p = Passthrough::new();
        assert_eq!(check_gate(&p, None), Ok(None));
        assert_eq!(check_gate(&p, Some(&labels(&["public"]))), Ok(None));
        assert_eq!(
            check_gate(&p, Some(&labels(&["finance", "legal"]))),
            Ok(Some(labels(&["finance", "legal"])))
        );
        for refused in [
            labels(&[]),
            labels(&[""]),
            labels(&["  "]),
            labels(&["finance", "public"]),
            labels(&["inherited"]),
        ] {
            assert!(check_gate(&p, Some(&refused)).is_err(), "{refused:?}");
        }
    }

    /// A plugin that maps every label away leaves a gate nobody can pass.
    #[test]
    fn a_gate_whose_labels_name_no_terms_is_refused() {
        struct NoTerms;
        impl Plugin for NoTerms {
            fn terms_of_labels(&self, _: &[Descriptor]) -> Result<Vec<Descriptor>, PluginError> {
                Ok(Vec::new())
            }
            fn terms_of_auth(&self, auth_data: &[u8]) -> Result<AuthTerms, PluginError> {
                Passthrough.terms_of_auth(auth_data)
            }
            fn present_terms(&self, d: &[Descriptor]) -> Result<Vec<String>, PluginError> {
                Passthrough.present_terms(d)
            }
            fn declared_bounds(&self) -> DeclaredBounds {
                Passthrough.declared_bounds()
            }
            fn data_plugin_hash(&self) -> String {
                Passthrough.data_plugin_hash()
            }
            fn auth_plugin_hash(&self) -> String {
                Passthrough.auth_plugin_hash()
            }
        }
        assert!(check_gate(&NoTerms, Some(&labels(&["finance"]))).is_err());
    }
}
