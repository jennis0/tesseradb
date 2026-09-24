//! `tessera check`, its payloads, and `reports/disclosure.json`.
//!
//! Through the real binary, on `build_invocation.rs`'s precedent: what is under test is the verb —
//! what it reads, what it refuses, what it prints and what it leaves beside the bundle — and the
//! rules underneath it are `tessera-build`'s own tests'.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use arrow::array::{
    ArrayRef, Float64Array, ListArray, StringArray, StringBuilder, UInt32Array, UInt64Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

const KEY: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 32;

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

fn write(path: &Path, schema: Arc<Schema>, columns: Vec<ArrayRef>) {
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// One list-of-string column, the shape a point's access terms arrive in.
fn list_of_strings(rows: usize, value: &str) -> ArrayRef {
    let mut values = StringBuilder::new();
    for _ in 0..rows {
        values.append_value(value);
    }
    let offsets = OffsetBuffer::from_lengths((0..rows).map(|_| 1usize));
    Arc::new(ListArray::new(
        Arc::new(Field::new("item", DataType::Utf8, true)),
        offsets,
        Arc::new(values.finish()) as ArrayRef,
        None,
    ))
}

/// A whole project: the deployment file, the declaration, and the sources beside them.
fn project(dir: &Path) {
    std::fs::write(
        dir.join("tessera.toml"),
        r#"
[bundle]
path  = "bundles/corpus"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37591"
session = "127.0.0.1:49311"
control = "127.0.0.1:45731"
"#,
    )
    .unwrap();
    std::fs::write(dir.join("schema.toml"), DECLARATION).unwrap();

    let rows = N as usize;
    let ids: Vec<u64> = (0..N).collect();
    write(
        &dir.join("points.parquet"),
        Arc::new(Schema::new(vec![
            Field::new("entity_id", DataType::UInt64, false),
            Field::new("x", DataType::Float64, false),
            Field::new("y", DataType::Float64, false),
            Field::new(
                "categories",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                true,
            ),
            Field::new("severity", DataType::Utf8, true),
        ])),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| *e as f64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| (*e % 7) as f64).collect::<Vec<_>>(),
            )),
            list_of_strings(rows, "public"),
            Arc::new(StringArray::from(vec!["low"; rows])),
        ],
    );
    write_clusters(&dir.join("clusters.parquet"), "members");
    write(
        &dir.join("topics.parquet"),
        Arc::new(Schema::new(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new(
                "contents",
                DataType::List(Arc::new(Field::new(
                    "item",
                    DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                    true,
                ))),
                true,
            ),
            Field::new("attached_layer", DataType::Utf8, true),
            Field::new("attached_key", DataType::Utf8, true),
        ])),
        vec![
            Arc::new(StringArray::from(vec!["t0"])),
            // One entry per rank, each carrying a value per supplied kind — so a list of lists.
            Arc::new(ListArray::new(
                Arc::new(Field::new(
                    "item",
                    DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                    true,
                )),
                OffsetBuffer::from_lengths([1usize]),
                list_of_strings(1, "a topic"),
                None,
            )),
            Arc::new(StringArray::from(vec!["clusters/a"])),
            Arc::new(StringArray::from(vec!["c0"])),
        ],
    );
    write(
        &dir.join("topic_members.parquet"),
        Arc::new(Schema::new(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new("entity", DataType::UInt64, false),
            Field::new("rank", DataType::UInt32, false),
        ])),
        vec![
            Arc::new(StringArray::from(vec!["t0", "t0"])),
            Arc::new(UInt64Array::from(vec![0u64, 1])),
            Arc::new(UInt32Array::from(vec![0u32, 0])),
        ],
    );
}

/// One layer's artifacts, with the membership column under `column` — so a test can misspell it.
fn write_clusters(path: &Path, column: &str) {
    let members = ListArray::new(
        Arc::new(Field::new("item", DataType::UInt64, true)),
        OffsetBuffer::from_lengths([2usize, 2]),
        Arc::new(UInt64Array::from(vec![0u64, 1, 2, 3])) as ArrayRef,
        None,
    );
    write(
        path,
        Arc::new(Schema::new(vec![
            Field::new("key", DataType::Utf8, false),
            Field::new(
                column,
                DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
                true,
            ),
        ])),
        vec![
            Arc::new(StringArray::from(vec!["c0", "c1"])),
            Arc::new(members),
        ],
    );
}

