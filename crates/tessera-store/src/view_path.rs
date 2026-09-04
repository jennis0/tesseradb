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
///
/// **Re-exported, not redefined**: the roster's WAL records travel through `tessera-lifecycle`,
/// which does not depend on this crate, so the character lives beside them in `tessera-types` and
/// both halves read the same one.
pub use tessera_types::view::GROUP_SEPARATOR;

use tessera_types::view::{ViewIncarnation, DECLARED_INCARNATION};

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

/// `attrs/<column>/<view components>/` — where one view's column of a **group-scoped family**
/// lives, prefix-relative under a partition (`views.md` §5).
///
/// **The incarnation joins the last component above the build's** (decision 0115). A scoped
/// column's *base* is the one artefact a view owns at a fixed path: its extents are named by a
/// `seg_id` that is never reused, but the base sits at the directory root and a flush of a
/// **recreated** key would otherwise write straight over its predecessor's. That is not a
/// tidiness point. The live manifest still digests that file, so overwriting it corrupts a path
/// the current bundle names — a crash between the write and the next publication leaves a bundle
/// that refuses to open — and the dropped view's column may still be memory-mapped by the live
/// generation, which carries `filter_columns` across a drop unchanged.
///
/// At [`DECLARED_INCARNATION`] the path is exactly what it always was, so nothing a build wrote
/// moves; a key created while the service runs gets `<key>@<n>`, and `@` is reserved out of a view
/// key (`tessera_types::view::check_view_key`) so the suffix can never collide with one.
pub fn scoped_column_rel(
    partition: &str,
    column: &str,
    view_id: &str,
    incarnation: ViewIncarnation,
) -> String {
    let mut rel = format!("partitions/{partition}/attrs/{column}");
    for component in scoped_column_components(view_id, incarnation) {
        rel.push('/');
        rel.push_str(&component);
    }
    rel
}

/// [`scoped_column_rel`]'s components, for a caller building a `Path` rather than a manifest key.
pub fn scoped_column_components(view_id: &str, incarnation: ViewIncarnation) -> Vec<String> {
    let mut components: Vec<String> = view_path_components(view_id)
        .into_iter()
        .map(str::to_string)
        .collect();
    if incarnation != DECLARED_INCARNATION {
        if let Some(last) = components.last_mut() {
            last.push('@');
            last.push_str(&incarnation.to_string());
        }
    }
    components
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

    /// **A recreated key's scoped column does not land on its predecessor's path**
    /// (decision 0115), and a build-declared view's path does not move.
    ///
    /// The base is the one artefact a view owns at a fixed path — its extents are named by a
    /// `seg_id` that is never reused — so this is what stops a flush of a key created again from
    /// writing over a file the live manifest still digests, and over bytes the previous
    /// generation may still hold mapped (`filter_columns` is carried across a drop unchanged).
    #[test]
    fn a_scoped_columns_path_moves_with_the_incarnation_and_not_at_the_build() {
        let declared = scoped_column_rel("p0", "mood", "quarter:2026-Q3", DECLARED_INCARNATION);
        assert_eq!(declared, "partitions/p0/attrs/mood/quarter/2026-Q3");
        let recreated = scoped_column_rel("p0", "mood", "quarter:2026-Q3", 4);
        assert_eq!(recreated, "partitions/p0/attrs/mood/quarter/2026-Q3@4");
        assert_ne!(declared, recreated);
        // Two live incarnations of one key never share a directory either.
        assert_ne!(
            scoped_column_rel("p0", "mood", "quarter:2026-Q3", 4),
            scoped_column_rel("p0", "mood", "quarter:2026-Q3", 5)
        );
        // A plain view has one component, and it takes the suffix the same way.
        assert_eq!(
            scoped_column_rel("p0", "mood", "world", DECLARED_INCARNATION),
            "partitions/p0/attrs/mood/world"
        );
        // The components and the relative path are one derivation, so a reader joining the first
        // reaches the file the writer named with the second.
        assert_eq!(
            scoped_column_components("quarter:2026-Q3", 4),
            vec!["quarter".to_string(), "2026-Q3@4".to_string()]
        );
    }

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
