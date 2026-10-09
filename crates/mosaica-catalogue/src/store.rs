//! The SQLite file: creating it, refusing one this build cannot read, and loading it whole.
//!
//! The schema version is SQLite's `user_version`. A file at version 0 with no tables is new and
//! receives the schema; any other version but [`SCHEMA_VERSION`] is refused.
//!
//! One `Catalogue` at a time holds a catalogue open. It takes an exclusive `flock` on a lock file
//! beside the database. The lock belongs to the open file, so a second open in the same process
//! is refused as one in another process is. SQLite's own locks are per process and would admit
//! the second.
//!
//! Loading applies the rules that a write applies to every name and term, and refuses a
//! catalogue holding a row that breaks one.

use std::collections::HashMap;
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::catalogue::{GroupRec, KeyRec, PrincipalKind, PrincipalRec, State};
use crate::names;
use crate::permission::PermissionSet;
use crate::provider::{ClaimRule, Provider, RoleMapping};
use crate::Error;

pub(crate) const SCHEMA_VERSION: i64 = 6;
pub(crate) const FILE_NAME: &str = "catalogue.sqlite";
const LOCK_NAME: &str = "catalogue.lock";

const SCHEMA: &str = "
CREATE TABLE principal (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('person', 'service')),
    disabled INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0, 1)),
    permissions INTEGER NOT NULL DEFAULT 0,
    password_hash TEXT
);
CREATE TABLE local_group (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    permissions INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE member (
    group_id INTEGER NOT NULL REFERENCES local_group (id) ON DELETE CASCADE,
    principal_id INTEGER NOT NULL REFERENCES principal (id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, principal_id)
) WITHOUT ROWID;
CREATE TABLE principal_term (
    principal_id INTEGER NOT NULL REFERENCES principal (id) ON DELETE CASCADE,
    term TEXT NOT NULL,
    PRIMARY KEY (principal_id, term)
) WITHOUT ROWID;
CREATE TABLE group_term (
    group_id INTEGER NOT NULL REFERENCES local_group (id) ON DELETE CASCADE,
    term TEXT NOT NULL,
    PRIMARY KEY (group_id, term)
) WITHOUT ROWID;
CREATE TABLE api_key (
    prefix TEXT PRIMARY KEY,
    principal_id INTEGER NOT NULL REFERENCES principal (id) ON DELETE CASCADE,
    secret_sha256 BLOB NOT NULL CHECK (length(secret_sha256) = 32),
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    permissions INTEGER
) WITHOUT ROWID;
CREATE TABLE provider (
    name TEXT PRIMARY KEY,
    issuer TEXT NOT NULL,
    audience TEXT NOT NULL,
    jwks_url TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE claim_rule (
    provider TEXT NOT NULL REFERENCES provider (name) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    claim TEXT NOT NULL,
    template TEXT NOT NULL,
    PRIMARY KEY (provider, position)
) WITHOUT ROWID;
CREATE TABLE role_mapping (
    provider TEXT NOT NULL REFERENCES provider (name) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    claim TEXT NOT NULL,
    value TEXT NOT NULL,
    local_group TEXT NOT NULL,
    PRIMARY KEY (provider, position)
) WITHOUT ROWID;
CREATE TABLE generation (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    value INTEGER NOT NULL CHECK (value >= 0)
);
INSERT INTO generation (id, value) VALUES (0, 0);
";

/// Opens the catalogue in `dir`, creating the directory (mode 0700) and the file (mode 0600)
/// when they do not exist, and locks it. Returns the connection and the locked file, which holds
/// the lock until it is dropped. A directory or file that others can reach is refused.
pub(crate) fn open(dir: &Path) -> Result<(Connection, File), Error> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    owner_only(dir, 0o700)?;
    let lock = lock(dir)?;
    let path = dir.join(FILE_NAME);
    create_owner_only(&path)?;
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "foreign_keys", true)?;
    check_integrity(&conn, &path)?;
    create_or_check_schema(&conn)?;
    Ok((conn, lock))
}

