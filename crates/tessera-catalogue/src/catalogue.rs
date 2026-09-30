//! The catalogue's operations. Each change validates against memory, writes to SQLite in one
//! transaction, commits, and only then applies the same change to memory. Changes are serialised
//! by the connection's lock, so memory cannot move between the validation and the apply.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::File;
use std::path::Path;

use parking_lot::{Mutex, RwLock};
use rusqlite::{params, Connection, Transaction};
use serde_json::Value;

use crate::apikey;
use crate::names;
use crate::password::{self, Limiter};
use crate::permission::{Permission, PermissionSet};
use crate::provider::Provider;
use crate::store;
use crate::{Affected, AuthError, Clock, Error, Options};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalKind {
    Person,
    Service,
}

impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::Person => "person",
            PrincipalKind::Service => "service",
        }
    }

    pub fn parse(s: &str) -> Option<PrincipalKind> {
        match s.trim() {
            "person" => Some(PrincipalKind::Person),
            "service" => Some(PrincipalKind::Service),
            _ => None,
        }
    }
}

/// Who a grant is made to.
#[derive(Clone, Copy, Debug)]
pub enum Grantee<'a> {
    Principal(&'a str),
    Group(&'a str),
}

/// A local principal as the catalogue holds it. The password hash is not exposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrincipalInfo {
    pub name: String,
    pub kind: PrincipalKind,
    pub disabled: bool,
    pub bypass: bool,
    pub has_password: bool,
    /// Terms granted directly, without those of its groups.
    pub terms: BTreeSet<String>,
    /// Permissions granted directly, without those of its groups.
    pub permissions: PermissionSet,
    pub groups: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupInfo {
    pub name: String,
    pub terms: BTreeSet<String>,
    pub permissions: PermissionSet,
    pub members: BTreeSet<String>,
}

/// An API key without its secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeyInfo {
    pub prefix: String,
    pub principal: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    /// The key's own permissions, when narrower than its principal's.
    pub permissions: Option<PermissionSet>,
}

/// A key as issued. `key` is the whole credential, returned here and nowhere else. `Debug`
/// shows the prefix alone.
#[derive(Clone)]
pub struct IssuedKey {
    pub prefix: String,
    pub key: String,
}

impl fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedKey")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderInfo {
    pub provider: Provider,
    /// Declared in the configuration; the catalogue refuses to change or remove it.
    pub read_only: bool,
}

/// A local principal whose credential has been checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authenticated {
    pub principal: String,
    /// The prefix of the API key used, if one was.
    pub api_key: Option<String>,
}

/// What a session holds: its terms, its permissions and whether it writes against the whole
/// corpus. The reserved term `public`, which every session holds, is not listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    pub terms: BTreeSet<String>,
    pub permissions: PermissionSet,
    pub bypass: bool,
    /// The catalogue's generation this was resolved at.
    pub generation: u64,
}

pub(crate) struct PrincipalRec {
    pub id: i64,
    pub kind: PrincipalKind,
    pub disabled: bool,
    pub bypass: bool,
    pub permissions: PermissionSet,
    pub password: Option<String>,
    pub terms: BTreeSet<String>,
    pub groups: BTreeSet<String>,
}

pub(crate) struct GroupRec {
    pub id: i64,
    pub permissions: PermissionSet,
    pub terms: BTreeSet<String>,
    pub members: BTreeSet<String>,
}

pub(crate) struct KeyRec {
    pub principal: String,
    pub hash: [u8; 32],
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub permissions: Option<PermissionSet>,
}

#[derive(Default)]
pub(crate) struct State {
    pub principals: BTreeMap<String, PrincipalRec>,
    pub groups: BTreeMap<String, GroupRec>,
    pub keys: HashMap<String, KeyRec>,
    /// Each provider, and whether it came from the configuration.
    pub providers: BTreeMap<String, (Provider, bool)>,
    pub generation: u64,
}

impl State {
    fn principal(&self, name: &str) -> Result<&PrincipalRec, Error> {
        self.principals.get(name).ok_or_else(|| Error::NotFound {
            what: "principal",
            name: name.to_owned(),
        })
    }

