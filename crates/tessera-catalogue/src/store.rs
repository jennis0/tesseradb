//! The SQLite file: creating it, refusing one this build cannot read, and loading it whole.
//!
//! The schema version is SQLite's `user_version`. A file at version 0 with no tables is new and
//! receives the schema; any other version but [`SCHEMA_VERSION`] is refused.

use std::fs::{DirBuilder, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::catalogue::{GroupRec, KeyRec, PrincipalKind, PrincipalRec, State};
use crate::permission::PermissionSet;
use crate::provider::{ClaimRule, Provider, RuleTarget};
use crate::Error;

pub(crate) const SCHEMA_VERSION: i64 = 1;
pub(crate) const FILE_NAME: &str = "catalogue.sqlite";

const SCHEMA: &str = "
CREATE TABLE principal (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('person', 'service')),
    disabled INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0, 1)),
    bypass INTEGER NOT NULL DEFAULT 0 CHECK (bypass IN (0, 1)),
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
    target TEXT NOT NULL CHECK (target IN ('term', 'group')),
    template TEXT NOT NULL,
    PRIMARY KEY (provider, position)
) WITHOUT ROWID;
";

/// Opens the catalogue in `dir`, creating the directory (mode 0700) and the file (mode 0600)
/// when they do not exist.
pub(crate) fn open(dir: &Path) -> Result<Connection, Error> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let path = dir.join(FILE_NAME);
    // SQLite gives its journal the database file's mode, so creating the file first covers both.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let integrity: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(Error::Storage(format!(
            "{} fails SQLite's integrity check: {integrity}",
            path.display()
        )));
    }
    // `load` relies on every reference resolving.
    let dangling: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if dangling != 0 {
        return Err(Error::Storage(format!(
            "{} holds {dangling} rows that name a missing principal, group or provider",
            path.display()
        )));
    }
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
    match version {
        0 if tables == 0 => {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?;
        }
        SCHEMA_VERSION => {}
        found => {
            return Err(Error::Version {
                found,
                supported: SCHEMA_VERSION,
            })
        }
    }
    Ok(conn)
}