/// Takes the exclusive lock on `dir`'s lock file, which is held until the file is dropped.
fn lock(dir: &Path) -> Result<File, Error> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(LOCK_NAME))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(Error::Locked {
            dir: dir.display().to_string(),
        }),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Creates the database file with mode 0600 when it does not exist, and refuses one others can
/// reach. SQLite gives its journal the database file's mode, so this covers both.
fn create_owner_only(path: &Path) -> Result<(), Error> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    owner_only(path, 0o600)
}

/// Refuses a file that fails SQLite's integrity check or holds a reference that does not resolve,
/// which `load` relies on.
fn check_integrity(conn: &Connection, path: &Path) -> Result<(), Error> {
    let integrity: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(Error::Corrupt(format!(
            "{} fails SQLite's integrity check: {integrity}",
            path.display()
        )));
    }
    let dangling: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if dangling != 0 {
        return Err(Error::Corrupt(format!(
            "{} holds {dangling} rows that name a missing principal, group or provider",
            path.display()
        )));
    }
    Ok(())
}

/// Creates the schema in an empty file, and refuses a file written with another schema version.
fn create_or_check_schema(conn: &Connection) -> Result<(), Error> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
    match version {
        0 if tables == 0 => {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?;
            Ok(())
        }
        SCHEMA_VERSION => Ok(()),
        found => Err(Error::Version {
            found,
            supported: SCHEMA_VERSION,
        }),
    }
}

/// Refuses `path` when its mode grants anything outside `want`.
fn owner_only(path: &Path, want: u32) -> Result<(), Error> {
    let found = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if found & !want != 0 {
        return Err(Error::Mode {
            path: path.display().to_string(),
            found,
            want,
        });
    }
    Ok(())
}

/// Refuses a stored name or term that a write would not store as it is.
fn stored(row: &str, value: &str, rule: Result<String, Error>) -> Result<(), Error> {
    match rule {
        Ok(v) if v == value => Ok(()),
        Ok(_) => Err(Error::Corrupt(format!(
            "{row} holds `{value}`, which has leading or trailing white space"
        ))),
        Err(e) => Err(Error::Corrupt(format!("{row} holds `{value}`: {e}"))),
    }
}

fn corrupt(what: String) -> Error {
    Error::Corrupt(format!("the catalogue holds {what}"))
}

fn perms(bits: i64) -> Result<PermissionSet, Error> {
    PermissionSet::from_bits(bits)
        .ok_or_else(|| corrupt(format!("an unknown permission set {bits}")))
}

/// The row ids of loaded principals and groups, which the tables that refer to them name.
#[derive(Default)]
struct Ids {
    principals: HashMap<i64, String>,
    groups: HashMap<i64, String>,
}

/// Reads the whole catalogue into memory. A stored provider's JWKS URL takes the rule a write
/// applies with `allow_insecure_jwks`.
pub(crate) fn load(conn: &Connection, allow_insecure_jwks: bool) -> Result<State, Error> {
    let mut st = State::default();
    let mut ids = Ids::default();
    load_principals(conn, &mut st, &mut ids)?;
    load_groups(conn, &mut st, &mut ids)?;
    load_members(conn, &mut st, &ids)?;
    load_principal_terms(conn, &mut st, &ids)?;
    load_group_terms(conn, &mut st, &ids)?;
    load_keys(conn, &mut st, &ids)?;
    load_providers(conn, &mut st, allow_insecure_jwks)?;
    st.generation = load_generation(conn)?;
    Ok(st)
}

fn load_principals(conn: &Connection, st: &mut State, ids: &mut Ids) -> Result<(), Error> {
    let mut q = conn.prepare(
        "SELECT id, name, kind, disabled, permissions, password_hash FROM principal",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let (name, rec) = principal_row(r)?;
        ids.principals.insert(rec.id, name.clone());
        st.principals.insert(name, rec);
    }
    Ok(())
}

