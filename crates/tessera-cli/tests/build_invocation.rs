//! What `tessera build` is invoked as, through the real binary.
//!
//! **The whole of it is `tessera build`.** `tessera.toml` — found by walking up from the working
//! directory — says where the corpus declaration is and where the bundle goes; the declaration
//! says where the corpus is and what frame it is quantised against; the environment carries the
//! identity key. Every flag is an override or a performance knob, and this file is where that
//! claim is checked against the binary rather than against the parsers underneath it.
//!
//! `tessera-build`'s own tests cover the declaration rules. What is here is the *invocation*: the
//! deployment file and its refusal when there is none, the environment key and its `.env`, the
//! `--file` override, and the flags that no longer exist.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

const KEY: &str = "000102030405060708090a0b0c0d0e0f";
const N: u64 = 64;

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

/// A whole project, laid out as a deployment is: the two documents, the two sources beside them,
/// and nothing absolute anywhere.
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
viewer  = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("schema.toml"),
        "[[view]]\nname = \"s0\"\nextent = \"auto\"\nsource = \"points.parquet\"\n\
         point_visibility = { source = \"pairs.parquet\", default = \"public\" }\n",
    )
    .unwrap();
    write_points(&dir.join("points.parquet"));
    write_pairs(&dir.join("pairs.parquet"));
}

fn write_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    // A deliberately non-square cloud: x spans 100, y spans 10. `auto` squares it.
    let xs: Vec<f64> = ids.iter().map(|e| (e % 101) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e % 11) as f64) + 1000.0).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let entities: Vec<u64> = (0..N).collect();
    let terms: Vec<u32> = (0..N).map(|e| (e % 5) as u32 + 1).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

/// `tessera build` from `cwd`, with the identity key in the environment and nothing else.
fn build_in(cwd: &Path, args: &[&str]) -> Output {
    tessera()
        .arg("build")
        .args(args)
        .current_dir(cwd)
        .env("TESSERA_IDENTITY_KEY", KEY)
        .output()
        .expect("failed to run tessera")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn refusal(cwd: &Path, args: &[&str]) -> String {
    let output = build_in(cwd, args);
    assert!(
        !output.status.success(),
        "the build accepted an invocation it must refuse:\n{}",
        stderr(&output)
    );
    stderr(&output)
}

// -------------------------------------------------------------------------------------------
// The target invocation
// -------------------------------------------------------------------------------------------

/// **`tessera build`, and nothing else.** No `--config`, no `--out`, no `--extent`, no `--view`,
/// no `--file`, no key on the command line — and the bundle lands where `tessera.toml` says a
/// server would open it.
#[test]
fn the_whole_invocation_is_tessera_build() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let output = build_in(tmp.path(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        tmp.path().join("bundles/corpus/CURRENT").is_file(),
        "the bundle must land at tessera.toml's own bundle.path"
    );
}

/// The deployment file is found by walking up, as `Cargo.toml` is — so the verb works from
/// anywhere under the project root and the paths inside it still mean what they say.
#[test]
fn the_deployment_config_is_found_by_walking_up() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let deep = tmp.path().join("a/b/c");
    std::fs::create_dir_all(&deep).unwrap();
    let output = build_in(&deep, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        tmp.path().join("bundles/corpus/CURRENT").is_file(),
        "every path in tessera.toml resolves against tessera.toml, not against the shell's cwd"
    );
}

/// **A missing `tessera.toml` is a refusal naming what to create**, never a set of defaults: every
/// path in it is a decision, and a guessed one is a build writing where nobody asked.
#[test]
fn a_missing_deployment_config_names_what_to_create() {
    let tmp = tempfile::tempdir().unwrap();
    let stderr = refusal(tmp.path(), &[]);
    assert!(stderr.contains("no tessera.toml found"), "{stderr}");
    for expected in ["[bundle]", "[build]", "[identity]", "[serve]", "--deployment"] {
        assert!(stderr.contains(expected), "{expected} missing from: {stderr}");
    }
}