    fn group(&self, name: &str) -> Result<&GroupRec, Error> {
        self.groups.get(name).ok_or_else(|| Error::NotFound {
            what: "group",
            name: name.to_owned(),
        })
    }

    /// The members of `group`, and every provider with a role mapping to it.
    fn affected_by_group(&self, group: &str) -> Affected {
        Affected {
            principals: self
                .groups
                .get(group)
                .map(|g| g.members.clone())
                .unwrap_or_default(),
            api_keys: BTreeSet::new(),
            generation: 0,
            providers: self
                .providers
                .values()
                .filter(|(p, _)| p.maps_to(group))
                .map(|(p, _)| p.name.clone())
                .collect(),
        }
    }

    fn affected_by_grantee(&self, grantee: &Grantee<'_>) -> Affected {
        match grantee {
            Grantee::Principal(p) => only_principal(p),
            Grantee::Group(g) => self.affected_by_group(g),
        }
    }

    fn add_group_grants(&self, groups: &BTreeSet<String>, out: &mut Resolution) {
        for g in groups.iter().filter_map(|g| self.groups.get(g)) {
            out.terms.extend(g.terms.iter().cloned());
            out.permissions = out.permissions.union(g.permissions);
        }
    }
}

fn only_principal(name: &str) -> Affected {
    Affected {
        principals: BTreeSet::from([name.to_owned()]),
        ..Affected::default()
    }
}

/// The change to memory once the transaction commits, or `None` when there is nothing to change.
type Apply = Option<Box<dyn FnOnce(&mut State) + Send>>;

fn nothing() -> Apply {
    None
}

fn secs(t: u64) -> Result<i64, Error> {
    i64::try_from(t).map_err(|_| {
        Error::Invalid(format!(
            "the time {t} is out of range; write seconds since the epoch"
        ))
    })
}

/// The identity catalogue. It is `Send + Sync`; reads take a shared lock on memory and changes
/// are serialised.
pub struct Catalogue {
    conn: Mutex<Connection>,
    state: RwLock<State>,
    limiter: Limiter,
    clock: Clock,
    min_password_length: usize,
    allow_insecure_jwks: bool,
    /// Holds the catalogue's lock until the catalogue is dropped.
    _lock: File,
}

impl Catalogue {
    /// Opens the catalogue in `dir`, creating it when absent. A catalogue that another
    /// `Catalogue` holds open, that others can reach, that was written by a different schema
    /// version, that holds a row breaking a rule of what may be stored, or that holds a provider
    /// also in `options.config_providers` is refused, as is a `min_password_length` of 0.
    pub fn open(dir: &Path, options: Options) -> Result<Catalogue, Error> {
        if options.min_password_length < 1 {
            return Err(Error::Invalid(
                "the minimum password length is 0, which accepts the empty password; set it to \
                 at least 1"
                    .into(),
            ));
        }
        let (conn, lock) = store::open(dir)?;
        let mut state = store::load(&conn, options.allow_insecure_jwks)?;
        add_config_providers(&mut state, &options)?;
        Ok(Catalogue {
            conn: Mutex::new(conn),
            state: RwLock::new(state),
            limiter: Limiter::new(
                options.failed_attempt_limit,
                options.failed_attempt_window.as_secs(),
                options.failed_attempt_names,
            ),
            clock: options.clock,
            min_password_length: options.min_password_length,
            allow_insecure_jwks: options.allow_insecure_jwks,
            _lock: lock,
        })
    }

    /// The catalogue's generation. It starts at 0, rises by one with each committed change that
    /// alters the catalogue, and is stored, so it never goes back when the catalogue is reopened.
    /// Every [`Resolution`] and [`Affected`] carries the generation it was made at. A server
    /// resolves a credential, registers the session, and then compares the resolution's
    /// generation with this one or with the generation of each change it has applied since: if
    /// the catalogue has moved on, it resolves again or ends the session.
    pub fn generation(&self) -> u64 {
        self.state.read().generation
    }

