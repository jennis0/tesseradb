//! The `tessera build` identity-key CLI surface (contracts §2.2).
//!
//! Exercises the compiled binary directly (`CARGO_BIN_EXE_tessera`, set automatically for an
//! integration test in the same package as the `tessera` bin target) rather than calling
//! `resolve_identity` in-process, because the load-bearing property is what an **operator**
//! observes: exit code, stderr text, and whether the output directory was touched at all.
//!
//! **The key arrives through the environment**, which is why every case here sets or clears a
//! variable rather than passing a flag. There is no flag that takes a key: one on a command line
//! reaches shell history, process listings and CI logs.

use std::path::Path;
use std::process::Command;

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

fn tiny_points(dir: &Path) -> std::path::PathBuf {
    use arrow::array::{ArrayRef, Float64Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    let path = dir.join("points.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..8).collect();
    let xs: Vec<f64> = ids.iter().map(|e| *e as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| *e as f64).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)) as ArrayRef,
            Arc::new(Float64Array::from(xs)) as ArrayRef,
            Arc::new(Float64Array::from(ys)) as ArrayRef,
        ],
    )
    .unwrap();
    let file = std::fs::File::create(&path).unwrap();
    let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    path
}

fn tiny_pairs(dir: &Path) -> std::path::PathBuf {
    use arrow::array::{ArrayRef, UInt32Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    let path = dir.join("pairs.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(vec![0u64, 1, 2])) as ArrayRef,
            Arc::new(UInt32Array::from(vec![0u32, 1, 0])) as ArrayRef,
        ],
    )
    .unwrap();
    let file = std::fs::File::create(&path).unwrap();
    let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    path
}

/// The smallest project a build can run in: a deployment file, a declaration naming its own two
/// sources, and the sources beside them (`configuration.md` §3).
fn project(dir: &Path) {
    std::fs::write(
        dir.join("tessera.toml"),
        r#"
[bundle]
path  = "bundle"
cache = "cache"
wal   = "wal.log"

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
        "[sources]\npoints = \"points.parquet\"\npairs = \"pairs.parquet\"\n\
         [[view]]\nname = \"s0\"\nextent = { min = 0.0, max = 10.0 }\n\
         source = \"points\"\n\
         point_visibility = { source = \"pairs\", default = \"public\" }\n",
    )
    .unwrap();
    tiny_points(dir);
    tiny_pairs(dir);
}

/// The variable this deployment's `tessera.toml` names, by default.
const KEY_VAR: &str = "TESSERA_IDENTITY_KEY";

/// `tessera build` in `dir`, with `key` in the environment when there is one, writing to `out`.
///
/// Every invocation is the *ordinary* one plus whatever the case is about: no `--config`, no
/// `--extent`, no `--view`, no `--file`.
fn build(dir: &Path, out: &Path, key: Option<&str>, extra: &[&str]) -> std::process::Output {
    let mut cmd = tessera();
    cmd.arg("build")
        .args(["--out"])
        .arg(out)
        .args(extra)
        .current_dir(dir)
        .env_remove(KEY_VAR);
    if let Some(key) = key {
        cmd.env(KEY_VAR, key);
    }
    cmd.output().expect("failed to run tessera binary")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// CRITICAL N-1. A build given none of the four identity-key sources refuses, before any work:
/// non-zero exit, a message naming every source, and an output directory that was never created.
#[test]
fn a_build_with_no_identity_key_decision_refuses_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let out = tmp.path().join("bundle");

    let output = build(tmp.path(), &out, None, &[]);
    assert!(
        !output.status.success(),
        "a build with no key source must refuse"
    );
    let stderr = stderr(&output);
    for source in [
        "$TESSERA_IDENTITY_KEY",
        ".env",
        "--carry-id-key-from",
        "--identity-file",
        "--mint-id-key",
    ] {
        assert!(
            stderr.contains(source),
            "refusal must name {source}, got: {stderr}"
        );
    }
    assert!(
        !out.exists(),
        "the refusal must happen before any output directory is created"
    );
}