fn principal_row(r: &rusqlite::Row<'_>) -> Result<(String, PrincipalRec), Error> {
    let id: i64 = r.get(0)?;
    let name: String = r.get(1)?;
    let kind: String = r.get(2)?;
    let kind = PrincipalKind::parse(&kind)
        .ok_or_else(|| corrupt(format!("an unknown principal kind `{kind}`")))?;
    stored(
        &format!("principal row {id}"),
        &name,
        names::name("principal", &name),
    )?;
    let rec = PrincipalRec {
        id,
        kind,
        disabled: r.get(3)?,
        permissions: perms(r.get(4)?)?,
        password: r.get(5)?,
        terms: Default::default(),
        groups: Default::default(),
    };
    Ok((name, rec))
}

fn load_groups(conn: &Connection, st: &mut State, ids: &mut Ids) -> Result<(), Error> {
    let mut q = conn.prepare("SELECT id, name, permissions FROM local_group")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let name: String = r.get(1)?;
        stored(
            &format!("local_group row {id}"),
            &name,
            names::name("group", &name),
        )?;
        ids.groups.insert(id, name.clone());
        st.groups.insert(
            name,
            GroupRec {
                id,
                permissions: perms(r.get(2)?)?,
                terms: Default::default(),
                members: Default::default(),
            },
        );
    }
    Ok(())
}

fn load_members(conn: &Connection, st: &mut State, ids: &Ids) -> Result<(), Error> {
    let mut q = conn.prepare("SELECT group_id, principal_id FROM member")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let g = &ids.groups[&r.get::<_, i64>(0)?];
        let p = &ids.principals[&r.get::<_, i64>(1)?];
        st.groups
            .get_mut(g)
            .expect("loaded")
            .members
            .insert(p.clone());
        st.principals
            .get_mut(p)
            .expect("loaded")
            .groups
            .insert(g.clone());
    }
    Ok(())
}

fn load_principal_terms(conn: &Connection, st: &mut State, ids: &Ids) -> Result<(), Error> {
    let mut q = conn.prepare("SELECT principal_id, term FROM principal_term")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let p = &ids.principals[&r.get::<_, i64>(0)?];
        let term: String = r.get(1)?;
        let row = format!("principal_term row for principal `{p}`");
        stored(&row, &term, names::term(&term))?;
        st.principals.get_mut(p).expect("loaded").terms.insert(term);
    }
    Ok(())
}

fn load_group_terms(conn: &Connection, st: &mut State, ids: &Ids) -> Result<(), Error> {
    let mut q = conn.prepare("SELECT group_id, term FROM group_term")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let g = &ids.groups[&r.get::<_, i64>(0)?];
        let term: String = r.get(1)?;
        let row = format!("group_term row for group `{g}`");
        stored(&row, &term, names::term(&term))?;
        st.groups.get_mut(g).expect("loaded").terms.insert(term);
    }
    Ok(())
}

fn load_keys(conn: &Connection, st: &mut State, ids: &Ids) -> Result<(), Error> {
    let mut q = conn.prepare(
        "SELECT prefix, principal_id, secret_sha256, created_at, expires_at, permissions \
         FROM api_key",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let prefix: String = r.get(0)?;
        st.keys.insert(prefix, key_row(r, ids)?);
    }
    Ok(())
}

fn key_row(r: &rusqlite::Row<'_>, ids: &Ids) -> Result<KeyRec, Error> {
    let hash: Vec<u8> = r.get(2)?;
    let permissions: Option<i64> = r.get(5)?;
    Ok(KeyRec {
        principal: ids.principals[&r.get::<_, i64>(1)?].clone(),
        hash: hash
            .try_into()
            .map_err(|_| corrupt("an API key hash that is not 32 bytes".into()))?,
        created_at: time(r.get(3)?)?,
        expires_at: r.get::<_, Option<i64>>(4)?.map(time).transpose()?,
        permissions: permissions.map(perms).transpose()?,
    })
}

