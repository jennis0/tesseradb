//! Password hashing and the limit on failed attempts.
//!
//! A password is stored as an argon2id hash in PHC string form, which carries its own salt and
//! cost parameters, so a hash made under one set of parameters verifies after they change.
//!
//! The failed-attempt counts are held in memory and start empty when the catalogue is opened.
//! They are kept for the name presented, whether or not a principal has it, so a throttled name
//! does not show that the principal exists. An attempt is counted as failed before the hash is
//! checked and uncounted if it succeeds, so concurrent attempts cannot together exceed the limit.
//! At most a configured number of names is remembered. Past it, the name whose latest failure is
//! oldest is forgotten, so an attacker who spreads failures over that many names gets one name's
//! count reset.

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use parking_lot::Mutex;

use crate::Error;

/// Refuses a password of fewer than `min` characters. No other rule is applied.
pub(crate) fn check_length(password: &str, min: usize) -> Result<(), Error> {
    let len = password.chars().count();
    if len < min {
        return Err(Error::Invalid(format!(
            "the password holds {len} characters and the least accepted is {min}; write a \
             longer one, such as a phrase of several words"
        )));
    }
    Ok(())
}

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
    capacity: usize,
    failures: Mutex<HashMap<String, VecDeque<u64>>>,
}

impl Limiter {
    pub(crate) fn new(limit: u32, window_secs: u64, capacity: usize) -> Limiter {
        Limiter {
            limit: limit as usize,
            window: window_secs,
            capacity: capacity.max(1),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Counts an attempt for `name` as failed, or returns false when `name` has reached the
    /// limit within the window.
    pub(crate) fn begin(&self, name: &str, now: u64) -> bool {
        let mut failures = self.failures.lock();
        if !failures.contains_key(name) && failures.len() >= self.capacity {
            failures.retain(|_, times| times.back().is_some_and(|t| t + self.window > now));
            if failures.len() >= self.capacity {
                let oldest = failures
                    .iter()
                    .min_by_key(|(_, times)| times.back().copied())
                    .map(|(n, _)| n.clone());
                if let Some(oldest) = oldest {
                    failures.remove(&oldest);
                }
            }
        }
        let times = failures.entry(name.to_owned()).or_default();
        while times.front().is_some_and(|t| t + self.window <= now) {
            times.pop_front();
        }
        if times.len() >= self.limit {
            return false;
        }
        times.push_back(now);
        true
    }

    /// Clears `name`'s failed attempts after one succeeds.
    pub(crate) fn succeeded(&self, name: &str) {
        self.failures.lock().remove(name);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::testing::Fixture;
    use crate::{AuthError, Catalogue, Error, PrincipalKind};

    fn with_ada(fx: &Fixture, limit: u32, window: u64) -> Catalogue {
        let mut options = fx.options();
        options.failed_attempt_limit = limit;
        options.failed_attempt_window = Duration::from_secs(window);
        let cat = Catalogue::open(&fx.path(), options).unwrap();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        cat.set_password("ada", "correct horse battery").unwrap();
        cat
    }

    #[test]
    fn only_the_set_password_of_an_enabled_principal_authenticates() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 10, 900);
        let ok = cat
            .verify_password(" ada ", "correct horse battery")
            .unwrap();
        assert_eq!(ok.principal, "ada");
        assert_eq!(ok.api_key, None);
        assert_eq!(
            cat.verify_password("ada", "Correct horse battery"),
            Err(AuthError::Refused)
        );
        assert_eq!(
            cat.verify_password("bob", "correct horse battery"),
            Err(AuthError::Refused)
        );

        cat.disable_principal("ada").unwrap();
        assert_eq!(
            cat.verify_password("ada", "correct horse battery"),
            Err(AuthError::Refused)
        );
        cat.enable_principal("ada").unwrap();
        assert!(cat.verify_password("ada", "correct horse battery").is_ok());

        cat.set_password("ada", "battery staple horse").unwrap();
        assert_eq!(
            cat.verify_password("ada", "correct horse battery"),
            Err(AuthError::Refused)
        );
        assert!(cat.verify_password("ada", "battery staple horse").is_ok());

        cat.clear_password("ada").unwrap();
        assert!(!cat.principal("ada").unwrap().has_password);
        assert_eq!(
            cat.verify_password("ada", "battery staple horse"),
            Err(AuthError::Refused)
        );
    }

    #[test]
    fn a_password_shorter_than_the_minimum_is_refused_and_leaves_the_old_one() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 10, 900);
        let fourteen = "abcdefghijklmn";
        for short in ["", " ", fourteen, "ééééééééééééé"] {
            assert!(
                matches!(cat.set_password("ada", short), Err(Error::Invalid(_))),
                "{short:?} was accepted"
            );
        }
        assert!(cat.verify_password("ada", "correct horse battery").is_ok());
        // Fifteen characters, counted as characters and not bytes.
        cat.set_password("ada", "ééééééééééééééé").unwrap();
        assert!(cat.verify_password("ada", "ééééééééééééééé").is_ok());
    }

    #[test]
    fn the_minimum_password_length_is_configurable_and_at_least_one() {
        let fx = Fixture::new();
        let mut options = fx.options();
        options.min_password_length = 0;
        assert!(matches!(
            Catalogue::open(&fx.path(), options.clone()),
            Err(Error::Invalid(_))
        ));
        options.min_password_length = 1;
        let cat = Catalogue::open(&fx.path(), options).unwrap();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        assert!(matches!(
            cat.set_password("ada", ""),
            Err(Error::Invalid(_))
        ));
        cat.set_password("ada", "x").unwrap();
        assert!(cat.verify_password("ada", "x").is_ok());
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
        assert!(!stored.contains("correct horse battery"));
    }

    #[test]
    fn failures_up_to_the_limit_refuse_even_the_right_password_until_the_window_passes() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 3, 60);
        for _ in 0..3 {
            assert_eq!(cat.verify_password("ada", "wrong"), Err(AuthError::Refused));
            fx.advance(10);
        }
        // Failures at start, +10 and +20: the next attempt is taken once the first leaves.
        assert_eq!(
            cat.verify_password("ada", "correct horse battery"),
            Err(AuthError::Refused)
        );
        fx.advance(29);
        assert_eq!(
            cat.verify_password("ada", "correct horse battery"),
            Err(AuthError::Refused)
        );
        fx.advance(1);
        assert!(cat.verify_password("ada", "correct horse battery").is_ok());
    }

    #[test]
    fn a_throttled_name_answers_as_an_unknown_one_does() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 3, 60);
        let answers = |cat: &Catalogue| {
            (0..5)
                .map(|_| {
                    (
                        cat.verify_password("ada", "wrong"),
                        cat.verify_password("nobody", "wrong"),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (known, unknown) in answers(&cat) {
            assert_eq!(known, unknown);
        }
    }

    #[test]
    fn failures_count_against_a_name_that_cannot_authenticate() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 3, 60);
        cat.create_principal("disabled", PrincipalKind::Person)
            .unwrap();
        cat.set_password("disabled", "a long enough password")
            .unwrap();
        cat.disable_principal("disabled").unwrap();
        cat.create_principal("no-password", PrincipalKind::Person)
            .unwrap();
        for name in ["later", "disabled", "no-password"] {
            for _ in 0..3 {
                cat.verify_password(name, "wrong").unwrap_err();
            }
        }
        cat.create_principal("later", PrincipalKind::Person)
            .unwrap();
        cat.set_password("later", "a long enough password").unwrap();
        cat.enable_principal("disabled").unwrap();
        cat.set_password("no-password", "a long enough password")
            .unwrap();
        for name in ["later", "disabled", "no-password"] {
            assert_eq!(
                cat.verify_password(name, "a long enough password"),
                Err(AuthError::Refused)
            );
        }
        fx.advance(60);
        for name in ["later", "disabled", "no-password"] {
            assert!(
                cat.verify_password(name, "a long enough password").is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn past_the_remembered_names_the_oldest_is_forgotten() {
        let fx = Fixture::new();
        let mut options = fx.options();
        options.failed_attempt_limit = 2;
        options.failed_attempt_names = 2;
        let cat = Catalogue::open(&fx.path(), options).unwrap();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        cat.set_password("ada", "correct horse battery").unwrap();
        for _ in 0..2 {
            cat.verify_password("ada", "wrong").unwrap_err();
        }
        assert!(cat.verify_password("ada", "correct horse battery").is_err());
        fx.advance(1);
        cat.verify_password("x", "wrong").unwrap_err();
        fx.advance(1);
        cat.verify_password("y", "wrong").unwrap_err();
        assert!(cat.verify_password("ada", "correct horse battery").is_ok());
    }

    #[test]
    fn a_success_clears_the_failures_before_it() {
        let fx = Fixture::new();
        let cat = with_ada(&fx, 3, 60);
        for _ in 0..2 {
            cat.verify_password("ada", "wrong").unwrap_err();
        }
        cat.verify_password("ada", "correct horse battery").unwrap();
        for _ in 0..2 {
            cat.verify_password("ada", "wrong").unwrap_err();
        }
        assert!(cat.verify_password("ada", "correct horse battery").is_ok());
    }

    #[test]
    fn the_default_limit_is_ten_failures_in_fifteen_minutes() {
        let fx = Fixture::new();
        let cat = fx.open();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        cat.set_password("ada", "correct horse battery").unwrap();
        for _ in 0..10 {
            cat.verify_password("ada", "wrong").unwrap_err();
        }
        assert_eq!(
            cat.verify_password("ada", "correct horse battery"),
            Err(AuthError::Refused)
        );
        fx.advance(899);
        assert!(cat.verify_password("ada", "correct horse battery").is_err());
        fx.advance(1);
        assert!(cat.verify_password("ada", "correct horse battery").is_ok());
    }
}