    /// Runs one change: `f` validates against memory and writes through the transaction, and
    /// returns the apply for memory, which runs only after the commit succeeds.
    fn change<T>(
        &self,
        f: impl FnOnce(&State, &Transaction<'_>) -> Result<(Apply, T), Error>,
    ) -> Result<(T, u64), Error> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let (apply, out) = {
            let st = self.state.read();
            f(&st, &tx)?
        };
        let Some(apply) = apply else {
            return Ok((out, self.state.read().generation));
        };
        let generation: i64 = tx.query_row(
            "UPDATE generation SET value = value + 1 RETURNING value",
            [],
            |r| r.get(0),
        )?;
        let generation = generation as u64;
        tx.commit()?;
        let mut st = self.state.write();
        apply(&mut st);
        st.generation = generation;
        Ok((out, generation))
    }

    /// Runs a change that reports what it affected, stamping the report with the generation.
    fn affecting(
        &self,
        f: impl FnOnce(&State, &Transaction<'_>) -> Result<(Apply, Affected), Error>,
    ) -> Result<Affected, Error> {
        let (affected, generation) = self.change(f)?;
        Ok(Affected {
            generation,
            ..affected
        })
    }

    pub fn create_principal(&self, name: &str, kind: PrincipalKind) -> Result<Affected, Error> {
        let name = names::name("principal", name)?;
        self.affecting(|st, tx| {
            if st.principals.contains_key(&name) {
                return Err(Error::Exists {
                    what: "principal",
                    name,
                });
            }
            tx.execute(
                "INSERT INTO principal (name, kind) VALUES (?1, ?2)",
                params![name, kind.as_str()],
            )?;
            let id = tx.last_insert_rowid();
            let apply: Apply = Some(Box::new(move |st| {
                st.principals.insert(
                    name,
                    PrincipalRec {
                        id,
                        kind,
                        disabled: false,
                        bypass: false,
                        permissions: PermissionSet::EMPTY,
                        password: None,
                        terms: BTreeSet::new(),
                        groups: BTreeSet::new(),
                    },
                );
            }));
            Ok((apply, Affected::default()))
        })
    }

    pub fn disable_principal(&self, name: &str) -> Result<Affected, Error> {
        self.set_flag(name, "disabled", true)
    }

    pub fn enable_principal(&self, name: &str) -> Result<Affected, Error> {
        self.set_flag(name, "disabled", false)
    }

    /// Whether `principal` writes against the whole corpus, where it also holds `write`.
    pub fn set_bypass(&self, principal: &str, bypass: bool) -> Result<Affected, Error> {
        self.set_flag(principal, "bypass", bypass)
    }

    fn set_flag(&self, name: &str, column: &'static str, value: bool) -> Result<Affected, Error> {
        let name = name.trim().to_owned();
        self.affecting(|st, tx| {
            let p = st.principal(&name)?;
            let current = if column == "disabled" {
                p.disabled
            } else {
                p.bypass
            };
            if current == value {
                return Ok((nothing(), Affected::default()));
            }
            tx.execute(
                &format!("UPDATE principal SET {column} = ?1 WHERE id = ?2"),
                params![value, p.id],
            )?;
            let affected = only_principal(&name);
            let apply: Apply = Some(Box::new(move |st| {
                let p = st.principals.get_mut(&name).expect("validated");
                if column == "disabled" {
                    p.disabled = value;
                } else {
                    p.bypass = value;
                }
            }));
            Ok((apply, affected))
        })
    }

    /// Deletes a principal with its password, API keys, grants and memberships.
    pub fn delete_principal(&self, name: &str) -> Result<Affected, Error> {
        let name = name.trim().to_owned();
        let out = self.affecting(|st, tx| {
            let p = st.principal(&name)?;
            tx.execute("DELETE FROM principal WHERE id = ?1", params![p.id])?;
            let keys: BTreeSet<String> = st
                .keys
                .iter()
                .filter(|(_, k)| k.principal == name)
                .map(|(prefix, _)| prefix.clone())
                .collect();
            let affected = Affected {
                principals: BTreeSet::from([name.clone()]),
                api_keys: keys.clone(),
                ..Affected::default()
            };
            let apply: Apply = Some(Box::new(move |st| {
                if let Some(p) = st.principals.remove(&name) {
                    for g in &p.groups {
                        if let Some(g) = st.groups.get_mut(g) {
                            g.members.remove(&name);
                        }
                    }
                }
                for k in &keys {
                    st.keys.remove(k);
                }
            }));
            Ok((apply, affected))
        })?;
        for p in &out.principals {
            self.limiter.succeeded(p);
        }
        Ok(out)
    }

    pub fn principal(&self, name: &str) -> Option<PrincipalInfo> {
        let st = self.state.read();
        st.principals
            .get(name.trim())
            .map(|p| principal_info(name.trim(), p))
    }

    pub fn principals(&self) -> Vec<PrincipalInfo> {
        let st = self.state.read();
        st.principals
            .iter()
            .map(|(n, p)| principal_info(n, p))
            .collect()
    }

    /// Sets the principal's password, replacing any previous one. A password shorter than the
    /// minimum the catalogue was opened with is refused.
    pub fn set_password(&self, principal: &str, password: &str) -> Result<Affected, Error> {
        password::check_length(password, self.min_password_length)?;
        let hash = password::hash(password)?;
        self.write_password(principal, Some(hash))
    }

    /// Removes the principal's password, so no password authenticates it.
    pub fn clear_password(&self, principal: &str) -> Result<Affected, Error> {
        self.write_password(principal, None)
    }

    fn write_password(&self, principal: &str, hash: Option<String>) -> Result<Affected, Error> {
        let name = principal.trim().to_owned();
        self.affecting(|st, tx| {
            let p = st.principal(&name)?;
            if hash.is_none() && p.password.is_none() {
                return Ok((nothing(), Affected::default()));
            }
            tx.execute(
                "UPDATE principal SET password_hash = ?1 WHERE id = ?2",
                params![hash, p.id],
            )?;
            let affected = only_principal(&name);
            let apply: Apply = Some(Box::new(move |st| {
                st.principals.get_mut(&name).expect("validated").password = hash;
            }));
            Ok((apply, affected))
        })
    }

    /// Checks a password. Every failure counts against the name presented, whether or not a
    /// principal has it. A name that is unknown, disabled, has no password or has too many
    /// recent failures is refused after the same work as a wrong password, with the same answer.
    pub fn verify_password(
        &self,
        principal: &str,
        password: &str,
    ) -> Result<Authenticated, AuthError> {
        let name = principal.trim();
        let stored = if self.limiter.begin(name, (self.clock)()) {
            let st = self.state.read();
            st.principals
                .get(name)
                .filter(|p| !p.disabled)
                .and_then(|p| p.password.clone())
        } else {
            None
        };
        let accepted = match stored {
            Some(stored) => password::verify(&stored, password),
            None => {
                password::verify_nothing(password);
                false
            }
        };
        if !accepted {
            return Err(AuthError::Refused);
        }
        self.limiter.succeeded(name);
        Ok(Authenticated {
            principal: name.to_owned(),
            api_key: None,
        })
    }

    /// Issues an API key for `principal`. With `permissions`, a session authorised by the key
    /// holds at most those, and never more than the principal holds.
    pub fn create_api_key(
        &self,
        principal: &str,
        expires_at: Option<u64>,
        permissions: Option<PermissionSet>,
    ) -> Result<(IssuedKey, Affected), Error> {
        let name = principal.trim().to_owned();
        let expires = expires_at.map(secs).transpose()?;
        let now = (self.clock)();
        let created = secs(now)?;
        let (issued, generation) = self.change(|st, tx| {
            let p = st.principal(&name)?;
            let fresh = loop {
                let fresh = apikey::Fresh::generate()?;
                if !st.keys.contains_key(&fresh.prefix) {
                    break fresh;
                }
            };
            let hash = apikey::digest(&fresh.secret);
            tx.execute(
                "INSERT INTO api_key (prefix, principal_id, secret_sha256, created_at, \
                 expires_at, permissions) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    fresh.prefix,
                    p.id,
                    &hash[..],
                    created,
                    expires,
                    permissions.map(PermissionSet::bits)
                ],
            )?;
            let issued = IssuedKey {
                prefix: fresh.prefix.clone(),
                key: fresh.key(),
            };
            let prefix = fresh.prefix;
            let apply: Apply = Some(Box::new(move |st| {
                st.keys.insert(
                    prefix,
                    KeyRec {
                        principal: name,
                        hash,
                        created_at: now,
                        expires_at,
                        permissions,
                    },
                );
            }));
            Ok((apply, issued))
        })?;
        let affected = Affected {
            generation,
            ..Affected::default()
        };
        Ok((issued, affected))
    }

