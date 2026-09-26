//! The key `tessera build` generates for each bundle it creates: stored in the manifest, never
//! configured, and never printed.

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

/// `tessera build` in `dir`, writing to `out`.
fn build(dir: &Path, out: &Path) -> std::process::Output {
    tessera()
        .arg("build")
        .arg("--out")
        .arg(out)
        .current_dir(dir)
        .output()
        .expect("failed to run tessera binary")
}

/// **Every build generates its own key.** Two builds of the same data store two different
/// keys, each one that the manifest's reader accepts, and neither key appears in what the build
/// prints.
#[test]
fn each_build_generates_its_own_key_and_prints_none() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    let mut keys = Vec::new();
    for name in ["bundle-a", "bundle-b"] {
        let out = tmp.path().join(name);
        let output = build(tmp.path(), &out);
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "the build should succeed: {printed}");
        let key = manifest_of(&out)["identity"]["key"]
            .as_str()
            .expect("the manifest records the key")
            .to_string();
        tessera_types::IdentityKey::from_hex(&key).expect("the stored key is a valid key");
        assert!(!printed.contains(&key), "the build printed its key: {printed}");
        keys.push(key);
    }
    assert_ne!(keys[0], keys[1], "two builds generated the same key");
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