/// `--deployment` names one outright, for the invocation that is not standing in the project.
#[test]
fn the_deployment_config_can_be_named_outright() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let elsewhere = tempfile::tempdir().unwrap();
    let output = build_in(
        elsewhere.path(),
        &["--deployment", tmp.path().join("tessera.toml").to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(tmp.path().join("bundles/corpus/CURRENT").is_file());
}

// -------------------------------------------------------------------------------------------
// The extent
// -------------------------------------------------------------------------------------------

/// The frame a build quantised against, out of the line it prints. Reported at all because the
/// extent is what every stored cell is relative to, and a fitted one appears nowhere else.
fn reported_frame(output: &Output) -> [f64; 4] {
    let text = stderr(output);
    let line = text
        .lines()
        .find(|l| l.contains("quantising against"))
        .unwrap_or_else(|| panic!("no frame reported:\n{text}"));
    let numbers: Vec<f64> = line
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter(|s| !s.is_empty() && *s != "-")
        .filter_map(|s| s.parse().ok())
        .collect();
    // The view name carries a `0`, so the frame is the last four.
    let tail = &numbers[numbers.len() - 4..];
    [tail[0], tail[1], tail[2], tail[3]]
}

/// **`auto` squares the box**, so a circle in the data stays a circle on the grid, and carries a
/// small margin because the extent is half-open and a point sitting exactly at the maximum would
/// otherwise quantise to the clamp.
#[test]
fn auto_fits_a_square_box_around_the_data() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let output = build_in(tmp.path(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let [x_min, x_max, y_min, y_max] = reported_frame(&output);

    // The data is deliberately not square: x spans 63, y spans 10.
    let (wide, narrow) = (x_max - x_min, y_max - y_min);
    assert!(
        (wide - narrow).abs() < 1e-9,
        "auto must square the box, got x span {wide} against y span {narrow}"
    );
    // The larger axis decides the side, and the default margin is 1% of it on each side.
    assert!((wide - 63.0 * 1.02).abs() < 1e-9, "{wide}");
    // And the data is inside it, which is the whole point of the margin: 63 sits strictly below
    // the maximum rather than exactly on the half-open boundary.
    assert!(x_min < 0.0 && x_max > 63.0, "x [{x_min}, {x_max}]");
    assert!(y_min < 1000.0 && y_max > 1010.0, "y [{y_min}, {y_max}]");
}

/// `margin` is a fraction of the data span, added on each side. It exists for **growth**: `auto`
/// sees only the data present at build, so a corpus that will be written to needs headroom or the
/// first out-of-range ingest clamps.
#[test]
fn a_margin_is_a_fraction_of_the_data_span_on_each_side() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::write(
        tmp.path().join("schema.toml"),
        "[[view]]\nname = \"s0\"\nextent = { auto = true, margin = 0.25 }\n\
         source = \"points.parquet\"\n\
         point_visibility = { source = \"pairs.parquet\", default = \"public\" }\n",
    )
    .unwrap();
    let output = build_in(tmp.path(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    // Side 63, a quarter of it on each side, centred on x = 31.5 and y = 1005.
    assert_eq!(
        reported_frame(&output),
        [31.5 - 47.25, 31.5 + 47.25, 1005.0 - 47.25, 1005.0 + 47.25]
    );
}

/// A frame stated outright is used verbatim, and a degenerate one is refused rather than clamped.
#[test]
fn a_stated_extent_is_used_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::write(
        tmp.path().join("schema.toml"),
        "[[view]]\nname = \"s0\"\nextent = { min = -5.0, max = 2000.0 }\n\
         source = \"points.parquet\"\n\
         point_visibility = { source = \"pairs.parquet\", default = \"public\" }\n",
    )
    .unwrap();
    let output = build_in(tmp.path(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(reported_frame(&output), [-5.0, 2000.0, -5.0, 2000.0]);
}

/// A build materialises one coordinate system. With several declared, choosing one would publish
/// geometry nobody asked for — two views quantise the same corpus differently.
#[test]
fn several_views_refuse_rather_than_choosing_one() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let schema = std::fs::read_to_string(tmp.path().join("schema.toml")).unwrap();
    std::fs::write(
        tmp.path().join("schema.toml"),
        format!("{schema}{}", schema.replace("\"s0\"", "\"s1\"")),
    )
    .unwrap();
    let stderr = refusal(tmp.path(), &[]);
    assert!(stderr.contains("has 2 views"), "{stderr}");
    assert!(stderr.contains("s0, s1"), "{stderr}");
}

// -------------------------------------------------------------------------------------------
// The clamp report
// -------------------------------------------------------------------------------------------

/// Points at real projection coordinates — the shape a UMAP produces, spanning roughly −17…18 and
/// −21…23 about the origin.
fn write_projection_points(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    // Deterministic, spread across the sign on both axes, and the two axes run **opposite ways**
    // — a projection's two components are independent, so the points outside a grid-shaped frame
    // on x are not the same points that are outside it on y. Ramping them together would make the
    // union of the two halves one half, which is a corpus no projection produces.
    let xs: Vec<f64> = ids.iter().map(|e| -17.0 + (*e as f64) * 35.0 / 63.0).collect();
    let ys: Vec<f64> = ids.iter().map(|e| 23.0 - (*e as f64) * 44.0 / 63.0).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn declare_extent(dir: &Path, extent: &str) {
    std::fs::write(
        dir.join("schema.toml"),
        format!(
            "[[view]]\nname = \"s0\"\nextent = {extent}\nsource = \"points.parquet\"\n\
             point_visibility = {{ source = \"pairs.parquet\", default = \"public\" }}\n"
        ),
    )
    .unwrap();
}

/// **The notebook's own failure, and the reason this report exists.** A grid-shaped extent was
/// declared over projection coordinates, every point folded into a nineteen-cell corner, the
/// bundle came out well-formed with the geometry wrong — and the build said nothing at all.
///
/// It must now say all three things: how many points land on the frame's edge, what the frame is,
/// and what the data's own bounds are. And past half the corpus it must refuse rather than report,
/// because a frame that misplaces the majority of a corpus is not that corpus's frame.
#[test]
fn the_notebook_failure_is_loud() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    write_projection_points(&tmp.path().join("points.parquet"));
    declare_extent(tmp.path(), "{ min = 0.0, max = 65536.0 }");

    let stderr = refusal(tmp.path(), &[]);
    // The clamp count, as a count and as a share.
    assert!(stderr.contains("CLAMP onto the frame's edge"), "{stderr}");
    assert!(stderr.contains("of 64 point(s)"), "{stderr}");
    // The frame it was given, and the data's own bounds beside it — the pair is the diagnosis.
    assert!(stderr.contains("quantising against x [0, 65536]"), "{stderr}");
    assert!(stderr.contains("the data spans x [-17"), "{stderr}");
    // And how little of the grid that leaves, which is the number the degenerate map needed.
    assert!(stderr.contains("of the 65536 x 65536 cells"), "{stderr}");
    // A refusal, not a warning: the build wrote nothing.
    assert!(
        !tmp.path().join("bundles/corpus/CURRENT").is_file(),
        "a refused build must not have written a bundle"
    );
}

/// **A tail of clamped points is reported and built.** Outliers, or headroom a caller left for
/// growth, are a frame the caller may well mean — so under the threshold the build says what it
/// did and goes on. A refusal here would make the report something a caller works around.
#[test]
fn a_minority_of_clamped_points_is_reported_and_built() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    // The fixture's data is x 0..63, y 1000..1010. This frame leaves the last few x outside it.
    declare_extent(tmp.path(), "{ x = [0.0, 60.0], y = [999.0, 1011.0] }");
    let output = build_in(tmp.path(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("CLAMP onto the frame's edge"), "{text}");
    assert!(text.contains("3 of 64 point(s) (4.7%)"), "{text}");
}

/// **A point sitting exactly at the maximum is not a clamp.** Cells are half-open and `v = max`
/// lands in the top cell by construction, so counting it would report every tightly-fitted corpus
/// as damaged — and a frame stated to fit the data exactly is the case a careful caller writes.
#[test]
fn a_point_at_the_maximum_is_not_a_clamp() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    // Exactly the fixture's own bounds: x 0..63, y 1000..1010, both endpoints on the boundary.
    declare_extent(tmp.path(), "{ x = [0.0, 63.0], y = [1000.0, 1010.0] }");
    let output = build_in(tmp.path(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("64 point(s) placed, none on the frame's edge"), "{text}");
}

/// `auto` fits the box around the data it read, so nothing it framed can clamp — reported as a
/// fact rather than left to be assumed, since the fitted edge is arithmetic and arithmetic rounds.
#[test]
fn auto_clamps_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    write_projection_points(&tmp.path().join("points.parquet"));
    declare_extent(tmp.path(), "{ auto = true, margin = 0.0 }");
    let output = build_in(tmp.path(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("none on the frame's edge"),
        "{}",
        stderr(&output)
    );
}

// -------------------------------------------------------------------------------------------
// `--file`, the override
// -------------------------------------------------------------------------------------------

/// `--file` replaces **one object's** source, keyed by the object. Everything it does not name is
/// still the declaration's own path.
#[test]
fn an_override_stages_one_source_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let elsewhere = tempfile::tempdir().unwrap();
    let staged = elsewhere.path().join("staged-points.parquet");
    write_points(&staged);
    std::fs::remove_file(tmp.path().join("points.parquet")).unwrap();
    let output = build_in(
        tmp.path(),
        &["--file", &format!("view:s0={}", staged.display())],
    );
    assert!(output.status.success(), "{}", stderr(&output));
}

/// **An override that names nothing is a refusal**, listing the keys that exist — otherwise the
/// declaration's own path stays quietly in force under a command line asking for another corpus.
#[test]
fn an_override_naming_no_source_refuses_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let stderr = refusal(tmp.path(), &["--file", "pionts=/elsewhere/points.parquet"]);
    assert!(stderr.contains("'pionts=…'"), "{stderr}");
    assert!(stderr.contains("names no source in this config"), "{stderr}");
    assert!(stderr.contains("view:s0"), "{stderr}");
    assert!(
        !tmp.path().join("bundles").exists(),
        "the refusal must happen before any output directory is created"
    );
}

/// One key, one file. Which corpus a source names is not a last-one-wins question.
#[test]
fn one_key_overridden_twice_refuses_the_build() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let stderr = refusal(
        tmp.path(),
        &["--file", "view:s0=a.parquet", "--file", "view:s0=b.parquet"],
    );
    assert!(stderr.contains("bound 'view:s0' twice"), "{stderr}");
}