    pub fn revoke_api_key(&self, prefix: &str) -> Result<Affected, Error> {
        let prefix = prefix.trim().to_owned();
        self.affecting(|st, tx| {
            if !st.keys.contains_key(&prefix) {
                return Err(Error::NotFound {
                    what: "API key",
                    name: prefix,
                });
            }
            tx.execute("DELETE FROM api_key WHERE prefix = ?1", params![prefix])?;
            let affected = Affected {
                api_keys: BTreeSet::from([prefix.clone()]),
                ..Affected::default()
            };
            let apply: Apply = Some(Box::new(move |st| {
                st.keys.remove(&prefix);
            }));
            Ok((apply, affected))
        })
    }

    /// The API keys of `principal`, by prefix.
    pub fn api_keys(&self, principal: &str) -> Vec<ApiKeyInfo> {
        let st = self.state.read();
        let mut out: Vec<ApiKeyInfo> = st
            .keys
            .iter()
            .filter(|(_, k)| k.principal == principal.trim())
            .map(|(prefix, k)| ApiKeyInfo {
                prefix: prefix.clone(),
                principal: k.principal.clone(),
                created_at: k.created_at,
                expires_at: k.expires_at,
                permissions: k.permissions,
            })
            .collect();
        out.sort_by(|a, b| a.prefix.cmp(&b.prefix));
        out
    }