const DECLARATION: &str = r#"
[sources]
points        = "points.parquet"
clusters      = "clusters.parquet"
topics        = "topics.parquet"
topic_members = "topic_members.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = "auto"
point_visibility = { field = "categories", default = "public" }

[[vocabulary]]
name       = "severity"
width      = "u8"
value_set  = "closed"
visibility = "public"
values     = ["low", "medium", "high"]
reserved   = [7]

[[attribute]]
name       = "severity"
type       = "category"
vocabulary = "severity"
render     = true

[[layer]]
name       = "clusters/a"
title      = "Clusters"
views      = ["s0"]
source     = "clusters"
fields     = { members = "members" }
membership = "enumerated"
hierarchy  = { kind = "flat" }

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = { fraction = 0.05 }
content                   = { computed = ["centroid", "box"] }

  [layer.labels]
  source                    = "topics"
  name                      = "topics/a"
  type                      = "text"
  membership                = "enumerated"
  artifact_visibility       = { default = "inherited" }
  require_member_visibility = "all"
  visibility                = "ir:analyst"

    [layer.labels.content]
    require_member_visibility = "all"

    [layer.labels.members]
    source = "topic_members"
"#;

fn run(cwd: &Path, args: &[&str]) -> Output {
    tessera()
        .args(args)
        .current_dir(cwd)
        .env("TESSERA_IDENTITY_KEY", KEY)
        .output()
        .expect("failed to run tessera")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

// -------------------------------------------------------------------------------------------
// The verb
// -------------------------------------------------------------------------------------------

/// **`tessera check`, and nothing else** — the same `tessera.toml`, the same declaration, the same
/// walk up from the working directory `tessera build` does. And it writes no bundle: a check that
/// left something behind would not be a check.
#[test]
fn check_takes_no_flags_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let deep = tmp.path().join("a/b");
    std::fs::create_dir_all(&deep).unwrap();
    let output = run(&deep, &["check"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("check OK"), "{}", stderr(&output));
    assert!(!tmp.path().join("bundles").exists());
}

/// The disclosure decisions, as a table: every layer's gate and member requirement, every
/// vocabulary's visibility and value set, every attribute's placement — and which layers the
/// `[layer.labels]` sugar wrote, which nothing below the expansion can tell.
#[test]
fn check_prints_every_disclosure_decision() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let output = run(tmp.path(), &["check"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    for expected in [
        "labels from field, default 'public'",
        "public, closed, 3 declared value(s), reserved [7]",
        "u8 over vocabulary 'severity', render",
        "gate 'public' | artifacts 'inherited' | members {\"fraction\":0.05}",
        "written by `[layer.labels]` on 'clusters/a'",
        "gate 'ir:analyst'",
        "served only where clusters/a is served",
        "supplied text 'topics/a' requires all",
    ] {
        assert!(text.contains(expected), "{expected}\nmissing from:\n{text}");
    }
    // The whole page is one stream, so stdout carries the payloads and nothing else.
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
}

/// **Every finding, not the first.** A build stops at the first thing wrong because everything
/// after it is wasted work; a check exists to be run and fixed in one pass.
#[test]
fn check_reports_every_finding_rather_than_the_first() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    // Two independent breakages, in two different objects.
    write_clusters(&tmp.path().join("clusters.parquet"), "membership_ids");
    std::fs::write(
        tmp.path().join("schema.toml"),
        DECLARATION.replace(
            "type       = \"category\"\nvocabulary = \"severity\"",
            "type       = \"u16\"",
        ),
    )
    .unwrap();

    let output = run(tmp.path(), &["check"]);
    assert!(!output.status.success(), "{}", stdout(&output));
    let text = stderr(&output);
    assert!(text.contains("attribute 'severity'"), "{text}");
    assert!(text.contains("holds Utf8"), "{text}");
    assert!(text.contains("layer 'clusters/a'"), "{text}");
    assert!(text.contains("field `members`"), "{text}");
    assert!(text.contains("2 finding(s)"), "{text}");
    // And it says what it could not have seen, so a green check is not read as a green build.
    assert!(
        text.contains("a clean check is not a clean build"),
        "{text}"
    );
}

/// A source the declaration names and the filesystem does not carry is a finding rather than a
/// panic — the check's whole job being to name what a build would fall over on.
#[test]
fn check_names_a_source_that_is_not_there() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::remove_file(tmp.path().join("topic_members.parquet")).unwrap();
    let output = run(tmp.path(), &["check"]);
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("`[layer.members]`"), "{text}");
    assert!(text.contains("topic_members.parquet"), "{text}");
}

