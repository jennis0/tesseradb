//! `tessera`: the one binary that builds, checks, verifies and serves a bundle.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod identity;
mod records;

use clap::{Parser, Subcommand};
use tessera_spatial::Bounds;
use tessera_types::IdentityKey;

#[cfg(test)]
mod reference;

#[derive(Parser)]
#[command(
    name = "tessera",
    about = "Build a Tessera bundle from a corpus declaration, check the declaration, verify the \
             bundle and serve it.",
    // The commit rather than the crate version: a measurement is read against a tree, and the
    // crate version does not move between two of them.
    version = tessera_build::BUILD_COMMIT
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a bundle from the corpus declaration and the source files it names.
    ///
    /// `tessera build` needs no flags. It reads `tessera.toml` from the
    /// working directory, or from the nearest directory above it that has one. That file names
    /// the corpus declaration in `[build] schema` (default `schema.toml`) and the bundle
    /// directory in `[bundle] path`, each relative to the file's own directory. The declaration
    /// names the source files.
    ///
    /// Every build creates a new bundle with a new key for its `tessera_id`s, so a `tessera_id`
    /// read from an earlier bundle does not name an item in this one. A copy of a bundle keeps
    /// its `tessera_id`s.
    ///
    /// The other flags override `tessera.toml` or tune the build.
    Build {
        /// Read this `tessera.toml` instead of searching for one upward from the working
        /// directory.
        #[arg(long, value_name = "PATH")]
        deployment: Option<PathBuf>,
        /// Write the bundle to this directory instead of `[bundle] path` in `tessera.toml`.
        ///
        /// `tessera serve` opens `[bundle] path`, so it does not serve a bundle written
        /// elsewhere.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Build only the items whose value of the declaration's one unique integer attribute is
        /// below this value, and the rows of every file that name them.
        ///
        /// A view's points row whose value is at or above the limit, or null, creates no item. A
        /// negative value counts as its unsigned 64-bit value, at least 2^63, so the limit drops
        /// it. Refused when the declaration has no unique integer attribute, or more than one. A
        /// row that names a kept item by any unique field is read whatever its own value. A row
        /// that names none is left out: in a view's points it creates no item where its value is
        /// at or above the limit or null, and in any other file it is reported as outside the
        /// limit, since it may name an item the limit left out. The exception is a file that
        /// names items by the limit's attribute alone, whose row naming no item is refused where
        /// its value is below the limit.
        #[arg(long, value_name = "VALUE")]
        limit: Option<u64>,
        /// Refuse the build at the first file with a row the identity rule refuses.
        ///
        /// Without it, a row naming two items, naming an item or a unique value an earlier row of
        /// its file names, or, outside a view's points, naming no item, is left out and the build
        /// goes on. The refused rows are printed, and written to `reports/refused.json` in the
        /// bundle where there are any.
        #[arg(long)]
        strict: bool,
        /// Read this corpus declaration instead of `[build] schema` in `tessera.toml`.
        ///
        /// The declaration is compiled into the bundle's `MANIFEST.json`, and the server reads
        /// it from there.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Read the source NAME in the declaration's `[sources]` from PATH instead. Repeatable.
        ///
        /// A relative PATH is read from the working directory, not from the declaration's
        /// directory. Every block that reads the source reads PATH. Refused when `[sources]` has no source
        /// called NAME (the message lists the names it has), and when NAME is given twice. The
        /// flag replaces a source's path and cannot add a source.
        #[arg(long = "file", value_name = "NAME=PATH", value_parser = parse_file_binding)]
        file: Vec<(String, PathBuf)>,
        /// Do not write `pairs.parquet`.
        ///
        /// The server does not read the file. The conformance suite and `tessera verify --deep`
        /// do, and `verify --deep` passes a bundle without one.
        #[arg(long)]
        no_oracle_pairs: bool,
        /// Assign internal ids in batches of this many items.
        ///
        /// Default: the largest batch the memory budget allows, which is the whole corpus when
        /// it fits. Refused when the batch does not fit the budget, or when it is less than half
        /// the size the budget allows. A build of more than one batch records the size in the
        /// bundle.
        #[arg(long, value_name = "ITEMS")]
        batch_items: Option<u64>,
        /// Peak memory for the build's own structures, in bytes or with a `k`, `m` or `g`
        /// suffix, such as `24g`.
        ///
        /// Default: 80% of the available memory or of the process's cgroup limit, whichever is
        /// lower, kept between 2 GiB and 1 TiB, and 24 GiB where available memory cannot be
        /// read. Batch and band sizes follow from it. A build that would not fit is refused
        /// before it assigns internal ids, with the arithmetic in the message.
        #[arg(long, value_name = "SIZE", value_parser = parse_byte_size)]
        memory_budget: Option<u64>,

        /// Print each build stage's wall time, row count and peak resident memory to stderr as
        /// the stage ends.
        ///
        /// Peak resident memory is the process's high-water mark when the stage ended, so it
        /// shows the stage in which the peak was reached.
        #[arg(long)]
        stage_timings: bool,

        /// Write the per-stage records to this file as JSON when the build ends, including one
        /// that fails partway.
        ///
        /// One object per stage, in order, with `stage`, `wall_s`, `rows`, `peak_rss_kib`,
        /// `started_at` and `ended_at`. It does not need `--stage-timings`.
        #[arg(long, value_name = "PATH")]
        stage_timings_json: Option<PathBuf>,
    },
    /// Check the declaration against the column schemas of the files it names, without reading
    /// a row.
    ///
    /// `tessera check` finds `tessera.toml`, the declaration and the `--file` overrides as
    /// `tessera build` does, and stops at the first error in `tessera.toml` or the declaration,
    /// such as a missing key or a TOML syntax error. Once both parse, it reads the footer of
    /// each source Parquet file and reports every column that is missing or has the wrong type,
    /// not only the first. It reads no rows except the geometry of shape layers, which it reads
    /// to size them.
    ///
    /// The report goes to stderr: the files read, the findings, warnings, the columns each file
    /// names items by, the frames the views will have, the view groups, the shape layers' sizes,
    /// and on a clean check the disclosure table. The exit status is non-zero when there is a
    /// finding, such as a file other than a view's points with no column to name items by; a
    /// warning does not change it.
    ///
    /// It cannot check anything that needs a row: whether a closed vocabulary covers the values
    /// in the data, which rows the identity rule refuses, or where the data lies in its view's
    /// extent. `tessera build` reports those.
    Check {
        /// Read this `tessera.toml` instead of searching for one upward from the working
        /// directory.
        #[arg(long, value_name = "PATH")]
        deployment: Option<PathBuf>,
        /// Read this corpus declaration instead of `[build] schema` in `tessera.toml`.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Read the source NAME in the declaration's `[sources]` from PATH instead. Repeatable,
        /// and the same override `tessera build` takes.
        #[arg(long = "file", value_name = "NAME=PATH", value_parser = parse_file_binding)]
        file: Vec<(String, PathBuf)>,
        /// Print the declaration to stdout as the JSON request bodies the control plane takes,
        /// to declare the same corpus on a running service.
        ///
        /// Printed only when the check has no finding. The output is one object with the keys
        /// `layers`, `attributes`, `vocabularies`, `views` and `view_groups`, each an array in
        /// declaration order. A layer or attribute entry is the request body itself. A
        /// vocabulary, view or view group entry is `{ "name", "body" }`, because its name goes
        /// in the route's path. A vocabulary entry also has `values`, the body for `PATCH
        /// /control/vocabularies/{name}/values`, or `values_source`, the `[sources]` name its
        /// values are read from. Values read from a source are not printed.
        #[arg(long)]
        payloads: bool,
    },
    /// Verify a bundle's files and every row's `tessera_id`.
    ///
    /// It opens the bundle as the server does. That checks every manifest digest, the size and
    /// SHA-256 of every file except the unique indexes' runs, and that each segment's permutation
    /// maps one-to-one onto its rows. `--deep` hashes those runs too. It then confirms that the row space holds exactly the rows the segments claim, and computes
    /// each row's `tessera_id` again from the identity key, failing on the first row that
    /// differs.
    ///
    /// It also prints to stderr each indexed keyword column's count of distinct values against
    /// its rows, with a warning for a column whose values are unique per row. A warning does not
    /// fail the verify.
    Verify {
        /// The bundle directory, the one holding `CURRENT`.
        bundle: PathBuf,
        /// Also check the bundle's internal structures.
        ///
        /// The term lists must be sorted, free of duplicates and in range; dictionary records must
        /// not repeat; record blobs and Morton cells must agree with their indexes; each group-scoped
        /// render column must be present in every segment; each unique column's index must be
        /// hashed against the manifest, name at most one live item for a value and agree with the
        /// column's values in both directions; and `pairs.parquet`, when present, must match the
        /// term lists it was written with. Run it on a bundle no running server is writing to.
        #[arg(long)]
        deep: bool,
    },
    /// Split text into tokens with an analyser built into this binary. Each input line prints as
    /// one line, its tokens separated by tabs.
    ///
    /// When a `match` filter returns nothing, this shows how the index split the text.
    /// `/v1/meta` publishes each text column's analyser identity as
    /// `declared_scalars[].analyser`, such as `unicode/icu4x-2.2/p1`. The analyser's name is the
    /// part before the first `/`; run the query text through the analyser of that name. The
    /// conformance suite uses the command to compute expected `match` results with the analyser
    /// the index was built with.
    Tokenise {
        /// Text to split. Repeatable. With none, reads one input per line from stdin.
        #[arg(long = "text")]
        text: Vec<String>,
        /// The analyser to use, by name. An unknown name is refused with the list of names this
        /// binary carries.
        #[arg(long, value_name = "NAME", default_value = tessera_analyse::UNICODE)]
        analyser: String,
        /// Print the analyser's identity, the value a text column records, and exit.
        #[arg(long)]
        identity: bool,
    },
    /// Generators for the conformance suite's corpus. Internal, and not shown in help.
    #[command(hide = true)]
    Corpus {
        #[command(subcommand)]
        command: CorpusCommand,
    },
    /// Ask a running server whether it is ready, and exit 0 if it is or 1 if it is not.
    ///
    /// Finds `tessera.toml` as `tessera build` does and sends `GET /readyz` to the viewer address
    /// in `[serve]`. The server answers 503 while its write-ahead log cannot be written, when the
    /// thread that applies writes has stopped, and when part of the bundle is served from an
    /// older list of segments because the newest could not be used. Otherwise it answers 200,
    /// even while a write to disc hangs.
    ///
    /// The command exits 0 on a 200. It exits 1, with the reason on stderr, on any other answer,
    /// on no answer within `--timeout`, when nothing is listening, and when `tessera.toml` is
    /// refused or declares no viewer address. A server starts listening only once it has opened
    /// its bundle, so the check fails until then.
    ///
    /// Exit 1 means stop sending viewer requests to the server, and keep sending it deletions and
    /// suppressions, which it still applies when its log has failed. It does not call for a
    /// restart: a restart undoes each deletion or suppression the server answered with 500
    /// because the log could not be written, until that change is sent again.
    ///
    /// A viewer address of `0.0.0.0` or `[::]` is reached on loopback, so run the command on the
    /// machine or in the container the server runs in. The Docker image's health check runs it.
    Health {
        /// Read this `tessera.toml` instead of searching for one upward from the working
        /// directory.
        #[arg(long, value_name = "PATH")]
        deployment: Option<PathBuf>,
        /// Seconds the whole request may take, from connecting to reading the answer.
        #[arg(long, value_name = "SECONDS", default_value_t = 3)]
        timeout: u64,
    },
    /// Read every item a session token may see in one view, with the fields named, from a running
    /// server, and write them as Arrow IPC or Parquet.
    ///
    /// The server answers `POST /v1/items` a page at a time, several pages to a response, and ends
    /// each response with a cursor for the next. This requests responses until no row remains and
    /// writes each page as it arrives. Each of the route's fields is the argument of the same
    /// name, sent only when given, so the server's own setting applies otherwise.
    ///
    /// The columns are `tessera_id`, the fields in the order named, the system fields in the order
    /// named, then `tessera:matched` under `--keep-unmatched`. A category field is a dictionary
    /// column of its value keys, each page's dictionary holding the keys of its own rows. A read
    /// that returns no row writes these columns with no rows.
    ///
    /// A read cut short leaves the whole pages read before it in the output, exits 1 and prints
    /// the cursor to read the rest with. The first response's head, with the counts under
    /// `--count`, is printed on stderr at the end.
    ///
    /// For example, `tessera items --server http://127.0.0.1:8080 --view papers --fields
    /// title,year --system-fields labels --out papers.parquet`.
    Items(records::ItemsArgs),
    /// Read every artifact of one layer a session token is served, with the properties named, from
    /// a running server, and write them as Arrow IPC or Parquet.
    ///
    /// An artifact is one member of a layer: a cluster, a region, a node in a taxonomy. The read is
    /// carried across `POST /v1/artifacts` responses, and written, as `tessera items` carries and
    /// writes one. The columns are `tessera_id`, the properties in the order named, then
    /// `matched_count` under `--filters`. The rows are in order of level, and in the order they
    /// were published within a level.
    ///
    /// For example, `tessera artifacts --server http://127.0.0.1:8080 --view papers --layer
    /// clusters --fields key,masked_count --format ipc > clusters.arrows`.
    Artifacts(records::ArtifactsArgs),
    /// Serve the bundle that `tessera.toml` names.
    ///
    /// Finds `tessera.toml` as `tessera build` does, opens the bundle at `[bundle] path` and
    /// replays the write-ahead log at `[bundle] wal`, all before it binds any address. It then
    /// listens on the viewer, session and control addresses in `[serve]`. When all three are bound
    /// it prints one line of JSON to stdout naming them. Diagnostics go to stderr, in colour only
    /// when stderr is a terminal.
    ///
    /// SIGTERM or SIGINT stops the server at once, even while it opens the bundle, and it exits
    /// 0. Stopping ends every viewer session, because sessions are held in memory. A write is
    /// acknowledged only once the write-ahead log holds it on disc, so stopping loses no
    /// acknowledged write. A deletion or suppression answered with 500 because the log could not
    /// be written is in force but not on disc, and a restart undoes it until the change is sent
    /// again.
    ///
    /// It refuses to start, and exits 1, when `tessera.toml` is refused as `tessera build` would
    /// refuse it or lacks one of the three `[serve]` addresses. It refuses when the operator
    /// credential is not set, is empty, or its file cannot be read: under `[serve]`,
    /// `operator_credential_file` or `operator_credential_env` names the file or environment
    /// variable holding it. It refuses when `[catalogue] dir` is not set, or the catalogue there
    /// cannot be opened or is held open by another process. It also refuses when the bundle cannot be read
    /// and when the write-ahead log fails its checksum. An address that cannot be bound, such as
    /// one already in use, stops it with exit 1 after the bundle has opened.
    Serve {
        /// Read this `tessera.toml` instead of searching for one upward from the working
        /// directory.
        #[arg(long, value_name = "PATH")]
        deployment: Option<PathBuf>,
    },
    /// Log in on the viewer plane and print the session token and its `expires_at` as JSON.
    ///
    /// The password, API key or OIDC access token is read from the first line of stdin, never
    /// from an argument. The principal must hold `read`.
    ///
    /// For example, `tessera login --server http://127.0.0.1:8080 --principal ann <
    /// password.txt`.
    Login(identity::LoginArgs),
    /// End a session on the viewer plane. The session token is read from `TESSERA_TOKEN`, never
    /// from an argument.
    Logout(identity::LogoutArgs),
    /// Mint and revoke sessions on the session plane, and list and end sessions on the control
    /// plane.
    ///
    /// The session plane's verbs read their credential from `TESSERA_API_KEY`: an API key holding
    /// `authorise-as`, or the operator credential, which alone may name the session's terms or ask
    /// for a session reading every item.
    /// The control plane's read their credential from `TESSERA_CREDENTIAL` and need `admin`.
    Session {
        #[command(subcommand)]
        command: identity::SessionCommand,
    },
    /// Manage local principals on the control plane: people and services.
    ///
    /// Every verb reads the control plane's credential from `TESSERA_CREDENTIAL`, which is the
    /// operator credential, an API key or an OIDC access token, and needs `admin`. Each prints the
    /// server's JSON answer; a change answers how many sessions it ended.
    Principal {
        #[command(subcommand)]
        command: identity::PrincipalCommand,
    },
    /// Manage API keys on the control plane. The credential is read as `tessera principal` reads
    /// it.
    Key {
        #[command(subcommand)]
        command: identity::KeyCommand,
    },
    /// Manage local groups and their members on the control plane. The credential is read as
    /// `tessera principal` reads it.
    Group {
        #[command(subcommand)]
        command: identity::GroupCommand,
    },
    /// Grant a term or a permission to a principal or a group, on the control plane.
    ///
    /// A term says what the grantee may see, and a permission what it may do. The sessions the
    /// grant affects end, so they pick it up when they authorise again. The credential is read as
    /// `tessera principal` reads it.
    Grant(identity::GrantArgs),
    /// Revoke a term or a permission from a principal or a group, on the control plane. The
    /// credential is read as `tessera principal` reads it.
    RevokeGrant(identity::GrantArgs),
    /// Manage OIDC providers on the control plane. The credential is read as `tessera principal`
    /// reads it.
    Provider {
        #[command(subcommand)]
        command: identity::ProviderCommand,
    },
}