    /// Checks an API key. A malformed, unknown, revoked or expired key, a wrong secret, and a
    /// disabled principal are refused.
    pub fn verify_api_key(&self, key: &str) -> Result<Authenticated, AuthError> {
        let (prefix, secret) = apikey::parse(key.trim()).ok_or(AuthError::Refused)?;
        let st = self.state.read();
        let k = st.keys.get(prefix).ok_or(AuthError::Refused)?;
        if !apikey::matches(&k.hash, secret) || !self.usable(&st, prefix, &k.principal) {
            return Err(AuthError::Refused);
        }
        Ok(Authenticated {
            principal: k.principal.clone(),
            api_key: Some(prefix.to_owned()),
        })
    }

    /// Whether the principal is enabled and, if a key is named, the key is its and unexpired.
    fn usable(&self, st: &State, key: &str, principal: &str) -> bool {
        let enabled = st.principals.get(principal).is_some_and(|p| !p.disabled);
        let key_ok = st.keys.get(key).is_some_and(|k| {
            k.principal == principal && k.expires_at.is_none_or(|t| (self.clock)() < t)
        });
        enabled && key_ok
    }

    pub fn create_group(&self, name: &str) -> Result<Affected, Error> {
        let name = names::name("group", name)?;
        self.affecting(|st, tx| {
            if st.groups.contains_key(&name) {
                return Err(Error::Exists {
                    what: "group",
                    name,
                });
            }
            tx.execute("INSERT INTO local_group (name) VALUES (?1)", params![name])?;
            let id = tx.last_insert_rowid();
            let apply: Apply = Some(Box::new(move |st| {
                st.groups.insert(
                    name,
                    GroupRec {
                        id,
                        permissions: PermissionSet::EMPTY,
                        terms: BTreeSet::new(),
                        members: BTreeSet::new(),
                    },
                );
            }));
            Ok((apply, Affected::default()))
        })
    }

    /// Deletes a group with its grants and memberships.
    pub fn delete_group(&self, name: &str) -> Result<Affected, Error> {
        let name = name.trim().to_owned();
        self.affecting(|st, tx| {
            let g = st.group(&name)?;
            tx.execute("DELETE FROM local_group WHERE id = ?1", params![g.id])?;
            let affected = st.affected_by_group(&name);
            let apply: Apply = Some(Box::new(move |st| {
                if let Some(g) = st.groups.remove(&name) {
                    for m in &g.members {
                        if let Some(p) = st.principals.get_mut(m) {
                            p.groups.remove(&name);
                        }
                    }
                }
            }));
            Ok((apply, affected))
        })
    }

    pub fn add_member(&self, group: &str, principal: &str) -> Result<Affected, Error> {
        self.membership(group, principal, true)
    }

