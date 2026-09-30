//! The rules for what a term and a name may be. Every write that stores one calls these, and
//! a stored value is the trimmed one.

use crate::Error;

/// The term every session holds. It is added where a credential is evaluated, and no grant or
/// claim rule may name it.
pub const PUBLIC: &str = "public";

/// Trims a term. An empty term and the reserved term `public` are refused.
pub fn term(raw: &str) -> Result<String, Error> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(Error::Invalid(
            "a term is empty after trimming; write at least one visible character".into(),
        ));
    }
    if t == PUBLIC {
        return Err(Error::Invalid(
            "`public` is reserved and every session holds it; grant a different term".into(),
        ));
    }
    Ok(t.to_owned())
}

/// Trims the name of a principal, a group or a provider. An empty name is refused.
pub fn name(what: &str, raw: &str) -> Result<String, Error> {
    let n = raw.trim();
    if n.is_empty() {
        return Err(Error::Invalid(format!(
            "a {what} name is empty after trimming; write at least one visible character"
        )));
    }
    Ok(n.to_owned())
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
            for bad in ["", "   ", "public", " public "] {
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
        // A term that differs from `public` only in case is an ordinary term.
        cat.grant_term(Grantee::Principal("ada"), "Public").unwrap();
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
}