/// The corpus extent both verbs default to: the cell grid's own coordinates, which is what the
/// conformance fixtures build against (contracts §2.5 — the grid is 2^16 × 2^16).
const GRID_EXTENT: &str = "0,65536,0,65536";

#[derive(Subcommand)]
enum CorpusCommand {
    /// Expected properties for served rows: `fx_key` values in (one decimal per line), an Arrow
    /// IPC stream out on stdout — each key's item identity, position and every declared field.
    ///
    /// Takes no `--n`, and that absence is the point: an item's properties depend on the seed and
    /// the item alone (the generator is prefix-stable), so the verb answers for any key a
    /// response carried without being told how large the corpus was. The keys are `fx_key`
    /// values — item identities — never `tessera_id`s, which invert to entity ids and say nothing
    /// about items (correctness-suite §8).
    Items {
        /// The run's seed.
        #[arg(long)]
        seed: u64,
        /// Where the fx_key values come from: `-` for stdin, else a file path.
        #[arg(long)]
        ids: PathBuf,
        /// Quantisation extent as `x_min,x_max,y_min,y_max` (contracts §2.5).
        #[arg(long, value_parser = parse_extent, default_value = GRID_EXTENT)]
        extent: Bounds,
    },
    /// Expected masked count per depth-`zoom` tile for one principal: an Arrow IPC stream on
    /// stdout, `(tile, count)` ascending, non-empty tiles only.
    ///
    /// Per tile rather than one global total, because a single number passes any defect that
    /// moves rows between tiles while preserving the sum (correctness-suite §9.2). The counts are
    /// what the *corpus* holds: denies the harness has had accepted are its own to subtract.
    Census {
        /// The run's seed.
        #[arg(long)]
        seed: u64,
        /// The corpus size — the one derivation-free use of n: the census's loop bound.
        #[arg(long)]
        n: u64,
        /// Tile depth, 0..=16.
        #[arg(long)]
        zoom: u8,
        /// The principal's grant, in the mask catalogue's term-set encoding: comma-separated
        /// decimal terms.
        #[arg(long)]
        grant: String,
        /// Quantisation extent as `x_min,x_max,y_min,y_max` (contracts §2.5).
        #[arg(long, value_parser = parse_extent, default_value = GRID_EXTENT)]
        extent: Bounds,
    },
    /// Write a `tessera build`-able fixture to disk: `points.parquet`, `pairs.parquet`, the
    /// `[[layer]]`-bearing declaration (`corpus-config.toml`), and the artifact scale campaign's
    /// four fixture files — a flat (interval-plus-scatter) layer's roster and membership, the
    /// partition arm's roster and its enumerated twin's membership, and the boundary arm's roster
    /// (`artifact-delivery.md` §5.1, §5.4). `out` is created if it does not exist.
    ///
    /// Every file lands beside the declaration, because a `source` is a path relative to the
    /// document that names it (`configuration.md` §3) — there is nothing to move independently.
    Materialise {
        /// The run's seed.
        #[arg(long)]
        seed: u64,
        /// The corpus size — every file below is this scale's own (§5.4: a scale is a prefix
        /// filter on entity id, so membership and generating sets are declared fresh at each size
        /// rather than filtered from a larger run).
        #[arg(long)]
        n: u64,
        /// Where to write the fixture.
        #[arg(long)]
        out: PathBuf,
        /// Slots per term level, widening the term space from the default 1024
        /// (`16 * terms_per_level`) — must be a nonzero power of two. `65536` reaches the
        /// campaign's ~10⁶-term target.
        #[arg(long)]
        terms_per_level: Option<u32>,
        /// Quantisation extent as `x_min,x_max,y_min,y_max` (contracts §2.5).
        #[arg(long, value_parser = parse_extent, default_value = GRID_EXTENT)]
        extent: Bounds,
    },
    /// The artifact census: expected masked count per artifact of one closed-form layer, for one
    /// principal — an Arrow IPC stream on stdout, `(artifact, count)` ascending, non-empty
    /// artifacts only (the campaign's oracle; `artifact-delivery.md` §5.1).
    ///
    /// One O(*n*) pass per call, on [`tessera_corpus::Corpus::bucket_census`]'s shared driver — the
    /// same rule [`CorpusCommand::Census`] follows for tiles, extended to artifacts: an artifact
    /// this grant sees nothing of is absent, never a zero-count row.
    ArtifactCensus {
        /// The run's seed.
        #[arg(long)]
        seed: u64,
        /// The corpus size.
        #[arg(long)]
        n: u64,
        /// Which closed-form layer to census: `flat` (the interval-plus-scatter arm),
        /// `partition` (single-valued, whichever of its two `[[layer]]` forms — enumerated or
        /// attribute-predicate — since both name one relation), `boundary` (the spatial arm's
        /// authored tiles), or `treed` (the lineage arm — a node's count includes every
        /// descendant's, root first).
        #[arg(long)]
        layer: ArtifactCensusLayer,
        /// The principal's grant, in the mask catalogue's term-set encoding.
        #[arg(long)]
        grant: String,
        /// Slots per term level, matching whatever `materialise --terms-per-level` this corpus
        /// was written with — the grant is checked against the same term space.
        #[arg(long)]
        terms_per_level: Option<u32>,
        /// Quantisation extent as `x_min,x_max,y_min,y_max` (contracts §2.5).
        #[arg(long, value_parser = parse_extent, default_value = GRID_EXTENT)]
        extent: Bounds,
    },
}

