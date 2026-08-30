//! The one place a view id becomes a path (`views.md` §3.2).
//!
//! A plain view lives at `views/<view>/`; a view of a group lives at `views/<group>/<key>/`,
//! **nested rather than joined**, because `:` is not a path character everywhere.
//! `SegmentDescriptor.view` and `WalRow.view` hold the joined `group:key` form — the id is what
//! a request names and what the manifest carries — and this module is what every path consumer
//! goes through, so the two can never drift apart.

use std::path::{Path, PathBuf};

/// The separator between a group's name and a view's key in a view id (`views.md` §3.2).
///
/// Reserved out of plain view names and keys at the configuration parser and again at manifest
/// load, which is what makes splitting on it unambiguous.
pub const GROUP_SEPARATOR: char = ':';

/// The path components `view_id` derives, in order: one for a plain view, two for a group's.
///
/// Exposed beside [`view_path`] so a caller that has to *check* the components — a build
/// validating that a declared id is a safe path — asks the same function that will lay them
/// down, rather than a second reading of the rule.
pub fn view_path_components(view_id: &str) -> Vec<&str> {
    match view_id.split_once(GROUP_SEPARATOR) {
        Some((group, key)) => vec![group, key],
        None => vec![view_id],
    }
}

/// `views/<view>/`, or `views/<group>/<key>/`, under `partition_dir`.
pub fn view_path(partition_dir: &Path, view_id: &str) -> PathBuf {
    let mut path = partition_dir.join("views");
    for component in view_path_components(view_id) {
        path.push(component);
    }
    path
}

/// `views/<view>` or `views/<group>/<key>` — the prefix-relative form the manifest's `files` map
/// and every digest check are keyed by.
///
/// Beside [`view_path`] because the two must agree: a `files` key that did not match the path the
/// build laid down would make every file under the view unverifiable, which the loader reads as a
/// corrupt bundle.
pub fn view_rel(view_id: &str) -> String {
    let mut rel = String::from("views");
    for component in view_path_components(view_id) {
        rel.push('/');
        rel.push_str(component);
    }
    rel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_view_is_one_component_and_a_groups_view_is_two() {
        let root = Path::new("/p");
        assert_eq!(view_path(root, "world"), Path::new("/p/views/world"));
        assert_eq!(
            view_path(root, "quarter:2026-Q2"),
            Path::new("/p/views/quarter/2026-Q2")
        );
        assert_eq!(
            view_path_components("quarter:2026-Q2"),
            ["quarter", "2026-Q2"]
        );
        assert_eq!(view_rel("world"), "views/world");
        assert_eq!(view_rel("quarter:2026-Q2"), "views/quarter/2026-Q2");
    }
}
