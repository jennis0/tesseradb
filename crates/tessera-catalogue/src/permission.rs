//! The four permissions over the whole database. Each is granted on its own: `admin` implies
//! neither `read` nor `write`, so an account that manages users can be one that sees nothing.

use std::fmt;

use crate::Error;

/// What a principal may do. Terms, which say what it may see, are separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    /// Authorising a session for itself, and every viewer request made with that session.
    Read,
    /// Insert, delete, suppress, annotate and declare, masked by the principal's own terms.
    Write,
    /// Authorising a session for another principal.
    AuthoriseAs,
    /// Every change to the catalogue.
    Admin,
}

impl Permission {
    pub const ALL: [Permission; 4] = [
        Permission::Read,
        Permission::Write,
        Permission::AuthoriseAs,
        Permission::Admin,
    ];

    /// The name used on every surface.
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Write => "write",
            Permission::AuthoriseAs => "authorise-as",
            Permission::Admin => "admin",
        }
    }

    /// Reads a permission by name, trimmed.
    pub fn parse(name: &str) -> Result<Permission, Error> {
        let name = name.trim();
        Permission::ALL
            .into_iter()
            .find(|p| p.as_str() == name)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "`{name}` is not a permission; write one of read, write, authorise-as, admin"
                ))
            })
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of permissions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PermissionSet(u8);

impl PermissionSet {
    pub const EMPTY: PermissionSet = PermissionSet(0);

    pub fn contains(self, p: Permission) -> bool {
        self.0 & p.bit() != 0
    }

    pub fn insert(&mut self, p: Permission) {
        self.0 |= p.bit();
    }

    pub fn remove(&mut self, p: Permission) {
        self.0 &= !p.bit();
    }

    pub fn union(self, other: PermissionSet) -> PermissionSet {
        PermissionSet(self.0 | other.0)
    }

    pub fn intersection(self, other: PermissionSet) -> PermissionSet {
        PermissionSet(self.0 & other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Permission> {
        Permission::ALL
            .into_iter()
            .filter(move |p| self.contains(*p))
    }

    pub(crate) fn bits(self) -> i64 {
        i64::from(self.0)
    }

    pub(crate) fn from_bits(bits: i64) -> Option<PermissionSet> {
        let all = Permission::ALL.iter().fold(0u8, |acc, p| acc | p.bit());
        u8::try_from(bits)
            .ok()
            .filter(|b| b & !all == 0)
            .map(PermissionSet)
    }
}

impl FromIterator<Permission> for PermissionSet {
    fn from_iter<I: IntoIterator<Item = Permission>>(iter: I) -> Self {
        let mut set = PermissionSet::EMPTY;
        for p in iter {
            set.insert(p);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permission_reads_back_from_its_name_and_nothing_else_parses() {
        for p in Permission::ALL {
            assert_eq!(Permission::parse(p.as_str()).unwrap(), p);
            assert_eq!(Permission::parse(&format!("  {p} ")).unwrap(), p);
        }
        for bad in ["", "Read", "authorise_as", "superuser", "bypass"] {
            assert!(Permission::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn a_set_holds_exactly_what_was_inserted() {
        let mut set: PermissionSet = [Permission::Admin].into_iter().collect();
        assert!(set.contains(Permission::Admin));
        assert!(!set.contains(Permission::Read));
        assert!(!set.contains(Permission::Write));
        set.insert(Permission::Read);
        set.remove(Permission::Admin);
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![Permission::Read]);
    }
}