/// [`CorpusCommand::ArtifactCensus`]'s `--layer` values — the four closed-form arms
/// `tessera-corpus` states a census over.
#[derive(Clone, Copy, clap::ValueEnum)]
enum ArtifactCensusLayer {
    Flat,
    Partition,
    Boundary,
    Treed,
}

/// `GET /readyz` on the viewer plane, waiting at most `timeout` to connect and for the answer.
fn readyz(mut addr: std::net::SocketAddr, timeout: std::time::Duration) -> Result<(), String> {
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr {
            std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
            std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("starting the HTTP client: {e}"))?;
    let status = client
        .get(format!("http://{addr}/readyz"))
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                format!("{addr} did not answer /readyz within {} s", timeout.as_secs())
            } else if e.is_connect() {
                format!("no server answering at {addr}: {}", innermost(&e))
            } else {
                format!("{addr}: {}", innermost(&e))
            }
        })?
        .status();
    match status.as_u16() {
        200 => Ok(()),
        code => Err(format!("{addr} answered /readyz with {code}: not ready")),
    }
}

/// The last error in `e`'s chain of sources, which names the cause: an HTTP client's own message
/// names only the request that failed.
fn innermost(e: &dyn std::error::Error) -> String {
    let mut last = e;
    while let Some(source) = last.source() {
        last = source;
    }
    last.to_string()
}