/// A stored time in seconds since the epoch, which a write never stores negative.
fn time(t: i64) -> Result<u64, Error> {
    u64::try_from(t).map_err(|_| corrupt(format!("an API key time {t} before 1970")))
}

fn load_providers(
    conn: &Connection,
    st: &mut State,
    allow_insecure_jwks: bool,
) -> Result<(), Error> {
    let mut q = conn.prepare("SELECT name, issuer, audience, jwks_url FROM provider")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(0)?;
        let provider = Provider {
            name: name.clone(),
            issuer: r.get(1)?,
            audience: r.get(2)?,
            jwks_url: r.get(3)?,
            rules: Vec::new(),
            role_mappings: Vec::new(),
        };
        st.providers.insert(name, (provider, false));
    }
    load_claim_rules(conn, st)?;
    load_role_mappings(conn, st)?;
    st.providers.values().try_for_each(|(p, _)| {
        check_provider(p, allow_insecure_jwks)?;
        st.check_issuer_audience(p)
            .map_err(|e| Error::Corrupt(format!("provider row `{}` breaks a rule: {e}", p.name)))
    })
}

fn load_claim_rules(conn: &Connection, st: &mut State) -> Result<(), Error> {
    let mut q = conn.prepare(
        "SELECT provider, position, claim, template FROM claim_rule ORDER BY provider, position",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let (provider, row) = provider_row(r, "claim_rule")?;
        let rule = ClaimRule {
            claim: text(r, 2, &row)?,
            template: text(r, 3, &row)?,
        };
        provider_mut(st, &provider).rules.push(rule);
    }
    Ok(())
}

fn load_role_mappings(conn: &Connection, st: &mut State) -> Result<(), Error> {
    let mut q = conn.prepare(
        "SELECT provider, position, claim, value, local_group FROM role_mapping \
         ORDER BY provider, position",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let (provider, row) = provider_row(r, "role_mapping")?;
        let mapping = RoleMapping {
            claim: text(r, 2, &row)?,
            value: text(r, 3, &row)?,
            group: text(r, 4, &row)?,
        };
        provider_mut(st, &provider).role_mappings.push(mapping);
    }
    Ok(())
}

/// The provider a rule or mapping row belongs to, and a description of the row for errors.
fn provider_row(r: &rusqlite::Row<'_>, table: &str) -> Result<(String, String), Error> {
    let provider: String = r.get(0)?;
    let position: i64 = r.get(1)?;
    Ok((
        provider.clone(),
        format!("{table} row {position} of provider `{provider}`"),
    ))
}

fn provider_mut<'a>(st: &'a mut State, name: &str) -> &'a mut Provider {
    &mut st.providers.get_mut(name).expect("loaded").0
}

/// Column `i` of `r` as text, refusing a value of another type, which no write stores.
fn text(r: &rusqlite::Row<'_>, i: usize, row: &str) -> Result<String, Error> {
    r.get(i).map_err(|e| match e {
        rusqlite::Error::InvalidColumnType(..) => {
            Error::Corrupt(format!("{row} holds a value that is not text"))
        }
        e => e.into(),
    })
}

/// Refuses a stored provider that a write would not store as it is.
fn check_provider(p: &Provider, allow_insecure_jwks: bool) -> Result<(), Error> {
    match p.validated(allow_insecure_jwks) {
        Ok(v) if v == *p => Ok(()),
        Ok(_) => Err(Error::Corrupt(format!(
            "provider row `{}` or one of its claim_rule or role_mapping rows holds a value with \
             leading or trailing white space",
            p.name
        ))),
        Err(e @ Error::InsecureJwks { .. }) => Err(e),
        Err(e) => Err(Error::Corrupt(format!(
            "provider row `{}` or one of its claim_rule or role_mapping rows breaks a rule: \
             {e}",
            p.name
        ))),
    }
}

