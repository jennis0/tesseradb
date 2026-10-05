//! `tessera items` and `tessera artifacts` against a served bundle. What they write, read back
//! from Arrow IPC and from Parquet, equals the same read made over HTTP and decoded here. The
//! requests they send are the arguments given. A read cut short or refused part of the way keeps
//! its whole pages and names the cursor to read the rest with, and a read they cannot make or
//! cannot write sends nothing and writes nothing.
//!
//! A proxy in front of the server records each request and can cut a body as the server does
//! when it aborts one: a chunked body whose connection closes before the body ends.

mod common;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arrow::array::{Array, ArrayRef};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{deployment, tessera, Ports, Server, OPERATOR_CREDENTIAL};

/// A served deployment, a token holding a hundred of its terms, and a proxy in front of it.
struct Served {
    dir: TempDir,
    _server: Server,
    _bound: common::Bound,
    viewer: String,
    token: String,
    proxy: Proxy,
}

fn serve() -> Served {
    let dir = TempDir::new().unwrap();
    deployment(dir.path(), &Ports::chosen());
    let (server, bound) = Server::announced(dir.path());
    // A principal holding a hundred of the corpus's terms, logged in by an API key.
    let control = |path: &str, body: Value| {
        post(&format!("{}{path}", bound.control), OPERATOR_CREDENTIAL, &body)
    };
    control("/control/principals", json!({ "name": "reader", "kind": "service" }));
    control("/control/grants", json!({ "principal": "reader", "permission": "read" }));
    for term in 0..100 {
        control(
            "/control/grants",
            json!({ "principal": "reader", "term": term.to_string() }),
        );
    }
    let key: Value =
        serde_json::from_slice(&control("/control/principals/reader/keys", json!({}))).unwrap();
    let answer: Value = serde_json::from_slice(&post(
        &format!("{}/v1/login", bound.viewer),
        "",
        &json!({ "api_key": key["key"] }),
    ))
    .unwrap();
    Served {
        dir,
        _server: server,
        proxy: Proxy::start(bound.viewer.clone()),
        viewer: bound.viewer.clone(),
        _bound: bound,
        token: answer["token"].as_str().unwrap().to_owned(),
    }
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

/// What the proxy does with one bulk read in place of passing the server's answer on.
#[derive(Clone, Copy)]
enum Step {
    /// Close the connection at the offset this gives into the body, before the chunked body ends.
    Cut(fn(&[u8]) -> usize),
    /// Answer with this status.
    Refuse(u16),
}

/// A proxy in front of the viewer plane. It records every request's body and passes the server's
/// answer on as a chunked body, unless a step is planned for that request.
struct Proxy {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    plan: Arc<Mutex<VecDeque<Option<Step>>>>,
}

impl Proxy {
    fn start(upstream: String) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
        let plan: Arc<Mutex<VecDeque<Option<Step>>>> = Arc::default();
        let (recorded, planned) = (Arc::clone(&bodies), Arc::clone(&plan));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                forward(stream, &upstream, &recorded, &planned);
            }
        });
        Proxy { url, bodies, plan }
    }

    /// The steps for the next requests, in order; `None` passes one on unchanged.
    fn plan(&self, steps: impl IntoIterator<Item = Option<Step>>) {
        self.plan.lock().unwrap().extend(steps);
    }

    /// Every request body received, in order, and forget them.
    fn bodies(&self) -> Vec<Value> {
        std::mem::take(&mut *self.bodies.lock().unwrap())
    }
}

