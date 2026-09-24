//! `tessera items` and `tessera artifacts`: a whole read of `POST /v1/items` or
//! `POST /v1/artifacts` from a running server, written as Arrow IPC or Parquet.
//!
//! Each response's pages are written as they arrive, and the next response is requested with the
//! cursor the last one ended on, until the cursor is null. A page is written once the page end
//! after it has arrived, so a read cut short leaves whole pages in the output and names the cursor
//! to read on from.

use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use clap::ValueEnum;
use parquet::arrow::ArrowWriter;
use serde_json::{Map, Value};
use tessera_wire::{read_frame, FRAME_PAGE_END, FRAME_RECORDS, FRAME_RECORDS_HEAD, FRAME_TRAILER};

#[derive(clap::Args)]
pub(crate) struct ItemsArgs {
    /// The view to read, as `/v1/meta` names it. An item with no position in it is not returned.
    #[arg(long)]
    view: String,
    /// The declared fields to return, comma-separated, in the order wanted. `--fields ''`
    /// returns `tessera_id` alone. A field declared for a view group, read under a view outside
    /// that group, is named `<field>@<key>`.
    #[arg(long, value_name = "NAMES")]
    fields: String,
    /// Any of `position`, `external_id` and `labels`, comma-separated, in the order wanted: the
    /// columns `tessera:x` and `tessera:y`, `tessera:external_id` and `tessera:labels`.
    #[arg(long, value_name = "NAMES")]
    system_fields: Option<String>,
    /// A filter expression as JSON, such as `{"year": {"range": {"gte": 2020}}}`. Only the items
    /// that match are returned.
    #[arg(long, value_name = "JSON")]
    filters: Option<String>,
    /// Return every item, with a `tessera:matched` column saying whether it matches `--filters`.
    #[arg(long)]
    keep_unmatched: bool,
    /// Count the items the token may see in the view and those that match. The counts are
    /// printed on stderr at the end.
    #[arg(long)]
    count: bool,
    /// `map` returns the items by their place on the map; `stored` in the order the server stores
    /// records, which is faster for a field that is neither rendered nor indexed. Without it the
    /// server chooses.
    #[arg(long, value_enum)]
    order: Option<Order>,
    #[command(flatten)]
    paging: Paging,
    #[command(flatten)]
    target: Target,
}

#[derive(clap::Args)]
pub(crate) struct ArtifactsArgs {
    /// The view whose items the counts and the geometry are computed over.
    #[arg(long)]
    view: String,
    /// The layer to read, as `/v1/meta` lists it.
    #[arg(long)]
    layer: String,
    /// Any of `key`, `level`, `parents`, `target`, `masked_count`, `content`, `centroid`, `box`
    /// and `shape`, comma-separated, in the order wanted.
    #[arg(long, value_name = "NAMES")]
    fields: String,
    /// Only the artifacts at this level of a levelled layer.
    #[arg(long)]
    level: Option<u32>,
    /// Only the children of this artifact, by its `tessera_id`.
    #[arg(long, value_name = "TESSERA_ID")]
    parent: Option<u64>,
    /// Only the artifacts whose key, or first text, contains this, ignoring case.
    #[arg(long, value_name = "TEXT")]
    q: Option<String>,
    /// A filter expression as JSON. Only the artifacts with a visible member that matches are
    /// returned, each with a `matched_count` column.
    #[arg(long, value_name = "JSON")]
    filters: Option<String>,
    /// With `--filters`, return every artifact, those with no matching member included.
    #[arg(long)]
    keep_unmatched: bool,
    /// Count the artifacts served and those that match. The counts are printed on stderr at the
    /// end.
    #[arg(long)]
    count: bool,
    #[command(flatten)]
    paging: Paging,
    #[command(flatten)]
    target: Target,
}

#[derive(clap::Args)]
struct Paging {
    /// Rows in a page, at most the server's `selection.max_page_rows`, which applies without it.
    #[arg(long, value_name = "N")]
    page_rows: Option<u32>,
    /// The most pages in one response. Responses are requested until the read is done.
    #[arg(long, value_name = "N")]
    pages: Option<u32>,
    /// Start after the last page of an earlier read: the cursor a read cut short printed.
    #[arg(long, allow_hyphen_values = true)]
    cursor: Option<String>,
    /// `zstd` compresses the pages on their way from the server. The output is written
    /// uncompressed either way.
    #[arg(long, value_enum)]
    compression: Option<Compression>,
    /// The id numbering the ids are read in, `/v1/meta`'s `idset`. A server holding another
    /// refuses the read.
    #[arg(long)]
    idset: Option<u32>,
}