fn load_generation(conn: &Connection) -> Result<u64, Error> {
    let generation: i64 = conn
        .query_row("SELECT value FROM generation WHERE id = 0", [], |r| {
            r.get(0)
        })
        .map_err(|_| corrupt("no generation row".into()))?;
    u64::try_from(generation).map_err(|_| corrupt(format!("generation {generation}")))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testing::Fixture;
    use crate::{Catalogue, Grantee, Permission, PrincipalKind};

    #[test]
    fn the_file_is_created_readable_by_its_owner_alone() {
        let fx = Fixture::new();
        let dir = fx.dir.path().join("nested").join("catalogue");
        let cat = Catalogue::open(&dir, fx.options()).unwrap();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        let mode = std::fs::metadata(dir.join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
    }

    #[test]
    fn a_catalogue_opens_once_at_a_time() {
        let fx = Fixture::new();
        let cat = fx.open();
        assert!(matches!(
            Catalogue::open(&fx.path(), fx.options()),
            Err(Error::Locked { .. })
        ));
        let other = std::thread::spawn({
            let (path, options) = (fx.path(), fx.options());
            move || Catalogue::open(&path, options).err()
        });
        assert!(matches!(other.join().unwrap(), Some(Error::Locked { .. })));
        drop(cat);
        fx.open();
    }

    #[test]
    fn a_directory_or_file_others_can_reach_is_refused() {
        let chmod = |path: &std::path::Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        let fx = Fixture::new();
        drop(fx.open());
        let file = fx.path().join(FILE_NAME);
        for (path, wide, narrow) in [(fx.path(), 0o750, 0o700), (file, 0o604, 0o600)] {
            chmod(&path, wide);
            assert!(matches!(
                Catalogue::open(&fx.path(), fx.options()),
                Err(Error::Mode { .. })
            ));
            chmod(&path, narrow);
            drop(fx.open());
        }
    }

    #[test]
    fn a_stored_name_or_term_that_a_write_would_refuse_is_refused_on_open() {
        let rows = [
            "INSERT INTO principal (name, kind) VALUES ('  ', 'person')",
            "INSERT INTO principal (name, kind) VALUES (' ada', 'person')",
            "INSERT INTO local_group (name) VALUES ('')",
            "INSERT INTO principal (id, name, kind) VALUES (7, 'ada', 'person'); \
             INSERT INTO principal_term VALUES (7, 'public')",
            "INSERT INTO principal (id, name, kind) VALUES (7, 'ada', 'person'); \
             INSERT INTO principal_term VALUES (7, 'x ')",
            "INSERT INTO local_group (id, name) VALUES (7, 'eu'); \
             INSERT INTO group_term VALUES (7, 'public')",
            "INSERT INTO provider VALUES ('corp', 'https://login.example.org/', 'mosaica', \
             'keys')",
            "INSERT INTO provider VALUES ('corp', 'https://login.example.org/', 'mosaica', \
             'https://login.example.org/keys'); \
             INSERT INTO claim_rule VALUES ('corp', 0, 'groups[*]', 'public')",
            "DELETE FROM generation",
            "INSERT INTO principal (id, name, kind) VALUES (7, 'ada', 'person'); \
             INSERT INTO api_key VALUES ('abcd', 7, zeroblob(32), -1, NULL, NULL)",
            "INSERT INTO principal (id, name, kind) VALUES (7, 'ada', 'person'); \
             INSERT INTO api_key VALUES ('abcd', 7, zeroblob(32), 0, -1, NULL)",
            "PRAGMA foreign_keys = OFF; INSERT INTO member VALUES (7, 8)",
            "INSERT INTO principal (name, kind) VALUES ('a' || char(7) || 'b', 'person')",
            "INSERT INTO local_group (name) VALUES ('e' || char(10) || 'u')",
            "INSERT INTO principal (name, kind) VALUES ('ad' || char(8238) || 'a', 'person')",
            "INSERT INTO local_group (name) VALUES ('e' || char(8203) || 'u')",
            "INSERT INTO provider VALUES ('co' || char(8294) || 'rp', \
             'https://login.example.org/', 'mosaica', 'https://login.example.org/keys')",
            "INSERT INTO local_group (id, name) VALUES (7, 'eu'); \
             INSERT INTO group_term VALUES (7, 'PUBLIC')",
            "INSERT INTO provider VALUES ('co' || char(27) || 'rp', 'https://login.example.org/', \
             'mosaica', 'https://login.example.org/keys')",
        ];
        for row in rows {
            let fx = Fixture::new();
            drop(fx.open());
            fx.raw().execute_batch(row).unwrap();
            assert!(
                matches!(
                    Catalogue::open(&fx.path(), fx.options()),
                    Err(Error::Corrupt(_))
                ),
                "{row}"
            );
        }
    }

    #[test]
    fn a_stored_claim_rule_or_role_mapping_that_a_write_would_refuse_is_refused_on_open() {
        let provider = "INSERT INTO provider VALUES ('corp', 'https://login.example.org/', \
                        'mosaica', 'https://login.example.org/keys');";
        for row in [
            "claim_rule VALUES ('corp', 0, 'groups[*]', '{value}-{value}')",
            "claim_rule VALUES ('corp', 0, 'groups[*]', ' {value}')",
            "claim_rule VALUES ('corp', 0, 'a..b', '{value}')",
            "claim_rule VALUES ('corp', 0, 'groups[*]', x'7b76616c75657d')",
            "role_mapping VALUES ('corp', 0, 'groups[*]', ' mosaica-admins', 'admins')",
            "role_mapping VALUES ('corp', 0, 'groups[*]', 'mosaica-admins', ' admins')",
            "role_mapping VALUES ('corp', 0, 'groups[*]', 'mosaica' || char(7), 'admins')",
            "role_mapping VALUES ('corp', 0, 'groups[*]', '', 'admins')",
            "role_mapping VALUES ('corp', 0, 'a..b', 'mosaica-admins', 'admins')",
            "role_mapping VALUES ('corp', 0, 'groups[*]', 'mosaica-admins', \
             'ad' || char(8238) || 'mins')",
            "role_mapping VALUES ('corp', 0, 'groups[*]', 'mosaica-admins', '')",
            "role_mapping VALUES ('corp', 0, 'groups[*]', x'61', 'admins')",
        ] {
            let fx = Fixture::new();
            drop(fx.open());
            let sql = format!("{provider} INSERT INTO {row}");
            fx.raw().execute_batch(&sql).unwrap();
            assert!(
                matches!(
                    Catalogue::open(&fx.path(), fx.options()),
                    Err(Error::Corrupt(_))
                ),
                "{row}"
            );
        }
    }

    #[test]
    fn a_file_that_is_not_a_sound_sqlite_database_is_refused() {
        let fx = Fixture::new();
        drop(fx.open());
        let file = fx.path().join(FILE_NAME);
        let mut bytes = std::fs::read(&file).unwrap();
        let half = bytes.len() / 2;
        bytes[half..].fill(0xa5);
        std::fs::write(&file, bytes).unwrap();
        assert!(matches!(
            Catalogue::open(&fx.path(), fx.options()),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn a_catalogue_of_another_schema_version_is_refused() {
        for version in [SCHEMA_VERSION + 1, 99, -1] {
            let fx = Fixture::new();
            drop(fx.open());
            fx.raw()
                .pragma_update(None, "user_version", version)
                .unwrap();
            let refused = Catalogue::open(&fx.path(), fx.options()).err();
            assert_eq!(
                refused,
                Some(Error::Version {
                    found: version,
                    supported: SCHEMA_VERSION
                })
            );
        }
    }

    #[test]
    fn a_database_that_is_not_a_catalogue_is_refused() {
        let fx = Fixture::new();
        fx.raw().execute_batch("CREATE TABLE t (x)").unwrap();
        assert!(matches!(
            Catalogue::open(&fx.path(), fx.options()),
            Err(Error::Version { found: 0, .. })
        ));
    }

    #[test]
    fn everything_stored_survives_closing_and_reopening() {
        let fx = Fixture::new();
        let cat = fx.open();
        cat.create_principal("ada", PrincipalKind::Person).unwrap();
        cat.create_principal("ingest", PrincipalKind::Service)
            .unwrap();
        cat.create_principal("gone", PrincipalKind::Person).unwrap();
        cat.set_password("ada", "correct horse battery").unwrap();
        cat.disable_principal("gone").unwrap();
        cat.grant_permission(Grantee::Principal("ingest"), Permission::Write)
            .unwrap();
        cat.grant_permission(Grantee::Principal("ingest"), Permission::WriteAll)
            .unwrap();
        cat.grant_permission(Grantee::Principal("ingest"), Permission::ReadAll)
            .unwrap();
        cat.grant_term(Grantee::Principal("ada"), "secret").unwrap();
        cat.create_group("eu").unwrap();
        cat.add_member("eu", "ada").unwrap();
        cat.grant_term(Grantee::Group("eu"), "region:eu").unwrap();
        cat.grant_permission(Grantee::Group("eu"), Permission::Read)
            .unwrap();
        let (key, _) = cat
            .create_api_key(
                "ingest",
                Some(fx.now() + 3600),
                Some([Permission::Write].into_iter().collect()),
            )
            .unwrap();
        let snapshot = |cat: &Catalogue| {
            (
                cat.principals(),
                cat.groups(),
                cat.api_keys("ingest"),
                cat.providers(),
                cat.resolve("ada", None),
                cat.resolve("ingest", Some(&key.prefix)),
            )
        };
        let before = snapshot(&cat);
        drop(cat);
        let cat = fx.open();
        assert_eq!(before, snapshot(&cat));
        assert!(cat.verify_password("ada", "correct horse battery").is_ok());
        assert!(cat.verify_api_key(&key.key).is_ok());
    }

    #[test]
    fn a_provider_with_its_rules_and_role_mappings_survives_closing_and_reopening() {
        let fx = Fixture::new();
        let cat = fx.open();
        cat.create_group("eu").unwrap();
        cat.grant_term(Grantee::Group("eu"), "region:eu").unwrap();
        cat.grant_permission(Grantee::Group("eu"), Permission::Read)
            .unwrap();
        cat.create_provider(&Provider {
            name: "corp".into(),
            issuer: "https://login.example.org/".into(),
            audience: "mosaica".into(),
            jwks_url: "https://login.example.org/keys".into(),
            rules: vec![
                ClaimRule {
                    claim: "groups[*]".into(),
                    template: "{value}".into(),
                },
                ClaimRule {
                    claim: "tid".into(),
                    template: "tenant:{value}".into(),
                },
            ],
            role_mappings: vec![
                RoleMapping {
                    claim: "groups[*]".into(),
                    value: "mosaica-admins".into(),
                    group: "eu".into(),
                },
                RoleMapping {
                    claim: "tid".into(),
                    value: "7f3a".into(),
                    group: "eu".into(),
                },
            ],
        })
        .unwrap();

        let claims = serde_json::json!({"groups": ["mosaica-admins"], "tid": "7f3a"});
        let before = (cat.providers(), cat.resolve_claims("corp", &claims));
        drop(cat);
        let cat = fx.open();
        assert_eq!(
            before,
            (cat.providers(), cat.resolve_claims("corp", &claims))
        );
        let r = before.1.unwrap();
        assert_eq!(r.permissions, [Permission::Read].into_iter().collect());
    }
}