// -------------------------------------------------------------------------------------------
// The control-plane payloads
// -------------------------------------------------------------------------------------------

/// **A `[[layer]]` block minus its acquisition keys *is* the `PUT /control/layers` body**
/// (`configuration.md` §2), so this is a serialisation and not a translation — and a declaration
/// written with `[layer.labels]` yields the second body without the caller writing it twice.
#[test]
fn payloads_are_the_control_plane_bodies() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let output = run(tmp.path(), &["check", "--payloads"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // Round-tripped through the type the endpoint takes: a body that will not deserialise there is
    // not a payload, whatever it looks like.
    let payloads: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("stdout is one JSON object of payloads");
    let bodies: Vec<tessera_types::layer::LayerDeclaration> =
        serde_json::from_value(payloads["layers"].clone()).expect("`layers` is the layer bodies");
    assert_eq!(bodies.len(), 2, "the label sugar's layer is a body too");
    assert_eq!(bodies[0].name, "clusters/a");
    assert_eq!(bodies[0].visibility, None, "`public` is an absence here");
    assert_eq!(bodies[1].name, "topics/a");
    assert_eq!(bodies[1].depends_on, vec!["clusters/a".to_string()]);
    assert_eq!(bodies[1].visibility.as_deref(), Some("ir:analyst"));
    // Acquisition is absent: nothing in a body names a file.
    assert!(!stdout(&output).contains("parquet"), "{}", stdout(&output));
}

/// **The deployment that declares and never builds** (`configuration.md` §2): no `source`
/// anywhere, which is legal and is the normal state for a corpus written through the service. Its
/// objects are declared and empty, the check says so rather than refusing, and the payloads come
/// out — which is the whole point, since that deployment used to author every layer a second time
/// by hand.
#[test]
fn a_declaration_with_no_sources_checks_and_still_emits_payloads() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::write(
        tmp.path().join("schema.toml"),
        r#"
[[view]]
name             = "s0"
extent           = { min = -25.0, max = 25.0 }
point_visibility = { default = "public" }

[[layer]]
name       = "clusters/a"
views      = ["s0"]
membership = "enumerated"
hierarchy  = { kind = "flat" }

visibility                = "ir:analyst"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"
"#,
    )
    .unwrap();
    let output = run(tmp.path(), &["check", "--payloads"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("declared and empty"),
        "{}",
        stderr(&output)
    );
    let payloads: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let bodies: Vec<tessera_types::layer::LayerDeclaration> =
        serde_json::from_value(payloads["layers"].clone()).unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0].visibility.as_deref(), Some("ir:analyst"));
}

/// **An attribute that names no file is declared and empty**, not a finding (`configuration.md`
/// §2, python-sdk §6.2): the values arrive through `PUT /control/attributes` and the batches after
/// it, so there is no file for the check to disagree with. The payload is emitted, which is what
/// the deployment posts to declare the column.
#[test]
fn an_attribute_naming_no_source_is_a_note_and_a_payload() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::write(
        tmp.path().join("schema.toml"),
        r#"
[[view]]
name             = "s0"
extent           = { min = -25.0, max = 25.0 }
point_visibility = { default = "public" }

[[attribute]]
name  = "sentiment"
type  = "f32"
"#,
    )
    .unwrap();
    let output = run(tmp.path(), &["check", "--payloads"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains("attribute 'sentiment'"), "{said}");
    assert!(said.contains("declared and empty"), "{said}");
    let payloads: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let attributes = payloads["attributes"].as_array().unwrap();
    assert_eq!(attributes.len(), 1);
    assert_eq!(attributes[0]["name"], "sentiment");
}

/// **A view group that names no points file and no roster is declared and empty** on the same
/// rule. `tessera build` refuses it, the points having to come from somewhere, and the refusal
/// names the two roster forms.
#[test]
fn a_view_group_naming_no_source_is_a_note_at_check_and_a_refusal_at_build() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::write(
        tmp.path().join("schema.toml"),
        r#"
[[view_group]]
name             = "quarters"
extent           = { min = -25.0, max = 25.0 }
point_visibility = { default = "public" }
"#,
    )
    .unwrap();
    let output = run(tmp.path(), &["check", "--payloads"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains("view group 'quarters'"), "{said}");
    assert!(said.contains("declared and empty"), "{said}");
    let payloads: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let groups = payloads["view_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["name"], "quarters");

    let built = run(tmp.path(), &["build"]);
    assert!(!built.status.success(), "{}", stderr(&built));
    assert!(
        stderr(&built).contains("no `source` and no `[[view_group.view]]` roster"),
        "{}",
        stderr(&built)
    );
}

/// **Never a payload out of a declaration that failed its check.** The stream is what a CI job
/// pipes at a control plane, and a body emitted beside a finding is one somebody posts.
#[test]
fn a_failed_check_emits_no_payloads() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::remove_file(tmp.path().join("clusters.parquet")).unwrap();
    let output = run(tmp.path(), &["check", "--payloads"]);
    assert!(!output.status.success());
    assert!(stdout(&output).trim().is_empty(), "{}", stdout(&output));
}

