//! The words an access label may be written as.

/// The label every principal holds. A `visibility` of `public` alone admits everyone and is stored
/// as `None`.
pub const PUBLIC: &str = "public";
/// The word a layer writes as its artifacts' default visibility to give them the layer's own
/// `visibility`. It is not a label, and it is refused wherever a label is expected.
pub const INHERITED: &str = "inherited";

/// Check a word written where one access label is expected. `public` is accepted as a label. An
/// empty word and [`INHERITED`] are refused. `key` names the setting in the refusal.
pub fn check_label(key: &str, label: &str) -> Result<(), String> {
    if label.trim().is_empty() {
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
    Ok(())
}
