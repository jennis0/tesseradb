//! **`tessera check` reads inside an inline artifact row**, which is where the whole row is: a
//! geometry on a layer that declares no `[layer.shape]`, two geometries on one row, and a
//! geometry that is not the layer's declared kind are each a finding here, seconds after the
//! declaration is written, rather than minutes into the build that would refuse them.

use std::path::Path;

use tessera_build::check::check;
use tessera_build::config::Config;

/// One unprojected view, and the layer block as the caller wrote it.
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

{layer}
"#
        ),
    )
    .unwrap();
    Config::parse(&path, &Default::default()).expect("the declaration parses")
}

fn layer(shape: &str, rows: &str) -> String {
    format!(
        r#"
[[layer]]
name                      = "regions"
views                     = ["atlas"]
membership                = "spatial"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = "none"
artifacts = [
{rows}
]
{shape}"#
    )
}

/// The findings as one string, so a test asks whether the row an author must edit is named.
fn findings(config: &Config) -> String {
    let report = check(config);
    report
        .findings
        .iter()
        .map(|f| format!("{}: {}", f.object, f.detail))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_clean_inline_shape_layer_has_no_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let config = declaration(
        tmp.path(),
        &layer(
            "\n  [layer.shape]\n  kind = \"bbox\"\n",
            r#"  { key = "box", bbox = [-10.0, -10.0, 10.0, 10.0] },"#,
        ),
    );
    assert!(check(&config).is_clean(), "{}", findings(&config));
}

#[test]
fn a_shape_on_a_layer_that_declares_none_is_a_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let config = declaration(
        tmp.path(),
        r#"
[[layer]]
name                      = "cases"
views                     = ["atlas"]
membership                = "enumerated"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "none"
artifacts = [
  { key = "unshaped", members = [0], bbox = [-1.0, -1.0, 1.0, 1.0] },
]
"#,
    );
    let found = findings(&config);
    assert!(!check(&config).is_clean(), "no finding");
    assert!(
        found.contains("cases") && found.contains("unshaped"),
        "{found}"
    );
}

#[test]
fn two_shapes_on_one_row_are_a_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let config = declaration(
        tmp.path(),
        &layer(
            "\n  [layer.shape]\n  kind = \"bbox\"\n",
            r#"  { key = "both", bbox = [-1.0, -1.0, 1.0, 1.0], circle = [0.0, 0.0, 1.0] },"#,
        ),
    );
    let found = findings(&config);
    assert!(!check(&config).is_clean(), "no finding");
    assert!(found.contains("regions") && found.contains("both"), "{found}");
}

#[test]
fn a_shape_that_is_not_the_declared_kind_is_a_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let config = declaration(
        tmp.path(),
        &layer(
            "\n  [layer.shape]\n  kind = \"bbox\"\n",
            r#"  { key = "round", circle = [0.0, 0.0, 1.0] },"#,
        ),
    );
    let found = findings(&config);
    assert!(!check(&config).is_clean(), "no finding");
    assert!(
        found.contains("regions") && found.contains("round"),
        "{found}"
    );
}

/// Every finding, not the first — the whole point of the verb over the build.
#[test]
fn every_bad_row_is_named_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let config = declaration(
        tmp.path(),
        &layer(
            "\n  [layer.shape]\n  kind = \"bbox\"\n",
            r#"  { key = "one", circle = [0.0, 0.0, 1.0] },
  { key = "two", bbox = [-1.0, -1.0, 1.0, 1.0], wkt = "POLYGON EMPTY" },"#,
        ),
    );
    let found = findings(&config);
    assert!(found.contains("one") && found.contains("two"), "{found}");
}
