//! The rules for what a term and a name may be. Every write that stores one calls these, and
//! a stored value is the trimmed one.

use crate::Error;

/// The term every session holds. It is added where a credential is evaluated, and no grant or
/// claim rule may name it in any case.
pub const PUBLIC: &str = "public";

/// Trims a term. An empty term, a term holding a control character, and a term equal to
/// `public` ignoring ASCII case are refused.
pub fn term(raw: &str) -> Result<String, Error> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(Error::Invalid(
            "a term is empty after trimming; write at least one visible character".into(),
        ));
    }
    if t.eq_ignore_ascii_case(PUBLIC) {
        return Err(Error::Invalid(format!(
            "`{t}` is the reserved term `public`, which is compared ignoring case and which \
             every session holds; grant a different term"
        )));
    }
    no_control("term", t)?;
    Ok(t.to_owned())
}

/// Trims the name of a principal, a group or a provider. An empty name, a name holding a
/// control character, and a name holding a zero-width or bidirectional formatting character
/// (U+200B to U+200F, U+202A to U+202E, U+2066 to U+2069, U+FEFF) are refused.
pub fn name(what: &str, raw: &str) -> Result<String, Error> {
    let n = raw.trim();
    if n.is_empty() {
        return Err(Error::Invalid(format!(
            "a {what} name is empty after trimming; write at least one visible character"
        )));
    }
    no_control(&format!("{what} name"), n)?;
    if let Some(c) = n.chars().find(|&c| is_display_format(c)) {
        return Err(Error::Invalid(format!(
            "the {what} name {n:?} holds U+{:04X}, a character that is invisible or changes the \
             direction of the text around it; remove it",
            c as u32
        )));
    }
    Ok(n.to_owned())
}

fn is_display_format(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

fn no_control(what: &str, value: &str) -> Result<(), Error> {
    match value.chars().find(|c| c.is_control()) {
        Some(c) => Err(Error::Invalid(format!(
            "the {what} {value:?} holds the control character {c:?}; remove it"
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::Fixture;
    use crate::{Error, Grantee, PrincipalKind};

    #[test]
    fn a_term_is_stored_trimmed_and_an_empty_or_reserved_term_is_refused() {
        let fx = Fixture::new();
        let cat = fx.open();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        cat.create_group("eu").unwrap();
        for to in [Grantee::Principal("ada"), Grantee::Group("eu")] {
            cat.grant_term(to, "  secret\t").unwrap();
            for bad in [
                "", "   ", "public", " public ", "Public", "PUBLIC", "a\u{7}b", "x\ny",
            ] {
                assert!(
                    matches!(cat.grant_term(to, bad), Err(Error::Invalid(_))),
                    "{bad:?} was granted"
                );
            }
        }
        drop(cat);
        let cat = fx.open();
        let only_secret = std::collections::BTreeSet::from(["secret".to_owned()]);
        assert_eq!(cat.principal("ada").unwrap().terms, only_secret);
        assert_eq!(cat.group("eu").unwrap().terms, only_secret);
    }

    #[test]
    fn a_name_is_stored_trimmed_and_an_empty_or_taken_name_is_refused() {
        let fx = Fixture::new();
        let cat = fx.open();
        cat.create_principal(" ada ", PrincipalKind::Person)
            .unwrap();
        cat.create_group(" eu ").unwrap();
        assert!(cat.principal("ada").is_some());
        assert!(cat.group("eu").is_some());
        assert!(matches!(
            cat.create_principal("ada", PrincipalKind::Service),
            Err(Error::Exists { .. })
        ));
        assert!(matches!(cat.create_group("eu"), Err(Error::Exists { .. })));
        assert!(matches!(
            cat.create_principal("  ", PrincipalKind::Person),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(cat.create_group(""), Err(Error::Invalid(_))));
    }

    #[test]
    fn a_name_holding_a_control_character_is_refused() {
        let fx = Fixture::new();
        let cat = fx.open();
        for bad in ["a\u{0}b", "ad\ta", "x\u{1b}[0m", "y\u{85}z"] {
            assert!(matches!(
                cat.create_principal(bad, PrincipalKind::Person),
                Err(Error::Invalid(_))
            ));
            assert!(matches!(cat.create_group(bad), Err(Error::Invalid(_))));
        }
        assert!(cat.principals().is_empty() && cat.groups().is_empty());
    }

    #[test]
    fn a_name_holding_a_zero_width_or_bidirectional_character_is_refused_and_a_term_is_not() {
        let fx = Fixture::new();
        let cat = fx.open();
        let bad = [
            "ad\u{200B}a",
            "a\u{200F}b",
            "\u{202E}nimda",
            "x\u{202A}y",
            "x\u{2066}y",
            "x\u{2069}y",
            "\u{FEFF}ada",
        ];
        for name in bad {
            assert!(matches!(
                cat.create_principal(name, PrincipalKind::Person),
                Err(Error::Invalid(_))
            ));
            assert!(matches!(cat.create_group(name), Err(Error::Invalid(_))));
        }
        assert!(cat.principals().is_empty() && cat.groups().is_empty());
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        for term in bad {
            cat.grant_term(Grantee::Principal("ada"), term).unwrap();
        }
        assert_eq!(cat.principal("ada").unwrap().terms.len(), bad.len());
    }
}