fn forward(
    mut stream: TcpStream,
    upstream: &str,
    bodies: &Mutex<Vec<Value>>,
    plan: &Mutex<VecDeque<Option<Step>>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let path = line.split_whitespace().nth(1).unwrap().to_owned();
    let (mut length, mut bearer) = (0, String::new());
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let Some((name, value)) = header.trim_end().split_once(':') else {
            break;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap(),
            "authorization" => bearer = value.trim().to_owned(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    bodies.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
    let step = plan.lock().unwrap().pop_front().flatten();

    let answer = reqwest::blocking::Client::new()
        .post(format!("{upstream}{path}"))
        .header(reqwest::header::AUTHORIZATION, bearer)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .unwrap();
    let mut status = answer.status().as_u16();
    let mut content = answer.bytes().unwrap().to_vec();
    let mut end = content.len();
    match step {
        Some(Step::Refuse(code)) => {
            status = code;
            content = br#"{"error":"planned by the test"}"#.to_vec();
        }
        Some(Step::Cut(at)) => end = at(&content),
        None => {}
    }
    if status != 200 {
        let head = format!(
            "HTTP/1.1 {status} Refused\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n",
            content.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&content);
        return;
    }
    let head = "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                transfer-encoding: chunked\r\nconnection: close\r\n\r\n";
    let _ = stream.write_all(head.as_bytes());
    for chunk in content[..end].chunks(4096) {
        let _ = stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes());
        let _ = stream.write_all(chunk);
        let _ = stream.write_all(b"\r\n");
    }
    if end == content.len() {
        let _ = stream.write_all(b"0\r\n\r\n");
    }
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// Where each frame of a whole body ends, with its kind.
fn frame_ends(body: &[u8]) -> Vec<(u8, usize)> {
    let mut at = 0;
    tessera_wire::split_frames(body)
        .unwrap()
        .into_iter()
        .map(|(kind, payload)| {
            at += tessera_wire::FRAME_HEADER_BYTES + payload.len();
            (kind, at)
        })
        .collect()
}

/// Where the `n`th frame of `kind` in `body` ends.
fn end_of(body: &[u8], kind: u8, n: usize) -> usize {
    frame_ends(body)
        .into_iter()
        .filter(|(each, _)| *each == kind)
        .nth(n - 1)
        .unwrap()
        .1
}

impl Served {
    /// A read made over HTTP, following each response's cursor until it is null: every page,
    /// and the number of responses.
    fn http_read(&self, route: &str, body: &Value) -> (Vec<RecordBatch>, usize) {
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
            request = following(body, &next);
        }
    }

    /// `tessera <args> --server <the proxy> --token <the token>`.
    fn run(&self, args: &[&str]) -> Run {
        let mut command = tessera();
        command
            .args(args)
            .args(["--server", &self.proxy.url, "--token", &self.token])
            .current_dir(self.dir.path());
        run(command, self.dir.path())
    }
}

/// A later request of the read `body` begins: the same with the cursor, and without `count`.
fn following(body: &Value, cursor: &Value) -> Value {
    let mut request = body.clone();
    request.as_object_mut().unwrap().remove("count");
    request["cursor"] = cursor.clone();
    request
}

/// What a command did: its exit status, stdout and stderr.
struct Run {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
}

impl Run {
    /// The cursor the command printed to read the rest with.
    fn cursor(&self) -> Option<String> {
        let (_, after) = self.stderr.split_once("--cursor=")?;
        Some(after.split_whitespace().next()?.to_owned())
    }
}

/// Run `command` in `dir`, killing it and failing if it runs for a minute: a read that never ends
/// is a failure, not a hang.
fn run(mut command: Command, dir: &Path) -> Run {
    let (stdout, stderr) = (dir.join("stdout"), dir.join("stderr"));
    let mut child = command
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{command:?} ran for a minute");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Run {
        status,
        stdout: std::fs::read(stdout).unwrap(),
        stderr: std::fs::read_to_string(stderr).unwrap(),
    }
}

fn decode_stream(bytes: &[u8]) -> Vec<RecordBatch> {
    arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None)
        .expect("an Arrow IPC stream")
        .map(|batch| batch.unwrap())
        .collect()
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

/// The batches of an output file, `.parquet` or `.arrows`.
fn read_output(path: &Path) -> Vec<RecordBatch> {
    match path.extension().unwrap().to_str().unwrap() {
        "parquet" => read_parquet(path),
        _ => decode_stream(&std::fs::read(path).unwrap()),
    }
}

/// The schema an output file declares, `.parquet` or `.arrows`, whether or not it holds a row.
fn output_schema(path: &Path) -> arrow::datatypes::SchemaRef {
    match path.extension().unwrap().to_str().unwrap() {
        "parquet" => parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            std::fs::File::open(path).unwrap(),
        )
        .unwrap()
        .schema()
        .clone(),
        _ => arrow::ipc::reader::StreamReader::try_new(
            std::io::Cursor::new(std::fs::read(path).unwrap()),
            None,
        )
        .unwrap()
        .schema(),
    }
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