/// The override's own shape, refused by the value parser rather than read as a path with no key.
#[test]
fn an_override_without_a_key_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let output = build_in(tmp.path(), &["--file", "points.parquet"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--file expects KEY=PATH"),
        "{}",
        stderr(&output)
    );
}

// -------------------------------------------------------------------------------------------
// The identity key
// -------------------------------------------------------------------------------------------

/// **There is no flag that takes a key.** One on a command line reaches shell history, process
/// listings and CI logs, so `--id-key` is gone and is not aliased to anything.
#[test]
fn a_key_on_the_command_line_is_not_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    for flag in ["--id-key", "--id-key-file"] {
        let output = build_in(tmp.path(), &[flag, KEY]);
        assert!(!output.status.success(), "{flag} must not be accepted");
        assert!(
            stderr(&output).contains("unexpected argument"),
            "{flag}: {}",
            stderr(&output)
        );
    }
}

/// A `.env` beside `tessera.toml` supplies the variable, for the working copy that would rather
/// not export one per shell.
#[test]
fn a_dot_env_beside_the_deployment_config_supplies_the_key() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::write(
        tmp.path().join(".env"),
        format!("# this deployment's lineage\nTESSERA_IDENTITY_KEY=\"{KEY}\"\n"),
    )
    .unwrap();
    let output = tessera()
        .arg("build")
        .current_dir(tmp.path())
        .env_remove("TESSERA_IDENTITY_KEY")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

