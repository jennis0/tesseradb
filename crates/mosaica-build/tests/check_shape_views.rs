//! **What `tessera check` sizes a shape layer over** — the views a build would materialise, a
//! layer naming a whole group included (`views.md` §2, §3.5; decision 0111).
//!
//! A layer's `views` list may name a `[[view_group]]`, which draws it on every view of that group.
//! Resolved against the `[[view]]` blocks alone the group matches nothing, and the layer is sized
//! over whichever plain views remain — with the span check run on that partial list, which is how
//! a projected/unprojected mix hides behind a group name.

use std::path::Path;

use tessera_build::config::Config;
use tessera_build::shapes::check_reports;

/// A plain unprojected view whose frame is the group's, a projected one, and a two-view group.
/// The layer block is the caller's, appended as written.
fn declaration(dir: &Path, layer: &str) -> Config {
    let path = dir.join("schema.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[[view]]
name             = "atlas"
extent           = {{ x = [-40.0, 40.0], y = [-40.0, 40.0] }}
point_visibility = {{ default = "public" }}

[[view]]
name             = "world"
projection       = "web_mercator"
extent           = {{ lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }}
point_visibility = {{ default = "public" }}

[[view_group]]
name             = "quarter"
projection       = "none"
extent           = {{ x = [-40.0, 40.0], y = [-40.0, 40.0] }}
point_visibility = {{ default = "public" }}

[[view_group.view]]
key = "2026-Q1"

[[view_group.view]]
key = "2026-Q2"

{layer}
"#
        ),
    )
    .unwrap();
    Config::parse(&path, &Default::default()).expect("the declaration parses")
}

/// One inline shape, in view space — the frames below are identical wherever a row is read at all.
const ARTIFACTS: &str = r#"artifacts = [
  { key = "box", bbox = [-10.0, -10.0, 10.0, 10.0] },
]

  [layer.shape]
  kind = "bbox"
"#;

/// **A group name is every view of the group, here as at the build.** The layer is sized over its
/// plain view *and* both views of `quarter`: a report over the plain view alone would understate
/// the decomposition it will hold by the group's whole roster.
#[test]
fn a_layer_naming_a_group_is_sized_over_every_view_of_it() {
    let tmp = tempfile::tempdir().unwrap();
    let config = declaration(
        tmp.path(),
        &format!(
            r#"
[[layer]]
name                      = "regions"
views                     = ["atlas", "quarter"]
membership                = "spatial"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"
{ARTIFACTS}"#
        ),
    );
    let reports = check_reports(&config);
    assert_eq!(reports.len(), 1, "one shape layer, one report");
    let report = reports[0].as_ref().expect("the frames are all declared");
    assert_eq!(
        report.views,
        vec![
            "atlas".to_string(),
            "quarter:2026-Q1".to_string(),
            "quarter:2026-Q2".to_string()
        ],
        "the group is its views, in the registry's order"
    );
    assert_eq!(report.by_view.len(), 3, "a row per view, the group's included");
    assert_eq!(report.artifacts, 1, "one row, drawn on three views");
    assert_eq!(
        report.interior_tiles,
        report.by_view.iter().map(|v| v.interior_tiles).sum::<u64>(),
        "and the decomposition summed over all three frames"
    );
}

/// **A mix hiding behind the group name is refused, not dropped.** `world` is projected and
/// `quarter`'s views are an embedding: `wgs84` means nothing in an embedding, so no geometry spans
/// the two kinds of space (decision 0111). Resolved against the `[[view]]` blocks alone the group
/// contributes no frame, the remaining list is one projected view, and the refusal never fires.
#[test]
fn a_projected_view_and_a_groups_embedding_are_refused_at_the_check() {
    let tmp = tempfile::tempdir().unwrap();
    let config = declaration(
        tmp.path(),
        &format!(
            r#"
[[layer]]
name                      = "regions"
views                     = ["world", "quarter"]
membership                = "spatial"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"
{ARTIFACTS}"#
        ),
    );
    let reports = check_reports(&config);
    assert_eq!(reports.len(), 1);
    let refusal = reports[0]
        .as_ref()
        .expect_err("a projected view and an embedding do not share a kind of space");
    assert!(refusal.contains("layer 'regions'"), "{refusal}");
    assert!(refusal.contains("'world'"), "{refusal}");
    assert!(refusal.contains("'quarter:2026-Q1'"), "{refusal}");
    assert!(refusal.contains("'quarter:2026-Q2'"), "{refusal}");
    assert!(refusal.contains("decision 0111"), "{refusal}");
}