    pub fn remove_member(&self, group: &str, principal: &str) -> Result<Affected, Error> {
        self.membership(group, principal, false)
    }

    fn membership(&self, group: &str, principal: &str, add: bool) -> Result<Affected, Error> {
        let (group, principal) = (group.trim().to_owned(), principal.trim().to_owned());
        self.affecting(|st, tx| {
            let g = st.group(&group)?;
            let p = st.principal(&principal)?;
            if g.members.contains(&principal) == add {
                return Ok((nothing(), Affected::default()));
            }
            let sql = if add {
                "INSERT INTO member (group_id, principal_id) VALUES (?1, ?2)"
            } else {
                "DELETE FROM member WHERE group_id = ?1 AND principal_id = ?2"
            };
            tx.execute(sql, params![g.id, p.id])?;
            let affected = only_principal(&principal);
            let apply: Apply = Some(Box::new(move |st| {
                let g = st.groups.get_mut(&group).expect("validated");
                let p = st.principals.get_mut(&principal).expect("validated");
                if add {
                    g.members.insert(principal);
                    p.groups.insert(group);
                } else {
                    g.members.remove(&principal);
                    p.groups.remove(&group);
                }
            }));
            Ok((apply, affected))
        })
    }

    pub fn group(&self, name: &str) -> Option<GroupInfo> {
        let st = self.state.read();
        st.groups
            .get(name.trim())
            .map(|g| group_info(name.trim(), g))
    }

    pub fn groups(&self) -> Vec<GroupInfo> {
        let st = self.state.read();
        st.groups.iter().map(|(n, g)| group_info(n, g)).collect()
    }

    /// Grants a term, trimmed. `public` and an empty term are refused.
    pub fn grant_term(&self, to: Grantee<'_>, term: &str) -> Result<Affected, Error> {
        self.term_grant(to, names::term(term)?, true)
    }

    pub fn revoke_term(&self, from: Grantee<'_>, term: &str) -> Result<Affected, Error> {
        self.term_grant(from, term.trim().to_owned(), false)
    }

    fn term_grant(&self, who: Grantee<'_>, term: String, grant: bool) -> Result<Affected, Error> {
        let owned = trimmed(who);
        let who = owned.as_grantee();
        self.affecting(|st, tx| {
            let (id, held, table, column) = match who {
                Grantee::Principal(n) => {
                    let p = st.principal(n)?;
                    (p.id, &p.terms, "principal_term", "principal_id")
                }
                Grantee::Group(n) => {
                    let g = st.group(n)?;
                    (g.id, &g.terms, "group_term", "group_id")
                }
            };
            if held.contains(&term) == grant {
                return Ok((nothing(), Affected::default()));
            }
            let sql = if grant {
                format!("INSERT INTO {table} ({column}, term) VALUES (?1, ?2)")
            } else {
                format!("DELETE FROM {table} WHERE {column} = ?1 AND term = ?2")
            };
            tx.execute(&sql, params![id, term])?;
            let affected = st.affected_by_grantee(&who);
            let owner = OwnedGrantee::from(who);
            let apply: Apply = Some(Box::new(move |st| {
                let held = match &owner {
                    OwnedGrantee::Principal(n) => {
                        &mut st.principals.get_mut(n).expect("validated").terms
                    }
                    OwnedGrantee::Group(n) => &mut st.groups.get_mut(n).expect("validated").terms,
                };
                if grant {
                    held.insert(term);
                } else {
                    held.remove(&term);
                }
            }));
            Ok((apply, affected))
        })
    }

    pub fn grant_permission(&self, to: Grantee<'_>, p: Permission) -> Result<Affected, Error> {
        self.permission_grant(to, p, true)
    }

    pub fn revoke_permission(&self, from: Grantee<'_>, p: Permission) -> Result<Affected, Error> {
        self.permission_grant(from, p, false)
    }