/// **The process environment wins over the `.env`.** An operator who exported a variable for one
/// invocation has said something more specific than a file sitting beside the config.
#[test]
fn the_process_environment_wins_over_the_dot_env() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    std::fs::write(
        tmp.path().join(".env"),
        "TESSERA_IDENTITY_KEY=0f0e0d0c0b0a09080706050403020100\n",
    )
    .unwrap();
    // Build once from the environment, then rebuild carrying the first bundle's key forward. The
    // two agree only if the environment was the source; the `.env`'s different key would refuse.
    assert!(build_in(tmp.path(), &[]).status.success());
    let output = build_in(
        tmp.path(),
        &[
            "--out",
            tmp.path().join("second").to_str().unwrap(),
            "--carry-id-key-from",
            tmp.path().join("bundles/corpus").to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
}

/// With no key anywhere the build refuses **before any work**, naming the variable this
/// deployment's own file asked for rather than a variable it did not.
#[test]
fn no_key_anywhere_refuses_naming_this_deployments_variable() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let deployment = std::fs::read_to_string(tmp.path().join("tessera.toml")).unwrap();
    std::fs::write(
        tmp.path().join("tessera.toml"),
        format!("{deployment}\n[identity]\nenv = \"ACME_TESSERA_KEY\"\n"),
    )
    .unwrap();
    let output = tessera()
        .arg("build")
        .current_dir(tmp.path())
        .env_remove("TESSERA_IDENTITY_KEY")
        .env_remove("ACME_TESSERA_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(stderr.contains("no identity key decision"), "{stderr}");
    assert!(stderr.contains("$ACME_TESSERA_KEY"), "{stderr}");
    assert!(stderr.contains("--identity-file"), "{stderr}");
    assert!(
        !tmp.path().join("bundles").exists(),
        "N-1: the refusal must precede every scrap of work"
    );
}

/// **The key itself never goes in `tessera.toml`**, which belongs in git. Refused with its own
/// message, because the mistake is a reasonable one — `--identity-file` does spell it `key`.
#[test]
fn a_key_written_into_the_deployment_config_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let deployment = std::fs::read_to_string(tmp.path().join("tessera.toml")).unwrap();
    std::fs::write(
        tmp.path().join("tessera.toml"),
        format!("{deployment}\n[identity]\nkey = \"{KEY}\"\n"),
    )
    .unwrap();
    let stderr = refusal(tmp.path(), &[]);
    assert!(stderr.contains("[identity] carries `key`"), "{stderr}");
    assert!(stderr.contains("never appears in this file"), "{stderr}");
    assert!(stderr.contains("--identity-file"), "{stderr}");
}