#[derive(clap::Args)]
struct Target {
    /// The viewer plane's address, such as `http://127.0.0.1:8080`.
    #[arg(long, value_name = "URL")]
    server: String,
    /// A session token, as the session plane's `/session/authorise` issues one. Without it the
    /// token is read from `TESSERA_TOKEN`.
    #[arg(long)]
    token: Option<String>,
    /// The file to write. Without it, or with `-`, the output goes to stdout.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
    /// `ipc` writes an Arrow IPC stream and `parquet` a Parquet file. Without it the format is
    /// the extension of `--out`, `.arrows` or `.parquet`; stdout and any other extension need it.
    #[arg(long, value_enum)]
    format: Option<Format>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Order {
    Map,
    Stored,
}

#[derive(Clone, Copy, ValueEnum)]
enum Compression {
    Zstd,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Ipc,
    Parquet,
}

/// The name a value enum is written as on the command line, which is also the route's.
fn wire_name(value: impl ValueEnum) -> Value {
    value
        .to_possible_value()
        .expect("no variant is skipped")
        .get_name()
        .into()
}

pub(crate) fn items(args: ItemsArgs) -> ExitCode {
    let request = request(&args.fields, args.filters.as_deref(), &args.paging).map(|mut request| {
        request.insert("view".into(), args.view.into());
        if let Some(system) = &args.system_fields {
            request.insert("system_fields".into(), names(system));
        }
        if args.keep_unmatched {
            request.insert("keep_unmatched".into(), true.into());
        }
        if args.count {
            request.insert("count".into(), true.into());
        }
        if let Some(order) = args.order {
            request.insert("order".into(), wire_name(order));
        }
        request
    });
    run("items", request, &args.target)
}

pub(crate) fn artifacts(args: ArtifactsArgs) -> ExitCode {
    let request = request(&args.fields, args.filters.as_deref(), &args.paging).map(|mut request| {
        request.insert("view".into(), args.view.into());
        request.insert("layer".into(), args.layer.into());
        if let Some(level) = args.level {
            request.insert("level".into(), level.into());
        }
        if let Some(parent) = args.parent {
            request.insert("parent".into(), parent.to_string().into());
        }
        if let Some(q) = args.q {
            request.insert("q".into(), q.into());
        }
        if args.keep_unmatched {
            request.insert("keep_unmatched".into(), true.into());
        }
        if args.count {
            request.insert("count".into(), true.into());
        }
        request
    });
    run("artifacts", request, &args.target)
}

/// The fields both routes share, each sent only when given.
fn request(
    fields: &str,
    filters: Option<&str>,
    paging: &Paging,
) -> Result<Map<String, Value>, String> {
    let mut request = Map::new();
    request.insert("fields".into(), names(fields));
    if let Some(filters) = filters {
        let parsed: Value = serde_json::from_str(filters).map_err(|e| {
            format!(
                "--filters is not JSON ({e}); write one filter expression, such as \
                 '{{\"year\": {{\"eq\": 2020}}}}'"
            )
        })?;
        request.insert("filters".into(), parsed);
    }
    if let Some(rows) = paging.page_rows {
        request.insert("page_rows".into(), rows.into());
    }
    if let Some(pages) = paging.pages {
        request.insert("pages".into(), pages.into());
    }
    if let Some(cursor) = &paging.cursor {
        request.insert("cursor".into(), cursor.clone().into());
    }
    if let Some(compression) = paging.compression {
        request.insert("compression".into(), wire_name(compression));
    }
    if let Some(idset) = paging.idset {
        request.insert("idset".into(), idset.into());
    }
    Ok(request)
}

/// A comma-separated list as a JSON array; the empty string is the empty list.
fn names(list: &str) -> Value {
    if list.is_empty() {
        return Value::Array(Vec::new());
    }
    list.split(',').map(|one| Value::from(one.trim())).collect()
}

fn run(route: &str, request: Result<Map<String, Value>, String>, target: &Target) -> ExitCode {
    match request.and_then(|request| read(route, request, target)) {
        Ok(summary) => {
            eprintln!(
                "tessera {route}: {} rows in {} pages from {} responses; head {}",
                summary.rows,
                summary.pages,
                summary.responses,
                summary.head.unwrap_or(Value::Null)
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tessera {route}: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Default)]
struct Summary {
    rows: usize,
    pages: usize,
    /// The responses that answered, each with a head.
    responses: usize,
    /// The first response's head.
    head: Option<Value>,
    /// The cursor to read on from after the last page written; `None` is the start.
    resume: Option<String>,
    /// Whether a page end or a trailer has said that no row remains.
    done: bool,
}

impl Summary {
    /// Take the cursor of a page end or a trailer.
    fn reached(&mut self, frame: &Value) {
        self.resume = frame.get("next").and_then(Value::as_str).map(str::to_owned);
        self.done = self.resume.is_none();
    }
}

fn read(
    route: &str,
    mut request: Map<String, Value>,
    target: &Target,
) -> Result<Summary, String> {
    let token = match &target.token {
        Some(token) => token.clone(),
        None => std::env::var("TESSERA_TOKEN")
            .map_err(|_| "no session token: pass --token, or set TESSERA_TOKEN".to_string())?,
    };
    let mut output = Output::new(target)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("starting the HTTP client: {e}"))?;
    let url = format!("{}/v1/{route}", target.server.trim_end_matches('/'));
    let mut summary = Summary {
        resume: request.get("cursor").and_then(Value::as_str).map(str::to_owned),
        ..Summary::default()
    };
    let read = loop {
        let response = match post(&client, &url, &token, &request) {
            Ok(response) => response,
            Err(e) => break Err(e),
        };
        summary.responses += 1;
        if let Err(e) = read_response(response, &mut output, &mut summary) {
            break Err(e);
        }
        let (Some(next), false) = (summary.resume.clone(), summary.done) else {
            break Ok(());
        };
        // The server takes `count` on a read's first request only.
        request.remove("count");
        request.insert("cursor".into(), next.into());
    };
    let Err(e) = read else {
        return output.finish().map(|()| summary);
    };
    if summary.responses == 0 {
        return Err(e);
    }
    let written = if summary.pages == 0 {
        "Nothing was written".to_string()
    } else {
        let (rows, pages) = (summary.rows, summary.pages);
        match output.finish() {
            Ok(()) => format!("{rows} rows in {pages} whole pages were written"),
            Err(also) => format!("{rows} rows were read but not all written ({also})"),
        }
    };
    let rest = match (&summary.resume, summary.done) {
        (_, true) => "every row had arrived".to_string(),
        (Some(cursor), false) => format!("pass --cursor={cursor} to read the rest"),
        (None, false) => "run the read again for the rest".to_string(),
    };
    Err(format!("{e}. {written}; {rest}"))
}

/// One request, answered with its body open to read, or the refusal the server gave.
fn post(
    client: &reqwest::blocking::Client,
    url: &str,
    token: &str,
    request: &Map<String, Value>,
) -> Result<reqwest::blocking::Response, String> {
    let response = client
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(Value::Object(request.clone()).to_string())
        .send()
        .map_err(|e| format!("no answer from {url}: {}", crate::innermost(&e)))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let detail = response.text().unwrap_or_default();
    Err(format!("{url} refused the read ({status}): {detail}"))
}

/// Write one response's pages, each once its page end has arrived, taking the cursor of each
/// page end and of the trailer.
fn read_response(
    mut body: impl Read,
    output: &mut Output,
    summary: &mut Summary,
) -> Result<(), String> {
    let mut pending = None;
    let mut first = true;
    loop {
        let (kind, payload) = match read_frame(&mut body) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Err("the response ended before its trailer".into()),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return Err(format!("the response is not a bulk read's body: {e}"))
            }
            Err(e) => {
                return Err(format!("the response was cut short ({})", crate::innermost(&e)))
            }
        };
        if first != (kind == FRAME_RECORDS_HEAD) {
            return Err("the response does not have exactly one head, first".into());
        }
        first = false;
        match kind {
            FRAME_RECORDS_HEAD => {
                if summary.head.is_none() {
                    summary.head = Some(json(&payload)?);
                }
            }
            FRAME_RECORDS => pending = Some(decode(&payload)?),
            FRAME_PAGE_END => {
                let batch: RecordBatch = pending
                    .take()
                    .ok_or("the response has a page end with no page before it")?;
                output.write(&batch)?;
                summary.rows += batch.num_rows();
                summary.pages += 1;
                summary.reached(&json(&payload)?);
            }
            FRAME_TRAILER => {
                summary.reached(&json(&payload)?);
                return Ok(());
            }
            other => return Err(format!("the response holds a frame of kind {other}")),
        }
    }
}