    fn permission_grant(
        &self,
        who: Grantee<'_>,
        permission: Permission,
        grant: bool,
    ) -> Result<Affected, Error> {
        let owned = trimmed(who);
        let who = owned.as_grantee();
        self.affecting(|st, tx| {
            let (id, held, table) = match who {
                Grantee::Principal(n) => {
                    let p = st.principal(n)?;
                    (p.id, p.permissions, "principal")
                }
                Grantee::Group(n) => {
                    let g = st.group(n)?;
                    (g.id, g.permissions, "local_group")
                }
            };
            if held.contains(permission) == grant {
                return Ok((nothing(), Affected::default()));
            }
            let mut next = held;
            if grant {
                next.insert(permission);
            } else {
                next.remove(permission);
            }
            tx.execute(
                &format!("UPDATE {table} SET permissions = ?1 WHERE id = ?2"),
                params![next.bits(), id],
            )?;
            let affected = st.affected_by_grantee(&who);
            let owner = OwnedGrantee::from(who);
            let apply: Apply = Some(Box::new(move |st| match &owner {
                OwnedGrantee::Principal(n) => {
                    st.principals.get_mut(n).expect("validated").permissions = next
                }
                OwnedGrantee::Group(n) => {
                    st.groups.get_mut(n).expect("validated").permissions = next
                }
            }));
            Ok((apply, affected))
        })
    }

    pub fn create_provider(&self, provider: &Provider) -> Result<Affected, Error> {
        let p = provider.validated(self.allow_insecure_jwks)?;
        self.affecting(|st, tx| {
            match st.providers.get(&p.name) {
                Some((_, true)) => return Err(Error::ReadOnly { provider: p.name }),
                Some((_, false)) => {
                    return Err(Error::Exists {
                        what: "provider",
                        name: p.name,
                    })
                }
                None => {}
            }
            write_provider(tx, &p)?;
            Ok((provider_apply(p), Affected::default()))
        })
    }

    /// Replaces a stored provider, claim rules and role mappings included.
    pub fn update_provider(&self, provider: &Provider) -> Result<Affected, Error> {
        let p = provider.validated(self.allow_insecure_jwks)?;
        self.affecting(|st, tx| {
            self.writable_provider(st, &p.name)?;
            tx.execute("DELETE FROM provider WHERE name = ?1", params![p.name])?;
            write_provider(tx, &p)?;
            let affected = only_provider(&p.name);
            Ok((provider_apply(p), affected))
        })
    }

    pub fn drop_provider(&self, name: &str) -> Result<Affected, Error> {
        let name = name.trim().to_owned();
        self.affecting(|st, tx| {
            self.writable_provider(st, &name)?;
            tx.execute("DELETE FROM provider WHERE name = ?1", params![name])?;
            let affected = only_provider(&name);
            let apply: Apply = Some(Box::new(move |st| {
                st.providers.remove(&name);
            }));
            Ok((apply, affected))
        })
    }

    fn writable_provider(&self, st: &State, name: &str) -> Result<(), Error> {
        match st.providers.get(name) {
            Some((_, true)) => Err(Error::ReadOnly {
                provider: name.to_owned(),
            }),
            Some((_, false)) => Ok(()),
            None => Err(Error::NotFound {
                what: "provider",
                name: name.to_owned(),
            }),
        }
    }

    pub fn provider(&self, name: &str) -> Option<ProviderInfo> {
        let st = self.state.read();
        st.providers.get(name.trim()).map(|(p, ro)| ProviderInfo {
            provider: p.clone(),
            read_only: *ro,
        })
    }

    /// Every provider, stored and configured, by name.
    pub fn providers(&self) -> Vec<ProviderInfo> {
        let st = self.state.read();
        st.providers
            .values()
            .map(|(p, ro)| ProviderInfo {
                provider: p.clone(),
                read_only: *ro,
            })
            .collect()
    }

    /// The terms and permissions a session for `principal` holds: those granted to it directly
    /// and through its groups. With `api_key`, the permissions are narrowed to the key's own
    /// where it has them. `None` when the principal is unknown or disabled, or the key is not the
    /// principal's or has expired.
    pub fn resolve(&self, principal: &str, api_key: Option<&str>) -> Option<Resolution> {
        let name = principal.trim();
        let st = self.state.read();
        let p = st.principals.get(name).filter(|p| !p.disabled)?;
        let mut out = Resolution {
            terms: p.terms.clone(),
            permissions: p.permissions,
            bypass: p.bypass,
            generation: st.generation,
        };
        st.add_group_grants(&p.groups, &mut out);
        if let Some(prefix) = api_key {
            if !self.usable(&st, prefix, name) {
                return None;
            }
            if let Some(narrow) = st.keys[prefix].permissions {
                out.permissions = out.permissions.intersection(narrow);
            }
        }
        Some(out)
    }