/// The environment is a *source*, not a fallback: a key that disagrees with a carried lineage
/// refuses, exactly as two flags disagreeing would.
#[test]
fn an_environment_key_disagreeing_with_a_carried_one_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    assert!(build_in(tmp.path(), &[]).status.success());
    let output = tessera()
        .arg("build")
        .args(["--out", tmp.path().join("second").to_str().unwrap()])
        .args([
            "--carry-id-key-from",
            tmp.path().join("bundles/corpus").to_str().unwrap(),
        ])
        .current_dir(tmp.path())
        .env("TESSERA_IDENTITY_KEY", "0f0e0d0c0b0a09080706050403020100")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("identity key sources disagree"),
        "{}",
        stderr(&output)
    );
}

/// `--identity-file` is the `0600`-file route, for the deployment that would rather not have the
/// key readable from `/proc`.
#[test]
fn an_identity_file_supplies_the_key() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let key_file = tmp.path().join("identity.toml");
    std::fs::write(&key_file, format!("[identity]\nkey = \"{KEY}\"\n")).unwrap();
    let output = tessera()
        .arg("build")
        .args(["--identity-file", key_file.to_str().unwrap()])
        .current_dir(tmp.path())
        .env_remove("TESSERA_IDENTITY_KEY")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

/// The declaration's own path is where the schema comes from, and `--config` overrides it.
#[test]
fn the_schema_path_comes_from_the_deployment_config() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let renamed: PathBuf = tmp.path().join("corpus-declaration.toml");
    std::fs::rename(tmp.path().join("schema.toml"), &renamed).unwrap();

    // Without it, the default `schema.toml` is simply absent.
    let stderr = refusal(tmp.path(), &[]);
    assert!(stderr.contains("schema.toml"), "{stderr}");

    let deployment = std::fs::read_to_string(tmp.path().join("tessera.toml")).unwrap();
    std::fs::write(
        tmp.path().join("tessera.toml"),
        format!("{deployment}\n[build]\nschema = \"corpus-declaration.toml\"\n"),
    )
    .unwrap();
    assert!(build_in(tmp.path(), &[]).status.success());
}