// -------------------------------------------------------------------------------------------
// `reports/disclosure.json`
// -------------------------------------------------------------------------------------------

fn build(cwd: &Path) -> Output {
    let output = run(cwd, &["build"]);
    assert!(output.status.success(), "{}", stderr(&output));
    output
}

/// A build into a named root, so two builds of one project can be compared. A build refuses to
/// write over an existing bundle, which is what makes this the shape a diff test takes.
fn build_out(cwd: &Path, out: &str) -> String {
    let output = run(cwd, &["build", "--out", out]);
    assert!(output.status.success(), "{}", stderr(&output));
    std::fs::read_to_string(cwd.join(out).join("reports/disclosure.json")).unwrap()
}

/// **The report is what a reviewer diffs**, so two builds of one declaration must produce one
/// document — no timestamp, no path, no map iterated in hash order.
#[test]
fn the_disclosure_report_is_byte_identical_between_builds() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let first = build_out(tmp.path(), "one");
    let second = build_out(tmp.path(), "two");
    assert_eq!(
        first, second,
        "two builds of one declaration must write one document"
    );
    // Nothing machine-specific: the tempdir's own path is the thing most likely to leak in.
    assert!(
        !first.contains(tmp.path().to_str().unwrap()),
        "a path reached the report:\n{first}"
    );
    assert!(!first.contains(".parquet"), "{first}");
}

/// Every disclosure decision the declaration makes, and one a reviewer would otherwise have to
/// reconstruct: which layers the `[layer.labels]` sugar wrote, and the dependency edge that gates
/// them (decision 0089).
#[test]
fn the_disclosure_report_carries_every_decision() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    build(tmp.path());
    let text =
        std::fs::read_to_string(tmp.path().join("bundles/corpus/reports/disclosure.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(json["views"][0]["labels_from"], "field");
    assert_eq!(json["views"][0]["default"], "public");
    assert_eq!(json["vocabularies"][0]["visibility"], "public");
    assert_eq!(json["vocabularies"][0]["value_set"], "closed");
    assert_eq!(json["vocabularies"][0]["reserved"][0], 7);
    assert_eq!(json["attributes"][0]["placement"], "render");
    assert_eq!(json["attributes"][0]["vocabulary"], "severity");

    let clusters = &json["layers"][0];
    assert_eq!(clusters["visibility"], "public");
    assert_eq!(clusters["require_member_visibility"]["fraction"], 0.05);
    assert_eq!(clusters["expanded_from"], serde_json::Value::Null);

    let topics = &json["layers"][1];
    assert_eq!(topics["expanded_from"], "clusters/a");
    assert_eq!(topics["visibility"], "ir:analyst");
    assert_eq!(topics["require_member_visibility"], "all");
    assert_eq!(topics["depends_on"][0], "clusters/a");
    assert_eq!(
        topics["content"]["supplied"][0]["require_member_visibility"],
        "all"
    );
}

/// **`tessera check` computes the same document**, which is the whole reason it lives on the
/// declaration rather than on the build: a reviewer can read the decisions before anything is
/// built, and the two cannot disagree.
#[test]
fn a_moved_gate_moves_exactly_one_line_of_the_report() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let before = build_out(tmp.path(), "before");

    std::fs::write(
        tmp.path().join("schema.toml"),
        DECLARATION.replace(
            "visibility                = \"ir:analyst\"",
            "visibility                = \"public\"",
        ),
    )
    .unwrap();
    let after = build_out(tmp.path(), "after");

    let changed: Vec<(&str, &str)> = before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(
        changed,
        vec![(
            "      \"visibility\": \"ir:analyst\",",
            "      \"visibility\": \"public\","
        )],
        "a gate moving must move one line and nothing else"
    );
}