    /// The terms and permissions a session for an OIDC identity holds, from `claims` as accepted
    /// from a token of `provider`: the terms its claim rules produce, and the terms and
    /// permissions granted to each existing local group named by a role mapping whose claim
    /// holds the mapping's value. It never holds `bypass`. `None` when the provider is unknown.
    pub fn resolve_claims(&self, provider: &str, claims: &Value) -> Option<Resolution> {
        let st = self.state.read();
        let (p, _) = st.providers.get(provider.trim())?;
        let mapped = p.apply(claims);
        let mut out = Resolution {
            terms: mapped.terms,
            generation: st.generation,
            ..Resolution::default()
        };
        st.add_group_grants(&mapped.groups, &mut out);
        Some(out)
    }
}

fn only_provider(name: &str) -> Affected {
    Affected {
        providers: BTreeSet::from([name.to_owned()]),
        ..Affected::default()
    }
}

/// Validates the configured providers and adds them, refusing one declared twice.
fn add_config_providers(state: &mut State, options: &Options) -> Result<(), Error> {
    for p in &options.config_providers {
        let p = p.validated(options.allow_insecure_jwks)?;
        if state.providers.contains_key(&p.name) {
            return Err(Error::DeclaredTwice { provider: p.name });
        }
        state.providers.insert(p.name.clone(), (p, true));
    }
    Ok(())
}

fn write_provider(tx: &Transaction<'_>, p: &Provider) -> Result<(), Error> {
    tx.execute(
        "INSERT INTO provider (name, issuer, audience, jwks_url) VALUES (?1, ?2, ?3, ?4)",
        params![p.name, p.issuer, p.audience, p.jwks_url],
    )?;
    for (i, r) in p.rules.iter().enumerate() {
        tx.execute(
            "INSERT INTO claim_rule (provider, position, claim, template) \
             VALUES (?1, ?2, ?3, ?4)",
            params![p.name, i as i64, r.claim, r.template],
        )?;
    }
    for (i, m) in p.role_mappings.iter().enumerate() {
        tx.execute(
            "INSERT INTO role_mapping (provider, position, claim, value, local_group) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![p.name, i as i64, m.claim, m.value, m.group],
        )?;
    }
    Ok(())
}

fn provider_apply(p: Provider) -> Apply {
    Some(Box::new(move |st| {
        st.providers.insert(p.name.clone(), (p, false));
    }))
}

enum OwnedGrantee {
    Principal(String),
    Group(String),
}

impl OwnedGrantee {
    fn as_grantee(&self) -> Grantee<'_> {
        match self {
            OwnedGrantee::Principal(n) => Grantee::Principal(n),
            OwnedGrantee::Group(n) => Grantee::Group(n),
        }
    }
}

impl From<Grantee<'_>> for OwnedGrantee {
    fn from(g: Grantee<'_>) -> Self {
        match g {
            Grantee::Principal(n) => OwnedGrantee::Principal(n.to_owned()),
            Grantee::Group(n) => OwnedGrantee::Group(n.to_owned()),
        }
    }
}

fn trimmed(g: Grantee<'_>) -> OwnedGrantee {
    match g {
        Grantee::Principal(n) => OwnedGrantee::Principal(n.trim().to_owned()),
        Grantee::Group(n) => OwnedGrantee::Group(n.trim().to_owned()),
    }
}

fn principal_info(name: &str, p: &PrincipalRec) -> PrincipalInfo {
    PrincipalInfo {
        name: name.to_owned(),
        kind: p.kind,
        disabled: p.disabled,
        bypass: p.bypass,
        has_password: p.password.is_some(),
        terms: p.terms.clone(),
        permissions: p.permissions,
        groups: p.groups.clone(),
    }
}

fn group_info(name: &str, g: &GroupRec) -> GroupInfo {
    GroupInfo {
        name: name.to_owned(),
        terms: g.terms.clone(),
        permissions: g.permissions,
        members: g.members.clone(),
    }
}

#[cfg(test)]
mod tests;