fn json(payload: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(payload)
        .map_err(|e| format!("the response holds a frame that is not JSON: {e}"))
}

/// A records frame's one batch.
fn decode(payload: &[u8]) -> Result<RecordBatch, String> {
    let undecodable = |e: arrow::error::ArrowError| format!("a page could not be decoded: {e}");
    arrow::ipc::reader::StreamReader::try_new(payload, None)
        .map_err(undecodable)?
        .next()
        .ok_or("a records frame holds no batch")?
        .map_err(undecodable)
}

/// Where the pages go, opened when the first page arrives so that a refused read writes nothing.
struct Output {
    format: Format,
    path: Option<PathBuf>,
    writer: Option<Writer>,
}

enum Writer {
    Ipc(StreamWriter<Box<dyn Write + Send>>),
    Parquet(ArrowWriter<Box<dyn Write + Send>>),
}

impl Output {
    fn new(target: &Target) -> Result<Output, String> {
        let path = target.out.clone().filter(|path| path != Path::new("-"));
        let format = output_format(path.as_deref(), target.format)?;
        Ok(Output {
            format,
            path,
            writer: None,
        })
    }

    fn write(&mut self, batch: &RecordBatch) -> Result<(), String> {
        if self.writer.is_none() {
            self.writer = Some(self.open(&batch.schema())?);
        }
        let written = match self.writer.as_mut().expect("opened above") {
            Writer::Ipc(writer) => writer.write(batch).map_err(|e| e.to_string()),
            Writer::Parquet(writer) => writer.write(batch).map_err(|e| e.to_string()),
        };
        written.map_err(|e| format!("writing {}: {e}", self.name()))
    }

