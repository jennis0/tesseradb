//! `tessera items` and `tessera artifacts` against a served bundle: what they write, read back
//! from Arrow IPC and from Parquet, equals the same read made over HTTP and decoded here, and a
//! read they cannot make or cannot write is refused before anything is written.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Array, ArrayRef};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use base64::Engine;
use serde_json::{json, Value};
use tempfile::TempDir;

const KEY: &str = "000102030405060708090a0b0c0d0e0f";

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A generated corpus of 2,000 items built into a bundle with external ids and served, with a
/// token holding a hundred of its terms. The server is stopped when this drops.
struct Served {
    dir: TempDir,
    server: Child,
    viewer: String,
    token: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn serve() -> Served {
    let dir = TempDir::new().unwrap();
    let path = dir.path();
    let materialised = tessera()
        .args(["corpus", "materialise", "--seed", "1", "--n", "2000", "--out"])
        .arg(path)
        .output()
        .unwrap();
    assert!(materialised.status.success(), "{materialised:?}");
    std::fs::rename(path.join("corpus-config.toml"), path.join("schema.toml")).unwrap();
    std::fs::write(path.join("session.cred"), "session-credential").unwrap();
    std::fs::write(path.join("operator.cred"), "operator-credential").unwrap();
    let (viewer, session) = (free_port(), free_port());
    std::fs::write(
        path.join("tessera.toml"),
        format!(
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
viewer  = "127.0.0.1:{viewer}"
session = "127.0.0.1:{session}"
control = "127.0.0.1:{}"
session_credential_file  = "session.cred"
operator_credential_file = "operator.cred"
"#,
            free_port()
        ),
    )
    .unwrap();
    let built = tessera()
        .args(["build", "--mint-external-ids"])
        .current_dir(path)
        .env("TESSERA_IDENTITY_KEY", KEY)
        .output()
        .unwrap();
    assert!(built.status.success(), "{built:?}");

    let server = tessera()
        .arg("serve")
        .current_dir(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut served = Served {
        dir,
        server,
        viewer: format!("http://127.0.0.1:{viewer}"),
        token: String::new(),
    };
    let started = Instant::now();
    while !tessera()
        .arg("health")
        .current_dir(served.dir.path())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
    {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the server did not become ready within a minute"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let terms: Vec<String> = std::iter::once("public".to_string())
        .chain((0..100).map(|term| term.to_string()))
        .collect();
    let auth_data = base64::engine::general_purpose::STANDARD
        .encode(json!({ "terms": terms }).to_string());
    let answer: Value = serde_json::from_slice(&post(
        &format!("http://127.0.0.1:{session}/session/authorise"),
        "session-credential",
        &json!({ "auth_data": auth_data }),
    ))
    .unwrap();
    served.token = answer["token"].as_str().unwrap().to_owned();
    served
}

/// One POST of `body` as JSON: the answer's body.
fn post(url: &str, bearer: &str, body: &Value) -> Vec<u8> {
    let response = reqwest::blocking::Client::new()
        .post(url)
        .bearer_auth(bearer)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .unwrap();
    assert!(response.status().is_success(), "{url}: {}", response.status());
    response.bytes().unwrap().to_vec()
}

impl Served {
    /// A read made over HTTP, following each response's cursor until it is null: every page,
    /// and the number of responses.
    fn http_read(&self, route: &str, body: Value) -> (Vec<RecordBatch>, usize) {
        let (mut pages, mut responses, mut request) = (Vec::new(), 0, body.clone());
        loop {
            let bytes = post(&format!("{}/v1/{route}", self.viewer), &self.token, &request);
            responses += 1;
            let mut trailer = None;
            for (kind, payload) in tessera_wire::split_frames(&bytes).unwrap() {
                match kind {
                    tessera_wire::FRAME_RECORDS => pages.extend(decode_stream(payload)),
                    tessera_wire::FRAME_TRAILER => {
                        trailer = Some(serde_json::from_slice::<Value>(payload).unwrap())
                    }
                    _ => {}
                }
            }
            let next = trailer.expect("every response has a trailer")["next"].clone();
            if next.is_null() {
                return (pages, responses);
            }
            request = body.clone();
            request.as_object_mut().unwrap().remove("count");
            request["cursor"] = next;
        }
    }

    /// `tessera <args>` with this server and token, stdout to `stdout`, within a minute.
    fn run(&self, args: &[&str], stdout: &Path) -> ExitStatus {
        let mut command = tessera();
        command
            .args(args)
            .args(["--server", &self.viewer, "--token", &self.token])
            .current_dir(self.dir.path());
        finish(command, stdout)
    }
}

/// Run `command` with stdout to a file, killing it and failing if it runs for a minute: a read
/// that never ends is a failure, not a hang.
fn finish(mut command: Command, stdout: &Path) -> ExitStatus {
    let mut child = command
        .stdout(std::fs::File::create(stdout).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{command:?} ran for a minute");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn decode_stream(bytes: &[u8]) -> Vec<RecordBatch> {
    arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None)
        .expect("an Arrow IPC stream")
        .map(|batch| batch.unwrap())
        .collect()
}

fn read_ipc(path: &Path) -> Vec<RecordBatch> {
    decode_stream(&std::fs::read(path).unwrap())
}

fn read_parquet(path: &Path) -> Vec<RecordBatch> {
    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        std::fs::File::open(path).unwrap(),
    )
    .unwrap()
    .build()
    .unwrap()
    .map(|batch| batch.unwrap())
    .collect()
}

/// `batches` as one batch with every dictionary column decoded to its values, so that two reads
/// of the same rows compare equal however their pages and dictionaries fell.
fn plain(batches: &[RecordBatch]) -> RecordBatch {
    let schema = batches[0].schema();
    let whole = arrow::compute::concat_batches(&schema, batches).unwrap();
    let (fields, columns): (Vec<Field>, Vec<ArrayRef>) = schema
        .fields()
        .iter()
        .zip(whole.columns())
        .map(|(field, column)| match field.data_type() {
            DataType::Dictionary(_, values) => (
                field.as_ref().clone().with_data_type(values.as_ref().clone()),
                arrow::compute::cast(column, values).unwrap(),
            ),
            _ => (field.as_ref().clone(), Arc::clone(column)),
        })
        .unzip();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
}

fn rows(batches: &[RecordBatch]) -> usize {
    batches.iter().map(RecordBatch::num_rows).sum()
}

const ITEMS: &[&str] = &[
    "items",
    "--view",
    "s0",
    "--fields",
    "fx_key,bay,tag,seen_at,weight",
    "--system-fields",
    "position,external_id,labels",
    "--order",
    "map",
    "--page-rows",
    "97",
    "--pages",
    "2",
];

fn items_body() -> Value {
    json!({
        "view": "s0",
        "fields": ["fx_key", "bay", "tag", "seen_at", "weight"],
        "system_fields": ["position", "external_id", "labels"],
        "order": "map",
    })
}

#[test]
fn an_items_read_written_as_ipc_and_as_parquet_is_the_read_over_http() {
    let served = serve();
    let (whole, _) = served.http_read("items", items_body());
    let mut paged_body = items_body();
    paged_body["page_rows"] = 97.into();
    paged_body["pages"] = 2.into();
    let (paged, responses) = served.http_read("items", paged_body);
    assert!(responses > 2, "the read spans several responses");
    assert!(rows(&whole) > 1000);
    // Every column holds values, so an equality below is not an equality of nulls.
    let values = plain(&whole);
    for name in ["fx_key", "bay", "tag", "seen_at", "weight", "tessera:external_id"] {
        let column = values.column_by_name(name).unwrap();
        assert!(column.null_count() < column.len(), "{name} holds values");
    }

    let out = served.dir.path();
    let ipc = out.join("items.arrows");
    let parquet = out.join("items.parquet");
    for path in [&ipc, &parquet] {
        let mut args = ITEMS.to_vec();
        args.extend(["--out", path.to_str().unwrap()]);
        assert!(served.run(&args, &out.join("stdout")).success());
    }

    let from_ipc = read_ipc(&ipc);
    assert_eq!(from_ipc.len(), paged.len(), "one batch for each page");
    for (written, page) in from_ipc.iter().zip(&paged) {
        assert_eq!(plain(&[written.clone()]), plain(&[page.clone()]));
    }
    let from_parquet = read_parquet(&parquet);
    assert_eq!(from_parquet[0].schema().fields(), whole[0].schema().fields());
    assert_eq!(plain(&from_parquet), plain(&whole));
    assert_eq!(plain(&from_ipc), plain(&whole));
}

#[test]
fn stdout_carries_the_read_in_the_format_named() {
    let served = serve();
    let (whole, _) = served.http_read("items", items_body());
    let out = served.dir.path();

    let mut args = ITEMS.to_vec();
    args.extend(["--format", "ipc", "--compression", "zstd"]);
    assert!(served.run(&args, &out.join("stdout.arrows")).success());
    assert_eq!(plain(&read_ipc(&out.join("stdout.arrows"))), plain(&whole));

    let mut args = ITEMS.to_vec();
    args.extend(["--format", "parquet", "--out", "-"]);
    assert!(served.run(&args, &out.join("stdout.parquet")).success());
    assert_eq!(plain(&read_parquet(&out.join("stdout.parquet"))), plain(&whole));
}

#[test]
fn a_read_goes_on_from_the_cursor_it_is_given() {
    let served = serve();
    let (whole, _) = served.http_read("items", items_body());
    let mut first = items_body();
    first["page_rows"] = 500.into();
    first["pages"] = 1.into();
    let bytes = post(&format!("{}/v1/items", served.viewer), &served.token, &first);
    let frames = tessera_wire::split_frames(&bytes).unwrap();
    let trailer: Value = serde_json::from_slice(frames.last().unwrap().1).unwrap();
    let cursor = trailer["next"].as_str().unwrap();

    let out = served.dir.path().join("rest.arrows");
    let mut args = ITEMS.to_vec();
    args.extend(["--cursor", cursor, "--out", out.to_str().unwrap()]);
    assert!(served.run(&args, &served.dir.path().join("stdout")).success());
    let whole = plain(&whole);
    assert_eq!(plain(&read_ipc(&out)), whole.slice(500, whole.num_rows() - 500));
}

#[test]
fn an_artifacts_read_is_the_layer_read_over_http() {
    let served = serve();
    let (whole, _) = served.http_read(
        "artifacts",
        json!({
            "view": "s0",
            "layer": "generator/flat",
            "fields": ["key", "level", "parents", "masked_count", "centroid"],
        }),
    );
    assert!(rows(&whole) > 4);
    let out = served.dir.path().join("flat.parquet");
    let args = [
        "artifacts",
        "--view",
        "s0",
        "--layer",
        "generator/flat",
        "--fields",
        "key,level,parents,masked_count,centroid",
        "--page-rows",
        "4",
        "--pages",
        "1",
        "--out",
        out.to_str().unwrap(),
    ];
    assert!(served.run(&args, &served.dir.path().join("stdout")).success());
    let written = read_parquet(&out);
    assert_eq!(written[0].schema().fields(), whole[0].schema().fields());
    assert_eq!(plain(&written), plain(&whole));
}

#[test]
fn a_read_without_a_token_is_refused_and_writes_nothing() {
    let served = serve();
    let out = served.dir.path().join("refused.parquet");
    let mut command = tessera();
    command
        .args(ITEMS)
        .args(["--server", &served.viewer, "--out", out.to_str().unwrap()])
        .env_remove("TESSERA_TOKEN");
    assert!(!finish(command, &served.dir.path().join("stdout")).success());
    assert!(!out.exists());

    let mut command = tessera();
    command
        .args(ITEMS)
        .args(["--server", &served.viewer, "--out", out.to_str().unwrap()])
        .env("TESSERA_TOKEN", "not-a-token");
    assert!(!finish(command, &served.dir.path().join("stdout")).success());
    assert!(!out.exists());

    let mut command = tessera();
    command
        .args(ITEMS)
        .args(["--server", &served.viewer, "--out", out.to_str().unwrap()])
        .env("TESSERA_TOKEN", &served.token);
    assert!(finish(command, &served.dir.path().join("stdout")).success());
    assert!(out.exists());
}

/// No server is needed: the output is checked before anything is asked.
#[test]
fn a_format_neither_named_nor_given_by_the_extension_is_refused() {
    let tmp = TempDir::new().unwrap();
    let refused = |extra: &[&str]| {
        let mut command = tessera();
        command
            .args(ITEMS)
            .args(["--server", "http://127.0.0.1:9", "--token", "t"])
            .args(extra)
            .current_dir(tmp.path());
        let status = finish(command, &tmp.path().join("stdout"));
        let written: Vec<PathBuf> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| !path.ends_with("stdout"))
            .collect();
        !status.success()
            && written.is_empty()
            && std::fs::metadata(tmp.path().join("stdout")).unwrap().len() == 0
    };
    assert!(refused(&[]), "stdout with no format");
    assert!(refused(&["--out", "-"]), "stdout with no format");
    assert!(refused(&["--out", "items.txt"]), "an extension naming no format");
    assert!(refused(&["--out", "items"]), "no extension");
    assert!(refused(&["--out", "items.parquet", "--format", "ipc"]), "a disagreement");
    assert!(refused(&["--out", "items.arrows", "--format", "parquet"]), "a disagreement");
}