/// **A bare-geometry bundle gets no `reports/` at all.** That directory is where an operator polls
/// for the fold's notices, and creating it in every bundle ever built to say *this declaration
/// decides nothing* is worse than saying nothing — the argument `containment.json` already makes
/// for its own emptiness.
#[test]
fn a_declaration_that_decides_nothing_writes_no_report() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::write(
        tmp.path().join("schema.toml"),
        "[sources]\npoints = \"points.parquet\"\n\
         [[view]]\nname = \"s0\"\nextent = \"auto\"\nsource = \"points\"\n\
         point_visibility = { field = \"categories\", default = \"public\" }\n",
    )
    .unwrap();
    build(tmp.path());
    assert!(!tmp.path().join("bundles/corpus/reports").exists());
}

// -------------------------------------------------------------------------------------------
// A points file with no identity column
// -------------------------------------------------------------------------------------------

/// The project's points, minus the identity column: geometry, access terms and one attribute.
fn write_points_without_identity(dir: &Path) {
    let rows = N as usize;
    let ids: Vec<u64> = (0..N).collect();
    write(
        &dir.join("points.parquet"),
        Arc::new(Schema::new(vec![
            Field::new("x", DataType::Float64, false),
            Field::new("y", DataType::Float64, false),
            Field::new(
                "categories",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                true,
            ),
            Field::new("severity", DataType::Utf8, true),
        ])),
        vec![
            Arc::new(Float64Array::from(
                ids.iter().map(|e| *e as f64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| (*e % 7) as f64).collect::<Vec<_>>(),
            )),
            list_of_strings(rows, "public"),
            Arc::new(StringArray::from(vec!["low"; rows])),
        ],
    );
}

/// One layer's artifacts, keys and nothing else — no membership beside them.
fn write_keys(dir: &Path) {
    write(
        &dir.join("keys.parquet"),
        Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)])),
        vec![Arc::new(StringArray::from(vec!["t0"]))],
    );
}

/// The whole declaration over that file. The attribute is read from the points file itself, which
/// is the one place a positional build can read one from.
const NO_IDENTITY: &str = r#"
[sources]
points = "points.parquet"

[defaults]
source = "points"

[[view]]
name             = "s0"
extent           = "auto"
source           = "points"
point_visibility = { field = "categories", default = "public" }

[[vocabulary]]
name       = "severity"
width      = "u8"
value_set  = "closed"
visibility = "public"
values     = ["low", "medium", "high"]

[[attribute]]
name       = "severity"
type       = "category"
vocabulary = "severity"
render     = true
"#;

/// **A points file may carry no identity column** (`configuration.md` §8), and the check accepts
/// exactly what the build does: the rows are named by their position, the note says how they are
/// addressed instead, and the check stays clean.
#[test]
fn a_points_file_with_no_identity_column_checks_clean_and_builds() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    write_points_without_identity(tmp.path());
    std::fs::write(tmp.path().join("schema.toml"), NO_IDENTITY).unwrap();

    let output = run(tmp.path(), &["check"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(
        said.contains("no identity column: rows addressable by tessera_id only"),
        "{said}"
    );
    assert!(said.contains("check OK"), "{said}");

    build(tmp.path());
}

/// A `[layer.members]` table names one entity per row, and a position is not an entity. The
/// finding names the table, so the author reads what needs the column rather than that one is
/// missing.
#[test]
fn a_members_table_over_a_file_with_no_identity_column_is_a_finding_naming_it() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    write_points_without_identity(tmp.path());
    write_keys(tmp.path());
    std::fs::write(
        tmp.path().join("schema.toml"),
        r#"
[sources]
points        = "points.parquet"
keys          = "keys.parquet"
topic_members = "topic_members.parquet"

[[view]]
name             = "s0"
extent           = "auto"
source           = "points"
point_visibility = { field = "categories", default = "public" }

[[layer]]
name       = "topics/a"
views      = ["s0"]
source     = "keys"
membership = "enumerated"
hierarchy  = { kind = "flat" }

visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "all"

  [layer.members]
  source = "topic_members"
"#,
    )
    .unwrap();

    let output = run(tmp.path(), &["check"]);
    assert!(!output.status.success(), "{}", stderr(&output));
    let said = stderr(&output);
    assert!(said.contains("view 's0'"), "{said}");
    assert!(said.contains("topic_members.parquet"), "{said}");
    assert!(said.contains("Declare an identity column"), "{said}");
}