/// Exit with success on the first SIGTERM or SIGINT, from a thread of its own so the handler is
/// in place before the bundle opens. Inside a container the server is PID 1, which the kernel
/// sends no signal it has no handler for.
fn exit_on_termination() {
    use tokio::signal::unix::{signal, SignalKind};
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("tessera serve: SIGTERM and SIGINT will not stop the server: {e}");
            return;
        }
    };
    let (term, int) = runtime.block_on(async {
        (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        )
    });
    let (mut term, mut int) = match (term, int) {
        (Ok(term), Ok(int)) => (term, int),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("tessera serve: SIGTERM and SIGINT will not stop the server: {e}");
            return;
        }
    };
    std::thread::spawn(move || {
        let name = runtime.block_on(async {
            tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
            }
        });
        eprintln!("tessera serve: stopped on {name}");
        std::process::exit(0);
    });
}

fn parse_extent(raw: &str) -> Result<Bounds, String> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!(
            "expected four comma-separated numbers 'x_min,x_max,y_min,y_max', got '{raw}'"
        ));
    }
    let mut values = [0f64; 4];
    for (slot, text) in values.iter_mut().zip(parts) {
        *slot = text
            .parse::<f64>()
            .map_err(|e| format!("'{text}' is not a number: {e}"))?;
    }
    let extent = Bounds {
        x_min: values[0],
        x_max: values[1],
        y_min: values[2],
        y_max: values[3],
    };
    extent.validate()?;
    Ok(extent)
}

/// `24g` / `512m` / `1073741824` — the human forms a budget is actually typed in.
fn parse_byte_size(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let (digits, multiplier) = match value.chars().last() {
        Some('g') | Some('G') => (&value[..value.len() - 1], 1u64 << 30),
        Some('m') | Some('M') => (&value[..value.len() - 1], 1u64 << 20),
        Some('k') | Some('K') => (&value[..value.len() - 1], 1u64 << 10),
        _ => (value, 1),
    };
    digits
        .parse::<u64>()
        .map_err(|e| format!("not a byte size: {e}"))?
        .checked_mul(multiplier)
        .ok_or_else(|| "byte size overflows u64".to_string())
}

/// Everything `tessera build` and `tessera check` both resolve, before either does its own work.
///
/// **One resolution, not two.** The deployment file found by walking up, the declaration it names,
/// the `--file` overrides against it — a second copy of that in the check verb would be a copy
/// free to drift, and the whole value of a check is that it saw what the build will see.
struct Declaration {
    deployment: tessera_config::Config,
    config: tessera_build::config::Config,
}

fn resolve_declaration(
    deployment: Option<&Path>,
    config: Option<PathBuf>,
    file: Vec<(String, PathBuf)>,
    strictness: tessera_build::config::Strictness,
) -> Result<Declaration, String> {
    // **The deployment file first, because everything else is read through it**: where the
    // declaration is and where the bundle goes. A missing one is a refusal naming what to create
    // (configuration.md §3) — never a silent set of defaults, since every path in it is a decision.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (_, deployment) = tessera_config::open(deployment, &cwd)?;
    let schema_path = config.unwrap_or_else(|| deployment.schema_path.clone());
    let bindings = collect_bindings(file)?;
    let config = tessera_build::config::Config::parse_with(&schema_path, &bindings, strictness)
        .map_err(|e| e.to_string())?;
    Ok(Declaration {
        deployment,
        config,
    })
}

/// The `--file` bindings as one map, refusing a key bound twice.
///
/// **Last-one-wins is not available to a binding**: the two paths are two corpora, and a build that
/// silently took the second would produce a bundle nobody can tell from one built on the first.
fn collect_bindings(
    file: Vec<(String, PathBuf)>,
) -> Result<std::collections::HashMap<String, PathBuf>, String> {
    let mut bindings: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::with_capacity(file.len());
    for (key, path) in file {
        if let Some(already) = bindings.get(&key) {
            return Err(format!(
                "--file bound '{key}' twice, to {} and to {}. Which file a source names is not a \
                 last-one-wins question: the two are two different corpora",
                already.display(),
                path.display()
            ));
        }
        bindings.insert(key, path);
    }
    Ok(bindings)
}

/// `--file NAME=PATH`. Split at the **first** `=` so a path may contain one.
/// Prints one line per pipeline stage as it completes, for `--stage-timings`.
///
/// **Peak RSS is the process's high-water at the moment the stage ended**, not the stage's own —
/// it only ever rises, so a stage that adds nothing repeats the last figure. What it locates is
/// the stage the peak arrived in, which is the question `--memory-budget` is answered against.
///
/// **One observer, two sinks**, rather than a second observer for `--stage-timings-json`: the
/// pipeline takes one, and a wrapper that fanned out to two would have to re-sample nothing but
/// still be a second place a stage could be dropped from.
struct StageTimings {
    /// Print a line per stage — `--stage-timings`. Off when only the JSON path was asked for.
    print: bool,
    /// Collect the records — `--stage-timings-json`. `None` when only the lines were asked for.
    json: Option<tessera_build::observer::JsonStageTimings>,
}

impl tessera_build::observer::BuildObserver for StageTimings {
    fn stage_end(
        &self,
        stage: tessera_build::observer::BuildStage,
        elapsed: std::time::Duration,
        rows: u64,
        peak_rss_kib: u64,
    ) {
        if self.print {
            eprintln!(
                "stage {:>16}  {:>8.2}s  rows={rows:<12} peak={:>6} MiB",
                stage.name(),
                elapsed.as_secs_f64(),
                peak_rss_kib / 1024,
            );
        }
        if let Some(json) = &self.json {
            tessera_build::observer::BuildObserver::stage_end(
                json,
                stage,
                elapsed,
                rows,
                peak_rss_kib,
            );
        }
    }
}

fn parse_file_binding(raw: &str) -> Result<(String, PathBuf), String> {
    let (key, path) = raw.split_once('=').ok_or_else(|| {
        format!(
            "--file expects NAME=PATH, got '{raw}' (no '=' — the name is a key of `[sources]`, the \
             path is the file it should read instead)"
        )
    })?;
    if key.is_empty() || path.is_empty() {
        return Err(format!(
            "--file '{raw}': both the name and the path must be non-empty"
        ));
    }
    Ok((key.to_string(), PathBuf::from(path)))
}

/// Print the schema's residency cost against the corpus about to be built (§2.3).
///
/// **Totalled, not per column.** Several categories are what makes the cost bite, and a
/// per-attribute table lets each one look affordable on its own. §10.5 prices a hot column at
/// 0.93 GiB per byte per row per 10⁹ items, which is what the projection below reproduces.
///
/// Warns on what §2.3 names as breaking in practice — which is now one thing, not three.
///
/// `discovered` + `u8` is unreachable while discovered vocabularies are refused at parse, and
/// `render_in` is refused outright, so its every-view consequence is stated once rather than per
/// column.
///
/// **Dense codes under `visibility = "derived"` is deliberately not warned about** (owner ruling,
/// 2026-08-07), and this is worth recording because §2.3 asks for the warning and a reader will
/// otherwise add it. That warning guards vocabulary *cardinality* — a visible code being a lower
/// bound on how many values exist. The owner does not hold cardinality as a threat. What must be
/// enforced is the other half: **a principal may see a category value only if it belongs to data
/// they can see** — §3.3's membership-derived visibility, which is a property of the read path,
/// not of how an author numbered their codes. A cardinality warning here would be mechanism that
/// looks like access control and is not.
fn report_residency(schema: &tessera_build::config::Schema, limit: Option<u64>) {
    let columns = schema.attributes.len();
    match schema.row_bits() {
        Some(per_row_bits) => {
            // Reported in bytes because that is the unit §10.5 prices a column in, but computed
            // from bits and shown to two places: a schema of `bool`s costs a real fraction of a
            // byte per row, and rounding it to zero would report the cheap case as free.
            let per_row = per_row_bits as f64 / 8.0;
            eprintln!(
                "schema: {columns} column(s), {per_row:.2} B/row against the 12 B fixed row \
                 (+{:.0}%)",
                per_row / 12.0 * 100.0
            );
            if let Some(rows) = limit {
                let gib = per_row * rows as f64 / (1024.0 * 1024.0 * 1024.0);
                eprintln!("        {gib:.2} GiB resident at {rows} items");
            }
            eprintln!(
                "        {:.2} GiB per 10^9 items — unalterable without rewriting the corpus",
                per_row * 0.93
            );
        }
        None => eprintln!("schema: {columns} column(s), variable width per row"),
    }
    eprintln!(
        "        every column materialises in EVERY view — including ones whose items carry no \
         value for it"
    );
}

