//! Task 7: the `tessera build` identity-key CLI surface (contracts §2.2, plan Critical N-1).
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
        .args(["--extent", "0,10,0,10", "--slice", "s0"])
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

/// `--mint-id-key` produces a fresh, non-degenerate key at epoch 1; carrying it forward via
/// `--carry-id-key-from` reproduces the identical key, epoch, and `tessera_id` column.
#[test]
fn mint_then_carry_reproduces_the_same_key_and_epoch() {
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
        .args(["--extent", "0,10,0,10", "--slice", "s0", "--mint-id-key"])
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
        .args(["--extent", "0,10,0,10", "--slice", "s0"])
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
        manifest_a["identity"]["epoch"],
        manifest_b["identity"]["epoch"]
    );

    let columns_a = std::fs::read(
        out_a
            .join(current_prefix(&out_a))
            .join("partitions/default/slices/s0/segments/seg-0/columns.arrow"),
    )
    .unwrap();
    let columns_b = std::fs::read(
        out_b
            .join(current_prefix(&out_b))
            .join("partitions/default/slices/s0/segments/seg-0/columns.arrow"),
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
        .args(["--extent", "0,10,0,10", "--slice", "s0", "--id-key", key_a])
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
        .args(["--extent", "0,10,0,10", "--slice", "s0"])
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
        .args(["--extent", "0,10,0,10", "--slice", "s0"])
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
    assert_eq!(manifest_b["identity"]["epoch"], 1);
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
