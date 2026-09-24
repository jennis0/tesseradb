//! The plugin interface. A plugin maps a deployment's access labels and credentials to
//! authorisation descriptors, which Tessera interns as terms. [`Passthrough`]
//! (`builtin:passthrough`) is the one implementation.
//!
//! Not built yet: a host that loads a plugin from a guest module. Until it exists,
//! `builtin:passthrough` is the only plugin a deployment can run. The build, the session path and
//! the conformance oracle call the [`Plugin`] trait, so a loaded plugin will need no change to them.
//!
//! Two rules bind every implementation.
//!
//! * It is deterministic. The same input bytes give the same descriptors, in the same order, on
//!   every host. Term ids and entity ids are assigned from the descriptors.
//! * It fails closed. A credential it cannot parse is an error. A credential that parses to no
//!   terms is valid, and its session sees nothing.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// An authorisation descriptor. The bytes belong to the plugin: Tessera interns them as a `TermId`
/// and does not read them.
pub type Descriptor = Vec<u8>;

/// The string whose SHA-256 is both of `builtin:passthrough`'s hashes. Change it when either
/// mapping changes. The two sides share one hash so that a change to the labelling also discards
/// the cached fragments built under the old labelling, which are keyed by the auth hash.
const PASSTHROUGH_IDENTITY: &str = "builtin:passthrough:3";

/// The sizes a plugin declares. An item that exceeds one is counted, reported and stored with all
/// of its terms, because dropping a term changes who can see the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredBounds {
    pub max_distinct_terms: u64,
    pub max_terms_per_item: u32,
}