/// `tessera corpus items` (correctness-suite §12.1): served `fx_key` values in, their expected
/// items out. The corpus is constructed with `n = 0` because the lookups take no part in it —
/// see the verb's own doc. The `partition` column is answered here for the same reason: it is a
/// function of `(seed, layer, e)`, not of the corpus's size (`tessera-corpus`'s `partition.rs`).
fn corpus_items(seed: u64, ids: &Path, extent: Bounds) -> ExitCode {
    use arrow::array::{
        ArrayRef, Float64Builder, StringBuilder, TimestampMicrosecondBuilder, UInt32Builder,
        UInt64Builder,
    };
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let corpus = match tessera_corpus::Corpus::new(seed, 0, extent) {
        Ok(corpus) => corpus,
        Err(e) => {
            eprintln!("corpus items: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = if ids == Path::new("-") {
        use std::io::Read;
        let mut buffer = String::new();
        if let Err(e) = std::io::stdin().lock().read_to_string(&mut buffer) {
            eprintln!("corpus items: reading stdin: {e}");
            return ExitCode::FAILURE;
        }
        buffer
    } else {
        match std::fs::read_to_string(ids) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("corpus items: {}: {e}", ids.display());
                return ExitCode::FAILURE;
            }
        }
    };

    let mut fx_key = UInt64Builder::new();
    let mut e_col = UInt64Builder::new();
    let mut x = Float64Builder::new();
    let mut y = Float64Builder::new();
    let mut weight = UInt32Builder::new();
    let mut seen_at = TimestampMicrosecondBuilder::new();
    let mut bay = StringBuilder::new();
    let mut tag = StringBuilder::new();
    let mut blurb = StringBuilder::new();
    let mut partition = UInt32Builder::new();
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(fx) = line.parse::<u64>() else {
            // Refused rather than skipped: a non-decimal line means the caller is feeding this
            // verb something other than fx_key values, and a silently shortened answer would be
            // compared as if it were complete.
            eprintln!(
                "corpus items: line {}: '{line}' is not a decimal fx_key",
                line_no + 1
            );
            return ExitCode::FAILURE;
        };
        let e = corpus.item_of_fx_key(fx);
        let item = corpus.item(e);
        fx_key.append_value(fx);
        e_col.append_value(e);
        x.append_value(item.x);
        y.append_value(item.y);
        weight.append_option(item.weight);
        seen_at.append_option(item.seen_at);
        bay.append_option(item.bay);
        tag.append_option(item.tag.as_deref());
        blurb.append_option(item.blurb.as_deref());
        partition.append_value(
            corpus.partition_artifact_of(tessera_corpus::materialise::PARTITION_LAYER, e) as u32,
        );
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("fx_key", DataType::UInt64, false),
        Field::new("e", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("weight", DataType::UInt32, true),
        Field::new(
            "seen_at",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
        Field::new("bay", DataType::Utf8, true),
        Field::new("tag", DataType::Utf8, true),
        Field::new("blurb", DataType::Utf8, true),
        Field::new("partition", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(fx_key.finish()) as ArrayRef,
            Arc::new(e_col.finish()),
            Arc::new(x.finish()),
            Arc::new(y.finish()),
            Arc::new(weight.finish()),
            Arc::new(seen_at.finish()),
            Arc::new(bay.finish()),
            Arc::new(tag.finish()),
            Arc::new(blurb.finish()),
            Arc::new(partition.finish()),
        ],
    )
    .expect("columns built to one length from one loop");
    let schema = batch.schema();
    write_arrow_stdout(&schema, &[batch], "corpus items")
}

/// `tessera corpus census` (correctness-suite §9.2, §12.1): the expected masked count per tile,
/// computed in one O(n) pass here so the driver compares two count vectors.
fn corpus_census(seed: u64, n: u64, zoom: u8, grant: &str, extent: Bounds) -> ExitCode {
    use arrow::array::{ArrayRef, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    if zoom > 16 {
        eprintln!("corpus census: zoom {zoom} exceeds grid depth 16 (contracts §2.5)");
        return ExitCode::FAILURE;
    }
    let grant = match tessera_corpus::Grant::parse(grant) {
        Ok(grant) => grant,
        Err(e) => {
            eprintln!("corpus census: {e}");
            return ExitCode::FAILURE;
        }
    };
    let corpus = match tessera_corpus::Corpus::new(seed, n, extent) {
        Ok(corpus) => corpus,
        Err(e) => {
            eprintln!("corpus census: {e}");
            return ExitCode::FAILURE;
        }
    };
    let counts = corpus.census(zoom, &grant);

    let schema = Arc::new(Schema::new(vec![
        Field::new("tile", DataType::UInt64, false),
        Field::new("count", DataType::UInt64, false),
    ]));
    // Chunked: at deep zooms the tile vector can run to millions of rows, and a bounded batch
    // size keeps the stream's consumers (and this process's transient) flat.
    let batches: Vec<RecordBatch> = counts
        .chunks(1 << 16)
        .map(|chunk| {
            let tiles = UInt64Array::from_iter_values(chunk.iter().map(|(tile, _)| *tile));
            let totals = UInt64Array::from_iter_values(chunk.iter().map(|(_, count)| *count));
            RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(tiles) as ArrayRef, Arc::new(totals)],
            )
            .expect("two columns of one chunk's length")
        })
        .collect();
    write_arrow_stdout(&schema, &batches, "corpus census")
}

/// A corpus at the default term space unless `terms_per_level` overrides it — the constructor
/// [`CorpusCommand::Materialise`] and [`CorpusCommand::ArtifactCensus`] share.
fn corpus_with_terms_per_level(
    seed: u64,
    n: u64,
    extent: Bounds,
    terms_per_level: Option<u32>,
) -> Result<tessera_corpus::Corpus, String> {
    match terms_per_level {
        Some(width) => tessera_corpus::Corpus::with_terms_per_level(seed, n, extent, width),
        None => tessera_corpus::Corpus::new(seed, n, extent),
    }
}

