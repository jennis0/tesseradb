//! The `tessera build` identity-key CLI surface (contracts §2.2).
//!
//! Exercises the compiled binary directly (`CARGO_BIN_EXE_tessera`, set automatically for an
//! integration test in the same package as the `tessera` bin target) rather than calling
//! `resolve_identity` in-process, because the load-bearing property is what an **operator**
//! observes: exit code, stderr text, and whether the output directory was touched at all.

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

/// CRITICAL N-1. A build given none of the four identity-key sources refuses, before any work:
/// non-zero exit, a message naming every source, and an output directory that was never created.
#[test]
fn a_build_with_no_identity_key_decision_refuses_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tiny_points(tmp.path());
    let pairs = tiny_pairs(tmp.path());
    let out = tmp.path().join("bundle");

    let output = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out)
        .args(["--extent", "0,10,0,10", "--view", "s0"])
        .output()
        .expect("failed to run tessera binary");

    assert!(
        !output.status.success(),
        "a build with no key flag must refuse"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for flag in [
        "--carry-id-key-from",
        "--id-key-file",
        "--id-key",
        "--mint-id-key",
    ] {
        assert!(
            stderr.contains(flag),
            "refusal must name {flag}, got: {stderr}"
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
    let points = tiny_points(tmp.path());
    let pairs = tiny_pairs(tmp.path());
    let out_a = tmp.path().join("bundle-a");
    let out_b = tmp.path().join("bundle-b");

    let minted = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_a)
        .args(["--extent", "0,10,0,10", "--view", "s0", "--mint-id-key"])
        .output()
        .unwrap();
    assert!(
        minted.status.success(),
        "mint should succeed: {}",
        String::from_utf8_lossy(&minted.stderr)
    );

    let carried = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_b)
        .args(["--extent", "0,10,0,10", "--view", "s0"])
        .args(["--carry-id-key-from"])
        .arg(&out_a)
        .output()
        .unwrap();
    assert!(
        carried.status.success(),
        "carry should succeed: {}",
        String::from_utf8_lossy(&carried.stderr)
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

/// Two key sources that disagree refuse without `--rotate-id-key`, and succeed with it.
#[test]
fn disagreeing_key_sources_refuse_without_rotate_and_succeed_with_it() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tiny_points(tmp.path());
    let pairs = tiny_pairs(tmp.path());
    let out_a = tmp.path().join("bundle-a");
    let out_b = tmp.path().join("bundle-b");

    let key_a = "000102030405060708090a0b0c0d0e0f";
    let key_b = "100f0e0d0c0b0a090807060504030201";

    let first = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_a)
        .args(["--extent", "0,10,0,10", "--view", "s0", "--id-key", key_a])
        .output()
        .unwrap();
    assert!(first.status.success());

    let refused = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_b)
        .args(["--extent", "0,10,0,10", "--view", "s0"])
        .args(["--carry-id-key-from"])
        .arg(&out_a)
        .args(["--id-key", key_b])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "disagreeing key sources must refuse without --rotate-id-key"
    );
    assert!(!out_b.exists());

    let rotated = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_b)
        .args(["--extent", "0,10,0,10", "--view", "s0"])
        .args(["--carry-id-key-from"])
        .arg(&out_a)
        .args(["--id-key", key_b, "--rotate-id-key"])
        .output()
        .unwrap();
    assert!(
        rotated.status.success(),
        "disagreeing key sources with --rotate-id-key must succeed: {}",
        String::from_utf8_lossy(&rotated.stderr)
    );
    let manifest_b = manifest_of(&out_b);
    assert_eq!(manifest_b["identity"]["key"], key_b);
    assert_eq!(manifest_b["identity"]["idset"], 1);
}

