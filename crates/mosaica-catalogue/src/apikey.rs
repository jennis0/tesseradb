//! The API key format: `tsk_<prefix>_<secret>`, with a 12-hex-digit prefix that is public and
//! finds the record, and a 64-hex-digit secret of 256 random bits. The catalogue stores the
//! SHA-256 of the secret. A secret of that entropy cannot be recovered from its hash by guessing,
//! so a fast hash is enough.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::Error;

const TAG: &str = "tsk";
const PREFIX_BYTES: usize = 6;
const SECRET_BYTES: usize = 32;

pub(crate) struct Fresh {
    pub prefix: String,
    pub secret: String,
}

impl Fresh {
    pub(crate) fn generate() -> Result<Fresh, Error> {
        let mut prefix = [0u8; PREFIX_BYTES];
        let mut secret = [0u8; SECRET_BYTES];
        getrandom::getrandom(&mut prefix)
            .and_then(|()| getrandom::getrandom(&mut secret))
            .map_err(|e| Error::Storage(format!("no randomness: {e}")))?;
        Ok(Fresh {
            prefix: hex(&prefix),
            secret: hex(&secret),
        })
    }

    pub(crate) fn key(&self) -> String {
        format!("{TAG}_{}_{}", self.prefix, self.secret)
    }
}