/// `--mint-id-key` produces a fresh, non-degenerate key at idset 1; carrying it forward via
/// `--carry-id-key-from` reproduces the identical key, idset, and `tessera_id` column.
#[test]
fn mint_then_carry_reproduces_the_same_key_and_idset() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let out_a = tmp.path().join("bundle-a");
    let out_b = tmp.path().join("bundle-b");

    let minted = build(tmp.path(), &out_a, None, &["--mint-id-key"]);
    assert!(minted.status.success(), "mint should succeed: {}", stderr(&minted));

    let carried = build(
        tmp.path(),
        &out_b,
        None,
        &["--carry-id-key-from", out_a.to_str().unwrap()],
    );
    assert!(
        carried.status.success(),
        "carry should succeed: {}",
        stderr(&carried)
    );

    let manifest_a = manifest_of(&out_a);
    let manifest_b = manifest_of(&out_b);
    assert_eq!(manifest_a["identity"]["key"], manifest_b["identity"]["key"]);
    assert_eq!(
        manifest_a["identity"]["idset"],
        manifest_b["identity"]["idset"]
    );

    let columns_a = std::fs::read(
        out_a
            .join(current_prefix(&out_a))
            .join("partitions/default/views/s0/segments/seg-0/columns.arrow"),
    )
    .unwrap();
    let columns_b = std::fs::read(
        out_b
            .join(current_prefix(&out_b))
            .join("partitions/default/views/s0/segments/seg-0/columns.arrow"),
    )
    .unwrap();
    assert_eq!(
        columns_a, columns_b,
        "carrying the same key forward must reproduce a byte-identical columns.arrow"
    );
}

/// **Minting is a decision, not a fallback.** A key already in the environment and `--mint-id-key`
/// together is refused rather than resolved by precedence: one of the two starts a new lineage and
/// the other restores one, and guessing which was meant invalidates every `tessera_id` a client
/// holds.
#[test]
fn minting_beside_an_environment_key_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let output = build(
        tmp.path(),
        &tmp.path().join("bundle"),
        Some(KEY_A),
        &["--mint-id-key"],
    );
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(stderr.contains("cannot be combined"), "{stderr}");
    assert!(stderr.contains("$TESSERA_IDENTITY_KEY"), "{stderr}");
}

const KEY_A: &str = "000102030405060708090a0b0c0d0e0f";
const KEY_B: &str = "100f0e0d0c0b0a090807060504030201";

/// Two key sources that disagree refuse without `--rotate-id-key`, and succeed with it.
#[test]
fn disagreeing_key_sources_refuse_without_rotate_and_succeed_with_it() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let out_a = tmp.path().join("bundle-a");
    let out_b = tmp.path().join("bundle-b");

    assert!(build(tmp.path(), &out_a, Some(KEY_A), &[]).status.success());

    let carry = ["--carry-id-key-from", out_a.to_str().unwrap()];
    let refused = build(tmp.path(), &out_b, Some(KEY_B), &carry);
    assert!(
        !refused.status.success(),
        "disagreeing key sources must refuse without --rotate-id-key"
    );
    assert!(!out_b.exists());

    let rotated = build(
        tmp.path(),
        &out_b,
        Some(KEY_B),
        &[carry[0], carry[1], "--rotate-id-key"],
    );
    assert!(
        rotated.status.success(),
        "disagreeing key sources with --rotate-id-key must succeed: {}",
        stderr(&rotated)
    );
    let manifest_b = manifest_of(&out_b);
    assert_eq!(manifest_b["identity"]["key"], KEY_B);
    assert_eq!(manifest_b["identity"]["idset"], 1);
}

/// S2: `--identity-file` is one home for a deployment's key (contracts §2.2), so the idset must
/// travel with it. Before this, a deployment that advanced to idset 2 for a repartition and then
/// rebuilt from its key file silently republished idset 1 — under which a stale pre-repartition
/// `tessera_id` compares equal and is accepted, exactly the failure the idset exists to prevent.
#[test]
fn the_identity_file_carries_the_idset_and_idset_flag_overrides_it() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());

    let build_with = |out: &Path, key_file: &Path, extra: &[&str]| {
        let mut args = vec!["--identity-file", key_file.to_str().unwrap()];
        args.extend_from_slice(extra);
        build(tmp.path(), out, None, &args)
    };

    // (a) No idset in the file: the lineage never advanced, so idset 1.
    let plain = tmp.path().join("plain.toml");
    std::fs::write(&plain, format!("[identity]\nkey = \"{KEY_A}\"\n")).unwrap();
    let out_a = tmp.path().join("bundle-a");
    assert!(build_with(&out_a, &plain, &[]).status.success());
    assert_eq!(manifest_of(&out_a)["identity"]["idset"], 1);

    // (b) The file records idset 2 — a normal rebuild must republish 2, not regress to 1.
    let with_idset = tmp.path().join("idset2.toml");
    std::fs::write(
        &with_idset,
        format!("[identity]\nkey = \"{KEY_A}\"\nidset = 2\n"),
    )
    .unwrap();
    let out_b = tmp.path().join("bundle-b");
    let built = build_with(&out_b, &with_idset, &[]);
    assert!(built.status.success(), "{}", stderr(&built));
    assert_eq!(
        manifest_of(&out_b)["identity"]["idset"],
        2,
        "a rebuild from a key file recording idset 2 must not republish idset 1"
    );

    // (c) `--idset` accompanies `--identity-file` and wins over the file.
    let out_c = tmp.path().join("bundle-c");
    assert!(build_with(&out_c, &with_idset, &["--idset", "5"])
        .status
        .success());
    assert_eq!(manifest_of(&out_c)["identity"]["idset"], 5);

    // (d) `--bump-idset` still advances from whatever the file said.
    let out_d = tmp.path().join("bundle-d");
    assert!(build_with(&out_d, &with_idset, &["--bump-idset"])
        .status
        .success());
    assert_eq!(manifest_of(&out_d)["identity"]["idset"], 3);

    // (e) An idset of 0 is refused where the operator can still see which file said it.
    let zero = tmp.path().join("zero.toml");
    std::fs::write(&zero, format!("[identity]\nkey = \"{KEY_A}\"\nidset = 0\n")).unwrap();
    let out_e = tmp.path().join("bundle-e");
    let refused = build_with(&out_e, &zero, &[]);
    assert!(!refused.status.success());
    assert!(!out_e.exists());
}