    /// Close the output. A read that returned no row writes `tessera_id` alone, since no page
    /// carried the other columns.
    fn finish(mut self) -> Result<(), String> {
        let writer = match self.writer.take() {
            Some(writer) => writer,
            None => self.open(&Arc::new(Schema::new(vec![Field::new(
                "tessera_id",
                DataType::UInt64,
                false,
            )])))?,
        };
        let inner = match writer {
            Writer::Ipc(writer) => writer.into_inner().map_err(|e| e.to_string()),
            Writer::Parquet(writer) => writer.into_inner().map_err(|e| e.to_string()),
        };
        inner
            .and_then(|mut inner| inner.flush().map_err(|e| e.to_string()))
            .map_err(|e| format!("finishing {}: {e}", self.name()))
    }

    fn open(&self, schema: &Arc<Schema>) -> Result<Writer, String> {
        let sink: Box<dyn Write + Send> = match &self.path {
            None => Box::new(BufWriter::new(std::io::stdout())),
            Some(path) => Box::new(BufWriter::new(
                std::fs::File::create(path)
                    .map_err(|e| format!("creating {}: {e}", path.display()))?,
            )),
        };
        match self.format {
            Format::Ipc => StreamWriter::try_new(sink, schema)
                .map(Writer::Ipc)
                .map_err(|e| e.to_string()),
            Format::Parquet => ArrowWriter::try_new(sink, Arc::clone(schema), None)
                .map(Writer::Parquet)
                .map_err(|e| e.to_string()),
        }
        .map_err(|e| format!("starting {}: {e}", self.name()))
    }

    fn name(&self) -> String {
        match &self.path {
            Some(path) => path.display().to_string(),
            None => "stdout".into(),
        }
    }
}

/// The output format: `--format`, or else the extension of `--out`. A flag that disagrees with
/// the extension, and an output that names neither, is refused.
fn output_format(path: Option<&Path>, flag: Option<Format>) -> Result<Format, String> {
    let extension = path.and_then(Path::extension).and_then(|e| e.to_str());
    let named = extension.and_then(|extension| {
        if extension.eq_ignore_ascii_case("parquet") {
            Some(Format::Parquet)
        } else if extension.eq_ignore_ascii_case("arrows") {
            Some(Format::Ipc)
        } else {
            None
        }
    });
    match (flag, named, path) {
        (Some(flag), Some(named), Some(path)) if flag != named => Err(format!(
            "--format {} and the extension of {} name different formats; make them agree",
            wire_name(flag),
            path.display()
        )),
        (Some(flag), _, _) => Ok(flag),
        (None, Some(named), _) => Ok(named),
        (None, None, None) => {
            Err("writing to stdout needs --format ipc or --format parquet".into())
        }
        (None, None, Some(path)) => Err(format!(
            "{} does not end in .arrows or .parquet; name one of those, or pass --format ipc or \
             --format parquet",
            path.display()
        )),
    }
}