/// `tessera corpus materialise` (`artifact-delivery.md` §5.1, §5.4): the generator's build inputs
/// plus the artifact scale campaign's fixture, written to `out`.
fn corpus_materialise(
    seed: u64,
    n: u64,
    out: &Path,
    terms_per_level: Option<u32>,
    extent: Bounds,
) -> ExitCode {
    let corpus = match corpus_with_terms_per_level(seed, n, extent, terms_per_level) {
        Ok(corpus) => corpus,
        Err(e) => {
            eprintln!("corpus materialise: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::create_dir_all(out) {
        eprintln!("corpus materialise: creating {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = corpus.write_points_parquet(&out.join("points.parquet")) {
        eprintln!("corpus materialise: writing points.parquet: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = corpus.write_pairs_parquet(&out.join("pairs.parquet")) {
        eprintln!("corpus materialise: writing pairs.parquet: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::write(out.join("corpus-config.toml"), corpus.config_toml()) {
        eprintln!("corpus materialise: writing corpus-config.toml: {e}");
        return ExitCode::FAILURE;
    }
    let counts = match corpus.write_artifact_fixtures(out) {
        Ok(counts) => counts,
        Err(e) => {
            eprintln!("corpus materialise: writing the artifact fixture: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "materialised {} (seed {seed}, n {n}, term space {}): {} flat artifacts / {} member \
         rows, {} partition artifacts / {} member rows, {} boundary artifacts, {} treed \
         artifacts / {} member rows",
        out.display(),
        corpus.term_space(),
        counts.flat_artifacts,
        counts.flat_member_rows,
        counts.partition_artifacts,
        counts.partition_member_rows,
        counts.boundary_artifacts,
        counts.treed_artifacts,
        counts.treed_member_rows,
    );
    ExitCode::SUCCESS
}

/// `tessera corpus artifact-census` (`artifact-delivery.md` §5.1): the expected masked count per
/// artifact of one closed-form layer, for one principal.
fn corpus_artifact_census(
    seed: u64,
    n: u64,
    layer: ArtifactCensusLayer,
    grant: &str,
    terms_per_level: Option<u32>,
    extent: Bounds,
) -> ExitCode {
    use arrow::array::{ArrayRef, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let corpus = match corpus_with_terms_per_level(seed, n, extent, terms_per_level) {
        Ok(corpus) => corpus,
        Err(e) => {
            eprintln!("corpus artifact-census: {e}");
            return ExitCode::FAILURE;
        }
    };
    let grant = match tessera_corpus::Grant::parse_bounded(grant, corpus.term_space()) {
        Ok(grant) => grant,
        Err(e) => {
            eprintln!("corpus artifact-census: {e}");
            return ExitCode::FAILURE;
        }
    };
    let counts = match layer {
        ArtifactCensusLayer::Flat => corpus.flat_artifact_census(
            tessera_corpus::materialise::FLAT_LAYER,
            tessera_corpus::materialise::FIXTURE_LEVEL,
            &grant,
        ),
        ArtifactCensusLayer::Partition => {
            corpus.partition_artifact_census(tessera_corpus::materialise::PARTITION_LAYER, &grant)
        }
        ArtifactCensusLayer::Boundary => corpus.boundary_artifact_census(
            tessera_corpus::materialise::BOUNDARY_LAYER,
            tessera_corpus::materialise::FIXTURE_LEVEL,
            &grant,
        ),
        ArtifactCensusLayer::Treed => {
            corpus.treed_artifact_census(tessera_corpus::materialise::TREED_LAYER, &grant)
        }
    };

    let schema = Arc::new(Schema::new(vec![
        Field::new("artifact", DataType::UInt64, false),
        Field::new("count", DataType::UInt64, false),
    ]));
    let batches: Vec<RecordBatch> = counts
        .chunks(1 << 16)
        .map(|chunk| {
            let artifacts = UInt64Array::from_iter_values(chunk.iter().map(|(a, _)| *a));
            let totals = UInt64Array::from_iter_values(chunk.iter().map(|(_, count)| *count));
            RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(artifacts) as ArrayRef, Arc::new(totals)],
            )
            .expect("two columns of one chunk's length")
        })
        .collect();
    write_arrow_stdout(&schema, &batches, "corpus artifact-census")
}

/// One Arrow IPC stream on stdout. An empty batch list still writes a valid stream carrying only
/// the schema — "no tiles" must be distinguishable from "no output".
fn write_arrow_stdout(
    schema: &std::sync::Arc<arrow::datatypes::Schema>,
    batches: &[arrow::record_batch::RecordBatch],
    verb: &str,
) -> ExitCode {
    let out = std::io::stdout().lock();
    let write = || -> Result<(), arrow::error::ArrowError> {
        let mut writer = arrow::ipc::writer::StreamWriter::try_new(out, schema)?;
        for batch in batches {
            writer.write(batch)?;
        }
        writer.finish()
    };
    match write() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{verb}: writing Arrow stream: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            deployment,
            out,
            limit,
            strict,
            config,
            file,
            no_oracle_pairs,
            batch_items,
            memory_budget,
            stage_timings,
            stage_timings_json,
        } => {
            // **The deployment file first, because everything else is read through it**: where the
            // declaration is and where the bundle goes. A missing one is a refusal naming what to
            // create (configuration.md §3) — never a silent set of defaults, since every path in
            // it is a decision.
            //
            // Parsed and refused before any work: a declaration refusal is an operator's typo, and
            // discovering it after a multi-minute build has written a bundle prefix costs the
            // whole build. Every rule in `tessera_build::config`
            // fires here, against no data at all — which is also the whole of what `tessera check`
            // does, through this same function.
            let Declaration {
                deployment,
                config,
            } = match resolve_declaration(
                deployment.as_deref(),
                config,
                file,
                tessera_build::config::Strictness::Build,
            ) {
                Ok(resolved) => resolved,
                Err(detail) => {
                    eprintln!("build refused: {detail}");
                    return ExitCode::FAILURE;
                }
            };
            let out = out.unwrap_or_else(|| deployment.bundle_path.clone());

            // **A build materialises every declared view and every view of every group**
            // (`views.md` §7). `--view` is withdrawn with the refusal it went with: there is
            // nothing to choose between when the answer is all of them.
            let registry = match config.build_views() {
                Ok(registry) => registry,
                Err(e) => {
                    eprintln!("build refused: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // The anchor decides which view's Morton code orders ids within a signature group
            // (decision 0112). Resolved before any file is read: it is a permanent property of
            // the corpus (I9), so a declaration that has not said which view it is refuses here
            // rather than after a multi-minute build.
            let anchor = match config.anchor_view(&registry) {
                Ok(anchor) => anchor,
                Err(e) => {
                    eprintln!("build refused: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // `--limit`'s attribute: the declaration's one unique integer attribute, refused
            // before any file is read where there is not exactly one.
            let limited = match tessera_build::ids::Limit::of(&config.schema, limit) {
                Ok(found) => found.is_some(),
                Err(e) => {
                    eprintln!("build refused: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // The files this build reads, resolved from the declaration and any overrides — and
            // every absence a refusal here rather than an empty read (configuration.md §8).
            let acquired = match config.acquire() {
                Ok(acquired) => acquired,
                Err(e) => {
                    eprintln!("build refused: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // **The frame, resolved before any work — and surveyed in the same pass, per view.**
            // `auto` fits the box; every other spelling is already the answer and the pass is
            // what establishes how much of the corpus that frame clamps. Printed for every view,
            // because the extent is the view's (decision 0040) and four plausible-looking numbers
            // are only checkable beside the data's own box.
            let mut acquired_views = Vec::with_capacity(registry.len());
            for view in &registry {
                match tessera_build::config::acquire_view(view) {
                    Ok(acquired) => acquired_views.push(acquired),
                    Err(e) => {
                        eprintln!("build refused: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            // **One frame per view, except on a group, where one frame covers every view of it**
            // (`views.md` §3.1): a group's views differ by a key and by per-view metadata and by
            // nothing else, so `auto` is fitted over the union of their sources and a stated
            // extent is surveyed against every one of them. The views of a group are contiguous
            // in the registry, so the fold is a scan.
            //
            // Under `--limit` the build fits each frame once the identity pass has decided which
            // rows it keeps, and each view holds a placeholder until then.
            let mut framings: Vec<tessera_build::Framing> = Vec::new();
            let mut extents: Vec<tessera_spatial::Bounds> = Vec::with_capacity(registry.len());
            let mut frame_of: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for (index, view) in registry.iter().enumerate() {
                let owner = match &view.group {
                    Some(membership) => membership.group.as_str(),
                    None => view.id.as_str(),
                };
                match frame_of.get(owner) {
                    Some(&first) => {
                        extents.push(extents[first]);
                        continue;
                    }
                    None => frame_of.insert(owner, index),
                };
                let members: Vec<usize> = registry
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| match (&v.group, &view.group) {
                        (Some(a), Some(b)) => a.group == b.group,
                        _ => v.id == view.id,
                    })
                    .map(|(i, _)| i)
                    .collect();
                let subject = match &view.group {
                    Some(membership) => format!("view group '{}'", membership.group),
                    None => format!("view '{}'", view.id),
                };
                if limited {
                    framings.push(tessera_build::Framing {
                        subject,
                        projection: view.projection,
                        extent: view.extent,
                        views: members,
                    });
                    extents.push(tessera_spatial::Bounds {
                        x_min: 0.0,
                        x_max: 1.0,
                        y_min: 0.0,
                        y_max: 1.0,
                    });
                    continue;
                }
                let sources: Vec<tessera_build::config::FrameSource> = members
                    .iter()
                    .map(|&i| tessera_build::config::FrameSource {
                        points: &acquired_views[i].points,
                        fields: &acquired_views[i].point_fields,
                        select: acquired_views[i].select.as_ref(),
                        kept: None,
                    })
                    .collect();
                let frame = match tessera_build::config::frame_of(
                    &subject,
                    view.projection,
                    &view.extent,
                    &sources,
                ) {
                    Ok(frame) => frame,
                    Err(e) => {
                        eprintln!("build refused: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                eprintln!("{}", frame.report());
                extents.push(frame.extent);
            }

            let view_args: Vec<tessera_build::ViewArgs> = registry
                .iter()
                .zip(acquired_views)
                .zip(&extents)
                .map(|((view, acquired_view), extent)| tessera_build::ViewArgs {
                    // **This view's own gate** (`views.md` §6), as the declaration compiled it:
                    // the plain view's `visibility`, or — for a view of a group — its roster
                    // record's own. The group's half travels on the group descriptor beside it.
                    visibility: view.visibility.clone(),
                    view_id: view.id.clone(),
                    projection: view.projection,
                    extent: *extent,
                    points: acquired_view.points,
                    point_fields: acquired_view.point_fields,
                    select: acquired_view.select,
                    access: acquired_view.access,
                })
                .collect();
            // **One column per view of the group** (`views.md` §5), resolved against the registry
            // the build just enumerated: the views a family covers are the ones its group owns,
            // and each of them already says where its rows are.
            let scoped_attributes: Vec<tessera_build::ScopedColumnFamily> = config
                .scoped_attributes
                .iter()
                .map(|scoped| tessera_build::ScopedColumnFamily {
                    attribute: scoped.attribute.clone(),
                    group: scoped.group.clone(),
                    views: registry
                        .iter()
                        .enumerate()
                        .filter(|(_, view)| {
                            view.group
                                .as_ref()
                                .is_some_and(|group| group.group == scoped.group)
                        })
                        .map(|(index, _)| index)
                        .collect(),
                    source: scoped.source.clone(),
                })
                .collect();
            // **A layer naming a group is drawn on every view of it** (`views.md` §2, §3.5),
            // and the expansion happens here, against the registry the build just enumerated: a
            // build materialises the views that exist, and a layer's extents are per row space.
            // The rule itself is [`Config::expand_layer_views`], which `tessera check` sizes its
            // shape layers through so that the two entry points cannot disagree about which views
            // a layer is drawn on.
            let mut config = config;
            for layer in &mut config.layers {
                layer.views =
                    tessera_build::config::Config::expand_layer_views(&registry, &layer.views);
            }
            // **A scoped layer is a different artifact set per view of one group** (§3.5): its
            // rows say which view each artifact belongs to, under the layer's own `fields.view`,
            // and the group's keys are what a stray value is refused against.
            let scoped_layers: std::collections::BTreeMap<String, tessera_build::ScopedLayer> =
                config
                    .scopes
                    .layers
                    .iter()
                    .map(|(layer, group)| {
                        let mut keys: Vec<String> = registry
                            .iter()
                            .filter_map(|view| view.group.as_ref())
                            .filter(|membership| {
                                &membership.group == group
                                    || membership.members_of.as_ref() == Some(group)
                            })
                            .map(|membership| membership.key.clone())
                            .collect();
                        keys.sort();
                        keys.dedup();
                        let column = config
                            .layer_sources
                            .iter()
                            .find(|source| &source.name == layer)
                            .and_then(|source| match &source.artifacts {
                                Some(tessera_build::config::ArtifactSource::File {
                                    fields,
                                    ..
                                }) => Some(fields.of("view").to_string()),
                                _ => None,
                            })
                            .unwrap_or_else(|| "view".to_string());
                        (
                            layer.clone(),
                            tessera_build::ScopedLayer {
                                group: group.clone(),
                                column,
                                keys,
                            },
                        )
                    })
                    .collect();
            // Read out before the declaration is broken up into build arguments: it is a
            // property of the declaration, and every value in it exists by now.
            let disclosure = tessera_build::disclosure::Disclosure::of(&config);
            // The group registry beside it, and before the declaration is broken up for the same
            // reason: it reads the declaration's groups and the frames the views resolved to
            // (`views.md` §3.2).
            let groups = config.group_registry(&registry, &view_args);
            let schema = config.schema;
            // §2.3: the cost is reported, never hidden — a hot column is baked into every row and
            // is unalterable without rewriting the corpus, so the operator sees the per-row and
            // total figures at the moment they can still change the declaration. Reported and
            // **never refused**: Appendix A is a sizing table with no deployment ceiling, and
            // giving this a refusal means giving Appendix A a ceiling first, which is an owner
            // decision rather than a derivable number.
            if !schema.is_empty() {
                report_residency(&schema, limit);
            }

            let args = tessera_build::BuildArgs {
                views: view_args,
                anchor,
                groups,
                scoped_attributes,
                attribute_sources: acquired.attribute_sources,
                out: out.clone(),
                limit,
                strict,
                identity_key: IdentityKey::generate(),
                shard_id: 0,
                emit_oracle_pairs: !no_oracle_pairs,
                batch_items,
                memory_budget,
                band_rows: None,
                schema,
                layers: config.layers,
                layer_inputs: acquired.layers,
                scoped_layers,
            };
            let observer = StageTimings {
                print: stage_timings,
                json: stage_timings_json
                    .is_some()
                    .then(tessera_build::observer::JsonStageTimings::new),
            };
            let built = if stage_timings || stage_timings_json.is_some() {
                tessera_build::build_framed(&args, &framings, &observer)
            } else {
                tessera_build::build_framed(&args, &framings, &tessera_build::NoopObserver)
            };
            // Written whether the build succeeded or failed: a build that died in `layers` is
            // exactly the one whose per-stage record is worth having, and the records collected
            // before the failure are as true as any other.
            if let (Some(path), Some(json)) = (&stage_timings_json, &observer.json) {
                if let Err(e) = json.write(path) {
                    eprintln!("stage timings: writing {}: {e}", path.display());
                }
            }
            match built {
                Ok(report) => {
                    // **The other half of the frame report**, and the half the clamp count
                    // cannot see: data far too small for its frame clamps nothing, and every
                    // stored position is correct while nearly all the resolution is gone.
                    // Printed as raw numbers always — a frame this does not warn about is one
                    // the caller can still judge — and emphatically past the collapse threshold.
                    // Never a refusal: a coarse map is stored correctly, and may be meant.
                    //
                    // **Per view, because the frame is** (decision 0040): two views of one
                    // bundle may give the corpus quite different resolution.
                    for view in &report.views {
                        eprintln!("{}", view.occupancy.report(&view.view_id));
                        if let Some(detail) = view.occupancy.warning(&view.view_id) {
                            eprintln!("{detail}");
                        }
                        // What this view's term images cost and came to, per view because a
                        // group's keys are separate views over one dictionary and each pays its
                        // own table and payload (ruling G, the term-images memo). Absent for a
                        // view with no rows.
                        if let Some(images) = &view.term_images {
                            eprintln!("{}", images.report(&view.view_id));
                        }
                    }
                    if let Err(e) = tessera_build::write_disclosure_report(&out, &disclosure) {
                        eprintln!("build FAILED: writing reports/disclosure.json: {e}");
                        return ExitCode::FAILURE;
                    }
                    // **The rows the identity rule refused**, counted by file and reason with a few
                    // of the values they carried: the build went on without them, as an ingest
                    // refusing rows one at a time would.
                    for line in tessera_build::describe_refused(&report.refused) {
                        eprintln!("refused: {line}");
                    }
                    if let Err(e) = tessera_build::write_refused_report(&out, &report.refused) {
                        eprintln!("build FAILED: writing reports/refused.json: {e}");
                        return ExitCode::FAILURE;
                    }
                    println!(
                        "built {} ({}): {} items, {} terms, {} pairs, {} bytes on disk, {} \
                         artifact(s) minted, {} unclustered member row(s), {} row(s) refused",
                        out.display(),
                        report.prefix,
                        report.items,
                        report.terms,
                        report.pairs,
                        report.bundle_bytes,
                        report.minted_artifacts,
                        report.unclustered_member_rows,
                        report
                            .refused
                            .iter()
                            .filter(|entry| entry.is_refusal())
                            .map(|entry| entry.rows)
                            .sum::<u64>(),
                    );
                    // The per-view shapes, which is what a multi-view build has to say and a
                    // single total cannot: a view holds a subset of entity space (`views.md` §8).
                    for view in &report.views {
                        println!("  view {}: {} row(s)", view.view_id, view.rows);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("build FAILED: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Tokenise {
            text,
            analyser,
            identity,
        } => {
            let Some(analyser) = tessera_analyse::analyser(&analyser) else {
                eprintln!(
                    "'{analyser}' is not an analyser this binary carries. Available: {}",
                    tessera_analyse::ANALYSER_NAMES.join(", ")
                );
                return ExitCode::FAILURE;
            };
            if identity {
                println!("{}", analyser.identity());
                let _ = text;
                return ExitCode::SUCCESS;
            }
            // Tab-separated because a token can contain anything but a tab or a newline: the
            // segmenter's word-like segments never span a line break, and a caller that split on
            // spaces would corrupt nothing here but would elsewhere.
            let emit = |line: &str| println!("{}", analyser.tokens(line).join("\t"));
            if text.is_empty() {
                use std::io::BufRead;
                for line in std::io::stdin().lock().lines() {
                    emit(&line.expect("a readable line on stdin"));
                }
            } else {
                for line in &text {
                    emit(line);
                }
            }
            ExitCode::SUCCESS
        }
        Command::Verify { bundle, deep } => {
            let print_shallow = |report: &tessera_build::VerifyReport| {
                println!(
                    "OK {} ({}): {} partition(s), {} view(s), {} segment(s), {} rows, \
                     entity_id_high_water {}",
                    bundle.display(),
                    report.prefix,
                    report.partitions,
                    report.views,
                    report.segments,
                    report.rows,
                    report.entity_id_high_water
                );
                // Each indexed keyword against the rows carrying it, and the warning a key unique
                // per row earns: the same figures the build printed, read back from the bundle
                // (`tessera_build::unique_key`). Reported, never a failure.
                for column in &report.keyword_cardinalities {
                    eprintln!("{}", column.report());
                    if let Some(warning) = column.warning(report.bundle_bytes) {
                        eprintln!("{warning}");
                    }
                }
            };
            if deep {
                match tessera_build::verify_deep(&bundle, &tessera_build::VerifyOpts::default()) {
                    Ok(report) => {
                        print_shallow(&report.shallow);
                        println!(
                            "deep: {} term(s), {} delta tier(s), {} pairs row(s), {} dict \
                             record(s), {} record blob row(s), {} scoped render lane(s), {} \
                             Morton cell(s), {} unique index entr(ies), {} edited item(s) over {} \
                             row(s)",
                            report.terms,
                            report.delta_tiers,
                            report.pairs_rows,
                            report.dict_records,
                            report.record_rows,
                            report.scoped_render_lanes,
                            report.cells,
                            report.unique_entries,
                            report.edited_pairs,
                            report.edited_rows
                        );
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("FAILED {}: {e}", bundle.display());
                        ExitCode::FAILURE
                    }
                }
            } else {
                match tessera_build::verify(&bundle) {
                    Ok(report) => {
                        print_shallow(&report);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("FAILED {}: {e}", bundle.display());
                        ExitCode::FAILURE
                    }
                }
            }
        }
        Command::Corpus { command } => match command {
            CorpusCommand::Items { seed, ids, extent } => corpus_items(seed, &ids, extent),
            CorpusCommand::Census {
                seed,
                n,
                zoom,
                grant,
                extent,
            } => corpus_census(seed, n, zoom, &grant, extent),
            CorpusCommand::Materialise {
                seed,
                n,
                out,
                terms_per_level,
                extent,
            } => corpus_materialise(seed, n, &out, terms_per_level, extent),
            CorpusCommand::ArtifactCensus {
                seed,
                n,
                layer,
                grant,
                terms_per_level,
                extent,
            } => corpus_artifact_census(seed, n, layer, &grant, terms_per_level, extent),
        },
        Command::Check {
            deployment,
            config,
            file,
            payloads,
        } => {
            let declaration = match resolve_declaration(
                deployment.as_deref(),
                config,
                file,
                tessera_build::config::Strictness::Declared,
            ) {
                Ok(resolved) => resolved,
                Err(detail) => {
                    eprintln!("check FAILED: {detail}");
                    return ExitCode::FAILURE;
                }
            };
            let config = declaration.config;
            let report = tessera_build::check::check(&config);
            // The page is rendered where the report is, so the binary and the Python extension
            // module print the same bytes. Stdout carries the payloads and nothing else, which is
            // what a CI job pipes into `curl`.
            eprint!("{}", tessera_build::check::page(&config, &report));
            if !report.is_clean() {
                return ExitCode::FAILURE;
            }
            if payloads {
                // The declaration, minus its acquisition keys, is the payload, so this is a
                // serialisation and not a translation and there is no second authority to drift.
                match serde_json::to_string_pretty(&tessera_build::config::control_payloads(
                    &config,
                )) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("check FAILED: serialising the declaration payloads: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Command::Login(args) => identity::login(args),
        Command::Logout(args) => identity::logout(args),
        Command::Session { command } => identity::session(command),
        Command::Principal { command } => identity::principal(command),
        Command::Key { command } => identity::key(command),
        Command::Group { command } => identity::group(command),
        Command::Grant(args) => identity::grant(args, false),
        Command::RevokeGrant(args) => identity::grant(args, true),
        Command::Provider { command } => identity::provider(command),
        Command::Items(args) => records::items(args),
        Command::Artifacts(args) => records::artifacts(args),
        Command::Health {
            deployment,
            timeout,
        } => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let config = match tessera_config::open(deployment.as_deref(), &cwd) {
                Ok((_, config)) => config,
                Err(e) => {
                    eprintln!("tessera health: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let Some(viewer) = config.viewer_addr else {
                eprintln!(
                    "tessera health: this deployment declares no viewer address; add one under \
                     `[serve]`, such as `viewer = \"127.0.0.1:8080\"`"
                );
                return ExitCode::FAILURE;
            };
            match readyz(viewer, std::time::Duration::from_secs(timeout)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("tessera health: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Serve { deployment } => {
            // Diagnostics on stderr, because stdout carries one thing: the JSON line naming the
            // three bound addresses, which a supervisor reads as the process's first stdout line
            // (`tessera_server::serve_announcing`).
            // Colour only for a terminal, so a container's or a supervisor's log is plain text.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .init();
            // SIGTERM or SIGINT ends the process at once, including while the bundle opens. A
            // write is fsynced before it is acknowledged, so stopping loses no acknowledged write,
            // and the next start replays the log. A deletion or suppression answered 500 because
            // the log could not be written is applied but not logged, so stopping undoes it.
            exit_on_termination();
            let deployment = match tessera_config::discover(
                deployment.as_deref(),
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            ) {
                Ok(path) => path,
                Err(e) => {
                    eprintln!("tessera serve: refused to start: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let prepared = match tessera_server::prepare(&deployment) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("tessera serve: refused to start: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // **The blocking pool is sized here, from the config's declared consumers**, and this
            // is the only place in the process where that number exists.
            //
            // It is sized explicitly rather than left at tokio's default (512) because three things
            // depend on it and none states it: the viewer plane's `spawn_blocking` closures,
            // bounded by `compute_admission` and `bulk_admission`, password checks at login,
            // bounded by `password_admission`, and `/control/ingest`'s, bounded by
            // `ingest_admission`. A tokio release or an embedder's own builder could move that
            // default in silence, and an admitted viewport would then queue behind ingest closures
            // in the shared FIFO with no timeout — hanging rather than shedding.
            //
            // Derived rather than asserted-against: `serving_blocking_threads` covers every bound
            // plus a reserve, so there is no configuration in which an admitted request finds no
            // thread.
            let blocking_threads = tessera_config::serving_blocking_threads(&prepared.config);
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .max_blocking_threads(blocking_threads)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("tessera serve: could not start async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match runtime.block_on(tessera_server::run(prepared)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("tessera serve: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