/// S2: `--id-key-file` is *the* home for a deployment's key (contracts §2.2), so the idset must
/// travel with it. Before this, a deployment that advanced to idset 2 for a repartition and then
/// rebuilt from its key file silently republished idset 1 — under which a stale pre-repartition
/// `tessera_id` compares equal and is accepted, exactly the failure the idset exists to prevent.
#[test]
fn the_key_file_carries_the_idset_and_idset_flag_overrides_it() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tiny_points(tmp.path());
    let pairs = tiny_pairs(tmp.path());
    let key = "000102030405060708090a0b0c0d0e0f";

    let build_with = |out: &Path, key_file: &Path, extra: &[&str]| {
        let mut cmd = tessera();
        cmd.args(["build", "--points"])
            .arg(&points)
            .args(["--pairs"])
            .arg(&pairs)
            .args(["--out"])
            .arg(out)
            .args(["--extent", "0,10,0,10", "--view", "s0"])
            .args(["--id-key-file"])
            .arg(key_file)
            .args(extra);
        cmd.output().unwrap()
    };

    // (a) No idset in the file: the lineage never advanced, so idset 1.
    let plain = tmp.path().join("plain.toml");
    std::fs::write(&plain, format!("[identity]\nkey = \"{key}\"\n")).unwrap();
    let out_a = tmp.path().join("bundle-a");
    assert!(build_with(&out_a, &plain, &[]).status.success());
    assert_eq!(manifest_of(&out_a)["identity"]["idset"], 1);

    // (b) The file records idset 2 — a normal rebuild must republish 2, not regress to 1.
    let with_idset = tmp.path().join("idset2.toml");
    std::fs::write(
        &with_idset,
        format!("[identity]\nkey = \"{key}\"\nidset = 2\n"),
    )
    .unwrap();
    let out_b = tmp.path().join("bundle-b");
    let built = build_with(&out_b, &with_idset, &[]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert_eq!(
        manifest_of(&out_b)["identity"]["idset"],
        2,
        "a rebuild from a key file recording idset 2 must not republish idset 1"
    );

    // (c) `--idset` accompanies `--id-key-file` and wins over the file.
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
    std::fs::write(&zero, format!("[identity]\nkey = \"{key}\"\nidset = 0\n")).unwrap();
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
    let points = tiny_points(tmp.path());
    let pairs = tiny_pairs(tmp.path());
    let key = "000102030405060708090a0b0c0d0e0f";

    // A bundle at idset 2 (the post-repartition state).
    let out_a = tmp.path().join("bundle-a");
    let key_file = tmp.path().join("key.toml");
    std::fs::write(
        &key_file,
        format!("[identity]\nkey = \"{key}\"\nidset = 2\n"),
    )
    .unwrap();
    let first = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_a)
        .args(["--extent", "0,10,0,10", "--view", "s0"])
        .args(["--id-key-file"])
        .arg(&key_file)
        .output()
        .unwrap();
    assert!(first.status.success());
    assert_eq!(manifest_of(&out_a)["identity"]["idset"], 2);

    // A key file that still says idset 1, carried alongside that bundle: same key, two idsets.
    let stale_file = tmp.path().join("stale.toml");
    std::fs::write(
        &stale_file,
        format!("[identity]\nkey = \"{key}\"\nidset = 1\n"),
    )
    .unwrap();
    let out_b = tmp.path().join("bundle-b");
    let refused = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_b)
        .args(["--extent", "0,10,0,10", "--view", "s0"])
        .args(["--carry-id-key-from"])
        .arg(&out_a)
        .args(["--id-key-file"])
        .arg(&stale_file)
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "two recorded idsets that disagree must refuse"
    );
    assert!(!out_b.exists());

    // `--idset` is how the operator resolves it.
    let resolved = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_b)
        .args(["--extent", "0,10,0,10", "--view", "s0"])
        .args(["--carry-id-key-from"])
        .arg(&out_a)
        .args(["--id-key-file"])
        .arg(&stale_file)
        .args(["--idset", "2"])
        .output()
        .unwrap();
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    assert_eq!(manifest_of(&out_b)["identity"]["idset"], 2);
}

/// S3: the rotation refusal must not print either key. It goes to stderr on a CLI whose own
/// `--id-key` doc warns that keys on a command line reach CI logs — printing both disagreeing
/// keys in full put them there through the refusal path too.
#[test]
fn the_rotation_refusal_prints_fingerprints_not_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tiny_points(tmp.path());
    let pairs = tiny_pairs(tmp.path());
    let key_a = "000102030405060708090a0b0c0d0e0f";
    let key_b = "100f0e0d0c0b0a090807060504030201";
    let out_a = tmp.path().join("bundle-a");
    let out_b = tmp.path().join("bundle-b");

    assert!(tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_a)
        .args(["--extent", "0,10,0,10", "--view", "s0", "--id-key", key_a])
        .output()
        .unwrap()
        .status
        .success());

    let refused = tessera()
        .args(["build", "--points"])
        .arg(&points)
        .args(["--pairs"])
        .arg(&pairs)
        .args(["--out"])
        .arg(&out_b)
        .args(["--extent", "0,10,0,10", "--view", "s0"])
        .args(["--carry-id-key-from"])
        .arg(&out_a)
        .args(["--id-key", key_b])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !stderr.contains(key_a) && !stderr.contains(key_b),
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