/// Two *recorded* idsets that disagree are refused rather than silently ranked: whichever were
/// picked, the other could be the true one, and picking the lower one is fail-open.
#[test]
fn disagreeing_idset_sources_refuse_until_idset_is_stated() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());

    // A bundle at idset 2 (the post-repartition state).
    let out_a = tmp.path().join("bundle-a");
    let key_file = tmp.path().join("key.toml");
    std::fs::write(
        &key_file,
        format!("[identity]\nkey = \"{KEY_A}\"\nidset = 2\n"),
    )
    .unwrap();
    let first = build(
        tmp.path(),
        &out_a,
        None,
        &["--identity-file", key_file.to_str().unwrap()],
    );
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(manifest_of(&out_a)["identity"]["idset"], 2);

    // A key file that still says idset 1, carried alongside that bundle: same key, two idsets.
    let stale_file = tmp.path().join("stale.toml");
    std::fs::write(
        &stale_file,
        format!("[identity]\nkey = \"{KEY_A}\"\nidset = 1\n"),
    )
    .unwrap();
    let out_b = tmp.path().join("bundle-b");
    let stale = [
        "--carry-id-key-from",
        out_a.to_str().unwrap(),
        "--identity-file",
        stale_file.to_str().unwrap(),
    ];
    let refused = build(tmp.path(), &out_b, None, &stale);
    assert!(
        !refused.status.success(),
        "two recorded idsets that disagree must refuse"
    );
    assert!(!out_b.exists());

    // `--idset` is how the operator resolves it.
    let resolved = build(
        tmp.path(),
        &out_b,
        None,
        &[stale[0], stale[1], stale[2], stale[3], "--idset", "2"],
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    assert_eq!(manifest_of(&out_b)["identity"]["idset"], 2);
}

/// S3: the rotation refusal must not print either key. Key material in a refusal reaches CI logs
/// exactly as a key on a command line would — which is why there is no flag that takes one.
#[test]
fn the_rotation_refusal_prints_fingerprints_not_keys() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let out_a = tmp.path().join("bundle-a");
    let out_b = tmp.path().join("bundle-b");

    assert!(build(tmp.path(), &out_a, Some(KEY_A), &[]).status.success());

    let refused = build(
        tmp.path(),
        &out_b,
        Some(KEY_B),
        &["--carry-id-key-from", out_a.to_str().unwrap()],
    );
    assert!(!refused.status.success());
    let stderr = stderr(&refused);
    assert!(
        !stderr.contains(KEY_A) && !stderr.contains(KEY_B),
        "the refusal must not print key material, got: {stderr}"
    );
    assert!(
        stderr.matches("fp:").count() >= 2,
        "the refusal must fingerprint each disagreeing source, got: {stderr}"
    );
}

fn current_prefix(bundle: &Path) -> String {
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("CURRENT")).unwrap()).unwrap();
    current["prefix"].as_str().unwrap().to_string()
}

fn manifest_of(bundle: &Path) -> serde_json::Value {
    let prefix = current_prefix(bundle);
    serde_json::from_slice(&std::fs::read(bundle.join(prefix).join("MANIFEST.json")).unwrap())
        .unwrap()
}
