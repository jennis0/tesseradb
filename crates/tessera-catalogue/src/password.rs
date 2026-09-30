//! Password hashing and the limit on failed attempts.
//!
//! A password is stored as an argon2id hash in PHC string form, which carries its own salt and
//! cost parameters, so a hash made under one set of parameters verifies after they change.
//!
//! The failed-attempt counts are held in memory and start empty when the catalogue is opened.
//! An attempt is counted as failed before the hash is checked and uncounted if it succeeds, so
//! concurrent attempts cannot together exceed the limit.

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use parking_lot::Mutex;

use crate::Error;

pub(crate) fn hash(password: &str) -> Result<String, Error> {
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).map_err(|e| Error::Storage(format!("no randomness: {e}")))?;
    let salt = SaltString::encode_b64(&salt)
        .map_err(|e| Error::Storage(format!("cannot encode a salt: {e}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::Storage(format!("cannot hash a password: {e}")))
}

/// Whether `password` matches `stored`. A stored value that is not a valid hash matches
/// nothing.
pub(crate) fn verify(stored: &str, password: &str) -> bool {
    PasswordHash::new(stored)
        .map(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
        .unwrap_or(false)
}

/// Spends the time of one verification, for a name with no password to check, so the answer
/// takes as long as a wrong password would.
pub(crate) fn verify_nothing(password: &str) {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let stored = DUMMY.get_or_init(|| hash("").unwrap_or_default());
    verify(stored, password);
}

pub(crate) struct Limiter {
    limit: usize,
    window: u64,
    failures: Mutex<HashMap<String, VecDeque<u64>>>,
}

impl Limiter {
    pub(crate) fn new(limit: u32, window_secs: u64) -> Limiter {
        Limiter {
            limit: limit as usize,
            window: window_secs,
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Counts an attempt for `principal` as failed, or refuses it with the time at which the
    /// next attempt will be taken.
    pub(crate) fn begin(&self, principal: &str, now: u64) -> Result<(), u64> {
        let mut failures = self.failures.lock();
        let times = failures.entry(principal.to_owned()).or_default();
        while times.front().is_some_and(|t| t + self.window <= now) {
            times.pop_front();
        }
        if times.len() >= self.limit {
            return Err(times.front().map_or(now, |t| t + self.window));
        }
        times.push_back(now);
        Ok(())
    }

    /// Clears `principal`'s failed attempts after one succeeds.
    pub(crate) fn succeeded(&self, principal: &str) {
        self.failures.lock().remove(principal);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::testing::Fixture;
    use crate::{AuthError, Catalogue, PrincipalKind};

    fn with_ada(fx: &Fixture, limit: u32, window: u64) -> Catalogue {
        let mut options = fx.options();
        options.failed_attempt_limit = limit;
        options.failed_attempt_window = Duration::from_secs(window);
        let cat = Catalogue::open(fx.dir.path(), options).unwrap();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        cat.set_password("ada", "correct horse").unwrap();
        cat
    }

    #[test]
    fn only_the_set_password_of_an_enabled_principal_authenticates() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 10, 900);
        let ok = cat.verify_password(" ada ", "correct horse").unwrap();
        assert_eq!(ok.principal, "ada");
        assert_eq!(ok.api_key, None);
        assert_eq!(
            cat.verify_password("ada", "Correct horse"),
            Err(AuthError::Refused)
        );
        assert_eq!(
            cat.verify_password("bob", "correct horse"),
            Err(AuthError::Refused)
        );

        cat.disable_principal("ada").unwrap();
        assert_eq!(
            cat.verify_password("ada", "correct horse"),
            Err(AuthError::Refused)
        );
        cat.enable_principal("ada").unwrap();
        assert!(cat.verify_password("ada", "correct horse").is_ok());

        cat.set_password("ada", "battery staple").unwrap();
        assert_eq!(
            cat.verify_password("ada", "correct horse"),
            Err(AuthError::Refused)
        );
        assert!(cat.verify_password("ada", "battery staple").is_ok());

        cat.clear_password("ada").unwrap();
        assert!(!cat.principal("ada").unwrap().has_password);
        assert_eq!(
            cat.verify_password("ada", "battery staple"),
            Err(AuthError::Refused)
        );
    }

    #[test]
    fn the_stored_password_is_an_argon2id_hash() {
        let fx = Fixture::new();
        drop(with_ada(&fx, 10, 900));
        let stored: String = fx
            .raw()
            .query_row("SELECT password_hash FROM principal", [], |r| r.get(0))
            .unwrap();
        assert!(stored.starts_with("$argon2id$"));
        assert!(!stored.contains("correct horse"));
    }

    #[test]
    fn failures_up_to_the_limit_refuse_even_the_right_password_until_the_window_passes() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 3, 60);
        let start = fx.now();
        for _ in 0..3 {
            assert_eq!(cat.verify_password("ada", "wrong"), Err(AuthError::Refused));
            fx.advance(10);
        }
        // Failures at start, +10 and +20: the next attempt is taken once the first leaves.
        let throttled = Err(AuthError::Throttled {
            retry_at: start + 60,
        });
        assert_eq!(cat.verify_password("ada", "correct horse"), throttled);
        fx.advance(29);
        assert_eq!(cat.verify_password("ada", "correct horse"), throttled);
        fx.advance(1);
        assert!(cat.verify_password("ada", "correct horse").is_ok());
    }

    #[test]
    fn a_success_clears_the_failures_before_it() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 3, 60);
        for _ in 0..2 {
            cat.verify_password("ada", "wrong").unwrap_err();
        }
        cat.verify_password("ada", "correct horse").unwrap();
        for _ in 0..2 {
            cat.verify_password("ada", "wrong").unwrap_err();
        }
        assert!(cat.verify_password("ada", "correct horse").is_ok());
    }

    #[test]
    fn the_default_limit_is_ten_failures_in_fifteen_minutes() {
        let fx = Fixture::new();
        let cat = fx.open();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        cat.set_password("ada", "correct horse").unwrap();
        let start = fx.now();
        for _ in 0..10 {
            cat.verify_password("ada", "wrong").unwrap_err();
        }
        assert_eq!(
            cat.verify_password("ada", "correct horse"),
            Err(AuthError::Throttled {
                retry_at: start + 900
            })
        );
        fx.advance(900);
        assert!(cat.verify_password("ada", "correct horse").is_ok());
    }
}
