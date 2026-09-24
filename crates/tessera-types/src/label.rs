//! The one rule for access labels, on the build and on a running service, applied before any
//! plugin sees a label. A label read from data is trimmed, and one left empty is no label. A
//! declared word is trimmed and stored trimmed, and one left empty is refused.

/// The label every principal holds. A `visibility` of `public` alone admits everyone and is stored
/// as `None`.
pub const PUBLIC: &str = "public";
/// The word a layer writes as its artifacts' default visibility to give them the layer's own
/// `visibility`. It is not a label, and it is refused wherever a label is expected.
pub const INHERITED: &str = "inherited";

/// A label value read from data, trimmed. `None` where nothing is left, which is no label.
pub fn label_value(value: &str) -> Option<&str> {
    let label = value.trim();
    (!label.is_empty()).then_some(label)
}

/// A row's label values, each trimmed, with the empty ones dropped. An empty result is no label.
pub fn label_values<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    values.into_iter().filter_map(label_value).collect()
}

/// A word declared where one access label is expected, trimmed: what is stored. An empty word and
/// [`INHERITED`] are refused; `public` is accepted. `key` names the setting in the refusal.
pub fn declared_label<'a>(key: &str, word: &'a str) -> Result<&'a str, String> {
    let label = word.trim();
    if label.is_empty() {
        return Err(format!(
            "`{key}` is empty; write an access label, or `public` for the label every principal \
             holds"
        ));
    }
    if label == INHERITED {
        return Err(format!(
            "`{key}` is `inherited`, which is not a label; write an access label or `public`"
        ));
    }
    Ok(label)
}

/// Whether a declared word is `public`, the label every principal holds.
pub fn is_public(word: &str) -> bool {
    word.trim() == PUBLIC
}

/// Whether a declared gate is `public` alone, which is stored as no gate.
pub fn is_public_gate(labels: &[String]) -> bool {
    matches!(labels, [only] if is_public(only))
}

/// Whether a declared word is `inherited`, where a layer's artifact default may take it.
pub fn is_inherited(word: &str) -> bool {
    word.trim() == INHERITED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_trimmed_and_an_empty_one_is_no_label() {
        assert_eq!(label_value(" red "), Some("red"));
        assert_eq!(label_value("red"), Some("red"));
        assert_eq!(label_value(""), None);
        assert_eq!(label_value(" \t "), None);
        assert_eq!(label_values([" red ", "", " ", "a, b"]), vec!["red", "a, b"]);
        assert!(label_values(["", "  "]).is_empty());
    }

    #[test]
    fn a_declared_word_is_stored_trimmed_and_refused_when_it_names_nothing() {
        assert_eq!(declared_label("k", " red "), Ok("red"));
        assert_eq!(declared_label("k", " public "), Ok("public"));
        assert!(declared_label("k", "").is_err());
        assert!(declared_label("k", "   ").is_err());
        assert!(declared_label("k", "inherited").is_err());
        assert!(declared_label("k", " inherited ").is_err());
        assert!(is_public(" public "));
        assert!(!is_public("publicly"));
        assert!(is_inherited(" inherited "));
    }
}