/// `tessera items` over every kind of field, 97 rows to a page and `pages` to a response.
fn items_args(pages: &'static str) -> Vec<&'static str> {
    vec![
        "items",
        "--view",
        "s0",
        "--fields",
        "fx_key,bay,tag,seen_at,weight,id",
        "--system-fields",
        "position,labels",
        "--order",
        "map",
        "--page-rows",
        "97",
        "--pages",
        pages,
    ]
}

/// The request `items_args` asks for.
fn items_body(pages: u32) -> Value {
    json!({
        "view": "s0",
        "fields": ["fx_key", "bay", "tag", "seen_at", "weight", "id"],
        "system_fields": ["position", "labels"],
        "order": "map",
        "page_rows": 97,
        "pages": pages,
    })
}

fn with_out<'a>(mut args: Vec<&'a str>, out: &'a Path) -> Vec<&'a str> {
    args.extend(["--out", out.to_str().unwrap()]);
    args
}

#[test]
fn an_items_read_written_as_ipc_and_as_parquet_is_the_read_over_http() {
    let served = serve();
    let (paged, responses) = served.http_read("items", &items_body(2));
    assert!(responses > 2, "the read spans several responses");
    assert!(rows(&paged) > 1000);
    // Every column holds values, so an equality below is not an equality of nulls.
    let values = plain(&paged);
    for name in ["fx_key", "bay", "tag", "seen_at", "weight", "id"] {
        let column = values.column_by_name(name).unwrap();
        assert!(column.null_count() < column.len(), "{name} holds values");
    }

    for name in ["items.arrows", "items.parquet"] {
        let out = served.dir.path().join(name);
        assert!(served.run(&with_out(items_args("2"), &out)).status.success());
        let written = read_output(&out);
        assert_eq!(written[0].schema().fields(), paged[0].schema().fields());
        assert_eq!(plain(&written), values, "{name}");
        if name.ends_with(".arrows") {
            assert_eq!(written.len(), paged.len(), "one batch for each page");
        }

        let bodies = served.proxy.bodies();
        assert_eq!(bodies.len(), responses);
        assert_eq!(bodies[0], items_body(2), "the arguments, and nothing else");
        for body in &bodies[1..] {
            assert!(body["cursor"].is_string());
            assert_eq!(body, &following(&items_body(2), &body["cursor"]));
        }
    }
}

#[test]
fn stdout_carries_the_read_in_the_format_named() {
    let served = serve();
    let (whole, _) = served.http_read("items", &items_body(2));
    let whole = plain(&whole);

    let mut args = items_args("2");
    args.extend(["--format", "ipc", "--compression", "zstd"]);
    let ipc = served.run(&args);
    assert!(ipc.status.success());
    assert_eq!(plain(&decode_stream(&ipc.stdout)), whole);

    let mut args = items_args("2");
    args.extend(["--format", "parquet", "--out", "-"]);
    let parquet = served.run(&args);
    assert!(parquet.status.success());
    let out = served.dir.path().join("stdout.parquet");
    std::fs::write(&out, &parquet.stdout).unwrap();
    assert_eq!(plain(&read_output(&out)), whole);
}

#[test]
fn a_read_goes_on_from_the_cursor_it_is_given() {
    let served = serve();
    let (whole, _) = served.http_read("items", &items_body(2));
    let mut first = items_body(1);
    first["page_rows"] = 500.into();
    let bytes = post(&format!("{}/v1/items", served.viewer), &served.token, &first);
    let frames = tessera_wire::split_frames(&bytes).unwrap();
    let trailer: Value = serde_json::from_slice(frames.last().unwrap().1).unwrap();
    let cursor = trailer["next"].as_str().unwrap();

    let out = served.dir.path().join("rest.arrows");
    let mut args = with_out(items_args("2"), &out);
    args.extend(["--cursor", cursor]);
    assert!(served.run(&args).status.success());
    let whole = plain(&whole);
    assert_eq!(plain(&read_output(&out)), whole.slice(500, whole.num_rows() - 500));
    assert_eq!(served.proxy.bodies()[0]["cursor"], cursor);

    // A cursor is base64url, which can begin with a hyphen.
    let mut args = items_args("2");
    args.extend(["--format", "ipc", "--cursor", "-not-a-cursor"]);
    assert!(!served.run(&args).status.success());
    assert_eq!(served.proxy.bodies()[0]["cursor"], "-not-a-cursor");
}

#[test]
fn count_goes_on_the_first_request_and_every_other_argument_on_each() {
    let served = serve();
    let filters = r#"{"bay": {"in": ["amber", "cedar"]}}"#;
    let body = json!({
        "view": "s0",
        "fields": ["bay", "weight"],
        "filters": serde_json::from_str::<Value>(filters).unwrap(),
        "keep_unmatched": true,
        "count": true,
        "page_rows": 300,
        "pages": 1,
    });
    let (whole, responses) = served.http_read("items", &body);
    assert!(responses > 2);
    let out = served.dir.path().join("matched.parquet");
    let args = [
        "items",
        "--view",
        "s0",
        "--fields",
        "bay,weight",
        "--filters",
        filters,
        "--keep-unmatched",
        "--count",
        "--page-rows",
        "300",
        "--pages",
        "1",
        "--out",
        out.to_str().unwrap(),
    ];
    let done = served.run(&args);
    assert!(done.status.success(), "{}", done.stderr);
    let whole = plain(&whole);
    assert_eq!(plain(&read_output(&out)), whole);
    let matched = whole.column_by_name("tessera:matched").unwrap();
    let matched = matched.as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
    assert!(matched.true_count() > 0 && matched.false_count() > 0);

    let bodies = served.proxy.bodies();
    assert_eq!(bodies.len(), responses);
    assert_eq!(bodies[0], body);
    for later in &bodies[1..] {
        assert_eq!(later, &following(&body, &later["cursor"]));
    }
}

#[test]
fn an_artifacts_read_passes_its_own_arguments_and_is_the_read_over_http() {
    let served = serve();
    let out = served.dir.path().join("artifacts.parquet");
    let artifacts = |extra: &[&str], body: Value| {
        let (whole, _) = served.http_read("artifacts", &body);
        let mut args = vec!["artifacts", "--view", "s0", "--page-rows", "4", "--pages", "1"];
        args.extend(extra);
        args.extend(["--out", out.to_str().unwrap()]);
        let done = served.run(&args);
        assert!(done.status.success(), "{}", done.stderr);
        let bodies = served.proxy.bodies();
        let mut asked = body.clone();
        asked["page_rows"] = 4.into();
        asked["pages"] = 1.into();
        assert_eq!(bodies[0], asked);
        for later in &bodies[1..] {
            assert_eq!(later, &following(&asked, &later["cursor"]));
        }
        let written = read_output(&out);
        assert_eq!(written[0].schema().fields(), whole[0].schema().fields());
        assert_eq!(plain(&written), plain(&whole));
        whole
    };

    let fields = "key,level,parents,masked_count,centroid";
    let listed = ["key", "level", "parents", "masked_count", "centroid"];
    let flat = artifacts(
        &["--layer", "generator/flat", "--fields", fields],
        json!({"view": "s0", "layer": "generator/flat", "fields": listed}),
    );
    assert!(rows(&flat) > 4);

    let keyed = artifacts(
        &["--layer", "generator/flat", "--fields", "key", "--q", "1"],
        json!({"view": "s0", "layer": "generator/flat", "fields": ["key"], "q": "1"}),
    );
    assert!(rows(&keyed) > 0 && rows(&keyed) < rows(&flat));

    let filters = r#"{"bay": {"eq": "amber"}}"#;
    let matched = artifacts(
        &[
            "--layer",
            "generator/flat",
            "--fields",
            "key",
            "--filters",
            filters,
            "--keep-unmatched",
            "--count",
        ],
        json!({
            "view": "s0",
            "layer": "generator/flat",
            "fields": ["key"],
            "filters": {"bay": {"eq": "amber"}},
            "keep_unmatched": true,
            "count": true,
        }),
    );
    assert_eq!(rows(&matched), rows(&flat));

    let tree = served.http_read(
        "artifacts",
        &json!({"view": "s0", "layer": "generator/treed", "fields": ["key", "parents"]}),
    );
    let ids = plain(&tree.0).column(0).clone();
    let root = ids
        .as_any()
        .downcast_ref::<arrow::array::UInt64Array>()
        .unwrap()
        .value(0)
        .to_string();
    let children = artifacts(
        &["--layer", "generator/treed", "--fields", "key", "--parent", &root],
        json!({"view": "s0", "layer": "generator/treed", "fields": ["key"], "parent": root}),
    );
    assert!(rows(&children) > 0);

    // By identifier: the rows of the ids named, and none for an id naming nothing.
    let ids_of = |batches: &[RecordBatch]| -> Vec<u64> {
        let column = plain(batches).column(0).clone();
        let ids = column.as_any().downcast_ref::<arrow::array::UInt64Array>().unwrap();
        let mut ids: Vec<u64> = ids.values().to_vec();
        ids.sort_unstable();
        ids
    };
    let all = ids_of(&flat);
    let wanted = [all[0].to_string(), all[2].to_string(), "1".to_string()];
    let named = artifacts(
        &["--layer", "generator/flat", "--fields", "key", "--ids", &wanted.join(",")],
        json!({"view": "s0", "layer": "generator/flat", "fields": ["key"], "ids": wanted}),
    );
    assert_eq!(ids_of(&named), [all[0], all[2]]);

    // A level on a layer with one level is the server's refusal, sent as asked.
    let refused = served.run(&[
        "artifacts",
        "--view",
        "s0",
        "--layer",
        "generator/treed",
        "--fields",
        "key",
        "--level",
        "0",
        "--format",
        "ipc",
    ]);
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
    assert_eq!(
        served.proxy.bodies(),
        [json!({"view": "s0", "layer": "generator/treed", "fields": ["key"], "level": 0})]
    );
}

/// **A read that finds no row writes the columns it asked for**, typed as a read with rows types
/// them, with no rows, in both formats and to stdout.
#[test]
fn a_read_that_finds_no_row_writes_its_columns_with_no_rows() {
    let served = serve();
    let (paged, _) = served.http_read("items", &items_body(2));
    let schema = paged[0].schema();
    let filters = r#"{"tag": {"eq": "no such tag"}}"#;
    let mut nothing = items_body(2);
    nothing["filters"] = serde_json::from_str(filters).unwrap();
    let (empty, _) = served.http_read("items", &nothing);
    assert_eq!(rows(&empty), 0);
    assert_eq!(empty[0].schema(), schema, "the server's page of no rows has the read's columns");

    for (format, out) in [("ipc", "none.arrows"), ("parquet", "none.parquet")] {
        let out = served.dir.path().join(out);
        let mut args = with_out(items_args("2"), &out);
        args.extend(["--filters", filters]);
        let done = served.run(&args);
        assert!(done.status.success(), "{}", done.stderr);
        assert_eq!(rows(&read_output(&out)), 0, "{format}");
        assert_eq!(output_schema(&out).fields(), schema.fields(), "{format}");

        let mut args = items_args("2");
        args.extend(["--filters", filters, "--format", format]);
        let piped = served.run(&args);
        assert!(piped.status.success());
        let copy = served.dir.path().join("piped").with_extension(out.extension().unwrap());
        std::fs::write(&copy, &piped.stdout).unwrap();
        assert_eq!(rows(&read_output(&copy)), 0);
        assert_eq!(output_schema(&copy).fields(), schema.fields(), "{format} on stdout");
    }
}

/// Cut 10 bytes into the frame after the second page end, exactly at the second page end, or
/// after the third page but before its page end.
const CUTS: [fn(&[u8]) -> usize; 3] = [
    |body| end_of(body, tessera_wire::FRAME_PAGE_END, 2) + 10,
    |body| end_of(body, tessera_wire::FRAME_PAGE_END, 2),
    |body| end_of(body, tessera_wire::FRAME_RECORDS, 3),
];

#[test]
fn a_read_cut_short_keeps_its_whole_pages_and_prints_the_cursor_to_read_on_from() {
    let served = serve();
    let (paged, _) = served.http_read("items", &items_body(3));
    let whole = plain(&paged);
    // The first response's three pages, and two of the second's.
    let kept_rows = rows(&paged[..5]);
    for (n, cut) in CUTS.into_iter().enumerate() {
        for format in ["arrows", "parquet"] {
            let out = served.dir.path().join(format!("cut{n}.{format}"));
            served.proxy.plan([None, Some(Step::Cut(cut))]);
            let stopped = served.run(&with_out(items_args("3"), &out));
            assert!(!stopped.status.success(), "cut {n} {format}");
            let kept = plain(&read_output(&out));
            assert_eq!(kept, whole.slice(0, kept_rows), "cut {n} {format}");

            let cursor = stopped.cursor().expect("the cursor to read on from");
            let rest = served.dir.path().join(format!("rest{n}.{format}"));
            let mut args = with_out(items_args("3"), &rest);
            args.extend(["--cursor", &cursor]);
            assert!(served.run(&args).status.success());
            let rest = plain(&read_output(&rest));
            assert_eq!(rest, whole.slice(kept.num_rows(), whole.num_rows() - kept.num_rows()));
            served.proxy.bodies();
        }
    }
}

#[test]
fn a_read_cut_after_its_last_page_writes_every_row() {
    let served = serve();
    let body = json!({"view": "s0", "fields": ["fx_key", "bay"]});
    let (whole, responses) = served.http_read("items", &body);
    assert_eq!(responses, 1);
    for format in ["arrows", "parquet"] {
        let out = served.dir.path().join(format!("all.{format}"));
        let last_page_end = |body: &[u8]| end_of(body, tessera_wire::FRAME_PAGE_END, 1);
        served.proxy.plan([Some(Step::Cut(last_page_end))]);
        let path = out.to_str().unwrap();
        let args = ["items", "--view", "s0", "--fields", "fx_key,bay", "--out", path];
        let stopped = served.run(&args);
        assert!(!stopped.status.success());
        assert_eq!(stopped.cursor(), None, "no row remains");
        assert_eq!(plain(&read_output(&out)), plain(&whole), "{format}");
    }
}

#[test]
fn a_refused_request_part_of_the_way_keeps_the_pages_before_it() {
    let served = serve();
    let (paged, _) = served.http_read("items", &items_body(3));
    let whole = plain(&paged);
    for format in ["arrows", "parquet"] {
        let out = served.dir.path().join(format!("shed.{format}"));
        served.proxy.plan([None, Some(Step::Refuse(429))]);
        let stopped = served.run(&with_out(items_args("3"), &out));
        assert!(!stopped.status.success());
        let first_response = rows(&paged[..3]);
        assert_eq!(plain(&read_output(&out)), whole.slice(0, first_response));

        let cursor = stopped.cursor().expect("the cursor to read on from");
        let rest = served.dir.path().join(format!("after.{format}"));
        let mut args = with_out(items_args("3"), &rest);
        args.extend(["--cursor", &cursor]);
        assert!(served.run(&args).status.success());
        let rest = plain(&read_output(&rest));
        assert_eq!(rest, whole.slice(first_response, whole.num_rows() - first_response));
        served.proxy.bodies();
    }
}

#[test]
fn a_read_without_a_token_sends_nothing_and_writes_nothing() {
    let served = serve();
    let out = served.dir.path().join("refused.parquet");
    let read = |token: Option<&str>| {
        let mut command = tessera();
        command
            .args(with_out(items_args("2"), &out))
            .args(["--server", &served.proxy.url])
            .env_remove("TESSERA_TOKEN")
            .current_dir(served.dir.path());
        if let Some(token) = token {
            command.env("TESSERA_TOKEN", token);
        }
        run(command, served.dir.path()).status
    };
    assert!(!read(None).success());
    assert!(served.proxy.bodies().is_empty());
    assert!(!out.exists());

    assert!(!read(Some("not-a-token")).success());
    assert!(!out.exists());

    assert!(read(Some(&served.token)).success());
    assert!(out.exists());
}

/// Each output that names no format, or two, is refused before a request is sent, where the same
/// read with its format named succeeds.
#[test]
fn the_format_is_named_or_refused_before_any_request() {
    let served = serve();
    let dir = served.dir.path();
    let written = |name: &str| dir.join(name).exists();
    for (extra, refused) in [
        (vec![], true),
        (vec!["--out", "-"], true),
        (vec!["--out", "items.txt"], true),
        (vec!["--out", "items"], true),
        (vec!["--out", "items.arrow"], true),
        (vec!["--out", "clash.parquet", "--format", "ipc"], true),
        (vec!["--out", "clash.arrows", "--format", "parquet"], true),
        (vec!["--format", "ipc"], false),
        (vec!["--out", "items.dat", "--format", "parquet"], false),
        (vec!["--out", "named.arrows", "--format", "ipc"], false),
        (vec!["--out", "named.PARQUET"], false),
    ] {
        let mut args = items_args("2");
        args.extend(&extra);
        let done = served.run(&args);
        let bodies = served.proxy.bodies();
        if refused {
            assert!(!done.status.success(), "{extra:?}");
            assert!(bodies.is_empty(), "{extra:?} sent a request");
            assert!(done.stdout.is_empty(), "{extra:?}");
            for name in ["items.txt", "items", "items.arrow", "clash.parquet", "clash.arrows"] {
                assert!(!written(name), "{extra:?} wrote {name}");
            }
        } else {
            assert!(done.status.success(), "{extra:?}: {}", done.stderr);
            assert!(!bodies.is_empty());
        }
    }
    assert!(rows(&read_parquet(&dir.join("items.dat"))) > 0);
    assert!(rows(&read_parquet(&dir.join("named.PARQUET"))) > 0);
}

/// `tessera aggregate` writes the one table its grouping asks for, as the same read over HTTP
/// gives it: a histogram, which is one page however small the pages asked for, and a breakdown,
/// carried across responses from its cursor; and arguments that are not JSON send nothing.
#[test]
fn an_aggregate_read_is_the_table_over_http() {
    let served = serve();
    let reads = [
        (
            r#"{"by": {"field": "seen_at", "bins": 12}}"#,
            json!({ "by": { "field": "seen_at", "bins": 12 } }),
            "histogram.arrows",
            false,
        ),
        (
            r#"{"by": {"field": "bay", "top": 3}}"#,
            json!({ "by": { "field": "bay", "top": 3 } }),
            "breakdown.parquet",
            true,
        ),
    ];
    for (grouping, parsed, name, pages) in reads {
        let body = json!({
            "view": "s0",
            "groupings": [parsed],
            "filters": { "bay": { "in": ["amber", "basalt", "cedar", "dune"] } },
            "reference": {},
            "page_rows": 2,
            "pages": 1,
        });
        let (whole, responses) = served.http_read("aggregate", &body);
        assert_eq!(responses > 1, pages, "{name}: {responses} responses, {} rows", rows(&whole));
        let out = served.dir.path().join(name);
        let done = served.run(&[
            "aggregate",
            "--view",
            "s0",
            "--grouping",
            grouping,
            "--filters",
            r#"{"bay": {"in": ["amber", "basalt", "cedar", "dune"]}}"#,
            "--reference",
            "{}",
            "--page-rows",
            "2",
            "--pages",
            "1",
            "--out",
            out.to_str().unwrap(),
        ]);
        assert!(done.status.success(), "{}", done.stderr);
        assert!(done.stderr.contains("\"total\""), "the head is printed: {}", done.stderr);
        assert_eq!(plain(&read_output(&out)), plain(&whole), "{name}");
        let bodies = served.proxy.bodies();
        assert_eq!(bodies.len(), responses);
        assert_eq!(bodies[0], body, "the arguments, and nothing else");
        for later in &bodies[1..] {
            assert_eq!(later, &following(&body, &later["cursor"]));
        }
    }

    let out = served.dir.path().join("refused.arrows");
    for (flag, text) in [("--grouping", "{by"), ("--filters", "nope")] {
        let mut args = vec!["aggregate", "--view", "s0", "--grouping", "{}"];
        if flag == "--grouping" {
            args.truncate(3);
        }
        args.extend([flag, text, "--out", out.to_str().unwrap()]);
        let refused = served.run(&args);
        assert_eq!(refused.status.code(), Some(1), "{flag}: {}", refused.stderr);
    }
    assert!(served.proxy.bodies().is_empty());
    assert!(!out.exists());
}