/// Reads the whole catalogue into memory.
pub(crate) fn load(conn: &Connection) -> Result<State, Error> {
    let corrupt = |what: String| Error::Storage(format!("the catalogue holds {what}"));
    let perms = |bits: i64| {
        PermissionSet::from_bits(bits)
            .ok_or_else(|| corrupt(format!("an unknown permission set {bits}")))
    };
    let mut st = State::default();
    let mut principal_names = std::collections::HashMap::new();
    let mut group_names = std::collections::HashMap::new();

    let mut q = conn.prepare(
        "SELECT id, name, kind, disabled, bypass, permissions, password_hash FROM principal",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let name: String = r.get(1)?;
        let kind: String = r.get(2)?;
        let kind = PrincipalKind::parse(&kind)
            .ok_or_else(|| corrupt(format!("an unknown principal kind `{kind}`")))?;
        principal_names.insert(id, name.clone());
        st.principals.insert(
            name,
            PrincipalRec {
                id,
                kind,
                disabled: r.get(3)?,
                bypass: r.get(4)?,
                permissions: perms(r.get(5)?)?,
                password: r.get(6)?,
                terms: Default::default(),
                groups: Default::default(),
            },
        );
    }

    let mut q = conn.prepare("SELECT id, name, permissions FROM local_group")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let name: String = r.get(1)?;
        group_names.insert(id, name.clone());
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

    let mut q = conn.prepare("SELECT group_id, principal_id FROM member")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let (g, p) = (
            &group_names[&r.get::<_, i64>(0)?],
            &principal_names[&r.get::<_, i64>(1)?],
        );
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

    let mut q = conn.prepare("SELECT principal_id, term FROM principal_term")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let p = &principal_names[&r.get::<_, i64>(0)?];
        st.principals
            .get_mut(p)
            .expect("loaded")
            .terms
            .insert(r.get(1)?);
    }

    let mut q = conn.prepare("SELECT group_id, term FROM group_term")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let g = &group_names[&r.get::<_, i64>(0)?];
        st.groups
            .get_mut(g)
            .expect("loaded")
            .terms
            .insert(r.get(1)?);
    }

    let mut q = conn.prepare(
        "SELECT prefix, principal_id, secret_sha256, created_at, expires_at, permissions \
         FROM api_key",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let prefix: String = r.get(0)?;
        let hash: Vec<u8> = r.get(2)?;
        let permissions: Option<i64> = r.get(5)?;
        st.keys.insert(
            prefix,
            KeyRec {
                principal: principal_names[&r.get::<_, i64>(1)?].clone(),
                hash: hash
                    .try_into()
                    .map_err(|_| corrupt("an API key hash that is not 32 bytes".into()))?,
                created_at: r.get::<_, i64>(3)? as u64,
                expires_at: r.get::<_, Option<i64>>(4)?.map(|t| t as u64),
                permissions: permissions.map(perms).transpose()?,
            },
        );
    }

    let mut q = conn.prepare("SELECT name, issuer, audience, jwks_url FROM provider")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(0)?;
        st.providers.insert(
            name.clone(),
            (
                Provider {
                    name,
                    issuer: r.get(1)?,
                    audience: r.get(2)?,
                    jwks_url: r.get(3)?,
                    rules: Vec::new(),
                },
                false,
            ),
        );
    }

    let mut q = conn.prepare(
        "SELECT provider, claim, target, template FROM claim_rule ORDER BY provider, position",
    )?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let provider: String = r.get(0)?;
        let target: String = r.get(2)?;
        let template: String = r.get(3)?;
        let target = match target.as_str() {
            "term" => RuleTarget::Term(template),
            _ => RuleTarget::LocalGroup(template),
        };
        st.providers
            .get_mut(&provider)
            .expect("loaded")
            .0
            .rules
            .push(ClaimRule {
                claim: r.get(1)?,
                target,
            });
    }
    Ok(st)
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
    fn a_catalogue_of_another_schema_version_is_refused() {
        for version in [SCHEMA_VERSION + 1, 99, -1] {
            let fx = Fixture::new();
            drop(fx.open());
            fx.raw()
                .pragma_update(None, "user_version", version)
                .unwrap();
            let refused = Catalogue::open(fx.dir.path(), fx.options()).err();
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
            Catalogue::open(fx.dir.path(), fx.options()),
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
        cat.set_password("ada", "correct horse").unwrap();
        cat.disable_principal("gone").unwrap();
        cat.set_bypass("ingest", true).unwrap();
        cat.grant_permission(Grantee::Principal("ingest"), Permission::Write)
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
        cat.create_provider(&Provider {
            name: "corp".into(),
            issuer: "https://login.example.org/".into(),
            audience: "tessera".into(),
            jwks_url: "https://login.example.org/keys".into(),
            rules: vec![
                ClaimRule {
                    claim: "groups[*]".into(),
                    target: RuleTarget::Term("group:{value}".into()),
                },
                ClaimRule {
                    claim: "tid".into(),
                    target: RuleTarget::LocalGroup("tenant-{value}".into()),
                },
            ],
        })
        .unwrap();

        let before = (
            cat.principals(),
            cat.groups(),
            cat.api_keys("ingest"),
            cat.providers(),
            cat.resolve("ada", None),
            cat.resolve("ingest", Some(&key.prefix)),
        );
        drop(cat);
        let cat = fx.open();
        let after = (
            cat.principals(),
            cat.groups(),
            cat.api_keys("ingest"),
            cat.providers(),
            cat.resolve("ada", None),
            cat.resolve("ingest", Some(&key.prefix)),
        );
        assert_eq!(before, after);
        assert!(cat.verify_password("ada", "correct horse").is_ok());
        assert!(cat.verify_api_key(&key.key).is_ok());
    }
}