/// Splits a presented key into its prefix and secret, or `None` when it is not in the format.
pub(crate) fn parse(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix(TAG)?.strip_prefix('_')?;
    let (prefix, secret) = rest.split_once('_')?;
    let is_hex = |s: &str, bytes: usize| {
        s.len() == bytes * 2 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    (is_hex(prefix, PREFIX_BYTES) && is_hex(secret, SECRET_BYTES)).then_some((prefix, secret))
}

pub(crate) fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

pub(crate) fn matches(stored: &[u8; 32], secret: &str) -> bool {
    stored.ct_eq(&digest(secret)).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use crate::testing::Fixture;
    use crate::{AuthError, Catalogue, Error, Grantee, Permission, PermissionSet, PrincipalKind};

    fn with_ingest(fx: &Fixture) -> Catalogue {
        let cat = fx.open();
        cat.create_principal("ingest", PrincipalKind::Service)
            .unwrap();
        cat
    }

    #[test]
    fn an_issued_key_authenticates_its_principal_and_its_secret_is_not_stored() {
        let fx = Fixture::new();
        let cat = with_ingest(&fx);
        let (issued, _) = cat.create_api_key("ingest", None, None).unwrap();
        let (prefix, secret) = super::parse(&issued.key).expect("the issued key parses");
        assert_eq!(prefix, issued.prefix);

        let ok = cat.verify_api_key(&issued.key).unwrap();
        assert_eq!(ok.principal, "ingest");
        assert_eq!(ok.api_key.as_deref(), Some(prefix));
        let listed = cat.api_keys("ingest");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].prefix, issued.prefix);
        assert_eq!(listed[0].created_at, fx.now());

        drop(cat);
        let file = std::fs::read(fx.path().join(crate::store::FILE_NAME)).unwrap();
        let found = file.windows(secret.len()).any(|w| w == secret.as_bytes());
        assert!(!found, "the secret is in the catalogue file");
    }

    #[test]
    fn an_issued_key_prints_its_prefix_and_not_its_secret() {
        let fx = Fixture::new();
        let cat = with_ingest(&fx);
        let (issued, _) = cat.create_api_key("ingest", None, None).unwrap();
        let (_, secret) = super::parse(&issued.key).unwrap();
        let printed = format!("{issued:?} {issued:#?}");
        assert!(printed.contains(&issued.prefix));
        assert!(!printed.contains(secret));
    }

    #[test]
    fn two_keys_are_distinct_and_each_verifies_alone() {
        let fx = Fixture::new();
        let cat = with_ingest(&fx);
        let (a, _) = cat.create_api_key("ingest", None, None).unwrap();
        let (b, _) = cat.create_api_key("ingest", None, None).unwrap();
        assert_ne!(a.prefix, b.prefix);
        // b's secret under a's prefix.
        let (_, b_secret) = super::parse(&b.key).unwrap();
        let crossed = format!("tsk_{}_{}", a.prefix, b_secret);
        assert_eq!(cat.verify_api_key(&crossed), Err(AuthError::Refused));
    }

    #[test]
    fn a_wrong_malformed_revoked_or_expired_key_is_refused() {
        let fx = Fixture::new();
        let cat = with_ingest(&fx);
        let (issued, _) = cat
            .create_api_key("ingest", Some(fx.now() + 60), None)
            .unwrap();

        let mut wrong = issued.key.clone();
        let last = wrong.pop().unwrap();
        wrong.push(if last == '0' { '1' } else { '0' });
        assert_eq!(cat.verify_api_key(&wrong), Err(AuthError::Refused));
        for malformed in [
            "",
            "tsk_",
            "tsk__",
            &issued.key.to_uppercase(),
            &issued.key[4..],
        ] {
            assert_eq!(cat.verify_api_key(malformed), Err(AuthError::Refused));
        }

        assert!(cat.verify_api_key(&issued.key).is_ok());
        fx.advance(60);
        assert_eq!(cat.verify_api_key(&issued.key), Err(AuthError::Refused));
        assert_eq!(cat.resolve("ingest", Some(&issued.prefix)), None);

        let (live, _) = cat.create_api_key("ingest", None, None).unwrap();
        cat.revoke_api_key(&live.prefix).unwrap();
        assert_eq!(cat.verify_api_key(&live.key), Err(AuthError::Refused));
        assert!(matches!(
            cat.revoke_api_key(&live.prefix),
            Err(Error::NotFound { .. })
        ));
        drop(cat);
        let cat = fx.open();
        assert_eq!(cat.verify_api_key(&live.key), Err(AuthError::Refused));
    }

    #[test]
    fn a_disabled_principal_s_key_is_refused_until_it_is_enabled() {
        let fx = Fixture::new();
        let cat = with_ingest(&fx);
        let (issued, _) = cat.create_api_key("ingest", None, None).unwrap();
        cat.disable_principal("ingest").unwrap();
        assert_eq!(cat.verify_api_key(&issued.key), Err(AuthError::Refused));
        cat.enable_principal("ingest").unwrap();
        assert!(cat.verify_api_key(&issued.key).is_ok());
    }

    #[test]
    fn a_key_narrows_its_principal_s_permissions_and_never_widens_them() {
        let fx = Fixture::new();
        let cat = with_ingest(&fx);
        cat.grant_permission(Grantee::Principal("ingest"), Permission::Read)
            .unwrap();
        cat.grant_permission(Grantee::Principal("ingest"), Permission::Write)
            .unwrap();
        let only = |ps: &[Permission]| ps.iter().copied().collect::<PermissionSet>();
        let (narrow, _) = cat
            .create_api_key(
                "ingest",
                None,
                Some(only(&[Permission::Write, Permission::Admin])),
            )
            .unwrap();
        let (full, _) = cat.create_api_key("ingest", None, None).unwrap();

        let with_narrow = cat.resolve("ingest", Some(&narrow.prefix)).unwrap();
        assert_eq!(with_narrow.permissions, only(&[Permission::Write]));
        let with_full = cat.resolve("ingest", Some(&full.prefix)).unwrap();
        assert_eq!(
            with_full.permissions,
            only(&[Permission::Read, Permission::Write])
        );
    }

    #[test]
    fn a_key_resolves_only_for_its_own_principal() {
        let fx = Fixture::new();
        let cat = with_ingest(&fx);
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        let (issued, _) = cat.create_api_key("ingest", None, None).unwrap();
        assert_eq!(cat.resolve("ada", Some(&issued.prefix)), None);
        assert_eq!(cat.resolve("ingest", Some("000000000000")), None);
        assert!(matches!(
            cat.create_api_key("nobody", None, None),
            Err(Error::NotFound { .. })
        ));
    }
}