/// A plugin could not read its input. The caller refuses the request.
#[derive(Debug)]
pub enum PluginError {
    /// The bytes are not in the form this plugin reads.
    Malformed(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Malformed(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for PluginError {}

/// The plugin interface. No method has a default body, so every labelling and presentation rule
/// is one the plugin's author wrote.
pub trait Plugin: Send + Sync {
    /// The descriptors that a list of labels names. This is the data side.
    ///
    /// Every reader of labels calls it: the build for a points file's term column,
    /// `/control/ingest` for the `access` column, and a view's `visibility` when the view is
    /// declared, changed or authorised against. Each element is one whole label, so an
    /// implementation does not split or parse it.
    fn terms_of_labels(&self, labels: &[Descriptor]) -> Result<Vec<Descriptor>, PluginError>;

    /// The descriptors that a credential's `auth_data` authorises. This is the auth side.
    fn terms_of_auth(&self, auth_data: &[u8]) -> Result<Vec<Descriptor>, PluginError>;

    /// The text a viewer is shown for each descriptor, in the order given.
    ///
    /// The one caller fills the item card's `labels` array. It passes only descriptors that are on
    /// the item and in the session's satisfied set, so an implementation chooses how a label is
    /// spelled and cannot add to the labels shown. The caller refuses an answer whose length
    /// differs from `descriptors`.
    fn present_terms(&self, descriptors: &[Descriptor]) -> Result<Vec<String>, PluginError>;

    fn declared_bounds(&self) -> DeclaredBounds;

    /// Lowercase hex SHA-256 identifying the data side. `MANIFEST.json` records it, and an engine
    /// refuses a bundle labelled by a plugin with a different hash.
    fn data_plugin_hash(&self) -> String;

    /// Lowercase hex SHA-256 identifying the auth side. The fragment cache is keyed by it.
    fn auth_plugin_hash(&self) -> String;
}

/// `builtin:passthrough`. A label is its own descriptor, and `auth_data` is the JSON
/// `{"terms": ["<label>", …]}`, each term trimmed as a stored label is, so a credential matches
/// the label it names however either was padded. The conformance oracle implements the same
/// mapping.
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
    /// Each label is returned as one descriptor, unchanged and in the order given. Labels are not
    /// split, trimmed, deduplicated or dropped: the build's dictionary pass pairs source terms
    /// with descriptors by position, so a missing or reordered descriptor would index an item
    /// under a term it was not labelled with.
    ///
    /// An empty label is refused, because it names no term and cannot be dropped. An empty list is
    /// accepted: an item may carry no labels.
    fn terms_of_labels(&self, labels: &[Descriptor]) -> Result<Vec<Descriptor>, PluginError> {
        if let Some(i) = labels.iter().position(|label| label.is_empty()) {
            return Err(PluginError::Malformed(format!(
                "label {} of {} is empty; remove it or give it a value",
                i + 1,
                labels.len()
            )));
        }
        Ok(labels.to_vec())
    }

    /// A passthrough descriptor is the label's own text and is shown unchanged. A descriptor that
    /// is not UTF-8 is refused: converting it with replacement characters would show a label that
    /// no item carries.
    fn present_terms(&self, descriptors: &[Descriptor]) -> Result<Vec<String>, PluginError> {
        descriptors
            .iter()
            .enumerate()
            .map(|(i, d)| {
                String::from_utf8(d.clone()).map_err(|_| {
                    PluginError::Malformed(format!(
                        "label {} of {} is not UTF-8, so `builtin:passthrough` cannot show it as \
                         text; label items with UTF-8 strings",
                        i + 1,
                        descriptors.len()
                    ))
                })
            })
            .collect()
    }

    fn terms_of_auth(&self, auth_data: &[u8]) -> Result<Vec<Descriptor>, PluginError> {
        let parsed: PassthroughAuthData = serde_json::from_slice(auth_data).map_err(|e| {
            PluginError::Malformed(format!(
                "`auth_data` must be JSON of the form {{\"terms\": [\"<label>\", ...]}} ({e})"
            ))
        })?;
        Ok(label_values(parsed.terms.iter().map(String::as_str))
            .into_iter()
            .map(|term| term.as_bytes().to_vec())
            .collect())
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

use tessera_types::label::{declared_label, is_public, is_public_gate, label_values, PUBLIC};

/// An artifact's own access labels as the descriptors it is stored and compared by, on the build
/// and a running service alike. Each label is trimmed and an empty one dropped; none left is no
/// label. Labels the plugin maps to no term are refused: stored as no label, the artifact would
/// take its layer's default, which can admit everyone the layer does.
pub fn artifact_access(plugin: &dyn Plugin, labels: &[String]) -> Result<Vec<Descriptor>, String> {
    let labels: Vec<Descriptor> = label_values(labels.iter().map(String::as_str))
        .into_iter()
        .map(|label| label.as_bytes().to_vec())
        .collect();
    if labels.is_empty() {
        return Ok(Vec::new());
    }
    let descriptors = plugin
        .terms_of_labels(&labels)
        .map_err(|e| format!("`access`: the plugin refused the labels: {e}"))?;
    if descriptors.is_empty() {
        return Err(
            "`access` names labels the plugin maps to no term, so no viewer could hold them; \
             write labels the plugin maps to a term, or send no `access` for no label"
                .to_string(),
        );
    }
    Ok(descriptors)
}

/// Check `point_visibility.default`, the label given to a point that carries none of its own, and
/// return it trimmed, as it is stored. `inherited` is refused because a point has no layer to take
/// a `visibility` from. A label the plugin maps to no term is refused because no viewer would see
/// the points given it.
pub fn check_point_default(plugin: &dyn Plugin, default: &str) -> Result<String, String> {
    const KEY: &str = "point_visibility.default";
    let default = declared_label(KEY, default)?;
    if is_public(default) {
        return Ok(PUBLIC.to_string());
    }
    let terms = plugin
        .terms_of_labels(&[default.as_bytes().to_vec()])
        .map_err(|e| format!("the plugin refused `{KEY} = {default:?}`: {e}"))?;
    if terms.is_empty() {
        return Err(format!(
            "the plugin maps `{KEY} = {default:?}` to no term, so no viewer would see a point \
             given it; write `public` or a label the plugin maps to a term"
        ));
    }
    Ok(default.to_string())
}

/// Check the labels a view or view group declares as its `visibility`, and return them as they
/// are stored: each trimmed, and `None` for `public` or where none is declared. A principal
/// reaches the view by holding any one of the terms, so labels that map to no term admit nobody
/// and are refused.
pub fn check_visibility(
    plugin: &dyn Plugin,
    declared: Option<&[String]>,
) -> Result<Option<Vec<String>>, String> {
    let Some(declared) = declared else {
        return Ok(None);
    };
    if declared.is_empty() {
        return Err(
            "`visibility = []` lists no labels; write `public`, or the labels a viewer needs \
             one of"
                .to_string(),
        );
    }
    let labels = declared
        .iter()
        .map(|label| declared_label("visibility", label).map(str::to_string))
        .collect::<Result<Vec<String>, String>>()?;
    if is_public_gate(&labels) {
        return Ok(None);
    }
    if labels.iter().any(|label| is_public(label)) {
        return Err(
            "`visibility` lists `public` with other labels, and every principal holds \
             `public`; write it alone or remove it"
                .to_string(),
        );
    }
    let descriptors: Vec<Descriptor> = labels.iter().map(|l| l.as_bytes().to_vec()).collect();
    let terms = plugin
        .terms_of_labels(&descriptors)
        .map_err(|e| format!("the plugin refused `visibility = {labels:?}`: {e}"))?;
    if terms.is_empty() {
        return Err(format!(
            "the plugin maps `visibility = {labels:?}` to no term, so no viewer could reach \
             the view; list a label the plugin maps to a term"
        ));
    }
    Ok(Some(labels))
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
        // Leading spaces, a comma and a repeated label all come back as given.
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
            vec![b"1207".to_vec(), b"9".to_vec()]
        );
        // Each term is trimmed as a stored label is, and an empty one is dropped.
        assert_eq!(
            p.terms_of_auth(br#"{"terms": [" red ", "", "  "]}"#).unwrap(),
            vec![b"red".to_vec()]
        );
        // A credential of no terms is valid. Only input that does not parse is refused.
        assert!(p.terms_of_auth(br#"{"terms": []}"#).unwrap().is_empty());
        assert!(p.terms_of_auth(b"not json").is_err());
        assert!(p.terms_of_auth(br#"{"nope": 1}"#).is_err());
    }

    /// The engine decodes the auth hash to 32 bytes, and compares the data hash with the hex
    /// string in the manifest.
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
    fn visibility_is_stored_as_its_labels_and_public_as_none() {
        let p = Passthrough::new();
        assert_eq!(check_visibility(&p, None), Ok(None));
        assert_eq!(check_visibility(&p, Some(&labels(&["public"]))), Ok(None));
        assert_eq!(check_visibility(&p, Some(&labels(&[" public "]))), Ok(None));
        assert_eq!(
            check_visibility(&p, Some(&labels(&["finance", "legal"]))),
            Ok(Some(labels(&["finance", "legal"])))
        );
        assert_eq!(
            check_visibility(&p, Some(&labels(&[" finance ", "legal "]))),
            Ok(Some(labels(&["finance", "legal"])))
        );
        for refused in [
            labels(&[]),
            labels(&[""]),
            labels(&["  "]),
            labels(&["finance", "public"]),
            labels(&["finance", " public"]),
            labels(&["inherited"]),
            labels(&[" inherited "]),
        ] {
            assert!(check_visibility(&p, Some(&refused)).is_err(), "{refused:?}");
        }
    }

    /// With a plugin that maps every label to no term, a view's labels admit nobody and a default
    /// label hides every point given it. Both are refused.
    #[test]
    fn labels_that_name_no_terms_are_refused() {
        struct NoTerms;
        impl Plugin for NoTerms {
            fn terms_of_labels(&self, _: &[Descriptor]) -> Result<Vec<Descriptor>, PluginError> {
                Ok(Vec::new())
            }
            fn terms_of_auth(&self, auth_data: &[u8]) -> Result<Vec<Descriptor>, PluginError> {
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
        assert!(check_visibility(&NoTerms, Some(&labels(&["finance"]))).is_err());
        assert!(artifact_access(&NoTerms, &labels(&["finance"])).is_err());
        assert_eq!(artifact_access(&NoTerms, &[]), Ok(Vec::new()));
        assert_eq!(artifact_access(&NoTerms, &labels(&["", " "])), Ok(Vec::new()));
        assert_eq!(
            artifact_access(&Passthrough, &labels(&[" finance ", ""])),
            Ok(vec![b"finance".to_vec()])
        );
        assert!(check_point_default(&NoTerms, "finance").is_err());
        // `public` is accepted without asking the plugin.
        assert!(check_point_default(&NoTerms, "public").is_ok());
    }

    #[test]
    fn a_point_default_is_a_label_and_not_inherited() {
        let p = Passthrough::new();
        let accepted = [("public", "public"), (" public ", "public"), (" finance ", "finance")];
        for (declared, stored) in accepted {
            assert_eq!(check_point_default(&p, declared).as_deref(), Ok(stored));
        }
        for refused in ["", "  ", "inherited", " inherited "] {
            assert!(check_point_default(&p, refused).is_err(), "{refused:?}");
        }
    }
}
