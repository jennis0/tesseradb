//! The `tessera corpus` verbs (correctness-suite §12.1), exercised through the compiled binary —
//! what the Python driver actually invokes — and the corpus's schema declaration run through the
//! build's real parser, which is the one consumer `tessera-corpus`'s own tests cannot reach
//! without breaching §13's dependency rule.

use std::io::Write;
use std::process::{Command, Stdio};

use arrow::array::{Float64Array, UInt64Array};
use arrow::record_batch::RecordBatch;
use tessera_corpus::{Corpus, Grant};
use tessera_spatial::Bounds;

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

fn grid() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 65536.0,
        y_min: 0.0,
        y_max: 65536.0,
    }
}

fn decode_stream(bytes: &[u8]) -> Vec<RecordBatch> {
    arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None)
        .expect("stdout is an Arrow IPC stream")
        .map(|batch| batch.unwrap())
        .collect()
}

/// `items` with fx_keys on stdin answers each key with its item — the same answers the library
/// gives, through the surface the driver uses.
#[test]
fn items_answers_served_keys_with_their_items() {
    let corpus = Corpus::new(41, 0, grid()).unwrap();
    let keys: Vec<u64> = (0..64).map(|e| corpus.item(e).fx_key).collect();
    let stdin_text: String = keys.iter().map(|k| format!("{k}\n")).collect();

    let mut child = tessera()
        .args(["corpus", "items", "--seed", "41", "--ids", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());

    let batches = decode_stream(&out.stdout);
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, keys.len());
    let batch = &batches[0];
    let fx = batch
        .column_by_name("fx_key")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let e_col = batch
        .column_by_name("e")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let x = batch
        .column_by_name("x")
        .unwrap()
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    for (i, key) in keys.iter().enumerate() {
        assert_eq!(fx.value(i), *key);
        assert_eq!(e_col.value(i), i as u64, "the key inverts to its item");
        assert_eq!(x.value(i), corpus.item(i as u64).x);
    }
}

/// `census` reproduces the library's counts over the binary boundary, byte-decoded rather than
/// trusted.
#[test]
fn census_matches_the_library_census() {
    let corpus = Corpus::new(42, 4_096, grid()).unwrap();
    let grant = Grant::parse("0,1,2,64").unwrap();
    let expected = corpus.census(3, &grant);

    let out = tessera()
        .args([
            "corpus", "census", "--seed", "42", "--n", "4096", "--zoom", "3", "--grant", "0,1,2,64",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut got = Vec::new();
    for batch in decode_stream(&out.stdout) {
        let tiles = batch
            .column_by_name("tile")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        let counts = batch
            .column_by_name("count")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        for i in 0..batch.num_rows() {
            got.push((tiles.value(i), counts.value(i)));
        }
    }
    assert_eq!(got, expected);
}

/// A grant typo is a refusal naming the descriptor, and an over-deep zoom a refusal naming the
/// grid — never an empty count vector wearing a success's exit code.
#[test]
fn census_refuses_bad_grants_and_depths() {
    let out = tessera()
        .args([
            "corpus", "census", "--seed", "1", "--n", "10", "--zoom", "2", "--grant", "cs.LG",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cs.LG"));

    let out = tessera()
        .args([
            "corpus", "census", "--seed", "1", "--n", "10", "--zoom", "17", "--grant", "0",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("16"));
}

/// A non-decimal id line is a refusal naming the line — the caller is not feeding fx_keys, and a
/// silently shortened answer would be compared as if complete.
#[test]
fn items_refuses_a_non_decimal_key() {
    let mut child = tessera()
        .args(["corpus", "items", "--seed", "1", "--ids", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"12\nnot-a-key\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not-a-key"));
}

/// The corpus's declaration parses under the build's own `schema.toml` parser — the loop from
/// generator to `tessera build --config` closed with the real consumer, so a drifted spelling in
/// the generated declaration fails here rather than at the first suite run.
#[test]
fn the_corpus_schema_parses_under_the_builds_parser() {
    let corpus = Corpus::new(1, 0, grid()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schema.toml");
    std::fs::write(&path, corpus.config_toml()).unwrap();

    // No bindings: the declaration names its own files, relative to itself, and the generator
    // writes them beside it (`configuration.md` §3). Nothing here opens them — the schema half of
    // the parse is what this case is about.
    let config = tessera_build::config::Config::parse(&path, &Default::default())
        .expect("the corpus schema must parse");
    // The declaration also has to *acquire*: the generator's view name, its geometry source and
    // its label relation must be the ones the build asks for. Checked here rather than left to
    // the suite, which cannot run without a corpus on disk.
    let acquired = config
        .acquire()
        .expect("the corpus config acquires its own inputs");
    let registry = config.build_views().expect("the registry compiles");
    let view = tessera_build::config::acquire_view(&registry[0])
        .expect("the view acquires its own inputs");
    assert_eq!(view.points, dir.path().join("points.parquet"));
    assert!(
        matches!(
            &view.access.source,
            tessera_build::config::AccessSource::Relation(p) if *p == dir.path().join("pairs.parquet")
        ),
        "{:?}",
        view.access
    );
    assert_eq!(
        acquired.attribute_sources.len(),
        1,
        "one file carries every declared column"
    );
    assert_eq!(
        acquired.attribute_sources[0].path,
        dir.path().join("points.parquet")
    );
    let schema = config.schema;
    let names: Vec<&str> = schema.attributes.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "fx_key",
            "weight",
            "seen_at",
            "bay",
            "tag",
            "blurb",
            "partition"
        ]
    );
    let fx = &schema.attributes[0];
    assert!(fx.render, "the planted join column must be served");
}
